//! Video file import via in-process libav (rsmpeg) — the same decode stack
//! playback and export use (`docs/plan_video_decode_unify.md`). Media
//! Foundation was removed; libav handles metadata probe, one-frame thumbnail,
//! and audio extraction (incl. 10-bit H.264 / HEVC / AV1 that MF could not).
//!
//! Pipeline (mirrors `import_audio.rs` shape so the two co-exist cleanly):
//!
//! 1. Compute SHA-256 of the source file (first 4 bytes → 8 hex chars).
//! 2. Copy the file into `<project_dir>/samples/<basename>_<hash>.<ext>`
//!    (`Absolute` cache path when there is no project_dir yet, same as
//!    audio import — [`crate::media_dest`]).
//! 3. Probe metadata (width / height / framerate / duration / codec) via
//!    `avformat`, decode a thumbnail frame via `avcodec`, and — when the file
//!    carries an audio stream — extract the audio to a paired WAV file
//!    (`avcodec` decode + `swresample` → Float32 + `hound::WavWriter`).
//! 4. Build a `VideoSource` referencing the on-disk video path, and an
//!    optional `AudioSource` referencing the extracted WAV. The
//!    `VideoSource.audio_source_id` field carries the back-link.

use std::ffi::CString;
use std::path::Path;

use common::model::VideoSource;

use rsmpeg::avcodec::AVCodecContext;
use rsmpeg::avformat::AVFormatContextInput;
use rsmpeg::avutil::{AVChannelLayout, AVFrame};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swresample::SwrContext;

use crate::import_audio::{file_hash8, samples_filename};

/// Convert a path to a NUL-terminated `CString` for libav's `open` APIs.
fn path_cstring(path: &Path) -> Result<CString, VideoImportError> {
    CString::new(path.to_string_lossy().as_bytes().to_vec())
        .map_err(|e| VideoImportError::IoError(format!("path has interior NUL: {e}")))
}

/// Successful metadata read. `audio_source_id` is populated only when
/// the source carried an audio stream that was extracted to a paired
/// WAV (P2.4 wires this).
#[derive(Debug, Clone)]
pub struct VideoMetadata {
    pub width: u32,
    pub height: u32,
    /// Frames per second from the stream's `avg_frame_rate` (falling back to
    /// `r_frame_rate`). 0.0 indicates "unknown" (neither was reported).
    pub framerate: f32,
    /// Total stream duration in microseconds (from `AVFormatContext.duration`,
    /// which is already in AV_TIME_BASE = µs). Zero when the container didn't
    /// report a duration.
    pub duration_micros: u64,
    /// Free-form codec label ("h264" / "hevc" / "vp9" / "av1" / "unknown").
    /// Used only for display and diagnostics.
    pub codec: String,
}

#[derive(Debug)]
pub enum VideoImportError {
    UnsupportedFormat(String),
    /// libav open / probe / decode failed.
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

/// Probe a video file's first video stream via `avformat`: width / height /
/// framerate / duration / codec. No frame is decoded.
pub fn extract_metadata(path: &Path) -> Result<VideoMetadata, VideoImportError> {
    if !path.exists() {
        return Err(VideoImportError::IoError(format!(
            "file not found: {}",
            path.display()
        )));
    }
    let url = path_cstring(path)?;
    let input = AVFormatContextInput::open(&url).map_err(|e| {
        VideoImportError::DecodeFailed(format!("open {}: {e:?}", path.display()))
    })?;

    let (stream_index, codec) = input
        .find_best_stream(ffi::AVMEDIA_TYPE_VIDEO)
        .map_err(|e| VideoImportError::DecodeFailed(format!("find video stream: {e:?}")))?
        .ok_or_else(|| {
            VideoImportError::DecodeFailed(format!("no video stream in {}", path.display()))
        })?;

    let stream = &input.streams()[stream_index];
    let par = stream.codecpar();
    let width = par.width.max(0) as u32;
    let height = par.height.max(0) as u32;
    if width == 0 || height == 0 {
        return Err(VideoImportError::DecodeFailed(format!(
            "invalid frame size {width}x{height}"
        )));
    }

    // Prefer avg_frame_rate; fall back to r_frame_rate; else unknown (0.0).
    let fr = if stream.avg_frame_rate.num > 0 && stream.avg_frame_rate.den > 0 {
        stream.avg_frame_rate
    } else {
        stream.r_frame_rate
    };
    let framerate = if fr.num > 0 && fr.den > 0 {
        fr.num as f32 / fr.den as f32
    } else {
        0.0
    };

    // `AVFormatContext.duration` is in AV_TIME_BASE (= 1/1_000_000 s) units, i.e.
    // already microseconds. Negative / AV_NOPTS_VALUE → unknown (0).
    let duration_micros = if input.duration > 0 {
        input.duration as u64
    } else {
        0
    };

    // libav's canonical short codec name ("h264" / "hevc" / "vp9" / "av1" ...).
    let codec = codec.name().to_string_lossy().into_owned();

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

/// Decode a single representative frame and return it as RGBA8 for the
/// arrangement-view clip thumbnail. Reuses the single libav engine (native
/// resolution) and swaps BGRA→RGBA. The first decodable frame is used — a
/// scene-y seek isn't worth the complexity for a thumbnail.
pub fn extract_thumbnail(path: &Path) -> Result<ThumbnailFrame, VideoImportError> {
    if !path.exists() {
        return Err(VideoImportError::IoError(format!(
            "file not found: {}",
            path.display()
        )));
    }
    let mut decoder = crate::libav_decoder::LibavVideoDecoder::new();
    let frame = decoder
        .decode_at(0, path, 0)
        .map_err(VideoImportError::DecodeFailed)?;
    let rgba = crate::video_playback::bgra_to_rgba(&frame.bgra);
    Ok(ThumbnailFrame {
        width: frame.width,
        height: frame.height,
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

/// Extract the first audio stream to a paired WAV via `avcodec` decode +
/// `swresample` → interleaved Float32 + `hound`. Returns `Ok(None)` when the
/// source has no audio stream (in which case `dst_wav` is NOT created). Output
/// preserves the source's native sample rate / channel count as a Float32 WAV,
/// interchangeable with the rest of daw_01's audio pipeline.
pub fn extract_audio_to_wav(
    src: &Path,
    dst_wav: &Path,
) -> Result<Option<ExtractedAudioInfo>, VideoImportError> {
    if !src.exists() {
        return Err(VideoImportError::IoError(format!(
            "file not found: {}",
            src.display()
        )));
    }
    let url = path_cstring(src)?;
    let mut input = AVFormatContextInput::open(&url).map_err(|e| {
        VideoImportError::DecodeFailed(format!("open {}: {e:?}", src.display()))
    })?;

    // No audio stream → Ok(None) so callers treat video-only inputs uniformly.
    let Some((stream_index, codec)) = input
        .find_best_stream(ffi::AVMEDIA_TYPE_AUDIO)
        .map_err(|e| VideoImportError::DecodeFailed(format!("find audio stream: {e:?}")))?
    else {
        return Ok(None);
    };

    let mut decoder = {
        let stream = &input.streams()[stream_index];
        let mut d = AVCodecContext::new(&codec);
        d.apply_codecpar(&stream.codecpar()).map_err(|e| {
            VideoImportError::DecodeFailed(format!("apply_codecpar(audio): {e:?}"))
        })?;
        d.open(None)
            .map_err(|e| VideoImportError::DecodeFailed(format!("open audio decoder: {e:?}")))?;
        d
    };

    let sample_rate = decoder.sample_rate.max(0) as u32;
    let channels = decoder.ch_layout.nb_channels.max(0) as u16;
    if sample_rate == 0 || channels == 0 {
        return Ok(None);
    }

    // swresample: decoder-native (planar / int / …) → Float32 packed, same rate.
    // Input uses the decoder's own channel layout so no channel remap happens;
    // the output uses a fresh default layout for the same channel count.
    let out_layout = AVChannelLayout::from_nb_channels(channels as i32).into_inner();
    let mut swr = SwrContext::new(
        &out_layout,
        ffi::AV_SAMPLE_FMT_FLT,
        sample_rate as i32,
        &decoder.ch_layout,
        decoder.sample_fmt,
        sample_rate as i32,
    )
    .map_err(|e| VideoImportError::DecodeFailed(format!("swr_alloc: {e:?}")))?;
    swr.init()
        .map_err(|e| VideoImportError::DecodeFailed(format!("swr_init: {e:?}")))?;

    if let Some(parent) = dst_wav.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            VideoImportError::IoError(format!("create_dir_all {}: {e}", parent.display()))
        })?;
    }
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(dst_wav, spec)
        .map_err(|e| VideoImportError::IoError(format!("hound::WavWriter::create: {e}")))?;

    let audio_stream = stream_index as i32;
    let mut frames_written: u64 = 0;
    let mut eof = false;
    'outer: loop {
        // Drain all frames currently available from the decoder.
        loop {
            match decoder.receive_frame() {
                Ok(frame) => {
                    let mut out = AVFrame::new();
                    out.set_format(ffi::AV_SAMPLE_FMT_FLT);
                    out.set_ch_layout(
                        AVChannelLayout::from_nb_channels(channels as i32).into_inner(),
                    );
                    out.set_sample_rate(sample_rate as i32);
                    out.set_nb_samples(frame.nb_samples);
                    out.alloc_buffer().map_err(|e| {
                        VideoImportError::DecodeFailed(format!("out frame alloc: {e:?}"))
                    })?;
                    swr.convert_frame(Some(&frame), &mut out).map_err(|e| {
                        VideoImportError::DecodeFailed(format!("swr convert: {e:?}"))
                    })?;
                    let n = out.nb_samples.max(0) as usize;
                    write_flt_frame(&mut writer, &out, n, channels as usize)?;
                    frames_written += n as u64;
                }
                Err(RsmpegError::DecoderDrainError) => break, // need more input
                Err(RsmpegError::DecoderFlushedError) => break 'outer, // fully drained
                Err(e) => {
                    return Err(VideoImportError::DecodeFailed(format!(
                        "receive_frame(audio): {e:?}"
                    )));
                }
            }
        }
        if eof {
            // Already flushed; the drain loop above will hit DecoderFlushedError.
            continue;
        }
        match input
            .read_packet()
            .map_err(|e| VideoImportError::DecodeFailed(format!("read_packet: {e:?}")))?
        {
            Some(packet) => {
                if packet.stream_index == audio_stream {
                    decoder.send_packet(Some(&packet)).map_err(|e| {
                        VideoImportError::DecodeFailed(format!("send_packet(audio): {e:?}"))
                    })?;
                }
            }
            None => {
                eof = true;
                decoder.send_packet(None).map_err(|e| {
                    VideoImportError::DecodeFailed(format!("flush send_packet: {e:?}"))
                })?;
            }
        }
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

/// Write one interleaved Float32 audio frame (`nb_samples` per channel) to the
/// WAV writer. `out.data[0]` is `nb_samples * channels` packed f32.
fn write_flt_frame(
    writer: &mut hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    out: &AVFrame,
    nb_samples: usize,
    channels: usize,
) -> Result<(), VideoImportError> {
    let plane = out.data[0];
    if plane.is_null() || nb_samples == 0 {
        return Ok(());
    }
    let total = nb_samples * channels;
    // SAFETY: the FLT packed plane holds `nb_samples * channels` contiguous f32
    // (alloc_buffer sized it from nb_samples + ch_layout).
    let samples = unsafe { std::slice::from_raw_parts(plane as *const f32, total) };
    for &s in samples {
        writer
            .write_sample(s)
            .map_err(|e| VideoImportError::IoError(format!("hound write: {e}")))?;
    }
    Ok(())
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
    /// First-frame RGBA8 thumbnail. `None` when libav could not decode a
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
/// The video and its paired WAV go to `dest` ([`crate::media_dest::MediaDest`]):
/// a saved project's `samples/` (`ProjectRelative("samples/...")`) or the
/// unsaved-project import cache (`Absolute(absolute_cache_path)`).
pub fn import_one_video(
    src: &Path,
    dest: &crate::media_dest::MediaDest,
) -> Result<ImportedVideo, VideoImportError> {
    use crate::import_audio::copy_into_dir;
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
    let target_dir = dest.dir();
    copy_into_dir(src, &target_dir, &video_filename).map_err(|e| {
        VideoImportError::IoError(format!("copy video into {}: {e}", target_dir.display()))
    })?;

    // Audio extract goes into a paired .wav next to the video, sharing
    // the same hash suffix so we can find / dedup it just like normal
    // audio imports.
    let audio_filename = samples_filename(&src.with_extension("wav"), &hash8);
    let audio = extract_paired_audio(src, &target_dir.join(&audio_filename))?.map(|buffer| {
        let source = AudioSource {
            path: dest.audio_path(&audio_filename),
            sample_rate: buffer.sample_rate,
            channels: buffer.channels,
            frames: buffer.frames,
            original_bpm: None,
            root_key: None,
        };
        let display_name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Video Audio")
            .to_string();
        crate::import_audio::ImportedAudio {
            buffer: std::sync::Arc::new(buffer),
            source,
            display_name,
        }
    });

    let video_source = VideoSource {
        path: dest.video_path(&video_filename),
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

/// 動画の対の WAV (`dst`) を抜き出して decode する。音声ストリームが無ければ `None`。
///
/// 取り込みのたびに抜き直し、一時ファイルへ書き切ってから `dst` を置き換える
/// ([`common::atomic_file`])。以前は `dst` へ直接抜き出していたので、同じ動画を並行して
/// 取り込むと (名前は動画の内容 hash なので同じ `dst`)、片方の `WavWriter::create` (truncate)
/// がもう片方の書き終えた WAV を 0 byte に戻し、そちらの decode が 0 byte / 長さ欄 0 のヘッダを
/// 読んで `no suitable format reader` / `wav: missing data chunk` で落ちるか、書きかけの途中まで
/// を黙って短い音声として読んだ。
///
/// 既存の `dst` を完成品として使い回さないのは、中身が名前 (動画の hash) ではなく **抜き出し方**
/// で決まる派生物だから — 旧版がクラッシュで残した書きかけや、抜き出し方を直す前の出力を
/// 黙って掴み続けてしまう (`voicevox_cache::put_inner` と同じ理由)。
fn extract_paired_audio(
    src: &Path,
    dst: &Path,
) -> Result<Option<crate::audio_source_cache::AudioSourceBuffer>, VideoImportError> {
    let pending = common::atomic_file::PendingFile::new(dst);
    if extract_audio_to_wav(src, pending.path())?.is_none() {
        return Ok(None);
    }
    pending
        .publish_replace()
        .map_err(|e| VideoImportError::IoError(format!("publish {}: {e}", dst.display())))?;
    crate::import_audio::decode_audio(dst).map(Some).map_err(|e| {
        VideoImportError::IoError(format!("decode extracted WAV {}: {e}", dst.display()))
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
    // 公開前整備: fixture 用 ffmpeg の解決とエンコーダ指定は `crate::test_ffmpeg` が SSoT。
    // 以前は 3 モジュールが同じ helper を各自に持ち、いずれも PATH の ffmpeg を探していた。
    use crate::test_ffmpeg::{locate_ffmpeg, skip_reason, H264_ENCODER};
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
            eprintln!("{}", skip_reason("extract_metadata_reads_h264_mp4_fixture"));
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("smoke.mp4");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "testsrc=duration=1:size=320x240:rate=30",
                "-c:v", H264_ENCODER,
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


    /// End-to-end check for the audio extract path: build a 1-second
    /// mp4 containing a 440Hz sine via the ffmpeg CLI, ask WMF to
    /// decode it to Float32 PCM in a .wav, then re-open the .wav with
    /// hound and verify the frame count matches the expected duration.
    /// Sample-rate / channel-count must equal what the source declared.
    #[test]
    fn extract_audio_to_wav_writes_pcm_float() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("{}", skip_reason("extract_audio_to_wav"));
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
                "-c:v", H264_ENCODER,
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
            eprintln!("{}", skip_reason("import_one_video"));
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
                "-c:v", H264_ENCODER,
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

        let dest = crate::media_dest::MediaDest::bundle(&project, crate::media_dest::MediaPool::Samples);
        let imported = import_one_video(&src, &dest).unwrap();

        // Video metadata round-tripped.
        assert_eq!(imported.video_source.width, 160);
        assert_eq!(imported.video_source.height, 120);
        assert_eq!(imported.video_source.codec, "h264");
        assert!(imported.video_source.duration_micros > 800_000);

        // Path is `samples/<sanitized_stem>_<hash>.<ext>` — non-ASCII
        // chars get sanitized to `_` so the hash is the only varying
        // suffix.
        match &imported.video_source.path {
            common::model::VideoSourcePath::ProjectRelative(p) => {
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

    /// 1 秒の映像 + 220Hz mono 音声の mp4 を `path` に作る。
    fn write_video_with_audio(ffmpeg: &Path, path: &Path) {
        let status = std::process::Command::new(ffmpeg)
            .args([
                "-f", "lavfi", "-i", "testsrc=duration=1:size=160x120:rate=30",
                "-f", "lavfi", "-i", "sine=frequency=220:duration=1:sample_rate=48000",
                "-c:v", H264_ENCODER, "-c:a", "aac", "-pix_fmt", "yuv420p", "-shortest", "-y",
                path.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("ffmpeg run");
        assert!(status.success(), "ffmpeg failed to build mp4");
    }

    /// 対の WAV の名前に **読めないファイル** (旧版が抜き出し途中で落ちて残した、長さ欄 0 の
    /// ヘッダだけの WAV) が居ても、完成品として掴まずに抜き直して置き換える。
    #[test]
    fn import_one_video_reextracts_an_unreadable_paired_wav() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("{}", skip_reason("import_one_video_reextracts"));
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mp4");
        write_video_with_audio(&ffmpeg, &src);
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let hash8 = file_hash8(&src).unwrap();
        let paired = cache.join(samples_filename(&src.with_extension("wav"), &hash8));
        // hound が finalize 前に置くのと同じ、RIFF / data の長さ欄が 0 の float mono ヘッダ
        // (symphonia は `wav: missing data chunk` を返す = 観測した flake と同じ状態)。
        let mut torn = Vec::new();
        for part in [&b"RIFF"[..], &0u32.to_le_bytes(), b"WAVEfmt ", &16u32.to_le_bytes()] {
            torn.extend_from_slice(part);
        }
        torn.extend_from_slice(&3u16.to_le_bytes()); // WAVE_FORMAT_IEEE_FLOAT
        torn.extend_from_slice(&1u16.to_le_bytes());
        torn.extend_from_slice(&48_000u32.to_le_bytes());
        torn.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
        torn.extend_from_slice(&4u16.to_le_bytes());
        torn.extend_from_slice(&32u16.to_le_bytes());
        torn.extend_from_slice(b"data");
        torn.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&paired, torn).unwrap();
        assert!(crate::import_audio::decode_audio(&paired).is_err(), "前提: 読めない");

        let imported =
            import_one_video(&src, &crate::media_dest::MediaDest::unsaved(&cache)).unwrap();
        let audio = imported.audio.expect("audio should be present");
        assert!(audio.buffer.frames > 45_000, "抜き直した音声: {}", audio.buffer.frames);
        let on_disk = crate::import_audio::decode_audio(&paired).expect("置き換え後は読める");
        assert_eq!(on_disk.frames, audio.buffer.frames);
    }

    #[test]
    fn extract_thumbnail_reads_first_frame_as_rgba() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("{}", skip_reason("extract_thumbnail"));
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("thumb.mp4");
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "color=c=red:size=160x120:duration=1:rate=30",
                "-c:v", H264_ENCODER,
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
        // sample away from the frame edges where a lossy H.264 encoder
        // sometimes leaves a couple of ringing rows after the keyframe.
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
            eprintln!("{}", skip_reason("extract_audio_returns_none"));
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
                "-c:v", H264_ENCODER,
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
