//! primary click の判定 — 「press も release も同じ widget の上」 を 1 箇所で決める
//! (daw_01 r.md #122)。
//!
//! immediate-mode では release フレームの pointer 位置しか見えないので、 素朴に
//! 「rect 内で `primary_just_released`」 を click とみなすと **別の widget で始めた
//! ドラッグの終わり** まで click に化ける (数値欄を縦にドラッグして、 たまたま dropdown の
//! 上で離すと dropdown が開く)。 widget ごとに `press_started_inside` を持つと、 press を
//! 受けた widget が release 側より後に描かれる配置で釣り合わず、 同じバグが widget の数だけ
//! 再発する。
//!
//! そこで press の **所有者** を `UiHost` がフレームを跨いで 1 つだけ持つ:
//! - press フレームに、 pointer が自分の rect 内だった widget が [`Ui::primary_click`] で
//!   所有者になる (同じフレームで複数が名乗れば後勝ち = 後に描かれる方が手前)。
//! - ドラッグ系の widget (数値欄 / knob / fader / scrollbar …) は click を返さないが、
//!   press を掴んだことを [`Ui::claim_press`] で宣言する。 これで「行の中の fader を
//!   ドラッグして行の上で離す」 が行の click にならない。
//! - release フレームに `clicked` が立つのは **所有者だけ**。 所有者不在 (背景 / ドラッグ
//!   widget で始まった press) の release は誰の click にもならない。
//! - release フレームの末尾で所有者は消える。 次の press で必ず取り直す。
//!
//! 「rect 内で `primary_just_released`」 を click として直接読む widget を新しく書かないこと。
//! release だけ見る判定は全部この口を通す。

use daw_ui_renderer::Rect;

use crate::id::WidgetId;
use crate::ui::Ui;

/// [`Ui::primary_click`] の結果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClickState {
    /// このフレームで click が成立した (press と release の両方がこの widget の上)。
    pub clicked: bool,
    /// この widget で始まった press を、 いま widget の上で保持している (pressed 表示用)。
    pub held: bool,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// primary click 判定の唯一の口。 `inside` は「pointer がこの widget の当たり判定の
    /// 中に居る」 (modal 遮蔽 / popup 開放中の除外は呼び出し側の `inside` に畳む)。
    ///
    /// press フレームに `inside` なら所有者になり、 release フレームに `inside` かつ所有者
    /// なら `clicked`。 所有者は release フレームの末尾に `UiHost` が消す。
    pub fn primary_click(&mut self, wid: WidgetId, inside: bool) -> ClickState {
        let pointer = self.pointer;
        if pointer.primary_just_pressed && inside {
            *self.press_owner = Some(wid);
        }
        let owned = *self.press_owner == Some(wid);
        ClickState {
            clicked: pointer.primary_just_released && inside && owned,
            held: owned && inside && pointer.primary_pressed,
        }
    }

    /// press を掴んだドラッグ系 widget が所有者を名乗る。 click は返さないが、 これを
    /// 呼ばないと同じ場所に重なる click widget (行の背景など) が release で click になる。
    /// press フレームで、 掴んだと決めた直後に呼ぶ。
    pub fn claim_press(&mut self, wid: WidgetId) {
        *self.press_owner = Some(wid);
    }

    /// 現在の press 所有者 (press〜release の間だけ `Some`)。
    pub fn press_owner(&self) -> Option<WidgetId> {
        *self.press_owner
    }

    /// `wid` が press を掴んでいたが、 **同じ press を後から別の widget が名乗った**
    /// (= 自分の中に描かれる子 widget がドラッグを始めた)。 コンテナ系のドラッグ
    /// (行の並べ替え等) は continuation フレームでこれを見て自分の session を捨てる —
    /// でないと「行の中の数値欄をドラッグしたら行まで動く」 (daw_01 r.md #124)。
    /// 所有者不在 (背景で始まった press) は「奪われた」 とはみなさない。
    pub fn press_taken_from(&self, wid: WidgetId) -> bool {
        self.press_owner.is_some_and(|o| o != wid)
    }

    /// daw_01 r.md #127: このフレームに「ボタンを押したまま Esc」 が来た。 drag session を
    /// 持つ widget は session を捨てる (値を per-frame で流していた widget は press 時の値へ
    /// 戻す)。 Esc 自体は `UiHost` が shortcut 層の手前で抜くので、 選択解除や窓の close
    /// には化けない。
    pub fn drag_cancel_requested(&self) -> bool {
        self.drag_cancel
    }

    /// **hover 用**のポインタ位置。 別の widget が press を掴んだままドラッグしている間は
    /// `None` (daw_01 r.md #124: ドラッグの通り道の部品が光らない)。 当たり判定 (press /
    /// release / drop 先) には使わない — それらは `pointer().pos` のまま。
    pub fn hover_pos(&self) -> Option<(f32, f32)> {
        if self.hover_blocked { None } else { self.pointer.pos }
    }

    /// `rect` に hover しているか ([`Self::hover_pos`] 基準)。
    pub fn hovers(&self, rect: Rect) -> bool {
        self.hover_pos().is_some_and(|(px, py)| rect.contains(px, py))
    }
}

#[cfg(test)]
mod tests {
    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Rect, Scene};

    use super::*;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    const SCREEN: PhysicalSize = PhysicalSize { width: 400, height: 300 };
    const A: Rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 50.0 };
    const B: Rect = Rect { x: 0.0, y: 100.0, w: 100.0, h: 50.0 };

    fn frame(pos: (f32, f32), pressed: bool, just_pressed: bool, just_released: bool) -> FrameInput {
        FrameInput {
            pointer: PointerFrame {
                pos: Some(pos),
                primary_pressed: pressed,
                primary_just_pressed: just_pressed,
                primary_just_released: just_released,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        }
    }

    /// 2 つの widget (A の後に B を描く) を 1 frame 回し、 (A, B) の ClickState を返す。
    fn run(host: &mut UiHost<()>, input: FrameInput) -> (ClickState, ClickState) {
        let mut scene = Scene::new();
        let out = std::cell::Cell::new((ClickState::default(), ClickState::default()));
        host.frame_to_edits(&(), &mut scene, SCREEN, input, |(), ui| {
            let wa = WidgetId::ROOT.child(b"a");
            let wb = WidgetId::ROOT.child(b"b");
            let pa = ui.pointer().pos;
            let a = ui.primary_click(wa, pa.is_some_and(|(x, y)| A.contains(x, y)));
            let b = ui.primary_click(wb, pa.is_some_and(|(x, y)| B.contains(x, y)));
            out.set((a, b));
        });
        out.get()
    }

    #[test]
    fn release_over_another_widget_is_not_its_click() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        // A で press → A の上で保持 (held)。
        let (a, b) = run(&mut host, frame((10.0, 10.0), true, true, false));
        assert!(a.held && !a.clicked && !b.held && !b.clicked);
        // B の上まで動かして離す → どちらの click でもない。
        let (a, b) = run(&mut host, frame((10.0, 110.0), true, false, false));
        assert!(!a.held && !b.held, "A の press は B の上では held にならない");
        let (a, b) = run(&mut host, frame((10.0, 110.0), false, false, true));
        assert!(!a.clicked && !b.clicked, "別 widget で始めた press の release は click でない");
        // release で所有者が消えているので、 次の press が無い release も誰の click でもない。
        let (a, b) = run(&mut host, frame((10.0, 110.0), false, false, true));
        assert!(!a.clicked && !b.clicked);
    }

    #[test]
    fn press_and_release_on_the_same_widget_is_a_click() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        run(&mut host, frame((10.0, 110.0), true, true, false));
        let (a, b) = run(&mut host, frame((20.0, 120.0), false, false, true));
        assert!(b.clicked && !a.clicked);
    }

    #[test]
    fn a_drag_widget_that_claims_the_press_takes_it_from_the_row_underneath() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let clicked = std::cell::Cell::new(false);
        let row = WidgetId::ROOT.child(b"row");
        let fader = WidgetId::ROOT.child(b"fader");
        // 行 (A 全体) の中に fader (A の右半分) が後から描かれる。 fader の上で press。
        for input in [
            frame((80.0, 10.0), true, true, false),
            frame((20.0, 10.0), false, false, true),
        ] {
            host.frame_to_edits(&(), &mut scene, SCREEN, input, |(), ui| {
                let pos = ui.pointer().pos;
                let in_row = pos.is_some_and(|(x, y)| A.contains(x, y));
                let c = ui.primary_click(row, in_row);
                if ui.pointer().primary_just_pressed && pos.is_some_and(|(x, _)| x >= 50.0) {
                    ui.claim_press(fader);
                }
                clicked.set(c.clicked);
            });
        }
        assert!(!clicked.get(), "fader のドラッグを行の上で離しても行の click にならない");
    }
}
