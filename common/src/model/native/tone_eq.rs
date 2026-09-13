//! 内蔵 Tone EQ (3 バンド固定周波数、ゲインのみ。Mixbus のトーンコントロール流) の値。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::EQ_SHELF_Q;
use super::super::ParamRange;

/// トーン EQ の 3 バンド。**周波数は固定**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum ToneEqBand {
    /// 90Hz ローシェルフ。
    Low,
    /// 300Hz ワイドベル (タープサチュレーションの倍音が乗る帯域)。
    LoMid,
    /// 8kHz ハイシェルフ。
    High,
}

impl ToneEqBand {
    pub const ALL: [Self; 3] = [Self::Low, Self::LoMid, Self::High];

    /// このバンドの固定中心周波数 (Hz)。
    #[must_use]
    pub fn freq_hz(self) -> f32 {
        match self {
            Self::Low => 90.0,
            Self::LoMid => 300.0,
            Self::High => 8_000.0,
        }
    }

    /// ベル (ピーキング) なら `Some(Q)`、シェルビングなら `None`。
    #[must_use]
    pub fn bell_q(self) -> Option<f32> {
        match self {
            Self::LoMid => Some(EQ_SHELF_Q),
            Self::Low | Self::High => None,
        }
    }

    /// ゲインの可動範囲。
    #[must_use]
    pub fn range(self) -> ParamRange {
        let lim = f64::from(TONE_EQ_LIMIT_DB);
        ParamRange::Linear { lo: -lim, hi: lim }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::LoMid => "LoMid",
            Self::High => "High",
        }
    }
}

/// トーン EQ のゲイン上下限 (dB)。**狭い**のは意図 — 最終段で大きく動かすのは事故で、
/// 狙った帯域を追い込むのは insert の EQ の仕事 (Mixbus の思想)。
pub const TONE_EQ_LIMIT_DB: f32 = 6.0;

/// Tone EQ の値。ON/OFF は [`super::NativeDevice::bypassed`] が唯一の SSoT。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
#[serde(default)]
pub struct ToneEqSettings {
    pub low_db: f32,
    pub lomid_db: f32,
    pub high_db: f32,
}

impl ToneEqSettings {
    #[must_use]
    pub fn gain_db(&self, band: ToneEqBand) -> f32 {
        match band {
            ToneEqBand::Low => self.low_db,
            ToneEqBand::LoMid => self.lomid_db,
            ToneEqBand::High => self.high_db,
        }
    }

    pub(super) fn gain_mut(&mut self, band: ToneEqBand) -> &mut f32 {
        match band {
            ToneEqBand::Low => &mut self.low_db,
            ToneEqBand::LoMid => &mut self.lomid_db,
            ToneEqBand::High => &mut self.high_db,
        }
    }

    /// 3 ゲインの値域回復 (非有限は 0 dB)。冪等。
    pub fn sanitize(&mut self) {
        for band in ToneEqBand::ALL {
            let v = self.gain_mut(band);
            *v = if v.is_finite() { band.range().clamp(*v) } else { 0.0 };
        }
    }
}
