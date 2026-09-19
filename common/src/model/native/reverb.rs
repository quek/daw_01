//! 内蔵 Reverb (Dattorro プレート) の値。音の作り方は daw_audio の `native_dsp::reverb` が持つ。
//!
//! 方式は Jon Dattorro, "Effect Design, Part 1: Reverberator and Other Filters",
//! *J. Audio Eng. Soc.* 45(9), 1997, pp.660-684 (Fig.1 / Table 1 / Table 2)。
//! 遅延長・タップ・係数がすべて論文に数値で載っている唯一の方式なので、推測を挟まずに書ける。
//! 設計の正本は `docs/plan_rmd_134_135_reverb_delay.md` §4 / §5。
//!
//! ON/OFF の唯一の SSoT は [`super::NativeDevice::bypassed`] なのでここには持たない。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::super::ParamRange;

/// リバーブのパラメーター住所。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum ReverbParam {
    /// 初期遅延 (`0.0` = OFF)。
    Predelay,
    /// 部屋の大きさ = 全ディレイ長の倍率 (%)。
    Size,
    /// T60 (秒)。`decay` 係数はここから導出する。
    Decay,
    /// タンク内の高域減衰 (%)。
    Damp,
    /// タンク内の低域減衰 (%)。
    LfDamp,
    /// 入力ディフューザの拡散量 (%)。
    Diffusion,
    /// 入力ハイパス (`0.0` = OFF)。
    LowCut,
    /// 入力ローパス (= 論文の `bandwidth`)。
    HighCut,
    /// タンク前段の変調 APF を揺らす速さ。
    ModRate,
    /// 同、揺れ幅 (%)。
    ModDepth,
    /// 出力の M/S 幅 (%、`0` = mono)。
    Width,
    /// dry / wet (%)。
    Mix,
    /// 入力を遮断して減衰を止める (ON/OFF)。
    Freeze,
}

impl ReverbParam {
    #[must_use]
    pub fn range(self) -> ParamRange {
        match self {
            Self::Predelay => ParamRange::LogWithOff { lo: 1.0, hi: 250.0 },
            Self::Size => ParamRange::Log { lo: 25.0, hi: 200.0 },
            Self::Decay => ParamRange::Log { lo: 0.1, hi: 20.0 },
            Self::Damp | Self::LfDamp | Self::Diffusion | Self::ModDepth | Self::Width | Self::Mix => {
                ParamRange::Linear { lo: 0.0, hi: 100.0 }
            }
            Self::LowCut => ParamRange::LogWithOff { lo: 20.0, hi: 1_000.0 },
            Self::HighCut => ParamRange::Log { lo: 1_000.0, hi: 20_000.0 },
            Self::ModRate => ParamRange::Log { lo: 0.1, hi: 5.0 },
            Self::Freeze => ParamRange::Toggle,
        }
    }

    /// つまみの下に出す短いラベル。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Predelay => "Pre",
            Self::Size => "Size",
            Self::Decay => "Dec",
            Self::Damp => "Damp",
            Self::LfDamp => "LF",
            Self::Diffusion => "Diff",
            Self::LowCut => "LoCut",
            Self::HighCut => "HiCut",
            Self::ModRate => "Rate",
            Self::ModDepth => "Dep",
            Self::Width => "Wid",
            Self::Mix => "Mix",
            Self::Freeze => "Frz",
        }
    }
}

/// タンク一周のサンプル数 (論文 Fig.1 の 8 本の合計、基準 29761 Hz)。
/// `672+4453+1800+3720 + 908+4217+2656+3163`。
pub const REVERB_TANK_LOOP_SAMPLES: f32 = 21_589.0;
/// 論文の基準サンプルレート (Table 1)。
pub const REVERB_BASE_SR: f32 = 29_761.0;
/// `Size` の上限 (%)。遅延バッファはこの倍率で確保する (Size を動かしても再確保しない)。
pub const REVERB_MAX_SIZE_PCT: f32 = 200.0;
/// `Predelay` の上限 (ms)。
pub const REVERB_MAX_PREDELAY_MS: f32 = 250.0;
/// 入力ディフューザの係数 (Table 1 の input diffusion 1 / 2)。`Diffusion` で按分する。
pub const REVERB_INPUT_DIFFUSION: (f32, f32) = (0.750, 0.625);
/// タンクの decay diffusion 1 (Fig.1 の "note sign" により APF では符号を反転して使う)。
pub const REVERB_DECAY_DIFFUSION_1: f32 = 0.70;
/// 出力タップの共通係数 (Table 2)。
pub const REVERB_OUTPUT_GAIN: f32 = 0.6;

/// Reverb の値。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(default)]
pub struct ReverbSettings {
    /// 0.0 = OFF。
    pub predelay_ms: f32,
    pub size_pct: f32,
    pub decay_s: f32,
    pub damp_pct: f32,
    pub lf_damp_pct: f32,
    pub diffusion_pct: f32,
    /// 0.0 = OFF。
    pub low_cut_hz: f32,
    pub high_cut_hz: f32,
    pub mod_rate_hz: f32,
    pub mod_depth_pct: f32,
    pub width_pct: f32,
    pub mix_pct: f32,
    pub freeze: bool,
}

impl Default for ReverbSettings {
    fn default() -> Self {
        Self {
            predelay_ms: 10.0,
            size_pct: 100.0,
            decay_s: 1.8,
            damp_pct: 20.0,
            lf_damp_pct: 20.0,
            diffusion_pct: 100.0,
            low_cut_hz: 80.0,
            high_cut_hz: 12_000.0,
            mod_rate_hz: 1.0,
            mod_depth_pct: 50.0,
            width_pct: 100.0,
            mix_pct: 25.0,
            freeze: false,
        }
    }
}

impl ReverbSettings {
    /// 連続値のフィールド (`sanitize` が回す対象。`Freeze` は bool なので含まない)。
    const CONTINUOUS: [ReverbParam; 12] = [
        ReverbParam::Predelay,
        ReverbParam::Size,
        ReverbParam::Decay,
        ReverbParam::Damp,
        ReverbParam::LfDamp,
        ReverbParam::Diffusion,
        ReverbParam::LowCut,
        ReverbParam::HighCut,
        ReverbParam::ModRate,
        ReverbParam::ModDepth,
        ReverbParam::Width,
        ReverbParam::Mix,
    ];

    /// 住所 → plain 値 (`Freeze` は 1.0 / 0.0)。
    #[must_use]
    pub fn param(&self, p: ReverbParam) -> f32 {
        match p {
            ReverbParam::Predelay => self.predelay_ms,
            ReverbParam::Size => self.size_pct,
            ReverbParam::Decay => self.decay_s,
            ReverbParam::Damp => self.damp_pct,
            ReverbParam::LfDamp => self.lf_damp_pct,
            ReverbParam::Diffusion => self.diffusion_pct,
            ReverbParam::LowCut => self.low_cut_hz,
            ReverbParam::HighCut => self.high_cut_hz,
            ReverbParam::ModRate => self.mod_rate_hz,
            ReverbParam::ModDepth => self.mod_depth_pct,
            ReverbParam::Width => self.width_pct,
            ReverbParam::Mix => self.mix_pct,
            ReverbParam::Freeze => f32::from(u8::from(self.freeze)),
        }
    }

    /// 住所へ書く (`v` は値域へクランプ済み・有限であること)。戻り値 = 実際に変わったか。
    pub(super) fn write(&mut self, p: ReverbParam, v: f32) -> bool {
        let before = *self;
        match p {
            ReverbParam::Predelay => self.predelay_ms = v,
            ReverbParam::Size => self.size_pct = v,
            ReverbParam::Decay => self.decay_s = v,
            ReverbParam::Damp => self.damp_pct = v,
            ReverbParam::LfDamp => self.lf_damp_pct = v,
            ReverbParam::Diffusion => self.diffusion_pct = v,
            ReverbParam::LowCut => self.low_cut_hz = v,
            ReverbParam::HighCut => self.high_cut_hz = v,
            ReverbParam::ModRate => self.mod_rate_hz = v,
            ReverbParam::ModDepth => self.mod_depth_pct = v,
            ReverbParam::Width => self.width_pct = v,
            ReverbParam::Mix => self.mix_pct = v,
            ReverbParam::Freeze => self.freeze = v >= 0.5,
        }
        *self != before
    }

    /// フィールド単位の値域回復 (非有限は既定値、有限は clamp)。冪等。
    pub fn sanitize(&mut self) {
        let d = Self::default();
        for p in Self::CONTINUOUS {
            let v = self.param(p);
            self.write(p, if v.is_finite() { p.range().clamp(v) } else { d.param(p) });
        }
    }

    /// タンク一周の秒数 (Size 倍率込み)。[`Self::decay_coeff`] と DSP の両方が読む。
    #[must_use]
    pub fn loop_secs(&self) -> f32 {
        REVERB_TANK_LOOP_SAMPLES / REVERB_BASE_SR * (self.size_pct / 100.0)
    }

    /// T60 (秒) → タンクの `decay` 係数。タンク一周に `×decay` が 4 回かかるので
    /// `decay = 0.001^(loop / (4 * t60))`。`Freeze` 中は 1.0 (減衰しない)。
    ///
    /// 検算: 既定サイズで `decay = 0.5` → T60 ≈ 1.807 s (論文 Table 1 の既定と整合)。
    #[must_use]
    pub fn decay_coeff(&self) -> f32 {
        if self.freeze {
            return 1.0;
        }
        let t60 = self.decay_s.max(0.01);
        0.001_f32.powf(self.loop_secs() / (4.0 * t60)).clamp(0.0, 0.9999)
    }

    /// タンクの decay diffusion 2 (Table 1: `decay + 0.15`、floor 0.25 / ceiling 0.50)。
    /// **独立したノブにしない** — 論文が導出式を与えているので SSoT は `decay` 1 本。
    #[must_use]
    pub fn decay_diffusion_2(&self) -> f32 {
        (self.decay_coeff() + 0.15).clamp(0.25, 0.50)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 論文 Table 1 の既定 `decay = 0.5` と T60 の対応 (§5.5 の検算)。
    #[test]
    fn decay_係数は_t60_から論文の式で導かれる() {
        let mut s = ReverbSettings { size_pct: 100.0, ..ReverbSettings::default() };
        // loop = 21589 / 29761 = 0.725412 s、decay 0.5 になる T60 は 1.807 s。
        s.decay_s = 1.807;
        assert!((s.decay_coeff() - 0.5).abs() < 1e-3, "got {}", s.decay_coeff());
        // 長い T60 ほど 1 に近づき、短いほど 0 に近づく (単調)。
        s.decay_s = 20.0;
        let long = s.decay_coeff();
        s.decay_s = 0.1;
        let short = s.decay_coeff();
        assert!(short < 0.5 && 0.5 < long && long < 1.0, "short={short} long={long}");
        // Freeze 中は減衰しない。
        s.freeze = true;
        assert_eq!(s.decay_coeff(), 1.0);
    }

    #[test]
    fn sanitize_は非有限を既定へ戻し値域へ収める() {
        let mut s = ReverbSettings { decay_s: f32::NAN, size_pct: 1e9, mix_pct: -5.0, ..ReverbSettings::default() };
        s.sanitize();
        assert_eq!(s.decay_s, ReverbSettings::default().decay_s);
        assert_eq!(s.size_pct, REVERB_MAX_SIZE_PCT);
        assert_eq!(s.mix_pct, 0.0);
        let again = { let mut t = s; t.sanitize(); t };
        assert_eq!(s, again, "冪等");
    }
}
