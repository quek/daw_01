//! チャンネルストリップの DSP 核 (バイクワッド係数・振幅応答・コンプの利得計算)。
//!
//! **daw_audio (音を出す側) と daw_gui (カーブを描く側) が同じ実装を共有する。**
//! 片方に式を写すと、画面のカーブと実際の音が静かに食い違う。
//!
//! 係数は Robert Bristow-Johnson の Audio EQ Cookbook (W3C 版
//! <https://www.w3.org/TR/audio-eq-cookbook/>) をそのまま使う。差分方程式の規約も
//! 同文書の Direct Form 1:
//!
//! ```text
//! y[n] = b0 x[n] + b1 x[n-1] + b2 x[n-2] - a1 y[n-1] - a2 y[n-2]
//! ```
//!
//! (係数は `a0` で正規化済みとして保持する。)
//!
//! RT 規約: この module の関数はヒープ確保・ロック・I/O を行わない。三角関数は
//! **buffer 先頭で係数を組むときだけ**呼ぶ (サンプルループ内では呼ばない)。

use crate::model::{
    COMP_KNEE_DB, CompSettings, EQ_Q_MAX, EQ_Q_MIN, EQ_SHELF_Q, EqBand, EqSettings,
};

/// 正規化済み (a0 = 1) のバイクワッド係数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Biquad {
    /// 素通し (係数を通しても信号が変わらない)。
    pub const IDENTITY: Self = Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    /// `a0` で割って正規化する。
    fn normalized(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        if a0.abs() < f32::EPSILON || !a0.is_finite() {
            return Self::IDENTITY;
        }
        let inv = 1.0 / a0;
        Self { b0: b0 * inv, b1: b1 * inv, b2: b2 * inv, a1: a1 * inv, a2: a2 * inv }
    }

    /// 共通の前処理 (ω0 / cos / α)。Nyquist を越える周波数は素通しに落とす。
    fn prep(sample_rate: f32, freq_hz: f32, q: f32) -> Option<(f32, f32, f32)> {
        if sample_rate <= 0.0 || !freq_hz.is_finite() || freq_hz <= 0.0 {
            return None;
        }
        // Nyquist 直下で係数が発散するので少し内側で止める。
        let f = freq_hz.min(sample_rate * 0.49);
        let q = q.clamp(EQ_Q_MIN * 0.5, EQ_Q_MAX * 4.0);
        let w0 = std::f32::consts::TAU * f / sample_rate;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() / (2.0 * q);
        Some((cos_w0, alpha, w0))
    }

    #[must_use]
    pub fn low_pass(sample_rate: f32, freq_hz: f32, q: f32) -> Self {
        let Some((c, alpha, _)) = Self::prep(sample_rate, freq_hz, q) else {
            return Self::IDENTITY;
        };
        Self::normalized((1.0 - c) * 0.5, 1.0 - c, (1.0 - c) * 0.5, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    #[must_use]
    pub fn high_pass(sample_rate: f32, freq_hz: f32, q: f32) -> Self {
        let Some((c, alpha, _)) = Self::prep(sample_rate, freq_hz, q) else {
            return Self::IDENTITY;
        };
        Self::normalized(
            (1.0 + c) * 0.5,
            -(1.0 + c),
            (1.0 + c) * 0.5,
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        )
    }

    /// ピーク利得 0 dB のバンドパス (検出フィルタ用)。
    #[must_use]
    pub fn band_pass(sample_rate: f32, freq_hz: f32, q: f32) -> Self {
        let Some((c, alpha, _)) = Self::prep(sample_rate, freq_hz, q) else {
            return Self::IDENTITY;
        };
        Self::normalized(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    #[must_use]
    pub fn peaking(sample_rate: f32, freq_hz: f32, q: f32, gain_db: f32) -> Self {
        let Some((c, alpha, _)) = Self::prep(sample_rate, freq_hz, q) else {
            return Self::IDENTITY;
        };
        let a = 10f32.powf(gain_db / 40.0);
        Self::normalized(
            1.0 + alpha * a,
            -2.0 * c,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * c,
            1.0 - alpha / a,
        )
    }

    #[must_use]
    pub fn low_shelf(sample_rate: f32, freq_hz: f32, q: f32, gain_db: f32) -> Self {
        let Some((c, alpha, _)) = Self::prep(sample_rate, freq_hz, q) else {
            return Self::IDENTITY;
        };
        let a = 10f32.powf(gain_db / 40.0);
        let sqrt_a2 = 2.0 * a.sqrt() * alpha;
        Self::normalized(
            a * ((a + 1.0) - (a - 1.0) * c + sqrt_a2),
            2.0 * a * ((a - 1.0) - (a + 1.0) * c),
            a * ((a + 1.0) - (a - 1.0) * c - sqrt_a2),
            (a + 1.0) + (a - 1.0) * c + sqrt_a2,
            -2.0 * ((a - 1.0) + (a + 1.0) * c),
            (a + 1.0) + (a - 1.0) * c - sqrt_a2,
        )
    }

    #[must_use]
    pub fn high_shelf(sample_rate: f32, freq_hz: f32, q: f32, gain_db: f32) -> Self {
        let Some((c, alpha, _)) = Self::prep(sample_rate, freq_hz, q) else {
            return Self::IDENTITY;
        };
        let a = 10f32.powf(gain_db / 40.0);
        let sqrt_a2 = 2.0 * a.sqrt() * alpha;
        Self::normalized(
            a * ((a + 1.0) + (a - 1.0) * c + sqrt_a2),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
            a * ((a + 1.0) + (a - 1.0) * c - sqrt_a2),
            (a + 1.0) - (a - 1.0) * c + sqrt_a2,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - sqrt_a2,
        )
    }

    /// `freq_hz` における振幅応答 (dB)。カーブ描画と検証テストの唯一の口。
    ///
    /// `|H(e^{jw})|` を係数から直接評価する (フィルタを走らせない)。
    #[must_use]
    pub fn magnitude_db(&self, sample_rate: f32, freq_hz: f32) -> f32 {
        if sample_rate <= 0.0 {
            return 0.0;
        }
        let w = std::f32::consts::TAU * freq_hz / sample_rate;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        let num = (num_re * num_re + num_im * num_im).sqrt();
        let den = (den_re * den_re + den_im * den_im).sqrt();
        if den <= f32::MIN_POSITIVE || num <= f32::MIN_POSITIVE {
            return -120.0;
        }
        20.0 * (num / den).log10()
    }
}

/// Direct Form 1 の遅延状態 (1 チャンネル分)。
#[derive(Debug, Clone, Copy, Default)]
pub struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl BiquadState {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// 1 サンプル進める。RT-safe (確保・分岐の重い処理なし)。
    #[inline]
    #[must_use]
    pub fn process(&mut self, c: &Biquad, x: f32) -> f32 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        // 無音区間では出力が指数的に 0 へ近づき、非正規化数に落ちると 1 サンプルの
        // 演算が数十倍遅くなる (= 無音のトラックほど重い)。閾値以下は 0 に潰す。
        // 発散 (NaN / inf) もここで断ち切る — 一度混ざると状態が二度と戻らない。
        let y = if y.is_finite() && y.abs() > 1e-25 { y } else { 0.0 };
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// EQ の段数 ([`EqBand::ALL`] と同順)。
pub const EQ_STAGES: usize = 6;

/// [`EqSettings`] から 6 段の係数を組む。`on = false` の段は素通し。
///
/// 三角関数を 6 回まわすので **buffer 先頭で 1 回だけ**呼ぶ (RT のサンプル
/// ループ内では呼ばない)。
#[must_use]
pub fn eq_stages(eq: &EqSettings, sample_rate: f32) -> [Biquad; EQ_STAGES] {
    let mut out = [Biquad::IDENTITY; EQ_STAGES];
    if !eq.on {
        return out;
    }
    for (slot, band) in out.iter_mut().zip(EqBand::ALL) {
        let b = eq.band(band);
        if !b.on {
            continue;
        }
        *slot = match band {
            EqBand::Hp => Biquad::high_pass(sample_rate, b.freq_hz, EQ_SHELF_Q),
            EqBand::Lp => Biquad::low_pass(sample_rate, b.freq_hz, EQ_SHELF_Q),
            EqBand::Lmf | EqBand::Hmf => {
                Biquad::peaking(sample_rate, b.freq_hz, b.q, b.gain_db)
            }
            EqBand::Lf => {
                if b.bell {
                    Biquad::peaking(sample_rate, b.freq_hz, b.q, b.gain_db)
                } else {
                    Biquad::low_shelf(sample_rate, b.freq_hz, EQ_SHELF_Q, b.gain_db)
                }
            }
            EqBand::Hf => {
                if b.bell {
                    Biquad::peaking(sample_rate, b.freq_hz, b.q, b.gain_db)
                } else {
                    Biquad::high_shelf(sample_rate, b.freq_hz, EQ_SHELF_Q, b.gain_db)
                }
            }
        };
    }
    out
}

/// EQ 全体の振幅応答 (dB)。HP/LP を含む合成カーブ = strip に描く線そのもの。
#[must_use]
pub fn eq_magnitude_db(stages: &[Biquad; EQ_STAGES], sample_rate: f32, freq_hz: f32) -> f32 {
    stages.iter().map(|s| s.magnitude_db(sample_rate, freq_hz)).sum()
}

/// 検出フィルタ (バンドパス) の Q。**周波数から一意に決まる** (§5.3)。
///
/// 低域では広く (地鳴りを外す用途は緩い山でよい)、高域では締まる (歯擦音を
/// 狙う)。`Q(20Hz) = 0.3` / `Q(16kHz) = 3.0` を対数で結んだだけの式:
///
/// ```text
/// Q(f) = Q_MIN * (f / F_MIN) ^ (ln(Q_MAX/Q_MIN) / ln(F_MAX/F_MIN))
/// ```
#[must_use]
pub fn sc_filter_q(freq_hz: f32) -> f32 {
    const F_MIN: f32 = 20.0;
    const F_MAX: f32 = 16_000.0;
    if freq_hz <= F_MIN {
        return EQ_Q_MIN;
    }
    let exponent = (EQ_Q_MAX / EQ_Q_MIN).ln() / (F_MAX / F_MIN).ln();
    (EQ_Q_MIN * (freq_hz / F_MIN).powf(exponent)).clamp(EQ_Q_MIN, EQ_Q_MAX)
}

/// 検出フィルタの係数。`sc_freq_hz == 0` (OFF) なら `None` = フルレンジ検出。
#[must_use]
pub fn sc_filter(comp: &CompSettings, sample_rate: f32) -> Option<Biquad> {
    if comp.sc_freq_hz <= 0.0 {
        return None;
    }
    Some(Biquad::band_pass(sample_rate, comp.sc_freq_hz, sc_filter_q(comp.sc_freq_hz)))
}

/// 静的圧縮カーブ: 検出レベル (dBFS) → 利得変化量 (dB、0 以下)。
///
/// ソフトニー ([`COMP_KNEE_DB`] 幅) はニーの中で二次関数で繋ぐ標準形。
/// `ratio <= 1` は圧縮なし。
#[must_use]
pub fn comp_static_gain_db(level_db: f32, threshold_db: f32, ratio: f32) -> f32 {
    if ratio <= 1.0 {
        return 0.0;
    }
    let slope = 1.0 / ratio - 1.0; // 負値
    let over = level_db - threshold_db;
    let half_knee = COMP_KNEE_DB * 0.5;
    if over <= -half_knee {
        0.0
    } else if over >= half_knee {
        slope * over
    } else {
        let x = over + half_knee;
        slope * x * x / (2.0 * COMP_KNEE_DB)
    }
}

/// 一次ローパスの平滑係数。`ms` 経過で目標との差が `1/e` になる。
#[must_use]
pub fn smoothing_coeff(ms: f32, sample_rate: f32) -> f32 {
    if ms <= 0.0 || sample_rate <= 0.0 {
        return 0.0;
    }
    (-1.0 / (ms * 0.001 * sample_rate)).exp()
}

/// 線形振幅 → dBFS (無音は `-120`)。
#[must_use]
pub fn amp_to_db(amp: f32) -> f32 {
    if amp <= 1e-6 { -120.0 } else { 20.0 * amp.log10() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EqBandSettings, EqParam};

    const SR: f32 = 48_000.0;

    #[test]
    fn バイパス中の_eq_は全帯域でフラット() {
        let eq = EqSettings { on: false, ..Default::default() };
        let stages = eq_stages(&eq, SR);
        for f in [30.0, 200.0, 1_000.0, 8_000.0, 16_000.0] {
            assert!(eq_magnitude_db(&stages, SR, f).abs() < 1e-4, "{f}Hz");
        }
    }

    #[test]
    fn ハイパスは下を落として上を通す() {
        let mut eq = EqSettings { on: true, ..Default::default() };
        eq.hp = EqBandSettings { on: true, freq_hz: 200.0, ..eq.hp };
        let stages = eq_stages(&eq, SR);
        // カットオフで -3dB 付近、1 オクターブ下は約 -12dB、上は素通し。
        let at_fc = eq_magnitude_db(&stages, SR, 200.0);
        let one_oct_below = eq_magnitude_db(&stages, SR, 100.0);
        let above = eq_magnitude_db(&stages, SR, 2_000.0);
        assert!((at_fc - -3.0).abs() < 1.5, "fc={at_fc}");
        assert!(one_oct_below < -10.0 && one_oct_below > -16.0, "-1oct={one_oct_below}");
        assert!(above.abs() < 0.5, "above={above}");
    }

    #[test]
    fn ベルは中心で指定ゲインになる() {
        let mut eq = EqSettings { on: true, ..Default::default() };
        eq.hmf.gain_db = 6.0;
        eq.hmf.q = 2.0;
        let stages = eq_stages(&eq, SR);
        let at_center = eq_magnitude_db(&stages, SR, eq.hmf.freq_hz);
        assert!((at_center - 6.0).abs() < 0.1, "center={at_center}");
        // 十分離れた帯域は影響を受けない。
        assert!(eq_magnitude_db(&stages, SR, 60.0).abs() < 0.5);
    }

    #[test]
    fn シェルフは帯域端で指定ゲインへ漸近する() {
        let mut eq = EqSettings { on: true, ..Default::default() };
        eq.lf.gain_db = -9.0;
        eq.lf.freq_hz = 200.0;
        let stages = eq_stages(&eq, SR);
        assert!((eq_magnitude_db(&stages, SR, 20.0) - -9.0).abs() < 1.0);
        assert!(eq_magnitude_db(&stages, SR, 5_000.0).abs() < 0.3);
    }

    #[test]
    fn 係数を走らせた出力が振幅応答と一致する() {
        // 「式は合っているが実装がずれている」を潰す: 正弦波を通した実測 RMS と
        // `magnitude_db` の解析値を突き合わせる。
        let c = Biquad::peaking(SR, 1_000.0, 1.0, 6.0);
        let mut st = BiquadState::default();
        let f = 1_000.0_f32;
        let mut peak = 0.0_f32;
        // 定常状態になるまで空回し → その後 1 周期分の最大値を測る。
        for i in 0..4_800 {
            let x = (std::f32::consts::TAU * f * i as f32 / SR).sin();
            let y = st.process(&c, x);
            if i > 2_400 {
                peak = peak.max(y.abs());
            }
        }
        let measured_db = 20.0 * peak.log10();
        let analytic_db = c.magnitude_db(SR, f);
        assert!(
            (measured_db - analytic_db).abs() < 0.1,
            "measured={measured_db} analytic={analytic_db}"
        );
    }

    #[test]
    fn 検出フィルタの_q_は周波数とともに締まる() {
        let cases = [(20.0, 0.30), (200.0, 0.66), (2_000.0, 1.46), (16_000.0, 3.00)];
        for (f, expected) in cases {
            let q = sc_filter_q(f);
            assert!((q - expected).abs() < 0.02, "{f}Hz: q={q} expected={expected}");
        }
    }

    #[test]
    fn 圧縮カーブはニーを挟んで理論値に載る() {
        // ratio 4:1 / threshold -20dB / knee 6dB。
        let cases = [
            // (入力 dB, 期待 GR dB)
            (-40.0, 0.0),              // ニーの下 = 無圧縮
            (-23.0, 0.0),              // ニー下端ちょうど
            (-20.0, -0.5625),          // ニー中央 = 二次補間
            (-17.0, -2.25),            // ニー上端 = 直線と接続
            (-10.0, -7.5),             // 直線部: 10dB 超過 × (1/4 - 1)
            (0.0, -15.0),
        ];
        for (level, expected) in cases {
            let gr = comp_static_gain_db(level, -20.0, 4.0);
            assert!((gr - expected).abs() < 1e-3, "level={level} gr={gr} expected={expected}");
        }
        // ratio 1:1 は常に無圧縮。
        assert_eq!(comp_static_gain_db(0.0, -40.0, 1.0), 0.0);
    }

    #[test]
    fn ゲイン範囲の両端でも係数が発散しない() {
        for gain in [-EqParam::Gain.range(EqBand::Hf).display_range().1 as f32, 15.0] {
            for band in EqBand::GAIN_BANDS {
                let mut eq = EqSettings { on: true, ..Default::default() };
                eq.band_mut(band).gain_db = gain;
                let stages = eq_stages(&eq, SR);
                for f in [20.0, 1_000.0, 20_000.0] {
                    assert!(eq_magnitude_db(&stages, SR, f).is_finite(), "{band:?} {gain} {f}");
                }
            }
        }
    }
}
