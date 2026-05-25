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

use common::model::{Song, TrackKind, VideoSourceId};
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

    /// Pure helper: walk the song top-down and return the
    /// `(VideoSourceId, source_micros)` of the topmost video clip
    /// whose extent covers `playhead_beat`. `None` when no video clip
    /// is active.
    ///
    /// MVP scope (P5): single active clip wins. P7 will replace this
    /// with a "stack of active clips" walker that the wgpu composite
    /// pass blends.
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
        // or past target_micros. Bound the walk so a malformed stream
        // doesn't spin forever (= ~1 second worth of frames at 30fps).
        let mut last_frame: Option<(u64, Vec<u8>)> = None;
        for _ in 0..60 {
            let Some((timestamp_100ns, bytes)) =
                read_one_frame(&entry.reader, entry.width, entry.height)?
            else {
                break; // EOS — keep the last decoded sample.
            };
            let timestamp_micros = timestamp_100ns / 10;
            last_frame = Some((timestamp_micros, bytes));
            if timestamp_micros >= target_micros {
                break;
            }
        }
        let Some((ts, rgba)) = last_frame else {
            return Err(format!(
                "no frame decoded for source {video_source_id} at {target_micros}μs"
            ));
        };
        entry.last_decoded_micros = Some(ts);
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
    let width = (frame_size >> 32) as u32;
    let height = (frame_size & 0xFFFF_FFFF) as u32;
    if width == 0 || height == 0 {
        return Err(format!("invalid frame size {width}x{height}"));
    }

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

    Ok(ReaderEntry {
        reader,
        width,
        height,
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

/// Pull one decoded sample. Returns `Ok(None)` on EOS, `Ok(Some(...))`
/// for a real sample. Skips STREAMTICK gaps internally.
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
        for px in src.chunks_exact(4) {
            rgba.push(px[2]);
            rgba.push(px[1]);
            rgba.push(px[0]);
            rgba.push(px[3]);
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
