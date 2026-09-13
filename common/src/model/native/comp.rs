//! 内蔵 Comp (チャンネルコンプ) の値。音の作り方は `crate::dsp` / daw_audio が持つ。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::super::ParamRange;

/// コンプの連続パラメータ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum CompParam {
    Threshold,
    Ratio,
    Attack,
    Release,
    /// メイクアップゲイン。
    Makeup,
    /// 検出フィルタの中心周波数 (`0.0` = OFF = フルレンジ検出)。
    ScFreq,
}

impl CompParam {
    #[must_use]
    pub fn range(self) -> ParamRange {
        match self {
            Self::Threshold => ParamRange::Linear { lo: -60.0, hi: 0.0 },
            Self::Ratio => ParamRange::Log { lo: 1.0, hi: 20.0 },
            Self::Attack => ParamRange::Log { lo: 0.1, hi: 100.0 },
            Self::Release => ParamRange::Log { lo: 10.0, hi: 2_000.0 },
            Self::Makeup => ParamRange::Linear { lo: 0.0, hi: 20.0 },
            Self::ScFreq => ParamRange::LogWithOff { lo: 20.0, hi: 16_000.0 },
        }
    }

    /// つまみに出す短いラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Threshold => "Thr",
            Self::Ratio => "Rat",
            Self::Attack => "Atk",
            Self::Release => "Rel",
            Self::Makeup => "Gain",
            Self::ScFreq => "SC",
        }
    }
}

/// コンプの動作モード (Mixbus の 3 択と同じ)。
///
/// モードは一部のノブを**上書き**する — 上書きされた値は音に効かないが、
/// モードを戻せばノブの値がそのまま復帰する (値を破壊しない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum CompMode {
    /// 低レシオ (2:1) + 速いリリース固定。アタックのみ可変。
    Leveler,
    /// 全パラメータ可変。
    #[default]
    Compressor,
    /// アタック 0.1ms 固定 + レシオ 20:1 下限。
    Limiter,
}

impl CompMode {
    pub const ALL: [Self; 3] = [Self::Leveler, Self::Compressor, Self::Limiter];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Leveler => "LEV",
            Self::Compressor => "CMP",
            Self::Limiter => "LIM",
        }
    }

    /// このモードが `param` のノブ値を上書きするか (= ノブを淡色にする)。
    #[must_use]
    pub fn overrides(self, param: CompParam) -> bool {
        match self {
            Self::Leveler => matches!(param, CompParam::Ratio | CompParam::Release),
            Self::Compressor => false,
            Self::Limiter => matches!(param, CompParam::Attack),
        }
    }
}

/// Leveler モードの固定レシオ。
pub const LEVELER_RATIO: f32 = 2.0;
/// Leveler モードの固定リリース (ms)。
pub const LEVELER_RELEASE_MS: f32 = 100.0;
/// Limiter モードの固定アタック (ms)。
pub const LIMITER_ATTACK_MS: f32 = 0.1;
/// Limiter モードのレシオ下限。
pub const LIMITER_MIN_RATIO: f32 = 20.0;
/// コンプのソフトニー幅 (dB)。Comp / Bus Comp 共通。
pub const COMP_KNEE_DB: f32 = 6.0;

/// Comp の値。ON/OFF は [`super::NativeDevice::bypassed`] が唯一の SSoT なので持たない。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(default)]
pub struct CompSettings {
    pub mode: CompMode,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
    /// 検出フィルタの中心周波数 (`0.0` = OFF = フルレンジ検出)。
    /// Q は周波数から導出する ([`crate::dsp::sc_filter_q`])。
    pub sc_freq_hz: f32,
}

impl Default for CompSettings {
    fn default() -> Self {
        Self {
            mode: CompMode::Compressor,
            threshold_db: 0.0,
            ratio: 2.0,
            attack_ms: 10.0,
            release_ms: 200.0,
            makeup_db: 0.0,
            sc_freq_hz: 0.0,
        }
    }
}

impl CompSettings {
    /// ノブの値を読む (モードの上書きは適用しない = ノブが指す値)。
    #[must_use]
    pub fn param(&self, param: CompParam) -> f32 {
        match param {
            CompParam::Threshold => self.threshold_db,
            CompParam::Ratio => self.ratio,
            CompParam::Attack => self.attack_ms,
            CompParam::Release => self.release_ms,
            CompParam::Makeup => self.makeup_db,
            CompParam::ScFreq => self.sc_freq_hz,
        }
    }

    pub(super) fn param_mut(&mut self, param: CompParam) -> &mut f32 {
        match param {
            CompParam::Threshold => &mut self.threshold_db,
            CompParam::Ratio => &mut self.ratio,
            CompParam::Attack => &mut self.attack_ms,
            CompParam::Release => &mut self.release_ms,
            CompParam::Makeup => &mut self.makeup_db,
            CompParam::ScFreq => &mut self.sc_freq_hz,
        }
    }

    /// モードの上書きを適用した **実効** レシオ / アタック / リリース。
    /// DSP はここだけを読む (上書き規則を 2 か所に書かない)。
    #[must_use]
    pub fn effective(&self) -> (f32, f32, f32) {
        match self.mode {
            CompMode::Leveler => (LEVELER_RATIO, self.attack_ms, LEVELER_RELEASE_MS),
            CompMode::Compressor => (self.ratio, self.attack_ms, self.release_ms),
            CompMode::Limiter => {
                (self.ratio.max(LIMITER_MIN_RATIO), LIMITER_ATTACK_MS, self.release_ms)
            }
        }
    }

    /// フィールド単位の値域回復 (非有限は既定値、有限は clamp)。冪等。
    pub fn sanitize(&mut self) {
        let d = Self::default();
        for p in [
            CompParam::Threshold,
            CompParam::Ratio,
            CompParam::Attack,
            CompParam::Release,
            CompParam::Makeup,
            CompParam::ScFreq,
        ] {
            let v = self.param_mut(p);
            *v = if v.is_finite() { p.range().clamp(*v) } else { d.param(p) };
        }
    }
}
