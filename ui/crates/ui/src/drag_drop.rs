//! widget / view をまたぐ drag&drop の payload チャネル。
//!
//! `reorderable_list` の内部 reorder session が「1 widget の中で閉じた drag」なのに対し、
//! こちらは **掴んだ場所と落とす場所が別 widget** の drag を成立させるための唯一の口。
//! core はペイロードの中身を知らない (型消去 = `Arc<dyn Any>`)。`kind` は
//! 「この drag は誰のものか」を表す静的な札で、drop 側は自分の札とだけ照合する。
//!
//! 寿命: `begin_drag` から `take_drag_payload` / `cancel_drag` まで。誰も取らずに
//! ポインタが release されたフレームの終わりに host が自動で捨てる (= どこにも
//! 落とさなかった drag はキャンセル)。この後始末は
//! `UiHost::frame_to_edits_with_fonts` (= production も test も通る本体) が持つので、
//! 「取り消し忘れ」の分岐が view 側に生えない。

use std::any::Any;
use std::sync::Arc;

use daw_ui_platform::Modifiers;

use crate::ui::Ui;

/// 運搬中の payload 1 本 (drag は同時に 1 本だけ)。
pub struct DragPayload {
    pub(crate) kind: &'static str,
    pub(crate) value: Arc<dyn Any + Send + Sync>,
    /// 掴んだ瞬間のポインタ座標 (drag プレビューの原点)。
    pub origin: (f32, f32),
    /// **ボタンが押されていた最後のフレーム**の修飾キー。 drop 側はこれを読む
    /// (`pointer.modifiers` の生読みではない)。 release フレームの生読みは
    /// `ModifiersChanged` 先行 race で Ctrl が落ちて見え、「Ctrl を押しながら
    /// 離したのに移動になる」が起きる (同じ罠を arrangement が
    /// `ArrangementState.press_modifiers` で回避している)。
    pub modifiers: Modifiers,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// この frame から widget をまたぐ drag を始める。`kind` は drop 側と照合する静的な札。
    /// 既に payload があれば上書きする (drag は同時に 1 本だけ)。
    pub fn begin_drag<T: Any + Send + Sync>(&mut self, kind: &'static str, value: T) {
        *self.drag_payload = Some(DragPayload {
            kind,
            value: Arc::new(value),
            origin: self.pointer.pos.unwrap_or((0.0, 0.0)),
            // 掴んだフレームの値が初期値。以後は host の frame 頭 hook が
            // 「押されていた最後のフレーム」へ更新し続ける。
            modifiers: self.pointer.modifiers,
        });
    }

    /// 運搬中の payload を **消費せずに** 覗く (札が一致するときだけ `Some`)。
    #[must_use]
    pub fn drag_payload<T: Any + Send + Sync>(&self, kind: &'static str) -> Option<Arc<T>> {
        let p = self.drag_payload.as_ref()?;
        if p.kind != kind {
            return None;
        }
        p.value.clone().downcast::<T>().ok()
    }

    /// 運搬中の payload を取り出して drag を終える (drop の commit)。
    pub fn take_drag_payload<T: Any + Send + Sync>(
        &mut self,
        kind: &'static str,
    ) -> Option<Arc<T>> {
        let p = self.drag_payload.as_ref()?;
        if p.kind != kind {
            return None;
        }
        // downcast に失敗したら payload は取らない (札は合っているのに型が違う =
        // 呼び出し側の取り違えなので、黙って捨てず drag を続けさせる)。
        let value = p.value.clone().downcast::<T>().ok()?;
        *self.drag_payload = None;
        Some(value)
    }

    /// 運搬中の札 (`None` = drag していない)。drop target が indicator を出すかの判定に使う。
    #[must_use]
    pub fn dragging_kind(&self) -> Option<&'static str> {
        self.drag_payload.as_ref().map(|p| p.kind)
    }

    /// ボタンが押されていた最後のフレームの修飾キー。 drop 側の「移動 / コピー」判定は
    /// **必ずこれ**を使う (release フレームの `pointer().modifiers` は race で落ちる)。
    #[must_use]
    pub fn drag_modifiers(&self) -> Option<Modifiers> {
        self.drag_payload.as_ref().map(|p| p.modifiers)
    }

    /// 運搬を明示的に取り消す。
    pub fn cancel_drag(&mut self) {
        *self.drag_payload = None;
    }
}

#[cfg(test)]
mod tests {
    use daw_ui_platform::{Modifiers, PhysicalSize};
    use daw_ui_renderer::Scene;

    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    #[derive(Debug, PartialEq, Eq)]
    struct Payload(u32);

    const KIND: &str = "test.payload";

    fn pointer_at(x: f32, y: f32, pressed: bool) -> PointerFrame {
        PointerFrame {
            pos: Some((x, y)),
            primary_pressed: pressed,
            ..PointerFrame::default()
        }
    }

    #[test]
    fn begin_peek_take() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 200 };
        // frame 1: 掴む → 同フレームで覗ける。
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: pointer_at(10.0, 10.0, true), ..FrameInput::default() },
            |(), ui| {
                assert!(ui.dragging_kind().is_none());
                ui.begin_drag(KIND, Payload(7));
                assert_eq!(ui.dragging_kind(), Some(KIND));
                assert_eq!(ui.drag_payload::<Payload>(KIND).as_deref(), Some(&Payload(7)));
                // 覗くだけでは消えない。
                assert!(ui.drag_payload::<Payload>(KIND).is_some());
            },
        );
        // frame 2: まだ押しっぱなし → payload は生きている。take で消える。
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: pointer_at(40.0, 40.0, true), ..FrameInput::default() },
            |(), ui| {
                assert_eq!(ui.take_drag_payload::<Payload>(KIND).as_deref(), Some(&Payload(7)));
                assert!(ui.dragging_kind().is_none());
            },
        );
    }

    #[test]
    fn release_frame_cancels_untaken_drag() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 200 };
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: pointer_at(10.0, 10.0, true), ..FrameInput::default() },
            |(), ui| ui.begin_drag(KIND, Payload(1)),
        );
        // release frame: 誰も take しない → frame 末尾で捨てられる。
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: pointer_at(10.0, 10.0, false), ..FrameInput::default() },
            |(), ui| {
                assert_eq!(ui.dragging_kind(), Some(KIND), "release frame 中はまだ生きている");
            },
        );
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput::default(),
            |(), ui| assert!(ui.dragging_kind().is_none(), "release 後は自動キャンセル"),
        );
    }

    #[test]
    fn kind_mismatch_does_not_take() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 200 };
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: pointer_at(10.0, 10.0, true), ..FrameInput::default() },
            |(), ui| {
                ui.begin_drag(KIND, Payload(3));
                assert!(ui.drag_payload::<Payload>("other.kind").is_none());
                assert!(ui.take_drag_payload::<Payload>("other.kind").is_none());
                assert_eq!(ui.dragging_kind(), Some(KIND), "札違いでは取られない");
            },
        );
    }

    /// 押している間に Ctrl を離しても、 `drag_modifiers()` は
    /// **押されていた最後のフレーム** の値を返す (release frame の生読みは race で落ちる)。
    #[test]
    fn drag_modifiers_keep_last_pressed_frame() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 200 };
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame { modifiers: ctrl, ..pointer_at(10.0, 10.0, true) },
                ..FrameInput::default()
            },
            |(), ui| ui.begin_drag(KIND, Payload(5)),
        );
        // release frame: 修飾キーが落ちて見えるが、payload の値は据え置き。
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: pointer_at(10.0, 10.0, false), ..FrameInput::default() },
            |(), ui| {
                assert!(!ui.pointer().modifiers.ctrl, "生読みでは Ctrl が落ちている");
                assert!(
                    ui.drag_modifiers().is_some_and(|m| m.ctrl),
                    "drag_modifiers は押されていた最後のフレームの値"
                );
            },
        );
    }
}
