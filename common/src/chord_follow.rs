//! コードトラックへの追従 (r.md #157、`docs/plan_chord_track.md`) の評価の SSoT。
//!
//! 追従は **非破壊** — ノートのデータは書き換えず、鳴らす音だけを変える。音程を生む全経路 (engine の
//! ノートとオーディオクリップ、GUI の演奏プレビューとピアノロールの重ね表示、VOICEVOX 歌唱、SMF 書き出し、
//! 「鳴る音で確定」) はここにある 2 つだけを引く (経路ごとに式を写さない):
//!
//! - **ノートの鍵盤**: [`follow_pitch`] (書いた音 → 鳴る音)。
//! - **オーディオの移動量**: [`follow_shift`] (半音。全体を平行移動するだけ)。
//!
//! どちらも **書いた音の空間** の値を返す。グローバルトランスポーズ (r.md #130) は、その後に
//! `crate::transpose::sounding_key` で足す (順序は 追従 → 移調。コードも Key も書いた音の空間にある)。
//!
//! コードはノートの**開始位置**で決まり、伸びているノートは途中のコード変更で鳴り直さない
//! (Studio One と同じ)。オーディオの移動量は buffer ごとの今の位置で決まる。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。RT 安全 (確保・ロックなし)。

use crate::model::{Chord, ChordFollow, ChordKind, Song, SourceChord};
use crate::scale::{Scale, ScaleChange};

/// pitch class `from` を `to` へ合わせる最短の移動量 (半音、`-6..=5`)。ちょうど 6 半音は下へ。
#[must_use]
pub fn nearest_delta(from: u8, to: u8) -> i32 {
    let up = (i32::from(to % 12) - i32::from(from % 12)).rem_euclid(12);
    if up >= 6 { up - 12 } else { up }
}

/// `pitch` を、12-bit の pitch class 集合 `mask` (bit 0 = C) の最寄りの音へ寄せる。
/// 同じ距離なら上 ([`ScaleChange::snap`] と同じ規則)。`mask` が空、または届く先が 0..=127 に無ければそのまま。
#[must_use]
pub fn snap_to_mask(pitch: u8, mask: u16) -> u8 {
    let has = |p: i32| (0..=127).contains(&p) && (mask >> (p.rem_euclid(12))) & 1 == 1;
    let p = i32::from(pitch);
    if mask & 0x0FFF == 0 || has(p) {
        return pitch;
    }
    for off in 1..=11 {
        if has(p + off) {
            return (p + off) as u8;
        }
        if has(p - off) {
            return (p - off) as u8;
        }
    }
    pitch
}

/// コードに合うスケール (絶対 pitch class の 12-bit 集合)。「スケール」モードの寄せ先。
///
/// その位置にキーがあり、コードの構成音が全部キーのスケールに入っていれば **キーのスケール**
/// (ダイアトニックなコードではキーの音階へ寄せる)。そうでなければコードの種類から決めた既定のスケール
/// (ルート起点) に構成音を足したもの。
#[must_use]
pub fn chord_scale_mask(chord: &Chord, key: Option<&ScaleChange>) -> u16 {
    let tones = chord.pitch_class_mask();
    if let Some(k) = key {
        let key_mask = rotate12(k.scale.pitch_class_mask(), k.root);
        if tones & !key_mask == 0 {
            return key_mask;
        }
    }
    let scale = match chord.kind {
        ChordKind::Major | ChordKind::Major6 | ChordKind::Major7 | ChordKind::Sus2 | ChordKind::Sus4 => Scale::Major,
        ChordKind::Dominant7 | ChordKind::Dominant7Sus4 => Scale::Mixolydian,
        ChordKind::Minor | ChordKind::Minor7 => Scale::NaturalMinor,
        ChordKind::Minor6 | ChordKind::MinorMajor7 => Scale::MelodicMinor,
        ChordKind::Diminished | ChordKind::Diminished7 => Scale::Diminished,
        ChordKind::HalfDiminished7 => Scale::Locrian,
        ChordKind::Augmented | ChordKind::Augmented7 | ChordKind::AugmentedMajor7 => Scale::WholeTone,
        ChordKind::Power => Scale::MinorPentatonic,
    };
    rotate12(scale.pitch_class_mask(), chord.root) | tones
}

/// ルート起点の 12-bit 集合を絶対 pitch class の集合へ回す。
fn rotate12(mask: u16, root: u8) -> u16 {
    let m = u32::from(mask & 0x0FFF);
    let r = u32::from(root % 12);
    (((m << r) | (m >> (12 - r))) & 0x0FFF) as u16
}

/// 素材のコードが分からないときの基準のルート: その位置のキーのルート、キーも無ければ C。
/// (「素材は曲のキーのトニックの上で書かれている」とみなす。単音のベースラインやメロディ向け。)
#[must_use]
pub fn fallback_source_root(key: Option<&ScaleChange>) -> u8 {
    key.map_or(0, |k| k.root % 12)
}

/// オーディオクリップ (と、ノートの平行移動 / ベース) の移動量 (半音)。
///
/// `target` = その位置のコードトラックのコード、`source` = 素材がもともと鳴らしているコード
/// (`None` なら [`fallback_source_root`])。オーディオは全体を動かすだけなので、`Narrow` / `Scale` も
/// `Parallel` と同じ量を返す。
#[must_use]
pub fn follow_shift(mode: ChordFollow, target: &Chord, source: Option<&Chord>, key: Option<&ScaleChange>) -> i32 {
    let from = source.map_or_else(|| fallback_source_root(key), |s| s.root);
    match mode {
        ChordFollow::Off => 0,
        ChordFollow::Bass => nearest_delta(from, target.bass_pc()),
        ChordFollow::Parallel | ChordFollow::Narrow | ChordFollow::Scale => nearest_delta(from, target.root),
    }
}

/// **ノートの追従の本体** (唯一の式): 書いた鍵盤 `pitch` → 鳴らす鍵盤 (書いた音の空間)。
/// 0..=127 を外れたら `None` (= 鳴らさない。端に丸めない — `crate::transpose::sounding_key` と同じ理由)。
#[must_use]
pub fn follow_pitch(
    mode: ChordFollow,
    pitch: u8,
    target: &Chord,
    source: Option<&Chord>,
    key: Option<&ScaleChange>,
) -> Option<u8> {
    match mode {
        ChordFollow::Off => Some(pitch),
        ChordFollow::Parallel | ChordFollow::Bass => {
            u8::try_from(i32::from(pitch) + follow_shift(mode, target, source, key)).ok().filter(|k| *k <= 127)
        }
        ChordFollow::Narrow => Some(snap_to_mask(pitch, target.pitch_class_mask())),
        ChordFollow::Scale => Some(snap_to_mask(pitch, chord_scale_mask(target, key))),
    }
}

/// MIDI の content (ノート列) が **もともと鳴らしているコード** の区間 (content-local 拍)。平行移動 / ベースの
/// 基準 ([`TrackChordFollow::key`] の `source`) を引く元で、engine の索引 (`SongIndex`) と off-RT の読み口
/// (VOICEVOX / SMF / GUI) が同じこれを通す。ミュートしたノートは数えない。検出の設定は既定
/// ([`crate::chord_detect::DetectOptions::default`]) に固定する (経路ごとに違う基準を引かないため)。
#[must_use]
pub fn note_source_chords(notes: &[crate::model::Note]) -> Vec<SourceChord> {
    let detect: Vec<crate::chord_detect::DetectNote> = notes
        .iter()
        .filter(|n| !n.muted)
        .map(|n| crate::chord_detect::DetectNote { start_beat: n.start_beat, len_beats: n.duration_beats, pitch: n.pitch })
        .collect();
    crate::chord_detect::detect_chords(&detect, &crate::chord_detect::DetectOptions::default())
}

/// トラック `track` が鳴らす MIDI の content (アレンジのクリップとランチャーのセル) の素材のコードを `out` へ足す
/// (既にある content は検出し直さない)。engine の索引 (`SongIndex`) と off-RT の読み口
/// (`crate::transpose::TrackTranspose`) が同じこれで集める。
pub fn collect_source_chords(
    song: &Song,
    track: &crate::model::Track,
    out: &mut std::collections::HashMap<crate::model::ContentId, Vec<SourceChord>>,
) {
    let clips = track.clips.iter().chain(track.session_clips.iter().map(|c| &c.clip));
    for clip in clips {
        if out.contains_key(&clip.content_id) {
            continue;
        }
        if let Some(notes) = song.clip_contents.get(&clip.content_id).and_then(|c| c.notes()) {
            out.insert(clip.content_id, note_source_chords(notes));
        }
    }
}

/// トラック 1 本の追従の読み口 (off-RT と RT の両方)。箱の無い位置と追従しないトラックは書いた音のまま。
///
/// engine は `SongIndex` に焼いた実効モードから [`TrackChordFollow::with_mode`] で作り、オフライン
/// (VOICEVOX / SMF / GUI) は [`TrackChordFollow::of`] で作る。
#[derive(Debug, Clone, Copy)]
pub struct TrackChordFollow<'a> {
    song: &'a Song,
    mode: ChordFollow,
}

impl<'a> TrackChordFollow<'a> {
    /// `track_id` のトラックの読み口 (実効モード = [`Song::track_chord_follow`])。
    #[must_use]
    pub fn of(song: &'a Song, track_id: u32) -> Self {
        Self { song, mode: song.track_chord_follow(track_id) }
    }

    /// 実効モードを渡して作る (engine: 索引に焼いた値)。
    #[must_use]
    pub fn with_mode(song: &'a Song, mode: ChordFollow) -> Self {
        Self { song, mode }
    }

    /// 追従しない読み口 (焼き込み = プロジェクトの中に作るものは書いた音)。
    #[must_use]
    pub fn written(song: &'a Song) -> Self {
        Self { song, mode: ChordFollow::Off }
    }

    #[must_use]
    pub fn mode(&self) -> ChordFollow {
        self.mode
    }

    /// この読み口が音を変えうるか (`false` なら呼び出し側は評価を丸ごと飛ばせる)。
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.mode.is_off() && !self.song.chords.is_empty()
    }

    /// 平行移動 / ベースで素材のコードが要るか (`false` なら `source` は使われない)。
    #[must_use]
    pub fn needs_source(&self) -> bool {
        matches!(self.mode, ChordFollow::Parallel | ChordFollow::Bass)
    }

    /// 曲の拍 `song_beat` で始まるノート `pitch` の鳴らす鍵盤 (書いた音の空間)。
    #[must_use]
    pub fn key(&self, pitch: u8, song_beat: f64, source: Option<&Chord>) -> Option<u8> {
        if self.mode.is_off() {
            return Some(pitch);
        }
        let Some(ev) = self.song.chord_at_onset(song_beat) else { return Some(pitch) };
        // 境界の直前で次の箱を引いたら、Key もその箱の頭で引く (同じ拍にある Key 変更の前の Key と組ませない)。
        follow_pitch(self.mode, pitch, &ev.chord, source, self.song.scale_at(song_beat.max(ev.start_beat)))
    }

    /// 曲の拍 `song_beat` でのオーディオの移動量 (半音)。
    #[must_use]
    pub fn shift(&self, song_beat: f64, source: Option<&Chord>) -> i32 {
        if self.mode.is_off() {
            return 0;
        }
        let Some(ev) = self.song.chord_at(song_beat) else { return 0 };
        follow_shift(self.mode, &ev.chord, source, self.song.scale_at(song_beat))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChordEvent, Track};

    fn song_with(chords: &[(f64, f64, Chord)], mode: ChordFollow) -> Song {
        let mut song =
            Song { tracks: vec![Track { id: 1, follow_chords: mode, ..Track::default() }], ..Song::default() };
        for (start, len, chord) in chords {
            song.place_chord(ChordEvent { id: 0, start_beat: *start, len_beats: *len, chord: *chord, color: None });
        }
        song
    }

    /// C の 3 和音を Am の位置で鳴らすと、モードごとに決まった音になる。箱の無い位置は書いた音のまま。
    #[test]
    fn each_mode_maps_a_c_triad_onto_a_minor() {
        let am = Chord::new(9, ChordKind::Minor);
        let c = Chord::new(0, ChordKind::Major);
        let written = [60u8, 64, 67];
        let sounding = |mode: ChordFollow, beat: f64| -> Vec<Option<u8>> {
            let song = song_with(&[(4.0, 4.0, am)], mode);
            let f = TrackChordFollow::of(&song, 1);
            written.iter().map(|p| f.key(*p, beat, Some(&c))).collect()
        };
        // 平行移動: C → A は 3 半音下。
        assert_eq!(sounding(ChordFollow::Parallel, 4.0), [Some(57), Some(61), Some(64)]);
        // 最寄りのコード音: C と E は構成音なのでそのまま、G は最寄りの A (2 半音上) へ。
        assert_eq!(sounding(ChordFollow::Narrow, 4.0), [Some(60), Some(64), Some(69)]);
        assert_eq!(sounding(ChordFollow::Bass, 4.0), [Some(57), Some(61), Some(64)]);
        // 箱の外 (拍 0) と Off は書いた音。
        assert_eq!(sounding(ChordFollow::Parallel, 0.0), [Some(60), Some(64), Some(67)]);
        assert_eq!(sounding(ChordFollow::Off, 4.0), [Some(60), Some(64), Some(67)]);
    }

    /// 曲全体のスイッチを切ると、トラックのモードは残ったまま全部書いた音に戻る。
    #[test]
    fn the_song_switch_turns_every_track_off() {
        let mut song = song_with(&[(0.0, 4.0, Chord::new(9, ChordKind::Minor))], ChordFollow::Narrow);
        assert_eq!(TrackChordFollow::of(&song, 1).key(67, 0.0, None), Some(69));
        song.chord_follow_enabled = false;
        assert_eq!(TrackChordFollow::of(&song, 1).key(67, 0.0, None), Some(67));
        assert_eq!(song.tracks[0].follow_chords, ChordFollow::Narrow);
    }

    /// スケール: ダイアトニックなコードはキーの音階へ、キーに無いコードはコードの種類のスケールへ寄せる。
    #[test]
    fn scale_mode_prefers_the_key_for_diatonic_chords() {
        let key = ScaleChange { beat: 0.0, root: 0, scale: Scale::Major };
        // C メジャーキーの Am: C# は C か D へ (同距離なら上の D)。
        let am = Chord::new(9, ChordKind::Minor);
        assert_eq!(follow_pitch(ChordFollow::Scale, 61, &am, None, Some(&key)), Some(62));
        // キーに無い E メジャー (G# を持つ): E メジャースケールへ寄せるので F は F# へ。
        let e = Chord::new(4, ChordKind::Major);
        assert_eq!(follow_pitch(ChordFollow::Scale, 65, &e, None, Some(&key)), Some(66));
    }

    /// 箱の境界の直前 (拍の丸め) に始まるノートは次の箱を引く。同じ拍に Key の変更があれば Key も変更後を引く:
    /// D は C メジャーキーに無い (D メジャースケールへ寄せ、C# はそのまま) が、G メジャーキーにはある (C# は D へ)。
    #[test]
    fn a_note_just_before_a_boundary_takes_the_key_change_on_that_boundary() {
        let (c, d) = (Chord::new(0, ChordKind::Major), Chord::new(2, ChordKind::Major));
        let mut song = song_with(&[(0.0, 4.0, c), (4.0, 4.0, d)], ChordFollow::Scale);
        song.scale_changes = vec![
            ScaleChange { beat: 0.0, root: 0, scale: Scale::Major },
            ScaleChange { beat: 4.0, root: 7, scale: Scale::Major },
        ];
        let f = TrackChordFollow::of(&song, 1);
        assert_eq!(f.key(61, 4.0, None), Some(62));
        assert_eq!(f.key(61, 4.0 - 1e-9, None), Some(62));
    }

    /// 素材のコードが無ければ、キーのトニック (無ければ C) を基準にする。範囲外へ出たら鳴らさない。
    #[test]
    fn missing_source_falls_back_to_the_key_and_out_of_range_is_silent() {
        let g = Chord::new(7, ChordKind::Major);
        assert_eq!(follow_shift(ChordFollow::Parallel, &g, None, None), -5);
        let key = ScaleChange { beat: 0.0, root: 5, scale: Scale::Major };
        assert_eq!(follow_shift(ChordFollow::Parallel, &g, None, Some(&key)), 2);
        assert_eq!(follow_pitch(ChordFollow::Parallel, 2, &g, None, None), None);
    }
}
