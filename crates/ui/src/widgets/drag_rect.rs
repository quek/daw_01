//! M8 Phase 33: rect drag による multi-select 共通基盤。
//!
//! `Ui::take_drag_rect_in_rect(wid, bounds)` で、bounds 内の primary 押下から release までを
//! 1 つの drag セッションとして扱う。drag 中は library 側で半透明 cyan overlay を自動描画。
//! release フレームで `finished=true` を 1 度だけ返してから state クリア。
//!
//! 利用例 (擬似コード):
//! ```ignore
//! if let Some(drag) = ui.take_drag_rect_in_rect(notes_id, content_rect) {
//!     if drag.finished {
//!         let r = drag.rect();
//!         let new_sel: Vec<_> = model.notes.iter()
//!             .filter(|n| r.contains_point(n.x, n.y)).map(|n| n.id).collect();
//!         ui.push_edit(Edit::with_inverse(
//!             "select notes",
//!             move |m| m.selected = new_sel.clone(),
//!             move |m| m.selected = old_sel.clone(),
//!         ));
//!     }
//! }
//! ```

use daw_ui_platform::Modifiers;
use daw_ui_renderer::Rect;

/// 1 度の rect drag を表す snapshot。
#[derive(Debug, Clone, Copy)]
pub struct DragRect {
    /// drag 開始時の pointer 座標。
    pub start: (f32, f32),
    /// 現在の pointer 座標 (drag 中) または release 時の座標 (finished)。
    pub end: (f32, f32),
    /// drag 開始時の修飾キー (Shift/Ctrl/Alt — 選択モードの判別用)。
    pub modifiers: Modifiers,
    /// このフレームで release されて drag が完了したか。
    pub finished: bool,
}

impl DragRect {
    /// `start` / `end` から normalize された Rect を返す。
    #[must_use]
    pub fn rect(&self) -> Rect {
        let x = self.start.0.min(self.end.0);
        let y = self.start.1.min(self.end.1);
        let w = (self.start.0 - self.end.0).abs();
        let h = (self.start.1 - self.end.1).abs();
        Rect { x, y, w, h }
    }

    /// 引数の点が drag 範囲内に入っているか。
    #[must_use]
    pub fn contains_point(&self, x: f32, y: f32) -> bool {
        self.rect().contains(x, y)
    }
}

/// `take_drag_rect_in_rect` の内部 state (frame 越しに保持)。
#[derive(Debug, Default)]
pub(crate) struct DragRectState {
    /// drag 開始位置 (None = drag していない)。
    pub(crate) drag_start: Option<(f32, f32)>,
    /// drag 開始時の修飾キー (drag 中は固定で覚えておく)。
    pub(crate) start_modifiers: Modifiers,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_normalizes_negative_drag() {
        let dr = DragRect {
            start: (100.0, 200.0),
            end: (50.0, 80.0),
            modifiers: Modifiers::empty(),
            finished: false,
        };
        let r = dr.rect();
        assert!((r.x - 50.0).abs() < 1e-5);
        assert!((r.y - 80.0).abs() < 1e-5);
        assert!((r.w - 50.0).abs() < 1e-5);
        assert!((r.h - 120.0).abs() < 1e-5);
    }

    #[test]
    fn contains_point_inside_and_outside() {
        let dr = DragRect {
            start: (10.0, 10.0),
            end: (50.0, 50.0),
            modifiers: Modifiers::empty(),
            finished: false,
        };
        assert!(dr.contains_point(20.0, 20.0));
        assert!(!dr.contains_point(60.0, 60.0));
    }
}
