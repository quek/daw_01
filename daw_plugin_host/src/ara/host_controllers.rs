// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! ARA host controller interfaces serviced by `daw_plugin_host`.
//!
//! These are the `#[repr(C)]` function-pointer tables the plug-in calls back
//! into. The host must provide them when creating a document controller (see
//! [`host_instance`]). Of the five ARA host controllers, `AudioAccess` and
//! `Archiving` must be non-null; `ContentAccess`, `ModelUpdate` and `Playback`
//! are optional and left null until their steps land (docs/plan_ara2.md).
//!
//! - **AudioAccess** is implemented: it streams whole-source PCM from
//!   [`AraAudioSourceHost`] into the plug-in's non-interleaved buffers.
//! - **Archiving** is a never-erroring stub (only invoked during project
//!   save/restore, wired up in the persistence step).
//!
//! Opaque ARA host refs carry our own pointers: `ARAAudioSourceHostRef` is a
//! `*const AraAudioSourceHost` (owned by the document), `ARAAudioReaderHostRef`
//! is a `*mut AraAudioReader` (owned by the plug-in's create/destroy pair).

use core::ffi::c_void;
use core::ptr;

use ara_sys::{
    ARAArchiveReaderHostRef, ARAArchiveWriterHostRef, ARAArchivingControllerHostRef,
    ARAArchivingControllerInterface, ARAAudioAccessControllerHostRef,
    ARAAudioAccessControllerInterface, ARAAudioReaderHostRef, ARAAudioSourceHostRef, ARABool,
    ARAByte, ARAContentAccessControllerHostRef, ARAContentAccessControllerInterface,
    ARAContentBarSignature, ARAContentGrade, ARAContentReaderHostRef, ARAContentTempoEntry,
    ARAContentTimeRange, ARAContentType, ARAContentUpdateFlags, ARADocumentControllerHostInstance,
    ARAModelUpdateControllerHostRef, ARAModelUpdateControllerInterface, ARAMusicalContextHostRef,
    ARAPersistentID, ARAPlaybackRegionHostRef, ARASampleCount, ARASamplePosition, ARASize,
    kARAContentGradeAdjusted, kARAContentTypeBarSignatures, kARAContentTypeTempoEntries,
};
use ara_sys::{ARAAnalysisProgressState, ARAAudioModificationHostRef};

use crate::ara::audio_source::AraAudioSourceHost;

/// `kARATrue` / `kARAFalse`. `ARABool` is a 32-bit int and ARA fixes these to
/// 1 / 0.
const ARA_TRUE: ARABool = 1;
const ARA_FALSE: ARABool = 0;

/// Diagnostic: how many times the plug-in has pulled source samples through our
/// AudioAccess controller (i.e. how much analysis it has driven). Read by the
/// `--ara-selftest` harness to confirm analysis is actually running.
pub static AUDIO_READ_SAMPLES_CALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Host data behind an `ARAAudioReaderHostRef`. Created per read session by the
/// plug-in via `createAudioReaderForSource` and freed via `destroyAudioReader`;
/// borrows the longer-lived [`AraAudioSourceHost`] (owned by the document).
struct AraAudioReader {
    source: *const AraAudioSourceHost,
    use_f64: bool,
}

// --- AudioAccess controller ------------------------------------------------

unsafe extern "C" fn audio_access_create_reader(
    _controller: ARAAudioAccessControllerHostRef,
    audio_source: ARAAudioSourceHostRef,
    use_64bit_samples: ARABool,
) -> ARAAudioReaderHostRef {
    let reader = Box::new(AraAudioReader {
        source: audio_source.cast::<AraAudioSourceHost>().cast_const(),
        use_f64: use_64bit_samples != ARA_FALSE,
    });
    Box::into_raw(reader) as ARAAudioReaderHostRef
}

unsafe extern "C" fn audio_access_read_samples(
    _controller: ARAAudioAccessControllerHostRef,
    audio_reader: ARAAudioReaderHostRef,
    sample_position: ARASamplePosition,
    samples_per_channel: ARASampleCount,
    buffers: *const *mut c_void,
) -> ARABool {
    AUDIO_READ_SAMPLES_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let reader = match unsafe { audio_reader.cast::<AraAudioReader>().as_ref() } {
        Some(r) => r,
        None => return ARA_FALSE,
    };
    let Some(source) = (unsafe { reader.source.as_ref() }) else {
        return ARA_FALSE;
    };
    if buffers.is_null() || samples_per_channel < 0 {
        return ARA_FALSE;
    }

    for channel in 0..source.channel_count as usize {
        let buf = unsafe { *buffers.add(channel) };
        if buf.is_null() {
            continue;
        }
        for i in 0..samples_per_channel {
            // `sample_position` is plug-in-supplied; saturate so a pathological
            // value can't overflow (out-of-range frames already read as silence).
            let value = source.sample_at(channel, sample_position.saturating_add(i));
            let offset = i as usize;
            if reader.use_f64 {
                unsafe { buf.cast::<f64>().add(offset).write(f64::from(value)) };
            } else {
                unsafe { buf.cast::<f32>().add(offset).write(value) };
            }
        }
    }
    ARA_TRUE
}

unsafe extern "C" fn audio_access_destroy_reader(
    _controller: ARAAudioAccessControllerHostRef,
    audio_reader: ARAAudioReaderHostRef,
) {
    if audio_reader.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(audio_reader.cast::<AraAudioReader>()) });
}

static AUDIO_ACCESS_IFACE: ARAAudioAccessControllerInterface = ARAAudioAccessControllerInterface {
    structSize: core::mem::size_of::<ARAAudioAccessControllerInterface>(),
    createAudioReaderForSource: Some(audio_access_create_reader),
    readAudioSamples: Some(audio_access_read_samples),
    destroyAudioReader: Some(audio_access_destroy_reader),
};

// --- Archiving controller --------------------------------------------------

/// Maximum ARA archive the host will buffer (32 MiB). Guards against a
/// pathological plug-in growing the writer without bound.
const MAX_ARA_ARCHIVE_BYTES: usize = 32 * 1024 * 1024;

/// Host writer behind an `ARAArchiveWriterHostRef`. The plug-in serialises its
/// edit state by calling `writeBytesToArchive`, possibly out of order (ARA
/// permits rewinding to patch chunk headers), so the buffer grows and zero-fills
/// gaps as needed.
#[derive(Default)]
pub struct AraArchiveWriter {
    pub data: Vec<u8>,
}

impl AraArchiveWriter {
    fn write_at(&mut self, position: usize, src: &[u8]) -> bool {
        let Some(end) = position.checked_add(src.len()) else {
            return false;
        };
        if end > MAX_ARA_ARCHIVE_BYTES {
            return false;
        }
        if self.data.len() < end {
            self.data.resize(end, 0);
        }
        self.data[position..end].copy_from_slice(src);
        true
    }
}

/// Host reader behind an `ARAArchiveReaderHostRef` during restore. Owns a copy
/// of the archive so its lifetime is independent of the caller's buffer.
pub struct AraArchiveReader {
    data: Vec<u8>,
}

impl AraArchiveReader {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

unsafe extern "C" fn archiving_get_size(
    _controller: ARAArchivingControllerHostRef,
    reader: ARAArchiveReaderHostRef,
) -> ARASize {
    unsafe { reader.cast::<AraArchiveReader>().as_ref() }.map_or(0, |r| r.data.len())
}

unsafe extern "C" fn archiving_read_bytes(
    _controller: ARAArchivingControllerHostRef,
    reader: ARAArchiveReaderHostRef,
    position: ARASize,
    length: ARASize,
    buffer: *mut ARAByte,
) -> ARABool {
    let Some(reader) = (unsafe { reader.cast::<AraArchiveReader>().as_ref() }) else {
        return ARA_FALSE;
    };
    if buffer.is_null() {
        return ARA_FALSE;
    }
    let Some(end) = position.checked_add(length) else {
        return ARA_FALSE;
    };
    if end > reader.data.len() {
        return ARA_FALSE; // reading past the archive end is a programming error
    }
    let dst = unsafe { core::slice::from_raw_parts_mut(buffer, length) };
    dst.copy_from_slice(&reader.data[position..end]);
    ARA_TRUE
}

unsafe extern "C" fn archiving_write_bytes(
    _controller: ARAArchivingControllerHostRef,
    writer: ARAArchiveWriterHostRef,
    position: ARASize,
    length: ARASize,
    buffer: *const ARAByte,
) -> ARABool {
    let Some(writer) = (unsafe { writer.cast::<AraArchiveWriter>().as_mut() }) else {
        return ARA_FALSE;
    };
    if buffer.is_null() {
        return ARA_FALSE;
    }
    let src = unsafe { core::slice::from_raw_parts(buffer, length) };
    if writer.write_at(position, src) {
        ARA_TRUE
    } else {
        ARA_FALSE
    }
}

unsafe extern "C" fn archiving_notify_archiving_progress(
    _controller: ARAArchivingControllerHostRef,
    _value: f32,
) {
}

unsafe extern "C" fn archiving_notify_unarchiving_progress(
    _controller: ARAArchivingControllerHostRef,
    _value: f32,
) {
}

/// Fallback host archive id if the plug-in's factory didn't provide one.
const DEFAULT_DOCUMENT_ARCHIVE_ID: &[u8] = b"daw01.ara.document.v1\0";

unsafe extern "C" fn archiving_get_document_archive_id(
    controller: ARAArchivingControllerHostRef,
    _reader: ARAArchiveReaderHostRef,
) -> ARAPersistentID {
    // ARA requires hosts to implement this (it must return a non-null, non-empty
    // id — Melodyne asserts on it). The correct value is the document archive id
    // the plug-in's own factory provided; we stash that factory id in the
    // archiving controller host ref at document-controller creation, so return
    // it here. A `null`/missing id falls back to a stable host constant.
    if controller.is_null() {
        return DEFAULT_DOCUMENT_ARCHIVE_ID.as_ptr().cast::<core::ffi::c_char>();
    }
    controller.cast_const().cast::<core::ffi::c_char>()
}

static ARCHIVING_IFACE: ARAArchivingControllerInterface = ARAArchivingControllerInterface {
    structSize: core::mem::size_of::<ARAArchivingControllerInterface>(),
    getArchiveSize: Some(archiving_get_size),
    readBytesFromArchive: Some(archiving_read_bytes),
    writeBytesToArchive: Some(archiving_write_bytes),
    notifyDocumentArchivingProgress: Some(archiving_notify_archiving_progress),
    notifyDocumentUnarchivingProgress: Some(archiving_notify_unarchiving_progress),
    getDocumentArchiveID: Some(archiving_get_document_archive_id),
};

// --- ContentAccess controller ----------------------------------------------

/// Host model behind an `ARAMusicalContextHostRef`: the song's tempo + bar
/// signature, which the host serves to the plug-in through the ContentAccess
/// controller. ARA reads the song timeline from here when a musical context is
/// created, so the plug-in (e.g. Melodyne) can align its bar/beat grid to the
/// host.
///
/// Owned by the [`crate::ara::session::AraSession`] (boxed so its address — the
/// opaque `ARAMusicalContextHostRef` we hand the plug-in — stays stable), and
/// updated in place when the project tempo changes.
pub struct AraMusicalContextHost {
    /// Seconds per quarter note (`60 / bpm`). Defines the (constant) tempo line.
    pub seconds_per_quarter: f64,
    pub bar_numerator: i32,
    pub bar_denominator: i32,
}

impl Default for AraMusicalContextHost {
    fn default() -> Self {
        // 120 bpm, 4/4 — a sane placeholder until the real project tempo is
        // pushed via `SetupAraDocument`.
        Self {
            seconds_per_quarter: 0.5,
            bar_numerator: 4,
            bar_denominator: 4,
        }
    }
}

/// Host data behind an `ARAContentReaderHostRef`: the materialised events for
/// one content-reader session. The plug-in calls `getContentReaderDataForEvent`
/// expecting a pointer that stays valid until the next such call or until the
/// reader is destroyed, so the event vector is heap-owned here and pointers into
/// it are returned directly.
enum AraContentReader {
    Tempo(Vec<ARAContentTempoEntry>),
    BarSignatures(Vec<ARAContentBarSignature>),
}

impl AraContentReader {
    fn event_count(&self) -> i32 {
        let len = match self {
            Self::Tempo(v) => v.len(),
            Self::BarSignatures(v) => v.len(),
        };
        i32::try_from(len).unwrap_or(i32::MAX)
    }

    /// Pointer to the `index`-th event, or null if out of range. The pointee is
    /// stable for the reader's lifetime (it lives in the heap-owned vector).
    fn event_ptr(&self, index: i32) -> *const c_void {
        let Ok(index) = usize::try_from(index) else {
            return ptr::null();
        };
        match self {
            Self::Tempo(v) => v.get(index).map_or(ptr::null(), |e| ptr::from_ref(e).cast()),
            Self::BarSignatures(v) => v.get(index).map_or(ptr::null(), |e| ptr::from_ref(e).cast()),
        }
    }
}

/// Whether `content_type` is one the host serves for musical contexts.
fn musical_context_type_served(content_type: ARAContentType) -> bool {
    content_type == kARAContentTypeTempoEntries.0 || content_type == kARAContentTypeBarSignatures.0
}

unsafe extern "C" fn content_is_musical_context_available(
    _controller: ARAContentAccessControllerHostRef,
    musical_context: ARAMusicalContextHostRef,
    content_type: ARAContentType,
) -> ARABool {
    if musical_context.is_null() || !musical_context_type_served(content_type) {
        return ARA_FALSE;
    }
    ARA_TRUE
}

unsafe extern "C" fn content_get_musical_context_grade(
    _controller: ARAContentAccessControllerHostRef,
    _musical_context: ARAMusicalContextHostRef,
    _content_type: ARAContentType,
) -> ARAContentGrade {
    // The host's project tempo / bar signature is authoritative.
    kARAContentGradeAdjusted.0
}

unsafe extern "C" fn content_create_musical_context_reader(
    _controller: ARAContentAccessControllerHostRef,
    musical_context: ARAMusicalContextHostRef,
    content_type: ARAContentType,
    _range: *const ARAContentTimeRange,
) -> ARAContentReaderHostRef {
    let Some(ctx) = (unsafe { musical_context.cast::<AraMusicalContextHost>().as_ref() }) else {
        return ptr::null_mut();
    };
    let reader = if content_type == kARAContentTypeTempoEntries.0 {
        // A constant tempo is two collinear points: (0s, 0q) and one quarter
        // later. The plug-in extrapolates the line beyond them.
        AraContentReader::Tempo(vec![
            ARAContentTempoEntry {
                timePosition: 0.0,
                quarterPosition: 0.0,
            },
            ARAContentTempoEntry {
                timePosition: ctx.seconds_per_quarter,
                quarterPosition: 1.0,
            },
        ])
    } else if content_type == kARAContentTypeBarSignatures.0 {
        AraContentReader::BarSignatures(vec![ARAContentBarSignature {
            numerator: ctx.bar_numerator,
            denominator: ctx.bar_denominator,
            position: 0.0,
        }])
    } else {
        return ptr::null_mut();
    };
    Box::into_raw(Box::new(reader)) as ARAContentReaderHostRef
}

unsafe extern "C" fn content_is_audio_source_available(
    _controller: ARAContentAccessControllerHostRef,
    _audio_source: ARAAudioSourceHostRef,
    _content_type: ARAContentType,
) -> ARABool {
    // The host performs no audio analysis; the plug-in derives its own.
    ARA_FALSE
}

unsafe extern "C" fn content_get_audio_source_grade(
    _controller: ARAContentAccessControllerHostRef,
    _audio_source: ARAAudioSourceHostRef,
    _content_type: ARAContentType,
) -> ARAContentGrade {
    kARAContentGradeAdjusted.0
}

unsafe extern "C" fn content_create_audio_source_reader(
    _controller: ARAContentAccessControllerHostRef,
    _audio_source: ARAAudioSourceHostRef,
    _content_type: ARAContentType,
    _range: *const ARAContentTimeRange,
) -> ARAContentReaderHostRef {
    ptr::null_mut()
}

unsafe extern "C" fn content_get_event_count(
    _controller: ARAContentAccessControllerHostRef,
    reader: ARAContentReaderHostRef,
) -> ara_sys::ARAInt32 {
    unsafe { reader.cast::<AraContentReader>().as_ref() }.map_or(0, AraContentReader::event_count)
}

unsafe extern "C" fn content_get_event_data(
    _controller: ARAContentAccessControllerHostRef,
    reader: ARAContentReaderHostRef,
    event_index: ara_sys::ARAInt32,
) -> *const c_void {
    unsafe { reader.cast::<AraContentReader>().as_ref() }
        .map_or(ptr::null(), |r| r.event_ptr(event_index))
}

unsafe extern "C" fn content_destroy_reader(
    _controller: ARAContentAccessControllerHostRef,
    reader: ARAContentReaderHostRef,
) {
    if reader.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(reader.cast::<AraContentReader>()) });
}

static CONTENT_ACCESS_IFACE: ARAContentAccessControllerInterface =
    ARAContentAccessControllerInterface {
        structSize: core::mem::size_of::<ARAContentAccessControllerInterface>(),
        isMusicalContextContentAvailable: Some(content_is_musical_context_available),
        getMusicalContextContentGrade: Some(content_get_musical_context_grade),
        createMusicalContextContentReader: Some(content_create_musical_context_reader),
        isAudioSourceContentAvailable: Some(content_is_audio_source_available),
        getAudioSourceContentGrade: Some(content_get_audio_source_grade),
        createAudioSourceContentReader: Some(content_create_audio_source_reader),
        getContentReaderEventCount: Some(content_get_event_count),
        getContentReaderDataForEvent: Some(content_get_event_data),
        destroyContentReader: Some(content_destroy_reader),
    };

// --- ModelUpdate controller ------------------------------------------------
//
// The plug-in calls these (only from within `notifyModelUpdates`) to tell the
// host about analysis progress and content changes. We don't yet act on the
// notifications, but the interface must be non-null: a plug-in driven head-less
// (Melodyne) won't run/finish its audio-source analysis if it has nowhere to
// report to. The diagnostics here confirm analysis is actually progressing.

unsafe extern "C" fn model_notify_analysis_progress(
    _controller: ARAModelUpdateControllerHostRef,
    _audio_source: ARAAudioSourceHostRef,
    state: ARAAnalysisProgressState,
    value: f32,
) {
    tracing::debug!(target: "ara", state, value, "ARA audio source analysis progress");
}

unsafe extern "C" fn model_notify_audio_source_content_changed(
    _controller: ARAModelUpdateControllerHostRef,
    _audio_source: ARAAudioSourceHostRef,
    _range: *const ARAContentTimeRange,
    flags: ARAContentUpdateFlags,
) {
    tracing::info!(target: "ara", flags, "ARA audio source content changed (analysis result)");
}

unsafe extern "C" fn model_notify_audio_modification_content_changed(
    _controller: ARAModelUpdateControllerHostRef,
    _audio_modification: ARAAudioModificationHostRef,
    _range: *const ARAContentTimeRange,
    _flags: ARAContentUpdateFlags,
) {
}

unsafe extern "C" fn model_notify_playback_region_content_changed(
    _controller: ARAModelUpdateControllerHostRef,
    _playback_region: ARAPlaybackRegionHostRef,
    _range: *const ARAContentTimeRange,
    _flags: ARAContentUpdateFlags,
) {
}

unsafe extern "C" fn model_notify_document_data_changed(_controller: ARAModelUpdateControllerHostRef) {
}

static MODEL_UPDATE_IFACE: ARAModelUpdateControllerInterface = ARAModelUpdateControllerInterface {
    structSize: core::mem::size_of::<ARAModelUpdateControllerInterface>(),
    notifyAudioSourceAnalysisProgress: Some(model_notify_analysis_progress),
    notifyAudioSourceContentChanged: Some(model_notify_audio_source_content_changed),
    notifyAudioModificationContentChanged: Some(model_notify_audio_modification_content_changed),
    notifyPlaybackRegionContentChanged: Some(model_notify_playback_region_content_changed),
    notifyDocumentDataChanged: Some(model_notify_document_data_changed),
};

// --- Host instance assembly ------------------------------------------------

/// Builds the `ARADocumentControllerHostInstance` passed to
/// `createDocumentControllerWithDocument`. The interface pointers reference the
/// `'static` tables above (valid for the whole process). AudioAccess, Archiving
/// and ContentAccess are provided; ModelUpdate and Playback are optional and
/// left null (the host drives playback itself and polls model updates lazily).
/// The caller must keep the returned struct (and the interfaces it references)
/// alive until the document controller is destroyed — ARA has the plug-in retain
/// this pointer, not copy it (see `AraDocumentController`).
///
/// `document_archive_id` is the plug-in's own `ARAFactory::documentArchiveID`; we
/// stash it in `archivingControllerHostRef` so `getDocumentArchiveID` (which ARA
/// requires and Melodyne asserts on) can return it. The other archiving
/// callbacks ignore the host ref, so this overload is safe.
pub fn host_instance(
    document_archive_id: ARAPersistentID,
) -> ARADocumentControllerHostInstance {
    ARADocumentControllerHostInstance {
        structSize: core::mem::size_of::<ARADocumentControllerHostInstance>(),
        audioAccessControllerHostRef: ptr::null_mut(),
        audioAccessControllerInterface: ptr::addr_of!(AUDIO_ACCESS_IFACE),
        archivingControllerHostRef: document_archive_id.cast_mut().cast(),
        archivingControllerInterface: ptr::addr_of!(ARCHIVING_IFACE),
        contentAccessControllerHostRef: ptr::null_mut(),
        contentAccessControllerInterface: ptr::addr_of!(CONTENT_ACCESS_IFACE),
        modelUpdateControllerHostRef: ptr::null_mut(),
        modelUpdateControllerInterface: ptr::addr_of!(MODEL_UPDATE_IFACE),
        playbackControllerHostRef: ptr::null_mut(),
        playbackControllerInterface: ptr::null(),
    }
}
