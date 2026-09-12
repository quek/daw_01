//! `docs/plan_project_tabs.md` §5.1: タブ (= 開いているプロジェクト) の集合。
//!
//! **いま見えているタブは `AppData::cur`** に居て、それ以外は [`Tabs::parked`] に居る。
//! handler / view は全部 `self.cur.*` を触るので、「アクティブなタブ」以外を扱うのは
//! [`crate::app::AppData::with_project`] (対象を一時的に `cur` へ swap して閉包を回し、
//! 戻す) だけ。ReaScript の「current project」と同じモデル — 背景タブ宛の IPC event
//! (`SlotPluginLoaded` / `ExportWavComplete` / 合成状態 ...) はこの経路で配る。
//!
//! 並び ([`Tabs::order`]) は表示順で、`cur.key` も含む。`ProjectKey` は単調増加で
//! 再利用しない (閉じたタブへの遅延 event が新しいタブに誤配送されない)。

use common::audio_bridge::MAX_PROJECTS;
use common::protocol::ProjectKey;

use crate::state::ProjectState;

pub struct Tabs {
    /// アクティブでないタブ (表示順は `order` が持つ)。
    pub parked: Vec<ProjectState>,
    /// タブ帯の並び (`cur.key` を含む全 key)。
    pub order: Vec<ProjectKey>,
    /// 次に採番する `ProjectKey` (1 始まり、単調増加)。
    next_key: u64,
    /// `AppData::with_project` が背景タブを `cur` へ swap している間、本当にアクティブな
    /// タブ (戻し先)。swap 中にそのタブを閉じる経路 (`close_tab_now`) が「隣」ではなく
    /// ここへ戻るために読む。swap の外では `None`。
    pub(crate) visiting_from: Option<ProjectKey>,
}

impl Tabs {
    /// 最初のタブ (`first`) だけを持つ状態。
    #[must_use]
    pub fn new(first: ProjectKey) -> Self {
        Self {
            parked: Vec::new(),
            order: vec![first],
            next_key: first.0 + 1,
            visiting_from: None,
        }
    }

    /// 新しいタブの住所を採番する (再利用しない)。
    pub fn mint(&mut self) -> ProjectKey {
        let key = ProjectKey(self.next_key);
        self.next_key += 1;
        key
    }

    /// 開いているタブの数 (`cur` を含む)。
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// これ以上タブを開けるか (`MAX_PROJECTS`)。
    #[must_use]
    pub fn can_open_more(&self) -> bool {
        self.order.len() < MAX_PROJECTS
    }

    /// `key` の表示位置。
    #[must_use]
    pub fn index_of(&self, key: ProjectKey) -> Option<usize> {
        self.order.iter().position(|k| *k == key)
    }

    /// parked の中の `key` (アクティブなタブは含まない)。
    #[must_use]
    pub fn parked_mut(&mut self, key: ProjectKey) -> Option<&mut ProjectState> {
        self.parked.iter_mut().find(|p| p.key == key)
    }

    #[must_use]
    pub fn parked_ref(&self, key: ProjectKey) -> Option<&ProjectState> {
        self.parked.iter().find(|p| p.key == key)
    }

    /// `cur` と parked の `key` を入れ替える。`key` が parked に居なければ何もしない
    /// (`cur.key == key` も含む)。戻り値 = 入れ替えたか。
    pub fn swap_in(&mut self, cur: &mut ProjectState, key: ProjectKey) -> bool {
        let Some(i) = self.parked.iter().position(|p| p.key == key) else {
            return false;
        };
        std::mem::swap(cur, &mut self.parked[i]);
        true
    }

    /// `order` の中で `key` を `to` 番目へ動かす (ドラッグ並べ替え)。
    pub fn move_to(&mut self, key: ProjectKey, to: usize) {
        let Some(from) = self.index_of(key) else { return };
        let k = self.order.remove(from);
        let to = to.min(self.order.len());
        self.order.insert(to, k);
    }

    /// `key` の次 / 前のタブ (端で巡回)。タブが 1 つなら `None`。
    #[must_use]
    pub fn neighbor(&self, key: ProjectKey, forward: bool) -> Option<ProjectKey> {
        let n = self.order.len();
        if n < 2 {
            return None;
        }
        let i = self.index_of(key)?;
        let j = if forward { (i + 1) % n } else { (i + n - 1) % n };
        Some(self.order[j])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 採番は単調で並びと近傍が巡回する() {
        let mut t = Tabs::new(ProjectKey(1));
        let b = t.mint();
        let c = t.mint();
        assert_eq!((b, c), (ProjectKey(2), ProjectKey(3)));
        t.order.push(b);
        t.order.push(c);
        assert_eq!(t.neighbor(ProjectKey(1), true), Some(b));
        assert_eq!(t.neighbor(c, true), Some(ProjectKey(1)), "端で巡回");
        assert_eq!(t.neighbor(ProjectKey(1), false), Some(c));
        t.move_to(c, 0);
        assert_eq!(t.order, vec![c, ProjectKey(1), b]);
        t.move_to(ProjectKey(1), 99);
        assert_eq!(t.order, vec![c, b, ProjectKey(1)], "末尾へ clamp");
        assert_eq!(Tabs::new(ProjectKey(7)).neighbor(ProjectKey(7), true), None);
    }
}
