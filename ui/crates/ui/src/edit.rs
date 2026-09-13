//! `Edit<M>` — ユーザ操作から発生する編集要求。アプリ層に渡され、apply される。
//!
//! 設計方針:
//! - メッセージ型は導入しない (`Application::Message: Clone` 伝染を防ぐため)
//! - mutation は `Box<dyn FnOnce(&mut M) + Send + 'static>` に畳み込む (1 回限りで十分)
//! - `M` をジェネリックパラメータで持つことで、ユーザが定義した任意の Model 型に紐づけられる
//!
//! **undo/redo は lib の責務ではない** (S4a、`docs/plan_arch_refactor.md` §8)。
//! かつて lib 側に `Edit::Undoable` + `UiHost` history stack を持たせていたが、消費側 (daw_gui)
//! は lib undo を emit も replay もせず、undo SSoT はアプリの `SongDoc` snapshot 方式一本だった
//! (= 死荷重かつ二重 undo の危険源)。よって lib 側 undo 機構は撤去し、`Edit` は forward の
//! mutation を運ぶだけの一本道 (`Mutate`) にした。undo が要るアプリは `Edit` を自前 undo 機構
//! (snapshot / inverse patch など) の入口として使う。
//!
//! ## 適用順: prelude → 通常 (それぞれ push 順)
//!
//! 1 フレームに積まれた Edit はフレーム末にまとめて適用される。[`Ui::push_edit`] は push 順だが、
//! **[`Ui::push_prelude_edit`] で積んだ Edit は、同じフレームの通常の Edit より必ず先に適用される**
//! (prelude 同士は push 順)。
//!
//! 用途は「同じフレームで後から出る Edit を束ねる宣言」— 例: widget の後で「このドラッグを
//! 1 つにまとめる」と宣言する側。immediate-mode では宣言する側が widget の応答 (ドラッグが始まった)
//! を見てからしか積めないが、widget 自身は同じフレームの中で既に値の Edit を積んでいる
//! (press でクリック位置へ飛ぶ / ドラッグが閾値を越えたフレームで最初の値を出す)。宣言を prelude に
//! 積めば、呼び出し側の描画順に依らず宣言が値より先に効く。
//!
//! 宣言を **閉じる** 側 (終わりの宣言) は通常の Edit で積むこと — 離したフレームに widget が出す
//! 最後の値が、閉じる前に適用されるようにするため。
//!
//! ### 同じフレームに「A を離す」と「B を掴む」が来たとき
//!
//! 適用順は「B を開く (prelude)」→「A の最後の値 (通常)」→「A を閉じる (通常)」になる。つまり
//! A の最後の値は B を開いた後に適用される。束ねる単位を 1 本しか持たないアプリでは A の最後の値が
//! B の側に入る。閉じる側は「自分が開いたものか」を照合して閉じること (B を閉じてしまわないように)。
//! 物理マウスでは同じフレームに離すと別 widget の押下が重なることはまず無く、離したフレームの値は
//! 直前のフレームと同値であることが多い。

use crate::ui::Ui;

/// 1 つの編集 (forward mutation)。
///
/// widget は値変化のたびにこれを 1 つ発行し、アプリ層で `&mut M` に apply される。
/// undo/redo はアプリ層の責務 (lib は forward だけを運ぶ)。
pub enum Edit<M: ?Sized + 'static> {
    /// model への 1 回限りの mutation。
    Mutate(Box<dyn FnOnce(&mut M) + Send + 'static>),
}

impl<M: ?Sized + 'static> Edit<M> {
    /// mutation を作る短縮コンストラクタ。`FnOnce` で十分 (再実行しない)。
    pub fn mutate<F: FnOnce(&mut M) + Send + 'static>(f: F) -> Self {
        Self::Mutate(Box::new(f))
    }

    /// アプリ側で保持している `&mut M` に対して apply する。
    pub fn apply(self, model: &mut M) {
        match self {
            Self::Mutate(f) => f(model),
        }
    }
}

impl<M: ?Sized + 'static> Ui<'_, M> {
    /// 同じフレームの通常の Edit ([`Ui::push_edit`]) より **先に** 適用される Edit を積む
    /// (prelude 同士は push 順)。意味と使いどころはモジュール doc の「適用順」。
    pub fn push_prelude_edit(&mut self, edit: Edit<M>) {
        self.edits.insert(self.prelude_len, edit);
        self.prelude_len += 1;
    }
}

impl<M: ?Sized + 'static> std::fmt::Debug for Edit<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mutate(_) => f.write_str("Edit::Mutate(<closure>)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ListModel {
        items: Vec<i32>,
    }

    /// prelude は通常の Edit より先、どちらも push 順。`frame` の適用順と `frame_to_edits` の並びが同じ。
    #[test]
    fn prelude_edits_apply_before_regular_edits_in_push_order() {
        use crate::input::FrameInput;
        use crate::ui::UiHost;
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        let build = |_: &ListModel, ui: &mut Ui<'_, ListModel>| {
            ui.push_edit(Edit::mutate(|m: &mut ListModel| m.items.push(1)));
            ui.push_prelude_edit(Edit::mutate(|m: &mut ListModel| m.items.push(2)));
            ui.push_edit(Edit::mutate(|m: &mut ListModel| m.items.push(3)));
            ui.push_prelude_edit(Edit::mutate(|m: &mut ListModel| m.items.push(4)));
        };
        let screen = PhysicalSize { width: 100, height: 100 };

        let mut host: UiHost<ListModel> = UiHost::no_redraw();
        let mut m = ListModel { items: Vec::new() };
        host.frame(&mut m, &mut Scene::new(), screen, FrameInput::default(), build);
        assert_eq!(m.items, vec![2, 4, 1, 3]);

        let mut m = ListModel { items: Vec::new() };
        let edits = host.frame_to_edits(&m, &mut Scene::new(), screen, FrameInput::default(), build);
        for e in edits {
            e.apply(&mut m);
        }
        assert_eq!(m.items, vec![2, 4, 1, 3], "frame_to_edits の並び = 適用順");
    }

    #[test]
    fn mutate_applies_forward() {
        let mut m = ListModel { items: vec![1, 2, 3] };
        let edit = Edit::mutate(|m: &mut ListModel| m.items.push(4));
        edit.apply(&mut m);
        assert_eq!(m.items, vec![1, 2, 3, 4]);
    }
}
