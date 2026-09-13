//! 内蔵 device のパラメーター住所 [`NativeParamId`] (オートメーション / 変調 / MIDI Learn の的)。
//!
//! 住所は **device id + この enum** (`AutomationTarget::NativeParam`)。種類を住所に含めるので、
//! song を引かずに色・値域・ラベルが決まる。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::{
    BusCompParam, BusCompSettings, CompParam, CompSettings, EqBand, EqParam, EqSettings,
    NativeKind, ParamRange, ToneEqBand, ToneEqSettings,
};

/// 内蔵 device のパラメーター住所。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum NativeParamId {
    /// ON/OFF (値域 Toggle、値 = `!bypassed`)。種類を持つので song を引かずに照合できる。
    On(NativeKind),
    Comp(CompParam),
    /// 実在は 12 組 ([`NativeParamId::exists`])。
    Eq { band: EqBand, param: EqParam },
    BusComp(BusCompParam),
    ToneEq(ToneEqBand),
}

use NativeParamId as P;

const COMP_ALL: [NativeParamId; 7] = [
    P::On(NativeKind::Comp),
    P::Comp(CompParam::Threshold),
    P::Comp(CompParam::Ratio),
    P::Comp(CompParam::Attack),
    P::Comp(CompParam::Release),
    P::Comp(CompParam::ScFreq),
    P::Comp(CompParam::Makeup),
];

const fn eq(band: EqBand, param: EqParam) -> NativeParamId {
    P::Eq { band, param }
}

const EQ_ALL: [NativeParamId; 13] = [
    P::On(NativeKind::Eq),
    eq(EqBand::Hp, EqParam::Freq),
    eq(EqBand::Lf, EqParam::Freq),
    eq(EqBand::Lf, EqParam::Gain),
    eq(EqBand::Lmf, EqParam::Freq),
    eq(EqBand::Lmf, EqParam::Gain),
    eq(EqBand::Lmf, EqParam::Q),
    eq(EqBand::Hmf, EqParam::Freq),
    eq(EqBand::Hmf, EqParam::Gain),
    eq(EqBand::Hmf, EqParam::Q),
    eq(EqBand::Hf, EqParam::Freq),
    eq(EqBand::Hf, EqParam::Gain),
    eq(EqBand::Lp, EqParam::Freq),
];

const BUS_COMP_ALL: [NativeParamId; 6] = [
    P::On(NativeKind::BusComp),
    P::BusComp(BusCompParam::Threshold),
    P::BusComp(BusCompParam::Ratio),
    P::BusComp(BusCompParam::Attack),
    P::BusComp(BusCompParam::Release),
    P::BusComp(BusCompParam::Makeup),
];

const TONE_EQ_ALL: [NativeParamId; 4] = [
    P::On(NativeKind::ToneEq),
    P::ToneEq(ToneEqBand::Low),
    P::ToneEq(ToneEqBand::LoMid),
    P::ToneEq(ToneEqBand::High),
];

impl NativeParamId {
    /// この住所が属する種類。
    #[must_use]
    pub fn kind(self) -> NativeKind {
        match self {
            P::On(k) => k,
            P::Comp(_) => NativeKind::Comp,
            P::Eq { .. } => NativeKind::Eq,
            P::BusComp(_) => NativeKind::BusComp,
            P::ToneEq(_) => NativeKind::ToneEq,
        }
    }

    /// 住所として実在するか。Eq: HP / LP = Freq、LF / HF = Freq / Gain、LMF / HMF = Freq / Gain / Q
    /// (つまみもカーブ点も持たない組は住所にしない)。他の種類は常に true。
    #[must_use]
    pub fn exists(self) -> bool {
        match self {
            P::Eq { band, param } => match param {
                EqParam::Freq => true,
                EqParam::Gain => band.has_gain(),
                EqParam::Q => band.has_q_knob(),
            },
            P::On(_) | P::Comp(_) | P::BusComp(_) | P::ToneEq(_) => true,
        }
    }

    /// 値域 (正規化の唯一の表 `common::automation::target_range` がここを引く)。
    #[must_use]
    pub fn range(self) -> ParamRange {
        match self {
            P::On(_) => ParamRange::Toggle,
            P::Comp(p) => p.range(),
            P::Eq { band, param } => param.range(band),
            P::BusComp(p) => p.range(),
            P::ToneEq(b) => b.range(),
        }
    }

    /// つまみの下に出す短いラベル。
    #[must_use]
    pub fn knob_label(self) -> &'static str {
        match self {
            P::On(_) => "On",
            P::Comp(p) => p.label(),
            P::Eq { param, .. } => param.label(),
            P::BusComp(p) => p.label(),
            P::ToneEq(b) => b.label(),
        }
    }

    /// レーン名 / gesture 名に出すラベル (device 名を除いた部分)。
    #[must_use]
    pub fn lane_label(self) -> &'static str {
        match self {
            P::Eq { band, param } => eq_lane_label(band, param),
            P::On(_) | P::Comp(_) | P::BusComp(_) | P::ToneEq(_) => self.knob_label(),
        }
    }

    /// その種類の全住所。`On` を先頭に、Par の列順。
    #[must_use]
    pub fn all_of(kind: NativeKind) -> &'static [NativeParamId] {
        match kind {
            NativeKind::Comp => &COMP_ALL,
            NativeKind::Eq => &EQ_ALL,
            NativeKind::BusComp => &BUS_COMP_ALL,
            NativeKind::ToneEq => &TONE_EQ_ALL,
        }
    }

    /// 既定値 (plain)。組み込みも picker 追加も同じ。`On` は `None` (既定は device ごとに違う)。
    #[must_use]
    pub fn default_plain(self) -> Option<f32> {
        match self {
            P::On(_) => None,
            P::Comp(p) => Some(CompSettings::default().param(p)),
            P::Eq { band, param } => Some(EqSettings::default().param(band, param)),
            P::BusComp(p) => Some(BusCompSettings::default().param(p)),
            P::ToneEq(b) => Some(ToneEqSettings::default().gain_db(b)),
        }
    }

    /// 段階式の表示ラベル (Bus Comp の Ratio / Attack / Release だけ `Some`)。
    #[must_use]
    pub fn step_labels(self) -> Option<&'static [&'static str]> {
        match self {
            P::BusComp(p) => p.step_labels(),
            P::On(_) | P::Comp(_) | P::Eq { .. } | P::ToneEq(_) => None,
        }
    }

    /// EQ のバンドを指す住所ならそのバンド。
    #[must_use]
    pub fn eq_band(self) -> Option<EqBand> {
        match self {
            P::Eq { band, .. } => Some(band),
            P::On(_) | P::Comp(_) | P::BusComp(_) | P::ToneEq(_) => None,
        }
    }
}

fn eq_lane_label(band: EqBand, param: EqParam) -> &'static str {
    match (band, param) {
        (EqBand::Hp, EqParam::Freq) => "HP Freq",
        (EqBand::Hp, EqParam::Gain) => "HP Gain",
        (EqBand::Hp, EqParam::Q) => "HP Q",
        (EqBand::Lp, EqParam::Freq) => "LP Freq",
        (EqBand::Lp, EqParam::Gain) => "LP Gain",
        (EqBand::Lp, EqParam::Q) => "LP Q",
        (EqBand::Lf, EqParam::Freq) => "LF Freq",
        (EqBand::Lf, EqParam::Gain) => "LF Gain",
        (EqBand::Lf, EqParam::Q) => "LF Q",
        (EqBand::Lmf, EqParam::Freq) => "LMF Freq",
        (EqBand::Lmf, EqParam::Gain) => "LMF Gain",
        (EqBand::Lmf, EqParam::Q) => "LMF Q",
        (EqBand::Hmf, EqParam::Freq) => "HMF Freq",
        (EqBand::Hmf, EqParam::Gain) => "HMF Gain",
        (EqBand::Hmf, EqParam::Q) => "HMF Q",
        (EqBand::Hf, EqParam::Freq) => "HF Freq",
        (EqBand::Hf, EqParam::Gain) => "HF Gain",
        (EqBand::Hf, EqParam::Q) => "HF Q",
    }
}

/// レーン名 / gesture 名の SSoT: `"{device_name}: {lane_label}"` (plugin の
/// `"{plugin}: {param}"` と同じ形)。例: `"Comp 2: Thr"`。
#[must_use]
pub fn native_param_label(device_name: &str, p: NativeParamId) -> String {
    format!("{device_name}: {}", p.lane_label())
}
