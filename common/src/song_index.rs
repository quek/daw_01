//! RT (buffer / 変調の刻み / サンプルごと) が `Song` を **id や target で探さない / 全件を舐めない**ための索引。
//!
//! off-RT で `Song` から作り ([`SongIndex::build`])、その `Song` の snapshot と **同じ便で** RT へ届ける
//! (daw_audio の `RtBundle::song_index`)。中身は位置なので、別の snapshot と組み合わせてはならない
//! (外れた位置は `get` で引くので panic はしない。本数が合わない置き場は空として扱い、道の先の id が違えば無いものとする)。
//!
//! 引ける答えは今までの線形探索と同じもの (同じ id / 同じ target が複数あれば、線形探索が先に見つける方):
//!
//! - パラメーターの置き場 (track / song の lane・routing) を **束ねる単位** ([`ParamSubject`]) で並べ、「この device の
//!   param 全部」「この target の routing 全部」を二分探索 + その分だけの走査で引く (routing の重複除去が routing 数の
//!   二乗になっていた)。lane の automation clip は点を覆う clip を引く。
//! - ランチャーの行の並びと鍵 → 行、行のセルは clip id / scene id → 位置、列ごとの最長のセル ([`launcher`])。
//! - device / chain は id → 位置の道 ([`nodes`])。トラック / シーンは id → 位置。
//! - トラックのアレンジ clip と、MIDI の note / 読み上げの event は「窓に掛かりうるもの」を元の並び順で引く
//!   ([`RangeIndex`])。

mod launcher;
mod nodes;
mod params;
mod ranges;

use std::collections::HashMap;
use std::sync::Arc;

pub use launcher::{CellIndex, CellSlice, LauncherRow, SessionCells};
pub use params::{LaneView, ParamStore, ParamStoreIndex, ParamSubject, TargetRoutings};
pub use ranges::{CoverIndex, EndTree, RangeIndex, Reaching};

use crate::chord_follow::{OwnChordsMemo, OwnScope};
use crate::model::{
    ChordFollow, ContentId, Device, NativeDevice, Parallel, ParallelChain, ParamStoreAt, Send, SessionClip, Song,
    SourceChord, Track,
};
use params::position;

/// トラック `t` の MIDI のセルごとの自分の和音 (`(clip id, 区間)` を id 順に、同じ id は先頭だけ)。
fn cell_own_chords(song: &Song, t: &Track, memo: &mut OwnChordsMemo) -> Vec<(u32, Arc<[SourceChord]>)> {
    let mut cells: Vec<(u32, Arc<[SourceChord]>)> = t
        .session_clips
        .iter()
        .filter(|c| song.clip_contents.get(&c.clip.content_id).and_then(|m| m.notes()).is_some())
        .map(|c| (c.clip.id, memo.own_chords(song, OwnScope::Cell { track_id: t.id, clip_id: c.clip.id })))
        .collect();
    cells.sort_by_key(|(id, _)| *id);
    cells.dedup_by_key(|(id, _)| *id);
    cells
}

/// トラック 1 本の索引。
#[derive(Debug, Clone, Default, PartialEq)]
struct TrackIndex {
    store: ParamStoreIndex,
    /// VOICEVOX の builtin を持つ (読み上げの note を出す、`Track::is_voicevox_vocal`)。
    voicevox_vocal: bool,
    /// 実効的に有効 (r.md #131、`Song::track_effectively_enabled`)。無効トラックの行はランチャーの行の集合に入らない。
    enabled: bool,
    /// r.md #130: 移調に実効的に追従する (`Song::track_follows_transpose`、祖先グループまで辿った値)。
    follows_transpose: bool,
    /// r.md #157: コードトラックへの実効の追従モード (曲全体のスイッチ込み。鳴る音が変わりえない曲では全部 `Off`)。
    chord_follow: ChordFollow,
    /// r.md #159: 追従するトラックのアレンジが鳴らすノートの自分の和音 (`chord_follow::own`、曲の拍)。RT がノートから
    /// 検出し直さないため。追従しないトラックは `None`。
    own_chords: Option<Arc<[SourceChord]>>,
    /// 同じく、MIDI のセルごとの自分の和音 (`(clip id, 行の拍の区間)` を id 順に)。
    cell_own_chords: Vec<(u32, Arc<[SourceChord]>)>,
    /// `(send id, 位置)` を id 順に (同じ id は先頭だけ)。
    sends: Vec<(u32, u32)>,
    /// アレンジの clip `[start_beat, start_beat + length_beats)`。
    clips: RangeIndex,
    /// セル (`session_clips`)。
    cells: CellIndex,
}

/// `Song` 1 枚ぶんの索引。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SongIndex {
    /// `song_lanes` / `song_mod_routings`。
    song: ParamStoreIndex,
    /// `tracks[i]`。
    tracks: Vec<TrackIndex>,
    /// `(track id, 位置)` / `(scene id, 位置)` を id 順に (同じ id は先頭だけ)。
    track_pos: Vec<(u32, u32)>,
    scene_pos: Vec<(u32, u32)>,
    nodes: nodes::NodeIndex,
    /// content id → MIDI の note `[start, start + duration)` / 読み上げの event `[start, start]` (content-local 拍)。
    contents: HashMap<ContentId, RangeIndex>,
    launcher: launcher::LauncherIndex,
}

/// `(id, 位置)` を id 順に並べ、同じ id は先頭だけ残す (`iter().position(|x| x.id == id)` と同じ答え)。
fn id_positions<K: Ord>(ids: impl Iterator<Item = K>) -> Vec<(K, u32)> {
    let mut v: Vec<(K, u32)> = ids.enumerate().map(|(i, id)| (id, position(i))).collect();
    v.sort_unstable();
    v.dedup_by(|later, first| later.0 == first.0);
    v
}

fn lookup<K: Ord + Copy>(v: &[(K, u32)], id: K) -> Option<usize> {
    let i = v.binary_search_by_key(&id, |&(k, _)| k).ok()?;
    Some(v[i].1 as usize)
}

impl SongIndex {
    /// off-RT で作る (曲の要素数に比例)。
    #[must_use]
    pub fn build(song: &Song) -> Self {
        Self::build_with(song, &mut OwnChordsMemo::default())
    }

    /// [`Self::build`] の、自分の和音の検出を `memo` で使い回す版 (engine が編集のたびに作り直す索引。鳴らすノートが
    /// 前と同じトラック / セルは検出し直さない — フェーダーのドラッグ中は検出 0 回)。`memo` は呼ぶたびに世代を閉じる。
    #[must_use]
    pub fn build_with(song: &Song, memo: &mut OwnChordsMemo) -> Self {
        let follow_applies = song.chord_follow_can_apply();
        // `follow_applies` は曲全体のスイッチを含むので、実効モード (`Song::track_chord_follow`) は手元の `t` の値
        // (id からトラックを引き直すと全トラックで O(T²))。
        let chord_follow = |t: &Track| if follow_applies { t.follow_chords } else { ChordFollow::Off };
        let tracks = song
            .tracks
            .iter()
            .zip(song.effectively_enabled_mask().into_iter().zip(song.follows_transpose_mask()))
            .map(|(t, (enabled, follows_transpose))| TrackIndex {
                store: ParamStoreIndex::build(&t.automation_lanes, &t.mod_routings),
                voicevox_vocal: t.is_voicevox_vocal(),
                enabled,
                follows_transpose,
                chord_follow: chord_follow(t),
                own_chords: (!chord_follow(t).is_off())
                    .then(|| memo.own_chords(song, OwnScope::Arrangement { track_id: t.id })),
                cell_own_chords: if chord_follow(t).is_off() { Vec::new() } else { cell_own_chords(song, t, memo) },
                sends: id_positions(t.sends.iter().map(|s| s.id)),
                clips: RangeIndex::build(t.clips.iter().map(|c| (c.start_beat, c.start_beat + c.length_beats))),
                cells: CellIndex::build(t.session_clips.iter().map(|c| (c.clip.id, c.scene_id))),
            })
            .collect();
        let contents = song
            .clip_contents
            .iter()
            .filter_map(|(&id, content)| {
                let ranges = match (content.notes(), content.text_events()) {
                    (Some(notes), _) => RangeIndex::build(notes.iter().map(|n| (n.start_beat, n.start_beat + n.duration_beats))),
                    (None, Some(events)) => {
                        RangeIndex::build(events.iter().map(|e| (e.event_start_in_clip_beats, e.event_start_in_clip_beats)))
                    }
                    (None, None) => return None,
                };
                Some((id, ranges))
            })
            .collect();
        memo.sweep();
        let mut index = Self {
            song: ParamStoreIndex::build(&song.song_lanes, &song.song_mod_routings),
            tracks,
            track_pos: id_positions(song.tracks.iter().map(|t| t.id)),
            scene_pos: id_positions(song.scenes.iter().map(|s| s.id)),
            nodes: nodes::NodeIndex::build(song),
            contents,
            launcher: launcher::LauncherIndex::default(),
        };
        index.launcher = launcher::LauncherIndex::build(song, &index);
        index
    }

    /// song 側の置き場 (master fx chain / master limiter / テンポ)。
    #[must_use]
    pub fn song_store<'a>(&'a self, song: &'a Song) -> ParamStore<'a> {
        ParamStore::new(&song.song_lanes, &song.song_mod_routings, &self.song)
    }

    /// `tracks[track_idx]` の置き場 (無ければ空)。
    #[must_use]
    pub fn track_store<'a>(&'a self, song: &'a Song, track_idx: usize) -> ParamStore<'a> {
        match (song.tracks.get(track_idx), self.tracks.get(track_idx)) {
            (Some(t), Some(index)) => ParamStore::new(&t.automation_lanes, &t.mod_routings, &index.store),
            _ => ParamStore::default(),
        }
    }

    /// 解決済みの置き場。
    #[must_use]
    pub fn store<'a>(&'a self, song: &'a Song, at: ParamStoreAt) -> ParamStore<'a> {
        match at {
            ParamStoreAt::Song => self.song_store(song),
            ParamStoreAt::Track(i) => self.track_store(song, i as usize),
        }
    }

    /// track id の位置。
    #[must_use]
    pub fn track_pos(&self, track_id: u32) -> Option<usize> {
        lookup(&self.track_pos, track_id)
    }

    /// scene id の位置。
    #[must_use]
    pub fn scene_pos(&self, scene_id: u32) -> Option<usize> {
        lookup(&self.scene_pos, scene_id)
    }

    /// `tracks[track_idx]` が VOICEVOX の builtin を持つか (`Track::is_voicevox_vocal`)。
    #[must_use]
    pub fn is_voicevox_vocal(&self, track_idx: usize) -> bool {
        self.tracks.get(track_idx).is_some_and(|t| t.voicevox_vocal)
    }

    /// `tracks[track_idx]` が移調に追従するか (`Song::track_follows_transpose`、RT が祖先を辿らないため)。
    #[must_use]
    pub fn follows_transpose(&self, track_idx: usize) -> bool {
        self.tracks.get(track_idx).is_some_and(|t| t.follows_transpose)
    }

    /// `tracks[track_idx]` のコードトラックへの実効の追従モード (`Song::track_chord_follow`。追従で鳴る音が
    /// 変わりえない曲では `Off`)。
    #[must_use]
    pub fn chord_follow(&self, track_idx: usize) -> ChordFollow {
        self.tracks.get(track_idx).map_or(ChordFollow::Off, |t| t.chord_follow)
    }

    /// `tracks[track_idx]` のアレンジの自分の和音 (曲の拍、追従するトラックだけ)。RT 安全。
    #[must_use]
    pub fn own_chords(&self, track_idx: usize) -> &[SourceChord] {
        self.tracks.get(track_idx).and_then(|t| t.own_chords.as_deref()).unwrap_or(&[])
    }

    /// `tracks[track_idx]` のセル `clip_id` の自分の和音 (行の拍、追従するトラックの MIDI のセルだけ)。RT 安全
    /// (二分探索)。
    #[must_use]
    pub fn cell_own_chords(&self, track_idx: usize, clip_id: u32) -> &[SourceChord] {
        let Some(t) = self.tracks.get(track_idx) else { return &[] };
        t.cell_own_chords
            .binary_search_by_key(&clip_id, |(id, _)| *id)
            .map_or(&[], |i| &t.cell_own_chords[i].1)
    }

    /// `tracks[track_idx]` の send id の send。
    #[must_use]
    pub fn send<'a>(&self, song: &'a Song, track_idx: usize, send_id: u32) -> Option<&'a Send> {
        let pos = lookup(&self.tracks.get(track_idx)?.sends, send_id)?;
        song.tracks.get(track_idx)?.sends.get(pos)
    }

    /// `tracks[track_idx]` のアレンジ clip の区間索引。
    #[must_use]
    pub fn track_clips(&self, track_idx: usize) -> Option<&RangeIndex> {
        self.tracks.get(track_idx).map(|t| &t.clips)
    }

    /// `tracks[track_idx]` のセル (clip id で)。
    #[must_use]
    pub fn track_cell<'a>(&'a self, song: &'a Song, track_idx: usize, clip_id: u32) -> Option<&'a SessionClip> {
        let pos = self.track_cells(song, track_idx)?.clip_pos(clip_id)?;
        song.tracks.get(track_idx)?.session_clips.get(pos)
    }

    /// content の note / 読み上げ event の区間索引 (content-local 拍)。
    #[must_use]
    pub fn content_ranges(&self, content_id: ContentId) -> Option<&RangeIndex> {
        self.contents.get(&content_id)
    }

    /// `Song::device_by_id` と同じ device。
    #[must_use]
    pub fn device<'a>(&self, song: &'a Song, id: u64) -> Option<&'a Device> {
        self.nodes.device(song, id)
    }

    #[must_use]
    pub fn parallel<'a>(&self, song: &'a Song, id: u64) -> Option<&'a Parallel> {
        self.device(song, id).and_then(Device::as_parallel)
    }

    /// 置き場 `owner` の device 列の中の内蔵 device (`model::native_in(owner の device 列, id)` と同じもの)。
    #[must_use]
    pub fn native_in<'a>(&self, song: &'a Song, owner: ParamStoreAt, id: u64) -> Option<&'a NativeDevice> {
        self.nodes.native_in(song, owner, id)
    }

    /// `Song::parallel_by_id(parallel_id)` の中の chain (`chains.iter().find(|c| c.id == chain_id)` と同じもの)。
    #[must_use]
    pub fn chain_in<'a>(&self, song: &'a Song, parallel_id: u64, chain_id: u64) -> Option<&'a ParallelChain> {
        self.nodes.chain_in(song, parallel_id, chain_id)
    }
}

#[cfg(test)]
mod tests;
