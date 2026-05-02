//! `scroll_area` widget — overflow をクリップし scrollbar + wheel/drag scroll を提供する。
//!
//! M7 Phase 22 (基本 widget 拡張)。`Ui::with_clip_rect` + `Ui::take_scroll_in_rect` の上に組む。
//!
//! # 使い方
//!
//! ```ignore
//! ui.scroll_area("track_list", area, (area.w, total_track_height_px), |ui, offset| {
//!     for (i, track) in tracks.iter().enumerate() {
//!         let y = area.y - offset.1 + (i as f32) * TRACK_H;
//!         ui.button_at(("track", i), &track.name, Rect { x: area.x, y, w: area.w, h: TRACK_H }, ..);
//!     }
//! });
//! ```
//!
//! 内側の widget は `offset` を引いて配置する (`y = area.y - offset.1 + i * TRACK_H`)。
//! library 側は `with_clip_rect` で `area` 外の描画を切り捨てるため、配置式が
//! はみ出しても安全。

use std::hash::Hash;

use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::id::WidgetId;
use crate::ui::Ui;

/// scrollbar の幅 (px)。track と thumb 共通。
const SCROLLBAR_W: f32 = 10.0;
/// thumb の最低長さ (px)。content_size が極端に大きいときも掴める大きさを保つ。
const THUMB_MIN_LEN: f32 = 24.0;

/// scroll_area の永続状態。
#[derive(Debug, Default)]
pub(crate) struct ScrollState {
    pub offset: (f32, f32),
    /// scrollbar drag 中: (押下時の pointer y/x, 押下時の offset y/x, axis)
    drag: Option<DragAnchor>,
}

#[derive(Debug, Clone, Copy)]
struct DragAnchor {
    /// 押下時の pointer 座標 (drag axis に対応する 1 軸のみ意味あり)。
    pointer_axis: f32,
    /// 押下時の offset 値 (同 axis)。
    offset_axis: f32,
    axis: Axis,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Axis {
    Vertical,
    Horizontal,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// scroll_area widget。`rect` 内に `content_size` のコンテンツを表示し、
    /// はみ出し部分を scrollbar で操作可能にする。
    ///
    /// `content_size` は (content_w, content_h)。`rect.w / rect.h` より大きい軸に
    /// scrollbar が出る。
    ///
    /// closure には `(ui, offset)` が渡される。`offset = (offset_x, offset_y)` は
    /// 「コンテンツ左上が viewport 左上から何 px 上 / 左にあるか」(= scroll 量)。
    /// 内側の widget は `area.x - offset.0` / `area.y - offset.1` を起点に配置する。
    ///
    /// 戻り値: 現在の `offset`。利用者が外側で別の widget の位置に同期させる用途で使う。
    #[allow(clippy::too_many_lines)]
    pub fn scroll_area<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        content_size: (f32, f32),
        f: F,
    ) -> (f32, f32)
    where
        F: FnOnce(&mut Ui<'a, M>, (f32, f32)),
    {
        let wid = WidgetId::ROOT.child((b"scroll_area", &id));
        let pointer = self.pointer;
        let max_x = (content_size.0 - rect.w).max(0.0);
        let max_y = (content_size.1 - rect.h).max(0.0);
        let need_v = max_y > 0.0;
        let need_h = max_x > 0.0;

        // ---- 1. wheel scroll を消費 (rect 内の pointer のみ) ----
        let scroll = self.take_scroll_in_rect(rect);

        // ---- 2. scrollbar drag 処理 + offset 更新 ----
        let v_track_rect = vertical_scrollbar_rect(rect, need_h);
        let h_track_rect = horizontal_scrollbar_rect(rect, need_v);
        let v_thumb_rect = if need_v {
            Some(thumb_rect_vertical(v_track_rect, content_size.1, rect.h, 0.0, max_y))
        } else {
            None
        };
        let h_thumb_rect = if need_h {
            Some(thumb_rect_horizontal(h_track_rect, content_size.0, rect.w, 0.0, max_x))
        } else {
            None
        };

        let scrolled = scroll.0.abs() > 1e-4 || scroll.1.abs() > 1e-4;
        let offset = {
            let state: &mut ScrollState = self.widget_state(wid);
            // wheel 適用 (winit 慣行: y > 0 = wheel up = view 上方向)。offset.y -= scroll.y
            // で「wheel down → offset 増 → 下のコンテンツが見える」になる。
            state.offset.0 = (state.offset.0 - scroll.0).clamp(0.0, max_x);
            state.offset.1 = (state.offset.1 - scroll.1).clamp(0.0, max_y);

            // scrollbar drag 開始判定
            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
            {
                if let Some(thumb) = v_thumb_rect
                    && thumb.contains(px, py)
                {
                    state.drag = Some(DragAnchor {
                        pointer_axis: py,
                        offset_axis: state.offset.1,
                        axis: Axis::Vertical,
                    });
                } else if let Some(thumb) = h_thumb_rect
                    && thumb.contains(px, py)
                {
                    state.drag = Some(DragAnchor {
                        pointer_axis: px,
                        offset_axis: state.offset.0,
                        axis: Axis::Horizontal,
                    });
                }
            }

            // drag 中: offset を再計算
            if let Some(anchor) = state.drag
                && let Some((px, py)) = pointer.pos
            {
                match anchor.axis {
                    Axis::Vertical => {
                        let track_h = v_track_rect.h;
                        let thumb_h = thumb_len(content_size.1, rect.h, track_h);
                        let drag_range = (track_h - thumb_h).max(1.0);
                        let dy = py - anchor.pointer_axis;
                        let new_offset =
                            anchor.offset_axis + dy / drag_range * max_y;
                        state.offset.1 = new_offset.clamp(0.0, max_y);
                    }
                    Axis::Horizontal => {
                        let track_w = h_track_rect.w;
                        let thumb_w = thumb_len(content_size.0, rect.w, track_w);
                        let drag_range = (track_w - thumb_w).max(1.0);
                        let dx = px - anchor.pointer_axis;
                        let new_offset =
                            anchor.offset_axis + dx / drag_range * max_x;
                        state.offset.0 = new_offset.clamp(0.0, max_x);
                    }
                }
            }
            if pointer.primary_just_released {
                state.drag = None;
            }

            state.offset
        };

        // wheel scroll / drag 中は次フレーム再描画を要求 (state 変化を視覚反映するため)
        if scrolled {
            self.request_redraw();
        }

        // ---- 3. 内側を with_clip_rect で描画 ----
        // viewport rect は scrollbar 領域を除いた本体エリア (scrollbar との重なり禁止)。
        let viewport = inner_viewport_rect(rect, need_h, need_v);
        self.with_clip_rect(viewport, |ui| {
            f(ui, offset);
        });

        // ---- 4. scrollbar 描画 (track + thumb) ----
        if need_v {
            let track_color = Color::rgba(1.0, 1.0, 1.0, 0.04);
            let thumb_color = Color::rgba(0.7, 0.75, 0.85, 0.55);
            let thumb_hover = Color::rgba(0.85, 0.90, 1.00, 0.80);
            self.push_rect(RectCommand::uniform_radius(v_track_rect, track_color, 2.0));
            let thumb = thumb_rect_vertical(v_track_rect, content_size.1, rect.h, offset.1, max_y);
            let hovered = pointer.pos.is_some_and(|(px, py)| thumb.contains(px, py));
            self.push_rect(RectCommand::uniform_radius(
                thumb,
                if hovered { thumb_hover } else { thumb_color },
                3.0,
            ));
        }
        if need_h {
            let track_color = Color::rgba(1.0, 1.0, 1.0, 0.04);
            let thumb_color = Color::rgba(0.7, 0.75, 0.85, 0.55);
            let thumb_hover = Color::rgba(0.85, 0.90, 1.00, 0.80);
            self.push_rect(RectCommand::uniform_radius(h_track_rect, track_color, 2.0));
            let thumb = thumb_rect_horizontal(h_track_rect, content_size.0, rect.w, offset.0, max_x);
            let hovered = pointer.pos.is_some_and(|(px, py)| thumb.contains(px, py));
            self.push_rect(RectCommand::uniform_radius(
                thumb,
                if hovered { thumb_hover } else { thumb_color },
                3.0,
            ));
        }

        offset
    }
}

/// 縦 scrollbar の track 矩形 (rect の右端、横 scrollbar がある場合は下端を空ける)。
fn vertical_scrollbar_rect(rect: Rect, has_horizontal: bool) -> Rect {
    let h_offset = if has_horizontal { SCROLLBAR_W } else { 0.0 };
    Rect {
        x: rect.x + rect.w - SCROLLBAR_W,
        y: rect.y,
        w: SCROLLBAR_W,
        h: (rect.h - h_offset).max(0.0),
    }
}

/// 横 scrollbar の track 矩形 (rect の下端、縦 scrollbar がある場合は右端を空ける)。
fn horizontal_scrollbar_rect(rect: Rect, has_vertical: bool) -> Rect {
    let v_offset = if has_vertical { SCROLLBAR_W } else { 0.0 };
    Rect {
        x: rect.x,
        y: rect.y + rect.h - SCROLLBAR_W,
        w: (rect.w - v_offset).max(0.0),
        h: SCROLLBAR_W,
    }
}

/// scrollbar を除いた viewport (内側描画領域)。
fn inner_viewport_rect(rect: Rect, has_horizontal: bool, has_vertical: bool) -> Rect {
    let v_offset = if has_vertical { SCROLLBAR_W } else { 0.0 };
    let h_offset = if has_horizontal { SCROLLBAR_W } else { 0.0 };
    Rect {
        x: rect.x,
        y: rect.y,
        w: (rect.w - v_offset).max(0.0),
        h: (rect.h - h_offset).max(0.0),
    }
}

fn thumb_len(content: f32, viewport: f32, track: f32) -> f32 {
    if content <= 0.0 {
        return track;
    }
    ((viewport / content) * track).max(THUMB_MIN_LEN).min(track)
}

fn thumb_rect_vertical(track: Rect, content_h: f32, viewport_h: f32, offset_y: f32, max_y: f32) -> Rect {
    let thumb_h = thumb_len(content_h, viewport_h, track.h);
    let frac = if max_y > 0.0 { offset_y / max_y } else { 0.0 };
    let thumb_y = track.y + (track.h - thumb_h) * frac;
    Rect { x: track.x, y: thumb_y, w: track.w, h: thumb_h }
}

fn thumb_rect_horizontal(track: Rect, content_w: f32, viewport_w: f32, offset_x: f32, max_x: f32) -> Rect {
    let thumb_w = thumb_len(content_w, viewport_w, track.w);
    let frac = if max_x > 0.0 { offset_x / max_x } else { 0.0 };
    let thumb_x = track.x + (track.w - thumb_w) * frac;
    Rect { x: thumb_x, y: track.y, w: thumb_w, h: track.h }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn thumb_len_full_visible_returns_track() {
        // content 100 viewport 100 → thumb = track 全体
        assert_eq!(thumb_len(100.0, 100.0, 200.0), 200.0);
    }

    #[test]
    fn thumb_len_half_visible_returns_half() {
        // content 200 viewport 100 → thumb = track / 2
        assert_eq!(thumb_len(200.0, 100.0, 200.0), 100.0);
    }

    #[test]
    fn thumb_len_min_clamped() {
        // content 10000 viewport 100 → thumb = 200 * 0.01 = 2px、min 24 にクランプ
        assert_eq!(thumb_len(10000.0, 100.0, 200.0), THUMB_MIN_LEN);
    }

    #[test]
    fn thumb_position_at_top() {
        let track = Rect { x: 0.0, y: 0.0, w: 10.0, h: 200.0 };
        let r = thumb_rect_vertical(track, 400.0, 200.0, 0.0, 200.0);
        assert_eq!(r.y, 0.0);
    }

    #[test]
    fn thumb_position_at_bottom() {
        let track = Rect { x: 0.0, y: 0.0, w: 10.0, h: 200.0 };
        // content 400, viewport 200, max_y 200, offset 200 = 一番下
        let r = thumb_rect_vertical(track, 400.0, 200.0, 200.0, 200.0);
        // thumb_h = 100 (track 200 * 200/400)、frac = 1.0、thumb_y = 0 + (200 - 100) * 1 = 100
        assert_eq!(r.y, 100.0);
    }
}
