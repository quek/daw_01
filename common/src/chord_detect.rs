//! ノート列からのコード検出 (r.md #157、`docs/plan_chord_track.md`)。
//!
//! 使い道は 2 つで、どちらも同じ [`detect_chords`] を通す:
//! - **コードトラックへの取り込み** (クリップを帯へドラッグ / 「コードを抽出」)。
//! - **平行移動 / ベースの追従の基準** — 素材がもともと鳴らしているコード ([`source_chord_at`])。
//!   MIDI はノートから毎回導けるので保存しない (保存すると、ノートを編集するたびに食い違う)。
//!
//! オーディオの検出は chroma (pitch class ごとの強さ) を作ったあと、同じ [`match_chord`] に渡す。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use crate::model::{AudioChordSpan, Chord, ChordKind, SourceChord, Spelling, Tensions};

/// 検出の設定 (Cubase の Create Chord Events ダイアログに相当)。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DetectOptions {
    /// 1 コードとして見る窓の長さ (拍)。分散和音 (アルペジオ) は窓を長く取ると 1 つのコードにまとまる。
    pub window_beats: f64,
    /// 最低音がルートでなければベース音 (`C/E`) として付ける。
    pub include_bass: bool,
    /// 構成音以外の音をテンションとして付ける。
    pub include_tensions: bool,
    /// これより短いノート (拍) は無視する (装飾音・ゴーストノート)。
    pub ignore_shorter_than_beats: f64,
}

impl Default for DetectOptions {
    fn default() -> Self {
        Self { window_beats: 2.0, include_bass: true, include_tensions: false, ignore_shorter_than_beats: 0.0 }
    }
}

/// 検出に渡すノート 1 個 (位置は呼び出し側の拍: content-local でも song-absolute でもよい)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectNote {
    pub start_beat: f64,
    pub len_beats: f64,
    pub pitch: u8,
}

/// コードとして認める最小の pitch class の数。1〜2 音 (単音のメロディ / ベースライン / 2 声) からは
/// コードを決めない (決めると、メロディの 1 音ごとに別のコードが出る)。
const MIN_PITCH_CLASSES: u32 = 3;

/// pitch class ごとの強さ `weights` (index 0 = C) と最低音の pitch class からコードを 1 つ決める。
/// 鳴っている pitch class が [`MIN_PITCH_CLASSES`] 未満なら `None`。
///
/// 全ルート × 全種類のひな形と突き合わせ、「構成音の強さ − 構成音でない音の強さ − 欠けた構成音の罰」が
/// 最大のものを選ぶ。同点なら [`ChordKind::ALL`] の並びで先 (= 単純な種類)、最低音がルートのものを優先する。
#[must_use]
pub fn match_chord(weights: &[f64; 12], lowest_pc: Option<u8>, opts: &DetectOptions) -> Option<Chord> {
    let total: f64 = weights.iter().sum();
    if !total.is_finite() || total <= 0.0 {
        return None;
    }
    // 窓の強さの 5% に満たない pitch class は「鳴っていない」とみなす (経過音・検出の漏れ)。
    let floor = total * 0.05;
    let present: u16 = (0..12).filter(|i| weights[*i] > floor).fold(0, |m, i| m | (1 << i));
    if present.count_ones() < MIN_PITCH_CLASSES {
        return None;
    }
    let mean = total / f64::from(present.count_ones());
    let mut best: Option<(f64, Chord)> = None;
    for root in 0..12u8 {
        for &kind in ChordKind::ALL {
            let chord = Chord::new(root, kind);
            let tones = chord.pitch_class_mask();
            let mut score = 0.0;
            for (pc, &w) in weights.iter().enumerate() {
                let in_chord = (tones >> pc) & 1 == 1;
                match (in_chord, (present >> pc) & 1 == 1) {
                    (true, true) => score += w,
                    (true, false) => score -= mean,
                    (false, true) => score -= w,
                    (false, false) => {}
                }
            }
            // 最低音がルートのコードを少しだけ優先する (C6 と Am7 のように構成音が同じ組を決める)。
            if lowest_pc == Some(root) {
                score += mean * 0.25;
            }
            if best.as_ref().is_none_or(|(s, _)| score > *s + 1e-9) {
                best = Some((score, chord));
            }
        }
    }
    let (_, mut chord) = best?;
    if opts.include_bass
        && let Some(low) = lowest_pc
        && low != chord.root
    {
        chord.bass = Some(low);
    }
    if opts.include_tensions {
        let tones = chord.pitch_class_mask();
        for &(t, _, semis) in Tensions::ALL {
            // 5 度の変化 (b5 / #5) は種類の側で表すので、ここでは足す音だけを見る。
            if t == Tensions::FLAT5 || t == Tensions::SHARP5 {
                continue;
            }
            let pc = usize::from((chord.root + semis) % 12);
            if (present >> pc) & 1 == 1 && (tones >> pc) & 1 == 0 {
                chord.tensions = chord.tensions.with(t);
            }
        }
    }
    chord.spelling = Spelling::Auto;
    Some(chord.sanitized())
}

/// ノート列からコードの区間の列を作る (`start_beat` 昇順・非交差、同じコードが続く窓は 1 つにまとめる)。
///
/// 窓は `opts.window_beats` 刻み (拍 0 起点)。窓ごとに、ノートが窓と重なる長さを pitch class の強さとして
/// [`match_chord`] に渡す。コードが決まらない窓 (無音・単音) は区間を作らない。
#[must_use]
pub fn detect_chords(notes: &[DetectNote], opts: &DetectOptions) -> Vec<SourceChord> {
    let window = if opts.window_beats.is_finite() && opts.window_beats > 0.0 { opts.window_beats } else { 2.0 };
    let usable = |n: &&DetectNote| {
        n.start_beat.is_finite()
            && n.len_beats.is_finite()
            && n.len_beats > 0.0
            && n.len_beats >= opts.ignore_shorter_than_beats
    };
    let Some(end) = notes.iter().filter(usable).map(|n| n.start_beat + n.len_beats).reduce(f64::max) else {
        return Vec::new();
    };
    let first = notes.iter().filter(usable).map(|n| n.start_beat).fold(f64::INFINITY, f64::min);
    let mut out: Vec<SourceChord> = Vec::new();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (w0, w1) = ((first / window).floor().max(0.0) as u64, (end / window).ceil().max(0.0) as u64);
    for w in w0..w1 {
        #[allow(clippy::cast_precision_loss)]
        let (a, b) = (w as f64 * window, (w + 1) as f64 * window);
        let mut weights = [0.0f64; 12];
        let mut lowest: Option<u8> = None;
        for n in notes.iter().filter(usable) {
            let overlap = (n.start_beat + n.len_beats).min(b) - n.start_beat.max(a);
            if overlap <= 0.0 {
                continue;
            }
            weights[usize::from(n.pitch % 12)] += overlap;
            lowest = Some(lowest.map_or(n.pitch, |l| l.min(n.pitch)));
        }
        let Some(chord) = match_chord(&weights, lowest.map(|p| p % 12), opts) else { continue };
        match out.last_mut() {
            Some(last) if last.chord == chord && (last.start_beat + last.len_beats - a).abs() < 1e-9 => {
                last.len_beats += window;
            }
            _ => out.push(SourceChord { start_beat: a, len_beats: window, chord }),
        }
    }
    out
}

/// 拍 `beat` で素材が鳴らしているコード: `beat` を覆う区間、無ければ直前の区間、それも無ければ最初の区間。
/// `spans` は `start_beat` 昇順。空なら `None` (呼び出し側は `crate::chord_follow::fallback_source_root` に倒す)。
///
/// RT 安全 (二分探索、確保なし)。
#[must_use]
pub fn source_chord_at(spans: &[SourceChord], beat: f64) -> Option<&Chord> {
    let i = spans.partition_point(|s| s.start_beat <= beat);
    spans.get(i.saturating_sub(1)).map(|s| &s.chord)
}

/// 音源のフレーム `frame` でオーディオ素材が鳴らしているコード ([`source_chord_at`] のオーディオ版: 覆う区間、
/// 無ければ直前、それも無ければ最初)。`spans` は `Song.audio_chords[source_id]` (`start_frame` 昇順)。
///
/// RT 安全 (二分探索、確保なし)。
#[must_use]
pub fn audio_chord_at(spans: &[AudioChordSpan], frame: u64) -> Option<&Chord> {
    let i = spans.partition_point(|s| s.start_frame <= frame);
    spans.get(i.saturating_sub(1)).map(|s| &s.chord)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(start: f64, len: f64, pitch: u8) -> DetectNote {
        DetectNote { start_beat: start, len_beats: len, pitch }
    }

    /// C → Am → F/A → G7 のブロックコードと分散和音が、それぞれのコードとして取れる。単音の区間は取らない。
    #[test]
    fn block_chords_and_arpeggios_are_named() {
        let mut notes = Vec::new();
        // 拍 0-2: C (ブロック)
        notes.extend([n(0.0, 2.0, 48), n(0.0, 2.0, 60), n(0.0, 2.0, 64), n(0.0, 2.0, 67)]);
        // 拍 2-4: Am (分散和音 A C E)
        notes.extend([n(2.0, 0.5, 57), n(2.5, 0.5, 60), n(3.0, 0.5, 64), n(3.5, 0.5, 60)]);
        // 拍 4-6: F の第 1 転回 (最低音 A)
        notes.extend([n(4.0, 2.0, 57), n(4.0, 2.0, 60), n(4.0, 2.0, 65)]);
        // 拍 6-8: G7
        notes.extend([n(6.0, 2.0, 55), n(6.0, 2.0, 59), n(6.0, 2.0, 62), n(6.0, 2.0, 65)]);
        // 拍 8-10: 単音 (コードにしない)
        notes.push(n(8.0, 2.0, 72));
        let spans = detect_chords(&notes, &DetectOptions::default());
        let got: Vec<(f64, f64, u8, ChordKind, Option<u8>)> =
            spans.iter().map(|s| (s.start_beat, s.len_beats, s.chord.root, s.chord.kind, s.chord.bass)).collect();
        assert_eq!(
            got,
            vec![
                (0.0, 2.0, 0, ChordKind::Major, None),
                (2.0, 2.0, 9, ChordKind::Minor, None),
                (4.0, 2.0, 5, ChordKind::Major, Some(9)),
                (6.0, 2.0, 7, ChordKind::Dominant7, None),
            ]
        );
        // 単音の区間 (拍 9) は直前のコードを基準にする。
        assert_eq!(source_chord_at(&spans, 9.0).map(|c| c.root), Some(7));
        assert_eq!(source_chord_at(&[], 0.0), None);
    }

    /// 同じコードが続く窓は 1 つの区間にまとまり、テンションは設定を入れたときだけ付く。
    #[test]
    fn repeated_windows_merge_and_tensions_are_optional() {
        let notes = [n(0.0, 4.0, 60), n(0.0, 4.0, 64), n(0.0, 4.0, 67), n(0.0, 4.0, 74)];
        let plain = detect_chords(&notes, &DetectOptions::default());
        assert_eq!(plain.len(), 1);
        assert_eq!((plain[0].len_beats, plain[0].chord.kind, plain[0].chord.tensions), (4.0, ChordKind::Major, Tensions::NONE));
        let with = detect_chords(&notes, &DetectOptions { include_tensions: true, ..DetectOptions::default() });
        assert_eq!(with[0].chord.tensions, Tensions::NINE);
    }
}
