//! パラメーター 1 個の可動範囲と plain ↔ 正規化 (0..=1) の写像。
//!
//! **ノブ / オートメーション / 変調 / IPC クランプが共有する唯一の定義**
//! (`common::automation::target_range` がここを引く)。境界は f64 で持ち、
//! plain 値の書き込み ([`ParamRange::clamp`]) だけ f32 で行う。
//!
//! wire には載らない (値だけが載る) ので `common/build.rs` の `WIRE_SOURCES` 対象外。

/// 1 パラメータの可動範囲と、plain (実単位) ↔ 正規化 (0..=1) の写像。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamRange {
    /// 線形 (dB ゲイン・スレッショルド等)。
    Linear { lo: f64, hi: f64 },
    /// 対数 (周波数・時定数・レシオ)。`lo > 0` が前提。
    Log { lo: f64, hi: f64 },
    /// 対数 + 左端の `OFF` (plain `0.0`)。検出フィルタの周波数専用。
    ///
    /// 正規化 `0.0..=OFF_SPAN` が OFF で、それより上は `lo..=hi` の対数目盛。
    /// ノブを左へ回し切ると必ず OFF に落ちる。
    LogWithOff { lo: f64, hi: f64 },
    /// ON/OFF (plain `0.0` / `1.0`、`0.5` 閾値)。
    Toggle,
    /// `count` 段の段階式 (plain = 段 index `0..count`)。正規化は連続で、段への丸めは
    /// 値を書く側 ([`ParamRange::clamp`]) が行う。
    Stepped { count: u8 },
}

impl ParamRange {
    /// [`ParamRange::LogWithOff`] で OFF に落ちる正規化値の上限。
    pub const OFF_SPAN: f64 = 0.02;

    /// plain → 正規化 (0..=1)。範囲外は端に丸める。
    #[must_use]
    pub fn to_norm(self, plain: f64) -> f64 {
        match self {
            Self::Linear { lo, hi } => ((plain - lo) / (hi - lo)).clamp(0.0, 1.0),
            Self::Log { lo, hi } => log_to_norm(plain, lo, hi),
            Self::LogWithOff { lo, hi } => {
                if plain < lo {
                    return 0.0;
                }
                Self::OFF_SPAN + log_to_norm(plain, lo, hi) * (1.0 - Self::OFF_SPAN)
            }
            Self::Toggle => toggle(plain),
            Self::Stepped { count } => {
                let top = f64::from(count.saturating_sub(1));
                if top <= 0.0 { 0.0 } else { (plain / top).clamp(0.0, 1.0) }
            }
        }
    }

    /// 正規化 (0..=1) → plain。範囲外は端に丸める。
    #[must_use]
    pub fn from_norm(self, norm: f64) -> f64 {
        let n = norm.clamp(0.0, 1.0);
        match self {
            Self::Linear { lo, hi } => lo + n * (hi - lo),
            Self::Log { lo, hi } => norm_to_log(n, lo, hi),
            Self::LogWithOff { lo, hi } => {
                if n <= Self::OFF_SPAN {
                    return 0.0;
                }
                norm_to_log((n - Self::OFF_SPAN) / (1.0 - Self::OFF_SPAN), lo, hi)
            }
            Self::Toggle => toggle(n),
            Self::Stepped { count } => n * f64::from(count.saturating_sub(1)),
        }
    }

    /// plain を可動範囲へ丸める (値を書く口のクランプ)。段階式は最寄りの段へ丸める。
    /// 非有限値の扱いは呼び出し側 (各 `sanitize` / `set`) が持つ。
    #[must_use]
    pub fn clamp(self, plain: f32) -> f32 {
        match self {
            Self::Linear { lo, hi } | Self::Log { lo, hi } => plain.clamp(lo as f32, hi as f32),
            Self::LogWithOff { lo, hi } => {
                if plain < lo as f32 {
                    0.0
                } else {
                    plain.min(hi as f32)
                }
            }
            Self::Toggle => toggle(f64::from(plain)) as f32,
            Self::Stepped { count } => plain.round().clamp(0.0, f32::from(count.saturating_sub(1))),
        }
    }

    /// plain↔正規化が **affine (直線)** か。オートメーション曲線を画面で直線として
    /// 描いてよいかの判定 (`common::automation::norm_mapping_is_affine`)。
    #[must_use]
    pub fn is_affine(self) -> bool {
        matches!(self, Self::Linear { .. })
    }

    /// plain↔正規化が **狭義単調 (= 逆写像を持つ)** か。平らな帯 (OFF / 段 / 閾値) を
    /// 持つものは false。
    #[must_use]
    pub fn is_invertible(self) -> bool {
        matches!(self, Self::Linear { .. } | Self::Log { .. })
    }

    /// 表示レンジ (数値欄の clamp 用)。`LogWithOff` は OFF (0) を下端に含む。
    #[must_use]
    pub fn display_range(self) -> (f64, f64) {
        match self {
            Self::Linear { lo, hi } | Self::Log { lo, hi } => (lo, hi),
            Self::LogWithOff { hi, .. } => (0.0, hi),
            Self::Toggle => (0.0, 1.0),
            Self::Stepped { count } => (0.0, f64::from(count.saturating_sub(1))),
        }
    }
}

fn toggle(v: f64) -> f64 {
    if v >= 0.5 { 1.0 } else { 0.0 }
}

fn log_to_norm(plain: f64, lo: f64, hi: f64) -> f64 {
    if plain <= lo {
        return 0.0;
    }
    ((plain / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}

fn norm_to_log(norm: f64, lo: f64, hi: f64) -> f64 {
    lo * (hi / lo).powf(norm.clamp(0.0, 1.0))
}
