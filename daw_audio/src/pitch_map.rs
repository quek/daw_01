//! トラック 1 本・1 buffer の「書いた音 → 鳴らす音」(r.md #157 の追従 → r.md #130 の移調)。
//!
//! sequencer (ノートの鍵盤) と audio clip renderer (オーディオの移動量) は、移調量の `i32` の代わりにこれを受け取る。
//! 評価の式は持たない — 追従は `common::chord_follow`、移調は `common::transpose` が唯一の口で、ここは
//! engine の索引 (`SongIndex` に焼いた実効モードと自分の和音) とクリップの記録 (r.md #159 `Clip::chord_basis`、
//! オーディオは compile で曲の拍へ写した `RenderedEvent::chord_basis`) から引数を揃えて呼ぶだけ。
//! ランチャーのセルは曲の位置を持たないので記録を読まない (自分の和音だけ、`docs/plan_chord_track.md`)。
//!
//! RT 安全: `Copy` の参照の束。評価は二分探索と HashMap の参照だけ (確保・ロックなし)。

use common::chord_follow::{Basis, NoteSource, TrackChordFollow};
use common::model::{Clip, Note, Song};
use common::song_index::SongIndex;

use crate::audio_clip_renderer::RenderedEvent;

/// event の中の読み出し位置 `pos_in_event` (source 窓の頭からのフレーム、`source_frame_lerp` と同じ座標) の、
/// 音源のフレーム (オーディオの自分の和音 `Song.audio_chords` を引く位置)。reversed は窓の末尾から数える。
#[must_use]
pub fn event_source_frame(event: &RenderedEvent, pos_in_event: f64) -> u64 {
    let len = event.source_end_frames.saturating_sub(event.source_start_frames);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pos = if pos_in_event.is_finite() { pos_in_event.max(0.0) as u64 } else { 0 };
    let pos = pos.min(len.saturating_sub(1));
    event.source_start_frames + if event.reversed { len.saturating_sub(1) - pos } else { pos }
}

/// 1 トラック・1 buffer の鳴らす音の写像。`From<i32>` は移調だけ (追従なし) の写像。
#[derive(Clone, Copy, Default)]
pub struct PitchMap<'a> {
    /// 曲の移調量 (半音、移調に追従しないトラックは 0)。
    transpose: i32,
    follow: Option<Follow<'a>>,
}

#[derive(Clone, Copy)]
struct Follow<'a> {
    chords: TrackChordFollow<'a>,
    song: &'a Song,
    index: &'a SongIndex,
    /// コードを引く曲の拍 = 素材の拍 + これ。アレンジは 0、ランチャーのセルは「曲の playhead − セルの実効拍」
    /// (セルの追従は再生時の曲の playhead の位置のコードで決める。`collect_row_midi` が区間ごとに足す)。
    song_beat_offset: f64,
    /// ランチャーのセルの写像 (クリップの記録を読まない)。
    cell: bool,
}

impl From<i32> for PitchMap<'_> {
    fn from(transpose: i32) -> Self {
        Self { transpose, follow: None }
    }
}

impl<'a> PitchMap<'a> {
    /// `tracks[track_idx]` の写像。追従は索引に焼いた実効モード (`SongIndex::chord_follow`)、`transpose` は
    /// 呼び出し側が buffer 頭で解いてトラックの追従で絞った値。
    #[must_use]
    pub fn for_track(song: &'a Song, index: &'a SongIndex, track_idx: usize, transpose: i32) -> Self {
        let chords = TrackChordFollow::with_mode(song, index.chord_follow(track_idx));
        let follow = chords.is_active().then_some(Follow { chords, song, index, song_beat_offset: 0.0, cell: false });
        Self { transpose, follow }
    }

    /// ランチャーのセルの区間の写像: コードを引く位置を `delta` 拍ずらし、クリップの記録を読まない (セルは曲の位置を
    /// 持たないので「どの箱の下で置いたか」が無い — 旋律は書いた音、和音のパートは playhead の位置のコードへ)。
    #[must_use]
    pub fn for_cell(mut self, delta: f64) -> Self {
        if let Some(f) = self.follow.as_mut() {
            f.song_beat_offset = delta;
            f.cell = true;
        }
        self
    }

    /// クリップ `clip` のノート `note` (content-local 拍) が鳴らす鍵盤。`song_beat` はノートの開始の (行の) 拍。
    /// コードはノートの **開始位置** で決まる。範囲外は `None`。
    #[must_use]
    pub fn note_key(&self, clip: &Clip, note: &Note, song_beat: f64) -> Option<u8> {
        let followed = match self.follow {
            None => Some(note.pitch),
            Some(f) => {
                let basis = if f.cell { &[][..] } else { &clip.chord_basis.notes };
                let src = NoteSource::of(f.index.own_chords(clip.content_id), basis, note);
                f.chords.key(note.pitch, song_beat + f.song_beat_offset, &src)
            }
        };
        followed.and_then(|k| common::transpose::sounding_key(k, self.transpose))
    }

    /// オーディオ event `event` の移動量 (半音) = 追従 (曲の拍 `song_beat` のコード) + 移調。自分の和音は音源の
    /// フレーム `source_frame` を覆う、和音を弾いている区間。記録はアレンジの event だけが読む (compile で曲の拍へ
    /// 写した `RenderedEvent::chord_basis`、`TrackChordFollow::audio_shift`)。
    #[must_use]
    pub fn audio_semitones(&self, event: &RenderedEvent, source_frame: u64, song_beat: f64) -> i32 {
        let shift = self.follow.map_or(0, |f| {
            let at = song_beat + f.song_beat_offset;
            let own = f
                .song
                .audio_chords
                .get(&event.source_id)
                .and_then(|s| common::chord_detect::audio_own_chord_at(s, source_frame));
            if f.cell || event.cell_clip_id != 0 {
                f.chords.shift(at, Basis { own, placed: None })
            } else {
                f.chords.audio_shift(at, own, &event.chord_basis)
            }
        });
        self.transpose + shift
    }
}
