//! ステレオ表示 — ゴニオメーター / 位相相関 / ステレオ幅 / 左右バランス (r.md #50)。
//!
//! 相関は Fons Adriaensen の `stcorrdsp` (jmeters / x42 meters.lv2) をそのまま
//! 移植する。2kHz の 1 極 LPF を通してから時定数 0.3 秒の指数移動平均で
//! `zlr / sqrt(zll * zrr)` を取る。**この LPF を外すと高域の位相ずれで表示が
//! 常時 0 付近に張り付く**ので省略できない。
//!
//! 幅とバランスは同じ EMA インフラから導く (`P_L` / `P_R` / `C = E[LR]` の 3 本)。
//! 左右等パワーのとき `r = (1 - W²)/(1 + W²)` が厳密に成り立つので、3 つの値は
//! 独立ではなく 1 組の状態から導出される (SSoT)。

/// 相関検出の前段 LPF カットオフ [Hz] (x42/Fons の実引数)。
const CORR_LPF_HZ: f32 = 2000.0;
/// 相関の時定数 [秒]。
const CORR_TAU_SECS: f32 = 0.3;
/// 幅の時定数 [秒]。
const WIDTH_TAU_SECS: f32 = 0.3;
/// バランスの時定数 [秒] (Voxengo SPAN と同じ 3 秒平均)。
const BALANCE_TAU_SECS: f32 = 3.0;
/// デノーマル回避の微小値 (Fons の実装と同じ)。
const DENORM: f32 = 1e-20;
/// 相関の分母のパワーフロア (移植元の `sqrtf(zll*zrr + 1e-10f)` の 1e-10)。
/// 無音では分母が振幅 1e-5 に固定され、相関は 0 へ落ちる。
const CORR_FLOOR: f32 = 1e-10;

/// 1 ティックにゴニオへ渡す点の上限。48kHz / 30Hz なら 1600 点なので通常は
/// 全点そのまま、省電力からの復帰など長い塊が来たときだけ間引く。
pub const MAX_GONIO_POINTS: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StereoReadout {
    /// -1.0 (逆相) .. +1.0 (モノ)。
    pub correlation: f32,
    /// リセット以降に観測した相関の最小 / 最大。
    pub correlation_min: f32,
    pub correlation_max: f32,
    /// ステレオ幅 `rms(S)/rms(M)`。0 = モノ、1 = 無相関、2 で clamp。
    pub width: f32,
    /// 左右バランス `10*log10(P_R/P_L)` [dB]。負 = 左が大きい。
    pub balance_db: f32,
}

impl Default for StereoReadout {
    fn default() -> Self {
        Self {
            correlation: 0.0,
            correlation_min: 0.0,
            correlation_max: 0.0,
            width: 0.0,
            balance_db: 0.0,
        }
    }
}

pub struct StereoMeter {
    sample_rate: u32,
    // stcorrdsp の状態
    w1: f32,
    w2: f32,
    zl: f32,
    zr: f32,
    zlr: f32,
    zll: f32,
    zrr: f32,
    // 幅 / バランス用 EMA
    w_width: f32,
    w_balance: f32,
    p_l: f32,
    p_r: f32,
    c_lr: f32,
    bal_l: f32,
    bal_r: f32,
    corr_min: f32,
    corr_max: f32,
    /// r.md #57: `false` (= トランスポート停止) の間は相関の観測レンジ
    /// (`corr_min` / `corr_max`) を更新しない。`reset_range` が `reset_loudness`
    /// から呼ばれている = 測定セッションに属する量であることが既に確定している。
    /// 相関の現在値 / 幅 / バランス / ゴニオ点はライブのまま。既定 `true`。
    running: bool,
    /// このティックで作ったゴニオ点 (正規化座標、`x = (R-L)/√2`, `y = (L+R)/√2`)。
    points: Vec<[f32; 2]>,
}

impl StereoMeter {
    pub fn new(sample_rate: u32) -> Self {
        let mut me = Self {
            sample_rate: sample_rate.max(1),
            w1: 0.0,
            w2: 0.0,
            zl: 0.0,
            zr: 0.0,
            zlr: 0.0,
            zll: 0.0,
            zrr: 0.0,
            w_width: 0.0,
            w_balance: 0.0,
            p_l: 0.0,
            p_r: 0.0,
            c_lr: 0.0,
            bal_l: 0.0,
            bal_r: 0.0,
            corr_min: 1.0,
            corr_max: -1.0,
            running: true,
            points: Vec::with_capacity(MAX_GONIO_POINTS),
        };
        me.design();
        me
    }

    /// r.md #57: 相関レンジの積算を running / stand-by で切り替える。
    pub fn set_running(&mut self, running: bool) {
        self.running = running;
    }

    fn design(&mut self) {
        let fs = self.sample_rate as f32;
        self.w1 = std::f32::consts::TAU * CORR_LPF_HZ / fs;
        self.w2 = 1.0 / (CORR_TAU_SECS * fs);
        self.w_width = 1.0 / (WIDTH_TAU_SECS * fs);
        self.w_balance = 1.0 / (BALANCE_TAU_SECS * fs);
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let sr = sample_rate.max(1);
        if sr != self.sample_rate {
            self.sample_rate = sr;
            self.design();
        }
    }

    /// 相関の観測レンジをリセットする (現在値は保つ)。
    pub fn reset_range(&mut self) {
        self.corr_min = 1.0;
        self.corr_max = -1.0;
    }

    pub fn process(&mut self, frames: &[[f32; 2]]) {
        self.points.clear();
        if frames.is_empty() {
            return;
        }
        for fr in frames {
            let (l, r) = (fr[0], fr[1]);
            // --- 相関 (stcorrdsp) ---
            self.zl += self.w1 * (l - self.zl) + DENORM;
            self.zr += self.w1 * (r - self.zr) + DENORM;
            self.zlr += self.w2 * (self.zl * self.zr - self.zlr);
            self.zll += self.w2 * (self.zl * self.zl - self.zll);
            self.zrr += self.w2 * (self.zr * self.zr - self.zrr);
            // --- 幅 / バランス ---
            self.p_l += self.w_width * (l * l - self.p_l);
            self.p_r += self.w_width * (r * r - self.p_r);
            self.c_lr += self.w_width * (l * r - self.c_lr);
            self.bal_l += self.w_balance * (l * l - self.bal_l);
            self.bal_r += self.w_balance * (r * r - self.bal_r);
        }
        // 非有限に落ちたらリセットする (Fons の実装と同じ防御)。**幅 / バランスの
        // EMA も一緒に畳む** — 片方だけ守ると、相関は復帰したのに幅とバランスだけ
        // NaN のまま (readout の `> 1e-12` 比較が false になって 0 固定) になり、
        // 音が正常に戻っても永久に動かない表示が残る。
        if !(self.zlr.is_finite() && self.zll.is_finite() && self.zrr.is_finite()) {
            self.zl = 0.0;
            self.zr = 0.0;
            self.zlr = 0.0;
            self.zll = 0.0;
            self.zrr = 0.0;
        }
        if !(self.p_l.is_finite()
            && self.p_r.is_finite()
            && self.c_lr.is_finite()
            && self.bal_l.is_finite()
            && self.bal_r.is_finite())
        {
            self.p_l = 0.0;
            self.p_r = 0.0;
            self.c_lr = 0.0;
            self.bal_l = 0.0;
            self.bal_r = 0.0;
        }
        self.zlr += 1e-10;
        self.zll += 1e-10;
        self.zrr += 1e-10;

        let c = self.correlation();
        if self.running {
            if c < self.corr_min {
                self.corr_min = c;
            }
            if c > self.corr_max {
                self.corr_max = c;
            }
        }

        // --- ゴニオ点 ---
        let stride = frames.len().div_ceil(MAX_GONIO_POINTS).max(1);
        const INV_SQRT2: f32 = std::f32::consts::FRAC_1_SQRT_2;
        for fr in frames.iter().step_by(stride) {
            let (l, r) = (fr[0], fr[1]);
            self.points.push([(r - l) * INV_SQRT2, (l + r) * INV_SQRT2]);
        }
    }

    /// `zlr / sqrt(zll*zrr + ε)`。
    ///
    /// **ε は sqrt の内側**でなければならない (移植元と同じ)。外に出すと
    /// パワー領域のフロアが振幅 1e-10 に化けて事実上効かず、無音時に
    /// zlr = zll = zrr (デノーマル除けの加算で 3 本が同じ値へ収束する) の比が
    /// 1 に近づいて「無音なのに完全同相 +0.9」を表示してしまう。
    fn correlation(&self) -> f32 {
        let d = (self.zll * self.zrr + CORR_FLOOR).max(CORR_FLOOR).sqrt();
        (self.zlr / d).clamp(-1.0, 1.0)
    }

    pub fn readout(&self) -> StereoReadout {
        // E[M²] = (P_L + P_R + 2C)/4、E[S²] = (P_L + P_R - 2C)/4。
        let m2 = (self.p_l + self.p_r + 2.0 * self.c_lr) * 0.25;
        let s2 = (self.p_l + self.p_r - 2.0 * self.c_lr) * 0.25;
        let width = if m2 > 1e-12 {
            (s2.max(0.0) / m2).sqrt().min(2.0)
        } else if s2 > 1e-12 {
            2.0
        } else {
            0.0
        };
        let balance_db = if self.bal_l > 1e-12 && self.bal_r > 1e-12 {
            (10.0 * (self.bal_r / self.bal_l).log10()).clamp(-24.0, 24.0)
        } else {
            0.0
        };
        let c = self.correlation();
        StereoReadout {
            correlation: c,
            correlation_min: if self.corr_min <= self.corr_max { self.corr_min } else { c },
            correlation_max: if self.corr_min <= self.corr_max { self.corr_max } else { c },
            width,
            balance_db,
        }
    }

    pub fn points(&self) -> &[[f32; 2]] {
        &self.points
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(fs: u32, secs: f32, f: impl Fn(usize) -> [f32; 2]) -> StereoMeter {
        let mut m = StereoMeter::new(fs);
        let n = (fs as f32 * secs) as usize;
        let frames: Vec<[f32; 2]> = (0..n).map(&f).collect();
        // 実運用と同じく小分けに流す。
        for chunk in frames.chunks(1024) {
            m.process(chunk);
        }
        m
    }

    fn sine(fs: u32, freq: f32, i: usize) -> f32 {
        (std::f32::consts::TAU * freq * i as f32 / fs as f32).sin()
    }

    #[test]
    fn mono_signal_reads_correlation_plus_one() {
        let fs = 48_000;
        let m = run(fs, 3.0, |i| {
            let v = sine(fs, 220.0, i) * 0.5;
            [v, v]
        });
        let r = m.readout();
        assert!(r.correlation > 0.99, "got {}", r.correlation);
        assert!(r.width < 0.02, "width should be ~0 for mono, got {}", r.width);
    }

    #[test]
    fn inverted_signal_reads_correlation_minus_one() {
        let fs = 48_000;
        let m = run(fs, 3.0, |i| {
            let v = sine(fs, 220.0, i) * 0.5;
            [v, -v]
        });
        let r = m.readout();
        assert!(r.correlation < -0.99, "got {}", r.correlation);
        assert!(r.width > 1.9, "width should saturate for out-of-phase, got {}", r.width);
    }

    /// 無関係な 2 音 (無理数比の周波数) は相関 0 付近。
    #[test]
    fn uncorrelated_channels_read_near_zero() {
        let fs = 48_000;
        let m = run(fs, 6.0, |i| {
            [sine(fs, 210.0, i) * 0.5, sine(fs, 317.0, i) * 0.5]
        });
        let r = m.readout();
        assert!(r.correlation.abs() < 0.35, "got {}", r.correlation);
    }

    #[test]
    fn balance_is_negative_when_left_is_louder() {
        let fs = 48_000;
        let m = run(fs, 8.0, |i| {
            let v = sine(fs, 220.0, i);
            [v * 0.5, v * 0.25]
        });
        let b = m.readout().balance_db;
        // 振幅が半分 = パワー 1/4 = -6.02 dB。
        assert!((b - (-6.02)).abs() < 0.5, "got {b}");
    }

    /// ゴニオ座標: モノは縦線 (x = 0)、逆相は横線 (y = 0)。
    #[test]
    fn goniometer_axes_match_the_standard_orientation() {
        let mut m = StereoMeter::new(48_000);
        m.process(&[[0.7, 0.7], [-0.4, -0.4]]);
        for p in m.points() {
            assert!(p[0].abs() < 1e-6, "mono should be a vertical line, got {p:?}");
        }
        m.process(&[[0.7, -0.7]]);
        for p in m.points() {
            assert!(p[1].abs() < 1e-6, "anti-phase should be horizontal, got {p:?}");
        }
    }

    /// 左だけの信号は左上 (x < 0, y > 0)。
    #[test]
    fn left_only_points_to_the_upper_left() {
        let mut m = StereoMeter::new(48_000);
        m.process(&[[1.0, 0.0]]);
        let p = m.points()[0];
        assert!(p[0] < 0.0 && p[1] > 0.0, "got {p:?}");
    }

    /// 大量に流し込んでも点数は上限で抑えられる。
    #[test]
    fn goniometer_points_are_capped() {
        let mut m = StereoMeter::new(48_000);
        m.process(&vec![[0.1, 0.2]; MAX_GONIO_POINTS * 4]);
        assert!(m.points().len() <= MAX_GONIO_POINTS, "got {}", m.points().len());
    }

    #[test]
    fn silence_reports_zero_width_and_flat_balance() {
        let mut m = StereoMeter::new(48_000);
        m.process(&vec![[0.0, 0.0]; 48_000]);
        let r = m.readout();
        assert_eq!(r.width, 0.0);
        assert_eq!(r.balance_db, 0.0);
    }

    /// 無音を流し続けたときの相関は 0 付近であること。
    ///
    /// デノーマル除けで zlr/zll/zrr が同じ微小値へ収束するので、分母のフロアを
    /// sqrt の外に置くと比が 1 に近づいて「無音なのに完全同相 +0.9」を指す
    /// (レビューで発見。実機では停止中ずっと相関バーが振り切ったままになる)。
    #[test]
    fn silence_reports_near_zero_correlation() {
        let mut m = StereoMeter::new(48_000);
        // 実運用と同じ 30Hz ティック相当のブロックで 30 秒ぶん。
        for _ in 0..900 {
            m.process(&vec![[0.0_f32, 0.0]; 1600]);
        }
        let r = m.readout();
        assert!(r.correlation.abs() < 0.01, "correlation = {}", r.correlation);
        assert!(r.correlation_max.abs() < 0.01, "max = {}", r.correlation_max);
    }

    /// ブロック長 (ポーリング間隔) が変わっても無音時の相関は 0 のまま。
    #[test]
    fn silence_correlation_does_not_depend_on_block_length() {
        for block in [256_usize, 1600, 12_000] {
            let mut m = StereoMeter::new(48_000);
            for _ in 0..(48_000 * 30 / block) {
                m.process(&vec![[0.0_f32, 0.0]; block]);
            }
            let c = m.readout().correlation;
            assert!(c.abs() < 0.01, "block={block} correlation={c}");
        }
    }

    /// NaN が 1 サンプル混ざっても、健全な音に戻れば幅 / バランスが復帰する。
    #[test]
    fn a_single_nan_does_not_permanently_freeze_width_and_balance() {
        let fs = 48_000;
        let mut m = StereoMeter::new(fs);
        m.process(&[[f32::NAN, 0.0]]);
        // 以後は正常なステレオ信号。
        let n = fs as usize * 3;
        let frames: Vec<[f32; 2]> = (0..n)
            .map(|i| [sine(fs, 210.0, i) * 0.5, sine(fs, 317.0, i) * 0.5])
            .collect();
        for chunk in frames.chunks(1024) {
            m.process(chunk);
        }
        let r = m.readout();
        assert!(r.width > 0.05, "width が固まったまま: {}", r.width);
        assert!(r.correlation.is_finite());
    }
}
