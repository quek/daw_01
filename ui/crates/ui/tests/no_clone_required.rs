// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! User Model 型が `Clone`/`PartialEq`/`Hash`/`Default` を一切実装しなくても
//! `daw-ui-core` の公開 API がコンパイル可能であることを `trybuild` で固定する。
//!
//! これは本ライブラリの **load-bearing な不変条件** (`docs/plan.html`「設計上の不変条件」) の
//! 回帰防止。API シグネチャに `Clone` バウンドが紛れ込んだ瞬間にここで失敗させて気付く。

#[test]
fn no_clone_required() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass/*.rs");
}
