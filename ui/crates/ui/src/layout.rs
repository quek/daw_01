// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! taffy ラッパ — chrome (toolbar / panel / dialog) のレイアウト計算に使う。
//!
//! 提供する API:
//! - `leaf(w, h)`: 固定サイズの leaf (両軸とも `Dimension::length`)
//! - `leaf_grow(grow)`: 残余空間を `flex_grow` 比率で取る leaf (cross-axis は taffy default = stretch)
//! - `flex(direction, gap, padding, children)`: flex 親。`Gap` で per-axis、`Padding` で per-side
//! - `compute(root, w, h)`: 計算実行 → `(NodeId, Rect)` の絶対座標 vec を返す
//!
//! taffy の型 (`Style` / `Dimension` / `LengthPercentage` 等) は実装詳細で、
//! 公開 API には露出しない (中立化、CLAUDE.md の `WindowBackend` trait 方針と同じ)。

use std::collections::HashMap;

use taffy::prelude::*;
// daw_ui_renderer::Rect は名前衝突するので関数本体でフルパス参照する

// taffy の `FlexDirection` / `NodeId` は `LayoutPass` の API シグネチャに登場するため、
// 利用側 (mixer 等) が taffy を直接依存に入れずに済むよう、ここで再エクスポートする。
// これらは "実装詳細を露出している" と解釈もできるが、`FlexDirection` は典型的な flex 概念
// で wrapping しても情報量が増えないため pragmatic に taffy のものをそのまま使う。
pub use taffy::prelude::{FlexDirection, NodeId};

/// flex 親の per-side padding (px)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Padding {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Padding {
    pub const ZERO: Self = Self { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 };

    /// 4 辺すべてに同じ値を入れる。
    pub fn all(v: f32) -> Self {
        Self { top: v, right: v, bottom: v, left: v }
    }

    /// 水平 (left/right) と垂直 (top/bottom) に別の値を入れる。
    pub fn axis(x: f32, y: f32) -> Self {
        Self { top: y, right: x, bottom: y, left: x }
    }
}

/// flex 親の per-axis gap (px)。`x` は row 方向、`y` は column 方向の隣接子間隔。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Gap {
    pub x: f32,
    pub y: f32,
}

impl Gap {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub fn all(v: f32) -> Self {
        Self { x: v, y: v }
    }

    pub fn xy(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// 1 フレームで使い捨てるレイアウト計算器。
///
/// `compute` / `compute_at` を呼ぶと内部の `rects` に各 NodeId の絶対座標が
/// 入る。利用側は `rect(node)` で O(1) 引きする (HashMap collect 不要)。
pub struct LayoutPass {
    tree: TaffyTree<()>,
    rects: HashMap<NodeId, daw_ui_renderer::Rect>,
}

impl LayoutPass {
    pub fn new() -> Self {
        Self {
            tree: TaffyTree::new(),
            rects: HashMap::new(),
        }
    }

    /// 固定サイズの leaf を作る。両軸とも `width` / `height` ピクセル。
    pub fn leaf(&mut self, width: f32, height: f32) -> NodeId {
        self.tree
            .new_leaf(Style {
                size: Size {
                    width: Dimension::length(width),
                    height: Dimension::length(height),
                },
                ..Default::default()
            })
            .expect("taffy leaf")
    }

    /// flex 親内で残余空間を `grow` の比率で取る leaf。
    /// - cross-axis は taffy default の stretch (= 親のサイズに揃える)
    /// - 同じ flex 親に複数の `leaf_grow` がある場合、grow 値の比率で残余を分配
    /// - `flex_basis = 0` で「基準ゼロ + grow ですべての残余を分配」。固定 leaf と
    ///   混在させた場合は固定の合計を引いた残余を grow で分配する。
    pub fn leaf_grow(&mut self, grow: f32) -> NodeId {
        self.tree
            .new_leaf(Style {
                flex_grow: grow,
                flex_basis: Dimension::length(0.0),
                size: Size { width: Dimension::auto(), height: Dimension::auto() },
                ..Default::default()
            })
            .expect("taffy leaf_grow")
    }

    /// 子ノード列を flex direction で並べる親ノード。
    pub fn flex(
        &mut self,
        direction: FlexDirection,
        gap: Gap,
        padding: Padding,
        children: &[NodeId],
    ) -> NodeId {
        self.tree
            .new_with_children(
                Style {
                    display: Display::Flex,
                    flex_direction: direction,
                    gap: Size {
                        width: LengthPercentage::length(gap.x),
                        height: LengthPercentage::length(gap.y),
                    },
                    padding: taffy::Rect::<LengthPercentage> {
                        left: LengthPercentage::length(padding.left),
                        right: LengthPercentage::length(padding.right),
                        top: LengthPercentage::length(padding.top),
                        bottom: LengthPercentage::length(padding.bottom),
                    },
                    // flex 親を「親の利用可能領域いっぱい」に広げる。これがないと
                    // `flex_basis: 0` の grow 子だけのとき auto = fit-content = 0px になり
                    // 残余分配が起きない。root として `compute(...)` に渡したときは
                    // `AvailableSpace::Definite(W, H)` の 100% = 利用可能領域全体になる。
                    size: Size {
                        width: Dimension::percent(1.0),
                        height: Dimension::percent(1.0),
                    },
                    ..Default::default()
                },
                children,
            )
            .expect("taffy flex")
    }

    /// 与えた利用可能サイズ (物理ピクセル) で root を計算し、各 node の絶対座標を
    /// `self.rects` に格納する。座標は origin (0, 0) 起点。
    /// 各 node の rect は `rect(node)` で取り出す。
    pub fn compute(&mut self, root: NodeId, avail_width: f32, avail_height: f32) {
        self.compute_at(root, avail_width, avail_height, (0.0, 0.0));
    }

    /// `compute` と同じだが、origin (= screen 座標等の左上オフセット) を指定する。
    /// 各 node の rect には `origin.0 + layout.x`, `origin.1 + layout.y` が入る。
    pub fn compute_at(
        &mut self,
        root: NodeId,
        avail_width: f32,
        avail_height: f32,
        origin: (f32, f32),
    ) {
        self.tree
            .compute_layout(
                root,
                Size {
                    width: AvailableSpace::Definite(avail_width),
                    height: AvailableSpace::Definite(avail_height),
                },
            )
            .expect("taffy compute");

        self.rects.clear();
        Self::collect(&self.tree, root, origin.0, origin.1, &mut self.rects);
    }

    /// `compute` / `compute_at` 後に各 node の絶対座標を取得 (O(1))。
    /// 未 compute の node を渡すとパニック (利用側ミスを早期検出)。
    pub fn rect(&self, node: NodeId) -> daw_ui_renderer::Rect {
        *self
            .rects
            .get(&node)
            .expect("LayoutPass::rect: compute されていない NodeId")
    }

    fn collect(
        tree: &TaffyTree<()>,
        node: NodeId,
        ox: f32,
        oy: f32,
        out: &mut HashMap<NodeId, daw_ui_renderer::Rect>,
    ) {
        let layout = tree.layout(node).expect("taffy layout");
        let x = ox + layout.location.x;
        let y = oy + layout.location.y;
        out.insert(
            node,
            daw_ui_renderer::Rect {
                x,
                y,
                w: layout.size.width,
                h: layout.size.height,
            },
        );
        for child in tree.children(node).expect("taffy children") {
            Self::collect(tree, child, x, y, out);
        }
    }
}

impl Default for LayoutPass {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    //! `LayoutPass` の双方向挙動テスト:
    //! - per-side padding / per-axis gap / fixed leaf / flex_grow が taffy 経由で
    //!   期待通り計算されること
    //! - `compute_at` の origin offset が正しく適用されること
    //!
    //! テストは `compute` 後に `rect(node)` で個別に rect を引いて assertion する。

    use super::*;

    /// fixed leaf 2 つを column flex に積んで、`Gap::all(10)` で間隔が 10px 開く。
    #[test]
    fn fixed_leaves_in_column_flex_stack_with_gap() {
        let mut p = LayoutPass::new();
        let a = p.leaf(50.0, 30.0);
        let b = p.leaf(50.0, 30.0);
        let root = p.flex(FlexDirection::Column, Gap::all(10.0), Padding::ZERO, &[a, b]);

        p.compute(root, 200.0, 200.0);
        assert!((p.rect(a).y - 0.0).abs() < 0.5, "child A y: {}", p.rect(a).y);
        assert!((p.rect(a).h - 30.0).abs() < 0.5);
        // child B は y=30+10=40
        assert!((p.rect(b).y - 40.0).abs() < 0.5, "child B y: {}", p.rect(b).y);
        assert!((p.rect(b).h - 30.0).abs() < 0.5);
    }

    /// `Padding { top: 5, right: 10, bottom: 15, left: 20 }` で子の起点が
    /// (left, top) = (20, 5) からになる。
    #[test]
    fn per_side_padding_offsets_children() {
        let mut p = LayoutPass::new();
        let a = p.leaf(50.0, 30.0);
        let pad = Padding { top: 5.0, right: 10.0, bottom: 15.0, left: 20.0 };
        let root = p.flex(FlexDirection::Column, Gap::ZERO, pad, &[a]);

        p.compute(root, 200.0, 200.0);
        assert!((p.rect(a).x - 20.0).abs() < 0.5, "child x: {}", p.rect(a).x);
        assert!((p.rect(a).y - 5.0).abs() < 0.5, "child y: {}", p.rect(a).y);
    }

    /// column flex で `Gap::xy(99, 8)` → main axis (= y) は 8px 間隔、
    /// cross axis (= x) の 99 は単独の column では子間距離として現れない。
    #[test]
    fn per_axis_gap_only_applies_main_axis() {
        let mut p = LayoutPass::new();
        let a = p.leaf(50.0, 30.0);
        let b = p.leaf(50.0, 30.0);
        let root = p.flex(FlexDirection::Column, Gap::xy(99.0, 8.0), Padding::ZERO, &[a, b]);

        p.compute(root, 200.0, 200.0);
        // main axis 間隔は y で 8
        let dy = p.rect(b).y - (p.rect(a).y + p.rect(a).h);
        assert!((dy - 8.0).abs() < 0.5, "main axis gap (y): {dy}");
        // cross axis 99 は同じ列の子なので無関係 (両者とも x=0 で並ぶ)
        assert!(
            (p.rect(a).x - p.rect(b).x).abs() < 0.5,
            "cross axis: a.x={}, b.x={}", p.rect(a).x, p.rect(b).x
        );
    }

    /// `leaf_grow(2.0)` + `leaf_grow(1.0)` を高さ 90 の column flex に → 2:1 で 60:30。
    #[test]
    fn leaf_grow_distributes_remaining_in_2_to_1() {
        let mut p = LayoutPass::new();
        let a = p.leaf_grow(2.0);
        let b = p.leaf_grow(1.0);
        let root = p.flex(FlexDirection::Column, Gap::ZERO, Padding::ZERO, &[a, b]);

        p.compute(root, 100.0, 90.0);
        assert!((p.rect(a).h - 60.0).abs() < 0.5, "child A h: {}", p.rect(a).h);
        assert!((p.rect(b).h - 30.0).abs() < 0.5, "child B h: {}", p.rect(b).h);
    }

    /// fixed (h=20) + grow を高さ 100 の column → fixed=20、grow=80。
    #[test]
    fn fixed_and_grow_combo_in_column() {
        let mut p = LayoutPass::new();
        let fixed = p.leaf(50.0, 20.0);
        let grow = p.leaf_grow(1.0);
        let root = p.flex(FlexDirection::Column, Gap::ZERO, Padding::ZERO, &[fixed, grow]);

        p.compute(root, 100.0, 100.0);
        assert!((p.rect(fixed).h - 20.0).abs() < 0.5, "fixed h: {}", p.rect(fixed).h);
        assert!((p.rect(grow).h - 80.0).abs() < 0.5, "grow h: {}", p.rect(grow).h);
    }

    /// 外形 100×100 の column flex に `Padding::all(10)` + `leaf_grow(1.0)` →
    /// grow 子は (10, 10) から 80×80 を取る。
    #[test]
    fn padding_shrinks_grow_area() {
        let mut p = LayoutPass::new();
        let g = p.leaf_grow(1.0);
        let root = p.flex(FlexDirection::Column, Gap::ZERO, Padding::all(10.0), &[g]);

        p.compute(root, 100.0, 100.0);
        assert!((p.rect(g).x - 10.0).abs() < 0.5, "grow x: {}", p.rect(g).x);
        assert!((p.rect(g).y - 10.0).abs() < 0.5, "grow y: {}", p.rect(g).y);
        assert!((p.rect(g).w - 80.0).abs() < 0.5, "grow w: {}", p.rect(g).w);
        assert!((p.rect(g).h - 80.0).abs() < 0.5, "grow h: {}", p.rect(g).h);
    }

    /// `compute_at(origin)` で全 rect の位置に origin が加算される。
    #[test]
    fn compute_at_applies_origin_offset() {
        let mut p = LayoutPass::new();
        let a = p.leaf(50.0, 30.0);
        let root = p.flex(FlexDirection::Column, Gap::ZERO, Padding::ZERO, &[a]);

        p.compute_at(root, 100.0, 100.0, (10.0, 20.0));
        assert!((p.rect(a).x - 10.0).abs() < 0.5, "child x: {}", p.rect(a).x);
        assert!((p.rect(a).y - 20.0).abs() < 0.5, "child y: {}", p.rect(a).y);
    }
}
