//! `arrangement` widget — DAW timeline (track header / ruler / lanes / clip drag) を 1 widget で扱う library widget (M9 Phase 45e)。
//!
//! 公開 API は `F:/dev/daw_01/docs/gui_01_conversation.md` の `## #005 [Replied]` を逐語踏襲。
//! 設計は piano_roll と完全平行 (heavy + cached + overlay / commit-by-release / `make_edit` callback)。
//!
//! - **schema**: `ArrangementClip { id, start_beat, len_beats, name, color }` / `ArrangementTrack { id, name, muted, solo, clips }`。
//!   `id` は track / clip 内で安定 (move/resize/track 跨ぎでも不変、index ではない)。
//! - **描画 + drag state machine + hit-test + shortcut + rect select** は widget 内に閉じる。
//!   heavy() ブロック + cached(viewport_key) で背景を粗粒度キャッシュ、selection / drag preview / playhead /
//!   loop band は cached 外で毎フレーム描画。
//! - **Edit 構築は callback**: `make_edit: Fn(ArrangementEditRequest) -> Edit<M>`。
//!   widget 自身は Model 型を知らず no-Clone 不変条件と整合する。
//! - **commit-by-release**: drag 中は library が overlay 描画、release frame で初めて
//!   `MoveClips` / `ResizeClips` / `SetLoopRange` を発行する。drag 中の Mutate Edit は発行しない。
//! - **track header の Rename / Delete** は widget 内蔵せず、`Response.track_header_rects` を返して
//!   app 側で `context_menu_for` 等を重ねて呼ぶ (#005 設計判断)。
//! - **SelectTrack トリガ** は track header 全体 click (button hit zone を除く)。

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::Arc;

use daw_ui_platform::CursorIcon;
use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::ui::Ui;
use crate::widgets::heavy::HeavyCtx;
use crate::widgets::playhead::draw_playhead_line;
use crate::widgets::toggle_button::ToggleButtonStyle;

// ============================================================
// Public types (conversation #005 [Replied] のまま)
// ============================================================

/// clip の identity。track_id + clip_id (どちらも track / track 内 clip で安定)。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ClipKey {
    pub track: u32,
    pub clip: u32,
}

/// 1 つの clip。`Arc<str>` で複数 clip 間の name 共有可能。
#[derive(Clone, Debug)]
pub struct ArrangementClip {
    pub id: u32,
    pub start_beat: f64,
    pub len_beats: f64,
    pub name: Arc<str>,
    pub color: Option<Color>,
}

/// 1 つの track。`clips` は `start_beat` 昇順前提。
#[derive(Clone, Debug)]
pub struct ArrangementTrack {
    pub id: u32,
    pub name: Arc<str>,
    pub muted: bool,
    pub solo: bool,
    pub clips: Vec<ArrangementClip>,
}

/// arrangement の view 状態 (pan / zoom / playhead / loop)。値渡し (Copy)。
#[derive(Clone, Copy, Debug)]
pub struct ArrangementView {
    /// 表示 left の拍 (浮動小数で smooth scroll)。
    pub start_beat: f64,
    /// 表示する拍範囲 (= zoom 倍率の逆数)。
    pub len_beats: f64,
    /// 縦 scroll offset (px、smooth)。`track_top = 0.0` で first track が lanes 上端。
    pub track_top: f32,
    /// 表示可能 row 数 (SetTrackTop の上限計算に user が使う、widget は読み取らず情報のみ)。
    pub tracks_visible: f32,
    /// 1 track row の高さ (px)。
    pub track_row_h: f32,
    /// track header 領域の幅 (px、`0.0` で header 無し)。
    pub header_w: f32,
    /// ruler 領域の高さ (px、`0.0` で ruler 無し)。
    pub ruler_h: f32,
    /// playhead 線を描く拍位置 (`None` で disabled)。
    pub playhead_beat: Option<f64>,
    /// ループ範囲 (`Some((start, end))`)。`start <= end` 前提。
    pub loop_range: Option<(f64, f64)>,
    /// track 構成 / clip 編集で bump する hook (cache busting)。
    /// selection 変化では bump しない (selection は cached 外 overlay)。
    pub data_generation: u64,
}

impl Default for ArrangementView {
    fn default() -> Self {
        Self {
            start_beat: 0.0,
            len_beats: 16.0,
            track_top: 0.0,
            tracks_visible: 8.0,
            track_row_h: 32.0,
            header_w: 160.0,
            ruler_h: 24.0,
            playhead_beat: None,
            loop_range: None,
            data_generation: 0,
        }
    }
}

/// clip drag の種別 (hit-test 結果)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipDragKind {
    /// clip 中央 drag = 平行移動 (start_beat + 任意 track 跨ぎ)。
    Move,
    /// 左端 drag = start_beat / len_beats 両方変化。
    ResizeLeft,
    /// 右端 drag = len_beats のみ変化。
    ResizeRight,
}

/// `MoveClips` の delta 1 件 (track 跨ぎ可)。`from.clip` は track 跨ぎでも不変。
#[derive(Clone, Copy, Debug)]
pub struct MoveClipDelta {
    pub from: ClipKey,
    pub to_track: u32,
    pub prev_start_beat: f64,
    pub next_start_beat: f64,
}

/// `ResizeClips` の delta 1 件。`ResizeLeft` は両方変化、`ResizeRight` は `next_start == prev_start`。
#[derive(Clone, Copy, Debug)]
pub struct ResizeClipDelta {
    pub key: ClipKey,
    pub prev_start: f64,
    pub prev_len: f64,
    pub next_start: f64,
    pub next_len: f64,
}

/// arrangement が user に発行する Edit 要求。1 frame 内で消費される一時 ADT。
#[derive(Debug)]
pub enum ArrangementEditRequest {
    SelectClips { prev: Vec<ClipKey>, next: Vec<ClipKey> },
    SelectTrack { prev: Option<u32>, next: Option<u32> },
    MoveClips(Vec<MoveClipDelta>),
    ResizeClips(Vec<ResizeClipDelta>),
    DeleteClips(Vec<ClipKey>),
    DoubleClickClip(ClipKey),
    DoubleClickEmpty { track: u32, beat: f64 },
    BeginRenameTrack(u32),
    DeleteTrack(u32),
    MoveTrackUp(u32),
    MoveTrackDown(u32),
    ToggleTrackMute(u32),
    ToggleTrackSolo(u32),
    SetLoopRange { start: f64, end: f64 },
    SetZoomX(f32),
    SetScrollX(f64),
    SetTrackTop(f32),
}

/// `Ui::arrangement` の戻り値。
#[derive(Clone, Debug)]
pub struct ArrangementResponse {
    pub hovered_track: Option<u32>,
    pub hovered_clip: Option<ClipKey>,
    pub hovered_zone: Option<ClipDragKind>,
    pub dragging: Option<ClipDragKind>,
    pub rect_select_active: bool,
    pub selection_changed: bool,
    pub clicked_at_track_beat: Option<(u32, f64)>,
    /// 各 track header の rect (app 側で `context_menu_for` / rename overlay を重ねる用)。
    pub track_header_rects: Vec<(u32, Rect)>,
    pub ruler_rect: Rect,
}

impl Default for ArrangementResponse {
    fn default() -> Self {
        Self {
            hovered_track: None,
            hovered_clip: None,
            hovered_zone: None,
            dragging: None,
            rect_select_active: false,
            selection_changed: false,
            clicked_at_track_beat: None,
            track_header_rects: Vec::new(),
            ruler_rect: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
        }
    }
}

/// arrangement の見た目スタイル。`Default` で example 互換の見た目を再現。
#[derive(Clone, Copy, Debug)]
pub struct ArrangementStyle {
    pub bg: Color,
    pub header_bg: Color,
    pub ruler_bg: Color,
    pub bar_line: Color,
    pub beat_line: Color,
    pub bar_line_width_px: f32,
    pub beat_line_width_px: f32,
    pub lane_line: Color,
    pub lane_line_width_px: f32,
    pub clip_default_fill: Color,
    pub clip_border: Color,
    pub clip_border_w: f32,
    pub clip_radius: f32,
    pub clip_selected_fill: Color,
    pub clip_selected_border: Color,
    pub clip_selected_border_w: f32,
    pub clip_text_color: Color,
    pub clip_text_size: f32,
    pub track_selected_bg: Color,
    pub track_text_color: Color,
    pub track_text_size: f32,
    pub mute_hint: Color,
    pub solo_hint: Color,
    pub mute_solo_hint_h: f32,
    pub playhead_color: Color,
    pub playhead_width_px: f32,
    pub loop_band: Color,
    pub loop_handle: Color,
    pub loop_handle_w: f32,
    pub resize_handle_px: f32,
    pub mute_button: ToggleButtonStyle,
    pub solo_button: ToggleButtonStyle,
}

impl Default for ArrangementStyle {
    fn default() -> Self {
        let mute_button = ToggleButtonStyle {
            off_color: Color::rgb(0.18, 0.20, 0.24),
            on_color: Color::rgb(0.55, 0.18, 0.18),
            hint_band: Some(Color::rgb(1.0, 0.30, 0.20)),
            hint_band_h: 3.0,
            border: Color::rgb(0.30, 0.32, 0.36),
            border_width: 1.0,
            radius: 3.0,
            font_size: 11.0,
            text_color: Color::rgb(0.95, 0.95, 0.97),
        };
        let solo_button = ToggleButtonStyle {
            off_color: Color::rgb(0.18, 0.20, 0.24),
            on_color: Color::rgb(0.55, 0.50, 0.18),
            hint_band: Some(Color::rgb(1.0, 0.85, 0.20)),
            hint_band_h: 3.0,
            border: Color::rgb(0.30, 0.32, 0.36),
            border_width: 1.0,
            radius: 3.0,
            font_size: 11.0,
            text_color: Color::rgb(0.95, 0.95, 0.97),
        };
        Self {
            bg: Color::rgb(0.10, 0.11, 0.13),
            header_bg: Color::rgb(0.14, 0.15, 0.18),
            ruler_bg: Color::rgb(0.16, 0.17, 0.20),
            bar_line: Color::rgba(1.0, 1.0, 1.0, 0.30),
            beat_line: Color::rgba(1.0, 1.0, 1.0, 0.10),
            bar_line_width_px: 1.5,
            beat_line_width_px: 1.0,
            lane_line: Color::rgba(0.0, 0.0, 0.0, 0.55),
            lane_line_width_px: 1.0,
            clip_default_fill: Color::rgb(0.18, 0.40, 0.65),
            clip_border: Color::rgb(0.30, 0.55, 0.78),
            clip_border_w: 1.0,
            clip_radius: 3.0,
            clip_selected_fill: Color::rgb(1.0, 0.85, 0.30),
            clip_selected_border: Color::rgb(1.0, 1.0, 1.0),
            clip_selected_border_w: 2.0,
            clip_text_color: Color::rgb(0.95, 0.95, 0.97),
            clip_text_size: 11.0,
            track_selected_bg: Color::rgb(0.20, 0.24, 0.32),
            track_text_color: Color::rgb(0.92, 0.92, 0.94),
            track_text_size: 12.0,
            mute_hint: Color::rgba(1.0, 0.30, 0.20, 0.60),
            solo_hint: Color::rgba(1.0, 0.85, 0.20, 0.60),
            mute_solo_hint_h: 3.0,
            playhead_color: Color::rgb(1.0, 0.25, 0.10),
            playhead_width_px: 2.5,
            loop_band: Color::rgba(0.50, 0.85, 1.0, 0.20),
            loop_handle: Color::rgb(0.50, 0.85, 1.0),
            loop_handle_w: 2.0,
            resize_handle_px: 4.0,
            mute_button,
            solo_button,
        }
    }
}

// ============================================================
// Public pure helpers
// ============================================================

/// (track_index, clip) → screen rect (lanes 範囲、horizontal clip 形状)。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn clip_to_rect(
    track_index: usize,
    clip: &ArrangementClip,
    view: ArrangementView,
    lanes: Rect,
) -> Rect {
    let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
    let x = lanes.x + ((clip.start_beat - view.start_beat) * beat_to_px) as f32;
    let w = ((clip.len_beats * beat_to_px) as f32).max(2.0);
    let row_top = lanes.y - view.track_top + track_index as f32 * view.track_row_h;
    let h = (view.track_row_h - 4.0).max(2.0);
    Rect { x, y: row_top + 2.0, w, h }
}

/// lanes 内 cursor 位置から hit する (ClipKey, ClipDragKind) を返す (後勝ち)。
#[must_use]
pub fn clip_hit(
    tracks: &[ArrangementTrack],
    view: ArrangementView,
    lanes: Rect,
    cx: f32,
    cy: f32,
    resize_handle_px: f32,
) -> Option<(ClipKey, ClipDragKind)> {
    if !lanes.contains(cx, cy) {
        return None;
    }
    let track_idx = track_index_from_y(cy, lanes.y, view.track_top, view.track_row_h)?;
    let track = tracks.get(track_idx)?;
    let mut hit: Option<(ClipKey, ClipDragKind)> = None;
    for clip in &track.clips {
        let r = clip_to_rect(track_idx, clip, view, lanes);
        if !r.contains(cx, cy) {
            continue;
        }
        let edge = resize_handle_px;
        let kind = if r.w > edge * 2.0 && cx - r.x < edge {
            ClipDragKind::ResizeLeft
        } else if r.w > edge * 2.0 && (r.x + r.w) - cx < edge {
            ClipDragKind::ResizeRight
        } else {
            ClipDragKind::Move
        };
        hit = Some((ClipKey { track: track.id, clip: clip.id }, kind));
    }
    hit
}

/// y 座標から track index を計算 (smooth scroll `track_top` を考慮)。範囲外なら None。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn track_index_from_y(
    y: f32,
    lanes_y: f32,
    track_top: f32,
    track_row_h: f32,
) -> Option<usize> {
    if track_row_h <= 0.0 {
        return None;
    }
    let local = y - lanes_y + track_top;
    if local < 0.0 {
        return None;
    }
    Some((local / track_row_h).floor() as usize)
}

/// loop band の hit 種別 (start handle / end handle / 中央 / 範囲外)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopBandHit {
    Start,
    End,
    Middle,
}

#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn loop_band_hit_kind(
    range: (f64, f64),
    view: ArrangementView,
    ruler: Rect,
    px: f32,
    handle_radius_px: f32,
) -> Option<LoopBandHit> {
    if !ruler.contains(px, ruler.y + ruler.h * 0.5) {
        return None;
    }
    let beat_to_px = f64::from(ruler.w) / view.len_beats.max(1e-6);
    let start_x = ruler.x + ((range.0 - view.start_beat) * beat_to_px) as f32;
    let end_x = ruler.x + ((range.1 - view.start_beat) * beat_to_px) as f32;
    let edge = handle_radius_px.max(1.0);
    if (px - start_x).abs() <= edge {
        Some(LoopBandHit::Start)
    } else if (px - end_x).abs() <= edge {
        Some(LoopBandHit::End)
    } else if px > start_x && px < end_x {
        Some(LoopBandHit::Middle)
    } else {
        None
    }
}

#[inline]
fn px_to_beat(px: f32, lanes_x: f32, lanes_w: f32, view: ArrangementView) -> f64 {
    let beat_per_px = view.len_beats / f64::from(lanes_w.max(1.0));
    view.start_beat + f64::from(px - lanes_x) * beat_per_px
}

// ============================================================
// Internal state
// ============================================================

#[derive(Clone, Copy, Debug)]
struct ClipDragAnchor {
    key: ClipKey,
    start_beat: f64,
    len_beats: f64,
    track_index: usize,
}

#[derive(Clone, Debug)]
struct ClipDragSession {
    kind: ClipDragKind,
    anchor_mouse: (f32, f32),
    /// drag 中の各 frame で更新される最終 pointer 位置。release frame の `pointer.pos` が
    /// winit の implementation によっては press 位置のままになる事があるため、release では
    /// `last_mouse` を delta 計算に使う (drag preview と一致する位置で確定する)。
    last_mouse: (f32, f32),
    anchors: Vec<ClipDragAnchor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopDragKind {
    Start,
    End,
    Middle,
    NewRange,
}

#[derive(Clone, Copy, Debug)]
struct LoopDragSession {
    kind: LoopDragKind,
    anchor_loop: (f64, f64),
    anchor_press_beat: f64,
    /// drag 中の最終 mouse x 位置 (release frame の `pointer.pos` に頼らないための保険、
    /// `ClipDragSession.last_mouse` と同じ理由)。
    last_mouse_x: f32,
}

#[derive(Debug, Default)]
pub(crate) struct ArrangementState {
    clip_drag: Option<ClipDragSession>,
    loop_drag: Option<LoopDragSession>,
}

// ============================================================
// Internal drawing helpers
// ============================================================

fn push_filled_rect<M: ?Sized + 'static>(hctx: &mut HeavyCtx<'_, '_, M>, r: Rect, fill: Color) {
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
}

fn draw_ruler_bg<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    ruler: Rect,
    view: ArrangementView,
    style: &ArrangementStyle,
) {
    push_filled_rect(hctx, ruler, style.ruler_bg);
    let view_end = view.start_beat + view.len_beats;
    let beat_to_px = f64::from(ruler.w) / view.len_beats.max(1e-6);
    #[allow(clippy::cast_possible_truncation)]
    let first = view.start_beat.floor() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let last = view_end.ceil() as i32;
    for b in first..=last {
        #[allow(clippy::cast_possible_truncation)]
        let x = ruler.x + ((f64::from(b) - view.start_beat) * beat_to_px) as f32;
        if x < ruler.x - 1.0 || x > ruler.x + ruler.w + 1.0 {
            continue;
        }
        let is_bar = b.rem_euclid(4) == 0;
        let (line_w, color) = if is_bar {
            (style.bar_line_width_px, style.bar_line)
        } else {
            (style.beat_line_width_px, style.beat_line)
        };
        let h_ratio = if is_bar { 1.0 } else { 0.55 };
        push_filled_rect(
            hctx,
            Rect {
                x: x - line_w * 0.5,
                y: ruler.y + ruler.h * (1.0 - h_ratio),
                w: line_w,
                h: ruler.h * h_ratio,
            },
            color,
        );
    }
}

fn draw_lanes_bg<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    lanes: Rect,
    tracks: &[ArrangementTrack],
    view: ArrangementView,
    selected_track: Option<u32>,
    style: &ArrangementStyle,
) {
    push_filled_rect(hctx, lanes, style.bg);

    // 各 track row 背景 (selection ハイライト + mute/solo hint band)
    for (i, t) in tracks.iter().enumerate() {
        let row_y = lanes.y - view.track_top + i as f32 * view.track_row_h;
        let row = Rect { x: lanes.x, y: row_y, w: lanes.w, h: view.track_row_h };
        if row.y + row.h < lanes.y || row.y > lanes.y + lanes.h {
            continue;
        }
        if Some(t.id) == selected_track {
            push_filled_rect(hctx, row, style.track_selected_bg);
        }
        if t.muted {
            push_filled_rect(
                hctx,
                Rect {
                    x: row.x,
                    y: row.y + row.h - style.mute_solo_hint_h,
                    w: row.w,
                    h: style.mute_solo_hint_h,
                },
                style.mute_hint,
            );
        }
        if t.solo {
            push_filled_rect(
                hctx,
                Rect {
                    x: row.x,
                    y: row.y + row.h - style.mute_solo_hint_h * 2.0 - 1.0,
                    w: row.w,
                    h: style.mute_solo_hint_h,
                },
                style.solo_hint,
            );
        }
        // row 下端 separator
        push_filled_rect(
            hctx,
            Rect {
                x: row.x,
                y: row.y + row.h - style.lane_line_width_px,
                w: row.w,
                h: style.lane_line_width_px,
            },
            style.lane_line,
        );
    }

    // bar/beat 縦線 (lanes 全幅)
    let view_end = view.start_beat + view.len_beats;
    let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
    #[allow(clippy::cast_possible_truncation)]
    let first = view.start_beat.floor() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let last = view_end.ceil() as i32;
    for b in first..=last {
        #[allow(clippy::cast_possible_truncation)]
        let x = lanes.x + ((f64::from(b) - view.start_beat) * beat_to_px) as f32;
        if x < lanes.x - 1.0 || x > lanes.x + lanes.w + 1.0 {
            continue;
        }
        let is_bar = b.rem_euclid(4) == 0;
        let (line_w, color) = if is_bar {
            (style.bar_line_width_px, style.bar_line)
        } else {
            (style.beat_line_width_px, style.beat_line)
        };
        push_filled_rect(
            hctx,
            Rect { x: x - line_w * 0.5, y: lanes.y, w: line_w, h: lanes.h },
            color,
        );
    }
}

fn draw_clip<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    clip: &ArrangementClip,
    style: &ArrangementStyle,
    lanes: Rect,
) {
    if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
        return;
    }
    let fill = clip.color.unwrap_or(style.clip_default_fill);
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border: style.clip_border,
        border_width: style.clip_border_w,
        radius: [style.clip_radius; 4],
        clip_rect: Some(lanes),
    });
    if r.w > 24.0 && r.h > style.clip_text_size + 2.0 {
        hctx.push_text(GlyphArea {
            text: clip.name.to_string(),
            left: r.x + 4.0,
            top: r.y + 2.0,
            font_size: style.clip_text_size,
            line_height: style.clip_text_size * 1.2,
            color: style.clip_text_color,
            clip_rect: Some(r),
        });
    }
}

fn draw_clips<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    tracks: &[ArrangementTrack],
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
) {
    let view_end = view.start_beat + view.len_beats;
    for (i, t) in tracks.iter().enumerate() {
        let row_y = lanes.y - view.track_top + i as f32 * view.track_row_h;
        if row_y + view.track_row_h < lanes.y || row_y > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            let end = c.start_beat + c.len_beats;
            if end < view.start_beat || c.start_beat > view_end {
                continue;
            }
            let r = clip_to_rect(i, c, view, lanes);
            draw_clip(hctx, r, c, style, lanes);
        }
    }
}

fn draw_selection_overlay<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    tracks: &[ArrangementTrack],
    selected: &HashSet<ClipKey>,
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
) {
    if selected.is_empty() {
        return;
    }
    for (i, t) in tracks.iter().enumerate() {
        let row_y = lanes.y - view.track_top + i as f32 * view.track_row_h;
        if row_y + view.track_row_h < lanes.y || row_y > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            let key = ClipKey { track: t.id, clip: c.id };
            if !selected.contains(&key) {
                continue;
            }
            let r = clip_to_rect(i, c, view, lanes);
            if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                continue;
            }
            hctx.push_rect(RectCommand {
                rect: r,
                fill: style.clip_selected_fill,
                border: style.clip_selected_border,
                border_width: style.clip_selected_border_w,
                radius: [style.clip_radius; 4],
                clip_rect: Some(lanes),
            });
            if r.w > 24.0 && r.h > style.clip_text_size + 2.0 {
                hctx.push_text(GlyphArea {
                    text: c.name.to_string(),
                    left: r.x + 4.0,
                    top: r.y + 2.0,
                    font_size: style.clip_text_size,
                    line_height: style.clip_text_size * 1.2,
                    color: Color::rgb(0.10, 0.10, 0.15),
                    clip_rect: Some(r),
                });
            }
        }
    }
}

fn drag_preview_geometry(
    anchor: ClipDragAnchor,
    kind: ClipDragKind,
    beat_delta: f64,
    track_delta: i32,
    n_tracks: usize,
) -> (f64, f64, usize) {
    match kind {
        ClipDragKind::Move => {
            let new_start = (anchor.start_beat + beat_delta).max(0.0);
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            let new_idx = (anchor.track_index as i32 + track_delta)
                .clamp(0, (n_tracks.saturating_sub(1)) as i32);
            #[allow(clippy::cast_sign_loss)]
            let new_idx_u = new_idx.max(0) as usize;
            (new_start, anchor.len_beats, new_idx_u)
        }
        ClipDragKind::ResizeRight => (
            anchor.start_beat,
            (anchor.len_beats + beat_delta).max(0.05),
            anchor.track_index,
        ),
        ClipDragKind::ResizeLeft => {
            let max_start = anchor.start_beat + anchor.len_beats - 0.05;
            let new_start = (anchor.start_beat + beat_delta).clamp(0.0, max_start);
            let actual_delta = new_start - anchor.start_beat;
            (new_start, (anchor.len_beats - actual_delta).max(0.05), anchor.track_index)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_drag_preview<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    nd: &ClipDragSession,
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
    n_tracks: usize,
    beat_delta: f64,
    track_delta: i32,
) {
    for a in &nd.anchors {
        let (start, len, new_idx) =
            drag_preview_geometry(*a, nd.kind, beat_delta, track_delta, n_tracks);
        let preview_clip = ArrangementClip {
            id: a.key.clip,
            start_beat: start,
            len_beats: len,
            name: Arc::from(""),
            color: None,
        };
        let r = clip_to_rect(new_idx, &preview_clip, view, lanes);
        if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
            continue;
        }
        hctx.push_rect(RectCommand {
            rect: r,
            fill: style.clip_selected_fill,
            border: style.clip_selected_border,
            border_width: style.clip_selected_border_w,
            radius: [style.clip_radius; 4],
            clip_rect: Some(lanes),
        });
    }
}

fn draw_loop_band<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    range: (f64, f64),
    view: ArrangementView,
    ruler: Rect,
    style: &ArrangementStyle,
) {
    let (lo, hi) = (range.0.min(range.1), range.0.max(range.1));
    let beat_to_px = f64::from(ruler.w) / view.len_beats.max(1e-6);
    #[allow(clippy::cast_possible_truncation)]
    let x0 = ruler.x + ((lo - view.start_beat) * beat_to_px) as f32;
    #[allow(clippy::cast_possible_truncation)]
    let x1 = ruler.x + ((hi - view.start_beat) * beat_to_px) as f32;
    let band_x = x0.max(ruler.x);
    let band_w = (x1.min(ruler.x + ruler.w) - band_x).max(0.0);
    if band_w > 0.0 {
        push_filled_rect(
            hctx,
            Rect { x: band_x, y: ruler.y, w: band_w, h: ruler.h },
            style.loop_band,
        );
    }
    let hw = style.loop_handle_w * 0.5;
    if x0 >= ruler.x - hw && x0 <= ruler.x + ruler.w + hw {
        push_filled_rect(
            hctx,
            Rect { x: x0 - hw, y: ruler.y, w: style.loop_handle_w, h: ruler.h },
            style.loop_handle,
        );
    }
    if x1 >= ruler.x - hw && x1 <= ruler.x + ruler.w + hw {
        push_filled_rect(
            hctx,
            Rect { x: x1 - hw, y: ruler.y, w: style.loop_handle_w, h: ruler.h },
            style.loop_handle,
        );
    }
}

// ============================================================
// Public widget API
// ============================================================

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// arrangement widget (M9 Phase 45e)。
    ///
    /// 詳細は module doc 参照。`tracks` は順序付き配列 (上から下に並ぶ)。
    /// `selected_clips` / `selected_track` は外部 immutable borrow (Model 側 SSoT)。
    /// `make_edit` callback で各 `ArrangementEditRequest` を `Edit<M>` に変換する。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn arrangement<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        tracks: &[ArrangementTrack],
        view: ArrangementView,
        selected_clips: &[ClipKey],
        selected_track: Option<u32>,
        style: &ArrangementStyle,
        make_edit: F,
    ) -> ArrangementResponse
    where
        F: Fn(ArrangementEditRequest) -> Edit<M> + Send + Sync + 'static,
    {
        let wid = WidgetId::ROOT.child((b"arrangement_widget", &id));
        let pointer = self.pointer;

        // ---- rect 分割 ----
        let header_w = view.header_w.max(0.0);
        let ruler_h = view.ruler_h.max(0.0);
        let lanes_h = (rect.h - ruler_h).max(1.0);
        let lanes_w = (rect.w - header_w).max(1.0);
        let header_pane =
            Rect { x: rect.x, y: rect.y + ruler_h, w: header_w, h: lanes_h };
        let ruler =
            Rect { x: rect.x + header_w, y: rect.y, w: lanes_w, h: ruler_h };
        let lanes =
            Rect { x: rect.x + header_w, y: rect.y + ruler_h, w: lanes_w, h: lanes_h };

        // ---- response 初期 ----
        let mut response = ArrangementResponse {
            ruler_rect: ruler,
            ..Default::default()
        };

        // ---- press 振り分け: clip_drag / loop_drag を state に積む ----
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
        {
            let in_lanes = lanes.contains(px, py);
            let in_ruler = ruler.contains(px, py);
            let shift = pointer.modifiers.shift;
            if in_lanes
                && !shift
                && let Some((hit_key, kind)) =
                    clip_hit(tracks, view, lanes, px, py, style.resize_handle_px)
            {
                let drag_keys: Vec<ClipKey> = if selected_clips.contains(&hit_key) {
                    selected_clips.to_vec()
                } else {
                    vec![hit_key]
                };
                let mut anchors: Vec<ClipDragAnchor> = Vec::new();
                for k in &drag_keys {
                    if let Some((t_idx, t)) =
                        tracks.iter().enumerate().find(|(_, t)| t.id == k.track)
                        && let Some(c) = t.clips.iter().find(|c| c.id == k.clip)
                    {
                        anchors.push(ClipDragAnchor {
                            key: *k,
                            start_beat: c.start_beat,
                            len_beats: c.len_beats,
                            track_index: t_idx,
                        });
                    }
                }
                if !anchors.is_empty() {
                    let state: &mut ArrangementState = self.widget_state(wid);
                    state.clip_drag = Some(ClipDragSession {
                        kind,
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        anchors,
                    });
                }
            }
            if in_ruler {
                let press_beat = px_to_beat(px, ruler.x, ruler.w, view);
                let kind = if let Some(range) = view.loop_range {
                    match loop_band_hit_kind(range, view, ruler, px, 4.0) {
                        Some(LoopBandHit::Start) => LoopDragKind::Start,
                        Some(LoopBandHit::End) => LoopDragKind::End,
                        Some(LoopBandHit::Middle) => LoopDragKind::Middle,
                        None => LoopDragKind::NewRange,
                    }
                } else {
                    LoopDragKind::NewRange
                };
                let anchor_loop = view.loop_range.unwrap_or((press_beat, press_beat));
                let state: &mut ArrangementState = self.widget_state(wid);
                state.loop_drag = Some(LoopDragSession {
                    kind,
                    anchor_loop,
                    anchor_press_beat: press_beat,
                    last_mouse_x: px,
                });
            }
        }

        // ---- drag continue / release 検出 ----
        // 1) drag 中なら毎フレーム last_mouse / last_mouse_x を update (release frame の
        //    pointer.pos が winit によっては press 位置のままになる事があるため、widget
        //    state 側で drag 中の最終位置を確実に保持する)。
        if let Some((px, py)) = pointer.pos {
            let state: &mut ArrangementState = self.widget_state(wid);
            if let Some(ref mut nd) = state.clip_drag {
                nd.last_mouse = (px, py);
            }
            if let Some(ref mut ld) = state.loop_drag {
                ld.last_mouse_x = px;
            }
        }
        // 2) drag overlay 計算用に clone を取る (last_mouse を更新した後)。
        let clip_drag_session: Option<ClipDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.clip_drag.clone()
        };
        let clip_drag_release_raw: Option<ClipDragSession> = if pointer.primary_just_released {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.clip_drag.take()
        } else {
            None
        };
        let (clip_drag_release, clip_short_click_pos): (Option<ClipDragSession>, Option<(f32, f32)>) =
            if let Some(nd) = clip_drag_release_raw {
                let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
                let dy = nd.last_mouse.1 - nd.anchor_mouse.1;
                let dist = dx.abs() + dy.abs();
                if dist < 16.0 {
                    (None, Some(nd.last_mouse))
                } else {
                    (Some(nd), None)
                }
            } else {
                (None, None)
            };

        let loop_drag_session: Option<LoopDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.loop_drag
        };
        let loop_drag_release: Option<LoopDragSession> = if pointer.primary_just_released {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.loop_drag.take()
        } else {
            None
        };

        // drag overlay delta (last_mouse ベース、release と一貫)
        let beat_per_px = view.len_beats / f64::from(lanes.w.max(1.0));
        let row_per_px = 1.0_f32 / view.track_row_h.max(1.0);
        let clip_drag_overlay: Option<(ClipDragSession, f64, i32)> = clip_drag_session
            .as_ref()
            .map(|nd| {
                let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
                let dy = nd.last_mouse.1 - nd.anchor_mouse.1;
                let beat_delta = f64::from(dx) * beat_per_px;
                #[allow(clippy::cast_possible_truncation)]
                let track_delta = (dy * row_per_px).round() as i32;
                (nd.clone(), beat_delta, track_delta)
            });

        let loop_drag_preview_range: Option<(f64, f64)> = loop_drag_session.map(|ld| {
            let cur_beat = px_to_beat(ld.last_mouse_x, ruler.x, ruler.w, view);
            match ld.kind {
                LoopDragKind::Start => (cur_beat.min(ld.anchor_loop.1), ld.anchor_loop.1),
                LoopDragKind::End => (ld.anchor_loop.0, cur_beat.max(ld.anchor_loop.0)),
                LoopDragKind::Middle => {
                    let dx_beat = cur_beat - ld.anchor_press_beat;
                    (ld.anchor_loop.0 + dx_beat, ld.anchor_loop.1 + dx_beat)
                }
                LoopDragKind::NewRange => (
                    ld.anchor_press_beat.min(cur_beat),
                    ld.anchor_press_beat.max(cur_beat),
                ),
            }
        });

        // ---- hover 計算 ----
        if let Some((cx, cy)) = pointer.pos
            && lanes.contains(cx, cy)
        {
            response.hovered_track = track_index_from_y(cy, lanes.y, view.track_top, view.track_row_h)
                .and_then(|idx| tracks.get(idx).map(|t| t.id));
            if let Some((hit_key, hit_kind)) =
                clip_hit(tracks, view, lanes, cx, cy, style.resize_handle_px)
            {
                response.hovered_clip = Some(hit_key);
                response.hovered_zone = Some(hit_kind);
            }
        }
        response.dragging = clip_drag_session.as_ref().map(|nd| nd.kind);

        // ---- cursor ----
        // drag 中 / hover 中の clip 上 / それ以外で arrangement 内なら明示的に Default
        // にリセット (`set_cursor` を呼ばないと OS 側に前フレームの形が残る、winit は state-full)。
        if let Some(kind) = response.dragging {
            let cur = match kind {
                ClipDragKind::Move => CursorIcon::Move,
                ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => CursorIcon::EwResize,
            };
            self.set_cursor(cur);
        } else if let Some(zone) = response.hovered_zone {
            let cur = match zone {
                ClipDragKind::Move => CursorIcon::Move,
                ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => CursorIcon::EwResize,
            };
            self.set_cursor(cur);
        } else if let Some((px, py)) = pointer.pos
            && (lanes.contains(px, py) || ruler.contains(px, py) || header_pane.contains(px, py))
        {
            self.set_cursor(CursorIcon::Default);
        }

        // ---- 描画 (heavy + cached + 動的 overlay) ----
        let viewport_key = (
            b"arrangement_widget_v1" as &[u8],
            rect.w.to_bits(),
            rect.h.to_bits(),
            view.start_beat.to_bits(),
            view.len_beats.to_bits(),
            view.track_top.to_bits(),
            view.track_row_h.to_bits(),
            view.tracks_visible.to_bits(),
            view.header_w.to_bits(),
            view.ruler_h.to_bits(),
            view.data_generation,
            u64::from(selected_track.unwrap_or(u32::MAX)),
        );

        let tracks_owned: Vec<ArrangementTrack> = tracks.to_vec();
        let style_copy = *style;
        let view_copy = view;
        let selected_set: HashSet<ClipKey> = selected_clips.iter().copied().collect();
        let drag_overlay_clone = clip_drag_overlay.clone();
        let loop_preview_clone = loop_drag_preview_range;

        self.heavy(("arrangement_inner", &id), move |hctx| {
            // === cached: viewport_key 一致時 skip ===
            hctx.cached(viewport_key, |hctx| {
                push_filled_rect(hctx, header_pane, style_copy.header_bg);
                draw_lanes_bg(hctx, lanes, &tracks_owned, view_copy, selected_track, &style_copy);
                draw_clips(hctx, &tracks_owned, view_copy, lanes, &style_copy);
                draw_ruler_bg(hctx, ruler, view_copy, &style_copy);
            });

            // === cached 外: selection / drag preview / playhead / loop band ===
            draw_selection_overlay(
                hctx,
                &tracks_owned,
                &selected_set,
                view_copy,
                lanes,
                &style_copy,
            );
            if let Some((nd, bd, td)) = drag_overlay_clone {
                draw_drag_preview(
                    hctx,
                    &nd,
                    view_copy,
                    lanes,
                    &style_copy,
                    tracks_owned.len(),
                    bd,
                    td,
                );
            }
            // loop band: drag preview がある場合は preview を描く、無ければ view.loop_range
            if let Some(range) = loop_preview_clone.or(view_copy.loop_range) {
                draw_loop_band(hctx, range, view_copy, ruler, &style_copy);
            }
            if let Some(b) = view_copy.playhead_beat
                && b >= view_copy.start_beat
                && b <= view_copy.start_beat + view_copy.len_beats
            {
                let beat_to_px = f64::from(lanes.w) / view_copy.len_beats.max(1e-6);
                #[allow(clippy::cast_possible_truncation)]
                let x = lanes.x + ((b - view_copy.start_beat) * beat_to_px) as f32;
                draw_playhead_line(
                    hctx,
                    x,
                    ruler.y,
                    lanes.y + lanes.h,
                    style_copy.playhead_color,
                    style_copy.playhead_width_px,
                );
            }
        });

        // ---- shortcut: Delete (selected clips を一括削除) ----
        if self.take_shortcut("delete") && !selected_clips.is_empty() {
            self.push_edit(make_edit(ArrangementEditRequest::DeleteClips(
                selected_clips.to_vec(),
            )));
        }

        // ---- clip drag release → MoveClips / ResizeClips ----
        let clip_drag_release_was_some = clip_drag_release.is_some();
        if let Some(nd) = clip_drag_release {
            let (beat_delta, track_delta): (f64, i32) = {
                let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
                let dy = nd.last_mouse.1 - nd.anchor_mouse.1;
                #[allow(clippy::cast_possible_truncation)]
                let td = (dy * row_per_px).round() as i32;
                (f64::from(dx) * beat_per_px, td)
            };
            match nd.kind {
                ClipDragKind::Move => {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    let max_idx_i32 = (tracks.len().saturating_sub(1)) as i32;
                    let mut deltas: Vec<MoveClipDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_start = (a.start_beat + beat_delta).max(0.0);
                        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                        let press_i32 = a.track_index as i32;
                        let new_idx = (press_i32 + track_delta).clamp(0, max_idx_i32);
                        #[allow(clippy::cast_sign_loss)]
                        let new_idx_u = new_idx.max(0) as usize;
                        let new_track_id = tracks
                            .get(new_idx_u)
                            .map_or(a.key.track, |t| t.id);
                        let moved = (new_start - a.start_beat).abs() > 1e-6
                            || new_track_id != a.key.track;
                        if moved {
                            deltas.push(MoveClipDelta {
                                from: a.key,
                                to_track: new_track_id,
                                prev_start_beat: a.start_beat,
                                next_start_beat: new_start,
                            });
                        }
                    }
                    if !deltas.is_empty() {
                        self.push_edit(make_edit(ArrangementEditRequest::MoveClips(deltas)));
                    }
                }
                ClipDragKind::ResizeRight => {
                    let mut deltas: Vec<ResizeClipDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_len = (a.len_beats + beat_delta).max(0.05);
                        if (new_len - a.len_beats).abs() > 1e-6 {
                            deltas.push(ResizeClipDelta {
                                key: a.key,
                                prev_start: a.start_beat,
                                prev_len: a.len_beats,
                                next_start: a.start_beat,
                                next_len: new_len,
                            });
                        }
                    }
                    if !deltas.is_empty() {
                        self.push_edit(make_edit(ArrangementEditRequest::ResizeClips(deltas)));
                    }
                }
                ClipDragKind::ResizeLeft => {
                    let mut deltas: Vec<ResizeClipDelta> = Vec::new();
                    for a in &nd.anchors {
                        let max_start = a.start_beat + a.len_beats - 0.05;
                        let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                        let actual = new_start - a.start_beat;
                        let new_len = (a.len_beats - actual).max(0.05);
                        if (new_start - a.start_beat).abs() > 1e-6
                            || (new_len - a.len_beats).abs() > 1e-6
                        {
                            deltas.push(ResizeClipDelta {
                                key: a.key,
                                prev_start: a.start_beat,
                                prev_len: a.len_beats,
                                next_start: new_start,
                                next_len: new_len,
                            });
                        }
                    }
                    if !deltas.is_empty() {
                        self.push_edit(make_edit(ArrangementEditRequest::ResizeClips(deltas)));
                    }
                }
            }
        }

        // ---- short click on lanes (drag<16px) → SelectClips ----
        if let Some((cx, cy)) = clip_short_click_pos
            && lanes.contains(cx, cy)
        {
            let prev = selected_clips.to_vec();
            let next: Vec<ClipKey> =
                if let Some((hit_key, _)) = clip_hit(tracks, view, lanes, cx, cy, style.resize_handle_px) {
                    vec![hit_key]
                } else {
                    Vec::new()
                };
            if prev != next {
                self.push_edit(make_edit(ArrangementEditRequest::SelectClips { prev, next }));
                response.selection_changed = true;
            }
            if let Some(idx) = track_index_from_y(cy, lanes.y, view.track_top, view.track_row_h)
                && let Some(t) = tracks.get(idx)
            {
                let beat = px_to_beat(cx, lanes.x, lanes.w, view);
                response.clicked_at_track_beat = Some((t.id, beat));
            }
        }

        // ---- pure release on empty lanes (no drag started) → SelectClips clear ----
        // clip_drag_session が無い + 空白 release + Shift なし
        if pointer.primary_just_released
            && clip_short_click_pos.is_none()
            && !clip_drag_release_was_some
            && !pointer.modifiers.shift
            && let Some((cx, cy)) = pointer.pos
            && lanes.contains(cx, cy)
            && clip_hit(tracks, view, lanes, cx, cy, style.resize_handle_px).is_none()
            && !selected_clips.is_empty()
        {
            self.push_edit(make_edit(ArrangementEditRequest::SelectClips {
                prev: selected_clips.to_vec(),
                next: Vec::new(),
            }));
            response.selection_changed = true;
        }

        // ---- loop drag release → SetLoopRange ----
        if let Some(ld) = loop_drag_release {
            let cur_beat = px_to_beat(ld.last_mouse_x, ruler.x, ruler.w, view);
            let (start, end) = match ld.kind {
                LoopDragKind::Start => (cur_beat.min(ld.anchor_loop.1), ld.anchor_loop.1),
                LoopDragKind::End => (ld.anchor_loop.0, cur_beat.max(ld.anchor_loop.0)),
                LoopDragKind::Middle => {
                    let dx = cur_beat - ld.anchor_press_beat;
                    (ld.anchor_loop.0 + dx, ld.anchor_loop.1 + dx)
                }
                LoopDragKind::NewRange => (
                    ld.anchor_press_beat.min(cur_beat),
                    ld.anchor_press_beat.max(cur_beat),
                ),
            };
            self.push_edit(make_edit(ArrangementEditRequest::SetLoopRange { start, end }));
        }

        // ---- Shift+drag rect select (lanes 内で加算) ----
        let drag_rect_wid = wid.child(b"rect_select");
        let shift_rect_active = {
            let state: &mut crate::widgets::drag_rect::DragRectState =
                self.widget_state(drag_rect_wid);
            state.drag_start.is_some()
        };
        let shift_press = pointer.primary_just_pressed && pointer.modifiers.shift;
        if (shift_press || shift_rect_active)
            && let Some(drag) = self.take_drag_rect_in_rect(drag_rect_wid, lanes)
        {
            response.rect_select_active = true;
            if drag.modifiers.shift && drag.finished {
                let drag_rect = drag.rect();
                let mut set: HashSet<ClipKey> = selected_clips.iter().copied().collect();
                for (i, t) in tracks.iter().enumerate() {
                    for c in &t.clips {
                        let r = clip_to_rect(i, c, view, lanes);
                        if rects_intersect(r, drag_rect) {
                            set.insert(ClipKey { track: t.id, clip: c.id });
                        }
                    }
                }
                let mut new_keys: Vec<ClipKey> = set.into_iter().collect();
                new_keys.sort_by_key(|a| (a.track, a.clip));
                let mut prev_sorted: Vec<ClipKey> = selected_clips.to_vec();
                prev_sorted.sort_by_key(|a| (a.track, a.clip));
                if prev_sorted != new_keys {
                    self.push_edit(make_edit(ArrangementEditRequest::SelectClips {
                        prev: selected_clips.to_vec(),
                        next: new_keys,
                    }));
                    response.selection_changed = true;
                }
            }
        }

        // ---- wheel: Ctrl=zoom_x / Shift=scroll_x / plain=track_top ----
        let scroll = self.take_scroll_in_rect(lanes);
        if scroll.1.abs() > 0.0 || scroll.0.abs() > 0.0 {
            let dy = scroll.1;
            if pointer.modifiers.ctrl {
                let factor = (-dy * 0.005).exp();
                self.push_edit(make_edit(ArrangementEditRequest::SetZoomX(factor)));
            } else if pointer.modifiers.shift {
                let delta = -f64::from(dy) * beat_per_px * 4.0;
                self.push_edit(make_edit(ArrangementEditRequest::SetScrollX(
                    view.start_beat + delta,
                )));
            } else {
                let new_top = (view.track_top - dy * 8.0).max(0.0);
                self.push_edit(make_edit(ArrangementEditRequest::SetTrackTop(new_top)));
            }
        }

        // ---- double-click (lanes 内で clip / 空白) ----
        if let Some((cx, cy)) = self.take_double_click_in_rect(lanes) {
            if let Some((hit_key, _)) =
                clip_hit(tracks, view, lanes, cx, cy, style.resize_handle_px)
            {
                self.push_edit(make_edit(ArrangementEditRequest::DoubleClickClip(hit_key)));
            } else if let Some(idx) =
                track_index_from_y(cy, lanes.y, view.track_top, view.track_row_h)
                && let Some(t) = tracks.get(idx)
            {
                let beat = px_to_beat(cx, lanes.x, lanes.w, view);
                self.push_edit(make_edit(ArrangementEditRequest::DoubleClickEmpty {
                    track: t.id,
                    beat,
                }));
            }
        }

        // ---- track headers (button_at × 4 + toggle_button_at × 2) + SelectTrack トリガ ----
        if header_w > 0.0 {
            for (i, t) in tracks.iter().enumerate() {
                let row_y = header_pane.y - view.track_top + i as f32 * view.track_row_h;
                let row =
                    Rect { x: header_pane.x, y: row_y, w: header_pane.w, h: view.track_row_h };
                if row.y + row.h < header_pane.y || row.y > header_pane.y + header_pane.h {
                    continue;
                }

                // 背景 (selection)
                if Some(t.id) == selected_track {
                    self.panel(("arr_thsel", t.id), row, style.track_selected_bg, 0.0);
                } else {
                    self.panel(("arr_thbg", t.id), row, style.header_bg, 0.0);
                }

                let pad = 4.0_f32;
                let inner = Rect {
                    x: row.x + pad,
                    y: row.y + pad,
                    w: (row.w - pad * 2.0).max(2.0),
                    h: (row.h - pad * 2.0).max(2.0),
                };
                let btn_h = inner.h.min(20.0);
                let small = 22.0_f32;
                let m_w = small;
                let s_w = small;
                let up_w = small;
                let dn_w = small;
                let del_w = small;
                let gap = 2.0_f32;
                // 順序: [Name (残り)] [M] [S] [Up] [Dn] [Del]
                let total_right = m_w + s_w + up_w + dn_w + del_w + gap * 5.0;
                let name_w = (inner.w - total_right).max(20.0);
                let name_rect = Rect { x: inner.x, y: inner.y, w: name_w, h: btn_h };
                let mut x_cursor = inner.x + name_w + gap;
                let m_rect = Rect { x: x_cursor, y: inner.y, w: m_w, h: btn_h };
                x_cursor += m_w + gap;
                let s_rect = Rect { x: x_cursor, y: inner.y, w: s_w, h: btn_h };
                x_cursor += s_w + gap;
                let up_rect = Rect { x: x_cursor, y: inner.y, w: up_w, h: btn_h };
                x_cursor += up_w + gap;
                let dn_rect = Rect { x: x_cursor, y: inner.y, w: dn_w, h: btn_h };
                x_cursor += dn_w + gap;
                let del_rect = Rect { x: x_cursor, y: inner.y, w: del_w, h: btn_h };

                let button_zones: [Rect; 6] =
                    [name_rect, m_rect, s_rect, up_rect, dn_rect, del_rect];

                let id_name = ("arr_tname", t.id);
                let id_mute = ("arr_tmute", t.id);
                let id_solo = ("arr_tsolo", t.id);
                let id_up = ("arr_tup", t.id);
                let id_dn = ("arr_tdn", t.id);
                let id_del = ("arr_tdel", t.id);

                let track_id = t.id;
                let muted = t.muted;
                let solo = t.solo;

                // make_edit 経由の Edit を発行する closure を 1 つずつ作成 (make_edit を Arc 化して share)
                let make_edit_arc: Arc<dyn Fn(ArrangementEditRequest) -> Edit<M> + Send + Sync> = {
                    // make_edit は move でこのクロージャに 1 度だけ取り込まれる + 各 button click に share する
                    // ただし make_edit は外側 closure に move されるので、内側で Arc::clone できない。
                    // そこで make_edit を Arc に包むのは一度だけ、widget 全体で 1 回だけ実行できる場所がない。
                    // → 各 button では make_edit を直接呼ぶことができないので、1 つの fn pointer にせず
                    // closure の中で req を作って push_edit する。
                    Arc::new(|_| -> Edit<M> { unreachable!("placeholder") })
                };
                drop(make_edit_arc); // 上のコメント通り、Arc 化は実装上不要 (各 closure で req を make_edit に直接渡す)

                let name_text = t.name.clone();
                // Name button: single click → SelectTrack (header background click と同じ動作)、
                // double-click は別途 take_double_click_in_rect で検出 → BeginRenameTrack 発行。
                let prev_sel = selected_track;
                self.button_at(id_name, &name_text, name_rect, || {
                    make_edit(ArrangementEditRequest::SelectTrack {
                        prev: prev_sel,
                        next: Some(track_id),
                    })
                });
                if self.take_double_click_in_rect(name_rect).is_some() {
                    self.push_edit(make_edit(ArrangementEditRequest::BeginRenameTrack(track_id)));
                }
                self.toggle_button_at(id_mute, "M", m_rect, muted, &style.mute_button, |_| {
                    make_edit(ArrangementEditRequest::ToggleTrackMute(track_id))
                });
                self.toggle_button_at(id_solo, "S", s_rect, solo, &style.solo_button, |_| {
                    make_edit(ArrangementEditRequest::ToggleTrackSolo(track_id))
                });
                self.button_at(id_up, "↑", up_rect, || {
                    make_edit(ArrangementEditRequest::MoveTrackUp(track_id))
                });
                self.button_at(id_dn, "↓", dn_rect, || {
                    make_edit(ArrangementEditRequest::MoveTrackDown(track_id))
                });
                self.button_at(id_del, "×", del_rect, || {
                    make_edit(ArrangementEditRequest::DeleteTrack(track_id))
                });

                // Response.track_header_rects に積む
                response.track_header_rects.push((t.id, row));

                // SelectTrack トリガ: row 内 release + button_zones いずれにも非 hit + dist 短
                if pointer.primary_just_released
                    && let Some((rx, ry)) = pointer.pos
                    && row.contains(rx, ry)
                    && !button_zones.iter().any(|b| b.contains(rx, ry))
                {
                    let prev = selected_track;
                    let next = Some(t.id);
                    if prev != next {
                        self.push_edit(make_edit(ArrangementEditRequest::SelectTrack {
                            prev,
                            next,
                        }));
                        response.selection_changed = true;
                    }
                }
            }
        }

        response
    }
}

#[must_use]
fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(id: u32, start: f64, len: f64, name: &str) -> ArrangementClip {
        ArrangementClip {
            id,
            start_beat: start,
            len_beats: len,
            name: Arc::from(name),
            color: None,
        }
    }

    fn track(id: u32, name: &str, clips: Vec<ArrangementClip>) -> ArrangementTrack {
        ArrangementTrack {
            id,
            name: Arc::from(name),
            muted: false,
            solo: false,
            clips,
        }
    }

    fn test_view() -> ArrangementView {
        ArrangementView {
            start_beat: 0.0,
            len_beats: 16.0,
            track_top: 0.0,
            tracks_visible: 8.0,
            track_row_h: 32.0,
            header_w: 0.0,
            ruler_h: 0.0,
            playhead_beat: None,
            loop_range: None,
            data_generation: 0,
        }
    }

    fn test_lanes() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 640.0, h: 256.0 }
    }

    #[test]
    fn clip_to_rect_basic_position() {
        let view = test_view();
        let lanes = test_lanes();
        let c = clip(0, 4.0, 4.0, "x");
        let r = clip_to_rect(2, &c, view, lanes);
        // beat_to_px = 640/16 = 40
        // x = 0 + 4*40 = 160, w = 4*40 = 160
        // row_top = 0 - 0 + 2*32 = 64, y = 64+2 = 66, h = 32-4 = 28
        assert!((r.x - 160.0).abs() < 1e-3);
        assert!((r.w - 160.0).abs() < 1e-3);
        assert!((r.y - 66.0).abs() < 1e-3);
        assert!((r.h - 28.0).abs() < 1e-3);
    }

    #[test]
    fn track_index_from_y_basic() {
        // lanes_y=10, track_top=0, row_h=32 → y=10 → idx 0, y=42 → idx 1, y=74 → idx 2
        assert_eq!(track_index_from_y(10.0, 10.0, 0.0, 32.0), Some(0));
        assert_eq!(track_index_from_y(42.0, 10.0, 0.0, 32.0), Some(1));
        assert_eq!(track_index_from_y(74.0, 10.0, 0.0, 32.0), Some(2));
        assert_eq!(track_index_from_y(5.0, 10.0, 0.0, 32.0), None);
    }

    #[test]
    fn track_index_from_y_with_scroll() {
        // track_top=16 で 1 row 半分上にスクロール → y=10 + 16 = 26 → idx 0 のまま (>16 で idx 1)
        assert_eq!(track_index_from_y(10.0, 10.0, 16.0, 32.0), Some(0));
        assert_eq!(track_index_from_y(26.0, 10.0, 16.0, 32.0), Some(1));
    }

    #[test]
    fn clip_hit_returns_move_in_center() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        // clip rect at (0, 2, 160, 28), center = (80, 16)
        let hit = clip_hit(&tracks, view, lanes, 80.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::Move))
        );
    }

    #[test]
    fn clip_hit_returns_resize_left_at_left_edge() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let hit = clip_hit(&tracks, view, lanes, 1.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeLeft))
        );
    }

    #[test]
    fn clip_hit_returns_resize_right_at_right_edge() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let hit = clip_hit(&tracks, view, lanes, 159.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeRight))
        );
    }

    #[test]
    fn clip_hit_returns_none_outside_lanes() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let hit = clip_hit(&tracks, view, lanes, -10.0, -10.0, 4.0);
        assert_eq!(hit, None);
    }

    #[test]
    fn loop_band_hit_kind_start_handle() {
        let view = test_view();
        let ruler = Rect { x: 0.0, y: 0.0, w: 640.0, h: 24.0 };
        // beat_to_px = 40, range=(2, 6) → start_x=80, end_x=240
        let hit = loop_band_hit_kind((2.0, 6.0), view, ruler, 80.0, 4.0);
        assert_eq!(hit, Some(LoopBandHit::Start));
    }

    #[test]
    fn loop_band_hit_kind_end_handle() {
        let view = test_view();
        let ruler = Rect { x: 0.0, y: 0.0, w: 640.0, h: 24.0 };
        let hit = loop_band_hit_kind((2.0, 6.0), view, ruler, 240.0, 4.0);
        assert_eq!(hit, Some(LoopBandHit::End));
    }

    #[test]
    fn loop_band_hit_kind_middle() {
        let view = test_view();
        let ruler = Rect { x: 0.0, y: 0.0, w: 640.0, h: 24.0 };
        let hit = loop_band_hit_kind((2.0, 6.0), view, ruler, 160.0, 4.0);
        assert_eq!(hit, Some(LoopBandHit::Middle));
    }

    #[test]
    fn loop_band_hit_kind_outside() {
        let view = test_view();
        let ruler = Rect { x: 0.0, y: 0.0, w: 640.0, h: 24.0 };
        let hit = loop_band_hit_kind((2.0, 6.0), view, ruler, 400.0, 4.0);
        assert_eq!(hit, None);
    }

    #[test]
    fn rects_intersect_basic() {
        let a = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        let b = Rect { x: 5.0, y: 5.0, w: 10.0, h: 10.0 };
        let c = Rect { x: 20.0, y: 0.0, w: 10.0, h: 10.0 };
        assert!(rects_intersect(a, b));
        assert!(!rects_intersect(a, c));
    }

    #[test]
    fn arrangement_view_default_sane() {
        let v = ArrangementView::default();
        assert!(v.len_beats > 0.0);
        assert!(v.track_row_h > 0.0);
        assert!(v.tracks_visible > 0.0);
        assert!(v.header_w > 0.0);
        assert!(v.ruler_h > 0.0);
    }

    #[test]
    fn arrangement_style_default_sane() {
        let s = ArrangementStyle::default();
        assert!(s.resize_handle_px > 0.0);
        assert!(s.playhead_width_px > 0.0);
        assert!(s.mute_solo_hint_h > 0.0);
        assert!(s.clip_radius >= 0.0);
    }

    #[test]
    fn drag_preview_geometry_move_clamps_track() {
        let anchor = ClipDragAnchor {
            key: ClipKey { track: 0, clip: 0 },
            start_beat: 4.0,
            len_beats: 2.0,
            track_index: 0,
        };
        let (s, l, idx) = drag_preview_geometry(anchor, ClipDragKind::Move, 1.5, 5, 3);
        assert!((s - 5.5).abs() < 1e-9);
        assert!((l - 2.0).abs() < 1e-9);
        // 0 + 5 = 5 → clamped to 2 (tracks=3 → max idx = 2)
        assert_eq!(idx, 2);
    }

    #[test]
    fn drag_preview_geometry_resize_left_clamps_min_len() {
        let anchor = ClipDragAnchor {
            key: ClipKey { track: 0, clip: 0 },
            start_beat: 4.0,
            len_beats: 2.0,
            track_index: 1,
        };
        let (s, l, idx) = drag_preview_geometry(anchor, ClipDragKind::ResizeLeft, 10.0, 0, 4);
        // max_start = 4 + 2 - 0.05 = 5.95 → new_start clamped to 5.95
        // actual_delta = 5.95 - 4 = 1.95 → new_len = 2 - 1.95 = 0.05
        assert!((s - 5.95).abs() < 1e-6);
        assert!((l - 0.05).abs() < 1e-6);
        assert_eq!(idx, 1);
    }
}
