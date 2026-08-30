//! `split_view` widget — 縦/横分割で 2 つの pane を持つ (M7 Phase 26)。
//!
//! 中央の handle (6px) を drag で分割比率を調整。各 pane は `with_clip_rect` で overflow が切られる。

use std::hash::Hash;

use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::ui::{Ui, hovered, pressed_inside};

const HANDLE_THICK: f32 = 6.0;
const HANDLE_HIT_PAD: f32 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Horizontal, // 左右分割 (handle が縦)
    Vertical,   // 上下分割 (handle が横)
}

/// **分割比率は widget が持たない** (caller が所有する controlled widget)。
///
/// 以前は `ratio` を widget state に置いていたので、値の持ち主が widget と
/// アプリの 2 か所になり、「アプリ側に永続化する口が無い」 = セッションを
/// またぐと必ず既定値へ戻る、という状態だった。ここに残すのは drag 中だけ
/// 生きる掴み位置のみ。
#[derive(Debug, Default)]
pub(crate) struct SplitState {
    pub drag_anchor: Option<DragAnchor>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DragAnchor {
    pub pointer_axis: f32,
    pub start_ratio: f32,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// split_view widget。`rect` を `orientation` で 2 分割し、各 pane に `f` の左右 closure を呼ぶ。
    ///
    /// **`ratio` (0.05..=0.95) は caller が所有する** — widget は覚えない。中央 handle を
    /// drag すると `on_change(new_ratio)` が `Edit<M>` を発行するので、caller が自分の
    /// model に書いて次フレーム渡し直す (= 永続化したければ caller がそのまま保存できる)。
    /// drag 中のフレームは掴んだ位置から解いた **live 値**で分割するので、
    /// `Edit` 適用の 1 フレーム遅れは見た目に出ない。
    ///
    /// closure: `f(ui, left_pane_rect, right_pane_rect)`。各 pane は `with_clip_rect` で
    /// overflow を切る。利用者は pane 内側の widget を描画。
    pub fn split_view<F, C>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        orientation: Orientation,
        ratio: f32,
        on_change: C,
        f: F,
    ) where
        F: FnOnce(&mut Ui<'a, M>, Rect, Rect),
        C: Fn(f32) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"split_view", &id));
        let pointer = self.pointer;

        // state 取得 + drag 処理。比率の真値は caller の `ratio`。
        let base = ratio.clamp(0.05, 0.95);
        let mut next_ratio = None;
        let ratio = {
            let state: &mut SplitState = self.widget_state(wid);
            // handle 矩形を計算
            let handle_rect = handle_rect_for(rect, orientation, base);
            let hit_rect = expand_rect(handle_rect, HANDLE_HIT_PAD);

            // 押下開始
            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && hit_rect.contains(px, py)
            {
                let p_axis = match orientation {
                    Orientation::Horizontal => px,
                    Orientation::Vertical => py,
                };
                state.drag_anchor = Some(DragAnchor { pointer_axis: p_axis, start_ratio: base });
            }
            // drag 中
            let mut live = base;
            if let Some(anchor) = state.drag_anchor
                && let Some((px, py)) = pointer.pos
            {
                let (delta, span) = match orientation {
                    Orientation::Horizontal => (px - anchor.pointer_axis, rect.w.max(1.0)),
                    Orientation::Vertical => (py - anchor.pointer_axis, rect.h.max(1.0)),
                };
                let new_ratio = (anchor.start_ratio + delta / span).clamp(0.05, 0.95);
                if (new_ratio - base).abs() > 1e-6 {
                    next_ratio = Some(new_ratio);
                }
                live = new_ratio;
            }
            if pointer.primary_just_released {
                state.drag_anchor = None;
            }
            live
        };
        if let Some(next) = next_ratio {
            self.push_edit(on_change(next));
            // drag で境界が動いたフレーム → 次フレーム再描画 (利用者が on_event で
            // request_redraw を呼んでいなくても動くように、library 側でも要求)
            self.request_redraw();
        }

        // pane / handle rect 計算
        let (pane_a, pane_b, handle) = split_rects(rect, orientation, ratio);

        // closure 呼び出し (各 pane を clip)
        f(self, pane_a, pane_b);

        // handle 描画 (hover / drag で色変化)
        let handle_hit = expand_rect(handle, HANDLE_HIT_PAD);
        let pressed = pressed_inside(handle_hit, pointer);
        let hover = hovered(handle_hit, pointer);
        let p = self.palette();
        let fill = if pressed {
            p.border_focus
        } else if hover {
            p.border_hover
        } else {
            p.border
        };
        self.push_rect(RectCommand {
            rect: handle,
            fill,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [1.0; 4],
            clip_rect: None,
        });
    }
}

fn handle_rect_for(rect: Rect, orientation: Orientation, ratio: f32) -> Rect {
    match orientation {
        Orientation::Horizontal => {
            let split_x = rect.x + rect.w * ratio;
            Rect {
                x: split_x - HANDLE_THICK * 0.5,
                y: rect.y,
                w: HANDLE_THICK,
                h: rect.h,
            }
        }
        Orientation::Vertical => {
            let split_y = rect.y + rect.h * ratio;
            Rect {
                x: rect.x,
                y: split_y - HANDLE_THICK * 0.5,
                w: rect.w,
                h: HANDLE_THICK,
            }
        }
    }
}

fn split_rects(rect: Rect, orientation: Orientation, ratio: f32) -> (Rect, Rect, Rect) {
    let handle = handle_rect_for(rect, orientation, ratio);
    match orientation {
        Orientation::Horizontal => {
            let pane_a = Rect { x: rect.x, y: rect.y, w: handle.x - rect.x, h: rect.h };
            let pane_b = Rect {
                x: handle.x + handle.w,
                y: rect.y,
                w: (rect.x + rect.w) - (handle.x + handle.w),
                h: rect.h,
            };
            (pane_a, pane_b, handle)
        }
        Orientation::Vertical => {
            let pane_a = Rect { x: rect.x, y: rect.y, w: rect.w, h: handle.y - rect.y };
            let pane_b = Rect {
                x: rect.x,
                y: handle.y + handle.h,
                w: rect.w,
                h: (rect.y + rect.h) - (handle.y + handle.h),
            };
            (pane_a, pane_b, handle)
        }
    }
}

fn expand_rect(r: Rect, pad: f32) -> Rect {
    Rect { x: r.x - pad, y: r.y - pad, w: r.w + pad * 2.0, h: r.h + pad * 2.0 }
}
