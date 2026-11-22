//! コード → 鍵盤 (voicing) と、コードトラック ⇄ MIDI クリップの変換 (r.md #157、`docs/plan_chord_track.md`)。
//!
//! - [`chord_pitches`]: コード 1 つを鳴らす鍵盤の列。試聴とノート化が同じ関数を通す (聴いた音がそのままノートになる)。
//! - [`Song::chords_to_clip`]: コードトラックの箱を MIDI クリップにする (帯 → トラックのドラッグ / Chords to MIDI)。
//! - [`Song::chords_from_clips`]: MIDI クリップのノートからコードを検出する (クリップ → 帯のドラッグ / コードを抽出)。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use crate::chord_detect::{DetectNote, DetectOptions, detect_chords};
use crate::model::{Chord, ChordEvent, Clip, ClipContent, MidiContent, Note, Song, Tensions};

/// コードを鍵盤にする並べ方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum Voicing {
    /// 基本形: ベース音を 1 オクターブ下に 1 つ、その上にルートから積む (Studio One のノート化と同じ形)。
    #[default]
    Basic,
}

/// 基本形のルートを置くオクターブの C (MIDI 鍵盤、C3 = 48)。
pub const DEFAULT_VOICING_BASE: u8 = 48;

/// コード 1 つを鳴らす鍵盤の列 (昇順、重複なし、0..=127 に入るものだけ)。
///
/// `base` はルートを探し始める鍵盤 (その鍵盤以上で最初のルート)。テンションは 1 オクターブ上に積む。
#[must_use]
pub fn chord_pitches(chord: &Chord, voicing: Voicing, base: u8) -> Vec<u8> {
    let Voicing::Basic = voicing;
    let root = i32::from(base) + (i32::from(chord.root % 12) - i32::from(base % 12)).rem_euclid(12);
    let altered_fifth = chord.tensions.contains(Tensions::FLAT5) || chord.tensions.contains(Tensions::SHARP5);
    let mut out: Vec<i32> = Vec::new();
    // ベース音はルートの下 (転回形 `C/E` は E を最低音に、指定が無ければルートの 1 オクターブ下)。
    let bass = root - (i32::from(chord.root % 12) - i32::from(chord.bass_pc())).rem_euclid(12);
    out.push(if bass == root { root - 12 } else { bass });
    for &i in chord.kind.intervals() {
        if i == 7 && altered_fifth {
            continue;
        }
        out.push(root + i32::from(i));
    }
    for &(t, _, semis) in Tensions::ALL {
        if !chord.tensions.contains(t) {
            continue;
        }
        // 変化した 5 度は 5 度の位置に、それ以外のテンションは 1 オクターブ上に置く。
        let in_place = t == Tensions::FLAT5 || t == Tensions::SHARP5;
        out.push(root + i32::from(semis) + if in_place { 0 } else { 12 });
    }
    let mut out: Vec<u8> = out.into_iter().filter_map(|p| u8::try_from(p).ok().filter(|p| *p <= 127)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

impl Song {
    /// コードトラックの箱 `chord_ids` (空なら全部) を、トラック `track_id` の 1 つの MIDI クリップにする。
    /// クリップは最初の箱の頭から最後の箱の終わりまでで、箱ごとにその長さのノートを置く。
    /// 置いたクリップの id を返す (箱が無い / トラックが無ければ `None`)。
    pub fn chords_to_clip(
        &mut self,
        chord_ids: &[u32],
        track_id: u32,
        voicing: Voicing,
        velocity: u8,
    ) -> Option<u32> {
        let picked: Vec<ChordEvent> = self
            .chords
            .iter()
            .filter(|c| chord_ids.is_empty() || chord_ids.contains(&c.id))
            .cloned()
            .collect();
        let start = picked.first()?.start_beat;
        let end = picked.iter().map(ChordEvent::end_beat).fold(start, f64::max);
        self.track_by_id(track_id)?;
        let mut content = MidiContent::default();
        for c in &picked {
            for pitch in chord_pitches(&c.chord, voicing, DEFAULT_VOICING_BASE) {
                let id = content.alloc_note_id();
                content.notes.push(Note {
                    id,
                    start_beat: c.start_beat - start,
                    duration_beats: c.len_beats,
                    pitch,
                    velocity: velocity.clamp(1, 127),
                    lyric: None,
                    muted: false,
                });
            }
        }
        let content_id = self.alloc_content(ClipContent::Midi(content), String::new());
        let track = self.tracks.iter_mut().find(|t| t.id == track_id)?;
        Some(track.place_clip(Clip { start_beat: start, length_beats: end - start, content_id, ..Clip::default() }))
    }

    /// MIDI クリップ `clips` (`(track id, clip id)`) のノートから検出したコードの列 (位置は song-absolute、
    /// id は 0)。窓に見えているノートだけを使う。Song は変えない — 置くのは呼び出し側
    /// (`ChordTrackEvent::PlaceMany`)。
    #[must_use]
    pub fn chords_from_clips(&self, clips: &[(u32, u32)], opts: &DetectOptions) -> Vec<ChordEvent> {
        let mut notes: Vec<DetectNote> = Vec::new();
        for (track_id, clip_id) in clips {
            let Some(clip) = self.track_by_id(*track_id).and_then(|t| t.clips.iter().find(|c| c.id == *clip_id))
            else {
                continue;
            };
            let (win_start, win_end) = clip.song_window();
            for n in self.clip_notes(clip).iter().filter(|n| !n.muted) {
                let s = clip.content_to_song_beat(n.start_beat);
                let (s, e) = (s.max(win_start), (s + n.duration_beats).min(win_end));
                if e > s {
                    notes.push(DetectNote { start_beat: s, len_beats: e - s, pitch: n.pitch });
                }
            }
        }
        detect_chords(&notes, opts)
            .into_iter()
            .map(|s| ChordEvent { id: 0, start_beat: s.start_beat, len_beats: s.len_beats, chord: s.chord, color: None })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChordKind, Track};

    #[test]
    fn basic_voicing_puts_the_bass_below_and_tensions_above() {
        // C: C2 + C3 E3 G3
        assert_eq!(chord_pitches(&Chord::new(0, ChordKind::Major), Voicing::Basic, 48), vec![36, 48, 52, 55]);
        // C/E: E2 が最低音
        let c_over_e = Chord { bass: Some(4), ..Chord::new(0, ChordKind::Major) };
        assert_eq!(chord_pitches(&c_over_e, Voicing::Basic, 48), vec![40, 48, 52, 55]);
        // A7(b9): A2 + A3 C#4 E4 G4 + Bb4
        let a7b9 = Chord { tensions: Tensions::FLAT9, ..Chord::new(9, ChordKind::Dominant7) };
        assert_eq!(chord_pitches(&a7b9, Voicing::Basic, 48), vec![45, 57, 61, 64, 67, 70]);
        // C7(b5): 5 度が b5 に置き換わる
        let c7b5 = Chord { tensions: Tensions::FLAT5, ..Chord::new(0, ChordKind::Dominant7) };
        assert_eq!(chord_pitches(&c7b5, Voicing::Basic, 48), vec![36, 48, 52, 54, 58]);
    }

    /// 箱 → クリップ → 検出で、元の進行に戻る (ノート化と検出が同じコードの定義を見ている)。
    #[test]
    fn chords_survive_a_round_trip_through_a_midi_clip() {
        let mut song = Song { tracks: vec![Track { id: 1, ..Track::default() }], ..Song::default() };
        let progression = [(0.0, 4.0, 0, ChordKind::Major), (4.0, 4.0, 9, ChordKind::Minor7), (8.0, 8.0, 7, ChordKind::Dominant7)];
        for (s, l, root, kind) in progression {
            song.place_chord(ChordEvent { id: 0, start_beat: s, len_beats: l, chord: Chord::new(root, kind), color: None });
        }
        let clip_id = song.chords_to_clip(&[], 1, Voicing::Basic, 100).expect("クリップができる");
        let clip = song.tracks[0].clips.iter().find(|c| c.id == clip_id).unwrap();
        assert_eq!(clip.song_window(), (0.0, 16.0));
        let found = song.chords_from_clips(&[(1, clip_id)], &DetectOptions { window_beats: 4.0, ..DetectOptions::default() });
        let got: Vec<(f64, f64, u8, ChordKind)> =
            found.iter().map(|c| (c.start_beat, c.len_beats, c.chord.root, c.chord.kind)).collect();
        assert_eq!(got, progression.to_vec());
    }
}
