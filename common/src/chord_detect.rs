//! ノート列からのコード検出 (r.md #157、`docs/plan_chord_track.md`)。
//!
//! 使い道は 2 つで、どちらも同じ「鳴り始めで区切ってコードを決める」手順 ([`extract_chords`] の手順 1) を通す:
//! - **コードトラックへの取り込み** (クリップを帯へドラッグ / 「コードを抽出」、[`extract_chords`])。
//! - **追従の基準になる自分の和音** (r.md #159、[`own_chords`]) — 素材が **一緒に弾いている** 和音だけ。
//!   窓で集めた旋律の音 (偶然コードに見えるもの) は和音にしない。MIDI はノートから毎回導けるので保存しない
//!   (保存すると、ノートを編集するたびに食い違う)。
//!
//! オーディオの検出は chroma (pitch class ごとの強さ) を作ったあと、同じ [`match_chord`] に渡す。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use crate::model::{AudioChordSpan, Chord, ChordKind, SourceChord, Spelling, Tensions};

/// 検出の設定 (Cubase の Create Chord Events ダイアログに相当)。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DetectOptions {
    /// 1 コードの長さをノートから決める ([`extract_chords`]: 和音の鳴り始めで区切る)。`false` なら
    /// `window_beats` 刻みの窓で見る。素材の基準コード ([`detect_chords`]) はこれを見ない (常に窓)。
    pub auto_length: bool,
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
        Self {
            auto_length: true,
            window_beats: 2.0,
            include_bass: true,
            include_tensions: false,
            ignore_shorter_than_beats: 0.0,
        }
    }
}

/// 長さが自動のときにオーディオを見る窓 (拍)。
pub const AUTO_AUDIO_WINDOW_BEATS: f64 = 2.0;

/// 窓の長さ (拍) の有効域。下限は窓の数 (= 計算量) を抑え、上限は「1 コード」として意味のある長さ。
pub const WINDOW_BEATS_RANGE: std::ops::RangeInclusive<f64> = 0.25..=64.0;

impl DetectOptions {
    /// 有効域へ揃えた設定 (disk / script / 呼び出し側から来た値)。非有限・0 以下の窓は既定、範囲外は
    /// [`WINDOW_BEATS_RANGE`] に収める。[`detect_chords`] は必ずこれを通してから使う。
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        if !(self.window_beats.is_finite() && self.window_beats > 0.0) {
            self.window_beats = Self::default().window_beats;
        }
        self.window_beats = self.window_beats.clamp(*WINDOW_BEATS_RANGE.start(), *WINDOW_BEATS_RANGE.end());
        if !(self.ignore_shorter_than_beats.is_finite() && self.ignore_shorter_than_beats >= 0.0) {
            self.ignore_shorter_than_beats = 0.0;
        }
        self
    }

    /// オーディオの検出で見る窓 (拍)。オーディオには区切りにできるノートの鳴り始めが無いので、長さが自動の
    /// ときは [`AUTO_AUDIO_WINDOW_BEATS`] 刻みで見る (同じコードが続く窓は 1 つにまとまる)。
    #[must_use]
    pub fn audio_window_beats(&self) -> f64 {
        if self.auto_length { AUTO_AUDIO_WINDOW_BEATS } else { self.window_beats }
    }
}

/// [`detect_chords`] が 1 回で見る窓の数の上限 (壊れた値で計算が終わらないための防御。既定の 2 拍で
/// 約 26 万拍、最小の窓でも 3 万拍あり、実際の曲では届かない)。
const MAX_WINDOWS: usize = 1 << 17;

/// 検出に渡すノート 1 個 (位置は呼び出し側の拍: content-local でも song-absolute でもよい)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectNote {
    pub start_beat: f64,
    pub len_beats: f64,
    pub pitch: u8,
}

/// コードとして認める最小の pitch class の数。1〜2 音 (単音のメロディ / ベースライン / 2 声) からは
/// コードを決めない (決めると、メロディの 1 音ごとに別のコードが出る)。
/// pitch class が 2 つでも、3 音以上が同時に鳴っていれば和音の押さえ方 (ルートと 3 度 / 5 度をオクターブで
/// 重ねた形) なので [`match_dyad`] が読む。
const MIN_PITCH_CLASSES: u32 = 3;

/// 2 つの pitch class を和音として読むのに要る、同時に鳴るノートの数 (2 声の旋律と区別する)。
const DYAD_MIN_POLYPHONY: usize = 3;

/// `weights` のうち「鳴っている」とみなす pitch class の集合 (12-bit) と強さの合計。
/// 窓の強さの 5% に満たない pitch class は数えない (経過音・検出の漏れ)。
fn present_mask(weights: &[f64; 12]) -> Option<(u16, f64)> {
    let total: f64 = weights.iter().sum();
    if !total.is_finite() || total <= 0.0 {
        return None;
    }
    let floor = total * 0.05;
    Some(((0..12).filter(|i| weights[*i] > floor).fold(0, |m, i| m | (1 << i)), total))
}

/// pitch class が 2 つだけの和音 (オクターブで重ねたルート + 3 度 / 5 度) を、最低音を手がかりに読む:
/// 最低音から短 3 度 → マイナー、長 3 度 → メジャー、完全 5 度 → パワーコード。転回 (最低音が 3 度 / 5 度) は
/// 上の音をルートにして、最低音をベース音にする。それ以外の音程 (2 度・増 4 度・7 度) は決めない。
#[must_use]
pub fn match_dyad(weights: &[f64; 12], lowest_pc: Option<u8>, opts: &DetectOptions) -> Option<Chord> {
    let (present, _) = present_mask(weights)?;
    let low = lowest_pc? % 12;
    if present.count_ones() != 2 || (present >> low) & 1 == 0 {
        return None;
    }
    let other = (present & !(1 << low)).trailing_zeros() as u8;
    let (root, kind) = match (other + 12 - low) % 12 {
        3 => (low, ChordKind::Minor),
        4 => (low, ChordKind::Major),
        7 => (low, ChordKind::Power),
        5 => (other, ChordKind::Power),
        8 => (other, ChordKind::Major),
        9 => (other, ChordKind::Minor),
        _ => return None,
    };
    let mut chord = Chord::new(root, kind);
    if opts.include_bass && root != low {
        chord.bass = Some(low);
    }
    chord.spelling = Spelling::Auto;
    Some(chord.sanitized())
}

/// コードの種類の数 ([`match_chord`] の表の大きさ)。
const KIND_COUNT: usize = ChordKind::ALL.len();

/// pitch class ごとの強さ `weights` (index 0 = C) と最低音の pitch class からコードを 1 つ決める。
/// 鳴っている pitch class が [`MIN_PITCH_CLASSES`] 未満なら `None`。
///
/// 全ルート × 全種類のひな形と突き合わせ、「構成音の強さ − 構成音でない音の強さ − 欠けた構成音の罰」が
/// 最大のものを選ぶ。同点なら [`ChordKind::ALL`] の並びで先 (= 単純な種類)、最低音がルートのものを優先する。
#[must_use]
pub fn match_chord(weights: &[f64; 12], lowest_pc: Option<u8>, opts: &DetectOptions) -> Option<Chord> {
    let (present, total) = present_mask(weights)?;
    if present.count_ones() < MIN_PITCH_CLASSES {
        return None;
    }
    let mean = total / f64::from(present.count_ones());
    // 「構成音の強さ − 構成音でない音の強さ − 欠けた構成音の罰」を、鳴っている構成音の強さ `hit` だけから出す:
    // 2 × hit − (鳴っている音の強さの合計) − mean × 欠けた構成音の数 (12 音を毎回なめない — 窓ごとに呼ばれる)。
    let present_sum: f64 = sum_bits(weights, present);
    // 種類ごとの構成音 (ルート C) を 1 回だけ作り、ルートぶん回して使う。
    let mut kind_masks = [0u16; KIND_COUNT];
    for (m, &kind) in kind_masks.iter_mut().zip(ChordKind::ALL) {
        *m = Chord::new(0, kind).pitch_class_mask();
    }
    let mut best: Option<(f64, u8, ChordKind)> = None;
    for root in 0..12u8 {
        let r = u32::from(root);
        for (&kind, &m) in ChordKind::ALL.iter().zip(&kind_masks) {
            let tones = (((u32::from(m) << r) | (u32::from(m) >> (12 - r))) & 0x0FFF) as u16;
            let missing = (tones & !present & 0x0FFF).count_ones();
            let mut score = 2.0 * sum_bits(weights, tones & present) - present_sum - mean * f64::from(missing);
            // 最低音がルートのコードを少しだけ優先する (C6 と Am7 のように構成音が同じ組を決める)。
            if lowest_pc == Some(root) {
                score += mean * 0.25;
            }
            if best.as_ref().is_none_or(|(s, _, _)| score > *s + 1e-9) {
                best = Some((score, root, kind));
            }
        }
    }
    let (_, root, kind) = best?;
    let mut chord = Chord::new(root, kind);
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

/// `mask` (12-bit) の立っている pitch class の強さの和 (低い pitch class から足す)。
fn sum_bits(weights: &[f64; 12], mut mask: u16) -> f64 {
    let mut sum = 0.0;
    while mask != 0 {
        sum += weights[mask.trailing_zeros() as usize];
        mask &= mask - 1;
    }
    sum
}

/// 検出に使うノート (位置と長さが有効で、無視する長さ以上) を開始順に並べる。
fn usable_notes<'a>(notes: &'a [DetectNote], opts: &DetectOptions) -> Vec<&'a DetectNote> {
    let mut sorted: Vec<&DetectNote> = notes
        .iter()
        .filter(|n| {
            n.start_beat.is_finite()
                && n.len_beats.is_finite()
                && n.len_beats > 0.0
                && n.len_beats >= opts.ignore_shorter_than_beats
        })
        .collect();
    sorted.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
    sorted
}

/// 区間 `[a, b)` に掛かるノートから決めたコード: 区間と重なる長さを pitch class の強さとして
/// [`match_chord`] に渡し、pitch class が 2 つで 3 音以上が同時に鳴るなら [`match_dyad`] で読む。
fn chord_in(notes: &[&DetectNote], a: f64, b: f64, opts: &DetectOptions) -> Option<Chord> {
    let mut weights = [0.0f64; 12];
    let mut lowest: Option<u8> = None;
    for n in notes {
        let overlap = (n.start_beat + n.len_beats).min(b) - n.start_beat.max(a);
        if overlap <= 0.0 {
            continue;
        }
        weights[usize::from(n.pitch % 12)] += overlap;
        lowest = Some(lowest.map_or(n.pitch, |l| l.min(n.pitch)));
    }
    let low_pc = lowest.map(|p| p % 12);
    match_chord(&weights, low_pc, opts).or_else(|| {
        let chord = match_dyad(&weights, low_pc, opts)?;
        (max_polyphony(notes, a, b) >= DYAD_MIN_POLYPHONY).then_some(chord)
    })
}

/// 区間 `[a, b)` の中で同時に鳴るノートの数の最大。
fn max_polyphony(notes: &[&DetectNote], a: f64, b: f64) -> usize {
    let mut edges: Vec<(f64, i32)> = Vec::with_capacity(notes.len() * 2);
    for n in notes {
        let (s, e) = (n.start_beat.max(a), (n.start_beat + n.len_beats).min(b));
        if e > s {
            edges.push((s, 1));
            edges.push((e, -1));
        }
    }
    // 同じ位置では終わりを先に数える (つながったノートを重なりに数えない)。
    edges.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
    let (mut cur, mut max) = (0i32, 0i32);
    for (_, d) in edges {
        cur += d;
        max = max.max(cur);
    }
    usize::try_from(max).unwrap_or(0)
}

/// 鳴り始めのずれをひとつの和音とみなす幅 (拍。手弾きのばらつき)。
const ONSET_EPS_BEATS: f64 = 1.0 / 16.0;
/// ずらして弾いた和音をひとつにまとめる幅の上限 (拍。最初の音から最後の音まで)。
const ONSET_GROUP_MAX_BEATS: f64 = 0.25;
/// 和音の鳴り始めで区切れない区間 (分散和音・旋律) を見る窓 (拍): この長さから始め、前半と後半で別のコードが
/// 出るあいだ半分に割っていく (下限まで)。
const ARP_WINDOW_BEATS: f64 = 4.0;
const ARP_MIN_WINDOW_BEATS: f64 = 1.0;

/// コードトラックへ取り込むコードの列 (「コードを抽出」/ クリップ・MIDI ファイルのドロップ)。
/// `opts.auto_length` が切れていれば [`detect_chords`] (固定の窓)。入っていれば 1 コードの長さをノートから決める:
///
/// 1. ノートの鳴り始め (と無音の始まり) で区切り、区間ごとにコードを決める — 同時に押さえた和音は、
///    その長さのまま 1 つの区間になる。
/// 2. コードが決まらなかった区間 (分散和音・旋律) は、[`ARP_WINDOW_BEATS`] の窓から割っていって決める。
/// 3. 同じコードが続く区間は 1 つにまとめる。
#[must_use]
pub fn extract_chords(notes: &[DetectNote], opts: &DetectOptions) -> Vec<SourceChord> {
    let opts = opts.sanitized();
    if !opts.auto_length {
        return detect_chords(notes, &opts);
    }
    let sorted = usable_notes(notes, &opts);
    let OnsetChords { mut found, gaps } = onset_chords(&sorted, &opts, false);
    // 区間は昇順なので、窓ごとに「窓に掛かるノート」だけを開始順の sweep で渡す (区間ごと・窓ごとに全ノートを
    // 走査しない)。窓の数は全区間の合計で [`MAX_WINDOWS`] まで。
    let mut active: Vec<&DetectNote> = Vec::new();
    let mut next = 0usize;
    let mut budget = MAX_WINDOWS;
    for (g0, g1) in gaps {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let mut cell = (g0 / ARP_WINDOW_BEATS).floor().max(0.0) as u64;
        while budget > 0 {
            #[allow(clippy::cast_precision_loss)]
            let a = cell as f64 * ARP_WINDOW_BEATS;
            if a >= g1 {
                break;
            }
            budget -= 1;
            let (lo, hi) = (a.max(g0), (a + ARP_WINDOW_BEATS).min(g1));
            while let Some(n) = sorted.get(next).filter(|n| n.start_beat < hi) {
                active.push(n);
                next += 1;
            }
            active.retain(|n| n.start_beat + n.len_beats > lo);
            split_window(&active, (a, a + ARP_WINDOW_BEATS), (g0, g1), &opts, &mut found);
            let Some(following) = cell.checked_add(1) else { break };
            cell = following;
        }
    }
    found.sort_by(|x, y| x.0.total_cmp(&y.0));
    merge_runs(found)
}

/// `(a, b, chord)` の昇順の列を、同じコードが続く区間を 1 つにまとめて [`SourceChord`] の列にする。
fn merge_runs(found: Vec<(f64, f64, Chord)>) -> Vec<SourceChord> {
    let mut out: Vec<SourceChord> = Vec::with_capacity(found.len());
    for (a, b, chord) in found {
        match out.last_mut() {
            Some(last) if last.chord == chord && (last.start_beat + last.len_beats - a).abs() < 1e-9 => {
                last.len_beats = b - last.start_beat;
            }
            _ => out.push(SourceChord { start_beat: a, len_beats: b - a, chord }),
        }
    }
    out
}

/// [`onset_chords`] の結果: コードが決まった区間と、決まらなかった区間 (つながっていれば 1 つ)。
struct OnsetChords {
    found: Vec<(f64, f64, Chord)>,
    gaps: Vec<(f64, f64)>,
}

/// [`extract_chords`] の手順 1: ノートの鳴り始め (と無音の始まり) で区切り、区間ごとにコードを決める
/// (ずらして弾いた和音は頭にまとめる)。`sorted` は [`usable_notes`] の結果 (開始順)。
///
/// `played_together` が立っていれば、区間に **一緒に弾いた和音** ([`played_together`]) が無い区間はコードに
/// しない (r.md #159 の自分の和音。重ねて弾いた旋律の音を和音と読まない)。
fn onset_chords(sorted: &[&DetectNote], opts: &DetectOptions, require_together: bool) -> OnsetChords {
    // 区切り: 鳴り始め (近いものは 1 つ) と、鳴っている音が無くなる位置。
    let mut bounds: Vec<f64> = Vec::new();
    let mut max_end = f64::NEG_INFINITY;
    // 直前の鳴り始め。ずらして弾いた和音 (ストローク / ロール) は、隣どうしが近ければ頭の 1 つにまとめる
    // (頭からの幅は [`ONSET_GROUP_MAX_BEATS`] まで — 速い連打を 1 つの和音にしない)。
    let mut prev_onset = f64::NEG_INFINITY;
    for n in sorted {
        if max_end.is_finite() && n.start_beat > max_end + ONSET_EPS_BEATS {
            bounds.push(max_end);
        }
        let joins = n.start_beat - prev_onset <= ONSET_EPS_BEATS
            && bounds.last().is_some_and(|head| n.start_beat - head <= ONSET_GROUP_MAX_BEATS);
        if !joins {
            bounds.push(n.start_beat);
        }
        prev_onset = n.start_beat;
        max_end = max_end.max(n.start_beat + n.len_beats);
    }
    if max_end.is_finite() && bounds.last().is_some_and(|l| max_end > *l) {
        bounds.push(max_end);
    }
    let mut found: Vec<(f64, f64, Chord)> = Vec::new();
    // コードが決まらなかった区間 (つながっていれば 1 つ)。
    let mut gaps: Vec<(f64, f64)> = Vec::new();
    let mut active: Vec<&DetectNote> = Vec::new();
    let mut next = 0usize;
    for w in bounds.windows(2) {
        let (a, b) = (w[0], w[1]);
        while let Some(n) = sorted.get(next).filter(|n| n.start_beat < b) {
            active.push(n);
            next += 1;
        }
        active.retain(|n| n.start_beat + n.len_beats > a);
        if active.is_empty() {
            continue;
        }
        let chord = if require_together && !played_together(&active) { None } else { chord_in(&active, a, b, opts) };
        match chord {
            Some(chord) => found.push((a, b, chord)),
            None => match gaps.last_mut() {
                Some(last) if last.1 == a => last.1 = b,
                _ => gaps.push((a, b)),
            },
        }
    }
    OnsetChords { found, gaps }
}

/// `active` (1 区間に掛かるノート) に **一緒に弾いた和音** があるか: 鳴り始めか終わりが [`ONSET_GROUP_MAX_BEATS`]
/// 以内に揃った音の組で、pitch class が [`MIN_PITCH_CLASSES`] 以上 (2 つなら [`DYAD_MIN_POLYPHONY`] 音以上)。
///
/// 和音 (ブロック / ストローク / 伸ばした分散和音 = 終わりが揃う) と、音を重ねて弾いた旋律 (レガート /
/// ビブラフォン風に鳴らし続ける旋律 = 始まりも終わりもずれている) を分ける。
fn played_together(active: &[&DetectNote]) -> bool {
    let chordal = |group: &[(f64, u8)]| {
        let mask = group.iter().fold(0u16, |m, (_, p)| m | (1 << (p % 12)));
        let pcs = mask.count_ones();
        pcs >= MIN_PITCH_CLASSES || (pcs == 2 && group.len() >= DYAD_MIN_POLYPHONY)
    };
    let aligned = |mut edges: Vec<(f64, u8)>| {
        edges.sort_by(|a, b| a.0.total_cmp(&b.0));
        (0..edges.len()).any(|i| {
            let head = edges[i].0;
            let len = edges[i..].iter().take_while(|(t, _)| t - head <= ONSET_GROUP_MAX_BEATS).count();
            chordal(&edges[i..i + len])
        })
    };
    aligned(active.iter().map(|n| (n.start_beat, n.pitch)).collect())
        || aligned(active.iter().map(|n| (n.start_beat + n.len_beats, n.pitch)).collect())
}

/// 素材が **自分で弾いている和音** の区間 (r.md #159、追従の基準)。位置はノートと同じ拍 (MIDI の content-local)。
///
/// [`extract_chords`] と同じ区切り (鳴り始めと無音) で、区間に一緒に弾いた和音 ([`played_together`]) があるものだけ
/// (分散和音の窓 = 手順 2 は通さない — 窓で集めた旋律の音を和音にしない)。設定は既定に固定する (経路ごとに違う
/// 基準を引かない)。区間は昇順・非交差で、同じコードが続けば 1 つにまとまる。
#[must_use]
pub fn own_chords(notes: &[DetectNote]) -> Vec<SourceChord> {
    let opts = DetectOptions::default();
    let sorted = usable_notes(notes, &opts);
    merge_runs(onset_chords(&sorted, &opts, true).found)
}

/// 自分の和音の区間を引くときの許容幅 (拍。ノートの位置の浮動小数の丸めで、区間の終わりの数 ulp 手前に
/// 始まるノートを前の和音に入れない)。
const OWN_EPS_BEATS: f64 = 1e-6;

/// 拍 `beat` (ノートの頭) で鳴っている自分の和音の区間 (覆うもの。前の区間を持ち越さない)。`spans` は
/// [`own_chords`] の結果。
///
/// RT 安全 (二分探索、確保なし)。
#[must_use]
pub fn own_chord_at(spans: &[SourceChord], beat: f64) -> Option<&SourceChord> {
    let i = spans.partition_point(|s| s.start_beat <= beat + OWN_EPS_BEATS);
    spans.get(i.checked_sub(1)?).filter(|s| beat + OWN_EPS_BEATS < s.start_beat + s.len_beats)
}

/// `[a, b)` (コードの箱の範囲) と重なりがいちばん長い自分の和音の区間 (同じ長さなら先)。
///
/// RT 安全 (二分探索と、範囲に掛かる区間だけの走査)。
#[must_use]
pub fn longest_own_in(spans: &[SourceChord], a: f64, b: f64) -> Option<&SourceChord> {
    let i = spans.partition_point(|s| s.start_beat + s.len_beats <= a);
    let mut best: Option<(&SourceChord, f64)> = None;
    for s in spans[i..].iter().take_while(|s| s.start_beat < b) {
        let overlap = (s.start_beat + s.len_beats).min(b) - s.start_beat.max(a);
        if overlap > 1e-9 && best.is_none_or(|(_, o)| overlap > o + 1e-9) {
            best = Some((s, overlap));
        }
    }
    best.map(|(s, _)| s)
}

/// 窓 `cell` (を区間 `gap` で切ったもの) のコードを `out` に足す。前半と後半で別のコードが出るか、窓全体では
/// 決まらないなら、半分に割って見直す ([`ARP_MIN_WINDOW_BEATS`] まで)。
fn split_window(
    notes: &[&DetectNote],
    cell: (f64, f64),
    gap: (f64, f64),
    opts: &DetectOptions,
    out: &mut Vec<(f64, f64, Chord)>,
) {
    let clip = |(a, b): (f64, f64)| (a.max(gap.0), b.min(gap.1));
    let chord_of = |r: (f64, f64)| {
        let (a, b) = clip(r);
        (b - a > 1e-9).then(|| chord_in(notes, a, b, opts)).flatten()
    };
    let (a, b) = clip(cell);
    if b - a <= 1e-9 {
        return;
    }
    let whole = chord_of(cell);
    if cell.1 - cell.0 > ARP_MIN_WINDOW_BEATS + 1e-9 {
        let mid = (cell.0 + cell.1) * 0.5;
        let (left, right) = (chord_of((cell.0, mid)), chord_of((mid, cell.1)));
        if whole.is_none() || (left.is_some() && right.is_some() && left != right) {
            split_window(notes, (cell.0, mid), gap, opts, out);
            split_window(notes, (mid, cell.1), gap, opts, out);
            return;
        }
    }
    if let Some(chord) = whole {
        out.push((a, b, chord));
    }
}

/// ノート列からコードの区間の列を作る (`start_beat` 昇順・非交差、同じコードが続く窓は 1 つにまとめる)。
///
/// 窓は `opts.window_beats` 刻み (拍 0 起点、設定は [`DetectOptions::sanitized`] を通す)。窓ごとに、ノートが
/// 窓と重なる長さを pitch class の強さとして [`match_chord`] に渡す。コードが決まらない窓 (無音・単音) は
/// 区間を作らない。
///
/// 開始順の sweep: ノートを開始順に並べ、窓ごとに「窓に掛かるノート」だけを足す (O(N log N + 窓 × 掛かるノート))。
/// ノートの無い窓は飛ばす。窓の数は [`MAX_WINDOWS`] で打ち切る。
#[must_use]
pub fn detect_chords(notes: &[DetectNote], opts: &DetectOptions) -> Vec<SourceChord> {
    let opts = opts.sanitized();
    let window = opts.window_beats;
    let sorted = usable_notes(notes, &opts);
    let Some(first) = sorted.first() else { return Vec::new() };
    let window_of = |beat: f64| -> u64 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let w = (beat / window).floor().max(0.0) as u64;
        w
    };
    // (窓の番号の始め, 終わり (含まない), コード)。位置は最後に番号 × 窓で出す (足し合わせの誤差を溜めない)。
    let mut runs: Vec<(u64, u64, Chord)> = Vec::new();
    // 窓 `w` に掛かりうるノート (開始が窓の終わりより前で、まだ外していないもの)。
    let mut active: Vec<&DetectNote> = Vec::new();
    let mut next = 0usize;
    let mut w = window_of(first.start_beat);
    for _ in 0..MAX_WINDOWS {
        let Some(w_end) = w.checked_add(1) else { break };
        #[allow(clippy::cast_precision_loss)]
        let (a, b) = (w as f64 * window, w_end as f64 * window);
        while let Some(n) = sorted.get(next).filter(|n| n.start_beat < b) {
            active.push(n);
            next += 1;
        }
        active.retain(|n| n.start_beat + n.len_beats > a);
        if active.is_empty() {
            // 次のノートが始まる窓へ飛ぶ (残りが無ければ終わり)。
            let Some(n) = sorted.get(next) else { break };
            w = window_of(n.start_beat).max(w_end);
            continue;
        }
        if let Some(chord) = chord_in(&active, a, b, &opts) {
            match runs.last_mut() {
                Some(last) if last.2 == chord && last.1 == w => last.1 = w_end,
                _ => runs.push((w, w_end, chord)),
            }
        }
        w = w_end;
    }
    #[allow(clippy::cast_precision_loss)]
    runs.into_iter()
        .map(|(w0, w1, chord)| SourceChord { start_beat: w0 as f64 * window, len_beats: (w1 - w0) as f64 * window, chord })
        .collect()
}

/// 音源のフレーム `frame` を **覆う**、和音を弾いている区間 (`AudioChordSpan::chordal`) のコード (r.md #159 の
/// オーディオの自分の和音)。前の区間を持ち越さない。`spans` は `Song.audio_chords[source_id]` (`start_frame` 昇順)。
///
/// RT 安全 (二分探索、確保なし)。
#[must_use]
pub fn audio_own_chord_at(spans: &[AudioChordSpan], frame: u64) -> Option<&Chord> {
    let i = spans.partition_point(|s| s.start_frame <= frame);
    spans.get(i.checked_sub(1)?).filter(|s| s.chordal && frame < s.end_frame).map(|s| &s.chord)
}

/// 音源フレームの範囲 `[a, b)` と重なりがいちばん長い、和音を弾いている区間のコード (同じ長さなら先)。
/// コードの箱 1 つの下では基準を 1 つに決める (箱の途中で減衰・休符の窓に入っても音程を跳ねさせない)。
#[must_use]
pub fn audio_longest_own_in(spans: &[AudioChordSpan], a: u64, b: u64) -> Option<(&Chord, u64)> {
    let i = spans.partition_point(|s| s.end_frame <= a);
    let mut best: Option<(&Chord, u64)> = None;
    for s in spans[i..].iter().take_while(|s| s.start_frame < b).filter(|s| s.chordal) {
        let overlap = s.end_frame.min(b).saturating_sub(s.start_frame.max(a));
        if overlap > 0 && best.is_none_or(|(_, o)| overlap > o) {
            best = Some((&s.chord, overlap));
        }
    }
    best
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
    }

    /// r.md #159: 自分の和音は **一緒に弾いた** 音だけ。ブロック・ストローク・伸ばした分散和音 (終わりが揃う)・
    /// 3 音のパワーコードは和音、重ねずに並べた旋律・2 声・スタッカートの分散和音・重ねて鳴らし続ける旋律
    /// (始まりも終わりもずれる) は和音ではない (2 拍の窓に集めれば C になる音でも)。
    #[test]
    fn own_chords_are_only_what_is_played_together() {
        let own = |notes: &[DetectNote]| names(&own_chords(notes));
        let s = |a: f64, len: f64, name: &str| (a, len, name.to_string());
        // ブロック / ストローク。
        assert_eq!(own(&[n(0.0, 2.0, 60), n(0.0, 2.0, 64), n(0.0, 2.0, 67)]), vec![s(0.0, 2.0, "C")]);
        assert_eq!(own(&[n(0.0, 2.0, 60), n(0.05, 1.95, 64), n(0.1, 1.9, 67)]), vec![s(0.0, 2.0, "C")]);
        // 伸ばした分散和音: 1 音目から和音の区間に入る (1 音目の位置は C だけなので和音の区間は 2 音目から)。
        let held = own(&[n(0.0, 4.0, 60), n(1.0, 3.0, 64), n(2.0, 2.0, 67)]);
        assert_eq!(held, vec![s(2.0, 2.0, "C")]);
        // パワーコード (3 音 = 2 pitch class)。
        assert_eq!(own(&[n(0.0, 2.0, 40), n(0.0, 2.0, 47), n(0.0, 2.0, 52)]), vec![s(0.0, 2.0, "E5")]);
        // 旋律 (C E G を重ねずに)。窓で集めると C になる音。
        let melody = [n(0.0, 0.5, 72), n(0.5, 0.5, 76), n(1.0, 0.5, 79), n(1.5, 0.5, 76)];
        assert_eq!(names(&detect_chords(&melody, &DetectOptions::default())), vec![s(0.0, 2.0, "C")]);
        assert!(own(&melody).is_empty());
        // 2 声 (3 度で重ねた旋律)・スタッカートの分散和音。
        assert!(own(&[n(0.0, 1.0, 64), n(0.0, 1.0, 67), n(1.0, 1.0, 65), n(1.0, 1.0, 69)]).is_empty());
        assert!(own(&[n(0.0, 0.25, 48), n(0.5, 0.25, 52), n(1.0, 0.25, 55), n(1.5, 0.25, 52)]).is_empty());
        // 重ねて鳴らし続ける旋律 (各音 3 拍、1 拍おき): 重なりは 3 音になるが一緒に弾いていない。
        assert!(own(&[n(0.0, 3.0, 76), n(1.0, 3.0, 79), n(2.0, 3.0, 84), n(3.0, 3.0, 83)]).is_empty());
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

    fn names(spans: &[SourceChord]) -> Vec<(f64, f64, String)> {
        spans
            .iter()
            .map(|s| {
                let name = crate::chord_name::format_chord(&s.chord, crate::chord_name::ChordNotation::Regular, None);
                (s.start_beat, s.len_beats, name)
            })
            .collect()
    }

    /// 実プロジェクトの Chord トラックのクリップ (ルートと 3 度をオクターブで重ねた 4 音、5 度なし。長さは 4 / 8 拍が
    /// 混ざる)。pitch class は 2 つだが和音として読み、長さは鳴り始めから決まる (窓の設定なし)。
    #[test]
    fn octave_doubled_dyads_are_read_as_chords_with_their_own_lengths() {
        let stack = |start: f64, len: f64, pitches: [u8; 4]| pitches.map(|p| n(start, len, p));
        let notes: Vec<DetectNote> = [
            stack(0.0, 4.0, [31, 43, 55, 58]),
            stack(4.0, 4.0, [29, 41, 53, 57]),
            stack(8.0, 8.0, [27, 39, 51, 55]),
            stack(16.0, 4.0, [31, 43, 55, 58]),
            stack(20.0, 4.0, [34, 46, 53, 60]),
            stack(24.0, 8.0, [27, 39, 51, 55]),
        ]
        .concat();
        let auto = extract_chords(&notes, &DetectOptions::default());
        let s = |a: f64, len: f64, name: &str| (a, len, name.to_string());
        assert_eq!(
            names(&auto),
            vec![s(0.0, 4.0, "Gm"), s(4.0, 4.0, "F"), s(8.0, 8.0, "Eb"), s(16.0, 4.0, "Gm"), s(20.0, 4.0, "Bbsus2"), s(24.0, 8.0, "Eb")]
        );
        // 固定の窓でも同じコードが読める (素材の基準コード = 平行移動の追従もこの読みを使う)。
        let fixed = detect_chords(&notes, &DetectOptions { window_beats: 4.0, ..DetectOptions::default() });
        assert_eq!(names(&fixed), names(&auto));
        // 2 音だけ (2 声) はコードにしない。
        let two = [n(0.0, 4.0, 55), n(0.0, 4.0, 58)];
        assert!(extract_chords(&two, &DetectOptions::default()).is_empty());
    }

    /// 長さの自動: 小節の途中で替わる和音は鳴り始めで区切り、分散和音は窓でまとめ、同じコードの弾き直しは 1 つにする。
    #[test]
    fn automatic_lengths_follow_onsets_and_fall_back_to_windows_for_arpeggios() {
        let mut notes = Vec::new();
        // 拍 0-3: C、拍 3-4: F (4 拍目で替わる)
        notes.extend([n(0.0, 3.0, 48), n(0.0, 3.0, 64), n(0.0, 3.0, 67)]);
        notes.extend([n(3.0, 1.0, 53), n(3.0, 1.0, 57), n(3.0, 1.0, 60)]);
        // 拍 4-8: Am を 2 拍ずつ弾き直す
        for start in [4.0, 6.0] {
            notes.extend([n(start, 2.0, 57), n(start, 2.0, 60), n(start, 2.0, 64)]);
        }
        // 拍 8-12: G の分散和音 (4 分音符、重ならない)、拍 12-16: 2 拍ずつ Dm → E の分散和音 (8 分音符)
        notes.extend([n(8.0, 1.0, 55), n(9.0, 1.0, 59), n(10.0, 1.0, 62), n(11.0, 1.0, 59)]);
        notes.extend([n(12.0, 0.5, 50), n(12.5, 0.5, 53), n(13.0, 0.5, 57), n(13.5, 0.5, 53)]);
        notes.extend([n(14.0, 0.5, 52), n(14.5, 0.5, 56), n(15.0, 0.5, 59), n(15.5, 0.5, 56)]);
        let got = names(&extract_chords(&notes, &DetectOptions::default()));
        let s = |a: f64, len: f64, name: &str| (a, len, name.to_string());
        assert_eq!(
            got,
            vec![s(0.0, 3.0, "C"), s(3.0, 1.0, "F"), s(4.0, 4.0, "Am"), s(8.0, 4.0, "G"), s(12.0, 2.0, "Dm"), s(14.0, 2.0, "E")]
        );
    }

    /// 音をずらして弾いた和音 (0.05 拍ずつ、頭から 0.1 拍) は、最初の音の位置から 1 つの箱になる
    /// (3 つ目の音の位置で区切らない)。
    #[test]
    fn a_strummed_chord_starts_at_its_first_note() {
        let strum = |at: f64, keys: [u8; 3]| [n(at, 4.0, keys[0]), n(at + 0.05, 3.95, keys[1]), n(at + 0.1, 3.9, keys[2])];
        let mut notes = Vec::new();
        notes.extend(strum(0.0, [48, 64, 67]));
        notes.extend(strum(4.0, [53, 57, 60]));
        let got = names(&extract_chords(&notes, &DetectOptions::default()));
        assert_eq!(got, vec![(0.0, 4.0, "C".to_string()), (4.0, 4.0, "F".to_string())]);
    }
}
