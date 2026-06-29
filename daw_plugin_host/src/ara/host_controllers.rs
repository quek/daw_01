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

// --- Archiving controller (stub until the persistence step) ----------------

unsafe extern "C" fn archiving_get_size(
    _controller: ARAArchivingControllerHostRef,
    _reader: ARAArchiveReaderHostRef,
) -> ARASize {
    0
}

unsafe extern "C" fn archiving_read_bytes(
    _controller: ARAArchivingControllerHostRef,
    _reader: ARAArchiveReaderHostRef,
    _position: ARASize,
    _length: ARASize,
    _buffer: *mut ARAByte,
) -> ARABool {
    tracing::warn!("ARA archiving read invoked before persistence is implemented");
    ARA_FALSE
}

unsafe extern "C" fn archiving_write_bytes(
    _controller: ARAArchivingControllerHostRef,
    _writer: ARAArchiveWriterHostRef,
    _position: ARASize,
    _length: ARASize,
    _buffer: *const ARAByte,
) -> ARABool {
    tracing::warn!("ARA archiving write invoked before persistence is implemented");
    ARA_FALSE
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
