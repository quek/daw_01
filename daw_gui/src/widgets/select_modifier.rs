// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! 選択面 (クリップ / ノート / オートメーション / トラック / セクション / オーディオイベント)
//! に共通する **修飾キー付き click の選択遷移** を 1 箇所に集約する (`docs/plan_selection_modifiers.md`)。
//!
//! 規約は OS 標準 (Windows Explorer / macOS Finder) および REAPER と同一:
//!
//! - 無修飾 click = `Single` (置換)
//! - Ctrl+click = `Toggle` (個別に足し引き)
//! - Shift+click = `RangeFromAnchor` (アンカーから clicked までの範囲)
//!
//! **アンカーは `Single` / `Toggle` で更新し `RangeFromAnchor` では更新しない**
//! (`SelectModifier::updates_anchor`)。 同じ基点から繰り返し Shift+click して範囲を
//! 伸縮できるようにするため。 「選択集合の末尾 = アンカー」 という旧 idiom は
//! `RangeFromAnchor` が集合ごと書き換えるので基点として使えず、 アンカーは
//! `SelectionState` の明示フィールドが所有する (SSoT)。

/// 選択面が 2 次元 (行 × 時間) のときの範囲計算に渡す 1 要素。
///
/// - `row`: 行の順序値。 クリップ = 可視トラック行 index、 ノート = pitch、
///   automation clip = 可視 lane 行 index、 1 次元面 (時間のみ) は全要素 0。
/// - `start` / `end`: 時間範囲 (拍)。 交差判定は「触れていれば入る」 (投げ縄と同じ)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RangeItem<T> {
    pub key: T,
    pub row: i64,
    pub start: f64,
    pub end: f64,
}

/// track header click / clip click / note click 等の selection 変更 modifier (DAW 業界標準)。
///
/// - `Single`: 修飾なし click → `next = vec![clicked]`、 アンカーを clicked で更新
/// - `RangeFromAnchor`: Shift+click → アンカーと clicked の間の範囲を選択。
///   アンカーが無い / 解決できない場合は `Single` 同等。 アンカーは更新しない
/// - `Toggle`: Ctrl+click → clicked が選択に居れば外し、 居なければ足す。 アンカーは更新する
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectModifier {
    Single,
    RangeFromAnchor,
    Toggle,
}

impl SelectModifier {
    /// press 時の modifier snapshot から解決する。 Shift が Ctrl より優先
    /// (Ctrl+Shift+click は範囲選択。 Explorer / Finder と同じ)。
    #[must_use]
    pub fn from_modifiers(shift: bool, ctrl: bool) -> Self {
        if shift {
            Self::RangeFromAnchor
        } else if ctrl {
            Self::Toggle
        } else {
            Self::Single
        }
    }

    /// この modifier がアンカーを clicked へ動かすか。 `RangeFromAnchor` だけ据え置き。
    #[must_use]
    pub fn updates_anchor(self) -> bool {
        !matches!(self, Self::RangeFromAnchor)
    }

    /// 選択集合の遷移を解決する。
    ///
    /// `range` は `RangeFromAnchor` のときだけ評価される (アンカーから clicked までの
    /// 範囲に入る要素。 アンカー不在等で範囲を作れないときは `None` を返す → `Single` 相当)。
    #[must_use]
    pub fn resolve<T, F>(self, prev: &[T], clicked: T, range: F) -> Vec<T>
    where
        T: Copy + PartialEq,
        F: FnOnce() -> Option<Vec<T>>,
    {
        match self {
            Self::Single => vec![clicked],
            Self::Toggle => {
                let mut next = prev.to_vec();
                if let Some(pos) = next.iter().position(|k| *k == clicked) {
                    next.remove(pos);
                } else {
                    next.push(clicked);
                }
                next
            }
            Self::RangeFromAnchor => range().unwrap_or_else(|| vec![clicked]),
        }
    }
}

/// 2 次元 (行 × 時間) の面で、 アンカー要素と clicked 要素を対角とする長方形ブロックに
/// 入る要素を返す。
///
/// 行は `[min(row), max(row)]` の閉区間、 時間は 2 要素の時間範囲の和
/// `[min(start), max(end)]` と**重なる**要素。
///
/// 時間の判定は投げ縄 (`rects_intersect`) と同じ **strict** な重なり
/// (`start < t_hi && end > t_lo`)。 端が接するだけ (前の clip の終端 == 範囲の始端) は
/// 入れない — clip / note は隣接要素が端点を共有するのが普通なので、 閉区間で判定すると
/// 範囲の外側にある隣の要素まで毎回 1 つ余計に拾ってしまう。
/// ただし **アンカーと clicked 自身は常に含める** (長さ 0 の要素で strict 判定が
/// 空振りするのを防ぐ)。
///
/// `items` の並び順がそのまま結果の順になる (描画順 = 選択順を保つ)。
///
/// `anchor` / `clicked` が `items` に見つからなければ `None` (caller は `Single` に倒す)。
#[must_use]
pub fn range_block<T>(items: &[RangeItem<T>], anchor: T, clicked: T) -> Option<Vec<T>>
where
    T: Copy + PartialEq,
{
    let a = items.iter().find(|it| it.key == anchor)?;
    let b = items.iter().find(|it| it.key == clicked)?;
    let (row_lo, row_hi) = (a.row.min(b.row), a.row.max(b.row));
    let t_lo = a.start.min(b.start);
    let t_hi = a.end.max(b.end);
    Some(
        items
            .iter()
            .filter(|it| {
                it.key == anchor
                    || it.key == clicked
                    || (it.row >= row_lo
                        && it.row <= row_hi
                        && it.start < t_hi
                        && it.end > t_lo)
            })
            .map(|it| it.key)
            .collect(),
    )
}

/// 1 次元の順序付き面 (トラック / セクション / audio event) で、 アンカーと clicked の
/// 間の連続範囲を返す。 `order` は表示順に並んだ全要素。
#[must_use]
pub fn range_ordered<T>(order: &[T], anchor: T, clicked: T) -> Option<Vec<T>>
where
    T: Copy + PartialEq,
{
    let ai = order.iter().position(|k| *k == anchor)?;
    let bi = order.iter().position(|k| *k == clicked)?;
    let (lo, hi) = (ai.min(bi), ai.max(bi));
    Some(order[lo..=hi].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: u32, row: i64, start: f64, end: f64) -> RangeItem<u32> {
        RangeItem { key, row, start, end }
    }

    #[test]
    fn from_modifiers_は_shift_を_ctrl_より優先する() {
        let cases = [
            (false, false, SelectModifier::Single),
            (false, true, SelectModifier::Toggle),
            (true, false, SelectModifier::RangeFromAnchor),
            (true, true, SelectModifier::RangeFromAnchor),
        ];
        for (shift, ctrl, expected) in cases {
            assert_eq!(
                SelectModifier::from_modifiers(shift, ctrl),
                expected,
                "shift={shift} ctrl={ctrl}"
            );
        }
    }

    #[test]
    fn アンカーは_range_のときだけ据え置き() {
        assert!(SelectModifier::Single.updates_anchor());
        assert!(SelectModifier::Toggle.updates_anchor());
        assert!(!SelectModifier::RangeFromAnchor.updates_anchor());
    }

    #[test]
    fn single_は選択を置換する() {
        let next = SelectModifier::Single.resolve(&[1_u32, 2, 3], 9, || None);
        assert_eq!(next, vec![9]);
    }

    #[test]
    fn toggle_は未選択を足し選択済を外す() {
        let add = SelectModifier::Toggle.resolve(&[1_u32, 2], 3, || None);
        assert_eq!(add, vec![1, 2, 3]);
        let remove = SelectModifier::Toggle.resolve(&[1_u32, 2, 3], 2, || None);
        assert_eq!(remove, vec![1, 3]);
    }

    #[test]
    fn range_はアンカー不在なら_single_に倒れる() {
        let next = SelectModifier::RangeFromAnchor.resolve(&[1_u32, 2], 5, || None);
        assert_eq!(next, vec![5]);
    }

    #[test]
    fn range_block_は行と時間の長方形を返す() {
        // 3 トラック × 4 拍グリッド。 行 = トラック index、 各クリップは 1 拍。
        //   row0: A(0) B(1) C(2) D(3)
        //   row1: E(0) F(1) G(2) H(3)
        //   row2: I(0) J(1) K(2) L(3)
        let items: Vec<RangeItem<u32>> = (0..3)
            .flat_map(|row| {
                (0..4).map(move |col| {
                    item(row * 4 + col, i64::from(row), f64::from(col), f64::from(col) + 1.0)
                })
            })
            .collect();
        // アンカー = B (row0,col1 = key 1)、 clicked = K (row2,col2 = key 10)
        // → row 0..=2 × beat 1..=3 → B C / F G / J K
        let got = range_block(&items, 1, 10).expect("両端が items に居る");
        assert_eq!(got, vec![1, 2, 5, 6, 9, 10]);
    }

    #[test]
    fn range_block_は行が同じなら同じ行だけ返す() {
        let items: Vec<RangeItem<u32>> = (0..3)
            .flat_map(|row| {
                (0..4).map(move |col| {
                    item(row * 4 + col, i64::from(row), f64::from(col), f64::from(col) + 1.0)
                })
            })
            .collect();
        // アンカー = B (key 1)、 clicked = D (key 3) → row0 の B C D のみ
        let got = range_block(&items, 1, 3).expect("両端が items に居る");
        assert_eq!(got, vec![1, 2, 3]);
    }

    #[test]
    fn range_block_は範囲に部分的に重なる長いクリップも含む() {
        // 同じ行の長いクリップ (5..20) は範囲 [0,7] に重なるので入る。
        let items = vec![item(1, 0, 0.0, 1.0), item(2, 0, 5.0, 20.0), item(3, 0, 6.0, 7.0)];
        let got = range_block(&items, 1, 3).expect("両端が items に居る");
        assert_eq!(got, vec![1, 2, 3]);
    }

    #[test]
    fn range_block_は端が接するだけの隣接要素を含めない() {
        // 隣接クリップは端点を共有する ([0,1] と [1,2])。 閉区間で判定すると範囲の
        // すぐ外側の隣接クリップまで毎回 1 つ余計に拾ってしまうので strict で判定する。
        let items = vec![item(0, 0, 0.0, 1.0), item(1, 0, 1.0, 2.0), item(2, 0, 2.0, 3.0)];
        // アンカー = clicked = key 1 ([1,2]) → 自分だけ。 両隣は端が接するのみ。
        assert_eq!(range_block(&items, 1, 1), Some(vec![1]));
    }

    #[test]
    fn range_block_は長さ0の要素でも自分自身を返す() {
        // strict 判定は長さ 0 の区間で空振りするので、 アンカー / clicked は常に含める。
        let items = vec![item(1, 0, 4.0, 4.0), item(2, 0, 9.0, 9.0)];
        assert_eq!(range_block(&items, 1, 1), Some(vec![1]));
        assert_eq!(range_block(&items, 1, 2), Some(vec![1, 2]));
    }

    #[test]
    fn range_block_は範囲外の行を除外する() {
        // ノート想定: row = pitch。 アンカー C4(60) / clicked G4(67) の間に居る
        // pitch だけ拾い、 C5(72) は時間帯が重なっていても除外する。
        let items = vec![
            item(1, 60, 0.0, 1.0),  // a: C4
            item(2, 67, 0.5, 1.5),  // b: G4
            item(3, 72, 0.5, 1.5),  // c: C5 → pitch 範囲外
        ];
        let got = range_block(&items, 1, 2).expect("両端が items に居る");
        assert_eq!(got, vec![1, 2]);
    }

    #[test]
    fn range_block_は片端が見つからなければ_none() {
        let items = vec![item(1, 0, 0.0, 1.0)];
        assert_eq!(range_block(&items, 1, 99), None);
        assert_eq!(range_block(&items, 99, 1), None);
    }

    #[test]
    fn range_ordered_は向きに依らず同じ範囲を返す() {
        let order = [10_u32, 11, 12, 13, 14];
        assert_eq!(range_ordered(&order, 11, 13), Some(vec![11, 12, 13]));
        assert_eq!(range_ordered(&order, 13, 11), Some(vec![11, 12, 13]));
        assert_eq!(range_ordered(&order, 12, 12), Some(vec![12]));
        assert_eq!(range_ordered(&order, 12, 99), None);
    }
}
