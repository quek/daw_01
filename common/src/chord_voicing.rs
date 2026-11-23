//! コード → 鍵盤 (voicing) と、コードトラック ⇄ MIDI クリップの変換 (r.md #157、`docs/plan_chord_track.md`)。
//!
//! - [`chord_pitches`]: コード 1 つを鳴らす鍵盤の列。試聴とノート化が同じ関数を通す (聴いた音がそのままノートになる)。
//! - [`Song::chords_to_clip`]: コードトラックの箱を MIDI クリップにする (帯 → トラックのドラッグ / Chords to MIDI)。
//! - [`Song::chords_from_clips`]: MIDI クリップのノートからコードを検出する (クリップ → 帯のドラッグ / コードを抽出)。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use crate::chord_detect::{DetectNote, DetectOptions, extract_chords};
use crate::model::{Chord, ChordEvent, Clip, ClipContent, MidiContent, Note, Song, Tensions};

/// 型は「再生中も鳴らす」の設定として wire を渡るので `crate::model` (`chords.rs`、`WIRE_SOURCES`) に置く。
pub use crate::model::Voicing;

/// 基本形のルートを置くオクターブの C (MIDI 鍵盤、C3 = 48)。
pub const DEFAULT_VOICING_BASE: u8 = 48;

/// コード 1 つを鳴らす鍵盤の数の上限 (どの voicing もこれを超えない)。基本形の形 = ベース 1 + 種類の音 + テンション
/// 全部、が最大 (ピアノ / 自動は 12 の pitch class + ベース、ギターは 6 弦)。再生中の試聴 (daw_audio) は固定長の
/// 配列で鍵盤を持つので、ここを超えない保証が要る。
pub const MAX_CHORD_KEYS: usize = 1 + max_kind_tones() + Tensions::ALL.len();

const fn max_kind_tones() -> usize {
    let kinds = crate::model::ChordKind::ALL;
    let (mut i, mut max) = (0, 0);
    while i < kinds.len() {
        let n = kinds[i].intervals().len();
        if n > max {
            max = n;
        }
        i += 1;
    }
    max
}

/// コード 1 つを鳴らす鍵盤の列 (昇順、重複なし、0..=127 に入るものだけ)。直前のコードを見ない形
/// ([`voice_chord`] の `prev = None`)。
///
/// `base` はルートを探し始める鍵盤 (その鍵盤以上で最初のルート)。テンションは 1 オクターブ上に積む。
#[must_use]
pub fn chord_pitches(chord: &Chord, voicing: Voicing, base: u8) -> Vec<u8> {
    voice_chord(chord, voicing, base, None)
}

/// コード 1 つを鳴らす鍵盤の列 (昇順、重複なし)。`prev` は直前に鳴らした鍵盤 (最低音 = ベース) で、
/// [`Voicing::Adaptive`] だけが見る。**試聴・再生中の試聴・ノート化はみなこれを通す**。
#[must_use]
pub fn voice_chord(chord: &Chord, voicing: Voicing, base: u8, prev: Option<&[u8]>) -> Vec<u8> {
    match voicing {
        Voicing::Basic => basic_pitches(chord, base),
        Voicing::Piano => piano_pitches(chord, base),
        Voicing::Guitar => guitar_pitches(chord).unwrap_or_else(|| piano_pitches(chord, base)),
        Voicing::Adaptive => match prev.filter(|p| p.len() >= 2) {
            Some(prev) => adaptive_pitches(chord, base, prev),
            None => piano_pitches(chord, base),
        },
    }
}

/// コード列を順に voicing する (自動は直前の結果を引き継ぐ)。`chords` と同じ長さの列を返す。
#[must_use]
pub fn voice_progression<'a>(
    chords: impl IntoIterator<Item = &'a Chord>,
    voicing: Voicing,
    base: u8,
) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for c in chords {
        let v = voice_chord(c, voicing, base, out.last().map(Vec::as_slice));
        out.push(v);
    }
    out
}

/// 上声部に積む pitch class (ルートからの半音、昇順)。変化した 5 度とテンションを含む。
fn upper_intervals(chord: &Chord) -> Vec<u8> {
    let m = chord.interval_mask();
    (0..12u8).filter(|i| (m >> i) & 1 == 1).collect()
}

fn to_keys(v: impl IntoIterator<Item = i32>) -> Vec<u8> {
    let mut out: Vec<u8> = v.into_iter().filter_map(|p| u8::try_from(p).ok().filter(|p| *p <= 127)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// `lo` 以上で最初の pitch class `pc` の鍵盤。
fn at_or_above(lo: i32, pc: u8) -> i32 {
    lo + (i32::from(pc % 12) - lo).rem_euclid(12)
}

/// 密集位置: `lo` 以上で最初の構成音から、構成音を 1 つずつ上へ積む。
fn close_position(chord: &Chord, lo: i32, start_index: usize) -> Vec<i32> {
    let ivs = upper_intervals(chord);
    let n = ivs.len();
    let mut out = Vec::with_capacity(n);
    let mut cur = lo - 1;
    for k in 0..n {
        let pc = (chord.root % 12 + ivs[(start_index + k) % n]) % 12;
        cur = at_or_above(cur + 1, pc);
        out.push(cur);
    }
    out
}

fn piano_pitches(chord: &Chord, base: u8) -> Vec<u8> {
    let base = i32::from(base);
    let bass = at_or_above(base - 12, chord.bass_pc());
    // 右手は `base` の 5 度上 (C3 基準なら G3) 以上で最初の構成音から。
    let lo = base + 7;
    let ivs = upper_intervals(chord);
    let first = (0..ivs.len())
        .min_by_key(|&k| at_or_above(lo, (chord.root % 12 + ivs[k]) % 12))
        .unwrap_or(0);
    let mut v = close_position(chord, lo, first);
    v.push(bass);
    to_keys(v)
}

fn adaptive_pitches(chord: &Chord, base: u8, prev: &[u8]) -> Vec<u8> {
    let base = i32::from(base);
    let prev_bass = i32::from(prev[0]);
    let prev_upper: Vec<i32> = prev[1..].iter().map(|&p| i32::from(p)).collect();
    // ベースは直前のベースに最も近い位置 (ただし base-17 ..= base+4 の帯の中、低すぎ / 高すぎを避ける)。
    let bass_pc = chord.bass_pc();
    let bass = (base - 17..=base + 4)
        .filter(|p| p.rem_euclid(12) == i32::from(bass_pc))
        .min_by_key(|p| (p - prev_bass).abs())
        .unwrap_or_else(|| at_or_above(base - 12, bass_pc));
    // 上声部は全転回形 × 近いオクターブから、直前の声部との距離 (各音の最寄りの距離の和、両方向) が最小のもの。
    let n = upper_intervals(chord).len();
    let dist = |cand: &[i32]| -> i32 {
        let one = |a: &[i32], b: &[i32]| a.iter().map(|x| b.iter().map(|y| (x - y).abs()).min().unwrap_or(0)).sum::<i32>();
        one(cand, &prev_upper) + one(&prev_upper, cand)
    };
    let mut best: Option<(i32, Vec<i32>)> = None;
    for lo in base + 2..=base + 19 {
        for k in 0..n {
            let cand = close_position(chord, lo, k);
            if cand.first() != Some(&lo) {
                continue; // 同じ形を別の lo で重複して数えない
            }
            let d = dist(&cand);
            if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                best = Some((d, cand));
            }
        }
    }
    let mut v = best.map(|(_, v)| v).unwrap_or_else(|| close_position(chord, base + 7, 0));
    // ベースは上声部より下 (上声部が下がってベースと交差したら、ベースを 1 オクターブ下げる)。
    let mut bass = bass;
    while v.first().is_some_and(|&lo| bass >= lo) {
        bass -= 12;
    }
    v.push(bass);
    to_keys(v)
}

/// 標準調弦の開放弦 (6 弦 → 1 弦)。
const GUITAR_STRINGS: [i32; 6] = [40, 45, 50, 55, 59, 64];

/// ギターの形: 最低音の弦でベース音を押さえ、それより高い弦は全部鳴らす (内側の弦はミュートしない)。
/// ポジション 0..=9 で、4 フレットの幅 (+ 開放弦) に収まる形のうち、必要な構成音 (完全 5 度以外) を
/// 全部含み、鳴る弦が多く、ポジションが低いものを選ぶ。押さえられる形が無ければ `None`。
fn guitar_pitches(chord: &Chord) -> Option<Vec<u8>> {
    let tones = chord.pitch_class_mask();
    let allowed = tones | (1 << chord.bass_pc());
    let fifth = (chord.root % 12 + 7) % 12;
    let required = tones & !(1 << fifth) | (1 << chord.bass_pc());
    let mut best: Option<(i32, Vec<i32>)> = None;
    for pos in 0..=9 {
        let frets: Vec<i32> = std::iter::once(0).chain((pos.max(1))..=pos + 3).collect();
        for low in 0..3 {
            let Some(bass_fret) = frets.iter().find(|&&f| (GUITAR_STRINGS[low] + f).rem_euclid(12) == i32::from(chord.bass_pc())) else {
                continue;
            };
            // 上の弦は「構成音のうち、まだ鳴っていない音を優先、次に低いフレット」で 1 つずつ決める。
            let mut notes = vec![GUITAR_STRINGS[low] + bass_fret];
            let mut covered: u16 = 1 << chord.bass_pc();
            let mut ok = true;
            for &open in &GUITAR_STRINGS[low + 1..] {
                let pick = frets
                    .iter()
                    .map(|f| open + f)
                    .filter(|p| (allowed >> p.rem_euclid(12)) & 1 == 1)
                    .min_by_key(|p| ((covered >> p.rem_euclid(12)) & 1, *p));
                match pick {
                    Some(p) => {
                        covered |= 1 << p.rem_euclid(12);
                        notes.push(p);
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || covered & required != required {
                continue;
            }
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            // 鳴る弦 1 本 = 4、ポジション 1 つ = 3 (開放の形を、開放弦を混ぜた高いポジションより優先する)。
            let score = notes.len() as i32 * 4 - pos * 3 - low as i32;
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, notes));
            }
        }
    }
    best.map(|(_, v)| to_keys(v))
}

fn basic_pitches(chord: &Chord, base: u8) -> Vec<u8> {
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
        let voiced = voice_progression(picked.iter().map(|c| &c.chord), voicing, DEFAULT_VOICING_BASE);
        for (c, pitches) in picked.iter().zip(voiced) {
            for pitch in pitches {
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
        extract_chords(&notes, opts)
            .into_iter()
            .map(|s| ChordEvent::new(s.start_beat, s.len_beats, s.chord))
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

    /// どの種類 × テンションの組み × ベース × voicing でも鍵盤の数は [`MAX_CHORD_KEYS`] 以下で、上限は
    /// 実際に使い切られうる (テンションを全部載せた 4 和音は 12 音を超える)。
    #[test]
    fn every_voicing_fits_in_max_chord_keys() {
        let mut most = 0usize;
        let prev = chord_pitches(&Chord::new(0, ChordKind::Major), Voicing::Piano, 48);
        for &kind in ChordKind::ALL {
            for bits in 0..(1u16 << Tensions::ALL.len()) {
                for root in 0..12u8 {
                    for bass in [None, Some((root + 1) % 12)] {
                        let chord = Chord { tensions: Tensions(bits), bass, ..Chord::new(root, kind) };
                        let n = [Voicing::Basic, Voicing::Piano, Voicing::Guitar]
                            .into_iter()
                            .map(|v| chord_pitches(&chord, v, DEFAULT_VOICING_BASE).len())
                            .chain((root % 6 == 0).then(|| voice_chord(&chord, Voicing::Adaptive, DEFAULT_VOICING_BASE, Some(&prev)).len()))
                            .max()
                            .unwrap_or(0);
                        assert!(n <= MAX_CHORD_KEYS, "{chord:?}: {n} 音 > {MAX_CHORD_KEYS}");
                        most = most.max(n);
                    }
                }
            }
        }
        assert!(most > 12, "テンションを全部載せると 12 音を超える: {most}");
    }

    /// ギターはローポジションの開放の形 (C = x32010, Am = x02210, G = 320003 系) を選ぶ。
    #[test]
    fn guitar_voicing_picks_open_shapes() {
        let c = chord_pitches(&Chord::new(0, ChordKind::Major), Voicing::Guitar, 48);
        assert_eq!(c, vec![48, 52, 55, 60, 64]);
        let am = chord_pitches(&Chord::new(9, ChordKind::Minor), Voicing::Guitar, 48);
        assert_eq!(am, vec![45, 52, 57, 60, 64]);
        let g = chord_pitches(&Chord::new(7, ChordKind::Major), Voicing::Guitar, 48);
        assert_eq!(g[0], 43, "6 弦 3 フレットの G がベース");
    }

    /// 自動の voicing は、ピアノの固定形より声部の動きが小さく、ベースは常に最低音。
    #[test]
    fn adaptive_voicing_moves_less_than_fixed_piano() {
        // ピアノの固定形 (G3 以上の最初の構成音から) は F# と G の間で 3 音とも跳ぶ。
        let prog: Vec<Chord> = [(6, ChordKind::Major), (7, ChordKind::Major), (6, ChordKind::Major), (0, ChordKind::Major7)]
            .into_iter()
            .map(|(r, k)| Chord::new(r, k))
            .collect();
        let movement = |vs: &[Vec<u8>]| -> i32 {
            vs.windows(2)
                .map(|w| {
                    let (a, b) = (&w[0][1..], &w[1][1..]);
                    b.iter().map(|x| a.iter().map(|y| (i32::from(*x) - i32::from(*y)).abs()).min().unwrap()).sum::<i32>()
                })
                .sum()
        };
        let piano = voice_progression(&prog, Voicing::Piano, 48);
        let adaptive = voice_progression(&prog, Voicing::Adaptive, 48);
        assert!(movement(&adaptive) < movement(&piano), "adaptive {adaptive:?} / piano {piano:?}");
        for (v, c) in adaptive.iter().zip(&prog) {
            assert_eq!(v[0] % 12, c.bass_pc(), "最低音はベース音: {v:?}");
            assert!(v[1..].iter().all(|p| c.contains(*p)), "上声部は構成音: {v:?}");
        }
    }

    /// 箱 → クリップ → 検出で、元の進行に戻る (ノート化と検出が同じコードの定義を見ている)。
    #[test]
    fn chords_survive_a_round_trip_through_a_midi_clip() {
        let mut song = Song { tracks: vec![Track { id: 1, ..Track::default() }], ..Song::default() };
        let progression = [(0.0, 4.0, 0, ChordKind::Major), (4.0, 4.0, 9, ChordKind::Minor7), (8.0, 8.0, 7, ChordKind::Dominant7)];
        for (s, l, root, kind) in progression {
            song.place_chord(ChordEvent::new(s, l, Chord::new(root, kind)));
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
