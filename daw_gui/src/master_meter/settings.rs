//! マスターパネルのメーター設定 (r.md #50)。
//!
//! 各メーターを右クリックして出るメニューから変更し、`app_config.json` に
//! 永続化する (プロジェクト非依存 = 「この人の画面の使い方」なので `ViewState`
//! ではなくアプリ設定側)。既定値の根拠はすべて `docs/plan_master_meters.md` §2。

use serde::{Deserialize, Serialize};

/// スペクトラムの FFT 長。ビン幅 `fs/N`、解析レイテンシ `N/fs`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpectrumFft {
    N1024,
    N2048,
    N4096,
    N8192,
}

impl SpectrumFft {
    pub const ALL: [Self; 4] = [Self::N1024, Self::N2048, Self::N4096, Self::N8192];

    pub fn size(self) -> usize {
        match self {
            Self::N1024 => 1024,
            Self::N2048 => 2048,
            Self::N4096 => 4096,
            Self::N8192 => 8192,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::N1024 => "1024",
            Self::N2048 => "2048",
            Self::N4096 => "4096",
            Self::N8192 => "8192",
        }
    }
}

/// スペクトラムの窓関数。Hann = 音楽解析の既定 (Voxengo SPAN)、
/// Blackman-Harris(92dB) = 微小成分の分離用 (REAPER の既定)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpectrumWindow {
    Hann,
    BlackmanHarris,
}

impl SpectrumWindow {
    pub const ALL: [Self; 2] = [Self::Hann, Self::BlackmanHarris];

    pub fn label(self) -> &'static str {
        match self {
            Self::Hann => "Hann",
            Self::BlackmanHarris => "Blackman-Harris",
        }
    }

    /// 窓係数 `w[j]` (`j = 0..n`)。
    pub fn coefficient(self, j: usize, n: usize) -> f32 {
        let z = std::f64::consts::TAU * j as f64 / n as f64;
        let v = match self {
            Self::Hann => (1.0 - z.cos()) * 0.5,
            // BH92 (Heinzel/Rüdiger/Schilling C.6)
            Self::BlackmanHarris => {
                0.35875 - 0.48829 * z.cos() + 0.14128 * (2.0 * z).cos()
                    - 0.01168 * (3.0 * z).cos()
            }
        };
        v as f32
    }
}

/// オシロスコープのトリガ方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeTrigger {
    /// Mid (L+R) の立ち上がりゼロクロス。
    RisingZero,
    /// Mid の立ち下がりゼロクロス。
    FallingZero,
    /// トリガ無し (最新の窓をそのまま表示 = 波形が横に流れる)。
    Free,
}

impl ScopeTrigger {
    pub const ALL: [Self; 3] = [Self::RisingZero, Self::FallingZero, Self::Free];

    pub fn label(self) -> &'static str {
        match self {
            Self::RisingZero => "立ち上がり",
            Self::FallingZero => "立ち下がり",
            Self::Free => "トリガ無し",
        }
    }
}

/// ラウドネスメーターの目盛り (EBU Tech 3341 §2.7)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoudnessScale {
    /// EBU +9: -18.0 .. +9.0 LU
    Ebu9,
    /// EBU +18: -36.0 .. +18.0 LU
    Ebu18,
}

impl LoudnessScale {
    pub const ALL: [Self; 2] = [Self::Ebu9, Self::Ebu18];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ebu9 => "EBU +9",
            Self::Ebu18 => "EBU +18",
        }
    }

    /// 目標値を 0 LU としたときの表示範囲 (下端, 上端) [LU]。
    pub fn range_lu(self) -> (f32, f32) {
        match self {
            Self::Ebu9 => (-18.0, 9.0),
            Self::Ebu18 => (-36.0, 18.0),
        }
    }
}

/// ラウドネス数値の単位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoudnessUnits {
    /// 絶対値 (LUFS)。
    Lufs,
    /// 目標値を 0 とした相対値 (LU)。
    Lu,
}

impl LoudnessUnits {
    pub const ALL: [Self; 2] = [Self::Lufs, Self::Lu];

    pub fn label(self) -> &'static str {
        match self {
            Self::Lufs => "LUFS (絶対)",
            Self::Lu => "LU (相対)",
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Self::Lufs => "LUFS",
            Self::Lu => "LU",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeterSettings {
    // ---- レベル (VU / ピーク) ----
    /// 0 VU が指す dBFS。-18 = EBU R68、-20 = SMPTE RP155。
    pub vu_reference_dbfs: f32,
    /// ピークバーの落下速度 [dB/s]。13.3 は x42 / Ardour の de-facto。
    pub peak_fall_db_per_s: f32,
    /// ピーク保持線の保持時間 [ms]。
    pub peak_hold_ms: u32,
    // ---- スペクトラム ----
    pub spectrum_fft: SpectrumFft,
    pub spectrum_window: SpectrumWindow,
    /// 1kHz 支点の傾き補正 [dB/oct]。4.5 は Voxengo / FabFilter の既定。
    pub spectrum_slope_db_oct: f32,
    /// 表示レンジ [dB] (0 dBFS からの深さ)。
    pub spectrum_range_db: f32,
    /// 20 dB 落ちるのに要する時間 [ms] (Voxengo の Avg Time と同義)。
    pub spectrum_release_ms: u32,
    /// ピーク保持線を出すか。
    pub spectrum_peak_hold: bool,
    // ---- オシロスコープ ----
    /// 表示窓の全幅 [ms]。
    pub scope_window_ms: f32,
    pub scope_trigger: ScopeTrigger,
    // ---- ゴニオ / 位相 ----
    /// 残光の 1 フレームあたり減衰率 (0.5..0.99)。大きいほど尾を引く。
    pub gonio_persistence: f32,
    // ---- ラウドネス ----
    /// 目標ラウドネス [LUFS]。-14 = 配信、-23 = EBU R128 放送。
    pub loudness_target_lufs: f32,
    pub loudness_scale: LoudnessScale,
    pub loudness_units: LoudnessUnits,
}

impl Default for MeterSettings {
    fn default() -> Self {
        Self {
            vu_reference_dbfs: -18.0,
            peak_fall_db_per_s: 13.3,
            peak_hold_ms: 1500,
            spectrum_fft: SpectrumFft::N4096,
            spectrum_window: SpectrumWindow::Hann,
            spectrum_slope_db_oct: 4.5,
            spectrum_range_db: 100.0,
            spectrum_release_ms: 600,
            spectrum_peak_hold: true,
            scope_window_ms: 20.0,
            scope_trigger: ScopeTrigger::RisingZero,
            gonio_persistence: 0.90,
            loudness_target_lufs: -14.0,
            loudness_scale: LoudnessScale::Ebu9,
            loudness_units: LoudnessUnits::Lufs,
        }
    }
}

/// UI スレッド (設定変更・リセット操作) からテレメトリスレッドの
/// [`super::MasterAnalyzer`] へ渡る唯一の口。ロックは 30Hz で 1 回だけ。
#[derive(Debug, Default)]
pub struct MeterControl {
    pub settings: MeterSettings,
    /// ラウドネス積算のリセット世代。UI 側が増やすと解析器が気付いて
    /// I / LRA / 最大 M / 最大 S / 最大 TP を同時リセットする
    /// (EBU Tech 3341 §2.2 が「同時にリセットできること」を要求)。
    pub loudness_reset_epoch: u64,
    /// ピーク保持 (バーの保持線 + 上端の数値) とクリップ表示のリセット世代。
    /// メーターのクリックはこちらだけを畳む — 実 DAW でも「ピーク数値の
    /// クリック」と「ラウドネスの Reset」は別操作。
    pub peak_reset_epoch: u64,
    /// パネルを描画しているか。閉じているときは解析自体を回さない。
    pub active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 公表係数 (a0-a1+a2-a3) との突き合わせ。係数のタイプミスを捕まえる
    /// (Hann と違い、スペクトラムの校正テストは既定の Hann しか通らないため
    /// BH92 の誤りはここでしか落ちない)。
    #[test]
    fn blackman_harris_matches_the_published_coefficient_sum() {
        // w[0] = a0 - a1 + a2 - a3 = 0.35875-0.48829+0.14128-0.01168 = 0.00006
        let v = SpectrumWindow::BlackmanHarris.coefficient(0, 16);
        assert!((v - 0.00006).abs() < 1e-6, "got {v}");
    }
}
