//! Playback-time video frame decode (`docs/plan_video.md` P5).
//!
//! Synchronous (= no background thread) decoder driven each frame
//! from `Runner::render_frame`. Holds per-`VideoSourceId`
//! `IMFSourceReader` so sequential ReadSample (= playback) is cheap;
//! seeks only when the playhead jumps (= scrub / transport move /
//! play-from-start). Returns RGBA8 bytes that `Runner` uploads via
//! `Renderer::upload_texture_rgba` into a single reusable preview
//! texture.
//!
//! Multi-clip composite (= crossfade + multi-track) is P7. P5 covers
//! "show the frame for the topmost active video clip at the
//! playhead" only — exactly mirroring the REAPER preview behaviour
//! when no video FX chain is present.

use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use common::model::{FadeCurve, Song, TrackKind, VideoEvent, VideoSourceId};
use windows::Win32::Media::MediaFoundation::{
    IMFSample, IMFSourceReader, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Video, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_ENDOFSTREAM, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, PCWSTR};

/// One decoded RGBA frame. Same shape as [`crate::import_video::ThumbnailFrame`]
/// — kept separate for now to keep the dependency tree light (this
/// module doesn't pull in the rest of `import_video`).
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly-packed RGBA8 in scanline order, length = `width * height * 4`.
    pub rgba: Vec<u8>,
}

/// One video clip active at the current playhead. The runner walks
/// the returned list bottom-up (= `z_index` ascending) and pushes one
/// textured quad per layer with the per-event `alpha`; gui_01's
/// call-order interleave then blends them via standard "src OVER
/// dst" semantics (= top track wins when alpha=1, crossfade
/// midpoint mixes at alpha=0.5/0.5). v12 (`docs/plan_video.md` §4 P7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveVideoFrame {
    pub video_source_id: VideoSourceId,
    pub source_micros: u64,
    /// Per-event alpha derived from `fade_in_beats` / `fade_out_beats`
    /// and the matching curve. `1.0` is fully opaque, `0.0` invisible.
    /// Caller pushes the quad directly with this value.
    pub alpha: f32,
    /// Bottom-up draw order. `0` is the lowest video track (drawn
    /// first), each higher track increments. Within the same track
    /// adjacent / overlapping events share the same `z_index`; their
    /// individual `alpha`s do the crossfade.
    pub z_index: u32,
}

/// One `IMFSourceReader` plus a few cached attributes that don't
/// change after the reader is built.
struct ReaderEntry {
    reader: IMFSourceReader,
    width: u32,
    height: u32,
    /// μs of the most recent frame we decoded, in source time. Used
    /// to decide whether the next `decode_at` is a forward-step (=
    /// just keep ReadSample-ing) or a jump (= SetCurrentPosition
    /// seek + flush).
    last_decoded_micros: Option<u64>,
}

/// Stateful playback decoder. Owned by `Runner` for the lifetime of
/// the process. Readers are created lazily on first request per
/// VideoSourceId; the engine never tears them down on its own (=
/// project unload happens at process exit for MVP).
pub struct VideoPlaybackEngine {
    readers: HashMap<VideoSourceId, ReaderEntry>,
}

impl VideoPlaybackEngine {
    pub fn new() -> Self {
        Self {
            readers: HashMap::new(),
        }
    }

    /// Pure helper: walk the song bottom-up and return every video
    /// clip active at `playhead_beat`, with a per-event `alpha`
    /// derived from the clip's `fade_in_beats` / `fade_out_beats` (=
    /// MVP crossfade behaviour). Returns a `Vec<ActiveVideoFrame>`
    /// ordered from lowest to topmost track so the caller can
    /// composite by call-order (= last pushed quad ends up on top).
    /// v12 (`docs/plan_video.md` §4 P7).
    ///
    /// Muted events are dropped from the result entirely (= same as
    /// the singular `active_source_at` MVP behaviour).
    pub fn active_sources_at(song: &Song, playhead_beat: f64) -> Vec<ActiveVideoFrame> {
        let bpm = song.bpm as f64;
        if bpm <= 0.0 {
            return Vec::new();
        }
        let mut out: Vec<ActiveVideoFrame> = Vec::new();
        // `song.tracks[0]` is the top of the arrangement, so iterating
        // `.rev()` yields bottom-most → topmost. Each video track gets
        // a contiguous `z_index` counter so events on the same track
        // share a layer (= their alphas blend within layer instead of
        // creating a third layer between clip A and clip B during
        // crossfade).
        let mut z_index: u32 = 0;
        for track in song.tracks.iter().rev() {
            if track.kind != TrackKind::Video {
                continue;
            }
            let mut track_emitted = false;
            for clip in &track.clips {
                let clip_start = clip.start_beat;
                let clip_end = clip.start_beat + clip.length_beats;
                if playhead_beat < clip_start || playhead_beat >= clip_end {
                    continue;
                }
                let clip_local = playhead_beat - clip_start;
                let Some(content) = song.clip_contents.get(&clip.content_id) else {
                    continue;
                };
                let Some(events) = content.video_events() else {
                    continue;
                };
                for event in events {
                    let event_end =
                        event.event_start_in_clip_beats + event.event_length_beats;
                    if clip_local < event.event_start_in_clip_beats
                        || clip_local >= event_end
                    {
                        continue;
                    }
                    if event.muted {
                        continue;
                    }
                    let event_progress_beats =
                        clip_local - event.event_start_in_clip_beats;
                    let event_progress_secs = event_progress_beats * 60.0 / bpm;
                    let event_progress_micros =
                        (event_progress_secs * 1_000_000.0).round() as u64;
                    let source_micros = event
                        .source_start_micros
                        .saturating_add(event_progress_micros)
                        .min(event.source_end_micros);
                    let alpha = event_alpha(event, clip_local);
                    if alpha <= 0.0 {
                        continue;
                    }
                    out.push(ActiveVideoFrame {
                        video_source_id: event.source_id,
                        source_micros,
                        alpha,
                        z_index,
                    });
                    track_emitted = true;
                }
            }
            if track_emitted {
                z_index += 1;
            }
        }
        out
    }

    /// Backwards-compatible singular accessor (`docs/plan_video.md`
    /// P5 baseline). Equivalent to `active_sources_at(...).last()` —
    /// the topmost active layer wins, with its alpha taken into
    /// account so a faded-out top clip with alpha < threshold defers
    /// to whatever is underneath. Kept around for callers that don't
    /// composite (= e.g. arrangement thumbnail picker).
    pub fn active_source_at(
        song: &Song,
        playhead_beat: f64,
    ) -> Option<(VideoSourceId, u64)> {
        let bpm = song.bpm as f64;
        if bpm <= 0.0 {
            return None;
        }
        for track in &song.tracks {
            if track.kind != TrackKind::Video {
                continue;
            }
            for clip in &track.clips {
                let clip_start = clip.start_beat;
                let clip_end = clip.start_beat + clip.length_beats;
                if playhead_beat < clip_start || playhead_beat >= clip_end {
                    continue;
                }
                let clip_local = playhead_beat - clip_start;
                let Some(content) = song.clip_contents.get(&clip.content_id) else {
                    continue;
                };
                let Some(events) = content.video_events() else {
                    continue;
                };
                for event in events {
                    let event_end =
                        event.event_start_in_clip_beats + event.event_length_beats;
                    if clip_local < event.event_start_in_clip_beats
                        || clip_local >= event_end
                    {
                        continue;
                    }
                    if event.muted {
                        return None;
                    }
                    let event_progress_beats =
                        clip_local - event.event_start_in_clip_beats;
                    let event_progress_secs = event_progress_beats * 60.0 / bpm;
                    let event_progress_micros =
                        (event_progress_secs * 1_000_000.0).round() as u64;
                    let source_micros = event
                        .source_start_micros
                        .saturating_add(event_progress_micros)
                        .min(event.source_end_micros);
                    return Some((event.source_id, source_micros));
                }
            }
        }
        None
    }

    /// Decode (or just-fetch when the target lands on the same frame
    /// we last decoded) the frame at `target_micros` of the source.
    /// `source_path` is only consulted when the reader for this
    /// `VideoSourceId` hasn't been created yet — caller resolves the
    /// `VideoSourcePath` (ProjectRelative vs Absolute) before passing
    /// in.
    pub fn decode_at(
        &mut self,
        video_source_id: VideoSourceId,
        source_path: &Path,
        target_micros: u64,
    ) -> Result<DecodedFrame, String> {
        // Lazy-init the reader for this source. `Entry::Vacant` keeps
        // a single hash lookup (clippy `map_entry`).
        let entry = match self.readers.entry(video_source_id) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let entry = create_reader_for_source(source_path)?;
                tracing::info!(
                    video_source_id,
                    width = entry.width,
                    height = entry.height,
                    "video reader created"
                );
                v.insert(entry)
            }
        };

        // Decide whether to seek. Forward-step within ~100ms = keep
        // ReadSample-ing (cheap). Backward or large jump = seek.
        const FORWARD_BUDGET_MICROS: u64 = 100_000;
        let should_seek = match entry.last_decoded_micros {
            None => true,
            Some(last) if target_micros < last => true,
            Some(last) if target_micros.saturating_sub(last) > FORWARD_BUDGET_MICROS => {
                true
            }
            Some(_) => false,
        };
        if should_seek {
            seek_reader(&entry.reader, target_micros)?;
            // After seek WMF re-syncs to the previous keyframe; the
            // forward scan below catches us up to the target.
        }

        // Walk forward until we have a sample whose timestamp is at
        // or past target_micros. Bound the walk to ~4 seconds of source
        // at 60fps (= 240 frames) so a malformed stream doesn't spin
        // forever — typical H.264 keyframe intervals are 1-4 sec so
        // post-seek catch-up should always finish within this budget.
        //
        // **Skip the BGRA→RGBA copy for non-target frames**: the
        // intermediate samples are read just to advance the decoder
        // (= H.264 P-frames have to be decoded sequentially to get to
        // the target). Dropping the `IMFSample` releases the GPU/CPU
        // surface back to WMF without us touching 8MB of pixel data
        // 240 times in a row. This is the 2-second-scrub-lag fix
        // (2026-05-25 user report).
        let mut last_sample: Option<(IMFSample, u64)> = None;
        let mut chosen: Option<(IMFSample, u64)> = None;
        for _ in 0..240 {
            let Some((sample, ts_100ns)) = read_sample_only(&entry.reader)? else {
                break; // EOS
            };
            let ts_micros = (ts_100ns.max(0) as u64) / 10;
            if ts_micros >= target_micros {
                chosen = Some((sample, ts_micros));
                break;
            }
            last_sample = Some((sample, ts_micros));
        }
        let (final_sample, final_ts) = chosen.or(last_sample).ok_or_else(|| {
            format!("no frame decoded for source {video_source_id} at {target_micros}μs")
        })?;
        let rgba = sample_to_rgba(&final_sample, entry.width, entry.height)?;
        entry.last_decoded_micros = Some(final_ts);
        Ok(DecodedFrame {
            width: entry.width,
            height: entry.height,
            rgba,
        })
    }
}

impl Default for VideoPlaybackEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-event alpha at the given clip-local beat, derived from
/// `fade_in_beats` / `fade_out_beats` with the event's own
/// `fade_in_curve` / `fade_out_curve`. Range `0.0..=1.0`. Outside
/// both fade regions returns `1.0` (= fully opaque).
///
/// docs/plan_video.md §4 P7: linear / s-curve / exp formulae match
/// `common::audio_render::fade_envelope` (the audio sibling), so
/// crossfade visuals stay in step with the audio engine's gain
/// envelope when the user fades both halves of a clip together.
fn event_alpha(event: &VideoEvent, clip_local_beat: f64) -> f32 {
    let event_local = clip_local_beat - event.event_start_in_clip_beats;
    if event_local < 0.0 {
        return 0.0;
    }
    let mut alpha = 1.0_f32;
    if event.fade_in_beats > 0.0 && event_local < event.fade_in_beats {
        let progress = (event_local / event.fade_in_beats) as f32;
        alpha *= fade_curve_value(progress, event.fade_in_curve);
    }
    let event_remaining = event.event_length_beats - event_local;
    if event.fade_out_beats > 0.0 && event_remaining > 0.0
        && event_remaining < event.fade_out_beats
    {
        let progress = (event_remaining / event.fade_out_beats) as f32;
        alpha *= fade_curve_value(progress, event.fade_out_curve);
    }
    alpha.clamp(0.0, 1.0)
}

/// Single fade-curve evaluator. `progress` is `0..=1`, output is
/// `0..=1`. Mirrors `common::audio_render::fade_envelope` math.
fn fade_curve_value(progress: f32, curve: FadeCurve) -> f32 {
    let x = progress.clamp(0.0, 1.0);
    match curve {
        FadeCurve::Linear => x,
        FadeCurve::Exponential => x * x,
        FadeCurve::SCurve => 0.5 - 0.5 * (std::f32::consts::PI * x).cos(),
    }
}

/// docs/plan_video.md P5 perf: scale the WMF output down to this
/// long-edge before the sample reaches the CPU. The preview window
/// caps at 960px in width by default ([`view::preview_window::
/// scale_to_fit_on_screen`]), so a 1920x1080 source decoded at native
/// would upload 8 MB / frame and waste ~4x more CPU + GPU bandwidth
/// than the eye ever consumes. 960 also keeps decode under ~5 ms /
/// frame on modern Intel iGPUs, which is the budget the GUI thread
/// has at 30fps preview without going chunky. Sources whose long
/// edge is already ≤ 960 are passed through native (= no upscale).
const PREVIEW_MAX_LONG_EDGE: u32 = 960;

fn scale_for_preview(native_w: u32, native_h: u32) -> (u32, u32) {
    let long = native_w.max(native_h);
    if long <= PREVIEW_MAX_LONG_EDGE {
        return (native_w.max(1), native_h.max(1));
    }
    let scale = PREVIEW_MAX_LONG_EDGE as f64 / long as f64;
    let w = ((native_w as f64) * scale).round().max(1.0) as u32;
    let h = ((native_h as f64) * scale).round().max(1.0) as u32;
    // Round to even so 4:2:0 / NV12 downstream consumers don't trip
    // on odd dimensions. Preview pipeline is RGB so this is just
    // hygienic.
    (w & !1, h & !1)
}

fn create_reader_for_source(path: &Path) -> Result<ReaderEntry, String> {
    // MFStartup is owned by `import_video` and idempotent.
    crate::import_video::ensure_mf_startup_pub()
        .map_err(|e| format!("MFStartup: {e}"))?;

    if !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let url = PCWSTR::from_raw(wide.as_ptr());

    let attrs = unsafe {
        let mut a = None;
        MFCreateAttributes(&mut a, 1)
            .map_err(|e| format!("MFCreateAttributes: {e}"))?;
        let attrs = a.ok_or_else(|| "MFCreateAttributes returned null".to_string())?;
        attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(|e| format!("SetUINT32 ENABLE_VIDEO_PROCESSING: {e}"))?;
        attrs
    };
    let reader: IMFSourceReader = unsafe {
        MFCreateSourceReaderFromURL(url, &attrs)
            .map_err(|e| format!("MFCreateSourceReaderFromURL: {e}"))?
    };

    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let native = unsafe { reader.GetNativeMediaType(stream, 0) }
        .map_err(|e| format!("GetNativeMediaType: {e}"))?;
    let frame_size = unsafe { native.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|e| format!("MF_MT_FRAME_SIZE: {e}"))?;
    let native_w = (frame_size >> 32) as u32;
    let native_h = (frame_size & 0xFFFF_FFFF) as u32;
    if native_w == 0 || native_h == 0 {
        return Err(format!("invalid frame size {native_w}x{native_h}"));
    }

    // Minimal output type — request only RGB32 subtype + the major
    // type. Empirically WMF's video processor MFT accepts this on
    // every H.264 / HEVC source we tested and falls back to native
    // dimensions automatically.
    //
    // **NB**: Earlier attempts to ask WMF to scale down at decode time
    // (by also setting `MF_MT_FRAME_SIZE` to a target like 960x540)
    // returned `MF_E_INVALIDMEDIATYPE` (0xC00D36B4) on the user's
    // 1920x1080 60fps source even with INTERLACE_MODE + FRAME_RATE +
    // PIXEL_ASPECT_RATIO populated — the video processor MFT seems to
    // accept format conversion but not arbitrary scaling for this
    // codec/driver combination. Preview throughput is therefore
    // limited by native-resolution decode; the proper fix is the
    // background worker thread described in `docs/plan_video.md` §3
    // (= lookahead ring buffer), which sits above this layer.
    let output = unsafe {
        let t = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| format!("set MAJOR_TYPE: {e}"))?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
            .map_err(|e| format!("set SUBTYPE RGB32: {e}"))?;
        t
    };
    unsafe { reader.SetCurrentMediaType(stream, None, &output) }
        .map_err(|e| format!("SetCurrentMediaType RGB32: {e}"))?;

    // Read back the delivered frame size — even when we don't request
    // scaling, WMF may pad e.g. 1080→1088 to satisfy H.264 macroblock
    // alignment. Trust the post-Set output type so the buffer math
    // below uses the actual delivered dimensions.
    let actual_size = unsafe {
        let cur = reader
            .GetCurrentMediaType(stream)
            .map_err(|e| format!("GetCurrentMediaType after Set: {e}"))?;
        cur.GetUINT64(&MF_MT_FRAME_SIZE)
            .map_err(|e| format!("output MF_MT_FRAME_SIZE: {e}"))?
    };
    let actual_w = (actual_size >> 32) as u32;
    let actual_h = (actual_size & 0xFFFF_FFFF) as u32;

    // `scale_for_preview` is kept as a pure helper for future use
    // (e.g. once we have an explicit video processor MFT) — silence
    // the unused-fn warning by referencing it here.
    let _ = scale_for_preview;

    Ok(ReaderEntry {
        reader,
        width: actual_w,
        height: actual_h,
        last_decoded_micros: None,
    })
}

/// `IMFSourceReader::SetCurrentPosition` with the default 100-ns time
/// format (`GUID_NULL`). PROPVARIANT carries the position as VT_I8
/// (signed 8-byte int) per the WMF docs.
fn seek_reader(reader: &IMFSourceReader, target_micros: u64) -> Result<(), String> {
    let position_100ns: i64 = (target_micros as i64).saturating_mul(10);
    // PROPVARIANT_0 is a union holding `ManuallyDrop<PROPVARIANT_0_0>`,
    // so writing into the inner struct field-by-field trips the
    // "cannot DerefMut a ManuallyDrop union field" check. Build the
    // inner struct value and replace the whole union variant in one
    // assignment — equivalent to the C-level `PropVariantInit + set
    // tag + set value` idiom but typed.
    let propvar = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_I8,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { hVal: position_100ns },
            }),
        },
    };
    let time_format = GUID::zeroed(); // GUID_NULL → 100-ns
    unsafe { reader.SetCurrentPosition(&time_format, &propvar) }
        .map_err(|e| format!("SetCurrentPosition: {e}"))?;
    Ok(())
}

/// Pull one decoded sample and return the `(IMFSample, timestamp)`
/// pair without copying its pixel content. Returns `Ok(None)` on EOS.
/// Skips STREAMTICK gaps internally. Used by `decode_at`'s forward
/// walk so intermediate P-frames don't pay the 8 MB BGRA→RGBA copy.
fn read_sample_only(reader: &IMFSourceReader) -> Result<Option<(IMFSample, i64)>, String> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    loop {
        let mut flags: u32 = 0;
        let mut timestamp: i64 = 0;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )
        }
        .map_err(|e| format!("ReadSample: {e}"))?;

        if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            return Ok(None);
        }
        let Some(sample) = sample else {
            // STREAMTICK or format change — drain again.
            continue;
        };
        return Ok(Some((sample, timestamp)));
    }
}

/// Extract RGBA8 bytes from an already-decoded `IMFSample`. Lock the
/// contiguous buffer, BGRA→RGBA channel-swap with alpha pinned to
/// 0xFF (`MFVideoFormat_RGB32`'s alpha byte is undefined per MSDN),
/// and unlock. Called by `decode_at` only on the final target frame.
///
/// Channel swap goes through `bgra_to_rgba`, which picks SSSE3
/// `_mm_shuffle_epi8` on x86_64 (= ~10x faster than scalar; the
/// 2026-05-25 "playback コマ送り" fix). Scalar fallback handles ARM
/// and pre-SSSE3 x86 (none in practice on Windows MMF targets).
fn sample_to_rgba(
    sample: &IMFSample,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let frame_bytes = width as usize * height as usize * 4;
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut max_len: u32 = 0;
    let mut cur_len: u32 = 0;
    unsafe { buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len)) }
        .map_err(|e| format!("Lock: {e}"))?;

    if ptr.is_null() || (cur_len as usize) < frame_bytes {
        let _ = unsafe { buffer.Unlock() };
        return Err(format!(
            "frame too small: {cur_len} < {frame_bytes}"
        ));
    }
    let src = unsafe { std::slice::from_raw_parts(ptr, frame_bytes) };
    let rgba = bgra_to_rgba(src);
    let _ = unsafe { buffer.Unlock() };
    Ok(rgba)
}

/// BGRA8 → RGBA8 channel swap with alpha pinned to 0xFF. Picks the
/// fastest available path: SSSE3 `_mm_shuffle_epi8` on x86_64 (~10x
/// faster than scalar for 1080p), scalar otherwise. Pure function +
/// allocation-free input + caller-owned output Vec. Both paths are
/// covered by `bgra_to_rgba_*` unit tests for correctness.
pub fn bgra_to_rgba(src: &[u8]) -> Vec<u8> {
    let len = src.len();
    debug_assert!(
        len.is_multiple_of(4),
        "BGRA input must be multiple of 4 bytes"
    );

    // `vec![0; len]` boils down to `memset` which is ~50 GB/s on a
    // modern CPU — adding 0.2 ms for an 8 MB 1080p frame, well under
    // 1 % of the channel-swap budget. Cheaper than the `set_len` +
    // write-everything-before-read pattern that clippy (rightfully)
    // flags as UB-prone.
    let mut dst = vec![0u8; len];

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("ssse3") {
            // SAFETY: feature detected at runtime, src + dst same len.
            unsafe { bgra_to_rgba_ssse3(src, &mut dst) };
            return dst;
        }
    }

    bgra_to_rgba_scalar(src, &mut dst);
    dst
}

/// Scalar fallback for `bgra_to_rgba`. ~15ms for 1080p on a typical
/// Skylake-class CPU. Used when SSSE3 isn't available (= ARM, very
/// old x86), or as the reference impl for the SIMD path's unit test.
fn bgra_to_rgba_scalar(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = 0xFF;
    }
}

/// SSSE3-accelerated BGRA→RGBA. Processes 4 pixels (16 bytes) per
/// iteration via `_mm_shuffle_epi8`, then `_mm_or_si128` to set the
/// alpha lanes to 0xFF. ~1.5ms for 1080p, ~6ms for 4K — both well
/// under one 30fps frame budget.
///
/// # Safety
///
/// - CPU must support SSSE3 (caller verifies via
///   `is_x86_feature_detected!("ssse3")`).
/// - `src` and `dst` must have the same length and not overlap.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn bgra_to_rgba_ssse3(src: &[u8], dst: &mut [u8]) {
    use core::arch::x86_64::{
        __m128i, _mm_loadu_si128, _mm_or_si128, _mm_set1_epi32, _mm_setr_epi8,
        _mm_shuffle_epi8, _mm_storeu_si128,
    };
    debug_assert_eq!(src.len(), dst.len());

    // Shuffle mask: read from BGRA positions (2, 1, 0, _) per pixel.
    // `-1` clears the alpha byte; we OR in 0xFF below.
    let shuffle_mask = _mm_setr_epi8(
        2, 1, 0, -1,
        6, 5, 4, -1,
        10, 9, 8, -1,
        14, 13, 12, -1,
    );
    // alpha_or = 0x_FF_00_00_00 per 32-bit lane (little-endian = byte 3 = 0xFF).
    let alpha_or = _mm_set1_epi32(0xFF00_0000_u32 as i32);

    let chunks = src.len() / 16;
    let src_ptr = src.as_ptr() as *const __m128i;
    let dst_ptr = dst.as_mut_ptr() as *mut __m128i;
    for i in 0..chunks {
        // SAFETY: i < chunks ⇒ offset is in bounds for both pointers,
        // alignment is not required for `_mm_loadu_si128` (the u =
        // unaligned).
        unsafe {
            let v = _mm_loadu_si128(src_ptr.add(i));
            let shuffled = _mm_shuffle_epi8(v, shuffle_mask);
            let with_alpha = _mm_or_si128(shuffled, alpha_or);
            _mm_storeu_si128(dst_ptr.add(i), with_alpha);
        }
    }

    // Tail: 1080p / 4K / 720p are all multiples of 16, but be
    // defensive — scalar handle the last 0..15 bytes.
    let processed = chunks * 16;
    if processed < src.len() {
        bgra_to_rgba_scalar(&src[processed..], &mut dst[processed..]);
    }
}

/// Pull one decoded sample. Returns `Ok(None)` on EOS, `Ok(Some(...))`
/// for a real sample. Skips STREAMTICK gaps internally.
#[allow(dead_code)]
fn read_one_frame(
    reader: &IMFSourceReader,
    width: u32,
    height: u32,
) -> Result<Option<(u64, Vec<u8>)>, String> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let frame_bytes = width as usize * height as usize * 4;
    loop {
        let mut flags: u32 = 0;
        let mut timestamp: i64 = 0;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )
        }
        .map_err(|e| format!("ReadSample: {e}"))?;

        if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            return Ok(None);
        }
        let Some(sample) = sample else {
            // STREAMTICK or format change — drain again.
            continue;
        };

        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        unsafe { buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len)) }
            .map_err(|e| format!("Lock: {e}"))?;

        if ptr.is_null() || (cur_len as usize) < frame_bytes {
            let _ = unsafe { buffer.Unlock() };
            return Err(format!(
                "frame too small: {cur_len} < {frame_bytes}"
            ));
        }
        let src = unsafe { std::slice::from_raw_parts(ptr, frame_bytes) };
        let mut rgba = Vec::with_capacity(frame_bytes);
        // BGRA → RGBA channel swap. The alpha byte in WMF's
        // `MFVideoFormat_RGB32` is documented as undefined (per MSDN:
        // "Media Foundation might or might not preserve its value")
        // — we hardcode 0xFF here so the texture renders opaque. If
        // we used `px[3]` instead, the alpha would often be 0 and the
        // entire preview would be invisibly blended out (= the exact
        // "preview shows nothing" bug 2026-05-25).
        for px in src.chunks_exact(4) {
            rgba.push(px[2]);
            rgba.push(px[1]);
            rgba.push(px[0]);
            rgba.push(0xFF);
        }
        let _ = unsafe { buffer.Unlock() };
        return Ok(Some((timestamp.max(0) as u64, rgba)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{
        Clip, ClipContent, Song, Track, TrackKind, VideoContent, VideoEvent,
        VideoSource, VideoSourcePath,
    };

    fn song_with_video_clip(bpm: f32, video_source_id: VideoSourceId) -> Song {
        let mut song = Song {
            bpm,
            ..Song::default()
        };
        let content_id = song.alloc_content_id();
        song.clip_contents.insert(
            content_id,
            ClipContent::Video(VideoContent {
                events: vec![VideoEvent {
                    source_id: video_source_id,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 8.0,
                    source_start_micros: 0,
                    source_end_micros: 4_000_000, // 4s
                    ..VideoEvent::default()
                }],
            }),
        );
        song.video_sources.insert(
            video_source_id,
            VideoSource {
                path: VideoSourcePath::Absolute("/dev/null".into()),
                width: 320,
                height: 240,
                framerate: 30.0,
                duration_micros: 4_000_000,
                codec: "h264".into(),
                audio_source_id: None,
            },
        );
        let mut track = Track {
            id: 1,
            kind: TrackKind::Video,
            name: "V".into(),
            ..Track::default()
        };
        track.clips.push(Clip {
            id: 1,
            name: "vid".into(),
            start_beat: 4.0,
            length_beats: 8.0,
            content_id,
            notes: Vec::new(),
        });
        song.tracks.push(track);
        song
    }

    #[test]
    fn active_source_at_returns_none_outside_clip() {
        let song = song_with_video_clip(120.0, 1);
        // playhead before clip start
        assert!(VideoPlaybackEngine::active_source_at(&song, 0.0).is_none());
        // playhead after clip end
        assert!(VideoPlaybackEngine::active_source_at(&song, 100.0).is_none());
    }

    #[test]
    fn active_source_at_returns_source_inside_clip() {
        // 120 bpm, clip starts at beat 4 (= 2s), playhead at beat 5 (=
        // 2.5s) → clip-local = 1 beat = 0.5s = 500_000μs.
        let song = song_with_video_clip(120.0, 7);
        let result = VideoPlaybackEngine::active_source_at(&song, 5.0)
            .expect("clip should be active at playhead 5.0");
        assert_eq!(result.0, 7);
        // Allow ±1μs rounding from f64 → u64.
        assert!(
            (result.1 as i64 - 500_000_i64).abs() <= 1,
            "expected ~500_000μs, got {}",
            result.1
        );
    }

    #[test]
    fn active_source_at_skips_audio_tracks() {
        let mut song = song_with_video_clip(120.0, 1);
        // Stick an Audio track at the top — must be skipped.
        let audio_track = Track {
            id: 2,
            kind: TrackKind::Audio,
            name: "A".into(),
            ..Track::default()
        };
        song.tracks.insert(0, audio_track);
        let result = VideoPlaybackEngine::active_source_at(&song, 5.0);
        assert!(result.is_some(), "video track should still be found");
        assert_eq!(result.unwrap().0, 1);
    }

    #[test]
    fn active_source_at_honors_event_muted() {
        let mut song = song_with_video_clip(120.0, 3);
        // Mute the only event → no source returned.
        let cid = song.tracks[0].clips[0].content_id;
        let Some(ClipContent::Video(content)) = song.clip_contents.get_mut(&cid) else {
            panic!("expected Video content");
        };
        content.events[0].muted = true;
        assert!(VideoPlaybackEngine::active_source_at(&song, 5.0).is_none());
    }

    #[test]
    fn decode_at_returns_rgba_frame_at_target_micros() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("decode_at: ffmpeg not on PATH, skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("playback.mp4");
        // 2-second 320x240 H.264 source with a smooth green gradient
        // so any decoded frame's center pixel reads as green-ish.
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "color=c=green:size=320x240:duration=2:rate=30",
                "-c:v", "libx264",
                "-pix_fmt", "yuv420p",
                "-y",
                mp4.to_str().unwrap(),
            ])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .expect("ffmpeg run");
        assert!(status.success());

        let mut engine = VideoPlaybackEngine::new();
        // Decode the frame at 1 second (= middle of the clip).
        let frame = engine
            .decode_at(42, &mp4, 1_000_000)
            .expect("decode_at @ 1s");
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 240);
        assert_eq!(frame.rgba.len(), 320 * 240 * 4);
        // Center pixel should be green-ish (G > R, G > B).
        let center = (120 * 320 + 160) * 4;
        let r = frame.rgba[center];
        let g = frame.rgba[center + 1];
        let b = frame.rgba[center + 2];
        assert!(g > 100, "center G should be high, got ({r}, {g}, {b})");
        assert!(g > r && g > b, "green channel should dominate");

        // Forward-step (= no seek): decode at 1.05s. Should reuse the
        // existing reader and just ReadSample forward — verified
        // implicitly by the result being a valid frame (no error).
        let frame2 = engine
            .decode_at(42, &mp4, 1_050_000)
            .expect("decode_at @ 1.05s (forward step)");
        assert_eq!(frame2.width, 320);
        assert_eq!(frame2.rgba.len(), 320 * 240 * 4);

        // Backward seek: decode at 0.3s. Engine should SetCurrentPosition
        // back to a keyframe and re-walk. Same validity check.
        let frame3 = engine
            .decode_at(42, &mp4, 300_000)
            .expect("decode_at @ 0.3s (backward seek)");
        assert_eq!(frame3.width, 320);
        assert_eq!(frame3.rgba.len(), 320 * 240 * 4);

        // Alpha channel must be hardcoded to 0xFF (= MFVideoFormat_RGB32's
        // undefined alpha byte must NOT leak through, or the preview
        // would render fully transparent). Scan a few pixels — every
        // 4th byte should be 255.
        for px in frame.rgba.chunks_exact(4).take(64) {
            assert_eq!(px[3], 0xFF, "alpha must be opaque 0xFF, got {}", px[3]);
        }
    }

    // ====================================================================
    // bgra_to_rgba (2026-05-25 playback コマ送り fix): SSSE3 SIMD path
    // is selected at runtime when available, scalar otherwise. Both
    // paths must produce byte-identical output.
    // ====================================================================

    #[test]
    fn bgra_to_rgba_swaps_channels_and_pins_alpha() {
        // 1 pixel: BGRA (10, 20, 30, 99) → RGBA (30, 20, 10, 255).
        let src = [10, 20, 30, 99];
        let rgba = bgra_to_rgba(&src);
        assert_eq!(rgba, vec![30, 20, 10, 255]);
    }

    #[test]
    fn bgra_to_rgba_handles_pure_colors() {
        // Pure blue BGRA = (255, 0, 0, _) → RGBA = (0, 0, 255, 255).
        let src = [255, 0, 0, 0];
        assert_eq!(bgra_to_rgba(&src), vec![0, 0, 255, 255]);
        // Pure red BGRA = (0, 0, 255, _) → RGBA = (255, 0, 0, 255).
        let src = [0, 0, 255, 0];
        assert_eq!(bgra_to_rgba(&src), vec![255, 0, 0, 255]);
        // Pure green BGRA = (0, 255, 0, _) → RGBA = (0, 255, 0, 255).
        let src = [0, 255, 0, 0];
        assert_eq!(bgra_to_rgba(&src), vec![0, 255, 0, 255]);
    }

    #[test]
    fn bgra_to_rgba_4_pixel_block_matches_scalar() {
        // 16-byte block = exactly one SSSE3 iteration. Verify SIMD
        // and scalar produce identical output.
        let src: Vec<u8> = (0..16).collect();
        let rgba = bgra_to_rgba(&src);
        let expected = vec![
            2, 1, 0, 255, // pixel 0
            6, 5, 4, 255, // pixel 1
            10, 9, 8, 255, // pixel 2
            14, 13, 12, 255, // pixel 3
        ];
        assert_eq!(rgba, expected);
    }

    #[test]
    fn bgra_to_rgba_large_buffer_handles_tail() {
        // Non-multiple-of-16: 5 pixels = 20 bytes (= 1 SSSE3 chunk +
        // 4-byte scalar tail). All 5 pixels should be converted.
        let src: Vec<u8> = (0..20).collect();
        let rgba = bgra_to_rgba(&src);
        // Pixel 4 (tail) bytes 16..20 = (16, 17, 18, 19) BGRA →
        // (18, 17, 16, 255) RGBA.
        assert_eq!(rgba.len(), 20);
        assert_eq!(&rgba[16..20], &[18, 17, 16, 255]);
    }

    #[test]
    fn bgra_to_rgba_scalar_path_matches_simd_path_random() {
        // Cross-check: synthesize 1024 random-ish bytes, run both
        // paths, assert byte-equality. Catches any mis-translation
        // of the shuffle mask vs the scalar indexing.
        let src: Vec<u8> = (0..1024).map(|i| ((i * 37) ^ 0xA5) as u8).collect();
        let simd_out = bgra_to_rgba(&src);
        let mut scalar_out = vec![0u8; src.len()];
        bgra_to_rgba_scalar(&src, &mut scalar_out);
        assert_eq!(simd_out, scalar_out, "SIMD and scalar paths must agree");
    }

    #[test]
    fn scale_for_preview_caps_long_edge() {
        // 1920x1080 → scaled to 960x540 (= long-edge 960, aspect kept).
        assert_eq!(scale_for_preview(1920, 1080), (960, 540));
        // 4K → 960x540 too (1920/3840 = 0.5).
        assert_eq!(scale_for_preview(3840, 2160), (960, 540));
        // Already small → identity (with even-round hygiene).
        assert_eq!(scale_for_preview(640, 480), (640, 480));
        assert_eq!(scale_for_preview(960, 540), (960, 540));
        // Portrait → long-edge cap on height.
        assert_eq!(scale_for_preview(1080, 1920), (540, 960));
        // Odd dimensions get rounded to even (NV12 hygiene).
        let (w, h) = scale_for_preview(1921, 1081);
        assert!(w % 2 == 0 && h % 2 == 0, "even-aligned, got {w}x{h}");
    }

    fn locate_ffmpeg() -> Option<std::path::PathBuf> {
        let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(exe))
                .find(|p| p.is_file())
        })
    }

    // ====================================================================
    // P7: multi-clip composite (active_sources_at) + per-event alpha.
    // ====================================================================

    #[test]
    fn active_sources_at_returns_empty_when_no_video_active() {
        let song = song_with_video_clip(120.0, 1);
        // playhead outside clip → empty
        assert!(VideoPlaybackEngine::active_sources_at(&song, 0.0).is_empty());
        assert!(VideoPlaybackEngine::active_sources_at(&song, 100.0).is_empty());
    }

    #[test]
    fn active_sources_at_returns_single_with_alpha_one_outside_fades() {
        // No fade_in / fade_out → alpha == 1.0 everywhere inside the clip.
        let song = song_with_video_clip(120.0, 5);
        let active = VideoPlaybackEngine::active_sources_at(&song, 5.0);
        assert_eq!(active.len(), 1);
        let frame = active[0];
        assert_eq!(frame.video_source_id, 5);
        assert_eq!(frame.z_index, 0);
        assert!(
            (frame.alpha - 1.0).abs() < 1e-6,
            "outside-fade alpha = 1.0, got {}",
            frame.alpha
        );
    }

    #[test]
    fn active_sources_at_applies_linear_fade_in() {
        // 8-beat event with fade_in=2 beats. At clip-local 1 beat
        // (half through the fade), Linear curve → alpha == 0.5.
        let mut song = song_with_video_clip(120.0, 1);
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Video(c)) = song.clip_contents.get_mut(&cid) {
            c.events[0].fade_in_beats = 2.0;
            c.events[0].fade_in_curve = common::model::FadeCurve::Linear;
        }
        // clip starts at beat 4, playhead at 5 → clip-local = 1 beat
        let active = VideoPlaybackEngine::active_sources_at(&song, 5.0);
        assert_eq!(active.len(), 1);
        assert!(
            (active[0].alpha - 0.5).abs() < 1e-3,
            "linear fade-in midpoint should be 0.5, got {}",
            active[0].alpha
        );
    }

    #[test]
    fn active_sources_at_applies_scurve_fade_out() {
        // 8-beat event with fade_out=2 beats. clip ends at beat 12,
        // playhead at 11 → 1 beat remaining → SCurve mid → 0.5.
        let mut song = song_with_video_clip(120.0, 1);
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Video(c)) = song.clip_contents.get_mut(&cid) {
            c.events[0].fade_out_beats = 2.0;
            c.events[0].fade_out_curve = common::model::FadeCurve::SCurve;
        }
        let active = VideoPlaybackEngine::active_sources_at(&song, 11.0);
        assert_eq!(active.len(), 1);
        assert!(
            (active[0].alpha - 0.5).abs() < 1e-3,
            "scurve fade-out midpoint should be 0.5, got {}",
            active[0].alpha
        );
    }

    #[test]
    fn active_sources_at_composites_multi_track_bottom_up() {
        // Stack 2 video tracks with clips covering the same playhead.
        // Bottom track gets z_index=0, top gets z_index=1.
        let mut song = song_with_video_clip(120.0, 1);
        // Add a second video track at the top with its own source.
        let cid2 = song.alloc_content_id();
        song.clip_contents.insert(
            cid2,
            ClipContent::Video(VideoContent {
                events: vec![VideoEvent {
                    source_id: 2,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 8.0,
                    source_start_micros: 0,
                    source_end_micros: 4_000_000,
                    ..VideoEvent::default()
                }],
            }),
        );
        song.video_sources.insert(
            2,
            VideoSource {
                path: VideoSourcePath::Absolute("/dev/null2".into()),
                width: 640,
                height: 480,
                framerate: 30.0,
                duration_micros: 4_000_000,
                codec: "h264".into(),
                audio_source_id: None,
            },
        );
        let top_track = Track {
            id: 2,
            kind: TrackKind::Video,
            name: "VTop".into(),
            clips: vec![Clip {
                id: 1,
                name: "vclip2".into(),
                start_beat: 4.0,
                length_beats: 8.0,
                content_id: cid2,
                notes: Vec::new(),
            }],
            next_clip_id: 2,
            ..Track::default()
        };
        // Insert at position 0 = top of arrangement.
        song.tracks.insert(0, top_track);

        let active = VideoPlaybackEngine::active_sources_at(&song, 5.0);
        assert_eq!(active.len(), 2, "both tracks should be active at 5.0");
        // z_index=0 is the bottom track (original source_id=1),
        // z_index=1 is the top (source_id=2). Caller renders in
        // ascending z_index so the source_id=2 layer ends up on top.
        assert_eq!(active[0].video_source_id, 1, "bottom track first");
        assert_eq!(active[0].z_index, 0);
        assert_eq!(active[1].video_source_id, 2, "top track second");
        assert_eq!(active[1].z_index, 1);
    }

    #[test]
    fn active_sources_at_drops_muted_events() {
        let mut song = song_with_video_clip(120.0, 1);
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Video(c)) = song.clip_contents.get_mut(&cid) {
            c.events[0].muted = true;
        }
        let active = VideoPlaybackEngine::active_sources_at(&song, 5.0);
        assert!(active.is_empty(), "muted event should be dropped");
    }

    #[test]
    fn active_sources_at_drops_zero_alpha_events() {
        // fade_in=2, position right at clip start → progress=0 →
        // alpha=0 → drop the frame entirely.
        let mut song = song_with_video_clip(120.0, 1);
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Video(c)) = song.clip_contents.get_mut(&cid) {
            c.events[0].fade_in_beats = 2.0;
        }
        // clip starts at beat 4 — playhead exactly at 4 = clip-local 0
        let active = VideoPlaybackEngine::active_sources_at(&song, 4.0);
        assert!(active.is_empty(), "alpha=0 frame should be dropped");
    }

    #[test]
    fn active_source_at_clamps_to_event_end_micros() {
        // Playhead right at the end of the event — source_micros should
        // clamp to source_end_micros (= 4_000_000 here, not extrapolate
        // beyond).
        let song = song_with_video_clip(120.0, 1);
        // 8 beats long at 120bpm = 4s. Clip starts at beat 4. End is
        // beat 12 (just after, so use beat 11.9 to stay inside).
        let result = VideoPlaybackEngine::active_source_at(&song, 11.9)
            .expect("should be inside clip");
        assert!(
            result.1 <= 4_000_000,
            "source_micros should be clamped to source_end_micros, got {}",
            result.1
        );
    }
}
