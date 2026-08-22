// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! WidgetId が 64-bit FNV-1a で 1M 件の child 生成で衝突しないことを担保。
//!
//! plan.md M4 の "input hash 衝突テスト: ランダムなウィジェット入力 1M 件で衝突 0" 仕様。
//! `rand` crate を持ち込まず sequential seed で決定論的に検証する。
//! FNV-1a 64-bit は uniform-ish な入力で十分な品質を持つ (悪意ある衝突攻撃は本ライブラリ
//! の脅威モデルに含まれない)。

use std::collections::HashSet;

use daw_ui_core::WidgetId;

/// 1000 parents × 1000 children = 1M unique IDs。
/// 64-bit hash の birthday-bound では 1M^2 / 2^64 ≈ 5e-8、現実的にゼロ衝突。
#[test]
fn no_collision_at_1m_children() {
    let mut ids: HashSet<WidgetId> = HashSet::with_capacity(1_000_000);
    for parent_idx in 0..1000_u64 {
        let parent = WidgetId::ROOT.child(parent_idx);
        for child_idx in 0..1000_u64 {
            ids.insert(parent.child(child_idx));
        }
    }
    assert_eq!(
        ids.len(),
        1_000_000,
        "WidgetId 衝突を検出: {} 個のキーが生成されたが unique は {} 個",
        1_000_000,
        ids.len(),
    );
}
