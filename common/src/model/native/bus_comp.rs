//! 内蔵 Bus Comp (SSL / Reason のバスコンプ流、5 操作子) の値。
//!
//! Ratio / Attack / Release が **段階式**なのは意図的 — 選択肢が少ないぶん速く決まる。
//! オートメーション / 変調では **段の index** を plain 値として載せる
//! ([`super::super::ParamRange::Stepped`])。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::COMP_KNEE_DB;
use super::super::ParamRange;

/// バスコンプのレシオ (3 択)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum BusCompRatio {
    #[default]
    R2,
    R4,
    R10,
}

impl BusCompRatio {
    pub const ALL: [Self; 3] = [Self::R2, Self::R4, Self::R10];
    /// [`Self::ALL`] と同順の表示ラベル (段階式の値表示の SSoT)。
    pub const LABELS: [&'static str; 3] = ["2:1", "4:1", "10:1"];

    #[must_use]
    pub fn value(self) -> f32 {
        match self {
            Self::R2 => 2.0,
            Self::R4 => 4.0,
            Self::R10 => 10.0,
        }
    }
}

/// バスコンプのアタック (6 段、ms)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum BusCompAttack {
    A01,
    A03,
    A1,
    #[default]
    A3,
    A10,
    A30,
}

impl BusCompAttack {
    pub const ALL: [Self; 6] = [Self::A01, Self::A03, Self::A1, Self::A3, Self::A10, Self::A30];
    /// [`Self::ALL`] と同順の表示ラベル。
    pub const LABELS: [&'static str; 6] = ["0.1ms", "0.3ms", "1ms", "3ms", "10ms", "30ms"];

    #[must_use]
    pub fn ms(self) -> f32 {
        match self {
            Self::A01 => 0.1,
            Self::A03 => 0.3,
            Self::A1 => 1.0,
            Self::A3 => 3.0,
            Self::A10 => 10.0,
            Self::A30 => 30.0,
        }
    }
}

/// バスコンプのリリース (4 段 + Auto)。
///
/// `Auto` は program-adaptive — 長いピークの後は遅く、短いピークの後は速く戻る
/// (Reason の Master Bus Compressor と同じ挙動)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum BusCompRelease {
    R100,
    #[default]
    R300,
    R600,
    R1200,
    Auto,
}

impl BusCompRelease {
    pub const ALL: [Self; 5] = [Self::R100, Self::R300, Self::R600, Self::R1200, Self::Auto];
    /// [`Self::ALL`] と同順の表示ラベル。
    pub const LABELS: [&'static str; 5] = ["0.1s", "0.3s", "0.6s", "1.2s", "Auto"];

    /// 固定段の時定数 (ms)。`Auto` は信号追従なので `None`。
    #[must_use]
    pub fn ms(self) -> Option<f32> {
        match self {
            Self::R100 => Some(100.0),
            Self::R300 => Some(300.0),
            Self::R600 => Some(600.0),
            Self::R1200 => Some(1_200.0),
            Self::Auto => None,
        }
    }
}

/// `Auto` リリースが動く範囲 (ms)。短いピークでは下端、長いピークでは上端へ寄る。
pub const BUS_COMP_AUTO_RELEASE_MIN_MS: f32 = 80.0;
/// [`BUS_COMP_AUTO_RELEASE_MIN_MS`] の上端。
pub const BUS_COMP_AUTO_RELEASE_MAX_MS: f32 = 1_500.0;
/// `Auto` リリースが「どれだけ長く潰れ続けたか」を測る時定数 (ms)。
/// この平均が深いほどリリースが遅くなる。
pub const BUS_COMP_AUTO_RELEASE_TRACK_MS: f32 = 2_000.0;

/// バスコンプのパラメーター住所。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum BusCompParam {
    Threshold,
    Ratio,
    Attack,
    Release,
    Makeup,
}

impl BusCompParam {
    /// 可動範囲。段階式は `0..=段数-1` の index ドメイン。
    #[must_use]
    pub fn range(self) -> ParamRange {
        match self {
            Self::Threshold => ParamRange::Linear { lo: -30.0, hi: 0.0 },
            Self::Ratio => ParamRange::Stepped { count: BusCompRatio::ALL.len() as u8 },
            Self::Attack => ParamRange::Stepped { count: BusCompAttack::ALL.len() as u8 },
            Self::Release => ParamRange::Stepped { count: BusCompRelease::ALL.len() as u8 },
            Self::Makeup => ParamRange::Linear { lo: -5.0, hi: 15.0 },
        }
    }

    /// つまみ / レーンに出す短いラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Threshold => "Thr",
            Self::Ratio => "Ratio",
            Self::Attack => "Atk",
            Self::Release => "Rel",
            Self::Makeup => "Makeup",
        }
    }

    /// 段階式の表示ラベル (`Some` = 段階式)。
    #[must_use]
    pub fn step_labels(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Ratio => Some(&BusCompRatio::LABELS),
            Self::Attack => Some(&BusCompAttack::LABELS),
            Self::Release => Some(&BusCompRelease::LABELS),
            Self::Threshold | Self::Makeup => None,
        }
    }
}

/// Bus Comp の値。ON/OFF は [`super::NativeDevice::bypassed`] が唯一の SSoT。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(default)]
pub struct BusCompSettings {
    /// -30〜0 dB。
    pub threshold_db: f32,
    pub ratio: BusCompRatio,
    pub attack: BusCompAttack,
    pub release: BusCompRelease,
    /// -5〜+15 dB。
    pub makeup_db: f32,
}

impl Default for BusCompSettings {
    fn default() -> Self {
        Self {
            threshold_db: 0.0,
            ratio: BusCompRatio::default(),
            attack: BusCompAttack::default(),
            release: BusCompRelease::default(),
            makeup_db: 0.0,
        }
    }
}

impl BusCompSettings {
    /// ソフトニーの幅 (dB)。Comp と同じ値を使う (音の作りを 2 種類にしない)。
    pub const KNEE_DB: f32 = COMP_KNEE_DB;

    /// 住所 → plain 値 (段階式は段の index)。
    #[must_use]
    pub fn param(&self, p: BusCompParam) -> f32 {
        match p {
            BusCompParam::Threshold => self.threshold_db,
            BusCompParam::Ratio => index_of(&BusCompRatio::ALL, self.ratio),
            BusCompParam::Attack => index_of(&BusCompAttack::ALL, self.attack),
            BusCompParam::Release => index_of(&BusCompRelease::ALL, self.release),
            BusCompParam::Makeup => self.makeup_db,
        }
    }

    /// 住所へ書く (`v` は値域へクランプ済み・有限であること)。段階式は最寄りの段へ丸める。
    /// 戻り値 = 実際に変わったか。
    pub(super) fn write(&mut self, p: BusCompParam, v: f32) -> bool {
        let before = *self;
        match p {
            BusCompParam::Threshold => self.threshold_db = v,
            BusCompParam::Ratio => self.ratio = nearest(&BusCompRatio::ALL, v),
            BusCompParam::Attack => self.attack = nearest(&BusCompAttack::ALL, v),
            BusCompParam::Release => self.release = nearest(&BusCompRelease::ALL, v),
            BusCompParam::Makeup => self.makeup_db = v,
        }
        *self != before
    }

    /// 連続フィールド (threshold / makeup) の値域回復。段は enum なので常に有効。冪等。
    pub fn sanitize(&mut self) {
        let d = Self::default();
        let fix = |v: f32, p: BusCompParam| if v.is_finite() { p.range().clamp(v) } else { d.param(p) };
        self.threshold_db = fix(self.threshold_db, BusCompParam::Threshold);
        self.makeup_db = fix(self.makeup_db, BusCompParam::Makeup);
    }
}

/// 段の並びの中での位置 (= plain 値)。
#[allow(clippy::cast_precision_loss)]
fn index_of<T: PartialEq + Copy>(all: &[T], v: T) -> f32 {
    all.iter().position(|x| *x == v).unwrap_or(0) as f32
}

/// plain 値 (index) を最寄りの段へ丸める。
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn nearest<T: Copy>(all: &[T], v: f32) -> T {
    let i = (v.round().max(0.0) as usize).min(all.len() - 1);
    all[i]
}
