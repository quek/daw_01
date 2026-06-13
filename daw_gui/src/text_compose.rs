//! Text overlay composite helpers (`docs/plan_text_overlay.md` §4 P3).
//!
//! Mirrors `image_compose` for `ClipContent::Text`. Walks the song
//! bottom-up, finds every `TextEvent` active at the playhead beat,
//! resolves the 23 `TextBuiltinParam` track-level automation lanes
//! against the event's defaults, and emits one `ActiveTextFrame` per
//! visible text.
//!
//! Caller (preview window + render_video) takes the `ActiveTextFrame`
//! list and pushes one `daw_ui_renderer::GlyphArea` per entry; gui_01
//! handles the offscreen-composite + outline / shadow / rotation /
//! blur passes (M14 Phase 78). daw_01 stays out of glyphon and just
//! supplies the per-frame description.

use std::collections::HashMap;
use std::sync::Arc;

use common::model::{
    AutomationLane, AutomationTarget, FadeCurve, Song, TextAlign, TextBuiltinParam, TextEvent,
    Track,
};

/// One text event active at the current playhead. Shares the
/// `z_index` numbering with `ActiveVideoFrame` / `ActiveImageFrame`
/// so caller can interleave all three by ascending `z_index` and
/// gui_01's call-order interleave gives the top track the front.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveTextFrame {
    /// Display string (single line, UTF-8). Already `Arc<str>` so the
    /// caller hands it straight to `GlyphArea.text` (also `Arc<str>`)
    /// with a refcount bump instead of a fresh allocation per frame.
    pub text: Arc<str>,
    /// System font name (`""` = renderer default).
    pub font_family: Arc<str>,
    /// Project-resolution px (= 1920x1080 で 48.0 なら 48 px)。
    /// `FontSize` lane が effective ならその値、 さもなくば event.font_size_px.
    pub font_size_px: f32,
    /// PiP rect (image と同 idiom、 normalized 0..=1)。 caller が
    /// project resolution の letterbox box 内 px に展開。
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Per-event alpha = resolved opacity × fade envelope。
    pub alpha: f32,
    /// rect 中心 (= `(x + w/2, y + h/2)` 単位 norm) を旋回中心とする
    /// 2D 回転 (radians, clockwise positive)。 NaN / ±Infinity は
    /// gui_01 renderer 側で 0.0 に正規化される。
    pub rotation_radians: f32,
    /// Horizontal alignment (vertical は単一行 text で center 固定)。
    pub align: TextAlign,
    /// Fill color RGBA (0..=1)。
    pub fill_color: [f32; 4],
    /// Outline color RGBA + width (px)。 `outline_width_px == 0.0` で無効。
    pub outline_color: [f32; 4],
    pub outline_width_px: f32,
    /// Drop shadow color RGBA + offset (px) + blur (px)。
    /// `shadow_color[3] == 0.0` で無効。
    pub shadow_color: [f32; 4],
    pub shadow_offset_px: (f32, f32),
    pub shadow_blur_px: f32,
    /// Bottom-up draw order。 caller がこれで video / image / text を
    /// interleave して higher track を front に置く。
    pub z_index: u32,
}

/// Walk the song bottom-up and return one `ActiveTextFrame` per text
/// event that is active at `playhead_beat`. Muted events, events with
/// alpha == 0 (= opacity × fade resolves to 0), and events on muted
/// tracks are dropped. Mirrors `image_compose::active_image_sources_at`.
pub fn active_text_sources_at(
    song: &Song,
    playhead_beat: f64,
    mod_scalars: &[f32],
) -> Vec<ActiveTextFrame> {
    let bpm = song.bpm as f64;
    if bpm <= 0.0 {
        return Vec::new();
    }
    let mut out: Vec<ActiveTextFrame> = Vec::new();
    let mut z_index: u32 = 0;
    for track in song.tracks.iter().rev() {
        // 自身の mute だけでなく、 グループ親の mute / solo (audio と同じ
        // effective-mute) で silenced な track は text overlay も無効化する
        // (`Song::track_visually_silenced` が SSoT、 preview / render 共通)。
        if song.track_visually_silenced(track.id) {
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
            let Some(events) = content.text_events() else {
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
                let resolved = resolve_text_fields(track, song, event, playhead_beat, mod_scalars);
                let alpha = resolved.opacity * env;
                if alpha <= 0.0 {
                    continue;
                }
                out.push(ActiveTextFrame {
                    text: Arc::from(event.text.as_str()),
                    font_family: Arc::from(event.font_family.as_str()),
                    font_size_px: resolved.font_size_px,
                    x: resolved.x,
                    y: resolved.y,
                    w: resolved.w,
                    h: resolved.h,
                    alpha,
                    rotation_radians: resolved.rotation_radians,
                    align: event.align,
                    fill_color: resolved.fill_color,
                    outline_color: resolved.outline_color,
                    outline_width_px: resolved.outline_width_px,
                    shadow_color: resolved.shadow_color,
                    shadow_offset_px: resolved.shadow_offset_px,
                    shadow_blur_px: resolved.shadow_blur_px,
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

/// FIXME #28: `common::model::TextAlign` を gui_01 renderer の `HAlign` へ写す
/// **唯一の変換点**。 preview (`preview_window`) と export (`render_video`) の
/// 双方がこれを使う。 揃え計算は box (`GlyphArea.box_width` + `align_h`) を渡して
/// レンダラが実 glyph 幅で行うので、 daw_01 側に文字幅推定は持たない (旧
/// `char_count * 0.55` 推定を撤去、 CJK ずれ + preview/export 二重手書きの解消)。
pub fn halign_for(align: TextAlign) -> daw_ui_renderer::HAlign {
    match align {
        TextAlign::Left => daw_ui_renderer::HAlign::Left,
        TextAlign::Center => daw_ui_renderer::HAlign::Center,
        TextAlign::Right => daw_ui_renderer::HAlign::Right,
    }
}

/// All 14 lane-overridable fields resolved against `track`'s
/// `TextBuiltinParam` lanes. `opacity` is returned separately because
/// the caller multiplies it into the fade envelope before storing the
/// composite alpha.
struct ResolvedText {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    opacity: f32,
    rotation_radians: f32,
    font_size_px: f32,
    fill_color: [f32; 4],
    outline_color: [f32; 4],
    outline_width_px: f32,
    shadow_color: [f32; 4],
    shadow_offset_px: (f32, f32),
    shadow_blur_px: f32,
}

/// Resolve each lane-overridable `TextEvent` field against the
/// `TextBuiltinParam` lane (if any). `_norm` paths clamp to `0..=1`
/// (= color channels, normalized PiP rect, opacity); `_plain` paths
/// pass through (= px sizes, blur, offset, radians).
fn resolve_text_fields(
    track: &Track,
    song: &Song,
    event: &TextEvent,
    song_beat: f64,
    mod_scalars: &[f32],
) -> ResolvedText {
    // Index the track's `TextBuiltin` lanes once (single pass) so the 23
    // field resolutions below are O(1) hashmap lookups instead of 23
    // linear scans over `automation_lanes`.
    let mut lane_index: HashMap<TextBuiltinParam, &AutomationLane> = HashMap::new();
    for lane in &track.automation_lanes {
        if let AutomationTarget::TextBuiltin(p) = lane.target {
            lane_index.insert(p, lane);
        }
    }
    // docs/plan_modulation_routing_redesign.md §3.1: base = lane があれば lane 値、
    // 無ければ event の field 値。そこに `Track.mod_routings` の当該 `TextBuiltin`
    // 変調を正規化領域で乗せる (lane 無しでも変調)。lane も routing も無ければ
    // apply_modulation は base を返すので従来挙動 (= 無回帰)。
    let base_for = |field: TextBuiltinParam, fallback: f32| -> f64 {
        match lane_index.get(&field) {
            Some(l) => common::automation::lane_value_at(l, &song.clip_contents, song_beat),
            None => f64::from(fallback),
        }
    };
    let resolve_norm = |field: TextBuiltinParam, fallback: f32| -> f32 {
        let target = AutomationTarget::TextBuiltin(field);
        (common::automation::apply_modulation_with_scalars(
            song,
            &target,
            base_for(field, fallback),
            &track.mod_routings,
            mod_scalars,
        ) as f32)
            .clamp(0.0, 1.0)
    };
    let resolve_plain = |field: TextBuiltinParam, fallback: f32| -> f32 {
        let target = AutomationTarget::TextBuiltin(field);
        common::automation::apply_modulation_with_scalars(
            song,
            &target,
            base_for(field, fallback),
            &track.mod_routings,
            mod_scalars,
        ) as f32
    };
    ResolvedText {
        x: resolve_norm(TextBuiltinParam::X, event.x),
        y: resolve_norm(TextBuiltinParam::Y, event.y),
        w: resolve_norm(TextBuiltinParam::W, event.w),
        h: resolve_norm(TextBuiltinParam::H, event.h),
        opacity: resolve_norm(TextBuiltinParam::Opacity, event.opacity),
        rotation_radians: resolve_plain(
            TextBuiltinParam::Rotation,
            event.rotation_radians,
        ),
        font_size_px: resolve_plain(TextBuiltinParam::FontSize, event.font_size_px),
        fill_color: [
            resolve_norm(TextBuiltinParam::FillR, event.fill_color[0]),
            resolve_norm(TextBuiltinParam::FillG, event.fill_color[1]),
            resolve_norm(TextBuiltinParam::FillB, event.fill_color[2]),
            resolve_norm(TextBuiltinParam::FillA, event.fill_color[3]),
        ],
        outline_color: [
            resolve_norm(TextBuiltinParam::OutlineR, event.outline_color[0]),
            resolve_norm(TextBuiltinParam::OutlineG, event.outline_color[1]),
            resolve_norm(TextBuiltinParam::OutlineB, event.outline_color[2]),
            resolve_norm(TextBuiltinParam::OutlineA, event.outline_color[3]),
        ],
        outline_width_px: resolve_plain(
            TextBuiltinParam::OutlineWidth,
            event.outline_width_px,
        ),
        shadow_color: [
            resolve_norm(TextBuiltinParam::ShadowR, event.shadow_color[0]),
            resolve_norm(TextBuiltinParam::ShadowG, event.shadow_color[1]),
            resolve_norm(TextBuiltinParam::ShadowB, event.shadow_color[2]),
            resolve_norm(TextBuiltinParam::ShadowA, event.shadow_color[3]),
        ],
        shadow_offset_px: (
            resolve_plain(
                TextBuiltinParam::ShadowOffsetX,
                event.shadow_offset_px.0,
            ),
            resolve_plain(
                TextBuiltinParam::ShadowOffsetY,
                event.shadow_offset_px.1,
            ),
        ),
        shadow_blur_px: resolve_plain(TextBuiltinParam::ShadowBlur, event.shadow_blur_px),
    }
}

/// Per-event fade envelope at the given clip-local beat. Range
/// `0.0..=1.0`. Mirrors `image_compose::event_alpha_envelope` exactly
/// so a text fade crossfading with an image fade stays in step.
fn event_alpha_envelope(event: &TextEvent, clip_local_beat: f64) -> f32 {
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
    use common::model::{Clip, ClipContent, TextContent, TextEvent};

    #[allow(clippy::too_many_arguments)]
    fn make_song_with_one_text(
        text: &str,
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
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Text(TextContent {
                events: vec![TextEvent {
                    text: text.into(),
                    x,
                    y,
                    w,
                    h,
                    opacity,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: event_length,
                    fade_in_beats: fade_in,
                    fade_out_beats: fade_out,
                    fade_in_curve: FadeCurve::Linear,
                    fade_out_curve: FadeCurve::Linear,
                    ..TextEvent::default()
                }],
            }),
        );
        let track_id = song.alloc_track_id();
        let mut track = crate::app::track_with(|t| {
            t.id = track_id;
            t.name = "T".into();
        });
        let cl = track.alloc_clip_id();
        track.clips.push(Clip {
            id: cl,
            name: "txt".into(),
            start_beat: 0.0,
            length_beats: event_length,
            content_id: cid,
            notes: Vec::new(),
            color: None,
            auto_lipsync: false,
            ..Default::default()
        });
        song.tracks.push(track);
        song
    }

    #[test]
    fn active_text_returns_single_layer_inside_event() {
        let song = make_song_with_one_text("Hello", 0.1, 0.4, 0.8, 0.2, 1.0, 8.0, 0.0, 0.0);
        let frames = active_text_sources_at(&song, 4.0, &[]);
        assert_eq!(frames.len(), 1);
        let f = &frames[0];
        assert_eq!(&*f.text, "Hello");
        assert_eq!(f.x, 0.1);
        assert_eq!(f.y, 0.4);
        assert_eq!(f.w, 0.8);
        assert_eq!(f.h, 0.2);
        assert!((f.alpha - 1.0).abs() < 1e-6);
        assert_eq!(f.z_index, 0);
    }

    #[test]
    fn active_text_returns_empty_outside_event() {
        let song = make_song_with_one_text("Hi", 0.0, 0.0, 1.0, 1.0, 1.0, 8.0, 0.0, 0.0);
        let frames = active_text_sources_at(&song, 16.0, &[]);
        assert!(frames.is_empty());
    }

    #[test]
    fn active_text_applies_opacity_multiplier() {
        let song = make_song_with_one_text("Hi", 0.0, 0.0, 1.0, 1.0, 0.5, 8.0, 0.0, 0.0);
        let frames = active_text_sources_at(&song, 4.0, &[]);
        assert_eq!(frames.len(), 1);
        assert!((frames[0].alpha - 0.5).abs() < 1e-6);
    }

    #[test]
    fn active_text_applies_linear_fade_in() {
        // 4-beat fade-in, query at half-way → alpha ~= opacity * 0.5.
        let song = make_song_with_one_text("Hi", 0.0, 0.0, 1.0, 1.0, 1.0, 8.0, 4.0, 0.0);
        let frames = active_text_sources_at(&song, 2.0, &[]);
        assert_eq!(frames.len(), 1);
        assert!(
            (frames[0].alpha - 0.5).abs() < 1e-6,
            "expected ~0.5, got {}",
            frames[0].alpha
        );
    }

    #[test]
    fn active_text_drops_muted_events() {
        let mut song = make_song_with_one_text("Hi", 0.0, 0.0, 1.0, 1.0, 1.0, 8.0, 0.0, 0.0);
        for c in song.clip_contents.values_mut() {
            if let Some(events) = c.text_events_mut() {
                events[0].muted = true;
            }
        }
        let frames = active_text_sources_at(&song, 4.0, &[]);
        assert!(frames.is_empty());
    }

    #[test]
    fn active_text_inherits_default_styles() {
        // No automation lanes → `ActiveTextFrame` should mirror the
        // event's default colors / sizes verbatim (= no lane override).
        let song = make_song_with_one_text(
            "T",
            0.0,
            0.4,
            1.0,
            0.2,
            1.0,
            8.0,
            0.0,
            0.0,
        );
        let frames = active_text_sources_at(&song, 4.0, &[]);
        let f = &frames[0];
        // TextEvent::default() defaults — see common/src/model.rs.
        assert_eq!(f.font_size_px, 64.0);
        assert_eq!(f.fill_color, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(f.outline_width_px, 0.0);
        assert_eq!(f.shadow_blur_px, 0.0);
        assert_eq!(f.shadow_offset_px, (0.0, 0.0));
        assert_eq!(f.align, TextAlign::Center);
    }
}
