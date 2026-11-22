//! オーディオ音源からのコード検出 (r.md #157 §3.4、`docs/plan_chord_track.md`)。
//!
//! 音源 → chroma (pitch class ごとの強さ) → `common::chord_detect::match_chord` (MIDI の検出と同じ判定)。
//! 重いので off-thread で呼ぶ (`handler::chord_audio`)。結果は音源のフレームで返す
//! (`Song.audio_chords[source_id]` の形。クリップの trim / 分割 / テンポ変更で狂わない)。
//!
//! chroma の作り方:
//! - 窓 (Hann) → 実数 FFT (`realfft`、`master_meter/spectrum.rs` と同じ) → 振幅スペクトルの **山だけ** を拾い
//!   (山の間の漏れを数えない)、放物線補間した周波数を MIDI の音高に直して pitch class へ足す。
//!   音高の中心から 0.3 半音より外れた山は捨てる (打楽器・雑音)。
//! - **倍音の差し引き**: 低い方の山から順に、その山の整数倍 (2〜6 倍) に当たる山の強さを減らす。
//!   C の第 3 倍音は G、E の第 3 倍音は B — 差し引かないと C が Cmaj7 に、Am が Am9 に化ける。
//! - ベース: 40〜260 Hz で最も強い山の pitch class を、窓の中で強さを足し合わせて最低音とする
//!   (`match_chord` の `lowest_pc`、`C/E` の判定と C6 / Am7 の選び分けに使う)。
//!
//! 窓の区切りは拍の格子 ([`DetectGrid`]): 呼び出し側がクリップの置き方から「曲の拍 0 が音源のどのフレームか」と
//! 「1 窓が何フレームか」を渡す。窓の中で音が小さすぎる (−50 dBFS 未満) ところはコードを作らない。

use std::sync::Arc;

use common::chord_detect::{DetectOptions, match_chord};
use common::model::{AudioChordSpan, Chord};
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// 窓の格子 (音源のフレーム)。窓 `k` は `[origin + k * window, origin + (k + 1) * window)`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectGrid {
    pub origin_frame: f64,
    pub window_frames: f64,
}

/// 検出に使う周波数の範囲 (Hz)。下は E1 付近、上は 5 オクターブ上まで (それより上は倍音ばかり)。
const F_LO: f32 = 38.0;
const F_HI: f32 = 2_100.0;
/// ベース (最低音) を探す範囲 (Hz)。
const BASS_HI: f32 = 260.0;
/// これより小さい窓はコードを作らない (RMS、−50 dBFS)。
const SILENCE_RMS: f32 = 0.003_16;
/// 窓の chroma で、最も強い pitch class に対してこれ未満の pitch class は落とす。
const RELATIVE_FLOOR: f64 = 0.2;

/// 1 FFT フレームの解析結果。
#[derive(Debug, Clone, Copy, Default)]
struct Frame {
    chroma: [f64; 12],
    /// ベースの pitch class とその強さ。
    bass: Option<(u8, f64)>,
    /// フレームの RMS (無音の判定)。
    rms: f32,
}

/// 平面 (チャンネルごと) の音声 `samples` (サンプルレート `sample_rate`) から、`grid` の窓ごとにコードを検出する。
/// 同じコードが続く窓は 1 つの区間にまとめる。返す区間は `start_frame` 昇順・非交差。
#[must_use]
pub fn detect_audio_chords(
    samples: &[Vec<f32>],
    sample_rate: u32,
    grid: DetectGrid,
    opts: &DetectOptions,
) -> Vec<AudioChordSpan> {
    let frames = samples.iter().map(Vec::len).max().unwrap_or(0);
    if frames == 0 || sample_rate == 0 || !(grid.window_frames.is_finite() && grid.window_frames >= 1.0) {
        return Vec::new();
    }
    let fft_len = fft_len_for(sample_rate);
    let hop = fft_len / 4;
    let analysed = analyse(samples, frames, sample_rate, fft_len, hop);
    // 窓の範囲 (k の始めと終わり)。
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let (k0, k1) = (
        ((0.0 - grid.origin_frame) / grid.window_frames).floor() as i64,
        ((frames as f64 - grid.origin_frame) / grid.window_frames).ceil() as i64,
    );
    let mut out: Vec<AudioChordSpan> = Vec::new();
    for k in k0..k1 {
        #[allow(clippy::cast_precision_loss)]
        let a = (grid.origin_frame + k as f64 * grid.window_frames).max(0.0);
        #[allow(clippy::cast_precision_loss)]
        let b = (grid.origin_frame + (k + 1) as f64 * grid.window_frames).min(frames as f64);
        if b - a < 1.0 {
            continue;
        }
        let Some(chord) = chord_in(&analysed, hop, fft_len, a, b, opts) else { continue };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (sa, sb) = (a.round() as u64, b.round() as u64);
        match out.last_mut() {
            Some(last) if last.chord == chord && last.end_frame == sa => last.end_frame = sb,
            _ => out.push(AudioChordSpan { start_frame: sa, end_frame: sb, chord }),
        }
    }
    out
}

/// 約 0.17 秒 (44.1 / 48 kHz で 8192) の 2 の冪。A1 (55 Hz) 付近の半音 (約 3 Hz) を分ける分解能。
fn fft_len_for(sample_rate: u32) -> usize {
    let want = (f64::from(sample_rate) * 0.17) as usize;
    want.next_power_of_two().clamp(2048, 32_768)
}

/// 窓 `[a, b)` (フレーム) に中心が入る FFT フレームを足し合わせてコードを決める。
fn chord_in(analysed: &[Frame], hop: usize, fft_len: usize, a: f64, b: f64, opts: &DetectOptions) -> Option<Chord> {
    let mut chroma = [0.0f64; 12];
    let mut bass = [0.0f64; 12];
    let mut rms_sum = 0.0f64;
    let mut n = 0u32;
    for (i, f) in analysed.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let center = (i * hop + fft_len / 2) as f64;
        if center < a || center >= b {
            continue;
        }
        for (c, v) in chroma.iter_mut().zip(f.chroma) {
            *c += v;
        }
        if let Some((pc, w)) = f.bass {
            bass[usize::from(pc)] += w;
        }
        rms_sum += f64::from(f.rms);
        n += 1;
    }
    if n == 0 || rms_sum / f64::from(n) < f64::from(SILENCE_RMS) {
        return None;
    }
    // 最も強い pitch class の [`RELATIVE_FLOOR`] 未満は鳴っていないとみなす (差し引き切れない倍音・残響・
    // 隣の窓からの漏れ)。MIDI と違い音の強さに幅があるので、`match_chord` の「全体の 5%」より強く切る。
    let max = chroma.iter().copied().fold(0.0f64, f64::max);
    for c in &mut chroma {
        if *c < max * RELATIVE_FLOOR {
            *c = 0.0;
        }
    }
    let lowest = (0..12u8)
        .filter(|&pc| bass[usize::from(pc)] > 0.0)
        .max_by(|&x, &y| bass[usize::from(x)].total_cmp(&bass[usize::from(y)]));
    match_chord(&chroma, lowest, opts)
}

/// 全体を `hop` 刻みで解析する。
fn analyse(samples: &[Vec<f32>], frames: usize, sample_rate: u32, fft_len: usize, hop: usize) -> Vec<Frame> {
    let mut planner = RealFftPlanner::<f32>::new();
    let fft: Arc<dyn realfft::RealToComplex<f32>> = planner.plan_fft_forward(fft_len);
    let window: Vec<f32> = (0..fft_len)
        .map(|j| {
            #[allow(clippy::cast_precision_loss)]
            let t = j as f32 / fft_len as f32;
            0.5 - 0.5 * (std::f32::consts::TAU * t).cos()
        })
        .collect();
    let mut input = vec![0.0f32; fft_len];
    let mut spectrum = vec![Complex::new(0.0f32, 0.0); fft_len / 2 + 1];
    let mut scratch = fft.make_scratch_vec();
    let mut mags = vec![0.0f32; fft_len / 2 + 1];
    #[allow(clippy::cast_precision_loss)]
    let inv_ch = 1.0 / samples.len().max(1) as f32;
    #[allow(clippy::cast_precision_loss)]
    let bin_hz = sample_rate as f32 / fft_len as f32;
    let mut out = Vec::with_capacity(frames / hop + 1);
    let mut start = 0usize;
    while start < frames {
        let mut sq = 0.0f32;
        for (j, x) in input.iter_mut().enumerate() {
            let s: f32 = samples.iter().map(|ch| ch.get(start + j).copied().unwrap_or(0.0)).sum::<f32>() * inv_ch;
            sq += s * s;
            *x = s * window[j];
        }
        #[allow(clippy::cast_precision_loss)]
        let rms = (sq / fft_len as f32).sqrt();
        let mut frame = Frame { rms, ..Frame::default() };
        if rms > 1e-5 && fft.process_with_scratch(&mut input, &mut spectrum, &mut scratch).is_ok() {
            for (m, c) in mags.iter_mut().zip(&spectrum) {
                *m = c.norm();
            }
            fill_chroma(&mags, bin_hz, &mut frame);
        }
        out.push(frame);
        start += hop;
    }
    out
}

/// 振幅スペクトル `mags` の山から chroma とベースを作る (倍音を差し引いてから)。
fn fill_chroma(mags: &[f32], bin_hz: f32, frame: &mut Frame) {
    let peak_max = mags.iter().copied().fold(0.0f32, f32::max);
    if peak_max <= 0.0 {
        return;
    }
    // 山 (周波数, 強さ) を低い順に。最大の山の −40 dB 未満は雑音として捨てる。
    let floor = peak_max * 0.01;
    let mut peaks: Vec<(f32, f32)> = Vec::new();
    for k in 1..mags.len() - 1 {
        let (l, c, r) = (mags[k - 1], mags[k], mags[k + 1]);
        if c < floor || c <= l || c < r {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let f = (k as f32 + parabolic_offset(l, c, r)) * bin_hz;
        if (F_LO..=F_HI * 6.0).contains(&f) {
            peaks.push((f, c));
        }
    }
    // 倍音の差し引き: 低い山から、その 2〜8 倍に当たる山を「基音の強さ / 倍数」(弦・管・鋸歯波の
    // おおよその倍音の減り方) だけ減らす。差し引いた後の山が次の山の基音として働く (低い順なので、
    // 和音の上の音の基音が下の音の倍音と重なっても、残りが上の音として残る)。
    for i in 0..peaks.len() {
        let (f0, a0) = peaks[i];
        if a0 <= 0.0 {
            continue;
        }
        for h in 2..=8u8 {
            let target = f0 * f32::from(h);
            let tol = target * 0.03;
            if let Some(j) = peaks.iter().position(|(f, _)| (f - target).abs() <= tol) {
                peaks[j].1 = (peaks[j].1 - a0 / f32::from(h)).max(0.0);
            }
        }
    }
    let bass_max = peaks.iter().filter(|(f, _)| *f <= BASS_HI).map(|(_, a)| *a).fold(0.0f32, f32::max);
    let mut bass_lowest: Option<(u8, f32)> = None;
    for &(f, a) in &peaks {
        if a <= 0.0 || f > F_HI {
            continue;
        }
        let pitch = 69.0 + 12.0 * (f / 440.0).log2();
        let nearest = pitch.round();
        if (pitch - nearest).abs() > 0.3 {
            continue;
        }
        #[allow(clippy::cast_possible_truncation)]
        let pc = (nearest as i32).rem_euclid(12) as u8;
        frame.chroma[usize::from(pc)] += f64::from(a);
        // ベース = ベースの範囲で、その範囲の最も強い山の 30% 以上ある一番低い山 (peaks は低い順)。
        if f <= BASS_HI && bass_lowest.is_none() && a >= bass_max * 0.3 {
            bass_lowest = Some((pc, a));
        }
    }
    frame.bass = bass_lowest.map(|(pc, a)| (pc, f64::from(a)));
}

/// 放物線補間した山の位置 (ビンからのずれ、−0.5..=0.5)。
fn parabolic_offset(l: f32, c: f32, r: f32) -> f32 {
    let d = l - 2.0 * c + r;
    if d.abs() < 1e-12 { 0.0 } else { (0.5 * (l - r) / d).clamp(-0.5, 0.5) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::ChordKind;

    /// 倍音を持つ (鋸歯状の 8 倍音まで) 合成音で、C → Am → F/A → G7 が取れる。
    #[test]
    fn synthetic_tones_with_harmonics_are_named() {
        let sr = 48_000u32;
        let sec = |s: f64| (s * f64::from(sr)) as usize;
        let midi_hz = |p: u8| 440.0 * 2f64.powf((f64::from(p) - 69.0) / 12.0);
        let prog: [&[u8]; 4] = [&[36, 60, 64, 67], &[45, 60, 64, 69], &[45, 60, 65, 69], &[43, 59, 62, 65, 67]];
        let mut mono = vec![0.0f32; sec(8.0)];
        for (i, chord) in prog.iter().enumerate() {
            for &p in *chord {
                let f = midi_hz(p);
                for (n, s) in mono[sec(i as f64 * 2.0)..sec((i + 1) as f64 * 2.0)].iter_mut().enumerate() {
                    let t = n as f64 / f64::from(sr);
                    let v: f64 = (1..=8).map(|h| (std::f64::consts::TAU * f * f64::from(h) * t).sin() / f64::from(h)).sum();
                    *s += (v * 0.05) as f32;
                }
            }
        }
        let grid = DetectGrid { origin_frame: 0.0, window_frames: f64::from(sr) * 2.0 };
        let spans = detect_audio_chords(&[mono], sr, grid, &DetectOptions::default());
        let got: Vec<(u8, ChordKind, Option<u8>)> = spans.iter().map(|s| (s.chord.root, s.chord.kind, s.chord.bass)).collect();
        assert_eq!(
            got,
            vec![(0, ChordKind::Major, None), (9, ChordKind::Minor, None), (5, ChordKind::Major, Some(9)), (7, ChordKind::Dominant7, None)]
        );
        assert_eq!(spans[0].start_frame, 0);
        assert_eq!(spans[3].end_frame, 8 * u64::from(sr));
    }

    /// 無音はコードにしない。
    #[test]
    fn silence_has_no_chords() {
        let grid = DetectGrid { origin_frame: 0.0, window_frames: 48_000.0 };
        assert!(detect_audio_chords(&[vec![0.0; 96_000]], 48_000, grid, &DetectOptions::default()).is_empty());
    }
}
