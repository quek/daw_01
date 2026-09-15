//! グループの入れ子を祖先まで辿る **実効フラグ** — 自分と祖先 group の全部でフラグが立っているか。
//!
//! r.md #131 の「実効的に有効」([`Song::track_effectively_enabled`]) と r.md #130 の「移調に実効的に追従」
//! ([`Song::track_follows_transpose`]) が同じ走査を読む (group で外すと子もまとめて外れ、子自身の値は保つ)。

use std::collections::HashMap;

use super::*;

/// `track_id` から `parent_group_id` を辿り、自分と祖先が全部 `flag` か。
/// 自分が居なければ `false`、途中の親が居なければそこで打ち切る (dangling な親は根と同じ)。
/// 循環は `cap` 回で打ち切る (`track_visually_silenced` の走査と同形)。
fn walk<'a>(track_id: u32, cap: usize, lookup: impl Fn(u32) -> Option<&'a Track>, flag: impl Fn(&Track) -> bool) -> bool {
    let mut cur = Some(track_id);
    let mut hops = 0usize;
    while let Some(id) = cur {
        let Some(t) = lookup(id) else {
            return hops > 0;
        };
        if !flag(t) {
            return false;
        }
        if hops > cap {
            break;
        }
        cur = t.parent_group_id;
        hops += 1;
    }
    true
}

impl Song {
    /// トラック `track_id` の自分と祖先 group の全部で `flag` が立っているか (居なければ `false`)。
    pub(crate) fn lineage_all(&self, track_id: u32, flag: impl Fn(&Track) -> bool) -> bool {
        walk(track_id, self.tracks.len(), |id| self.track_by_id(id), flag)
    }

    /// song-track index 順の [`Self::lineage_all`]。compile / 索引のように全トラックを 1 回で引く口
    /// (トラックごとに線形探索しない)。同じ id が複数あれば先頭を親として引く (`track_by_id` と同じ答え)。
    pub(crate) fn lineage_mask(&self, flag: impl Fn(&Track) -> bool) -> Vec<bool> {
        let mut by_id: HashMap<u32, &Track> = HashMap::with_capacity(self.tracks.len());
        for t in &self.tracks {
            by_id.entry(t.id).or_insert(t);
        }
        let cap = self.tracks.len();
        self.tracks.iter().map(|t| walk(t.id, cap, |id| by_id.get(&id).copied(), &flag)).collect()
    }
}
