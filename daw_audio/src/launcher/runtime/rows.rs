//! ランチャーの行の集合と、走行状態の行との突き合わせ。
//!
//! 行の集合と並びの定義は `Song` と同じ便で届く索引 ([`SongIndex`] の launcher 行) が持つ。**走行状態の行
//! (`LauncherRuntime::rows`) はその並びの先頭から器の容量ぶんに揃える** ([`LauncherRuntime::sync_rows`])。
//! 揃った後は「`i` 番目の走行状態の行 = `i` 番目の行」なので、毎 buffer の突き合わせ
//! ([`LauncherRuntime::for_each_row`] / `build_table`) は位置で組にでき、鍵 → 走行状態の行も索引の二分探索で引ける
//! ([`LauncherRuntime::row_idx`])。行数に上限が無くても行数の二乗にならない (`docs/plan_unbounded_tracks.md` §2.5)。

use common::model::{MASTER_TRACK_ID, ParamStoreAt};
use common::song_index::SongIndex;

use super::*;

impl LauncherRuntime {
    /// 走行状態の行を索引の行の並び (先頭から器の容量ぶん) に揃える: 残った行は走行状態を保って並べ替え、
    /// 消えた行を落とし、増えた行を作る。定常 (行の集合も並びも変わらない) は 1 周の鍵の比較だけ。
    ///
    /// RT 安全: 容量内の push と in-place の並べ替え / swap / truncate だけ (器は song と同じ便で届く)。
    pub(super) fn sync_rows(&mut self, index: &SongIndex) {
        let rows = &mut self.rows;
        let len = index.launcher_row_count().min(rows.capacity());
        let key_at = |i: usize| index.launcher_row_key(i).map(|(track_id, lane_id)| RowKey::lane(track_id, lane_id));
        if rows.len() == len && rows.iter().enumerate().all(|(i, r)| key_at(i) == Some(r.key)) {
            return;
        }
        // 行き先 = 揃えた後の位置。消えた行と器に入らない行は `usize::MAX` (末尾へ寄せて落とす)、同じ鍵の 2 本目以降も落とす。
        let dest = |key: RowKey| {
            index.launcher_row_pos(key.track_id, key.lane_id).filter(|&p| p < len).unwrap_or(usize::MAX)
        };
        rows.sort_unstable_by_key(|r| dest(r.key));
        rows.dedup_by_key(|r| dest(r.key));
        if rows.last().is_some_and(|r| dest(r.key) == usize::MAX) {
            rows.pop();
        }
        // 残った行は行き先の昇順で、どの行も行き先 >= 今の位置。後ろの席から埋めれば、まだ動かしていない行を踏まない。
        let mut kept = rows.len();
        rows.resize_with(len, || RowRuntime::new(RowKey::default()));
        for j in (0..len).rev() {
            if kept > 0 && dest(rows[kept - 1].key) == j {
                kept -= 1;
                rows.swap(kept, j);
            } else if let Some(key) = key_at(j) {
                rows[j] = RowRuntime::new(key);
            }
        }
    }

    /// 走行状態の行と `Song` の行を位置で組にして `f` を呼ぶ ([`Self::sync_rows`] の後)。
    pub(super) fn for_each_row(&mut self, song: SongRef<'_>, mut f: impl FnMut(&mut RowRuntime, RowCells<'_>, RowPlayback)) {
        for (i, row) in self.rows.iter_mut().enumerate() {
            if let Some(r) = song.index.launcher_row(song.song, i).filter(|r| RowKey::lane(r.track_id, r.lane_id) == row.key) {
                f(row, r.cells.into(), r.saved);
            }
        }
    }

    /// 鍵 `key` の走行状態の行の位置 (`rows.iter().position(|r| r.key == key)` と同じ。[`Self::sync_rows`] の後)。
    pub(super) fn row_idx(&self, index: &SongIndex, key: RowKey) -> Option<usize> {
        index
            .launcher_row_pos(key.track_id, key.lane_id)
            .filter(|&i| self.rows.get(i).is_some_and(|r| r.key == key))
    }

    /// `build_table` の 1 行: `key` が走行状態の `cursor` 番目の行ならその index を返してカーソルを進める
    /// (`build_table` はランチャーの行にならないテンポ / 拍子レーンの席も積むので、一致しない key は走行状態に
    /// 居ない行)。
    pub(super) fn take_row(&self, key: RowKey, cursor: &mut usize) -> Option<usize> {
        let idx = *cursor;
        self.rows.get(idx).is_some_and(|r| r.key == key).then(|| {
            *cursor += 1;
            idx
        })
    }
}

/// 行のセル列と、保存されている主導権。
///
/// **RowKey → セルの解決はここが唯一の口**なので、ランチャーが握れない行
/// (テンポ / 拍子レーン、確定済み設計判断 1) の門番もここに置く。`None` を返せば
/// 発火要求も seed も `Arranger` に倒れる。トラックは id で (`Song::tracks` の先頭から最初のもの)、
/// レーンはその置き場の中を id で (`Song::automation_lane_by_key` と同じもの) 引く。
pub(super) fn row_of(song: SongRef<'_>, key: RowKey) -> Option<(RowCells<'_>, RowPlayback)> {
    let SongRef { song, index } = song;
    // トラック行 (`lane_id == 0`)。マスター行はトラックを持たないのでレーン行だけ。
    if key.lane_id == 0 && key.track_id != MASTER_TRACK_ID {
        let track_idx = index.track_pos(key.track_id)?;
        let cells = index.track_cells(song, track_idx)?;
        return Some((cells.into(), song.tracks.get(track_idx)?.launcher));
    }
    // レーン行の置き場は `Song::param_store_at` と同じ規則 (`MASTER_TRACK_ID` = song、`0` はどのトラックでもない)。
    let store = match key.track_id {
        MASTER_TRACK_ID => index.store(song, ParamStoreAt::Song),
        0 => return None,
        track_id => index.track_store(song, index.track_pos(track_id)?),
    };
    let lane = store.lane_by_id(key.lane_id)?;
    lane.lane
        .target
        .accepts_launcher_cells()
        .then(|| (lane.session_cells().into(), lane.lane.launcher))
}
