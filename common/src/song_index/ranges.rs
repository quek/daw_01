//! 区間の集合の索引 (off-RT で作り、RT は確保なしで引く)。

use std::collections::BTreeSet;

/// 半開区間 `[start, end)` の集合で「点を覆う区間のうち **元の並びで最初のもの**」を `O(log n)` で引く。
///
/// 覆う区間の集合は区間の端点の間では変わらないので、端点で割った小区間ごとに答えを先に求めておく。
/// 規則は線形探索 `iter().position(|(s, e)| s <= x && x < e)` と完全に同じ (`start < end` でない区間は何も覆わない)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CoverIndex {
    /// 端点 (昇順・重複なし)。
    bounds: Vec<f64>,
    /// `first[k]` = `[bounds[k], bounds[k + 1])` を覆う最初の区間の位置 (`u32::MAX` = 無し)。
    first: Vec<u32>,
}

impl CoverIndex {
    #[must_use]
    pub fn build(intervals: impl IntoIterator<Item = (f64, f64)>) -> Self {
        let items: Vec<(u32, f64, f64)> = intervals
            .into_iter()
            .enumerate()
            .filter(|(_, (s, e))| s < e)
            .map(|(i, (s, e))| (u32::try_from(i).unwrap_or(u32::MAX), s, e))
            .collect();
        let mut bounds: Vec<f64> = items.iter().flat_map(|&(_, s, e)| [s, e]).collect();
        bounds.sort_by(f64::total_cmp);
        bounds.dedup();
        let mut starts: Vec<(f64, u32)> = items.iter().map(|&(i, s, _)| (s, i)).collect();
        starts.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut ends: Vec<(f64, u32)> = items.iter().map(|&(i, _, e)| (e, i)).collect();
        ends.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (mut si, mut ei) = (0, 0);
        let mut active = BTreeSet::new();
        let mut first = Vec::with_capacity(bounds.len());
        for &b in &bounds {
            // 端点 `b` で始まる区間を足し、`b` で終わる区間を外す (`[b, 次の端点)` を覆う集合)。
            while let Some(&(_, i)) = starts.get(si).filter(|(s, _)| *s <= b) {
                active.insert(i);
                si += 1;
            }
            while let Some(&(_, i)) = ends.get(ei).filter(|(e, _)| *e <= b) {
                active.remove(&i);
                ei += 1;
            }
            first.push(active.first().copied().unwrap_or(u32::MAX));
        }
        Self { bounds, first }
    }

    /// `x` を覆う区間のうち元の並びで最初のもの。
    #[must_use]
    pub fn first_covering(&self, x: f64) -> Option<usize> {
        let k = self.bounds.partition_point(|b| *b <= x).checked_sub(1)?;
        let i = *self.first.get(k)?;
        (i != u32::MAX).then_some(i as usize)
    }
}

/// 並んだ要素の終点の最大値の木。「先頭 `before` 個のうち終点が条件を満たす要素」を並び順に、**満たす要素の数に比例する
/// 手間で** 辿る — 先頭からの最大値で飛ばす形だと、早い長い要素が 1 つあるだけで以降の終わった要素を毎回全部舐める。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EndTree {
    n_items: usize,
    /// 葉の数 (2 冪、要素が無ければ 0)。
    leaves: usize,
    /// `nodes[1]` が根、`nodes[leaves + k]` が要素 `k` の終点 (NaN は +∞ = 満たす側、埋め草は -∞)、内側は子の最大値。
    nodes: Vec<f64>,
}

impl EndTree {
    /// `ends[k]` = 要素 `k` の終点 (off-RT)。
    #[must_use]
    pub fn build(ends: &[f64]) -> Self {
        if ends.is_empty() {
            return Self::default();
        }
        let leaves = ends.len().next_power_of_two();
        let mut nodes = vec![f64::NEG_INFINITY; 2 * leaves];
        for (k, &e) in ends.iter().enumerate() {
            nodes[leaves + k] = if e.is_nan() { f64::INFINITY } else { e };
        }
        for i in (1..leaves).rev() {
            nodes[i] = nodes[2 * i].max(nodes[2 * i + 1]);
        }
        Self { n_items: ends.len(), leaves, nodes }
    }

    /// 要素 `k < before` のうち `reaches(終点)` を満たすものの `k` を昇順に返す。`reaches` は終点について単調 (大きい
    /// ほど満たす) であること。確保しない (RT 可)。
    pub fn reaching<P: Fn(f64) -> bool>(&self, before: usize, reaches: P) -> Reaching<'_, P> {
        let mut stack = [0u32; 64];
        let sp = usize::from(self.leaves > 0);
        stack[0] = 1;
        Reaching { tree: self, before: before.min(self.n_items), reaches, stack, sp }
    }
}

/// [`EndTree::reaching`] の走査 (深さ優先・左から、作業領域は木の深さぶんの固定長)。
pub struct Reaching<'a, P> {
    tree: &'a EndTree,
    before: usize,
    reaches: P,
    stack: [u32; 64],
    sp: usize,
}

impl<P: Fn(f64) -> bool> Iterator for Reaching<'_, P> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        while self.sp > 0 {
            self.sp -= 1;
            let node = self.stack[self.sp] as usize;
            let level = node.ilog2();
            let first = (node - (1 << level)) * (self.tree.leaves >> level);
            if first >= self.before || !(self.reaches)(self.tree.nodes[node]) {
                continue;
            }
            if node >= self.tree.leaves {
                return Some(node - self.tree.leaves);
            }
            // 右を先に積む (左の部分木を先に出す = 昇順)。積む数は深さ + 1 を超えない。
            self.stack[self.sp] = u32::try_from(2 * node + 1).unwrap_or(u32::MAX);
            self.stack[self.sp + 1] = u32::try_from(2 * node).unwrap_or(u32::MAX);
            self.sp += 2;
        }
        None
    }
}

/// 区間の集合から、範囲 `[lo, hi]` に掛かりうる要素を **元の並び順で** 引く。
///
/// 絞り込みは保守的 (掛かる要素を落とさない) で、呼び出し側は今までと同じ判定をそのまま掛ける。元の並び順で渡すので、
/// 並び順に依存する出力 (同じ frame のイベントの順) も変わらない。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RangeIndex {
    /// 作ったときの要素数 (別の snapshot の列と組まれていないかの照合)。
    n_items: usize,
    /// 始点の昇順に並べた (始点, 元の位置) と、その並びの終点の木 (終点の NaN は +∞ = 掛かりうる側)。
    starts: Vec<f64>,
    order: Vec<u32>,
    ends: EndTree,
}

impl RangeIndex {
    /// `intervals` の `(始点, 終点)`。始点が NaN の要素は何にも掛からないものとして外す。
    #[must_use]
    pub fn build(intervals: impl IntoIterator<Item = (f64, f64)>) -> Self {
        let mut n_items = 0;
        let mut items: Vec<(f64, f64, u32)> = intervals
            .into_iter()
            .inspect(|_| n_items += 1)
            .enumerate()
            .filter(|(_, (s, _))| !s.is_nan())
            .map(|(i, (s, e))| (s, e, u32::try_from(i).unwrap_or(u32::MAX)))
            .collect();
        items.sort_by(|a, b| a.0.total_cmp(&b.0));
        let ends: Vec<f64> = items.iter().map(|x| x.1).collect();
        Self {
            n_items,
            starts: items.iter().map(|x| x.0).collect(),
            order: items.iter().map(|x| x.2).collect(),
            ends: EndTree::build(&ends),
        }
    }

    /// `len` 個の列から作った索引か。
    #[must_use]
    pub fn built_for(&self, len: usize) -> bool {
        self.n_items == len
    }

    /// `[lo, hi]` に掛かりうる要素の元の位置を `out` に昇順で書き、個数を返す。`out` に収まらない / 範囲が NaN なら
    /// `None` (呼び出し側は全件を元の並びで見る)。確保しない。
    pub fn overlapping(&self, lo: f64, hi: f64, out: &mut [u32]) -> Option<usize> {
        if lo.is_nan() || hi.is_nan() {
            return None;
        }
        let last = self.starts.partition_point(|s| *s <= hi);
        let mut n = 0;
        for k in self.ends.reaching(last, |end| end >= lo) {
            *out.get_mut(n)? = self.order[k];
            n += 1;
        }
        out[..n].sort_unstable();
        Some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 線形探索と同じ答え (重なり・長さ 0・負の長さ・NaN・端点ちょうど)。
    #[test]
    fn cover_index_は線形探索と同じ区間を返す() {
        let iv = [(0.0, 4.0), (2.0, 6.0), (6.0, 6.0), (8.0, 7.0), (f64::NAN, 9.0), (6.0, 10.0), (-1.0, 0.0)];
        let index = CoverIndex::build(iv);
        let linear = |x: f64| iv.iter().position(|&(s, e)| s < e && s <= x && x < e);
        for x in [-2.0, -1.0, -0.0, 0.0, 1.0, 2.0, 3.999, 4.0, 5.0, 6.0, 7.5, 9.999, 10.0, f64::NAN] {
            assert_eq!(index.first_covering(x), linear(x), "x={x}");
        }
    }

    /// 終点の木は「先頭 `before` 個のうち終点が条件を満たすもの」を線形探索と同じ順で返す (早い長い要素・NaN・埋め草)。
    #[test]
    fn end_tree_は条件を満たす要素を昇順に返す() {
        let ends = [100.0, 1.0, 2.0, f64::NAN, 5.0, 0.5, 7.0, 3.0, f64::NEG_INFINITY, 4.0];
        let tree = EndTree::build(&ends);
        for before in 0..=ends.len() + 2 {
            for lo in [f64::NEG_INFINITY, 0.0, 1.0, 2.5, 4.0, 6.0, 100.0, 200.0] {
                let got: Vec<usize> = tree.reaching(before, |e| e >= lo).collect();
                let want: Vec<usize> = (0..before.min(ends.len())).filter(|&k| ends[k].is_nan() || ends[k] >= lo).collect();
                assert_eq!(got, want, "before={before} lo={lo}");
            }
        }
        assert_eq!(EndTree::build(&[]).reaching(3, |_| true).count(), 0);
    }

    /// 範囲に掛かる要素を落とさず、元の並び順で返す。収まらなければ None。
    #[test]
    fn range_index_は掛かる要素を元の並びで返す() {
        let iv = [(5.0, 6.0), (0.0, 10.0), (2.0, 3.0), (7.0, f64::NAN), (f64::NAN, 1.0), (4.0, 4.0)];
        let index = RangeIndex::build(iv);
        let mut buf = [0u32; 8];
        let n = index.overlapping(3.5, 4.5, &mut buf).unwrap();
        assert_eq!(&buf[..n], &[1, 5]);
        let n = index.overlapping(2.5, 7.0, &mut buf).unwrap();
        assert_eq!(&buf[..n], &[0, 1, 2, 3, 5]);
        let mut small = [0u32; 1];
        assert_eq!(index.overlapping(0.0, 100.0, &mut small), None);
    }
}
