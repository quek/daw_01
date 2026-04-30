//! taffy ラッパ — chrome (toolbar/panel/dialog) のレイアウト計算に使う。
//!
//! M1 ではフラットな vbox/hbox ヘルパを提供する程度。
//! M3 以降で scenegraph と組み合わせる。

use taffy::prelude::*;
// daw_ui_renderer::Rect は名前衝突するので関数本体でフルパス参照する

/// 1 フレームで使い捨てるレイアウト計算器。
pub struct LayoutPass {
    tree: TaffyTree<()>,
}

impl LayoutPass {
    pub fn new() -> Self {
        Self { tree: TaffyTree::new() }
    }

    /// 子のサイズを与えて leaf node を作る。
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

    /// 子ノード列を flex direction で並べる親ノード。
    pub fn flex(
        &mut self,
        direction: FlexDirection,
        gap: f32,
        padding: f32,
        children: &[NodeId],
    ) -> NodeId {
        self.tree
            .new_with_children(
                Style {
                    display: Display::Flex,
                    flex_direction: direction,
                    gap: Size {
                        width: LengthPercentage::length(gap),
                        height: LengthPercentage::length(gap),
                    },
                    padding: taffy::Rect::<LengthPercentage> {
                        left: LengthPercentage::length(padding),
                        right: LengthPercentage::length(padding),
                        top: LengthPercentage::length(padding),
                        bottom: LengthPercentage::length(padding),
                    },
                    size: Size {
                        width: Dimension::auto(),
                        height: Dimension::auto(),
                    },
                    ..Default::default()
                },
                children,
            )
            .expect("taffy flex")
    }

    /// 与えた利用可能サイズ (物理ピクセル) で root を計算し、各 leaf の絶対座標を返す。
    pub fn compute(
        &mut self,
        root: NodeId,
        avail_width: f32,
        avail_height: f32,
    ) -> Vec<(NodeId, daw_ui_renderer::Rect)> {
        self.tree
            .compute_layout(
                root,
                Size {
                    width: AvailableSpace::Definite(avail_width),
                    height: AvailableSpace::Definite(avail_height),
                },
            )
            .expect("taffy compute");

        let mut out = Vec::new();
        Self::collect(&self.tree, root, 0.0, 0.0, &mut out);
        out
    }

    fn collect(
        tree: &TaffyTree<()>,
        node: NodeId,
        ox: f32,
        oy: f32,
        out: &mut Vec<(NodeId, daw_ui_renderer::Rect)>,
    ) {
        let layout = tree.layout(node).expect("taffy layout");
        let x = ox + layout.location.x;
        let y = oy + layout.location.y;
        out.push((
            node,
            daw_ui_renderer::Rect {
                x,
                y,
                w: layout.size.width,
                h: layout.size.height,
            },
        ));
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
