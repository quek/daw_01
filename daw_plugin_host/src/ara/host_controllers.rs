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
    ARAByte, ARADocumentControllerHostInstance, ARAPersistentID, ARASampleCount, ARASamplePosition,
    ARASize,
};

use crate::ara::audio_source::AraAudioSourceHost;

/// `kARATrue` / `kARAFalse`. `ARABool` is a 32-bit int and ARA fixes these to
/// 1 / 0.
const ARA_TRUE: ARABool = 1;
const ARA_FALSE: ARABool = 0;

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

unsafe extern "C" fn archiving_get_document_archive_id(
    _controller: ARAArchivingControllerHostRef,
    _reader: ARAArchiveReaderHostRef,
) -> ARAPersistentID {
    ptr::null()
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

// --- Host instance assembly ------------------------------------------------

/// Builds the `ARADocumentControllerHostInstance` passed to
/// `createDocumentControllerWithDocument`. The interface pointers reference the
/// `'static` tables above (valid for the whole process); the optional
/// controllers are null until their steps land. The struct only needs to stay
/// valid for the duration of the create call (the plug-in copies what it keeps).
pub fn host_instance() -> ARADocumentControllerHostInstance {
    ARADocumentControllerHostInstance {
        structSize: core::mem::size_of::<ARADocumentControllerHostInstance>(),
        audioAccessControllerHostRef: ptr::null_mut(),
        audioAccessControllerInterface: ptr::addr_of!(AUDIO_ACCESS_IFACE),
        archivingControllerHostRef: ptr::null_mut(),
        archivingControllerInterface: ptr::addr_of!(ARCHIVING_IFACE),
        contentAccessControllerHostRef: ptr::null_mut(),
        contentAccessControllerInterface: ptr::null(),
        modelUpdateControllerHostRef: ptr::null_mut(),
        modelUpdateControllerInterface: ptr::null(),
        playbackControllerHostRef: ptr::null_mut(),
        playbackControllerInterface: ptr::null(),
    }
}
