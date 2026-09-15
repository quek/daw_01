//! ランチャーの行と、行のセル列 (`session_clips`) の索引。
//!
//! **行の集合と並びの定義はここだけ** (`docs/plan_rmd_87_clip_launcher.md` Q4): トラックごとに
//! トラック行 → そのトラックのオートメーションレーン行、最後にマスター行 (`Song.song_lanes`)。テンポ / 拍子レーンだけが
//! 外れる ([`AutomationTarget::accepts_launcher_cells`](crate::model::AutomationTarget::accepts_launcher_cells) が SSoT)。

use std::collections::HashMap;

use super::params::position;
use super::{SongIndex, id_positions, lookup};
use crate::model::{MASTER_TRACK_ID, ParamStoreAt, RowPlayback, SessionAutomationClip, SessionClip, Song};

/// 1 行のセル列の索引。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CellIndex {
    /// 作ったときのセルの本数 (別の snapshot と組まれていないかの照合)。
    n_cells: usize,
    /// `(clip id, 位置)` / `(scene id, 位置)` を id 順に (同じ id は先頭だけ)。
    by_clip: Vec<(u32, u32)>,
    by_scene: Vec<(u32, u32)>,
}

/// 空のセル列の索引 (`Vec::new` は確保しないので static に置ける)。
static EMPTY_CELLS: CellIndex = CellIndex { n_cells: 0, by_clip: Vec::new(), by_scene: Vec::new() };

impl CellIndex {
    /// `(clip id, scene id)` の並びから作る (off-RT)。
    pub(super) fn build(cells: impl Iterator<Item = (u32, u32)> + Clone) -> Self {
        Self {
            n_cells: cells.clone().count(),
            by_clip: id_positions(cells.clone().map(|(clip, _)| clip)),
            by_scene: id_positions(cells.map(|(_, scene)| scene)),
        }
    }

    /// `n_cells` 本のセル列の索引として使えるなら自身、使えなければ (別の snapshot) 空。
    fn checked(&self, n_cells: usize) -> &Self {
        if self.n_cells == n_cells { self } else { &EMPTY_CELLS }
    }
}

/// 行のセル列そのもの。トラック行とレーン行で型が違うだけで規則は同じ (Q4)。
#[derive(Debug, Clone, Copy)]
pub enum CellSlice<'a> {
    Track(&'a [SessionClip]),
    Lane(&'a [SessionAutomationClip]),
}

impl CellSlice<'_> {
    fn len(self) -> usize {
        match self {
            Self::Track(v) => v.len(),
            Self::Lane(v) => v.len(),
        }
    }
}

/// 行のセル列と、その索引の組 (RT 可、確保なし)。
#[derive(Debug, Clone, Copy)]
pub struct SessionCells<'a> {
    pub cells: CellSlice<'a>,
    index: &'a CellIndex,
}

impl<'a> SessionCells<'a> {
    pub(super) fn new(cells: CellSlice<'a>, index: &'a CellIndex) -> Self {
        Self { cells, index: index.checked(cells.len()) }
    }

    /// `clip.id` のセルの位置 (`iter().position(|c| c.clip.id == clip_id)` と同じ)。
    #[must_use]
    pub fn clip_pos(self, clip_id: u32) -> Option<usize> {
        lookup(&self.index.by_clip, clip_id)
    }

    /// 列 `scene_id` のセルの位置 (`iter().position(|c| c.scene_id == scene_id)` と同じ)。
    #[must_use]
    pub fn scene_pos(self, scene_id: u32) -> Option<usize> {
        lookup(&self.index.by_scene, scene_id)
    }

    /// 位置 `pos` のセルの長さ (拍)。
    fn length_at(self, pos: usize) -> Option<f64> {
        match self.cells {
            CellSlice::Track(v) => v.get(pos).map(|c| c.clip.length_beats),
            CellSlice::Lane(v) => v.get(pos).map(|c| c.clip.length_beats),
        }
    }
}

/// ランチャーの行 1 本の置き場。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAt {
    Track(u32),
    /// `(トラックの位置, レーンの位置)`。
    TrackLane(u32, u32),
    SongLane(u32),
}

/// ランチャーの行 1 本 (鍵 + セル列 + 保存されている主導権)。
#[derive(Debug, Clone, Copy)]
pub struct LauncherRow<'a> {
    pub track_id: u32,
    /// `0` = トラック行。
    pub lane_id: u32,
    pub cells: SessionCells<'a>,
    pub saved: RowPlayback,
}

/// ランチャーの行の索引。
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct LauncherIndex {
    /// 行の並び (`((track id, lane id), 置き場)`)。
    rows: Vec<((u32, u32), RowAt)>,
    /// `((track id, lane id), 行の位置)` を鍵順に (同じ鍵は先頭だけ)。
    row_pos: Vec<((u32, u32), u32)>,
    /// `(scene id, その列のセルの最長の長さ)` を id 順に。数えるのは行ごとにその列の先頭のセル
    /// (`find_by_scene` と同じ)、畳み込みは 0 から行の並び順。セルが 1 つも無い列は載せない。
    scenes: Vec<(u32, f64)>,
}

impl LauncherIndex {
    /// `index` のセル列の索引が揃った後で作る (off-RT)。
    ///
    /// r.md #131: 実効的に無効なトラックの行 (トラック行とそのレーン行) は **行の集合に入れない** — 走行状態の行も
    /// 作られず (`sync_rows` が落とす)、列を撃っても掴まず、フォローアクションも回らず、publish もされない。
    /// 列の最長のセルにも数えない。
    pub(super) fn build(song: &Song, index: &SongIndex) -> Self {
        let mut rows = Vec::new();
        for (ti, track) in song.tracks.iter().enumerate() {
            if !index.tracks.get(ti).is_some_and(|t| t.enabled) {
                continue;
            }
            rows.push(((track.id, 0), RowAt::Track(position(ti))));
            for (li, lane) in track.automation_lanes.iter().enumerate() {
                if lane.target.accepts_launcher_cells() {
                    rows.push(((track.id, lane.id), RowAt::TrackLane(position(ti), position(li))));
                }
            }
        }
        for (li, lane) in song.song_lanes.iter().enumerate() {
            if lane.target.accepts_launcher_cells() {
                rows.push(((MASTER_TRACK_ID, lane.id), RowAt::SongLane(position(li))));
            }
        }
        let row_pos = id_positions(rows.iter().map(|(key, _)| *key));

        let mut longest: HashMap<u32, f64> = HashMap::new();
        for &(_, at) in &rows {
            let Some(cells) = index.cells_at(song, at) else { continue };
            for &(scene_id, pos) in &cells.index.by_scene {
                if let Some(len) = cells.length_at(pos as usize) {
                    let e = longest.entry(scene_id).or_insert(0.0);
                    *e = e.max(len);
                }
            }
        }
        let mut scenes: Vec<(u32, f64)> = longest.into_iter().collect();
        scenes.sort_unstable_by_key(|(id, _)| *id);
        Self { rows, row_pos, scenes }
    }
}

impl SongIndex {
    /// `tracks[track_idx]` のセル列。
    #[must_use]
    pub fn track_cells<'a>(&'a self, song: &'a Song, track_idx: usize) -> Option<SessionCells<'a>> {
        let track = song.tracks.get(track_idx)?;
        let index = self.tracks.get(track_idx).map_or(&EMPTY_CELLS, |t| &t.cells);
        Some(SessionCells::new(CellSlice::Track(&track.session_clips), index))
    }

    /// 置き場 `at` の `lane_idx` 番目のレーンのセル列。
    #[must_use]
    pub fn lane_cells<'a>(&'a self, song: &'a Song, at: ParamStoreAt, lane_idx: usize) -> Option<SessionCells<'a>> {
        let (lanes, store) = match at {
            ParamStoreAt::Song => (&song.song_lanes, Some(&self.song)),
            ParamStoreAt::Track(i) => {
                (&song.tracks.get(i as usize)?.automation_lanes, self.tracks.get(i as usize).map(|t| &t.store))
            }
        };
        let lane = lanes.get(lane_idx)?;
        let index = store.and_then(|s| s.cells.get(lane_idx)).unwrap_or(&EMPTY_CELLS);
        Some(SessionCells::new(CellSlice::Lane(&lane.session_clips), index))
    }

    fn cells_at<'a>(&'a self, song: &'a Song, at: RowAt) -> Option<SessionCells<'a>> {
        match at {
            RowAt::Track(ti) => self.track_cells(song, ti as usize),
            RowAt::TrackLane(ti, li) => self.lane_cells(song, ParamStoreAt::Track(ti), li as usize),
            RowAt::SongLane(li) => self.lane_cells(song, ParamStoreAt::Song, li as usize),
        }
    }

    /// ランチャーの行の本数。
    #[must_use]
    pub fn launcher_row_count(&self) -> usize {
        self.launcher.rows.len()
    }

    /// `i` 番目の行の鍵 `(track id, lane id)`。
    #[must_use]
    pub fn launcher_row_key(&self, i: usize) -> Option<(u32, u32)> {
        self.launcher.rows.get(i).map(|(key, _)| *key)
    }

    /// `i` 番目の行。
    #[must_use]
    pub fn launcher_row<'a>(&'a self, song: &'a Song, i: usize) -> Option<LauncherRow<'a>> {
        let &((track_id, lane_id), at) = self.launcher.rows.get(i)?;
        let saved = match at {
            RowAt::Track(ti) => song.tracks.get(ti as usize)?.launcher,
            RowAt::TrackLane(ti, li) => song.tracks.get(ti as usize)?.automation_lanes.get(li as usize)?.launcher,
            RowAt::SongLane(li) => song.song_lanes.get(li as usize)?.launcher,
        };
        Some(LauncherRow { track_id, lane_id, cells: self.cells_at(song, at)?, saved })
    }

    /// 鍵 `(track id, lane id)` の行のうち、並びで最初のものの位置。
    #[must_use]
    pub fn launcher_row_pos(&self, track_id: u32, lane_id: u32) -> Option<usize> {
        lookup(&self.launcher.row_pos, (track_id, lane_id))
    }

    /// 列 `scene_id` のセルのうち最も長いものの長さ (拍)。`None` = その列にセルを持つ行が無い。
    #[must_use]
    pub fn scene_longest(&self, scene_id: u32) -> Option<f64> {
        let scenes = &self.launcher.scenes;
        scenes.binary_search_by_key(&scene_id, |&(id, _)| id).ok().map(|i| scenes[i].1)
    }
}
