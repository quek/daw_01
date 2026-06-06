//! Image overlay composite helpers (`docs/plan_image_overlay.md` §P3).
//!
//! OS-neutral (= image clips have no decoder dependency; the `image`
//! crate handles import in `import_image.rs`, and the per-source GPU
//! texture is uploaded once at import time). This module provides:
//!
//! - [`ActiveImageFrame`] — what the composite pass needs to draw one
//!   image layer at the current playhead.
//! - [`active_image_sources_at`] — pure function that walks the song,
//!   collects all currently-active image events, and returns them
//!   bottom-up so the caller can interleave them with video layers
//!   from `video_playback::active_sources_at` by `z_index`.

use common::model::{
    AutomationLane, AutomationTarget, FadeCurve, ImageBuiltinParam, ImageEvent, ImageSourceId,
    Song, Track,
};

/// One image event active at the current playhead. Mirrors
/// `video_playback::ActiveVideoFrame` so the caller can merge video +
/// image layers in a single composite pass by sorting on `z_index`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveImageFrame {
    /// Which image to sample. The preview / render path reads
    /// `AppData::image_texture_cache[source_id]` to get the GPU
    /// handle. Missing entries are skipped silently (= not yet
    /// uploaded, first frame after import).
    pub image_source_id: ImageSourceId,
    /// v19 (`docs/plan_tachie_group_transform.md` §5.1): この frame を出した
    /// track の id。立ち絵 group 合成で frame を `parent_group_id` ごとに
    /// 仕分ける（= 親 group の visual transform 対象か判定する）ために使う。
    pub owning_track_id: u32,
    /// PiP rect in normalized 0-1 preview coordinates. `(x, y)` is
    /// the top-left of the image's bounding box; `(w, h)` is its
    /// dimensions. `(0.0, 0.0, 1.0, 1.0)` fills the preview;
    /// `(0.7, 0.0, 0.3, 0.3)` lands a 30% × 30% rect in the top-right
    /// corner. Out-of-range values (= negative w/h, x+w > 1) are
    /// tolerated by the composite pass (= they may render outside the
    /// preview, the runner clips to the surface).
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Per-event alpha = `opacity × fade_envelope(clip_local)`. Range
    /// `0.0..=1.0`. Caller multiplies this into the textured-quad
    /// alpha (= standard "src over dst" blend).
    pub alpha: f32,
    /// v15 (`docs/plan_image_automation.md` rotation): rect 中心を旋回
    /// 中心とする 2D 回転 (radians、 clockwise positive)。 lane override
    /// が effective なら lane 値、 さもなくば `ImageEvent.rotation_radians`。
    /// `0.0` = axis-aligned。 gui_01 #047 (`TexturedQuad.rotation_radians`)
    /// landing 後に preview / render passes が wire する。
    pub rotation_radians: f32,
    /// Bottom-up draw order, same numbering as
    /// `video_playback::ActiveVideoFrame::z_index`. The runner sorts
    /// all (video + image) frames by `z_index` ascending before
    /// pushing to the composite scene so call-order interleave
    /// places higher tracks on top.
    pub z_index: u32,
}

/// Walk the song bottom-up (= `track[N-1]` first → `track[0]` last)
/// and emit one `ActiveImageFrame` per active image event at
/// `playhead_beat`. Mirrors `video_playback::active_sources_at` for
/// image clips so the two are interleaved by the caller via
/// `z_index`.
///
/// Muted events and events whose fade envelope evaluates to 0 are
/// dropped from the result entirely.
pub fn active_image_sources_at(song: &Song, playhead_beat: f64) -> Vec<ActiveImageFrame> {
    let bpm = song.bpm as f64;
    if bpm <= 0.0 {
        return Vec::new();
    }
    let mut out: Vec<ActiveImageFrame> = Vec::new();
    // `song.tracks[0]` is the top of the arrangement; iterate `.rev()`
    // for bottom→top emit order (preview / export layer their composite
    // by emit order). The `.rev().enumerate()` index is the track's
    // position counted from the bottom, so it doubles as `z_index`:
    // bottom-most track → `0`, top track → `len-1`, monotone with the
    // real `song.tracks` index. Anchoring `z_index` to the real track
    // position (instead of a separate counter that only advanced on
    // emit) keeps it stable per track and removes the silently-packed
    // numbering the old counter produced (`[Low]`). Gaps from muted /
    // non-emitting tracks are harmless: layering is still emit-ordered,
    // `z_index` is metadata for the future multi-kind sort.
    for (idx, track) in song.tracks.iter().rev().enumerate() {
        // v16: TrackKind 廃止後は「image_events を持つ clip がある track」
        // が visual composite に参加する (= filter は content kind で行う)。
        // track.muted (= mixer M トグル) で image overlay も無効化、 SSoT
        // として preview / render 両方で扱う。
        if track.muted {
            continue;
        }
        let z_index = idx as u32;
        // Build the per-track `ImageBuiltin` lane index once (= a single
        // pass over `track.automation_lanes`) instead of re-`find`ing
        // each of the 6 fields for every event below (`[Mid]`).
        let lanes = ImageLaneIndex::build(track);
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
            let Some(events) = content.image_events() else {
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
                let env = event_alpha_envelope(event, clip_local);
                // lane override (`docs/plan_image_automation.md` §3.1)。
                // track-level lane の時間軸は track-global beats、 lane
                // が無い field は ImageEvent.field をそのまま使う。
                // opacity は fade envelope と multiply (= override しても
                // fade が効く)。 rotation は lane 経路の override のみ
                // (= fade 無関係)。
                let (x, y, w, h, opacity, rotation) =
                    resolve_image_fields(&lanes, song, event, playhead_beat);
                let alpha = opacity * env;
                if alpha <= 0.0 {
                    continue;
                }
                out.push(ActiveImageFrame {
                    image_source_id: event.source_id,
                    owning_track_id: track.id,
                    x,
                    y,
                    w,
                    h,
                    alpha,
                    rotation_radians: rotation,
                    z_index,
                });
            }
        }
    }
    out
}

/// Per-track index of the `ImageBuiltin` automation lanes, built once
/// in [`active_image_sources_at`] before walking a track's events so
/// the 6 lane lookups become array indexing instead of 6 linear
/// `find` scans of `track.automation_lanes` per event (= mirrors the
/// per-track resolve idiom of `text_compose`). Each slot is `None`
/// when the track has no lane targeting that field.
struct ImageLaneIndex<'a> {
    x: Option<&'a AutomationLane>,
    y: Option<&'a AutomationLane>,
    w: Option<&'a AutomationLane>,
    h: Option<&'a AutomationLane>,
    opacity: Option<&'a AutomationLane>,
    rotation: Option<&'a AutomationLane>,
}

impl<'a> ImageLaneIndex<'a> {
    /// Single pass over `track.automation_lanes` recording the first
    /// lane found for each `ImageBuiltinParam`. (Lane ids are unique
    /// per target in practice, so "first" is the only one.)
    fn build(track: &'a Track) -> Self {
        let mut index = ImageLaneIndex {
            x: None,
            y: None,
            w: None,
            h: None,
            opacity: None,
            rotation: None,
        };
        for lane in &track.automation_lanes {
            let AutomationTarget::ImageBuiltin(param) = lane.target else {
                continue;
            };
            let slot = match param {
                ImageBuiltinParam::X => &mut index.x,
                ImageBuiltinParam::Y => &mut index.y,
                ImageBuiltinParam::W => &mut index.w,
                ImageBuiltinParam::H => &mut index.h,
                ImageBuiltinParam::Opacity => &mut index.opacity,
                ImageBuiltinParam::Rotation => &mut index.rotation,
            };
            if slot.is_none() {
                *slot = Some(lane);
            }
        }
        index
    }
}

/// `ImageEvent.{x,y,w,h,opacity}` + rotation を track-level `ImageBuiltin`
/// lane で override する。 lane が存在し enabled / clip カバー範囲内 /
/// point 列を持つ場合のみ lane の値、 さもなくば event の値を返す。 lane
/// 探索は呼び出し前に [`ImageLaneIndex`] へ済ませてあるので、 ここでは
/// index 参照 + `lane_value_at` のみ。
fn resolve_image_fields(
    lanes: &ImageLaneIndex<'_>,
    song: &Song,
    event: &ImageEvent,
    song_beat: f64,
) -> (f32, f32, f32, f32, f32, f32) {
    let resolve_norm = |lane: Option<&AutomationLane>, fallback: f32| -> f32 {
        let Some(lane) = lane else {
            return fallback;
        };
        // lane_value_at は lane.default_value をフォールバックに使うが、
        // image lane の default は event 値とは独立 (= lane を空で作っ
        // ても event 値が見えるべきではない、 lane を作った時点で lane
        // が override)。 lane が enabled で clip が無い区間でも default
        // _value が effective。
        let v = common::automation::lane_value_at(lane, &song.clip_contents, song_beat);
        (v as f32).clamp(0.0, 1.0)
    };
    let rotation = match lanes.rotation {
        // Rotation は plain で radians、 範囲 -π..=π を超えても modulo
        // 2π で wrap (= 連続回転 lane を許容)。 lane.default_value は
        // 既に radians 単位。
        Some(lane) => {
            common::automation::lane_value_at(lane, &song.clip_contents, song_beat) as f32
        }
        None => event.rotation_radians,
    };
    let x = resolve_norm(lanes.x, event.x);
    let y = resolve_norm(lanes.y, event.y);
    let w = resolve_norm(lanes.w, event.w);
    let h = resolve_norm(lanes.h, event.h);
    let opacity = resolve_norm(lanes.opacity, event.opacity);
    (x, y, w, h, opacity, rotation)
}

/// Per-event fade envelope at the given clip-local beat. Range
/// `0.0..=1.0`. Mirrors `video_playback::event_alpha` exactly so
/// crossfade visuals between an image and a video stay in step.
fn event_alpha_envelope(event: &ImageEvent, clip_local_beat: f64) -> f32 {
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
    if event.fade_out_beats > 0.0
        && event_remaining > 0.0
        && event_remaining < event.fade_out_beats
    {
        let progress = (event_remaining / event.fade_out_beats) as f32;
        alpha *= fade_curve_value(progress, event.fade_out_curve);
    }
    alpha.clamp(0.0, 1.0)
}

fn fade_curve_value(progress: f32, curve: FadeCurve) -> f32 {
    let x = progress.clamp(0.0, 1.0);
    match curve {
        FadeCurve::Linear => x,
        FadeCurve::Exponential => x * x,
        FadeCurve::SCurve => 0.5 - 0.5 * (std::f32::consts::PI * x).cos(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{
        Clip, ClipContent, ImageContent, ImageEvent, ImageSource, ImageSourcePath, Track,
    };

    fn make_song_with_one_image(
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        opacity: f32,
        event_length: f64,
        fade_in: f64,
        fade_out: f64,
    ) -> Song {
        let mut song = Song {
            bpm: 120.0,
            length_beats: 32.0,
            ..Song::default()
        };
        let img_id = song.alloc_image_source_id();
        song.image_sources.insert(
            img_id,
            ImageSource {
                path: ImageSourcePath::Absolute("/tmp/x.png".into()),
                name: "x.png".into(),
                width: 100,
                height: 100,
                format: "Png".into(),
            },
        );
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Image(ImageContent {
                events: vec![ImageEvent {
                    source_id: img_id,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: event_length,
                    x,
                    y,
                    w,
                    h,
                    opacity,
                    rotation_radians: 0.0,
                    muted: false,
                    fade_in_beats: fade_in,
                    fade_out_beats: fade_out,
                    fade_in_curve: FadeCurve::Linear,
                    fade_out_curve: FadeCurve::Linear,
                }],
            }),
        );
        let track_id = song.alloc_track_id();
        let mut track = Track {
            id: track_id,
            name: "Img".into(),
            ..Track::default()
        };
        let cl = track.alloc_clip_id();
        track.clips.push(Clip {
            id: cl,
            name: "img".into(),
            start_beat: 0.0,
            length_beats: event_length,
            content_id: cid,
            notes: Vec::new(),
            color: None,
            auto_lipsync: false,
        });
        song.tracks.push(track);
        song
    }

    #[test]
    fn active_image_returns_single_layer_inside_event() {
        let song = make_song_with_one_image(0.1, 0.2, 0.5, 0.5, 1.0, 8.0, 0.0, 0.0);
        let frames = active_image_sources_at(&song, 4.0);
        assert_eq!(frames.len(), 1);
        let f = frames[0];
        assert_eq!(f.x, 0.1);
        assert_eq!(f.y, 0.2);
        assert_eq!(f.w, 0.5);
        assert_eq!(f.h, 0.5);
        assert!((f.alpha - 1.0).abs() < 1e-6);
        assert_eq!(f.z_index, 0);
    }

    #[test]
    fn active_image_returns_empty_outside_event() {
        let song = make_song_with_one_image(0.0, 0.0, 1.0, 1.0, 1.0, 8.0, 0.0, 0.0);
        let frames = active_image_sources_at(&song, 16.0);
        assert!(frames.is_empty());
    }

    #[test]
    fn active_image_applies_opacity_multiplier() {
        let song = make_song_with_one_image(0.0, 0.0, 1.0, 1.0, 0.5, 8.0, 0.0, 0.0);
        let frames = active_image_sources_at(&song, 4.0);
        assert_eq!(frames.len(), 1);
        assert!((frames[0].alpha - 0.5).abs() < 1e-6);
    }

    #[test]
    fn active_image_applies_linear_fade_in() {
        // 4-beat fade-in, query at half-way → alpha = opacity * 0.5
        let song = make_song_with_one_image(0.0, 0.0, 1.0, 1.0, 1.0, 8.0, 4.0, 0.0);
        let frames = active_image_sources_at(&song, 2.0);
        assert_eq!(frames.len(), 1);
        assert!(
            (frames[0].alpha - 0.5).abs() < 1e-6,
            "expected ~0.5, got {}",
            frames[0].alpha
        );
    }

    #[test]
    fn active_image_drops_muted_events() {
        let mut song = make_song_with_one_image(0.0, 0.0, 1.0, 1.0, 1.0, 8.0, 0.0, 0.0);
        // Flip the only event's `muted` flag.
        for c in song.clip_contents.values_mut() {
            if let Some(events) = c.image_events_mut() {
                events[0].muted = true;
            }
        }
        let frames = active_image_sources_at(&song, 4.0);
        assert!(frames.is_empty());
    }
}
