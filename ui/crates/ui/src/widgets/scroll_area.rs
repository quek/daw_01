// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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

use daw_ui_renderer::{Rect, RectCommand};

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
    /// `id` で識別される scroll_area の現在の offset を取得。
    /// 同じ `id` で `scroll_area` がまだ呼ばれていなければ `(0.0, 0.0)`。
    pub fn scroll_offset(&mut self, id: impl Hash) -> (f32, f32) {
        let wid = WidgetId::ROOT.child((b"scroll_area", &id));
        let state: &mut ScrollState = self.widget_state(wid);
        state.offset
    }

    /// `id` で識別される scroll_area の offset を外から書き換える。
    /// Ctrl+Wheel zoom などで anchor 維持式に offset を更新するときに使う。
    /// 値は scroll_area 側でフレーム時に `[0, max]` にクランプされる。
    pub fn set_scroll_offset(&mut self, id: impl Hash, offset: (f32, f32)) {
        let wid = WidgetId::ROOT.child((b"scroll_area", &id));
        let state: &mut ScrollState = self.widget_state(wid);
        state.offset = offset;
    }

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
        // 縦横どちらもあふれていない (= スクロールしようがない) scroll_area は wheel を
        // **消費しない**。 消費すると、 内側にネストした「今はスクロール不要な」 領域が
        // 親のホイール操作 (例: mixer strip 列の横スクロール) を無言で殺してしまう。
        let scroll = if need_v || need_h {
            self.take_scroll_in_rect(rect)
        } else {
            (0.0, 0.0)
        };

        // ---- 2. scrollbar drag 処理 + offset 更新 ----
        // M9 Phase 45g (daw_01 conversation #010): drag 判定用 thumb_rect は **wheel 適用後の
        // 現在 offset** で計算する。旧実装は `offset = 0.0` で計算していたため、scrolled 状態
        // で thumb の描画位置と hit-test 位置が乖離して drag が始まらない bug があった。
        let v_track_rect = vertical_scrollbar_rect(rect, need_h);
        let h_track_rect = horizontal_scrollbar_rect(rect, need_v);

        let scrolled = scroll.0.abs() > 1e-4 || scroll.1.abs() > 1e-4;
        let (offset, v_thumb_rect, h_thumb_rect) = {
            let state: &mut ScrollState = self.widget_state(wid);
            // wheel 適用 (winit 慣行: y > 0 = wheel up = view 上方向)。offset.y -= scroll.y
            // で「wheel down → offset 増 → 下のコンテンツが見える」になる。
            // M14 Phase 115 (daw_01 #089): 横だけあふれる領域 (need_h && !need_v、 例: mixer の
            // track strip 列) では plain 縦ホイール (scroll.1) を横 offset に回す (= 横一列レイアウト
            // で縦ホイール横スクロールの DAW / browser 慣習)。横ホイール (scroll.0 = Shift+wheel /
            // トラックパッド水平) は常に横 offset。縦あふれがあれば従来どおり縦ホイール → 縦。
            // 符号は既存の `offset -= scroll` を共有するので wheel down → 右スクロールで一貫する。
            let v_wheel_to_h = need_h && !need_v;
            let h_scroll = scroll.0 + if v_wheel_to_h { scroll.1 } else { 0.0 };
            let v_scroll = if v_wheel_to_h { 0.0 } else { scroll.1 };
            state.offset.0 = (state.offset.0 - h_scroll).clamp(0.0, max_x);
            state.offset.1 = (state.offset.1 - v_scroll).clamp(0.0, max_y);

            // wheel 適用後の現在 offset で thumb_rect を計算 (drag hit-test + 描画で共有)。
            let v_thumb_rect = if need_v {
                Some(thumb_rect_vertical(
                    v_track_rect,
                    content_size.1,
                    rect.h,
                    state.offset.1,
                    max_y,
                ))
            } else {
                None
            };
            let h_thumb_rect = if need_h {
                Some(thumb_rect_horizontal(
                    h_track_rect,
                    content_size.0,
                    rect.w,
                    state.offset.0,
                    max_x,
                ))
            } else {
                None
            };

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

            (state.offset, v_thumb_rect, h_thumb_rect)
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

        // ---- 4. scrollbar 描画 (track + thumb)。drag hit-test と同一 thumb rect を使う ----
        // 溝と thumb は専用トークン (grid_line からの alpha 派生ではない): ライトテーマでは
        // 溝を「薄い暗」、thumb を「濃い暗」にする必要があり、alpha だけでは表現できない。
        let p = self.palette();
        if let Some(thumb) = v_thumb_rect {
            self.push_rect(RectCommand::uniform_radius(v_track_rect, p.scrollbar_track, 2.0));
            let hovered = pointer.pos.is_some_and(|(px, py)| thumb.contains(px, py));
            self.push_rect(RectCommand::uniform_radius(
                thumb,
                if hovered { p.scrollbar_thumb_hover } else { p.scrollbar_thumb },
                3.0,
            ));
        }
        if let Some(thumb) = h_thumb_rect {
            self.push_rect(RectCommand::uniform_radius(h_track_rect, p.scrollbar_track, 2.0));
            let hovered = pointer.pos.is_some_and(|(px, py)| thumb.contains(px, py));
            self.push_rect(RectCommand::uniform_radius(
                thumb,
                if hovered { p.scrollbar_thumb_hover } else { p.scrollbar_thumb },
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

    /// M9 Phase 45g (daw_01 conversation #010): scrolled 状態で thumb 位置を click → drag 開始
    /// regression。旧実装は drag 判定用 thumb_rect が `offset = 0.0` で計算されたため、scrolled
    /// 状態 (thumb が track 上端から離れた位置) を click しても hit-test が空振りして drag が始まら
    /// なかった。修正後は **wheel 適用後の現在 `state.offset`** で thumb_rect を計算するので、
    /// scrolled thumb 位置で click → drag が始まり、その後 pointer move で offset が動く。
    #[test]
    fn drag_starts_at_scrolled_thumb_position() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 400, height: 400 };

        // viewport 200、content 600 → max_y = 400、thumb_len = 200*200/600 ≈ 66.7 (THUMB_MIN_LEN
        // 24 を超えるので clamp なし)。
        let area = Rect { x: 0.0, y: 0.0, w: 100.0, h: 200.0 };
        let content = (100.0_f32, 600.0_f32);
        let max_y = (content.1 - area.h).max(0.0); // = 400

        // 1) wheel で 200 px 下へスクロール (offset_y = 200)。
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((50.0, 50.0)),
                scroll_delta: (0.0, -200.0),
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let _ = host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
            ui.scroll_area("test", area, content, |_, _| {});
        });

        // 2) 次フレーム: thumb 描画位置 (現在 offset 反映) を計算。frac = 0.5 で thumb は track の
        // 中央付近 (y > 1.0) に来る。
        let track = vertical_scrollbar_rect(area, false);
        let thumb_at_scrolled =
            thumb_rect_vertical(track, content.1, area.h, 200.0, max_y);
        assert!(
            thumb_at_scrolled.y > 1.0,
            "scrolled thumb は track 上端から離れる (旧 bug の root cause): y={}",
            thumb_at_scrolled.y
        );

        // 3) scrolled thumb 位置を press。
        let press_x = thumb_at_scrolled.x + 1.0;
        let press_y = thumb_at_scrolled.y + 5.0;
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((press_x, press_y)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let _ = host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
            ui.scroll_area("test", area, content, |_, _| {});
        });

        // 4) 30px 下に move + release。drag が成立していれば offset.1 が 200 から増える。
        let observed = std::cell::Cell::new((0.0_f32, 0.0_f32));
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((press_x, press_y + 30.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let _ = host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
            let off = ui.scroll_area("test", area, content, |_, _| {});
            observed.set(off);
        });
        let off = observed.get();
        // drag 成立 → offset.1 > 200 (下方向 drag で増)。旧 bug なら offset.1 == 200 のまま。
        assert!(
            off.1 > 200.0,
            "scrolled thumb 位置からの下方向 drag で offset.1 が 200 から進む (got {})",
            off.1
        );
    }

    /// 1 フレーム分の wheel を scroll_area に与えて返り値 offset を観測するヘルパ。
    /// `scroll_delta` も返り値 `offset` も単位は **px** (入力層が LINE_HEIGHT_PX で px 化済の前提)。
    fn wheel_once(area: Rect, content: (f32, f32), scroll_delta: (f32, f32)) -> (f32, f32) {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 400, height: 400 };
        let observed = std::cell::Cell::new((0.0_f32, 0.0_f32));
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((area.x + area.w * 0.5, area.y + area.h * 0.5)),
                scroll_delta,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let _ = host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
            let off = ui.scroll_area("test", area, content, |_, _| {});
            observed.set(off);
        });
        observed.get()
    }

    /// M14 Phase 115 (daw_01 #089): 横だけあふれる領域 (need_h && !need_v) では plain 縦ホイール
    /// (scroll.1) が横 offset を動かす。wheel down (scroll.1 < 0) → 右スクロール (offset.0 増)。
    #[test]
    fn vertical_wheel_scrolls_horizontally_when_only_horizontal_overflow() {
        // area 100x200、content (600, 200) → max_x=500 (need_h)、max_y=0 (!need_v)。
        let area = Rect { x: 0.0, y: 0.0, w: 100.0, h: 200.0 };
        let content = (600.0_f32, 200.0_f32);
        // wheel down: scroll_delta.1 = -200 → offset.0 = 0 - (-200) = 200。offset.1 は max_y=0 で 0 のまま。
        let off = wheel_once(area, content, (0.0, -200.0));
        assert!(
            (off.0 - 200.0).abs() < 1e-3,
            "縦ホイール (wheel down) が横 offset を 200 まで動かす (got {})",
            off.0
        );
        assert!(off.1.abs() < 1e-3, "縦あふれ無しなので offset.1 は 0 のまま (got {})", off.1);
    }

    /// M14 Phase 115: 縦あふれがある領域では plain 縦ホイールは従来どおり縦 offset を動かす
    /// (横 offset には回さない = 回帰防止)。
    #[test]
    fn vertical_wheel_stays_vertical_when_vertical_overflow_present() {
        // area 100x200、content (600, 600) → max_x=500 (need_h) かつ max_y=400 (need_v)。
        let area = Rect { x: 0.0, y: 0.0, w: 100.0, h: 200.0 };
        let content = (600.0_f32, 600.0_f32);
        let off = wheel_once(area, content, (0.0, -200.0));
        assert!(
            (off.1 - 200.0).abs() < 1e-3,
            "縦あふれありなので縦ホイールは縦 offset を動かす (got {})",
            off.1
        );
        assert!(off.0.abs() < 1e-3, "横 offset には回さない (got {})", off.0);
    }

    /// M14 Phase 115: 横ホイール (scroll.0 = Shift+wheel / トラックパッド水平) は overflow 構成に
    /// 関係なく常に横 offset を動かす (#089 の規則 2、従来挙動の維持)。
    #[test]
    fn horizontal_wheel_always_scrolls_horizontally() {
        let area = Rect { x: 0.0, y: 0.0, w: 100.0, h: 200.0 };
        let content = (600.0_f32, 200.0_f32);
        let off = wheel_once(area, content, (-200.0, 0.0));
        assert!(
            (off.0 - 200.0).abs() < 1e-3,
            "横ホイールが横 offset を 200 まで動かす (got {})",
            off.0
        );
    }
}
