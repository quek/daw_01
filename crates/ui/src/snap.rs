//! Beat 単位の grid snap 計算。
//!
//! piano_roll / arrangement widget の drag overlay と commit 値を grid に吸着させるため
//! の純データ型 + 純関数。widget 側は `view.snap.snap_beat_delta(...)` を 1 行呼ぶだけで
//! grid 吸着が効く。
//!
//! デフォルト = `Adaptive` ON (DAW UI の業界標準。Cubase / Live と一致)。
//! 「絶対 snap させたくない」場面では `SnapConfig::OFF` を明示的に渡す。

/// snap mode。`Off` 以外で `enabled = true` のとき `snap_beat` が `raw` を round する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapMode {
    Off,
    /// `1/div` 拍 (例: `div = 16` → 1/16 拍)。
    Straight { div: u32 },
    /// `1.5/div` 拍 (付点音符)。
    Dotted { div: u32 },
    /// `(2/3)/div` 拍 (三連符)。
    Triplet { div: u32 },
    /// zoom (px/beat) に応じて 1/N を自動選択 (`MIN_VISIBLE_GRID_PX = 12.0` 以上を満たす最大 unit)。
    Adaptive,
}

/// snap 設定。`mode` + `enabled` + `min_beat_unit` (snap unit の下限) の 3 値で表現。
///
/// `Eq` は `min_beat_unit: f64` を含むので derive 不可、`PartialEq` のみ。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapConfig {
    pub mode: SnapMode,
    pub enabled: bool,
    pub min_beat_unit: f64,
}

/// `Adaptive` の zoom 閾値。`zoom_x * unit >= MIN_VISIBLE_GRID_PX` を満たす最大 unit を選ぶ。
const MIN_VISIBLE_GRID_PX: f64 = 12.0;

impl SnapConfig {
    /// デフォルト snap (Adaptive ON、`min_beat_unit = 1/128`)。
    /// `Default::default()` と同値。
    pub const DEFAULT: Self = Self {
        mode: SnapMode::Adaptive,
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
    };

    /// 明示的に snap を切る。caller が「絶対 snap させたくない」場面で渡す。
    pub const OFF: Self = Self {
        mode: SnapMode::Off,
        enabled: false,
        min_beat_unit: 1.0 / 128.0,
    };

    /// `alt_pressed` (一時無効化) / `enabled = false` / `mode == Off` のいずれかで `false`。
    #[must_use]
    pub fn is_active(&self, alt_pressed: bool) -> bool {
        if alt_pressed || !self.enabled {
            return false;
        }
        !matches!(self.mode, SnapMode::Off)
    }

    /// 現在の snap mode + zoom から 1 unit の長さ (拍) を返す。
    /// `min_beat_unit` で floor。snap が無効 (alt / disabled / Off) なら `None`。
    /// zoom が 0 / 負 / 非有限のときは `MIN_VISIBLE_GRID_PX` 計算が破綻するので `None`。
    #[must_use]
    pub fn beat_unit(&self, zoom_x_px_per_beat: f32) -> Option<f64> {
        if !self.is_active(false) {
            return None;
        }
        let raw_unit = match self.mode {
            SnapMode::Off => return None,
            SnapMode::Straight { div } => 1.0 / f64::from(div.max(1)),
            SnapMode::Triplet { div } => (2.0 / 3.0) / f64::from(div.max(1)),
            SnapMode::Dotted { div } => 1.5 / f64::from(div.max(1)),
            SnapMode::Adaptive => beat_unit_for_zoom(zoom_x_px_per_beat),
        };
        let unit = raw_unit.max(self.min_beat_unit);
        if unit > 0.0 && unit.is_finite() {
            Some(unit)
        } else {
            None
        }
    }

    /// raw beat 値を snap unit に round。
    /// `alt_pressed` / `enabled = false` / `mode == Off` / unit 計算失敗のとき `raw` をそのまま返す。
    #[must_use]
    pub fn snap_beat(&self, raw: f64, alt_pressed: bool, zoom_x_px_per_beat: f32) -> f64 {
        if alt_pressed {
            return raw;
        }
        match self.beat_unit(zoom_x_px_per_beat) {
            Some(unit) => (raw / unit).round() * unit,
            None => raw,
        }
    }

    /// drag delta を snap unit に round。complex selection drag で anchor 0 の delta を
    /// 一度 snap → 全 anchor に同 delta を適用すると相対関係が維持される。
    #[must_use]
    pub fn snap_beat_delta(&self, raw_delta: f64, alt_pressed: bool, zoom_x_px_per_beat: f32) -> f64 {
        self.snap_beat(raw_delta, alt_pressed, zoom_x_px_per_beat)
    }
}

impl Default for SnapConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `Adaptive` 用: `zoom_x * unit >= 12.0` を満たす最大 unit。
/// 候補は `1, 1/2, 1/4, ..., 1/128` (7 段階)。
fn beat_unit_for_zoom(zoom_x_px_per_beat: f32) -> f64 {
    let zoom = f64::from(zoom_x_px_per_beat.max(0.001));
    let mut unit = 1.0_f64;
    for _ in 0..7 {
        let next = unit / 2.0;
        if zoom * next < MIN_VISIBLE_GRID_PX {
            break;
        }
        unit = next;
    }
    unit
}
