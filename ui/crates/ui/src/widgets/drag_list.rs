//! `drag_list` — **可変高・異種行**の縦リストに「行 (とその後続ブロック) を掴んで
//! スロットへ落とす」 drag&drop を付けた widget (daw_01 r.md #110)。
//!
//! [`crate::widgets::reorderable_list`] は「一様行 + permutation」 の並べ替えで、行の種類が
//! 混ざるリスト (見出し行 / 子行 / 操作行) や「ブロックごと運ぶ」 にはならない。ここでは
//! **並べ替えの意味論を widget が持たない**: caller が
//!
//! - 行ごとの高さと「掴めるか」「掴んだとき一緒に運ぶ後続行数」 ([`DragListRow`])、
//! - 落とせる位置の一覧 ([`DragListSlot`] = 「行 `after_row` の直前」 + 表示インデント)、
//! - `valid_drop(掴んだ行, スロット)` の述語
//!
//! を渡し、widget は「どの行を掴んでどのスロットに落としたか」 ([`DragListResponse::dropped`])
//! だけを返す。描画は行の中身もふくめて全部 caller のクロージャ (widget が描くのは
//! drop indicator だけ)。ドメイン知識はゼロ。
//!
//! 外部 drag ([`Ui::begin_drag`] で始めた運搬) の受け入れも同じスロット表で行う
//! (`accept_drag_kind`)。内部 drag で矩形の **横へ** 出たら [`DragListResponse::dragged_out`]
//! で caller に運搬を引き継がせる (reorderable_list と同じ契約)。
//!
//! スクロールは持たない (caller が `scroll_area` の中に置く。`rect.h` = 内容の全高)。

use std::hash::Hash;

use daw_ui_platform::Modifiers;
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::id::WidgetId;
use crate::ui::Ui;

/// drag commit 判定の最小移動量 (px)。これ未満では click 扱いに格下げ
/// (`reorderable_list` と同値)。
const DRAG_THRESHOLD_PX: f32 = 16.0;

/// 掴んだ行をリスト矩形の **横へ** 出したと判定する余白 (px)。理由は
/// `reorderable_list::CARRY_OUT_MARGIN_PX` と同じ (縦は「先頭 / 末尾へ」 の途中経過)。
const CARRY_OUT_MARGIN_PX: f32 = 24.0;

/// 1 行の仕様。
#[derive(Clone, Copy, Debug)]
pub struct DragListRow {
    pub height: f32,
    /// この行を掴めるか (見出し行 / 操作行は `false`)。**click は全行で起きる**
    /// (掴めない行も選べる)。`false` の行は閾値を超えて動かしても drop / 横出しにならない。
    pub draggable: bool,
    /// 掴んだとき一緒に運ぶ **後続行の数を含む** ブロック長 (`>= 1`)。
    /// 例: 「Parallel 開始行」 を掴んだら終了行までの全部。
    pub block_len: usize,
}

/// 落とせる位置。`after_row` = その行の **直前** の隙間 (`rows.len()` = 末尾)。
/// 同じ隙間に複数のスロットがあってよい (深さ違い = ネストの内 / 外)。`indent` は
/// indicator の左端オフセット (px)。
#[derive(Clone, Copy, Debug)]
pub struct DragListSlot {
    pub after_row: usize,
    pub indent: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct DragListStyle {
    pub row_gap: f32,
    pub drop_indicator_color: Color,
    pub drop_indicator_h: f32,
}

#[derive(Clone, Debug, Default)]
pub struct DragListResponse {
    /// drag 距離 < 閾値の release で trigger (行 index)。掴めない行でも起きる。
    pub clicked: Option<usize>,
    /// `clicked` を起こした **press フレーム** の修飾キー (release の生読みは
    /// `ModifiersChanged` 先行 race で落ちる)。
    pub clicked_modifiers: Modifiers,
    /// hover 中の行。
    pub hovered: Option<usize>,
    /// drag 中 (閾値超え) の anchor 行。
    pub dragging: Option<usize>,
    /// 掴んだ行が矩形の横へ出た最初のフレームだけ `Some`。widget の session は破棄済み。
    pub dragged_out: Option<usize>,
    /// このフレームに release で確定した `(掴んだ行, スロット index)`。
    pub dropped: Option<(usize, usize)>,
    /// `accept_drag_kind` と一致する外部 drag が矩形の上にあるときの候補スロット。
    pub external_insert_slot: Option<usize>,
    /// 上の位置で **このフレームに release された**。
    pub external_dropped_slot: Option<usize>,
    /// 描いた行の `(index, 画面座標 rect)` (context menu を重ねる用)。
    pub row_rects: Vec<(usize, Rect)>,
}

#[derive(Clone, Copy, Debug)]
struct Session {
    anchor: usize,
    anchor_pos: (f32, f32),
    last_pos: (f32, f32),
    press_modifiers: Modifiers,
    /// `rows[anchor].draggable`。`false` の session は click 判定にだけ使う。
    draggable: bool,
}

#[derive(Debug, Default)]
pub(crate) struct DragListState {
    session: Option<Session>,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 可変高・異種行の drag&drop リスト (module doc 参照)。
    ///
    /// - `valid_drop(from, slot)`: `from = Some(行)` は内部 drag、`None` は外部 drag。
    /// - `row(ui, i, rect, hovered, dragging)`: 行の描画 (背景も含めて caller)。
    #[allow(clippy::too_many_arguments)]
    pub fn drag_list<V, R>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        rows: &[DragListRow],
        slots: &[DragListSlot],
        accept_drag_kind: Option<&'static str>,
        style: &DragListStyle,
        valid_drop: V,
        mut row: R,
    ) -> DragListResponse
    where
        V: Fn(Option<usize>, usize) -> bool,
        R: FnMut(&mut Ui<'a, M>, usize, Rect, bool, bool),
    {
        let wid = WidgetId::ROOT.child((b"drag_list", &id));
        let pointer = self.pointer;
        let n = rows.len();

        // ---- layout ----
        let mut tops: Vec<f32> = Vec::with_capacity(n + 1);
        let mut acc = 0.0f32;
        for r in rows {
            tops.push(acc);
            acc += r.height + style.row_gap;
        }
        tops.push(acc);
        let slot_y = |s: &DragListSlot| -> f32 {
            let i = s.after_row.min(n);
            // 行の直前 = 前の行との gap の中央。
            let t = tops[i];
            if i == 0 { t } else { t - style.row_gap * 0.5 }
        };
        let row_at = |py: f32| -> Option<usize> {
            let local = py - rect.y;
            (0..n).find(|&i| local >= tops[i] && local < tops[i] + rows[i].height)
        };
        let nearest_slot = |py: f32, from: Option<usize>| -> Option<usize> {
            let local = py - rect.y;
            slots
                .iter()
                .enumerate()
                .filter(|(si, _)| valid_drop(from, *si))
                .min_by(|(_, a), (_, b)| {
                    let da = (slot_y(a) - local).abs();
                    let db = (slot_y(b) - local).abs();
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(si, _)| si)
        };

        // ---- press ----
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
            && rect.contains(px, py)
            && let Some(i) = row_at(py)
        {
            let state: &mut DragListState = self.widget_state(wid);
            state.session = Some(Session {
                anchor: i,
                anchor_pos: (px, py),
                last_pos: (px, py),
                press_modifiers: pointer.modifiers,
                draggable: rows[i].draggable,
            });
        }

        // ---- drag continue / carry out ----
        let mut dragged_out: Option<usize> = None;
        if let Some((px, py)) = pointer.pos {
            let out_of_x = px < rect.x - CARRY_OUT_MARGIN_PX || px > rect.x + rect.w + CARRY_OUT_MARGIN_PX;
            let state: &mut DragListState = self.widget_state(wid);
            if let Some(s) = state.session.as_mut() {
                s.last_pos = (px, py);
                if out_of_x && s.draggable {
                    dragged_out = Some(s.anchor);
                    state.session = None;
                }
            }
        }

        // ---- release ----
        let released: Option<Session> = if pointer.primary_just_released {
            let state: &mut DragListState = self.widget_state(wid);
            state.session.take()
        } else {
            None
        };
        let mut clicked = None;
        let mut clicked_modifiers = Modifiers::default();
        let mut dropped = None;
        if let Some(s) = released {
            let dy = (s.last_pos.1 - s.anchor_pos.1).abs();
            if dy >= DRAG_THRESHOLD_PX {
                if s.draggable && let Some(slot) = nearest_slot(s.last_pos.1, Some(s.anchor)) {
                    dropped = Some((s.anchor, slot));
                }
            } else {
                clicked = Some(s.anchor);
                clicked_modifiers = s.press_modifiers;
            }
        }

        // ---- 外部 drag ----
        let external_insert_slot: Option<usize> = accept_drag_kind
            .filter(|k| self.dragging_kind() == Some(*k))
            .and(pointer.pos)
            .filter(|&(px, py)| rect.contains(px, py))
            .and_then(|(_, py)| nearest_slot(py, None));
        let external_dropped_slot = if pointer.primary_just_released {
            external_insert_slot
        } else {
            None
        };

        // ---- 描画 ----
        let session: Option<Session> = {
            let state: &mut DragListState = self.widget_state(wid);
            state.session
        };
        let dragging_anchor =
            session.filter(|s| s.draggable && (s.last_pos.1 - s.anchor_pos.1).abs() >= DRAG_THRESHOLD_PX);
        let mut hovered = None;
        let mut row_rects = Vec::with_capacity(n);
        for i in 0..n {
            let r = Rect {
                x: rect.x,
                y: rect.y + tops[i],
                w: rect.w,
                h: rows[i].height,
            };
            let inside = pointer.pos.is_some_and(|(px, py)| r.contains(px, py));
            let in_block = dragging_anchor
                .is_some_and(|s| i >= s.anchor && i < s.anchor + rows[s.anchor].block_len.max(1));
            row_rects.push((i, r));
            row(self, i, r, inside, in_block);
            if inside {
                hovered = Some(i);
            }
        }
        // drop indicator (内部 drag と外部 drag は同時に成立しない)。
        let indicator = if let Some(si) = external_insert_slot {
            slots.get(si).copied()
        } else if let Some(s) = dragging_anchor {
            nearest_slot(s.last_pos.1, Some(s.anchor)).and_then(|si| slots.get(si).copied())
        } else {
            None
        };
        if let Some(slot) = indicator {
            let y = rect.y + slot_y(&slot) - style.drop_indicator_h * 0.5;
            self.push_rect(RectCommand {
                rect: Rect {
                    x: rect.x + slot.indent,
                    y,
                    w: (rect.w - slot.indent).max(0.0),
                    h: style.drop_indicator_h,
                },
                fill: style.drop_indicator_color,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }

        DragListResponse {
            clicked,
            clicked_modifiers,
            hovered,
            dragging: dragging_anchor.map(|s| s.anchor),
            dragged_out,
            dropped,
            external_insert_slot,
            external_dropped_slot,
            row_rects,
        }
    }
}

#[cfg(test)]
mod tests {
    use daw_ui_platform::{Modifiers, PhysicalSize};
    use daw_ui_renderer::{Color, Rect, Scene};

    use super::{DragListRow, DragListSlot, DragListStyle};
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    fn style() -> DragListStyle {
        DragListStyle {
            row_gap: 2.0,
            drop_indicator_color: Color::WHITE,
            drop_indicator_h: 2.0,
        }
    }

    fn rows() -> Vec<DragListRow> {
        // 0: 見出し (掴めない) / 1..=3: 掴める、行 1 はブロック長 2 (行 2 を連れて行く)
        vec![
            DragListRow { height: 20.0, draggable: false, block_len: 1 },
            DragListRow { height: 20.0, draggable: true, block_len: 2 },
            DragListRow { height: 20.0, draggable: true, block_len: 1 },
            DragListRow { height: 40.0, draggable: true, block_len: 1 },
        ]
    }

    fn slots() -> Vec<DragListSlot> {
        (0..=4).map(|i| DragListSlot { after_row: i, indent: 0.0 }).collect()
    }

    fn frame(pos: (f32, f32), pressed: bool, released: bool, down: bool) -> FrameInput {
        FrameInput {
            pointer: PointerFrame {
                pos: Some(pos),
                primary_just_pressed: pressed,
                primary_just_released: released,
                primary_pressed: down,
                modifiers: Modifiers::default(),
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        }
    }

    fn run(host: &mut UiHost<()>, input: FrameInput) -> super::DragListResponse {
        let mut scene = Scene::new();
        let mut out = super::DragListResponse::default();
        let mut model = ();
        host.frame(
            &mut model,
            &mut scene,
            PhysicalSize { width: 400, height: 400 },
            input,
            |_m, ui| {
                out = ui.drag_list(
                    "t",
                    Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                    &rows(),
                    &slots(),
                    None,
                    &style(),
                    |_, _| true,
                    |_, _, _, _, _| {},
                );
            },
        );
        out
    }

    #[test]
    fn short_click_reports_clicked_row() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let _ = run(&mut host, frame((10.0, 30.0), true, false, true));
        let r = run(&mut host, frame((10.0, 31.0), false, true, false));
        assert_eq!(r.clicked, Some(1));
        assert_eq!(r.dropped, None);
    }

    #[test]
    fn drag_past_threshold_drops_on_nearest_slot() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        // 行 1 (y 22..42) を掴んで行 3 の下 (y ≈ 108 = 末尾スロット) へ。
        let _ = run(&mut host, frame((10.0, 30.0), true, false, true));
        let mid = run(&mut host, frame((10.0, 100.0), false, false, true));
        assert_eq!(mid.dragging, Some(1));
        let r = run(&mut host, frame((10.0, 105.0), false, true, false));
        assert_eq!(r.dropped, Some((1, 4)), "末尾スロット (after_row = 4)");
        assert_eq!(r.clicked, None);
    }

    #[test]
    fn non_draggable_row_clicks_but_never_drags() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        // 短い click は掴めない行でも届く (Parallel の chain 行を選ぶ経路)。
        let _ = run(&mut host, frame((10.0, 5.0), true, false, true));
        let r = run(&mut host, frame((10.0, 6.0), false, true, false));
        assert_eq!(r.clicked, Some(0));
        // 閾値を超えて動かしても drop / dragging にならない。
        let _ = run(&mut host, frame((10.0, 5.0), true, false, true));
        let mid = run(&mut host, frame((10.0, 100.0), false, false, true));
        assert_eq!(mid.dragging, None);
        let r = run(&mut host, frame((10.0, 150.0), false, true, false));
        assert_eq!(r.dropped, None);
        assert_eq!(r.clicked, None);
        // 横へ出しても運搬にならない。
        let _ = run(&mut host, frame((10.0, 5.0), true, false, true));
        let r = run(&mut host, frame((300.0, 5.0), false, false, true));
        assert_eq!(r.dragged_out, None);
    }

    #[test]
    fn carrying_out_sideways_hands_over_the_drag() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let _ = run(&mut host, frame((10.0, 30.0), true, false, true));
        let r = run(&mut host, frame((300.0, 30.0), false, false, true));
        assert_eq!(r.dragged_out, Some(1));
        let r2 = run(&mut host, frame((300.0, 60.0), false, true, false));
        assert_eq!(r2.dropped, None, "横へ出たら widget の session は捨てられている");
    }
}
