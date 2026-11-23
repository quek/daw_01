//! `resize_handle` — パネルの縁のリサイズ帯 (daw_01 r.md #158)。
//!
//! 「popup が開いている間は掴まない / 掴んだ press を名乗る / hover 中とドラッグ中はリサイズの
//! カーソル」 の 3 つを 1 か所に持つ。どれか 1 つを書き忘れると、 メニュー外クリックで閉じる
//! はずの press をリサイズ帯が食う / 帯の下にある行の click が release で成立する / どこを
//! 掴めるか見えない、 のどれかが起きる (使う側に同じ boilerplate を書かせない)。
//!
//! 値への写像 (帯の位置 → 幅) は caller が持つ — 右端固定のパネルは左へ動かすほど広く、
//! 左端固定のパネルは右へ動かすほど広い。写像には生のポインタ位置ではなく
//! [`ResizeDrag::handle_origin`] (帯の中の掴んだ位置を保った帯の左上) を使う — 生の位置を縁に写すと、
//! 帯の幅のぶん掴んだ瞬間に縁がポインタへ跳ぶ。

use std::hash::Hash;

use daw_ui_renderer::Rect;

use crate::id::WidgetId;
use crate::ui::Ui;
use crate::widgets::drag_in_rect::DragKind;
use crate::widgets::split_view::Orientation;
use daw_ui_platform::CursorIcon;

/// [`Ui::resize_handle`] の 1 フレーム分の結果 (掴んでいる間だけ `Some`)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResizeDrag {
    /// 今フレームのポインタ位置 (release フレームでは離した位置)。
    pub pointer: (f32, f32),
    /// 帯がポインタに付いて動いたときの帯の左上 (= `pointer` − 押した瞬間の「帯の左上から押した位置まで」)。
    /// 縁の位置はここから出す (帯の右端の縁なら `handle_origin.0 + 帯の幅`)。掴んだ瞬間は帯の今の左上と
    /// 一致するので、帯のどこを掴んでも縁は跳ばない。
    pub handle_origin: (f32, f32),
    /// このフレームで離した (= 値を確定して保存してよい)。
    pub released: bool,
}

/// 押した瞬間の「帯の左上から押した位置まで」(ドラッグの間だけ持つ)。
#[derive(Debug, Clone, Copy, Default)]
struct ResizeGrab {
    offset: (f32, f32),
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// パネルの縁のリサイズ帯。`rect` の中で press するとドラッグが始まり、 離すまで毎フレーム
    /// `Some` を返す (帯から外れても続く)。`axis` は帯の向きではなく **動かす方向**:
    /// `Horizontal` = 幅を変える (左右に動かす、 `EwResize`)、 `Vertical` = 高さを変える (`NsResize`)。
    ///
    /// popup (メニュー / dropdown の一覧) が開いている間は掴まない — 掴むと press を消費して
    /// 「一覧の外 click で閉じる」 が効かなくなる。 掴んだフレームに press の所有者を名乗るので、
    /// 帯の下に重なる click widget は release で click にならない (`click.rs`)。
    pub fn resize_handle(&mut self, id: impl Hash, rect: Rect, axis: Orientation) -> Option<ResizeDrag> {
        let wid = WidgetId::ROOT.child((b"resize_handle", &id));
        let cursor = match axis {
            Orientation::Horizontal => CursorIcon::EwResize,
            Orientation::Vertical => CursorIcon::NsResize,
        };
        let drag = if self.has_open_popups() { None } else { self.take_drag_in_rect(wid, rect) };
        if let Some(d) = drag {
            let grab_id = wid.child("grab");
            if d.kind == DragKind::Started {
                self.claim_press(wid);
                // 帯は値に付いて動くので、押した位置を帯の今の左上からの差で覚える (帯の左上の絶対位置は
                // 次のフレームには変わっている)。
                self.widget_state::<ResizeGrab>(grab_id).offset = (d.anchor.0 - rect.x, d.anchor.1 - rect.y);
            }
            let offset = self.widget_state::<ResizeGrab>(grab_id).offset;
            // ドラッグ中は帯から離れてもリサイズの形のまま (掴んでいる間の形を保つ)。
            self.set_cursor(cursor);
            return Some(ResizeDrag {
                pointer: d.current,
                handle_origin: (d.current.0 - offset.0, d.current.1 - offset.1),
                released: d.kind == DragKind::Released,
            });
        }
        if self.hovers(rect) {
            self.set_cursor(cursor);
        }
        None
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
    const HANDLE: Rect = Rect { x: 100.0, y: 0.0, w: 5.0, h: 300.0 };

    fn frame(pos: (f32, f32), pressed: bool, jp: bool, jr: bool) -> FrameInput {
        FrameInput {
            pointer: PointerFrame {
                pos: Some(pos),
                primary_pressed: pressed,
                primary_just_pressed: jp,
                primary_just_released: jr,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        }
    }

    /// 帯で press → 帯の外へ動かす → 離す。 掴んでいる間は毎フレーム位置が返り、 離した
    /// フレームだけ `released`。 帯の下に先に描いた click widget は click にならない。
    #[test]
    fn resize_handle_follows_the_pointer_until_release_and_owns_the_press() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let out = std::cell::Cell::new((None, false));
        let mut run = |host: &mut UiHost<()>, input: FrameInput| {
            host.frame_to_edits(&(), &mut scene, SCREEN, input, |(), ui| {
                // 帯の下 (先に描く) に重なる click widget。
                let below = ui.primary_click(WidgetId::ROOT.child("below"), HANDLE.contains(102.0, 10.0)).clicked;
                let drag = ui.resize_handle("h", HANDLE, Orientation::Horizontal);
                out.set((drag, below));
            });
        };
        // 帯 (x = 100, 幅 5) の左から 2px の所を掴む: 帯の左上は掴んだ瞬間は今の位置のまま (縁が跳ばない)、
        // 以後はポインタとの差 (2, 10) を保って動く。
        run(&mut host, frame((102.0, 10.0), true, true, false));
        let at = |pointer: (f32, f32), handle_origin: (f32, f32), released| ResizeDrag { pointer, handle_origin, released };
        assert_eq!(out.get().0, Some(at((102.0, 10.0), (100.0, 0.0), false)));
        run(&mut host, frame((180.0, 40.0), true, false, false));
        assert_eq!(out.get().0, Some(at((180.0, 40.0), (178.0, 30.0), false)), "帯の外でも続く");
        run(&mut host, frame((102.0, 10.0), false, false, true));
        assert_eq!(out.get().0, Some(at((102.0, 10.0), (100.0, 0.0), true)));
        assert!(!out.get().1, "帯の下の widget は click にならない (press の所有者は帯)");
        run(&mut host, frame((102.0, 10.0), false, false, false));
        assert_eq!(out.get().0, None, "離したら終わり");
    }

    /// popup が開いている間は帯を掴まない (一覧の外 click で閉じる、 を食わない)。
    #[test]
    fn resize_handle_ignores_presses_while_a_popup_is_open() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let out = std::cell::Cell::new(None);
        let input = frame((102.0, 10.0), true, true, false);
        host.frame_to_edits(&(), &mut scene, SCREEN, input, |(), ui| {
            ui.open_popup("menu", Rect { x: 300.0, y: 0.0, w: 50.0, h: 50.0 }, true);
            out.set(ui.resize_handle("h", HANDLE, Orientation::Horizontal));
        });
        assert_eq!(out.get(), None);
    }
}
