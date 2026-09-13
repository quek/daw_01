//! 内蔵 EQ (6 バンド) の値。係数の組み方は `crate::dsp` が持つ。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::super::ParamRange;

/// EQ の 6 段。**位置ではなくこの enum が住所** (不変条件 1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum EqBand {
    /// ハイパスフィルタ (12 dB/oct)。
    Hp,
    /// ローパスフィルタ (12 dB/oct)。
    Lp,
    /// 低域 (既定シェルビング、`bell` でベルへ)。
    Lf,
    /// 低中域 (常にベル、Q あり)。
    Lmf,
    /// 高中域 (常にベル、Q あり)。
    Hmf,
    /// 高域 (既定シェルビング、`bell` でベルへ)。
    Hf,
}

impl EqBand {
    /// DSP の段順 (`crate::dsp::eq_stages` の出力順)。
    pub const ALL: [Self; 6] = [Self::Hp, Self::Lp, Self::Lf, Self::Lmf, Self::Hmf, Self::Hf];
    /// 周波数の低い順 (Par の列順 / カーブ点の順)。
    pub const BY_FREQ: [Self; 6] = [Self::Hp, Self::Lf, Self::Lmf, Self::Hmf, Self::Hf, Self::Lp];
    /// ゲインとベル/シェルフを持つ 4 バンド (フィルタを除く)、Mixer 帯の表示順 (高→低)。
    pub const GAIN_BANDS: [Self; 4] = [Self::Hf, Self::Hmf, Self::Lmf, Self::Lf];

    /// このバンドの周波数可動範囲。Harrison 32C / SSL 9000 の帯域割りに倣う。
    #[must_use]
    pub fn freq_range(self) -> ParamRange {
        let (lo, hi) = match self {
            Self::Hp => (20.0, 3_100.0),
            Self::Lp => (160.0, 20_000.0),
            Self::Lf => (20.0, 600.0),
            Self::Lmf => (60.0, 2_000.0),
            Self::Hmf => (400.0, 8_000.0),
            Self::Hf => (1_500.0, 20_000.0),
        };
        ParamRange::Log { lo, hi }
    }

    /// Q ノブを出すバンドか (Mixbus / Reason と同じく中域のみ)。
    #[must_use]
    pub fn has_q_knob(self) -> bool {
        matches!(self, Self::Lmf | Self::Hmf)
    }

    /// シェルフ / ベルを切り替えられるバンドか (両端のみ)。
    #[must_use]
    pub fn has_bell_switch(self) -> bool {
        matches!(self, Self::Lf | Self::Hf)
    }

    /// ゲインを持つバンドか (フィルタ HP / LP 以外)。
    #[must_use]
    pub fn has_gain(self) -> bool {
        !matches!(self, Self::Hp | Self::Lp)
    }

    /// 短いラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Hp => "HP",
            Self::Lp => "LP",
            Self::Lf => "LF",
            Self::Lmf => "LMF",
            Self::Hmf => "HMF",
            Self::Hf => "HF",
        }
    }
}

/// EQ 1 バンドの連続パラメータ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum EqParam {
    Freq,
    Gain,
    Q,
}

impl EqParam {
    pub const ALL: [Self; 3] = [Self::Freq, Self::Gain, Self::Q];

    /// `band` におけるこのパラメータの可動範囲。
    #[must_use]
    pub fn range(self, band: EqBand) -> ParamRange {
        match self {
            Self::Freq => band.freq_range(),
            Self::Gain => {
                ParamRange::Linear { lo: -f64::from(EQ_GAIN_LIMIT_DB), hi: f64::from(EQ_GAIN_LIMIT_DB) }
            }
            Self::Q => ParamRange::Log { lo: f64::from(EQ_Q_MIN), hi: f64::from(EQ_Q_MAX) },
        }
    }

    /// 短いラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Freq => "Freq",
            Self::Gain => "Gain",
            Self::Q => "Q",
        }
    }
}

/// EQ ゲインの上下限 (dB)。Harrison 32C の ±15 dB に合わせる。
pub const EQ_GAIN_LIMIT_DB: f32 = 15.0;
/// EQ の Q 可動範囲。
pub const EQ_Q_MIN: f32 = 0.3;
/// EQ の Q 可動範囲。
pub const EQ_Q_MAX: f32 = 3.0;
/// シェルビング動作時の固定 Q (ベル切替時は `EqBandSettings::q` を使う)。
pub const EQ_SHELF_Q: f32 = 0.7;

/// EQ 1 バンドの設定。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct EqBandSettings {
    /// このバンドを通すか。フィルタ (HP/LP) は既定 off、ゲインバンドは既定 on
    /// (ゲイン 0 dB なので on でも音は変わらない)。
    pub on: bool,
    pub freq_hz: f32,
    /// フィルタ (HP/LP) では未使用。
    pub gain_db: f32,
    /// シェルビング時は [`EQ_SHELF_Q`] を使うので未使用。
    pub q: f32,
    /// `true` でベル (ピーキング)。両端バンドのみ意味を持つ。
    pub bell: bool,
}

impl EqBandSettings {
    fn new(on: bool, freq_hz: f32, q: f32) -> Self {
        Self { on, freq_hz, gain_db: 0.0, q, bell: false }
    }
}

/// EQ の値。バンドは**名前付きフィールド**で持つ (配列 index を住所にしない、不変条件 1)。
/// ON/OFF は [`super::NativeDevice::bypassed`] が唯一の SSoT なので持たない。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(default)]
pub struct EqSettings {
    pub hp: EqBandSettings,
    pub lp: EqBandSettings,
    pub lf: EqBandSettings,
    pub lmf: EqBandSettings,
    pub hmf: EqBandSettings,
    pub hf: EqBandSettings,
}

impl Default for EqSettings {
    fn default() -> Self {
        Self {
            hp: EqBandSettings::new(false, 80.0, EQ_SHELF_Q),
            lp: EqBandSettings::new(false, 12_000.0, EQ_SHELF_Q),
            lf: EqBandSettings::new(true, 100.0, EQ_SHELF_Q),
            lmf: EqBandSettings::new(true, 400.0, 0.7),
            hmf: EqBandSettings::new(true, 2_500.0, 0.7),
            hf: EqBandSettings::new(true, 8_000.0, EQ_SHELF_Q),
        }
    }
}

impl EqSettings {
    #[must_use]
    pub fn band(&self, band: EqBand) -> &EqBandSettings {
        match band {
            EqBand::Hp => &self.hp,
            EqBand::Lp => &self.lp,
            EqBand::Lf => &self.lf,
            EqBand::Lmf => &self.lmf,
            EqBand::Hmf => &self.hmf,
            EqBand::Hf => &self.hf,
        }
    }

    #[must_use]
    pub fn band_mut(&mut self, band: EqBand) -> &mut EqBandSettings {
        match band {
            EqBand::Hp => &mut self.hp,
            EqBand::Lp => &mut self.lp,
            EqBand::Lf => &mut self.lf,
            EqBand::Lmf => &mut self.lmf,
            EqBand::Hmf => &mut self.hmf,
            EqBand::Hf => &mut self.hf,
        }
    }

    /// バンドの連続パラメータを読む。
    #[must_use]
    pub fn param(&self, band: EqBand, param: EqParam) -> f32 {
        let b = self.band(band);
        match param {
            EqParam::Freq => b.freq_hz,
            EqParam::Gain => b.gain_db,
            EqParam::Q => b.q,
        }
    }

    pub(super) fn param_mut(&mut self, band: EqBand, param: EqParam) -> &mut f32 {
        let b = self.band_mut(band);
        match param {
            EqParam::Freq => &mut b.freq_hz,
            EqParam::Gain => &mut b.gain_db,
            EqParam::Q => &mut b.q,
        }
    }

    /// フィールド単位の値域回復。**住所として実在しない組 (HP の Gain / LF の Q 等) も回す** —
    /// LF / HF の Q はベルのとき DSP が使うし、NaN が残ると係数が壊れる。冪等。
    pub fn sanitize(&mut self) {
        let d = Self::default();
        for band in EqBand::ALL {
            for p in EqParam::ALL {
                let v = self.param_mut(band, p);
                *v = if v.is_finite() { p.range(band).clamp(*v) } else { d.param(band, p) };
            }
        }
    }
}
