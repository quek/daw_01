//! 内部 scenegraph データ構造 (M4 Phase 10 で導入)。
//!
//! Phase 11 以降で `Ui::with_widget_node` API から書き込まれ、widget の
//! visual inputs を hash した値の前フレーム比較に使う。一致時は描画スキップ可。
//!
//! Phase 10 では型と HashMap 操作 API のみ。各 widget からの利用は Phase 11 で追加。

use std::collections::{HashMap, HashSet};

use crate::id::WidgetId;

/// per-widget の前フレーム情報。Phase 11 以降で描画コマンド列を追加する想定。
#[derive(Debug, Clone, Copy)]
pub struct SceneNode {
    /// widget の visual inputs を hash した値。
    /// 前フレームの hash と一致 = 描画内容が同一 = キャッシュ再利用可。
    pub input_hash: u64,
}

/// widget ID をキーに前フレームの per-widget 状態を保持する。
///
/// `slotmap::SlotMap<NodeId, SceneNode>` の代わりに `HashMap<WidgetId, _>` を使う。
/// `WidgetId` 自体が `Eq + Hash` を備えた安定キーなので世代管理が不要。
#[derive(Debug, Default)]
pub struct Scenegraph {
    nodes: HashMap<WidgetId, SceneNode>,
}

impl Scenegraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// `wid` の前フレーム hash が `hash` と一致すれば true (描画スキップ可)。
    pub fn unchanged(&self, wid: WidgetId, hash: u64) -> bool {
        self.nodes.get(&wid).is_some_and(|n| n.input_hash == hash)
    }

    /// 今フレームの hash を記録 (次フレームで `unchanged` が一致を返せるように)。
    pub fn record(&mut self, wid: WidgetId, hash: u64) {
        self.nodes.insert(wid, SceneNode { input_hash: hash });
    }

    /// このフレームで `seen` に含まれない widget を eviction。
    /// Phase 11 で `Ui::frame` 末尾から呼ぶ。
    pub fn retain(&mut self, seen: &HashSet<WidgetId>) {
        self.nodes.retain(|wid, _| seen.contains(wid));
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wid(seed: u64) -> WidgetId {
        WidgetId::ROOT.child(seed)
    }

    #[test]
    fn record_then_unchanged_returns_true_for_same_hash() {
        let mut sg = Scenegraph::new();
        let id = wid(1);
        sg.record(id, 0xABCD);
        assert!(sg.unchanged(id, 0xABCD));
    }

    #[test]
    fn unchanged_returns_false_for_different_hash() {
        let mut sg = Scenegraph::new();
        let id = wid(1);
        sg.record(id, 0xABCD);
        assert!(!sg.unchanged(id, 0xBEEF));
    }

    #[test]
    fn unchanged_returns_false_for_unknown_widget() {
        let sg = Scenegraph::new();
        assert!(!sg.unchanged(wid(1), 0));
    }

    #[test]
    fn record_overwrites_existing() {
        let mut sg = Scenegraph::new();
        let id = wid(1);
        sg.record(id, 0xAAAA);
        sg.record(id, 0xBBBB);
        assert!(sg.unchanged(id, 0xBBBB));
        assert!(!sg.unchanged(id, 0xAAAA));
        assert_eq!(sg.len(), 1);
    }

    #[test]
    fn retain_evicts_unseen_widgets() {
        let mut sg = Scenegraph::new();
        let a = wid(1);
        let b = wid(2);
        let c = wid(3);
        sg.record(a, 0);
        sg.record(b, 0);
        sg.record(c, 0);
        assert_eq!(sg.len(), 3);

        let mut seen = HashSet::new();
        seen.insert(a);
        seen.insert(c);
        sg.retain(&seen);

        assert_eq!(sg.len(), 2);
        assert!(sg.unchanged(a, 0));
        assert!(!sg.unchanged(b, 0));
        assert!(sg.unchanged(c, 0));
    }
}
