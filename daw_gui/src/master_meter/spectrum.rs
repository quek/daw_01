//! リアルタイム スペクトラムアナライザ (r.md #50)。
//!
//! - 窓 (Hann / Blackman-Harris) → 実数 FFT → `mag = 2|y| / S1` 正規化。
//!   この正規化により **ビン中心にある 0 dBFS 正弦波がちょうど 0 dB** を指す
//!   (Voxengo の "Align 0 dB" 相当)。
//! - L / R を別々に解析し、**パワーの平均**で 1 本にまとめる。位相に依存しない
//!   ので逆相成分が消えない (L+R を先に足す方式の弱点)。
//! - ビン → 表示バンドの集約は**パワーの最大**。正弦のピークを取りこぼさない。
//! - 低域ではバンドがビン間隔より細く、最近傍ビンを読むと何本ものバンドが同じ値を共有して
//!   階段になる (傾き補正が乗ると「右上がり + 垂直落ち」の鋸歯)。そこで**ビン 2 本分より細い
//!   バンドは、バンド中心でビンの間を補間した値も見る** (dB 領域の単調 3 次 Hermite = PCHIP。
//!   行き過ぎないので、ビンに無い山や谷を作らない)。バンド内のビンの最大とも突き合わせるので、
//!   ビンちょうどの値 (校正) は変わらない。DC ビンは補間の節点にしない (DC オフセットを最低域の
//!   バンドへ混ぜない)。最初のビンより下は最初のビンの値で平らに伸ばす。
//! - L/R の平均とバンド内のビンの集約はパワー領域で行い、補間と傾き補正は dB 領域。
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
/// 表示値の解像度 [dB]。これより細かい差は画面上で見分けられないので、再描画のダイジェスト
/// (r.md #49: マスターメーター / device のスペクトラム) はこの刻みで量子化する。リリースの最長
/// (1500ms) でも 1 tick (33ms) の落下 (約 0.44 dB) がこの刻みを越えるので、減衰の途中で「表示が
/// 変わらない」と誤判定して解析を休むことはない。
pub const DISPLAY_STEP_DB: f32 = 0.25;

/// ピーク保持線の保持時間 [秒] と、その後の落下速度 [dB/s]。
const PEAK_HOLD_SECS: f32 = 2.0;
const PEAK_FALL_DB_PER_S: f32 = 13.3;

/// バンド `b` の中心周波数 [Hz] (対数等間隔)。
pub fn band_center_hz(b: usize) -> f32 {
    let t = b as f32 / (SPECTRUM_BANDS - 1) as f32;
    F_MIN * (F_MAX / F_MIN).powf(t)
}

/// 補間の節点に使うパワーの下限。パワー 0 のビンを `-inf` dB にすると補間が成り立たない。
/// その隣の区間の補間値はこの床 (-300 dB) へ引っ張られるが、f32 の FFT で厳密な 0 が出るのは
/// 入力全体が無音のときだけで、そのときは表示の下限に張り付く。
const BIN_POWER_FLOOR: f32 = 1e-30;

/// 等間隔の PCHIP の端点の傾き (Moler の 3 点式 + 形を保つ補正。scipy `PchipInterpolator` と同じ)。
/// `d_edge` は端の区間の差分、`d_inner` はその内側の区間の差分。
fn pchip_end_slope(d_edge: f32, d_inner: f32) -> f32 {
    let m = (3.0 * d_edge - d_inner) / 2.0;
    if m * d_edge <= 0.0 {
        0.0
    } else if d_edge * d_inner <= 0.0 && m.abs() > 3.0 * d_edge.abs() {
        3.0 * d_edge
    } else {
        m
    }
}

/// 等間隔の PCHIP の内側の点の傾き。両側の差分の符号が違えば (山・谷) 0、同じなら調和平均
/// (Fritsch–Butland)。
fn pchip_inner_slope(d_left: f32, d_right: f32) -> f32 {
    if d_left * d_right <= 0.0 { 0.0 } else { 2.0 * d_left * d_right / (d_left + d_right) }
}

/// 1 バンドが参照するビン。
#[derive(Debug, Clone, Copy)]
struct BandBins {
    /// 周波数がバンド内にあるビン (`lo..=hi`)。`lo > hi` なら 1 本も無い。
    lo: usize,
    hi: usize,
    /// バンドがビン 2 本分より細いときだけ、バンド中心の小数ビン位置。ここで補間した値も見る。
    /// 2 本分以上あると中心の両隣のビンがどちらもバンド内にあり、行き過ぎない補間は
    /// その最大を越えないので要らない。
    interp_at: Option<f32>,
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
    /// 補間の節点 (ビン 1..=`node_hi`) の dB と PCHIP の傾き。FFT ごとに 1 回だけ埋める
    /// (補間する数百本のバンドが同じ数本のビンを読むので、バンドごとに log10 しない)。
    /// 添字はビン番号のまま (0 = DC は使わない)。`node_hi == 0` なら補間するバンドが無い。
    node_db: Vec<f32>,
    node_slope: Vec<f32>,
    node_hi: usize,
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
            node_db: Vec::new(),
            node_slope: Vec::new(),
            node_hi: 0,
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
                let lo = ((f_lo * n / fs).ceil().max(0.0) as usize).min(bins);
                let hi = ((f_hi * n / fs).floor().max(0.0) as usize).min(bins);
                let width_bins = (f_hi - f_lo) * n / fs;
                let interp_at = (width_bins < 2.0).then(|| (c * n / fs).clamp(0.0, bins as f32));
                BandBins { lo, hi, interp_at }
            })
            .collect();
        // 補間で読む区間 k..=k+1 の右端の最大。節点は最低 2 本 (ビン 1, 2) 要る。
        self.node_hi = self
            .band_bins
            .iter()
            .filter_map(|b| b.interp_at)
            .map(|pos| (pos.floor() as usize + 1).max(2).min(bins))
            .max()
            .unwrap_or(0);
        // 傾きは両隣の dB を読むので、dB は 1 本先まで要る。
        self.node_db = vec![0.0; (self.node_hi + 2).min(bins + 1)];
        self.node_slope = vec![0.0; self.node_hi + 1];
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
                self.fill_nodes();
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

    /// ビンのパワーをバンドへ max で畳み (細いバンドはビン間の補間値とも max)、
    /// 傾き補正込みの dB にする。
    fn fold_bands(&self, out: &mut [f32]) {
        for (b, bins) in self.band_bins.iter().enumerate() {
            let mut p = 0.0_f32;
            for k in bins.lo..=bins.hi {
                let v = self.bin_power[k];
                if v > p {
                    p = v;
                }
            }
            let mut db = if p > 0.0 { 10.0 * p.log10() } else { f32::NEG_INFINITY };
            // `f32::max` は NaN の側を捨てるので、壊れたビンが表示へ NaN を持ち込まない。
            if let Some(pos) = bins.interp_at {
                db = db.max(self.interp_db(pos));
            }
            let db = db + self.band_slope_db[b];
            if db > out[b] {
                out[b] = db;
            }
        }
    }

    /// 補間の節点 (ビン 1..=`node_hi`) の dB と PCHIP の傾きを、今の `bin_power` から埋める。
    fn fill_nodes(&mut self) {
        let hi = self.node_hi;
        if hi == 0 {
            return;
        }
        let last_bin = self.bin_power.len() - 1;
        for (j, v) in self.node_db.iter_mut().enumerate().skip(1) {
            *v = 10.0 * self.bin_power[j].max(BIN_POWER_FLOOR).log10();
        }
        let db = &self.node_db;
        for j in 1..=hi {
            self.node_slope[j] = if j == 1 {
                pchip_end_slope(db[2] - db[1], db[3] - db[2])
            } else if j == last_bin {
                pchip_end_slope(db[j] - db[j - 1], db[j - 1] - db[j - 2])
            } else {
                pchip_inner_slope(db[j] - db[j - 1], db[j + 1] - db[j])
            };
        }
    }

    /// 小数ビン位置 `pos` の dB を、両隣の節点の間の 3 次 Hermite で補間する。最初の節点
    /// (ビン 1) より下は、その値で平らに伸ばす。`fill_nodes` の後に呼ぶ。
    fn interp_db(&self, pos: f32) -> f32 {
        if pos <= 1.0 {
            return self.node_db[1];
        }
        let k = (pos.floor() as usize).min(self.node_hi - 1);
        let t = (pos - k as f32).clamp(0.0, 1.0);
        let (t2, t3) = (t * t, t * t * t);
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        h00 * self.node_db[k]
            + h10 * self.node_slope[k]
            + h01 * self.node_db[k + 1]
            + h11 * self.node_slope[k + 1]
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

    /// 傾き補正で平らになるスペクトル (-4.5 dB/oct) は、低域まで平らに表示される。
    /// 低域でバンドが最近傍ビンを共有すると、1 本のビンの値に補正だけが乗って
    /// 「右上がり + 垂直落ち」の鋸歯になる (4096 点の 35 Hz のビンが受け持つ区間で約 2.2 dB)。
    /// 最初のビン (DC の次) より下は情報が無く平らに伸ばすだけなので、そこから上を見る。
    #[test]
    fn tilt_flattened_spectrum_reads_flat_down_to_the_low_end() {
        for fft in SpectrumFft::ALL {
            let fs = 48_000;
            let s = MeterSettings { spectrum_fft: fft, ..MeterSettings::default() };
            assert!((s.spectrum_slope_db_oct - 4.5).abs() < 1e-6);
            let mut a = SpectrumAnalyzer::new(fs, &s);
            let bin_hz = fs as f32 / fft.size() as f32;
            for k in 0..a.bin_power.len() {
                let f = k.max(1) as f32 * bin_hz;
                let db = -4.5 * (f / 1000.0).log2();
                a.bin_power[k] = 10f32.powf(db / 10.0);
            }
            a.fill_nodes();
            let mut out = vec![f32::NEG_INFINITY; SPECTRUM_BANDS];
            a.fold_bands(&mut out);
            for (b, db) in out.iter().enumerate() {
                let f = band_center_hz(b);
                if f < bin_hz {
                    continue;
                }
                assert!(db.abs() < 0.25, "{}: band {b} ({f:.1} Hz) = {db:.2} dB", fft.label());
            }
        }
    }

    /// 補間はビンの間で行き過ぎない (隣り合う 2 本のビンの値の範囲に収まる)。行き過ぎると
    /// 音に無い山や谷が低域に描かれる。
    #[test]
    fn interpolation_never_overshoots_the_neighboring_bins() {
        for fft in SpectrumFft::ALL {
            let mut a = SpectrumAnalyzer::new(48_000, &MeterSettings { spectrum_fft: fft, ..MeterSettings::default() });
            // 決定的な擬似乱数で -90..0 dB のでこぼこなスペクトルを作る。
            let mut x = 0x2545_f491_u32;
            for p in &mut a.bin_power {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let db = -90.0 * (x as f32 / u32::MAX as f32);
                *p = 10f32.powf(db / 10.0);
            }
            a.fill_nodes();
            for bins in &a.band_bins {
                let Some(pos) = bins.interp_at else { continue };
                if pos <= 1.0 {
                    continue;
                }
                let k = pos.floor() as usize;
                let (y0, y1) = (a.node_db[k], a.node_db[k + 1]);
                let v = a.interp_db(pos);
                assert!(
                    v >= y0.min(y1) - 1e-3 && v <= y0.max(y1) + 1e-3,
                    "{}: pos {pos} = {v} not within [{y0}, {y1}]",
                    fft.label()
                );
            }
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
