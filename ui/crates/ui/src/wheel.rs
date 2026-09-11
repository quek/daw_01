//! ホイール (scroll delta) の配信 — pointer 位置で決まる「誰が消費するか」を 1 箇所で持つ。
//!
//! 入力層 (`input.rs`) が縦 / 横の両軸を px 化してフレームに蓄積し、widget は自分の rect の上に
//! pointer があるときだけ取り出す。focus は要らない。同フレームに複数 widget が呼んでも、
//! **最初に呼んだ widget が消費する** (描画順 = 消費順)。
//!
//! 軸ごとに所有者が違う配置 (ランチャー帯の上で横ホイール = 帯のシーン送り / 縦ホイール = 行
//! スクロール) のために、横成分だけを消費する [`Ui::take_scroll_x_in_rect`] がある。

use daw_ui_renderer::Rect;

use crate::ui::Ui;

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// pointer が `rect` 内にあるなら、このフレームに蓄積された scroll delta (px) を取り出して
    /// 内部 buffer を 0 に戻す。
    ///
    /// 戻り値は `(dx, dy)` (winit 慣行: `dy > 0` = wheel を上方向に回した = コンテンツが上に流れる)。
    pub fn take_scroll_in_rect(&mut self, rect: Rect) -> (f32, f32) {
        if !self.scroll_deliverable_in_rect(rect) {
            return (0.0, 0.0);
        }
        let d = self.pointer.scroll_delta;
        self.pointer.scroll_delta = (0.0, 0.0);
        // M14 Phase 94 (daw_01 #065): consume を両 pointer に反映 (`consume_pointer_click` と対称)。
        // popup body は `pointer_raw` の copy を読むので、mirror しないと同 frame の別 body へ
        // 同じ scroll が二重配信されうる (multi-popup edge)。
        self.pointer_raw.scroll_delta = (0.0, 0.0);
        d
    }

    /// [`Self::take_scroll_in_rect`] の横成分だけを消費する版。縦成分は残るので、同フレームの
    /// 後続 widget が縦スクロールとして拾える。
    pub fn take_scroll_x_in_rect(&mut self, rect: Rect) -> f32 {
        if !self.scroll_deliverable_in_rect(rect) {
            return 0.0;
        }
        let dx = self.pointer.scroll_delta.0;
        self.pointer.scroll_delta.0 = 0.0;
        self.pointer_raw.scroll_delta.0 = 0.0;
        dx
    }

    /// modal popup の下に隠れている widget は pointer 入力を消費しない (#015)。
    fn scroll_deliverable_in_rect(&self, rect: Rect) -> bool {
        !self.pointer_blocked_by_modal_popup()
            && self.pointer.pos.is_some_and(|(px, py)| rect.contains(px, py))
    }
}
