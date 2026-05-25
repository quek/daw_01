//! MP4 render via Windows Media Foundation (`docs/plan_video.md` P8).
//!
//! Iterates output frames at `Song.video_framerate`, composites every
//! active `VideoEvent` at the corresponding playhead beat (same logic
//! as `video_playback::active_sources_at`), CPU-blends layers into
//! one RGBA buffer, converts to NV12, and writes through
//! `IMFSinkWriter` to an on-disk H.264 mp4. When an `audio_wav_path`
//! is provided, also opens it via `hound`, encodes PCM Float32 to AAC
//! via the same sink writer, and muxes a single output mp4.
//!
//! MVP scope (simplest viable):
//! - H.264 video at project resolution / framerate, ~5 Mbit/s
//! - AAC stereo audio at 192 Kbit/s (when wav supplied)
//! - Synchronous render on the GUI thread (= UI hangs for the
//!   duration). Real-time hiccup acceptable for offline render.
//! - Linear / s-curve alpha blend identical to preview composite.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use common::model::{Song, VideoSourceId, VideoSourcePath};
use windows::Win32::Media::MediaFoundation::{
    IMFSinkWriter, MFAudioFormat_AAC, MFAudioFormat_Float, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Audio,
    MFMediaType_Video, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
};
use windows::core::PCWSTR;

use crate::video_playback::VideoPlaybackEngine;

/// Render configuration. All fields are derived from `Song` except
/// `output_path` / `audio_wav_path` which the caller supplies from
/// the export dialog.
pub struct RenderConfig<'a> {
    pub song: &'a Song,
    pub project_dir: Option<&'a Path>,
    pub output_path: &'a Path,
    /// Optional sibling WAV produced by `Export WAV...`. When `Some`,
    /// the output mp4 carries an AAC audio track muxed in by the same
    /// `IMFSinkWriter` pass.
    pub audio_wav_path: Option<&'a Path>,
    /// Average video bitrate in bits-per-second. ~5 Mbit/s is a sane
    /// default for 1080p MV-style content (= visually transparent on
    /// most YouTube uploads, file size around 40 MB / minute).
    pub video_bitrate: u32,
    /// Average AAC audio bitrate. 192 kbit/s is the YouTube / SoundCloud
    /// upload sweet spot for music.
    pub audio_bitrate: u32,
}

impl<'a> RenderConfig<'a> {
    pub fn new(song: &'a Song, output_path: &'a Path) -> Self {
        Self {
            song,
            project_dir: None,
            output_path,
            audio_wav_path: None,
            video_bitrate: 5_000_000,
            audio_bitrate: 192_000,
        }
    }

    pub fn with_project_dir(mut self, dir: Option<&'a Path>) -> Self {
        self.project_dir = dir;
        self
    }

    pub fn with_audio_wav(mut self, path: Option<&'a Path>) -> Self {
        self.audio_wav_path = path;
        self
    }
}

/// Render the project to an mp4 at `cfg.output_path`. Synchronous,
/// runs to completion (or returns Err on the first stream-level
/// failure). Progress callbacks: not exposed on this signature
/// — the caller can wrap the call in a thread with a status_message
/// poll if they want a UI progress bar. For MVP we just block the
/// GUI thread.
pub fn render_mp4(cfg: &RenderConfig) -> Result<RenderStats, String> {
    crate::import_video::ensure_mf_startup_pub()
        .map_err(|e| format!("MFStartup: {e}"))?;

    let (out_w, out_h) = cfg.song.video_resolution;
    if out_w == 0 || out_h == 0 {
        return Err(format!(
            "invalid project video_resolution {out_w}x{out_h}"
        ));
    }
    let framerate = cfg.song.video_framerate;
    if framerate <= 0.0 {
        return Err(format!("invalid project video_framerate {framerate}"));
    }
    if cfg.song.bpm <= 0.0 {
        return Err(format!("invalid project bpm {}", cfg.song.bpm));
    }

    // Sink writer + video stream + (optional) audio stream.
    let wide: Vec<u16> = cfg
        .output_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let url = PCWSTR::from_raw(wide.as_ptr());
    let writer: IMFSinkWriter = unsafe {
        MFCreateSinkWriterFromURL(url, None, None)
            .map_err(|e| format!("MFCreateSinkWriterFromURL({}): {e}", cfg.output_path.display()))?
    };

    let video_stream = add_video_stream(&writer, out_w, out_h, framerate, cfg.video_bitrate)?;

    // Optional audio stream: prepared eagerly so AddStream/SetInputMediaType
    // happens before BeginWriting (= WMF requires that order).
    let audio = if let Some(wav_path) = cfg.audio_wav_path {
        let reader = hound::WavReader::open(wav_path).map_err(|e| {
            format!(
                "open audio wav {}: {e}",
                wav_path.display()
            )
        })?;
        let spec = reader.spec();
        if spec.sample_format != hound::SampleFormat::Float
            || spec.bits_per_sample != 32
        {
            return Err(format!(
                "audio wav must be PCM Float32 (got {:?} {}-bit)",
                spec.sample_format, spec.bits_per_sample
            ));
        }
        let audio_stream = add_audio_stream(
            &writer,
            spec.sample_rate,
            spec.channels as u32,
            cfg.audio_bitrate,
        )?;
        Some(AudioContext {
            stream: audio_stream,
            sample_rate: spec.sample_rate,
            channels: spec.channels as u32,
            reader,
        })
    } else {
        None
    };

    unsafe { writer.BeginWriting() }
        .map_err(|e| format!("BeginWriting: {e}"))?;

    // Frame loop.
    let mut engine = VideoPlaybackEngine::new();
    let total_seconds = beat_to_seconds(cfg.song.length_beats, cfg.song.bpm);
    let total_frames = (total_seconds * f64::from(framerate)).ceil() as u64;
    let frame_duration_100ns = (10_000_000.0_f64 / f64::from(framerate)).round() as i64;
    let mut composite = vec![0u8; out_w as usize * out_h as usize * 4];
    let mut nv12 = vec![0u8; out_w as usize * out_h as usize * 3 / 2];
    for frame_index in 0..total_frames {
        let frame_seconds = frame_index as f64 / f64::from(framerate);
        let playhead_beat = seconds_to_beat(frame_seconds, cfg.song.bpm);
        render_frame_composite(
            cfg.song,
            cfg.project_dir,
            &mut engine,
            playhead_beat,
            out_w,
            out_h,
            &mut composite,
        );
        rgba_to_nv12(&composite, out_w as usize, out_h as usize, &mut nv12);
        write_video_sample(
            &writer,
            video_stream,
            &nv12,
            i64::try_from(frame_index).map_err(|e| format!("frame_index overflow: {e}"))?
                * frame_duration_100ns,
            frame_duration_100ns,
        )?;
    }

    if let Some(mut audio_ctx) = audio {
        write_all_audio_samples(&writer, &mut audio_ctx)?;
    }

    unsafe { writer.Finalize() }
        .map_err(|e| format!("Finalize: {e}"))?;

    Ok(RenderStats {
        frames_written: total_frames,
        output_path: cfg.output_path.to_path_buf(),
    })
}

/// Returned by `render_mp4` so the caller can populate `status_message`
/// with a useful "wrote N frames to PATH" line.
#[derive(Debug, Clone)]
pub struct RenderStats {
    pub frames_written: u64,
    pub output_path: PathBuf,
}

#[inline]
fn beat_to_seconds(beats: f64, bpm: f32) -> f64 {
    beats * 60.0 / f64::from(bpm)
}

#[inline]
fn seconds_to_beat(seconds: f64, bpm: f32) -> f64 {
    seconds * f64::from(bpm) / 60.0
}

struct AudioContext {
    stream: u32,
    sample_rate: u32,
    channels: u32,
    reader: hound::WavReader<std::io::BufReader<std::fs::File>>,
}

fn add_video_stream(
    writer: &IMFSinkWriter,
    width: u32,
    height: u32,
    framerate: f32,
    bitrate: u32,
) -> Result<u32, String> {
    // Output: H.264.
    let out_type = unsafe {
        let t = MFCreateMediaType()
            .map_err(|e| format!("MFCreateMediaType(video out): {e}"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("video out major: {e}"))?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(|e| format!("video out subtype: {e}"))?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
            .map_err(|e| format!("video out bitrate: {e}"))?;
        t.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )
        .map_err(|e| format!("video out interlace: {e}"))?;
        set_frame_size(&t, width, height)?;
        set_frame_rate(&t, framerate)?;
        set_pixel_aspect_ratio(&t, 1, 1)?;
        t
    };
    let stream = unsafe { writer.AddStream(&out_type) }
        .map_err(|e| format!("AddStream(video): {e}"))?;

    // Input: NV12.
    let in_type = unsafe {
        let t = MFCreateMediaType()
            .map_err(|e| format!("MFCreateMediaType(video in): {e}"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("video in major: {e}"))?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|e| format!("video in subtype: {e}"))?;
        t.SetUINT32(
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )
        .map_err(|e| format!("video in interlace: {e}"))?;
        set_frame_size(&t, width, height)?;
        set_frame_rate(&t, framerate)?;
        set_pixel_aspect_ratio(&t, 1, 1)?;
        t
    };
    unsafe { writer.SetInputMediaType(stream, &in_type, None) }
        .map_err(|e| format!("SetInputMediaType(video): {e}"))?;

    Ok(stream)
}

fn add_audio_stream(
    writer: &IMFSinkWriter,
    sample_rate: u32,
    channels: u32,
    avg_bitrate: u32,
) -> Result<u32, String> {
    // Output: AAC.
    let out_type = unsafe {
        let t = MFCreateMediaType()
            .map_err(|e| format!("MFCreateMediaType(audio out): {e}"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(|e| format!("audio out major: {e}"))?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)
            .map_err(|e| format!("audio out subtype: {e}"))?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)
            .map_err(|e| format!("audio out channels: {e}"))?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)
            .map_err(|e| format!("audio out sr: {e}"))?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, avg_bitrate / 8)
            .map_err(|e| format!("audio out bps: {e}"))?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
            .map_err(|e| format!("audio out bits: {e}"))?;
        // AAC LC payload (= "raw_data_block stream wrapped in ADTS").
        t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
            .map_err(|e| format!("audio out aac payload: {e}"))?;
        t
    };
    let stream = unsafe { writer.AddStream(&out_type) }
        .map_err(|e| format!("AddStream(audio): {e}"))?;

    // Input: PCM Float32, native channels / rate (matches what we
    // read from the source WAV).
    let in_type = unsafe {
        let t = MFCreateMediaType()
            .map_err(|e| format!("MFCreateMediaType(audio in): {e}"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(|e| format!("audio in major: {e}"))?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)
            .map_err(|e| format!("audio in subtype: {e}"))?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32)
            .map_err(|e| format!("audio in bits: {e}"))?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)
            .map_err(|e| format!("audio in channels: {e}"))?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)
            .map_err(|e| format!("audio in sr: {e}"))?;
        // Block align = channels * (bits/8). For float32 = channels * 4.
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 4)
            .map_err(|e| format!("audio in block: {e}"))?;
        t.SetUINT32(
            &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            sample_rate * channels * 4,
        )
        .map_err(|e| format!("audio in bps: {e}"))?;
        t
    };
    unsafe { writer.SetInputMediaType(stream, &in_type, None) }
        .map_err(|e| format!("SetInputMediaType(audio): {e}"))?;

    Ok(stream)
}

fn set_frame_size(
    t: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    w: u32,
    h: u32,
) -> Result<(), String> {
    let packed = ((w as u64) << 32) | (h as u64);
    unsafe { t.SetUINT64(&MF_MT_FRAME_SIZE, packed) }
        .map_err(|e| format!("FRAME_SIZE: {e}"))
}

fn set_frame_rate(
    t: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    fps: f32,
) -> Result<(), String> {
    // Express the framerate as a rational. For typical project
    // values (24 / 30 / 60) numerator/1 is exact; for fractional
    // values (29.97 = 30000/1001) we round to a near-equivalent.
    let (num, den) = if (fps - fps.round()).abs() < 1e-3 {
        (fps.round() as u32, 1u32)
    } else {
        ((fps * 1000.0).round() as u32, 1000u32)
    };
    let packed = ((num as u64) << 32) | (den as u64);
    unsafe { t.SetUINT64(&MF_MT_FRAME_RATE, packed) }
        .map_err(|e| format!("FRAME_RATE: {e}"))
}

fn set_pixel_aspect_ratio(
    t: &windows::Win32::Media::MediaFoundation::IMFMediaType,
    num: u32,
    den: u32,
) -> Result<(), String> {
    let packed = ((num as u64) << 32) | (den as u64);
    unsafe { t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed) }
        .map_err(|e| format!("PIXEL_ASPECT_RATIO: {e}"))
}

/// Build a composite RGBA buffer for the given playhead. Layers are
/// alpha-blended bottom-up by per-layer alpha. Each source frame is
/// nearest-neighbor scaled to the output canvas — fast but visually
/// chunky for big resolution mismatches. P9+ can replace with a wgpu
/// pass that uses the existing preview composite shader.
fn render_frame_composite(
    song: &Song,
    project_dir: Option<&Path>,
    engine: &mut VideoPlaybackEngine,
    playhead_beat: f64,
    out_w: u32,
    out_h: u32,
    dst: &mut [u8],
) {
    // Clear to opaque black (= letterbox / no-clip background).
    for px in dst.chunks_exact_mut(4) {
        px[0] = 0;
        px[1] = 0;
        px[2] = 0;
        px[3] = 255;
    }
    let layers = VideoPlaybackEngine::active_sources_at(song, playhead_beat);
    if layers.is_empty() {
        return;
    }
    for layer in layers {
        let Some(path) =
            resolve_video_source_path(song, layer.video_source_id, project_dir)
        else {
            continue;
        };
        let Ok(frame) = engine.decode_at(layer.video_source_id, &path, layer.source_micros)
        else {
            continue;
        };
        let dst_rect = aspect_fit(out_w, out_h, frame.width, frame.height);
        blit_layer(
            dst,
            out_w as usize,
            out_h as usize,
            &frame.rgba,
            frame.width as usize,
            frame.height as usize,
            dst_rect,
            layer.alpha,
        );
    }
}

fn resolve_video_source_path(
    song: &Song,
    video_source_id: VideoSourceId,
    project_dir: Option<&Path>,
) -> Option<PathBuf> {
    let src = song.video_sources.get(&video_source_id)?;
    match &src.path {
        VideoSourcePath::Absolute(p) => Some(p.clone()),
        VideoSourcePath::ProjectRelative(rel) => project_dir.map(|d| d.join(rel)),
    }
}

/// Aspect-fit dst rect for a `src_w x src_h` source landing on a
/// `dst_w x dst_h` canvas. Returns `(x, y, w, h)` in canvas pixels.
fn aspect_fit(dst_w: u32, dst_h: u32, src_w: u32, src_h: u32) -> (i32, i32, u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0, 0, 0);
    }
    let dst_aspect = dst_w as f64 / dst_h as f64;
    let src_aspect = src_w as f64 / src_h as f64;
    if src_aspect >= dst_aspect {
        let h = (dst_w as f64 / src_aspect).round() as u32;
        let y = ((dst_h - h.min(dst_h)) / 2) as i32;
        (0, y, dst_w, h.min(dst_h))
    } else {
        let w = (dst_h as f64 * src_aspect).round() as u32;
        let x = ((dst_w - w.min(dst_w)) / 2) as i32;
        (x, 0, w.min(dst_w), dst_h)
    }
}

/// Alpha-blend a single source layer onto `dst`. Nearest-neighbor
/// scale (no interpolation) — fast and good enough for MVP. The
/// output canvas is `dst_w x dst_h`, the layer lands at the
/// `(rx, ry, rw, rh)` rect with the given `alpha`.
#[allow(clippy::too_many_arguments)]
fn blit_layer(
    dst: &mut [u8],
    dst_w: usize,
    _dst_h: usize,
    src: &[u8],
    src_w: usize,
    src_h: usize,
    rect: (i32, i32, u32, u32),
    alpha: f32,
) {
    let (rx, ry, rw, rh) = rect;
    if rw == 0 || rh == 0 {
        return;
    }
    let alpha = alpha.clamp(0.0, 1.0);
    let one_minus = 1.0 - alpha;
    let rw_us = rw as usize;
    let rh_us = rh as usize;
    for dy in 0..rh_us {
        let dst_y = ry + dy as i32;
        if dst_y < 0 {
            continue;
        }
        let sy = (dy as u64 * src_h as u64 / rh as u64).min(src_h as u64 - 1) as usize;
        for dx in 0..rw_us {
            let dst_x = rx + dx as i32;
            if dst_x < 0 {
                continue;
            }
            let sx = (dx as u64 * src_w as u64 / rw as u64).min(src_w as u64 - 1) as usize;
            let s_idx = (sy * src_w + sx) * 4;
            let d_idx = (dst_y as usize * dst_w + dst_x as usize) * 4;
            if d_idx + 4 > dst.len() {
                continue;
            }
            let sr = src[s_idx] as f32;
            let sg = src[s_idx + 1] as f32;
            let sb = src[s_idx + 2] as f32;
            let dr = dst[d_idx] as f32;
            let dg = dst[d_idx + 1] as f32;
            let db = dst[d_idx + 2] as f32;
            dst[d_idx] = (sr * alpha + dr * one_minus).clamp(0.0, 255.0) as u8;
            dst[d_idx + 1] = (sg * alpha + dg * one_minus).clamp(0.0, 255.0) as u8;
            dst[d_idx + 2] = (sb * alpha + db * one_minus).clamp(0.0, 255.0) as u8;
            dst[d_idx + 3] = 255;
        }
    }
}

/// Convert tightly-packed RGBA8 (`width * height * 4` bytes) to NV12
/// (`width * height` Y plane + `width * height / 2` UV plane). BT.601
/// limited range, integer formulae (matches every common H.264
/// encoder's reference). UV is 4:2:0 — one (U, V) pair per 2x2
/// luma block.
fn rgba_to_nv12(rgba: &[u8], width: usize, height: usize, dst: &mut [u8]) {
    let y_plane_size = width * height;
    let (y_plane, uv_plane) = dst.split_at_mut(y_plane_size);

    for y in 0..height {
        for x in 0..width {
            let s = (y * width + x) * 4;
            let r = rgba[s] as i32;
            let g = rgba[s + 1] as i32;
            let b = rgba[s + 2] as i32;
            let y_value = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[y * width + x] = y_value.clamp(0, 255) as u8;
        }
    }

    // UV plane: 4:2:0 — one (U,V) pair per 2x2 luma block. Average
    // the 4 source pixels to avoid chroma aliasing.
    for cy in 0..height / 2 {
        for cx in 0..width / 2 {
            let mut r_sum = 0i32;
            let mut g_sum = 0i32;
            let mut b_sum = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let s = ((cy * 2 + dy) * width + (cx * 2 + dx)) * 4;
                    r_sum += rgba[s] as i32;
                    g_sum += rgba[s + 1] as i32;
                    b_sum += rgba[s + 2] as i32;
                }
            }
            let r = r_sum / 4;
            let g = g_sum / 4;
            let b = b_sum / 4;
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            let uv_idx = (cy * (width / 2) + cx) * 2;
            uv_plane[uv_idx] = u.clamp(0, 255) as u8;
            uv_plane[uv_idx + 1] = v.clamp(0, 255) as u8;
        }
    }
}

fn write_video_sample(
    writer: &IMFSinkWriter,
    stream: u32,
    nv12: &[u8],
    time_100ns: i64,
    duration_100ns: i64,
) -> Result<(), String> {
    let buffer_len = nv12.len() as u32;
    let buffer = unsafe { MFCreateMemoryBuffer(buffer_len) }
        .map_err(|e| format!("MFCreateMemoryBuffer(video): {e}"))?;
    unsafe {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        buffer
            .Lock(&mut ptr, Some(&mut max_len), None)
            .map_err(|e| format!("video buffer Lock: {e}"))?;
        std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
        buffer
            .SetCurrentLength(buffer_len)
            .map_err(|e| format!("SetCurrentLength(video): {e}"))?;
        buffer
            .Unlock()
            .map_err(|e| format!("video buffer Unlock: {e}"))?;
    }
    let sample = unsafe { MFCreateSample() }
        .map_err(|e| format!("MFCreateSample(video): {e}"))?;
    unsafe {
        sample
            .AddBuffer(&buffer)
            .map_err(|e| format!("AddBuffer(video): {e}"))?;
        sample
            .SetSampleTime(time_100ns)
            .map_err(|e| format!("SetSampleTime(video): {e}"))?;
        sample
            .SetSampleDuration(duration_100ns)
            .map_err(|e| format!("SetSampleDuration(video): {e}"))?;
        writer
            .WriteSample(stream, &sample)
            .map_err(|e| format!("WriteSample(video): {e}"))?;
    }
    Ok(())
}

fn write_all_audio_samples(
    writer: &IMFSinkWriter,
    ctx: &mut AudioContext,
) -> Result<(), String> {
    // Stream audio in ~100ms chunks (= 4800 frames at 48kHz). Keeps
    // the encoder pipeline well-fed without blowing memory.
    const FRAMES_PER_CHUNK: usize = 4800;
    let bytes_per_frame = ctx.channels as usize * 4; // f32 stereo = 8 bytes
    let mut chunk: Vec<u8> = Vec::with_capacity(FRAMES_PER_CHUNK * bytes_per_frame);
    let mut total_frames: u64 = 0;
    let sr = ctx.sample_rate;
    let mut iter = ctx.reader.samples::<f32>();
    loop {
        // Read up to `FRAMES_PER_CHUNK` whole frames.
        chunk.clear();
        let mut frames_in_chunk: usize = 0;
        for _ in 0..FRAMES_PER_CHUNK {
            let mut frame_bytes = [0u8; 32]; // up to 8 channels of f32
            let mut byte_off = 0;
            let mut full_frame = true;
            for _ in 0..ctx.channels {
                match iter.next() {
                    Some(Ok(s)) => {
                        let bytes = s.to_le_bytes();
                        frame_bytes[byte_off..byte_off + 4].copy_from_slice(&bytes);
                        byte_off += 4;
                    }
                    Some(Err(e)) => {
                        return Err(format!("audio sample read: {e}"));
                    }
                    None => {
                        full_frame = false;
                        break;
                    }
                }
            }
            if !full_frame {
                break;
            }
            chunk.extend_from_slice(&frame_bytes[..byte_off]);
            frames_in_chunk += 1;
        }
        if frames_in_chunk == 0 {
            break;
        }
        let time_100ns = (total_frames as i64) * 10_000_000 / sr as i64;
        let duration_100ns = (frames_in_chunk as i64) * 10_000_000 / sr as i64;
        write_audio_sample(writer, ctx.stream, &chunk, time_100ns, duration_100ns)?;
        total_frames += frames_in_chunk as u64;
    }
    Ok(())
}

fn write_audio_sample(
    writer: &IMFSinkWriter,
    stream: u32,
    pcm: &[u8],
    time_100ns: i64,
    duration_100ns: i64,
) -> Result<(), String> {
    let buffer_len = pcm.len() as u32;
    let buffer = unsafe { MFCreateMemoryBuffer(buffer_len) }
        .map_err(|e| format!("MFCreateMemoryBuffer(audio): {e}"))?;
    unsafe {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        buffer
            .Lock(&mut ptr, Some(&mut max_len), None)
            .map_err(|e| format!("audio buffer Lock: {e}"))?;
        std::ptr::copy_nonoverlapping(pcm.as_ptr(), ptr, pcm.len());
        buffer
            .SetCurrentLength(buffer_len)
            .map_err(|e| format!("SetCurrentLength(audio): {e}"))?;
        buffer
            .Unlock()
            .map_err(|e| format!("audio buffer Unlock: {e}"))?;
    }
    let sample = unsafe { MFCreateSample() }
        .map_err(|e| format!("MFCreateSample(audio): {e}"))?;
    unsafe {
        sample
            .AddBuffer(&buffer)
            .map_err(|e| format!("AddBuffer(audio): {e}"))?;
        sample
            .SetSampleTime(time_100ns)
            .map_err(|e| format!("SetSampleTime(audio): {e}"))?;
        sample
            .SetSampleDuration(duration_100ns)
            .map_err(|e| format!("SetSampleDuration(audio): {e}"))?;
        writer
            .WriteSample(stream, &sample)
            .map_err(|e| format!("WriteSample(audio): {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_fit_pillarbox() {
        // 16:9 source (1920x1080) onto 4:3 canvas (640x480) → letterbox
        // top/bottom.
        let (x, y, w, h) = aspect_fit(640, 480, 1920, 1080);
        assert_eq!(x, 0);
        assert_eq!(w, 640);
        // 640 / (16/9) = 360 → height 360, y centred
        assert_eq!(h, 360);
        assert_eq!(y, 60);
    }

    #[test]
    fn aspect_fit_letterbox() {
        // 9:16 portrait source onto landscape canvas → side bars.
        let (x, y, w, h) = aspect_fit(640, 480, 1080, 1920);
        assert_eq!(y, 0);
        assert_eq!(h, 480);
        // 480 * (9/16) = 270 → width 270, x centred
        assert_eq!(w, 270);
        assert_eq!(x, 185);
    }

    #[test]
    fn rgba_to_nv12_pure_white_round_trips_roughly() {
        // Pure white 4x4 RGBA → Y plane should be near 235 (= BT.601
        // limited range white), UV near 128 (neutral chroma).
        let mut rgba = vec![255u8; 4 * 4 * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let mut nv12 = vec![0u8; 4 * 4 * 3 / 2];
        rgba_to_nv12(&rgba, 4, 4, &mut nv12);
        // Y plane = first 16 bytes
        for &y in &nv12[..16] {
            // BT.601 white = (66+129+25)*255/256 + 16 ≈ 235
            assert!((y as i32 - 235).abs() <= 3, "Y ~ 235, got {y}");
        }
        // UV plane = last 8 bytes (4 (U,V) pairs)
        for chunk in nv12[16..].chunks_exact(2) {
            assert!(
                (chunk[0] as i32 - 128).abs() <= 3,
                "U ~ 128, got {}",
                chunk[0]
            );
            assert!(
                (chunk[1] as i32 - 128).abs() <= 3,
                "V ~ 128, got {}",
                chunk[1]
            );
        }
    }

    /// End-to-end smoke: build a tiny project with one video track +
    /// one video clip pointing at an `ffmpeg`-generated source mp4,
    /// run `render_mp4`, and check the output exists + WMF can re-read
    /// it (= the container + H.264 stream were finalized correctly).
    /// Audio is skipped (video-only mp4) to keep the test fast and
    /// avoid pulling AAC encode setup into the smoke.
    #[test]
    fn render_mp4_video_only_smoke() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("render_mp4: ffmpeg not on PATH, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src_mp4 = dir.path().join("src.mp4");
        let out_mp4 = dir.path().join("out.mp4");
        // 1 second @ 30fps blue source.
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "color=c=blue:size=320x240:duration=1:rate=30",
                "-c:v", "libx264",
                "-pix_fmt", "yuv420p",
                "-y",
                src_mp4.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("ffmpeg run");
        assert!(status.success());

        // Build a song: 1 video track, 1 clip 4 beats long @ 120 BPM
        // (= 2 seconds). The source is only 1 second so the second
        // half renders the last-frame fallback (= acceptable for
        // smoke).
        let mut song = Song {
            bpm: 120.0,
            length_beats: 4.0,
            video_resolution: (320, 240),
            video_framerate: 30.0,
            ..Song::default()
        };
        let vsrc_id = song.alloc_video_source_id();
        song.video_sources.insert(
            vsrc_id,
            common::model::VideoSource {
                path: common::model::VideoSourcePath::Absolute(src_mp4),
                width: 320,
                height: 240,
                framerate: 30.0,
                duration_micros: 1_000_000,
                codec: "h264".into(),
                audio_source_id: None,
            },
        );
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            common::model::ClipContent::Video(common::model::VideoContent {
                events: vec![common::model::VideoEvent {
                    source_id: vsrc_id,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 4.0,
                    source_start_micros: 0,
                    source_end_micros: 1_000_000,
                    ..common::model::VideoEvent::default()
                }],
            }),
        );
        let track_id = song.alloc_track_id();
        let mut track = common::model::Track {
            id: track_id,
            kind: common::model::TrackKind::Video,
            name: "V".into(),
            ..common::model::Track::default()
        };
        let clip_id = track.alloc_clip_id();
        track.clips.push(common::model::Clip {
            id: clip_id,
            name: "vclip".into(),
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: cid,
            notes: Vec::new(),
        });
        song.tracks.push(track);

        let cfg = RenderConfig::new(&song, &out_mp4);
        let stats = render_mp4(&cfg).expect("render_mp4");
        assert!(out_mp4.exists(), "output mp4 should exist");
        // 2 seconds @ 30fps = 60 frames (ceil rounding).
        assert!(
            stats.frames_written >= 58 && stats.frames_written <= 62,
            "frame count near 60, got {}",
            stats.frames_written
        );

        // Verify the file is a valid mp4 that WMF can open + read
        // metadata back from. Re-uses the existing extract_metadata
        // path so we get the width / height / codec round-trip in
        // one call.
        let md = crate::import_video::extract_metadata(&out_mp4)
            .expect("output mp4 should be readable by WMF");
        assert_eq!(md.width, 320);
        assert_eq!(md.height, 240);
        assert_eq!(md.codec, "h264");
    }

    fn locate_ffmpeg() -> Option<std::path::PathBuf> {
        let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(exe))
                .find(|p| p.is_file())
        })
    }

    #[test]
    fn rgba_to_nv12_pure_red_chroma_signature() {
        // Pure red 2x2 RGBA → Y ~ 81 (BT.601), U < 128, V > 128.
        let mut rgba = vec![0u8; 2 * 2 * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[0] = 255; // R
            px[3] = 255;
        }
        let mut nv12 = vec![0u8; 2 * 2 * 3 / 2];
        rgba_to_nv12(&rgba, 2, 2, &mut nv12);
        for &y in &nv12[..4] {
            assert!((y as i32 - 81).abs() <= 3, "Y ~ 81 (red luma), got {y}");
        }
        // U < 128, V > 128 (red is +V, -U)
        assert!(nv12[4] < 128, "U < 128 for red, got {}", nv12[4]);
        assert!(nv12[5] > 128, "V > 128 for red, got {}", nv12[5]);
    }
}
