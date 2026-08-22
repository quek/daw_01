// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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

    #[test]
    fn mutate_applies_forward() {
        let mut m = ListModel { items: vec![1, 2, 3] };
        let edit = Edit::mutate(|m: &mut ListModel| m.items.push(4));
        edit.apply(&mut m);
        assert_eq!(m.items, vec![1, 2, 3, 4]);
    }
}
