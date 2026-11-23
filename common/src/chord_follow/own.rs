//! MIDI の **自分の和音** (r.md #159、`crate::chord_detect::own_chords`) を検出する素材と、その検出の使い回し。
//!
//! 検出するのは **鳴らすノート** だけ: sequencer と同じ門 (`Clip::sounding_note_len` — 窓の外に隠したノートは鳴らず、
//! 窓の末尾を越える長さは末尾で切る。ミュートしたクリップ / ノートは鳴らない) を通ったノートを、素材ごとに 1 回で
//! 検出する。素材は **トラックのアレンジ** (全クリップを曲の拍で 1 つに) か **ランチャーのセル 1 つ** (行の拍)。
//! content ごとに検出すると、窓の外に隠れた和音に接する旋律の音や、隠れたベース音と終わりが揃う 2 音が和音の
//! パートとして動き、分割・trim・Glue の前後で鳴りが変わる (鳴っていない音で決めない)。
//!
//! 位置は `Clip::content_to_song_beat` の拍 (アレンジは曲の拍、セルは行の拍) — ノートの読み口
//! (`super::NoteSource::of`) も同じ拍で引く。
//!
//! 検出は素材のノート数に比例して重いので、入力 (鳴らすノートの列) が前と同じなら結果を使い回す
//! ([`OwnChordsMemo`]。engine の索引の作り直し (編集のたびの LoadSong) と GUI の毎フレームの読み口が持つ)。

use std::collections::HashMap;
use std::sync::Arc;

use crate::chord_detect::DetectNote;
use crate::model::{Clip, ClipKey, Note, Song, SourceChord};

/// 自分の和音を検出する素材 1 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwnScope {
    /// トラック `track_id` のアレンジの全クリップ (曲の拍)。
    Arrangement { track_id: u32 },
    /// トラック `track_id` のランチャーのセル `clip_id` (行の拍、セルは曲の位置を持たない)。
    Cell { track_id: u32, clip_id: u32 },
}

impl Song {
    /// クリップ `key` の素材 (`Track::clip_by_id` はセルも返すので、アレンジの列に無ければセル)。
    #[must_use]
    pub fn own_scope_of(&self, key: ClipKey) -> OwnScope {
        let in_song = self.track_by_id(key.track_id).is_some_and(|t| t.clip_index_by_id(key.clip_id).is_some());
        if in_song {
            OwnScope::Arrangement { track_id: key.track_id }
        } else {
            OwnScope::Cell { track_id: key.track_id, clip_id: key.clip_id }
        }
    }
}

/// クリップ `clip` が鳴らすノート (`notes` はその content のノート) を、検出の入力として `out` へ足す
/// (module doc の門と拍)。
pub fn push_sounding_notes(clip: &Clip, notes: &[Note], out: &mut Vec<DetectNote>) {
    if clip.muted {
        return;
    }
    for n in notes.iter().filter(|n| !n.muted) {
        if let Some(len) = clip.sounding_note_len(n) {
            out.push(DetectNote { start_beat: clip.content_to_song_beat(n.start_beat), len_beats: len, pitch: n.pitch });
        }
    }
}

/// 素材 `scope` の検出の入力 (鳴らすノート、クリップの順)。`replace` = `(clip id, ノート)` はそのクリップの content の
/// ノートを差し替える (「鳴る音で確定」が確定した後の鳴り方を確かめる)。素材が無ければ空。
#[must_use]
pub fn own_chord_input(song: &Song, scope: OwnScope, replace: Option<(u32, &[Note])>) -> Vec<DetectNote> {
    let notes_of = |clip: &Clip| match replace {
        Some((id, notes)) if id == clip.id => notes,
        _ => song.clip_notes(clip),
    };
    let mut out = Vec::new();
    match scope {
        OwnScope::Arrangement { track_id } => {
            for clip in song.track_by_id(track_id).map_or(&[][..], |t| t.clips.as_slice()) {
                push_sounding_notes(clip, notes_of(clip), &mut out);
            }
        }
        OwnScope::Cell { track_id, clip_id } => {
            let cell = song.track_by_id(track_id).and_then(|t| t.session_clips.iter().find(|c| c.clip.id == clip_id));
            if let Some(cell) = cell {
                push_sounding_notes(&cell.clip, notes_of(&cell.clip), &mut out);
            }
        }
    }
    out
}

/// 素材 `scope` の自分の和音 (使い回さずに検出する。1 回きりの経路: 焼き込み / 「鳴る音で確定」/ オフライン)。
#[must_use]
pub fn own_chords_in(song: &Song, scope: OwnScope, replace: Option<(u32, &[Note])>) -> Vec<SourceChord> {
    crate::chord_detect::own_chords(&own_chord_input(song, scope, replace))
}

/// 入力 (鳴らすノートの列) が前と同じ素材は検出を使い回す memo。判定は入力の等値 (O(ノート数)、検出より桁で安い)
/// なので、世代や hash の取り違えで古い結果を返すことは無い。
#[derive(Debug, Default)]
pub struct OwnChordsMemo {
    entries: HashMap<OwnScope, MemoEntry>,
    /// 今の世代 ([`Self::sweep`] で進む)。
    generation: u64,
}

#[derive(Debug)]
struct MemoEntry {
    input: Vec<DetectNote>,
    own: Arc<[SourceChord]>,
    /// 最後に引かれた世代。
    used: u64,
}

impl OwnChordsMemo {
    /// 素材 `scope` の自分の和音。入力が前と同じなら前の結果 (共有)、違えば検出し直す。
    pub fn own_chords(&mut self, song: &Song, scope: OwnScope) -> Arc<[SourceChord]> {
        let input = own_chord_input(song, scope, None);
        let generation = self.generation;
        match self.entries.get_mut(&scope) {
            Some(e) if e.input == input => {
                e.used = generation;
                Arc::clone(&e.own)
            }
            _ => {
                let own: Arc<[SourceChord]> = crate::chord_detect::own_chords(&input).into();
                self.entries.insert(scope, MemoEntry { input, own: Arc::clone(&own), used: generation });
                own
            }
        }
    }

    /// 世代を閉じる: この世代で引かれなかった素材 (消えたトラック / セル、追従を切ったトラック) を捨て、次の世代を
    /// 始める。
    pub fn sweep(&mut self) {
        let generation = self.generation;
        self.entries.retain(|_, e| e.used == generation);
        self.generation = generation.wrapping_add(1);
    }
}
