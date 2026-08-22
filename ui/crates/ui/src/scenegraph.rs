// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! 内部 scenegraph データ構造 (M4 Phase 10-11)。
//!
//! Phase 10: `Scenegraph` / `SceneNode` / `record` / `unchanged` の API を導入。
//! Phase 11: `SceneNode` に描画コマンドキャッシュを乗せ、`get_cached` で取り出せるように。
//! Phase 11: `Ui::with_widget_node(wid, input_hash, draw_fn)` API がこれを利用する。
//!
//! `slotmap::SlotMap<NodeId, SceneNode>` の代わりに `HashMap<WidgetId, _>` を使う。
//! `WidgetId` 自体が `Eq + Hash` を備えた安定キーなので世代管理が不要。

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use daw_ui_renderer::Primitive;

use crate::id::WidgetId;

/// per-widget の前フレーム描画コマンド (M9 Phase 45f: rect/glyph/line を call order で
/// 並べた `Vec<Primitive>` に統一)。
/// `Ui::with_widget_node` がキャッシュ命中時に scene 末尾に append する素材。
#[derive(Debug, Clone, Default)]
pub struct CachedCommands {
    pub primitives: Vec<Primitive>,
}

/// per-widget の前フレーム情報。input_hash 一致 = 描画変化なし = `commands` を再利用可。
#[derive(Debug, Clone)]
pub struct SceneNode {
    /// widget の visual inputs を hash した値。
    pub input_hash: u64,
    /// 前フレームに記録した描画コマンド。
    pub commands: CachedCommands,
}

/// widget ID をキーに前フレームの per-widget 状態を保持する。
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

    /// hash 一致時の cached commands を返す。一致しないか未登録なら None。
    pub fn get_cached(&self, wid: WidgetId, input_hash: u64) -> Option<&CachedCommands> {
        self.nodes
            .get(&wid)
            .filter(|n| n.input_hash == input_hash)
            .map(|n| &n.commands)
    }

    /// 今フレームの hash と描画コマンドを記録 (次フレームで `unchanged` / `get_cached` が使う)。
    pub fn record(&mut self, wid: WidgetId, input_hash: u64, commands: CachedCommands) {
        self.nodes.insert(wid, SceneNode { input_hash, commands });
    }

    /// このフレームで `seen` に含まれない widget を eviction。
    /// `Ui::frame` 末尾から呼ぶ。
    pub fn retain(&mut self, seen: &HashSet<WidgetId>) {
        self.nodes.retain(|wid, _| seen.contains(wid));
    }

    /// 全 entry を破棄して、次フレームを **全 widget cache miss** から始める。
    ///
    /// 用途は GPU 資産の作り直し (device lost からの復旧、daw_01 r.md #42)。
    /// キャッシュ済みの描画コマンドには `TextureHandle` が焼き込まれており、
    /// GPU 再生成後はそれらが全部無効になる。捨てずに再送すると「前フレームと同じ
    /// 絵のはずが中身だけ空」という無言の描画崩れになるので、まとめて捨てる。
    pub fn clear(&mut self) {
        self.nodes.clear();
    }

    /// `wid` が前フレームに登場していた (= `record` 後 `retain` で残った) かを返す。
    /// M11 Phase 52: `text_input_at_focused` が「初回 show」判定に使う。
    /// frame 末尾の `retain` で eviction されるため、このフレーム途中で呼んだとき
    /// `true` ⇔ 「前フレームに `with_widget_node` で描画された」。
    pub fn contains(&self, wid: WidgetId) -> bool {
        self.nodes.contains_key(&wid)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// 任意の `Hash` 入力を `u64` ハッシュにする共通ヘルパ。各 widget の
/// `input_hash` 計算で使う。`(b"fader", rect.x.to_bits(), ...)` のような tuple を
/// 渡す形を想定。
pub fn hash_inputs<T: Hash>(inputs: T) -> u64 {
    let mut h = DefaultHasher::new();
    inputs.hash(&mut h);
    h.finish()
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
        sg.record(id, 0xABCD, CachedCommands::default());
        assert!(sg.unchanged(id, 0xABCD));
    }

    #[test]
    fn unchanged_returns_false_for_different_hash() {
        let mut sg = Scenegraph::new();
        let id = wid(1);
        sg.record(id, 0xABCD, CachedCommands::default());
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
        sg.record(id, 0xAAAA, CachedCommands::default());
        sg.record(id, 0xBBBB, CachedCommands::default());
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
        sg.record(a, 0, CachedCommands::default());
        sg.record(b, 0, CachedCommands::default());
        sg.record(c, 0, CachedCommands::default());
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

    #[test]
    fn get_cached_returns_commands_when_hash_matches() {
        let mut sg = Scenegraph::new();
        let id = wid(1);
        let cmds = CachedCommands::default();
        sg.record(id, 0xABCD, cmds.clone());
        assert!(sg.get_cached(id, 0xABCD).is_some());
        assert!(sg.get_cached(id, 0xBEEF).is_none());
    }

    #[test]
    fn hash_inputs_is_deterministic() {
        let a = hash_inputs((b"fader", 1u32, 2.5f32.to_bits(), true));
        let b = hash_inputs((b"fader", 1u32, 2.5f32.to_bits(), true));
        assert_eq!(a, b);
        let c = hash_inputs((b"fader", 1u32, 2.5f32.to_bits(), false));
        assert_ne!(a, c);
    }
}
