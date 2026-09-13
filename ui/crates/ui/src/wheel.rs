//! ホイール (scroll delta) の配信 — pointer 位置で決まる「誰が消費するか」を 1 箇所で持つ。
//!
//! 入力層 (`input.rs`) が縦 / 横の両軸を px 化してフレームに蓄積し、widget は自分の rect の上に
//! pointer があるときだけ取り出す。focus は要らない。同フレームに複数 widget が呼んでも、
//! **最初に呼んだ widget が消費する** (描画順 = 消費順)。
//!
//! 軸ごとに所有者が違う配置 (ランチャー帯の上で横ホイール = 帯のシーン送り / 縦ホイール = 行
//! スクロール) のために、横成分だけを消費する [`Ui::take_scroll_x_in_rect`] がある。
//!
//! ## 子 widget がホイールを使う (claim、daw_01 r.md #129)
//!
//! 「最初に呼んだ widget が消費する」ので、[`crate::widgets::scroll_area`] の **中** に置いた
//! widget (EQ カーブの点でホイール = Q) は、祖先の scroll_area が中身の closure より前に
//! ホイールを取ってしまい永久に受け取れない。そこで子は毎フレーム [`Ui::claim_wheel_in_rect`] で
//! 「この矩形の上のホイールは自分が使う」と宣言し、scroll_area は **前フレームに claim された
//! 矩形** の上ではホイールを消費しない (`wheel_claimed_at_pointer`)。
//!
//! 前フレームの宣言を読むので、claim は **1 フレーム遅れ** で効き始め、1 フレーム遅れで失効する
//! (widget が描かれなくなった次のフレームまでは祖先がホイールを譲り続ける)。

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

    /// `rect` の上のホイールをこの widget が使う、と宣言する (module doc)。ホイールを使う間は
    /// **毎フレーム** 呼ぶこと (呼ばなかったフレームの次から祖先の scroll_area が消費を再開する)。
    pub fn claim_wheel_in_rect(&mut self, rect: Rect) {
        self.wheel_claims[1].push(rect);
    }

    /// pointer が **前フレームに claim された** 矩形の上にあるか (祖先の scroll_area が消費を譲る)。
    pub(crate) fn wheel_claimed_at_pointer(&self) -> bool {
        self.pointer
            .pos
            .is_some_and(|(px, py)| self.wheel_claims[0].iter().any(|r| r.contains(px, py)))
    }

    /// modal popup の下に隠れている widget は pointer 入力を消費しない (#015)。
    fn scroll_deliverable_in_rect(&self, rect: Rect) -> bool {
        !self.pointer_blocked_by_modal_popup()
            && self.pointer.pos.is_some_and(|(px, py)| rect.contains(px, py))
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use std::cell::Cell;

    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Rect, Scene};

    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    const VIEWPORT: Rect = Rect { x: 0.0, y: 0.0, w: 200.0, h: 100.0 };
    /// scroll_area の中身 (viewport 座標) で子がホイールを使う矩形。
    const CLAIM: Rect = Rect { x: 50.0, y: 20.0, w: 20.0, h: 20.0 };

    /// あふれた scroll_area の中で、`claim` なら子が `CLAIM` を claim してホイールを取る。
    /// 戻り値 = (scroll_area の offset.y, 子が受け取った dy)。
    fn run(host: &mut UiHost<()>, pos: (f32, f32), dy: f32, claim: bool) -> (f32, f32) {
        let mut scene = Scene::new();
        let child = Cell::new(0.0);
        let input = FrameInput {
            pointer: PointerFrame { pos: Some(pos), scroll_delta: (0.0, dy), ..PointerFrame::default() },
            ..FrameInput::default()
        };
        let mut offset = (0.0, 0.0);
        host.frame_to_edits(&(), &mut scene, PhysicalSize { width: 400, height: 400 }, input, |(), ui| {
            offset = ui.scroll_area("s", VIEWPORT, (200.0, 1_000.0), |ui, _| {
                if claim {
                    ui.claim_wheel_in_rect(CLAIM);
                    child.set(ui.take_scroll_in_rect(CLAIM).1);
                }
            });
        });
        (offset.1, child.get())
    }

    /// R-12: 前フレームに claim された矩形の上では祖先の scroll_area がホイールを消費せず、
    /// claim は 1 フレーム遅れで効き / 失効する。矩形の外では通常どおりスクロールする。
    #[test]
    fn claimed_rect_keeps_the_wheel_from_the_ancestor_scroll_area_with_one_frame_lag() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let over = (60.0, 30.0);
        // 1 フレーム目: claim はまだ前フレームに無いので scroll_area が取る。
        let (off, child) = run(&mut host, over, -40.0, true);
        assert!(off > 0.0 && child == 0.0, "claim の初回フレームは祖先が消費する: off={off} child={child}");
        // 2 フレーム目以降: 子が受け取り、scroll 量は変わらない。
        let (off2, child) = run(&mut host, over, -40.0, true);
        assert_eq!(off2, off, "claim した矩形の上ではスクロールしない");
        assert_eq!(child, -40.0);
        // 矩形の外は通常どおりスクロール。
        let (off3, _) = run(&mut host, (150.0, 80.0), -40.0, true);
        assert!(off3 > off2);
        // claim をやめたフレームはまだ前フレームの claim が残る (祖先は消費しない) → 次で失効。
        let (off4, _) = run(&mut host, over, -40.0, false);
        assert_eq!(off4, off3, "claim をやめた直後の 1 フレームは譲り続ける");
        let (off5, _) = run(&mut host, over, -40.0, false);
        assert!(off5 > off4, "1 フレーム遅れで失効する");
    }
}
