//! master のフェーダー後に固定で掛かる Limiter (`Song::master_limiter`)。
//!
//! チェーン上の device ではない (動かせず消せない、Q3)。信号順は
//! `合算 → master_fx_chain (組み込み Bus Comp / Tone EQ を含む) → master_gain → Limiter`。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::ParamRange;

/// Limiter のパラメーター住所 (オートメーション / 変調 / MIDI Learn の的)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum MasterLimiterParam {
    On,
    Ceiling,
}

impl MasterLimiterParam {
    #[must_use]
    pub fn range(self) -> ParamRange {
        match self {
            Self::On => ParamRange::Toggle,
            Self::Ceiling => ParamRange::Linear { lo: -6.0, hi: 0.0 },
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::On => "On",
            Self::Ceiling => "Ceiling",
        }
    }

    #[must_use]
    pub fn default_plain(self) -> f32 {
        MasterLimiterSettings::default().param(self)
    }
}

/// master Limiter の値。操作子はシーリング 1 つ + ON/OFF。
///
/// リリースは信号追従の自動、ルックアヘッドは [`MASTER_LIMITER_LOOKAHEAD_MS`] 固定、
/// アタックは実質 0 (先読みで落とすため)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(default)]
pub struct MasterLimiterSettings {
    pub on: bool,
    /// -6〜0 dBFS。既定 -1.0 (配信の規定値 -1.0 dBTP に合わせやすい位置)。
    pub ceiling_db: f32,
}

impl Default for MasterLimiterSettings {
    fn default() -> Self {
        Self { on: false, ceiling_db: -1.0 }
    }
}

impl MasterLimiterSettings {
    /// 住所 → plain 値 (`On` は 0 / 1)。
    #[must_use]
    pub fn param(&self, p: MasterLimiterParam) -> f32 {
        match p {
            MasterLimiterParam::On => f32::from(u8::from(self.on)),
            MasterLimiterParam::Ceiling => self.ceiling_db,
        }
    }

    /// 住所へ書く。非有限は書かずに false、`On` は 0.5 閾値。戻り値 = 実際に変わったか。
    pub fn set_param(&mut self, p: MasterLimiterParam, v: f32) -> bool {
        if !v.is_finite() {
            return false;
        }
        let before = *self;
        match p {
            MasterLimiterParam::On => self.on = v >= 0.5,
            MasterLimiterParam::Ceiling => self.ceiling_db = p.range().clamp(v),
        }
        *self != before
    }

    /// 非有限の ceiling は既定値、有限は clamp。冪等。
    pub fn sanitize(&mut self) {
        let p = MasterLimiterParam::Ceiling;
        self.ceiling_db =
            if self.ceiling_db.is_finite() { p.range().clamp(self.ceiling_db) } else { p.default_plain() };
    }
}

/// リミッターのルックアヘッド (ms)。遅延は「静的 ON または On レーン / 変調がある」とき
/// compile 時に焼かれる (`Song::master_limiter_latency_active`)。
pub const MASTER_LIMITER_LOOKAHEAD_MS: f32 = 5.0;

/// [`MASTER_LIMITER_LOOKAHEAD_MS`] をサンプル数に直す **唯一の式**。
///
/// DSP (遅延線の長さ) と PDC 会計 (`Schedule::master_latency_samples` = 書き出しの
/// 窓ずらし / クリックの前出し) の両方がこれを引く。片方が丸め方を変えると、
/// 書き出しが 1 サンプル欠けるか、クリックが 1 サンプルずれる。
#[must_use]
pub fn limiter_lookahead_samples(sample_rate: u32) -> u32 {
    // 5ms = 1/200 秒。整数演算で切り捨て (44.1k → 220、48k → 240、96k → 480)。
    sample_rate / 200
}

/// リミッターのリリース時定数 (ms)。ルックアヘッドで先に落とすのでアタックは 0。
pub const MASTER_LIMITER_RELEASE_MS: f32 = 50.0;
