//! ランチャーの行の集合と、走行状態の行との突き合わせ。
//!
//! **走行状態の行 (`LauncherRuntime::rows`) は [`for_each_launcher_row`] と同じ並びに揃える**
//! ([`LauncherRuntime::sync_rows`])。毎 buffer の突き合わせ ([`LauncherRuntime::for_each_row`] /
//! `build_table`) は先頭から進むカーソル 1 本で済み、行数に上限が無くても行数の二乗にならない
//! (`docs/plan_unbounded_tracks.md` §2.5)。

use super::*;

impl LauncherRuntime {
    /// `Song` にある行と走行状態を突き合わせる (増えた行を作り、消えた行を落とし、並びを `Song` に揃える)。
    /// 定常 (行の集合も並びも変わらない) は 1 周の key 比較だけ。
    ///
    /// RT 安全: 容量内の push と in-place の rotate / truncate だけ (器は song と同じ便で届く)。
    pub(super) fn sync_rows(&mut self, song: &Song) {
        let rows = &mut self.rows;
        let mut cursor = 0;
        let mut missing = false;
        for_each_launcher_row(song, |key, _, _| match rows[cursor..].iter().position(|r| r.key == key) {
            Some(off) => {
                rows[cursor..=cursor + off].rotate_right(1);
                cursor += 1;
            }
            None => missing = true,
        });
        // `cursor` から後ろは `Song` に無い行 (消えた行)。増えた行を差し込む前に落とす — 器の容量は
        // 曲の行数なので、消えた行を残したままだと同じ便で増えた行が入らない。
        rows.truncate(cursor);
        if !missing {
            return;
        }
        let mut at = 0;
        for_each_launcher_row(song, |key, _, _| {
            if rows.get(at).is_some_and(|r| r.key == key) {
                at += 1;
            } else if rows.len() < rows.capacity() {
                rows.push(RowRuntime::new(key));
                rows[at..].rotate_right(1);
                at += 1;
            }
        });
    }

    /// 走行状態の行と `Song` の行を並び順に組にして `f` を呼ぶ ([`Self::sync_rows`] の後 = 同じ並び)。
    /// 走行状態に居ない行 (器が足りなかった行) は飛ばす。
    pub(super) fn for_each_row(&mut self, song: &Song, mut f: impl FnMut(&mut RowRuntime, RowCells<'_>, RowPlayback)) {
        let rows = &mut self.rows;
        let mut cursor = 0;
        for_each_launcher_row(song, |key, cells, saved| {
            if let Some(row) = rows.get_mut(cursor).filter(|r| r.key == key) {
                f(row, cells, saved);
                cursor += 1;
            }
        });
    }

    /// `build_table` の 1 行: `key` が走行状態の `cursor` 番目の行ならその index を返してカーソルを進める
    /// (`build_table` は [`for_each_launcher_row`] が飛ばすテンポ / 拍子レーンの席も積むので、一致しない
    /// key は走行状態に居ない行)。
    pub(super) fn take_row(&self, key: RowKey, cursor: &mut usize) -> Option<usize> {
        let idx = *cursor;
        self.rows.get(idx).is_some_and(|r| r.key == key).then(|| {
            *cursor += 1;
            idx
        })
    }
}

/// **ランチャーの行を 1 本の規則で数え上げる。**
///
/// 行の登録 ([`LauncherRuntime::sync_rows`])・列の占有判定
/// ([`LauncherRuntime::fill_scene_occupancy`])・列の長さ ([`scene_longest`]) が
/// 同じ集合を見るように、走査はここだけが持つ。集合は計画書 Q4 のとおり
/// 「通常トラック行 + 展開したオートメーションレーン行 + マスター行
/// (`Song.song_lanes`)」で、テンポ / 拍子レーンだけが外れる
/// ([`common::model::AutomationTarget::accepts_launcher_cells`] が SSoT)。`f` には行のセル列と
/// 保存されている主導権 ([`row_of`] と同じ組) も渡す。
///
/// RT 安全: 線形走査のみ (確保・ロック・I/O なし)。
pub(super) fn for_each_launcher_row(song: &Song, mut f: impl FnMut(RowKey, RowCells<'_>, RowPlayback)) {
    for track in &song.tracks {
        f(RowKey::track(track.id), RowCells::Track(&track.session_clips), track.launcher);
        for lane in &track.automation_lanes {
            if lane.target.accepts_launcher_cells() {
                f(RowKey::lane(track.id, lane.id), RowCells::Lane(&lane.session_clips), lane.launcher);
            }
        }
    }
    for lane in &song.song_lanes {
        if lane.target.accepts_launcher_cells() {
            let key = RowKey::lane(common::model::MASTER_TRACK_ID, lane.id);
            f(key, RowCells::Lane(&lane.session_clips), lane.launcher);
        }
    }
}

/// 行のセル列と、保存されている主導権。
///
/// **RowKey → セルの解決はここが唯一の口**なので、ランチャーが握れない行
/// (テンポ / 拍子レーン、確定済み設計判断 1) の門番もここに置く。`None` を返せば
/// 発火要求も seed も `Arranger` に倒れる。1 行を引くのに track を線形に探すので、全行を舐める
/// 毎 buffer の経路は [`LauncherRuntime::for_each_row`] を使う。
pub(super) fn row_of(song: &Song, key: RowKey) -> Option<(RowCells<'_>, RowPlayback)> {
    // トラック行 (`lane_id == 0`)。マスター行はトラックを持たないのでレーン行だけ。
    if key.lane_id == 0 && key.track_id != common::model::MASTER_TRACK_ID {
        let track: &Track = song.tracks.iter().find(|t| t.id == key.track_id)?;
        return Some((RowCells::Track(&track.session_clips), track.launcher));
    }
    // レーン行の置き場 (track か song か) は `automation_lane_by_key` 1 本。
    let lane: &AutomationLane = song.automation_lane_by_key(key.track_id, key.lane_id)?;
    lane.target
        .accepts_launcher_cells()
        .then(|| (RowCells::Lane(&lane.session_clips), lane.launcher))
}
