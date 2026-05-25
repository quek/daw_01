//! Video file import via Windows Media Foundation (`docs/plan_video.md` P2).
//!
//! Pipeline (mirrors `import_audio.rs` shape so the two co-exist cleanly):
//!
//! 1. Compute SHA-256 of the source file (first 4 bytes → 8 hex chars).
//! 2. Copy the file into `<project_dir>/samples/<basename>_<hash>.<ext>`
//!    (`Absolute` cache path when there is no project_dir yet, same as
//!    audio import).
//! 3. Open with `IMFSourceReader` to read metadata (width / height /
//!    framerate / duration / codec) and — when the file carries an
//!    audio stream — extract the audio to a paired WAV file via
//!    `IMFSourceReader::ReadSample` + `hound::WavWriter` (P2.4).
//! 4. Build a `VideoSource` referencing the on-disk video path, and an
//!    optional `AudioSource` referencing the extracted WAV. The
//!    `VideoSource.audio_source_id` field carries the back-link.
//!
//! WMF lifecycle: `MFStartup` is called lazily on first use via a
//! `OnceLock`-guarded helper. We do NOT call `MFShutdown` — daw_gui
//! shuts the process down without orderly teardown, and skipping
//! shutdown is documented as safe by the WMF docs.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

use common::model::{VideoSource, VideoSourcePath};

use windows::Win32::Media::MediaFoundation::{
    IMFSourceReader, MFAudioFormat_Float, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video, MFSTARTUP_FULL,
    MFStartup, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_PD_DURATION, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION,
    MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_RGB32,
    MFVideoFormat_VP90,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::core::{GUID, PCWSTR};

use crate::import_audio::{file_hash8, samples_filename};

/// Successful metadata read. `audio_source_id` is populated only when
/// the source carried an audio stream that was extracted to a paired
/// WAV (P2.4 wires this).
#[derive(Debug, Clone)]
pub struct VideoMetadata {
    pub width: u32,
    pub height: u32,
    /// Frames per second computed from `MF_MT_FRAME_RATE`
    /// (numerator / denominator). 0.0 indicates "unknown" (the stream
    /// reported neither attribute).
    pub framerate: f32,
    /// Total stream duration in microseconds, derived from the WMF
    /// `MF_PD_DURATION` (100-ns units) divided by 10. Zero when the
    /// container didn't report a duration.
    pub duration_micros: u64,
    /// Free-form codec label ("h264" / "hevc" / "vp9" / "av1" / "unknown").
    /// Used only for display and diagnostics.
    pub codec: String,
}

#[derive(Debug)]
pub enum VideoImportError {
    UnsupportedFormat(String),
    /// MFStartup, source reader creation, attribute read failed.
    DecodeFailed(String),
    IoError(String),
}

impl std::fmt::Display for VideoImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat(ext) => write!(
                f,
                "Unsupported video format: .{ext} (P2 supports mp4/mov/mkv/webm)"
            ),
            Self::DecodeFailed(s) => write!(f, "Video decode failed: {s}"),
            Self::IoError(s) => write!(f, "I/O error: {s}"),
        }
    }
}

impl std::error::Error for VideoImportError {}

/// Lazy-init `MFStartup` exactly once per process. Returns the cached
/// result on every subsequent call. `CoInitializeEx(MTA)` is also
/// invoked once; subsequent thread inits are independent.
///
/// Sibling `video_playback` module reuses this via
/// [`ensure_mf_startup_pub`] so the same process-wide guard covers
/// both import-time decode and playback-time decode.
fn ensure_mf_startup() -> Result<(), VideoImportError> {
    static MF_INIT: OnceLock<Result<(), String>> = OnceLock::new();
    let result = MF_INIT.get_or_init(|| {
        unsafe {
            // `CoInitializeEx` is mandatory before `MFStartup`. RPC_E_CHANGED_MODE
            // is returned when the apartment was already initialized as STA
            // by an unrelated subsystem; we ignore that and proceed because
            // MTA semantics are what WMF wants and STA-initialized callers
            // (e.g. winit / wgpu on some backends) accept MFStartup anyway.
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() && hr.0 != windows::Win32::Foundation::RPC_E_CHANGED_MODE.0 {
                return Err(format!("CoInitializeEx: {hr:?}"));
            }
            MFStartup(MF_VERSION, MFSTARTUP_FULL)
                .map_err(|e| format!("MFStartup: {e}"))?;
            Ok(())
        }
    });
    match result {
        Ok(()) => Ok(()),
        Err(s) => Err(VideoImportError::DecodeFailed(s.clone())),
    }
}

/// Convert a Rust path to the NUL-terminated UTF-16 buffer that
/// `MFCreateSourceReaderFromURL` consumes (`PCWSTR`). The returned
/// `Vec<u16>` MUST outlive the `PCWSTR` view it backs.
fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// `pub` wrapper around the module-private `ensure_mf_startup` so
/// sibling `video_playback` can share the same `OnceLock`. Returns
/// a stringified error so callers can format it without picking up
/// this module's error enum.
pub fn ensure_mf_startup_pub() -> Result<(), String> {
    ensure_mf_startup().map_err(|e| e.to_string())
}

/// Best-effort PROPVARIANT → u64 extractor for VT_UI8 values (= the
/// type MF_PD_DURATION uses, 100-ns units). Returns `None` when the
/// variant carries a different tag — defensive against MF backends
/// that report duration as VT_I8 / VT_EMPTY on certain containers.
fn propvariant_to_u64(
    propvar: &windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
) -> Option<u64> {
    use windows::Win32::System::Variant::VT_UI8;
    unsafe {
        let inner = &propvar.Anonymous.Anonymous;
        if inner.vt == VT_UI8 {
            Some(inner.Anonymous.uhVal)
        } else if inner.vt.0 == 20 /* VT_I8 */ {
            // Signed 64-bit; treat negative as "unknown duration".
            let v = inner.Anonymous.hVal;
            if v >= 0 { Some(v as u64) } else { None }
        } else {
            None
        }
    }
}

/// Map an MF video subtype GUID to a short codec label suitable for
/// `VideoSource.codec`. Unknown subtypes fall back to `"unknown"`.
fn codec_label(subtype: &GUID) -> &'static str {
    if subtype == &MFVideoFormat_H264 {
        "h264"
    } else if subtype == &MFVideoFormat_HEVC {
        "hevc"
    } else if subtype == &MFVideoFormat_VP90 {
        "vp9"
    } else if subtype == &MFVideoFormat_AV1 {
        "av1"
    } else {
        "unknown"
    }
}

/// Open a video file with `IMFSourceReader` and read metadata from its
/// first video stream. The reader is configured with
/// `MF_SOURCE_READERF_ENABLE_VIDEO_PROCESSING` so the SDK can pivot
/// codec-specific subtypes (e.g. NV12) to a uniform output later; the
/// attribute does not affect metadata read.
pub fn extract_metadata(path: &Path) -> Result<VideoMetadata, VideoImportError> {
    ensure_mf_startup()?;

    if !path.exists() {
        return Err(VideoImportError::IoError(format!(
            "file not found: {}",
            path.display()
        )));
    }

    let wide = to_wide(path);
    let url = PCWSTR::from_raw(wide.as_ptr());

    let attrs = unsafe {
        let mut a = None;
        MFCreateAttributes(&mut a, 1)
            .map_err(|e| VideoImportError::DecodeFailed(format!("MFCreateAttributes: {e}")))?;
        let attrs = a.ok_or_else(|| {
            VideoImportError::DecodeFailed("MFCreateAttributes returned null".into())
        })?;
        attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(|e| {
                VideoImportError::DecodeFailed(format!(
                    "SetUINT32 ENABLE_VIDEO_PROCESSING: {e}"
                ))
            })?;
        attrs
    };

    let reader: IMFSourceReader = unsafe {
        MFCreateSourceReaderFromURL(url, &attrs).map_err(|e| {
            VideoImportError::DecodeFailed(format!(
                "MFCreateSourceReaderFromURL({}): {e}",
                path.display()
            ))
        })?
    };

    // Native media type from the first video stream.
    let stream_index = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let video_type = unsafe { reader.GetNativeMediaType(stream_index, 0) }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!("GetNativeMediaType: {e}"))
        })?;

    // MF_MT_FRAME_SIZE packs (width:u32, height:u32) into a u64 — high
    // dword is width, low dword is height.
    let frame_size = unsafe { video_type.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!("MF_MT_FRAME_SIZE: {e}"))
        })?;
    let width = (frame_size >> 32) as u32;
    let height = (frame_size & 0xFFFF_FFFF) as u32;

    // MF_MT_FRAME_RATE packs (numerator:u32, denominator:u32) likewise.
    let framerate = match unsafe { video_type.GetUINT64(&MF_MT_FRAME_RATE) } {
        Ok(fr) => {
            let num = (fr >> 32) as u32;
            let den = (fr & 0xFFFF_FFFF) as u32;
            if den == 0 {
                0.0
            } else {
                num as f32 / den as f32
            }
        }
        Err(_) => 0.0,
    };

    let subtype = unsafe { video_type.GetGUID(&MF_MT_SUBTYPE) }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!("MF_MT_SUBTYPE: {e}"))
        })?;
    let codec = codec_label(&subtype).to_string();

    // Total stream duration in 100-ns units. Convert to microseconds.
    // `MF_SOURCE_READER_MEDIASOURCE` is the special stream sentinel
    // (= u32::MAX) used to query presentation-level attributes.
    let duration_micros = match unsafe {
        reader.GetPresentationAttribute(
            MF_SOURCE_READER_MEDIASOURCE.0 as u32,
            &MF_PD_DURATION,
        )
    } {
        Ok(propvar) => propvariant_to_u64(&propvar).map(|t| t / 10).unwrap_or(0),
        Err(_) => 0,
    };

    Ok(VideoMetadata {
        width,
        height,
        framerate,
        duration_micros,
        codec,
    })
}

/// One-frame thumbnail returned by [`extract_thumbnail`]. `rgba` is
/// `width * height * 4` bytes in scanline order, ready to upload to
/// gui_01's texture pipeline (`Renderer::upload_texture_rgba` →
/// `HeavyCtx::push_texture`).
#[derive(Debug, Clone)]
pub struct ThumbnailFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly-packed RGBA8 (R, G, B, A, ...). Length is
    /// `width * height * 4`.
    pub rgba: Vec<u8>,
}

/// Decode a single representative frame from a video and return it as
/// RGBA8. Used at import time to build the arrangement-view clip
/// thumbnail (gui_01 #044 `ArrangementClip.thumbnail`). The reader is
/// configured to deliver `MFVideoFormat_RGB32` (= BGRA byte order
/// under WMF's little-endian DIB convention); we swap channels into
/// RGBA at copy time so callers downstream get the standard layout
/// `Renderer::upload_texture_rgba` expects.
///
/// We don't seek — the first decodable frame is good enough for an
/// arrangement-view thumbnail (= a few seconds in is more "scene"-y
/// but adds seek complexity that the MVP doesn't need yet).
pub fn extract_thumbnail(path: &Path) -> Result<ThumbnailFrame, VideoImportError> {
    ensure_mf_startup()?;

    if !path.exists() {
        return Err(VideoImportError::IoError(format!(
            "file not found: {}",
            path.display()
        )));
    }

    let wide = to_wide(path);
    let url = PCWSTR::from_raw(wide.as_ptr());

    // ENABLE_VIDEO_PROCESSING lets the reader insert MFTs to convert
    // native (e.g. NV12 / YUV420P) to RGB32 transparently.
    let attrs = unsafe {
        let mut a = None;
        MFCreateAttributes(&mut a, 1).map_err(|e| {
            VideoImportError::DecodeFailed(format!("MFCreateAttributes: {e}"))
        })?;
        let attrs = a.ok_or_else(|| {
            VideoImportError::DecodeFailed("MFCreateAttributes returned null".into())
        })?;
        attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(|e| {
                VideoImportError::DecodeFailed(format!(
                    "SetUINT32 ENABLE_VIDEO_PROCESSING: {e}"
                ))
            })?;
        attrs
    };
    let reader: IMFSourceReader = unsafe {
        MFCreateSourceReaderFromURL(url, &attrs).map_err(|e| {
            VideoImportError::DecodeFailed(format!(
                "MFCreateSourceReaderFromURL({}): {e}",
                path.display()
            ))
        })?
    };

    let video_stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    // Read native FRAME_SIZE so we know what the converted RGB32
    // frame's dimensions will be. WMF preserves the native size unless
    // the output type explicitly sets a different one.
    let native_type = unsafe { reader.GetNativeMediaType(video_stream, 0) }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!(
                "thumbnail GetNativeMediaType: {e}"
            ))
        })?;
    let frame_size = unsafe { native_type.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!("thumbnail FRAME_SIZE: {e}"))
        })?;
    let width = (frame_size >> 32) as u32;
    let height = (frame_size & 0xFFFF_FFFF) as u32;
    if width == 0 || height == 0 {
        return Err(VideoImportError::DecodeFailed(format!(
            "thumbnail invalid frame size {width}x{height}"
        )));
    }

    // Request RGB32 output. The reader inserts a video processor MFT
    // to convert native (likely NV12) → BGRA8. We swap to RGBA below.
    let output_type = unsafe {
        let t = MFCreateMediaType().map_err(|e| {
            VideoImportError::DecodeFailed(format!("thumbnail MFCreateMediaType: {e}"))
        })?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| {
                VideoImportError::DecodeFailed(format!(
                    "thumbnail set MAJOR_TYPE Video: {e}"
                ))
            })?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
            .map_err(|e| {
                VideoImportError::DecodeFailed(format!(
                    "thumbnail set SUBTYPE RGB32: {e}"
                ))
            })?;
        t
    };
    unsafe { reader.SetCurrentMediaType(video_stream, None, &output_type) }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!(
                "thumbnail SetCurrentMediaType RGB32: {e}"
            ))
        })?;

    // Drain ReadSample until we get a real sample (skip STREAMTICK
    // gaps). Bail with an error if we reach EOS without any sample.
    let frame_bytes = width as usize * height as usize * 4;
    let mut rgba: Vec<u8> = Vec::with_capacity(frame_bytes);

    loop {
        let mut flags: u32 = 0;
        let mut sample: Option<windows::Win32::Media::MediaFoundation::IMFSample> =
            None;
        unsafe {
            reader.ReadSample(
                video_stream,
                0,
                None,
                Some(&mut flags),
                None,
                Some(&mut sample),
            )
        }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!("thumbnail ReadSample: {e}"))
        })?;

        if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            return Err(VideoImportError::DecodeFailed(
                "thumbnail: end of stream before any frame decoded".into(),
            ));
        }
        let Some(sample) = sample else {
            continue;
        };

        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|e| {
                VideoImportError::DecodeFailed(format!(
                    "thumbnail ConvertToContiguousBuffer: {e}"
                ))
            })?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        unsafe { buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len)) }
            .map_err(|e| {
                VideoImportError::DecodeFailed(format!("thumbnail Lock: {e}"))
            })?;

        if ptr.is_null() || (cur_len as usize) < frame_bytes {
            let _ = unsafe { buffer.Unlock() };
            return Err(VideoImportError::DecodeFailed(format!(
                "thumbnail frame too small: {cur_len} < {frame_bytes}"
            )));
        }

        // BGRA8 → RGBA8 swap. MF_MT_DEFAULT_STRIDE could in theory be
        // larger than width*4 (= row padding) — for RGB32 in WMF it's
        // almost always exactly width*4 but we defensively only read
        // `frame_bytes`. A more robust implementation would query
        // MF_MT_DEFAULT_STRIDE and copy row-by-row.
        let src = unsafe { std::slice::from_raw_parts(ptr, frame_bytes) };
        rgba.clear();
        rgba.reserve_exact(frame_bytes);
        for px in src.chunks_exact(4) {
            // src order: B, G, R, A → push R, G, B, A
            rgba.push(px[2]);
            rgba.push(px[1]);
            rgba.push(px[0]);
            rgba.push(px[3]);
        }

        let _ = unsafe { buffer.Unlock() };
        break;
    }

    Ok(ThumbnailFrame {
        width,
        height,
        rgba,
    })
}

/// Metadata returned by [`extract_audio_to_wav`] when the source has
/// an audio stream. Mirrors the relevant subset of
/// [`common::model::AudioSource`].
#[derive(Debug, Clone, Copy)]
pub struct ExtractedAudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    /// Total PCM frames written (per channel; not bytes).
    pub frames: u64,
}

/// Open `src` with WMF, find the first audio stream, ask the source
/// reader to deliver PCM Float32 (= the audio engine's native format),
/// and write the decoded samples into `dst_wav` via `hound`. Returns
/// `Ok(None)` when the source has no selectable audio stream; in that
/// case `dst_wav` is NOT created.
///
/// MFAudioFormat_Float in WMF is little-endian IEEE-754 32-bit. The
/// hound writer is configured with `bits_per_sample: 32` and
/// `sample_format: Float` so the output is interchangeable with any
/// other Float WAV daw_01 already handles (see
/// `import_audio::decode_wav`).
pub fn extract_audio_to_wav(
    src: &Path,
    dst_wav: &Path,
) -> Result<Option<ExtractedAudioInfo>, VideoImportError> {
    ensure_mf_startup()?;

    if !src.exists() {
        return Err(VideoImportError::IoError(format!(
            "file not found: {}",
            src.display()
        )));
    }

    let wide = to_wide(src);
    let url = PCWSTR::from_raw(wide.as_ptr());
    let reader: IMFSourceReader = unsafe {
        MFCreateSourceReaderFromURL(url, None).map_err(|e| {
            VideoImportError::DecodeFailed(format!(
                "MFCreateSourceReaderFromURL({}): {e}",
                src.display()
            ))
        })?
    };

    let audio_stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;

    // Probe the native audio media type. If the call fails the source
    // probably has no audio stream — return Ok(None) instead of
    // erroring so callers can treat video-only inputs uniformly.
    let native = match unsafe { reader.GetNativeMediaType(audio_stream, 0) } {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let channels = unsafe { native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }
        .map_err(|e| VideoImportError::DecodeFailed(format!("audio channels: {e}")))?
        as u16;
    let sample_rate = unsafe { native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }
        .map_err(|e| VideoImportError::DecodeFailed(format!("audio sample_rate: {e}")))?;

    // Build the desired output type: PCM Float32 at the source's
    // native rate / channels. Letting WMF insert the resampler MFT
    // would normalise to 48 kHz stereo, but for import-time extract
    // we want bit-identical content where possible — preserve the
    // source rate / channels so downstream code can resample on the
    // playback path if needed.
    let output_type = unsafe {
        let t = MFCreateMediaType().map_err(|e| {
            VideoImportError::DecodeFailed(format!("MFCreateMediaType: {e}"))
        })?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(|e| VideoImportError::DecodeFailed(format!("set MAJOR_TYPE: {e}")))?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)
            .map_err(|e| VideoImportError::DecodeFailed(format!("set SUBTYPE: {e}")))?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32)
            .map_err(|e| {
                VideoImportError::DecodeFailed(format!("set BITS_PER_SAMPLE: {e}"))
            })?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)
            .map_err(|e| {
                VideoImportError::DecodeFailed(format!("set NUM_CHANNELS: {e}"))
            })?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)
            .map_err(|e| {
                VideoImportError::DecodeFailed(format!("set SAMPLES_PER_SECOND: {e}"))
            })?;
        t
    };
    unsafe { reader.SetCurrentMediaType(audio_stream, None, &output_type) }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!(
                "SetCurrentMediaType(audio, Float32 {channels}ch {sample_rate}Hz): {e}"
            ))
        })?;

    // Deselect video so the reader doesn't decode frames we don't
    // need. Best-effort — older sources sometimes refuse, in which
    // case we still drain the audio stream below.
    let _ = unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, false)
    };
    unsafe { reader.SetStreamSelection(audio_stream, true) }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!("SetStreamSelection(audio): {e}"))
        })?;

    // Now drain samples into the WAV.
    if let Some(parent) = dst_wav.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            VideoImportError::IoError(format!(
                "create_dir_all {}: {e}",
                parent.display()
            ))
        })?;
    }
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(dst_wav, spec).map_err(|e| {
        VideoImportError::IoError(format!("hound::WavWriter::create: {e}"))
    })?;

    let mut frames_written: u64 = 0;
    loop {
        let mut flags: u32 = 0;
        let mut sample: Option<windows::Win32::Media::MediaFoundation::IMFSample> =
            None;
        unsafe {
            reader.ReadSample(
                audio_stream,
                0,
                None,
                Some(&mut flags),
                None,
                Some(&mut sample),
            )
        }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!("ReadSample(audio): {e}"))
        })?;
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
        let Some(sample) = sample else {
            // Spurious read — gap or format change. Keep looping.
            continue;
        };

        // ConvertToContiguousBuffer collapses the (possibly split)
        // sample storage into a single IMFMediaBuffer we can lock.
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|e| {
                VideoImportError::DecodeFailed(format!(
                    "ConvertToContiguousBuffer: {e}"
                ))
            })?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        unsafe {
            buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))
        }
        .map_err(|e| {
            VideoImportError::DecodeFailed(format!("IMFMediaBuffer::Lock: {e}"))
        })?;

        if !ptr.is_null() && cur_len > 0 {
            let bytes = unsafe {
                std::slice::from_raw_parts(ptr, cur_len as usize)
            };
            // Float32 little-endian. chunks_exact(4) so a partial tail
            // byte (which shouldn't happen for a Float stream but
            // we're defensive) doesn't panic in `f32::from_le_bytes`.
            for chunk in bytes.chunks_exact(4) {
                let s = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                writer.write_sample(s).map_err(|e| {
                    VideoImportError::IoError(format!("hound write: {e}"))
                })?;
            }
            // One "frame" = `channels` samples; count whole frames
            // written so the AudioSource entry can report the right
            // length downstream.
            frames_written += (cur_len as u64 / 4) / channels as u64;
        }

        let _ = unsafe { buffer.Unlock() };
    }

    writer
        .finalize()
        .map_err(|e| VideoImportError::IoError(format!("hound finalize: {e}")))?;

    Ok(Some(ExtractedAudioInfo {
        sample_rate,
        channels,
        frames: frames_written,
    }))
}

/// Successful import result. The video has been copied into the
/// project (or import_cache, when `project_dir` was `None`), metadata
/// has been read, and — when the source carried an audio stream — the
/// audio has been extracted to a paired WAV that is itself ready to
/// flow through the existing `AudioSource` pipeline. The caller (=
/// AppData handler in P2.6) allocates `AudioSourceId` /
/// `VideoSourceId`, links them via `VideoSource.audio_source_id`, and
/// registers them on `Song.{audio_sources, video_sources}`.
pub struct ImportedVideo {
    /// Model entry to insert under a fresh `VideoSourceId`. The
    /// `audio_source_id` field is left `None`; the caller fills it in
    /// after allocating the paired AudioSource id.
    pub video_source: VideoSource,
    /// File-stem of the input, used as the auto-created clip / track
    /// name (no hash suffix). The on-disk sample filename always
    /// carries the hash regardless.
    pub display_name: String,
    /// Paired audio extracted from the source. `None` when the input
    /// had no audio stream (= `extract_audio_to_wav` returned `None`).
    pub audio: Option<crate::import_audio::ImportedAudio>,
    /// First-frame RGBA8 thumbnail. `None` when WMF could not decode a
    /// representative frame (= rare; we accept import success without
    /// a thumbnail rather than fail the whole import). The caller
    /// queues this for GPU texture upload in `Runner` (P3.5).
    pub thumbnail: Option<ThumbnailFrame>,
}

/// One-shot import helper: hash → copy video into samples/ → metadata
/// → audio extract (when present) → decode the extracted WAV into an
/// `AudioSourceBuffer`. Mirrors the layered signature of
/// `import_audio::import_one` so the worker thread doesn't have to
/// reach into individual helpers.
///
/// `project_dir = Some(dir)`: video is copied to `<dir>/samples/...`,
/// path stored as `VideoSourcePath::ProjectRelative("samples/...")`.
/// `project_dir = None`: video goes to the unsaved-project import
/// cache (shared with audio import via
/// [`crate::import_audio::unsaved_import_cache_dir`]) and the path is
/// stored as `VideoSourcePath::Absolute(absolute_cache_path)`.
pub fn import_one_video(
    src: &Path,
    project_dir: Option<&Path>,
) -> Result<ImportedVideo, VideoImportError> {
    use crate::import_audio::{copy_into_dir, decode_wav, unsaved_import_cache_dir};
    use common::model::AudioSource;

    // Read metadata BEFORE copying so unsupported / corrupt sources
    // surface their error before we bother filling the samples dir.
    let metadata = extract_metadata(src)?;
    let hash8 = file_hash8(src).map_err(|e| {
        VideoImportError::IoError(format!("hash {}: {}", src.display(), e))
    })?;
    let video_filename = samples_filename(src, &hash8);

    // Copy the source video into the project sample pool (or the
    // unsaved-project import cache). The audio extract below writes
    // into the same directory so all per-import artifacts cluster
    // together.
    let (video_path_kind, target_dir) = match project_dir {
        Some(dir) => {
            let samples_dir = dir.join("samples");
            copy_into_dir(src, &samples_dir, &video_filename).map_err(|e| {
                VideoImportError::IoError(format!("copy video into samples/: {e}"))
            })?;
            (
                VideoSourcePath::ProjectRelative(
                    std::path::PathBuf::from("samples").join(&video_filename),
                ),
                samples_dir,
            )
        }
        None => {
            let cache = unsaved_import_cache_dir();
            let dst = copy_into_dir(src, &cache, &video_filename).map_err(|e| {
                VideoImportError::IoError(format!(
                    "copy video into import_cache: {e}"
                ))
            })?;
            (VideoSourcePath::Absolute(dst), cache)
        }
    };

    // Audio extract goes into a paired .wav next to the video, sharing
    // the same hash suffix so we can find / dedup it just like normal
    // audio imports.
    let audio_filename = {
        let stem = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("audio");
        let sanitized: String = stem
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("{sanitized}_{hash8}.wav")
    };
    let audio_dst = target_dir.join(&audio_filename);

    let audio_info = extract_audio_to_wav(src, &audio_dst)?;
    let audio = if let Some(info) = audio_info {
        let buffer = decode_wav(&audio_dst).map_err(|e| {
            VideoImportError::IoError(format!(
                "decode extracted WAV {}: {e}",
                audio_dst.display()
            ))
        })?;
        let source_path = match project_dir {
            Some(_) => common::model::AudioSourcePath::ProjectRelative(
                std::path::PathBuf::from("samples").join(&audio_filename),
            ),
            None => common::model::AudioSourcePath::Absolute(audio_dst),
        };
        let source = AudioSource {
            path: source_path,
            sample_rate: info.sample_rate,
            channels: info.channels,
            frames: info.frames,
            original_bpm: None,
            root_key: None,
        };
        let display_name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Video Audio")
            .to_string();
        Some(crate::import_audio::ImportedAudio {
            buffer: std::sync::Arc::new(buffer),
            source,
            display_name,
        })
    } else {
        // No audio stream in the source — leave any preemptively-
        // created .wav in place is fine, but extract_audio_to_wav
        // returns Ok(None) BEFORE opening the writer so no file
        // exists to clean up here.
        None
    };

    let video_source = VideoSource {
        path: video_path_kind,
        width: metadata.width,
        height: metadata.height,
        framerate: metadata.framerate,
        duration_micros: metadata.duration_micros,
        codec: metadata.codec,
        audio_source_id: None, // caller fills in after registering audio
    };
    let display_name = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Video Clip")
        .to_string();

    // Thumbnail is best-effort: a decode failure here shouldn't abort
    // the whole import (audio is already extracted, video metadata is
    // valid). The arrangement view falls back to a solid color when
    // `thumbnail = None`.
    let thumbnail = match extract_thumbnail(src) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(error = %e, path = %src.display(), "thumbnail extract failed");
            None
        }
    };

    Ok(ImportedVideo {
        video_source,
        display_name,
        audio,
        thumbnail,
    })
}

/// Best-effort helper: the canonical video extensions daw_01 supports
/// at import time. Used by the File menu / drag-and-drop dispatcher to
/// decide whether to route a file to `import_audio` or `import_video`.
pub fn looks_like_video(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("mp4" | "mov" | "mkv" | "webm" | "m4v" | "avi")
    )
}

/// Sanity check: file hash + samples filename builder reused from
/// `import_audio` so the on-disk naming scheme is the same for video.
/// Exposed here so the import worker thread doesn't reach across
/// modules.
pub fn project_filename(src: &Path, hash8: &str) -> String {
    let _ = file_hash8; // re-export tether — see imports above
    samples_filename(src, hash8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn looks_like_video_recognises_common_containers() {
        assert!(looks_like_video(&PathBuf::from("clip.mp4")));
        assert!(looks_like_video(&PathBuf::from("clip.MOV")));
        assert!(looks_like_video(&PathBuf::from("clip.mkv")));
        assert!(looks_like_video(&PathBuf::from("clip.webm")));
        assert!(!looks_like_video(&PathBuf::from("track.wav")));
        assert!(!looks_like_video(&PathBuf::from("clip")));
    }

    #[test]
    fn extract_metadata_errors_on_missing_file() {
        let err = extract_metadata(&PathBuf::from("Z:/no/such/file.mp4")).unwrap_err();
        assert!(matches!(err, VideoImportError::IoError(_)));
    }

    /// Smoke test: generates a 2-second 320x240 H.264 mp4 via the
    /// `ffmpeg` CLI in a temp dir, then verifies WMF reports the
    /// expected metadata back. Skipped if `ffmpeg` is not on PATH (=
    /// the test harness emits a `cargo:warning` but does not fail).
    /// This is the canonical end-to-end check that the
    /// `IMFSourceReader` + `MF_PD_DURATION` path actually links and
    /// runs on the dev machine.
    #[test]
    fn extract_metadata_reads_h264_mp4_fixture() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!(
                "extract_metadata_reads_h264_mp4_fixture: ffmpeg not on PATH, skipping"
            );
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("smoke.mp4");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "testsrc=duration=1:size=320x240:rate=30",
                "-c:v", "libx264",
                "-pix_fmt", "yuv420p",
                "-y",
                mp4.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("ffmpeg run");
        assert!(status.success(), "ffmpeg failed to produce fixture");

        let md = extract_metadata(&mp4).expect("extract_metadata");
        assert_eq!(md.width, 320, "width");
        assert_eq!(md.height, 240, "height");
        assert!(
            (md.framerate - 30.0).abs() < 0.5,
            "framerate ~30 (got {})",
            md.framerate
        );
        assert!(
            md.duration_micros > 800_000 && md.duration_micros < 1_200_000,
            "duration ~1s in micros (got {})",
            md.duration_micros
        );
        assert_eq!(md.codec, "h264", "codec");
    }

    fn locate_ffmpeg() -> Option<std::path::PathBuf> {
        let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(exe))
                .find(|p| p.is_file())
        })
    }

    /// End-to-end check for the audio extract path: build a 1-second
    /// mp4 containing a 440Hz sine via the ffmpeg CLI, ask WMF to
    /// decode it to Float32 PCM in a .wav, then re-open the .wav with
    /// hound and verify the frame count matches the expected duration.
    /// Sample-rate / channel-count must equal what the source declared.
    #[test]
    fn extract_audio_to_wav_writes_pcm_float() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("extract_audio_to_wav: ffmpeg not on PATH, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("audio.mp4");
        let wav = dir.path().join("extracted.wav");

        // 1s @ 440Hz mono AAC inside an mp4 container. Use a video
        // testsrc too so the container looks like a typical phone
        // recording (= one audio stream + one video stream).
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "testsrc=duration=1:size=160x120:rate=30",
                "-f", "lavfi",
                "-i", "sine=frequency=440:duration=1:sample_rate=48000",
                "-c:v", "libx264",
                "-c:a", "aac",
                "-pix_fmt", "yuv420p",
                "-shortest",
                "-y",
                mp4.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("ffmpeg run");
        assert!(status.success(), "ffmpeg failed to build mp4");

        let info = extract_audio_to_wav(&mp4, &wav)
            .expect("extract_audio_to_wav")
            .expect("audio stream should be present");

        // sine source was 48 kHz mono.
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 1);
        // ~1s of audio = ~48k frames. AAC framing leaves a small
        // priming + tail, so allow a generous window.
        assert!(
            info.frames > 45_000 && info.frames < 55_000,
            "extracted frames ~48k (got {})",
            info.frames
        );

        // Re-open with hound; spec must match what we wrote.
        let r = hound::WavReader::open(&wav).expect("open extracted wav");
        let spec = r.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.bits_per_sample, 32);
        assert!(matches!(spec.sample_format, hound::SampleFormat::Float));
    }

    #[test]
    fn import_one_video_copies_into_samples_and_decodes_audio() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("import_one_video: ffmpeg not on PATH, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let src = dir.path().join("clip!好き.mp4");

        // Source carries audio so the import yields Some(audio).
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "testsrc=duration=1:size=160x120:rate=30",
                "-f", "lavfi",
                "-i", "sine=frequency=220:duration=1:sample_rate=48000",
                "-c:v", "libx264",
                "-c:a", "aac",
                "-pix_fmt", "yuv420p",
                "-shortest",
                "-y",
                src.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("ffmpeg run");
        assert!(status.success());

        let imported = import_one_video(&src, Some(&project)).unwrap();

        // Video metadata round-tripped.
        assert_eq!(imported.video_source.width, 160);
        assert_eq!(imported.video_source.height, 120);
        assert_eq!(imported.video_source.codec, "h264");
        assert!(imported.video_source.duration_micros > 800_000);

        // Path is `samples/<sanitized_stem>_<hash>.<ext>` — non-ASCII
        // chars get sanitized to `_` so the hash is the only varying
        // suffix.
        match &imported.video_source.path {
            VideoSourcePath::ProjectRelative(p) => {
                assert!(p.starts_with("samples"));
                assert!(p.extension().unwrap() == "mp4");
            }
            other => panic!("expected ProjectRelative, got {other:?}"),
        }

        // Thumbnail decoded at native dimensions (P3.2).
        let thumb = imported
            .thumbnail
            .as_ref()
            .expect("thumbnail should be present");
        assert_eq!(thumb.width, 160);
        assert_eq!(thumb.height, 120);
        assert_eq!(thumb.rgba.len(), 160 * 120 * 4);

        // Audio extracted, decoded, and the WAV exists on disk.
        let audio = imported.audio.expect("audio should be present");
        assert_eq!(audio.source.sample_rate, 48_000);
        assert_eq!(audio.source.channels, 1);
        assert!(audio.buffer.frames > 45_000);
        match &audio.source.path {
            common::model::AudioSourcePath::ProjectRelative(p) => {
                assert!(p.starts_with("samples"));
                assert!(p.extension().unwrap() == "wav");
                let abs = project.join(p);
                assert!(abs.exists(), "extracted .wav should exist on disk");
            }
            other => panic!("expected ProjectRelative, got {other:?}"),
        }
    }

    #[test]
    fn extract_thumbnail_reads_first_frame_as_rgba() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("extract_thumbnail: ffmpeg not on PATH, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("thumb.mp4");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "color=c=red:size=160x120:duration=1:rate=30",
                "-c:v", "libx264",
                "-pix_fmt", "yuv420p",
                "-y",
                mp4.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("ffmpeg run");
        assert!(status.success(), "ffmpeg failed to build red fixture");

        let thumb = extract_thumbnail(&mp4).expect("extract_thumbnail");
        assert_eq!(thumb.width, 160);
        assert_eq!(thumb.height, 120);
        assert_eq!(thumb.rgba.len(), 160 * 120 * 4);

        // The center pixel should be near-red (R ~ 0xFF, G ~ 0x00,
        // B ~ 0x00) regardless of YUV→RGB rounding error. Pick a
        // sample away from the frame edges where libx264 sometimes
        // leaves a couple of ringing rows after the keyframe.
        let center = (60 * 160 + 80) * 4;
        let r = thumb.rgba[center];
        let g = thumb.rgba[center + 1];
        let b = thumb.rgba[center + 2];
        let a = thumb.rgba[center + 3];
        assert!(
            r > 200 && g < 80 && b < 80,
            "center pixel should be red, got RGBA ({r}, {g}, {b}, {a})"
        );
    }

    #[test]
    fn extract_audio_returns_none_for_video_only_mp4() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("extract_audio_returns_none: ffmpeg not on PATH, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("silent.mp4");
        let wav = dir.path().join("not_written.wav");

        // Video-only: no audio stream at all.
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "testsrc=duration=1:size=160x120:rate=30",
                "-c:v", "libx264",
                "-pix_fmt", "yuv420p",
                "-an",
                "-y",
                mp4.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("ffmpeg run");
        assert!(status.success(), "ffmpeg failed to build video-only mp4");

        let result = extract_audio_to_wav(&mp4, &wav).expect("extract_audio_to_wav");
        assert!(result.is_none(), "video-only input should yield None");
        assert!(!wav.exists(), "no .wav should be created for video-only");
    }
}
