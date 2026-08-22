// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! リアルタイム スペクトラムアナライザ (r.md #50)。
//!
//! - 窓 (Hann / Blackman-Harris) → 実数 FFT → `mag = 2|y| / S1` 正規化。
//!   この正規化により **ビン中心にある 0 dBFS 正弦波がちょうど 0 dB** を指す
//!   (Voxengo の "Align 0 dB" 相当)。
//! - L / R を別々に解析し、**パワーの平均**で 1 本にまとめる。位相に依存しない
//!   ので逆相成分が消えない (L+R を先に足す方式の弱点)。
//! - ビン → 表示バンドの集約は**パワーの最大**。正弦のピークを取りこぼさない。
//! - 平均・集約はすべてパワー領域で行い、dB 化は最終段だけ。
//!
//! バンド数は固定 ([`SPECTRUM_BANDS`])。widget 側は「バンド → ピクセル列」を
//! さらに max で畳むだけでよい (max は結合的なので二段に分けても結果は同じ)。

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use super::settings::{MeterSettings, SpectrumWindow};

/// 表示バンド数。パネル最大幅 (640px) より十分多く取る。
pub const SPECTRUM_BANDS: usize = 768;
/// 表示する周波数範囲。
pub const F_MIN: f32 = 20.0;
pub const F_MAX: f32 = 20_000.0;

/// ピーク保持線の保持時間 [秒] と、その後の落下速度 [dB/s]。
const PEAK_HOLD_SECS: f32 = 2.0;
const PEAK_FALL_DB_PER_S: f32 = 13.3;

/// バンド `b` の中心周波数 [Hz] (対数等間隔)。
pub fn band_center_hz(b: usize) -> f32 {
    let t = b as f32 / (SPECTRUM_BANDS - 1) as f32;
    F_MIN * (F_MAX / F_MIN).powf(t)
}

/// 1 バンドが参照するビン範囲 (`lo..hi`、空なら最近傍 1 本)。
#[derive(Debug, Clone, Copy)]
struct BandBins {
    lo: usize,
    hi: usize,
}

pub struct SpectrumAnalyzer {
    sample_rate: u32,
    fft_len: usize,
    hop: usize,
    window_kind: SpectrumWindow,
    window: Vec<f32>,
    /// `S1 = Σ w[j]`。正規化の分母。
    s1: f32,
    fft: Arc<dyn RealToComplex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// 入力リング (チャンネルごと、長さ = `fft_len`)。
    ring: [Vec<f32>; 2],
    write: usize,
    since_hop: usize,
    /// FFT 作業用。
    fft_in: Vec<f32>,
    fft_out: Vec<Complex<f32>>,
    /// ビン単位のパワー (L/R 平均)。
    bin_power: Vec<f32>,
    /// バンド → ビン範囲の写像 (fft_len / sample_rate が変わったら再構築)。
    band_bins: Vec<BandBins>,
    /// 傾き補正量 [dB] (バンドごとに定数)。
    band_slope_db: Vec<f32>,
    slope_db_oct: f32,
    /// 表示値 / ピーク保持値 [dB]。
    display_db: Vec<f32>,
    hold_db: Vec<f32>,
    hold_age: Vec<f32>,
    floor_db: f32,
    release_ms: u32,
}

impl SpectrumAnalyzer {
    pub fn new(sample_rate: u32, settings: &MeterSettings) -> Self {
        let mut me = Self {
            sample_rate: sample_rate.max(1),
            fft_len: 0,
            hop: 0,
            window_kind: settings.spectrum_window,
            window: Vec::new(),
            s1: 1.0,
            fft: RealFftPlanner::<f32>::new().plan_fft_forward(2),
            scratch: Vec::new(),
            ring: [Vec::new(), Vec::new()],
            write: 0,
            since_hop: 0,
            fft_in: Vec::new(),
            fft_out: Vec::new(),
            bin_power: Vec::new(),
            band_bins: Vec::new(),
            band_slope_db: vec![0.0; SPECTRUM_BANDS],
            slope_db_oct: f32::NAN,
            display_db: vec![f32::NEG_INFINITY; SPECTRUM_BANDS],
            hold_db: vec![f32::NEG_INFINITY; SPECTRUM_BANDS],
            hold_age: vec![0.0; SPECTRUM_BANDS],
            floor_db: -settings.spectrum_range_db,
            release_ms: settings.spectrum_release_ms,
        };
        me.rebuild(settings.spectrum_fft.size(), settings.spectrum_window);
        me.rebuild_slope(settings.spectrum_slope_db_oct);
        me
    }

    /// 設定変更 / サンプルレート変更に追従する。必要なときだけ再構築する。
    pub fn apply(&mut self, sample_rate: u32, settings: &MeterSettings) {
        let sr = sample_rate.max(1);
        let n = settings.spectrum_fft.size();
        if sr != self.sample_rate {
            self.sample_rate = sr;
            self.rebuild(n, settings.spectrum_window);
        } else if n != self.fft_len || settings.spectrum_window != self.window_kind {
            self.rebuild(n, settings.spectrum_window);
        }
        self.rebuild_slope(settings.spectrum_slope_db_oct);
        self.floor_db = -settings.spectrum_range_db;
        self.release_ms = settings.spectrum_release_ms.max(1);
    }

    fn rebuild(&mut self, fft_len: usize, window_kind: SpectrumWindow) {
        self.fft_len = fft_len;
        self.hop = fft_len / 4; // 75% オーバーラップ
        self.window_kind = window_kind;
        self.window = (0..fft_len)
            .map(|j| window_kind.coefficient(j, fft_len))
            .collect();
        self.s1 = self.window.iter().sum::<f32>().max(1e-9);
        self.fft = RealFftPlanner::<f32>::new().plan_fft_forward(fft_len);
        self.scratch = self.fft.make_scratch_vec();
        self.ring = [vec![0.0; fft_len], vec![0.0; fft_len]];
        self.write = 0;
        self.since_hop = 0;
        self.fft_in = self.fft.make_input_vec();
        self.fft_out = self.fft.make_output_vec();
        self.bin_power = vec![0.0; fft_len / 2 + 1];
        self.rebuild_bands();
    }

    fn rebuild_bands(&mut self) {
        let n = self.fft_len as f32;
        let fs = self.sample_rate as f32;
        let bins = self.fft_len / 2;
        let ratio = (F_MAX / F_MIN).powf(1.0 / (SPECTRUM_BANDS - 1) as f32);
        let half = ratio.sqrt();
        self.band_bins = (0..SPECTRUM_BANDS)
            .map(|b| {
                let c = band_center_hz(b);
                let f_lo = c / half;
                let f_hi = c * half;
                let lo = (f_lo * n / fs).ceil().max(0.0) as usize;
                let hi = (f_hi * n / fs).floor().max(0.0) as usize;
                let lo = lo.min(bins);
                let hi = hi.min(bins);
                if lo > hi {
                    // バンドがビン間隔より細い: 最近傍ビン 1 本を見る。
                    let k = ((c * n / fs).round().max(0.0) as usize).min(bins);
                    BandBins { lo: k, hi: k }
                } else {
                    BandBins { lo, hi }
                }
            })
            .collect();
    }

    fn rebuild_slope(&mut self, slope_db_oct: f32) {
        if (slope_db_oct - self.slope_db_oct).abs() < 1e-6 {
            return;
        }
        self.slope_db_oct = slope_db_oct;
        for (b, v) in self.band_slope_db.iter_mut().enumerate() {
            *v = slope_db_oct * (band_center_hz(b) / 1000.0).log2();
        }
    }

    /// フレームを流し込む。**弾道は FFT が走るたびに、その FFT までの経過時間で
    /// 進める**。1 呼び出しにまとめて進めると、省電力からの復帰などで長い塊が
    /// 来たとき「塊の中で一番大きかった値」が減衰なしで残ってしまう。
    pub fn process(&mut self, frames: &[[f32; 2]]) {
        let mut latest = vec![f32::NEG_INFINITY; SPECTRUM_BANDS];
        let mut since_advance = 0usize;
        for fr in frames {
            self.ring[0][self.write] = fr[0];
            self.ring[1][self.write] = fr[1];
            self.write = (self.write + 1) % self.fft_len;
            self.since_hop += 1;
            since_advance += 1;
            if self.since_hop >= self.hop {
                self.since_hop = 0;
                self.run_fft();
                latest.fill(f32::NEG_INFINITY);
                self.fold_bands(&mut latest);
                let dt = since_advance as f32 / self.sample_rate as f32;
                since_advance = 0;
                self.advance(dt, Some(&latest));
            }
        }
        if since_advance > 0 {
            let dt = since_advance as f32 / self.sample_rate as f32;
            self.advance(dt, None);
        }
    }

    /// リングの中身を窓掛けして FFT し、`bin_power` を L/R のパワー平均で埋める。
    fn run_fft(&mut self) {
        let n = self.fft_len;
        let s1 = self.s1;
        self.bin_power.fill(0.0);
        for ch in 0..2 {
            for j in 0..n {
                // `write` は「最も古いサンプル」の位置 (次に上書きする所)。
                self.fft_in[j] = self.ring[ch][(self.write + j) % n] * self.window[j];
            }
            // realfft は入力を破壊しうるが、毎回書き直すので問題ない。
            if self
                .fft
                .process_with_scratch(&mut self.fft_in, &mut self.fft_out, &mut self.scratch)
                .is_err()
            {
                return;
            }
            let bins = self.fft_out.len();
            for (k, c) in self.fft_out.iter().enumerate() {
                // 片側スペクトルなので DC と Nyquist 以外は 2 倍する。
                let scale = if k == 0 || k + 1 == bins { 1.0 } else { 2.0 };
                let mag = scale * c.norm() / s1;
                self.bin_power[k] += mag * mag * 0.5; // L/R のパワー平均
            }
        }
    }

    /// ビンのパワーをバンドへ max で畳み、傾き補正込みの dB にする。
    fn fold_bands(&self, out: &mut [f32]) {
        for (b, bins) in self.band_bins.iter().enumerate() {
            let mut p = 0.0_f32;
            for k in bins.lo..=bins.hi {
                let v = self.bin_power[k];
                if v > p {
                    p = v;
                }
            }
            let db = if p > 0.0 {
                10.0 * p.log10() + self.band_slope_db[b]
            } else {
                f32::NEG_INFINITY
            };
            if db > out[b] {
                out[b] = db;
            }
        }
    }

    /// アタック瞬時 / リリースのみ時定数、という FabFilter・Voxengo と同じ弾道。
    fn advance(&mut self, dt: f32, latest: Option<&[f32]>) {
        let fall = 20.0 * dt / (self.release_ms as f32 / 1000.0);
        for b in 0..SPECTRUM_BANDS {
            let new = latest.map_or(f32::NEG_INFINITY, |l| l[b]);
            let released = self.display_db[b] - fall;
            let v = if new > released { new } else { released };
            self.display_db[b] = v.max(self.floor_db);

            if v >= self.hold_db[b] {
                self.hold_db[b] = v;
                self.hold_age[b] = 0.0;
            } else {
                self.hold_age[b] += dt;
                if self.hold_age[b] > PEAK_HOLD_SECS {
                    self.hold_db[b] = (self.hold_db[b] - PEAK_FALL_DB_PER_S * dt).max(v);
                }
            }
        }
    }

    pub fn display_db(&self) -> &[f32] {
        &self.display_db
    }

    pub fn hold_db(&self) -> &[f32] {
        &self.hold_db
    }

    pub fn reset(&mut self) {
        self.display_db.fill(self.floor_db);
        self.hold_db.fill(self.floor_db);
        self.hold_age.fill(0.0);
        for r in &mut self.ring {
            r.fill(0.0);
        }
        self.write = 0;
        self.since_hop = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master_meter::settings::SpectrumFft;

    fn sine_n(fs: u32, freq: f32, n: usize, amp: f32) -> Vec<[f32; 2]> {
        (0..n)
            .map(|i| {
                let v = amp
                    * (std::f32::consts::TAU * freq * i as f32 / fs as f32).sin();
                [v, v]
            })
            .collect()
    }

    fn sine(fs: u32, freq: f32, secs: f32, amp: f32) -> Vec<[f32; 2]> {
        sine_n(fs, freq, (fs as f32 * secs) as usize, amp)
    }

    /// 表示値はホップ間で減衰するので、校正を測るときはホップ境界ちょうどで
    /// 止める (= FFT 直後の値を読む)。100 ホップ流せば起動過渡も抜ける。
    fn feed_whole_hops(a: &mut SpectrumAnalyzer, fs: u32, freq: f32, amp: f32, fft_len: usize) {
        let n = fft_len / 4 * 100;
        a.process(&sine_n(fs, freq, n, amp));
    }

    /// ビン中心にちょうど乗る周波数 (スキャロップ損を避ける)。
    fn bin_centered(fs: u32, fft_len: usize, near: f32) -> f32 {
        let bin = fs as f32 / fft_len as f32;
        bin * (near / bin).round()
    }

    fn nearest_band(freq: f32) -> usize {
        (0..SPECTRUM_BANDS)
            .min_by(|&a, &b| {
                (band_center_hz(a) - freq)
                    .abs()
                    .total_cmp(&(band_center_hz(b) - freq).abs())
            })
            .unwrap()
    }

    fn settings_no_slope() -> MeterSettings {
        MeterSettings { spectrum_slope_db_oct: 0.0, ..MeterSettings::default() }
    }

    /// 0 dBFS の正弦は、その周波数のバンドで 0 dB を指す (Align 0 dB)。
    #[test]
    fn full_scale_sine_reads_zero_db_at_its_band() {
        let fs = 48_000;
        let s = settings_no_slope();
        let mut a = SpectrumAnalyzer::new(fs, &s);
        let n = s.spectrum_fft.size();
        let f = bin_centered(fs, n, 1000.0);
        feed_whole_hops(&mut a, fs, f, 1.0, n);
        let b = nearest_band(f);
        let db = a.display_db()[b];
        assert!((db - 0.0).abs() < 0.15, "band {b} ({} Hz) = {db} dB", band_center_hz(b));
    }

    /// -20 dBFS の正弦は -20 dB 付近になる (線形性)。
    #[test]
    fn amplitude_scales_the_reading_one_for_one() {
        let fs = 48_000;
        let s = settings_no_slope();
        let mut a = SpectrumAnalyzer::new(fs, &s);
        let n = s.spectrum_fft.size();
        let f = bin_centered(fs, n, 1000.0);
        feed_whole_hops(&mut a, fs, f, 0.1, n);
        let db = a.display_db()[nearest_band(f)];
        assert!((db - (-20.0)).abs() < 0.15, "got {db}");
    }

    /// 傾き 4.5 dB/oct は 1kHz 支点。2kHz は +4.5 dB、500Hz は -4.5 dB 寄る。
    #[test]
    fn tilt_is_anchored_at_1khz() {
        let fs = 48_000;
        let s = MeterSettings { spectrum_slope_db_oct: 4.5, ..MeterSettings::default() };
        let mut a = SpectrumAnalyzer::new(fs, &s);
        let n = s.spectrum_fft.size();
        let f = bin_centered(fs, n, 2000.0);
        feed_whole_hops(&mut a, fs, f, 1.0, n);
        let db = a.display_db()[nearest_band(f)];
        let expected = 4.5 * (f / 1000.0).log2();
        assert!((db - expected).abs() < 0.2, "got {db}, expected ~{expected}");
    }

    /// 無音を流し続けると表示は下限へ落ちる。
    #[test]
    fn silence_falls_to_the_floor() {
        let fs = 48_000;
        let s = settings_no_slope();
        let mut a = SpectrumAnalyzer::new(fs, &s);
        a.process(&sine(fs, 1000.0, 0.5, 1.0));
        a.process(&vec![[0.0, 0.0]; fs as usize * 5]);
        let max = a.display_db().iter().cloned().fold(f32::MIN, f32::max);
        assert!((max - (-s.spectrum_range_db)).abs() < 0.01, "got {max}");
    }

    /// FFT 長を変えても校正はずれない。
    #[test]
    fn calibration_holds_across_fft_sizes() {
        for fft in SpectrumFft::ALL {
            let fs = 48_000;
            let s = MeterSettings {
                spectrum_fft: fft,
                spectrum_slope_db_oct: 0.0,
                ..MeterSettings::default()
            };
            let mut a = SpectrumAnalyzer::new(fs, &s);
            let f = bin_centered(fs, fft.size(), 1000.0);
            feed_whole_hops(&mut a, fs, f, 1.0, fft.size());
            let db = a.display_db()[nearest_band(f)];
            assert!((db - 0.0).abs() < 0.2, "{}: got {db}", fft.label());
        }
    }

    /// バンド中心は 20Hz から 20kHz を対数等間隔で覆う。
    #[test]
    fn band_centers_span_the_audible_range_logarithmically() {
        assert!((band_center_hz(0) - F_MIN).abs() < 1e-3);
        assert!((band_center_hz(SPECTRUM_BANDS - 1) - F_MAX).abs() < 1.0);
        let a = band_center_hz(100) / band_center_hz(0);
        let b = band_center_hz(200) / band_center_hz(100);
        assert!((a - b).abs() / a < 1e-4, "log spacing broken: {a} vs {b}");
    }
}
