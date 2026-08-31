//! Playback-time video timeline resolution + the preview decode entry point.
//!
//! Decode itself is done by the **single libav engine**
//! (`libav_decoder::LibavVideoDecoder`, shared with export). Media Foundation /
//! D3D11 was removed (`docs/plan_video_decode_unify.md`): its reader-teardown
//! path crashed with a COM access violation (`combase.dll`, 2026-07-06) and it
//! could not decode the 10-bit H.264 this project uses anyway. This module now
//! keeps only the pure Song→active-frames timeline queries plus a thin
//! [`VideoPlaybackEngine`] that owns the libav decoder for the preview worker.
//!
//! Multi-clip composite (= crossfade + multi-track) walks the returned
//! [`ActiveVideoFrame`] list bottom-up; the runner pushes one aspect-fit quad
//! per layer with the per-event `alpha`.

use std::path::Path;

use common::model::{Clip, FadeCurve, Song, VideoEvent, VideoSourceId};
use common::tempo_map::TempoMap;

use crate::launcher_time::{RowScan, RowTimeline};

/// Default forward-walk budget (µs) handed to [`VideoPlaybackEngine::decode_at`].
/// Retained for the worker/decode signature; the libav decoder makes its own
/// seek-vs-forward-walk decision internally (`libav_decoder.rs`).
pub const DEFAULT_FORWARD_BUDGET_MICROS: u64 = 100_000;

/// One decoded frame, ready for the main thread to upload into the preview
/// scene. libav decodes every source (incl. 10-bit H.264 High10, HEVC, AV1) to
/// tightly-packed BGRA8 via swscale; the main thread uploads it with
/// `Renderer::upload_texture_bgra`. There is a single path now — the old
/// zero-copy D3D11 `Shared` variant went away with Media Foundation.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly-packed BGRA8 in scanline order, length = `width * height * 4`.
    pub bgra: Vec<u8>,
}

impl DecodedFrame {
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
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
    /// この動画フレームを持つ track id (映像効果チェーンの解決に使う、
    /// `ActiveImageFrame::owning_track_id` と対)。
    pub owning_track_id: u32,
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

/// Stateful playback decoder owned by the preview worker. Wraps the single
/// libav engine (shared with export) so the worker's `engine.decode_at(...)`
/// call site is unchanged. Readers are created lazily per `VideoSourceId`
/// inside the libav decoder; nothing here touches the OS COM/MF runtime.
pub struct VideoPlaybackEngine {
    libav: crate::libav_decoder::LibavVideoDecoder,
}

impl VideoPlaybackEngine {
    pub fn new() -> Self {
        Self {
            libav: crate::libav_decoder::LibavVideoDecoder::new_preview(),
        }
    }

    /// Pure helper: walk the song bottom-up and return every video
    /// clip active now, with a per-event `alpha` derived from the
    /// clip's `fade_in_beats` / `fade_out_beats` (= MVP crossfade
    /// behaviour). Returns a `Vec<ActiveVideoFrame>` ordered from
    /// lowest to topmost track so the caller can composite by
    /// call-order (= last pushed quad ends up on top).
    /// v12 (`docs/plan_video.md` §4 P7).
    ///
    /// v35 (r.md #87): 「どのクリップを、どの拍で見るか」は
    /// [`RowTimeline::track_scan`] が行ごとに解く。アレンジ主導の行
    /// (`RowPlayback::Arranger`) では `track.clips` を song の playhead で見る
    /// **従来と同一**の経路になり、ランチャー主導の行では源が
    /// `track.session_clips` のセル 1 つ・拍がセル内の位相に切り替わる。
    ///
    /// Muted events are dropped from the result entirely.
    pub fn active_sources_at(song: &Song, rows: &RowTimeline<'_>) -> Vec<ActiveVideoFrame> {
        let bpm = song.bpm as f64;
        if bpm <= 0.0 {
            return Vec::new();
        }
        // A4 (r.md #8): tempo automation がある曲は映像 source 時間 (秒) を tempo
        // 積分で求める (= 映像が音とズレない)。 constant 曲は従来の高速 60/bpm 経路。
        let tempo_map = song_tempo_map_if_automated(song);
        let mut out: Vec<ActiveVideoFrame> = Vec::new();
        // `song.tracks[0]` is the top of the arrangement, so iterating
        // `.rev()` yields bottom-most → topmost. Each video track gets
        // a contiguous `z_index` counter so events on the same track
        // share a layer (= their alphas blend within layer instead of
        // creating a third layer between clip A and clip B during
        // crossfade).
        let mut z_index: u32 = 0;
        for track in song.tracks.iter().rev() {
            // v16: TrackKind 廃止後は「video_events を持つ clip がある
            // track」 が visual composite に参加する (= filter は content
            // kind で行う、 後段 `content.video_events()` で None を skip)。
            // 自身の mute だけでなく、 グループ親の mute / solo (audio と同じ
            // effective-mute) で silenced な track は preview / render の両方で
            // skip する (`Song::track_visually_silenced` が SSoT)。
            if song.track_visually_silenced(track.id) {
                continue;
            }
            // r.md #87: ランチャーが握っていて無音の行 (Stop Clips / ワンショット
            // 終端) は 1 枚も出さない。
            let Some(scan) = rows.track_scan(track) else {
                continue;
            };
            let mut track_emitted = false;
            for clip in scan.clips {
                // muted clip は video composite から除外する (黒/下層が出る)。
                if clip.muted {
                    continue;
                }
                track_emitted |= push_clip_video_frames(
                    song,
                    track.id,
                    clip,
                    &scan,
                    tempo_map.as_ref(),
                    bpm,
                    z_index,
                    &mut out,
                );
            }
            if track_emitted {
                z_index += 1;
            }
        }
        out
    }

    /// Decode the frame at `target_micros` (source time) for `source_path` via
    /// the single libav engine, returning tightly-packed BGRA8 at the source's
    /// native resolution. `slot_idx` / `forward_budget_micros` are retained for
    /// the worker's ring-loop signature; the libav BGRA sink is a
    /// 1-frame-latest upload so only slot 0 produces a frame (slots > 0 return
    /// an error so the worker truncates the ring to the playhead).
    pub fn decode_at(
        &mut self,
        video_source_id: VideoSourceId,
        source_path: &Path,
        target_micros: u64,
        slot_idx: u8,
        forward_budget_micros: u64,
    ) -> Result<DecodedFrame, String> {
        let _ = forward_budget_micros;
        if slot_idx != 0 {
            return Err("libav: center slot only".to_string());
        }
        let f = self
            .libav
            .decode_at(video_source_id, source_path, target_micros)?;
        Ok(DecodedFrame {
            width: f.width,
            height: f.height,
            bgra: f.bgra,
        })
    }
}

impl Default for VideoPlaybackEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// `clip` が `scan.clip_beat` を覆っているなら、その中で active な video event を
/// `out` へ積む。1 枚でも積んだら `true` (呼び側の `z_index` 前進の判定に使う)。
///
/// `active_sources_at` から切り出したのは、アレンジ行 (`track.clips` を song 拍で
/// 走査) とランチャー行 (セル 1 つをセル内位相で走査) が **同じ 1 本**を通るように
/// するため。`scan` が座標系の違いを吸収するので、ここから下は完全に共通。
#[allow(clippy::too_many_arguments)]
fn push_clip_video_frames(
    song: &Song,
    track_id: u32,
    clip: &Clip,
    scan: &RowScan<'_, Clip>,
    tempo_map: Option<&TempoMap>,
    bpm: f64,
    z_index: u32,
    out: &mut Vec<ActiveVideoFrame>,
) -> bool {
    let clip_start = clip.start_beat;
    let clip_end = clip.start_beat + clip.length_beats;
    if scan.clip_beat < clip_start || scan.clip_beat >= clip_end {
        return false;
    }
    // r.md #44: content 上の位置は clip 開始ではなく content 原点基準
    // (= 左端 trim した clip は窓の分だけ content の先を見せる)。
    let clip_local = clip.song_to_content_beat(scan.clip_beat);
    let Some(content) = song.clip_contents.get(&clip.content_id) else {
        return false;
    };
    let Some(events) = content.video_events() else {
        return false;
    };
    // tempo 積分は song-absolute な拍でしか意味を持たないので、ランチャー行では
    // 「セルを撃った拍 + 位相」(= 計画書 §2.1 の effective_beat) で写像する。
    let row_secs = tempo_map.map(|m| m.beat_to_seconds(scan.song_beat));
    let mut emitted = false;
    for event in events {
        let event_end = event.event_start_in_clip_beats + event.event_length_beats;
        if clip_local < event.event_start_in_clip_beats || clip_local >= event_end {
            continue;
        }
        if event.muted {
            continue;
        }
        let event_progress_beats = clip_local - event.event_start_in_clip_beats;
        let event_progress_secs = match (tempo_map, row_secs) {
            (Some(m), Some(now_secs)) => {
                // content-local 拍 → song-absolute 拍は `content_to_song_beat` が
                // 唯一の口 (r.md #44)。`clip.start_beat + <content-local>` と直に
                // 書くと **左端を trim したクリップで `content_offset_beats` ぶん
                // 起点が後ろへずれ**、テンポ自動化のある曲だけ映像 source 時刻が
                // ずれる (`event_progress_beats` を使う定 BPM 経路は content-local
                // の引き算なので正しく、この経路だけが食い違っていた)。
                // ランチャー行ではさらに `song_origin()` (= セルを撃った拍) を足す。
                let event_start = scan.song_origin()
                    + clip.content_to_song_beat(event.event_start_in_clip_beats);
                (now_secs - m.beat_to_seconds(event_start)).max(0.0)
            }
            _ => event_progress_beats * 60.0 / bpm,
        };
        let event_progress_micros = (event_progress_secs * 1_000_000.0).round() as u64;
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
            owning_track_id: track_id,
            source_micros,
            alpha,
            z_index,
        });
        emitted = true;
    }
    emitted
}

/// tempo automation (SongTempo lane) があれば `TempoMap` を build する (= 映像
/// source 時間を tempo 積分で求めて音とズレないようにする、 A4 r.md #8)。 無ければ
/// `None` で constant bpm の高速経路を使う。 build は O(song length) だが tempo
/// automation を持つ曲のみ (一般の constant 曲は無コスト)。
fn song_tempo_map_if_automated(song: &Song) -> Option<common::tempo_map::TempoMap> {
    let automated = song.song_lanes.iter().any(|l| {
        l.enabled && matches!(l.target, common::model::AutomationTarget::SongTempo)
    });
    automated.then(|| common::tempo_map::TempoMap::from_song(song))
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
    // r.md #38: fade カーブの式は `common::audio_render::fade_curve_at` が唯一の SSoT
    // (音 / 映像 / 画像 / 字幕 / アレンジ画面の描画が全部ここを通る)。
    common::audio_render::fade_curve_at(progress, curve)
}

/// BGRA8 → RGBA8 channel swap with alpha pinned to 0xFF. SSSE3-accelerated when
/// available, scalar otherwise; both paths produce byte-identical output. Used
/// by callers that need RGBA (e.g. thumbnail / offscreen upload).
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

#[cfg(test)]
mod tests {
    use super::*;
    // 公開前整備: fixture 用 ffmpeg の解決とエンコーダ指定は `crate::test_ffmpeg` が SSoT。
    // 以前は 3 モジュールが同じ helper を各自に持ち、いずれも PATH の ffmpeg を探していた。
    use crate::test_ffmpeg::{locate_ffmpeg, skip_reason, H264_ENCODER};
    use common::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
        AutomationTarget, Clip, ClipContent, Song, VideoContent, VideoEvent, VideoSource,
        VideoSourcePath,
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
        song.media.video_sources.insert(
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
        let mut track = crate::app::track_with(|t| {
            t.id = 1;
            t.name = "V".into();
        });
        track.clips.push(Clip {
            id: 1,
            start_beat: 4.0,
            length_beats: 8.0,
            content_id,
            color: None,
            auto_lipsync: false,
            ..Default::default()
        });
        song.tracks.push(track);
        song
    }

    /// r.md #8 A4: tempo automation 下では映像 source 時間が tempo 積分で進む
    /// (= 一定 bpm 換算の `progress*60/bpm` とズレ、 映像が音と同期し続ける)。
    #[test]
    fn active_sources_at_honors_tempo_automation() {
        // base 60bpm の video clip (clip/event は beat 4 始まり) に、 60→180 linear
        // の SongTempo lane [0,12) を載せる。 beat 4..8 は 100..140 bpm。
        let mut song = song_with_video_clip(60.0, 1);
        song.length_beats = 12.0;
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint { id: 1, time_beat: 0.0, value: 60.0, curve: AutomationCurve::Linear },
                    AutomationPoint { id: 2, time_beat: 12.0, value: 180.0, curve: AutomationCurve::Linear },
                ],
                next_point_id: 3,
            }),
        );
        song.song_lanes.push(AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "t".into(),
                start_beat: 0.0,
                length_beats: 12.0,
                content_id: cid,
                content_offset_beats: 0.0,
            }],
            ..AutomationLane::new(AutomationTarget::SongTempo, 60.0)
        });
        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(8.0));
        assert_eq!(active.len(), 1, "beat 8 は clip [4, 12) の中");
        let secs = active[0].source_micros as f64 / 1_000_000.0;
        // 期待値 = tempo 積分した beat 4→8 の実時間。
        let m = common::tempo_map::TempoMap::from_song(&song);
        let expected = m.beat_to_seconds(8.0) - m.beat_to_seconds(4.0);
        assert!((secs - expected).abs() < 0.02, "secs={secs} expected={expected}");
        // 一定 60bpm 換算 (4 拍 = 4.0s) より明確に短い (テンポが速いので)。
        assert!(secs < 3.5, "tempo-integrated should beat constant-60 (4.0s), got {secs}");
    }

    /// 左端を trim したクリップ (`content_offset_beats > 0`) の source 時刻は、
    /// **テンポ自動化のある曲でも** content 原点基準で進む (r.md #44)。
    ///
    /// この経路だけが `clip.start_beat + <content-local 拍>` を直に組み立てていて、
    /// `content_offset_beats` ぶん起点が後ろへずれていた。定 BPM 経路は
    /// content-local の引き算なので正しく、**テンポ自動化を載せたときだけ**
    /// 映像が音とズレる = build / clippy / 既存テストを全部すり抜ける。
    #[test]
    fn 左端_trim_した映像はテンポ自動化下でも_content_原点基準で進む() {
        let mut song = song_with_video_clip(60.0, 1);
        song.length_beats = 12.0;
        // clip を左端 2 拍ぶん trim (content 原点 = 拍 2、窓は [4, 12))。
        song.tracks[0].clips[0].content_offset_beats = 2.0;
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint { id: 1, time_beat: 0.0, value: 60.0, curve: AutomationCurve::Linear },
                    AutomationPoint { id: 2, time_beat: 12.0, value: 180.0, curve: AutomationCurve::Linear },
                ],
                next_point_id: 3,
            }),
        );
        song.song_lanes.push(AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "t".into(),
                start_beat: 0.0,
                length_beats: 12.0,
                content_id: cid,
                content_offset_beats: 0.0,
            }],
            ..AutomationLane::new(AutomationTarget::SongTempo, 60.0)
        });
        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(8.0));
        assert_eq!(active.len(), 1);
        let secs = active[0].source_micros as f64 / 1_000_000.0;
        let m = common::tempo_map::TempoMap::from_song(&song);
        // event の content-local 0.0 が置かれる song 拍 = content 原点 = 2.0。
        let expected = m.beat_to_seconds(8.0) - m.beat_to_seconds(2.0);
        assert!((secs - expected).abs() < 0.02, "secs={secs} expected={expected}");
        // 旧実装 (`clip.start_beat` 起点 = 拍 4) との差は誤差ではなく秒単位。
        let wrong = m.beat_to_seconds(8.0) - m.beat_to_seconds(4.0);
        assert!((secs - wrong).abs() > 0.5, "content_offset を落としている");
    }

    #[test]
    fn active_sources_at_maps_clip_local_beat_to_source_micros() {
        // 120 bpm, clip starts at beat 4 (= 2s), playhead at beat 5 (=
        // 2.5s) → clip-local = 1 beat = 0.5s = 500_000μs.
        let song = song_with_video_clip(120.0, 7);
        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(5.0));
        assert_eq!(active.len(), 1, "clip should be active at playhead 5.0");
        assert_eq!(active[0].video_source_id, 7);
        // Allow ±1μs rounding from f64 → u64.
        assert!(
            (active[0].source_micros as i64 - 500_000_i64).abs() <= 1,
            "expected ~500_000μs, got {}",
            active[0].source_micros
        );
    }

    #[test]
    fn decode_at_returns_bgra_frame_at_target_micros() {
        let Some(ffmpeg) = locate_ffmpeg() else {
            eprintln!("{}", skip_reason("decode_at"));
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mp4 = dir.path().join("playback.mp4");
        // 2-second 320x240 H.264 source with a solid green fill so any
        // decoded frame's center pixel reads as green-ish.
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-f", "lavfi",
                "-i", "color=c=green:size=320x240:duration=2:rate=30",
                "-c:v", H264_ENCODER,
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
            .decode_at(42, &mp4, 1_000_000, 0, DEFAULT_FORWARD_BUDGET_MICROS)
            .expect("decode_at @ 1s");
        assert_eq!(frame.width(), 320);
        assert_eq!(frame.height(), 240);
        // libav decodes to system-memory BGRA, so we can inspect pixels.
        assert_eq!(frame.bgra.len(), 320 * 240 * 4);
        // Center pixel should be green-ish. BGRA memory order:
        // px[0] = B, px[1] = G, px[2] = R, px[3] = alpha.
        let center = (120 * 320 + 160) * 4;
        let b = frame.bgra[center];
        let g = frame.bgra[center + 1];
        let r = frame.bgra[center + 2];
        assert!(g > 100, "center G should be high, got ({r}, {g}, {b})");
        assert!(g > r && g > b, "green channel should dominate");

        // Forward-step (= no seek): decode at 1.05s. Should reuse the
        // existing decoder and just decode forward — verified implicitly
        // by the result being a valid frame (no error).
        let frame2 = engine
            .decode_at(42, &mp4, 1_050_000, 0, DEFAULT_FORWARD_BUDGET_MICROS)
            .expect("decode_at @ 1.05s (forward step)");
        assert_eq!(frame2.width(), 320);
        assert_eq!(frame2.height(), 240);

        // Backward seek: decode at 0.3s. Engine should seek back to a
        // keyframe and re-walk. Same validity check.
        let frame3 = engine
            .decode_at(42, &mp4, 300_000, 0, DEFAULT_FORWARD_BUDGET_MICROS)
            .expect("decode_at @ 0.3s (backward seek)");
        assert_eq!(frame3.width(), 320);
        assert_eq!(frame3.height(), 240);
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


    // ====================================================================
    // P7: multi-clip composite (active_sources_at) + per-event alpha.
    // ====================================================================

    #[test]
    fn active_sources_at_returns_empty_when_no_video_active() {
        let song = song_with_video_clip(120.0, 1);
        // playhead outside clip → empty
        assert!(VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(0.0)).is_empty());
        assert!(VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(100.0)).is_empty());
    }

    #[test]
    fn active_sources_at_returns_single_with_alpha_one_outside_fades() {
        // No fade_in / fade_out → alpha == 1.0 everywhere inside the clip.
        let song = song_with_video_clip(120.0, 5);
        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(5.0));
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
        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(5.0));
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
        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(11.0));
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
        song.media.video_sources.insert(
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
        let top_track = crate::app::track_with(|t| {
            t.id = 2;
            t.name = "VTop".into();
            t.clips = vec![Clip {
                id: 1,
                start_beat: 4.0,
                length_beats: 8.0,
                content_id: cid2,
                color: None,
                auto_lipsync: false,
                ..Default::default()
            }];
            t.next_clip_id = 2;
        });
        // Insert at position 0 = top of arrangement.
        song.tracks.insert(0, top_track);

        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(5.0));
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
        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(5.0));
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
        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(4.0));
        assert!(active.is_empty(), "alpha=0 frame should be dropped");
    }

    #[test]
    fn active_sources_at_clamps_to_event_end_micros() {
        // Playhead right at the end of the event — source_micros should
        // clamp to source_end_micros (= 4_000_000 here, not extrapolate
        // beyond).
        let song = song_with_video_clip(120.0, 1);
        // 8 beats long at 120bpm = 4s. Clip starts at beat 4. End is
        // beat 12 (just after, so use beat 11.9 to stay inside).
        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(11.9));
        assert_eq!(active.len(), 1, "should be inside clip");
        assert!(
            active[0].source_micros <= 4_000_000,
            "source_micros should be clamped to source_end_micros, got {}",
            active[0].source_micros
        );
    }

    // ====================================================================
    // r.md #87 クリップランチャー: 行ごとの実効拍とイベント源
    // ====================================================================

    #[test]
    fn ランチャー主導の行はセルの映像をループで映す() {
        use common::model::{LaunchSettings, RowPlayback, SessionClip};
        // アレンジのクリップ (拍 4 から 8 拍) と、別 source を指す 4 拍のセルを
        // 同じトラックに置く。
        let mut song = song_with_video_clip(120.0, 1);
        let cell_content = song.alloc_content_id();
        song.clip_contents.insert(
            cell_content,
            ClipContent::Video(VideoContent {
                events: vec![VideoEvent {
                    source_id: 42,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 4.0,
                    source_start_micros: 0,
                    source_end_micros: 4_000_000,
                    ..VideoEvent::default()
                }],
            }),
        );
        song.tracks[0].session_clips.push(SessionClip {
            scene_id: 1,
            clip: Clip {
                id: 9,
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cell_content,
                ..Clip::default()
            },
            launch: LaunchSettings::default(),
        });
        song.tracks[0].launcher = RowPlayback::Launcher { clip_id: 9 };

        // 拍 5: 起点 (曲頭) から 5 拍 → 4 拍セルの位相 1.0 → 0.5s。
        // アレンジのクリップ (source 1) ではなくセル (source 42) が映る。
        let active = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(5.0));
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].video_source_id, 42, "源はセル 1 つ");
        assert!(
            (active[0].source_micros as i64 - 500_000).abs() <= 1,
            "位相 1 拍 = 0.5s, got {}",
            active[0].source_micros
        );

        // 拍 9: 位相 1.0 に巻き戻る (ループ跨ぎ) — 拍 5 と同じ絵。
        let looped = VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(9.0));
        assert_eq!(looped[0].source_micros, active[0].source_micros);

        // ワンショットにすると拍 8 以降は何も映さない。
        song.tracks[0].session_clips[0].launch.looping = false;
        assert!(
            VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(9.0)).is_empty(),
            "ワンショットは終端で消える"
        );
    }

    #[test]
    fn 停止したランチャー行はアレンジのクリップも映さない() {
        use common::model::RowPlayback;
        let mut song = song_with_video_clip(120.0, 1);
        // 拍 5 はアレンジのクリップの中 (通常なら 1 枚出る)。
        assert_eq!(
            VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(5.0)).len(),
            1
        );
        song.tracks[0].launcher = RowPlayback::LauncherStopped;
        assert!(
            VideoPlaybackEngine::active_sources_at(&song, &RowTimeline::preview(5.0)).is_empty(),
            "ランチャーが握ったまま無音の行はアレンジへ戻らない"
        );
    }
}
