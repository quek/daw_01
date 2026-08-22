//! M14 Phase 63l: caller 側 view 用 rect-based pointer hit-test API。
//!
//! `Ui::take_primary_press_in_rect(rect)` と `Ui::take_drag_in_rect(id, rect)` の
//! 戻り型 / 内部 state を定義。 daw_01 #026 — Audio Editor のような自前 view (= 内部に
//! 独自 layout / event ごとの rect 配置を持つ caller 側 view) が、 button widget を
//! 介さずに rect 内 click / drag を直接取れるよう low-level primitive を公開する。
//!
//! 既存の `take_double_click_in_rect` / `take_file_drop_in_rect` /
//! `take_scroll_in_rect` の延長線。 modal popup 配下では消費せず、 同 frame 内で
//! 複数 caller が同 rect を要求しても 1 度だけ消費する semantics に揃える。
//!
//! ## drag overlay 描画は **行わない**
//!
//! `take_drag_rect_in_rect` (multi-select 用 widget) は library 側で半透明 cyan
//! overlay を描画するが、 こちらは「rect 内 drag を pure に取り出す」 だけの primitive。
//! 描画は caller 側の責務。 trim handle / move band / resize edge ごとに別の visual
//! feedback (= cursor 形状変化、 ghost preview) を出したい場合の自由度を最大化する。
//!
//! ## 利用例 (Audio Editor)
//!
//! ```ignore
//! for (idx, ev) in events.iter().enumerate() {
//!     let event_rect = compute_event_rect(ev);
//!
//!     // 中央 drag (移動): event_rect の左右 4px を除いた center band
//!     let center_band = event_rect.shrink_horizontal(4.0);
//!     if let Some(drag) = ui.take_drag_in_rect(("event-move", idx), center_band) {
//!         match drag.kind {
//!             DragKind::Started => { /* 選択切り替え */ }
//!             DragKind::Continuing => { /* ghost preview 描画 */ }
//!             DragKind::Released => {
//!                 ui.push_edit(SetAudioEventStart(idx, drag.delta.0));
//!             }
//!         }
//!     }
//! }
//! ```

use daw_ui_platform::Modifiers;

/// drag session の lifecycle。 1 フレームに 1 値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    /// 当該フレームに primary が押下されて drag が始まった (anchor が rect 内に入った frame)。
    Started,
    /// 既に drag 中で、 まだ release されていない。
    Continuing,
    /// 当該フレームに primary が release されて drag が終了した。
    /// この値は **1 度だけ** 返り、 次フレーム以降は `take_drag_in_rect` が `None` に戻る。
    Released,
}

/// `Ui::take_drag_in_rect` の戻り値。 1 フレーム分の snapshot。
#[derive(Debug, Clone, Copy)]
pub struct DragInfo {
    /// drag 開始時 (= press) の pointer 座標。 frame 越しで固定。
    pub anchor: (f32, f32),
    /// 現フレームの pointer 座標。 release frame では release 時点の最後の座標。
    /// pointer が PointerLeft で None になっている場合は anchor をそのまま返す。
    pub current: (f32, f32),
    /// `current.0 - anchor.0`, `current.1 - anchor.1`。 caller の delta 計算 boilerplate を削減。
    pub delta: (f32, f32),
    /// 当該フレームの phase (Started / Continuing / Released)。
    pub kind: DragKind,
    /// drag 開始時の修飾キー snapshot (Shift / Ctrl / Alt 等)。 frame 越しで固定。
    /// drag 開始時の意図 (= Shift+drag = 微調整、 Ctrl+drag = 複製等) を判定するのに使う。
    pub start_modifiers: Modifiers,
    /// 現フレームの修飾キー。 drag 中に Alt 等を押下 / 解放した場合の追従値。
    /// arrangement / piano_roll の drag は modifier を「drag 中に切り替え可能」 にする UX が
    /// 多いが、 caller が start 時固定の `start_modifiers` か 現フレーム値の `modifiers` か
    /// 選べる。
    pub modifiers: Modifiers,
}

/// `Ui::take_drag_in_rect` の内部 state (frame 越しに保持、 widget_state HashMap に格納)。
#[derive(Debug, Default)]
pub(crate) struct DragInRectState {
    /// drag 開始位置 (None = drag していない、 Some = drag 中で press 位置を記録済)。
    pub(crate) anchor: Option<(f32, f32)>,
    /// drag 開始時の修飾キー (drag 中は固定で覚えておく)。
    pub(crate) start_modifiers: Modifiers,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_kind_eq_works() {
        assert_eq!(DragKind::Started, DragKind::Started);
        assert_ne!(DragKind::Started, DragKind::Continuing);
        assert_ne!(DragKind::Continuing, DragKind::Released);
    }

    #[test]
    fn drag_info_delta_field_consistency() {
        let info = DragInfo {
            anchor: (100.0, 50.0),
            current: (130.0, 70.0),
            delta: (30.0, 20.0),
            kind: DragKind::Continuing,
            start_modifiers: Modifiers::empty(),
            modifiers: Modifiers::empty(),
        };
        assert!((info.delta.0 - (info.current.0 - info.anchor.0)).abs() < 1e-5);
        assert!((info.delta.1 - (info.current.1 - info.anchor.1)).abs() < 1e-5);
    }
}
