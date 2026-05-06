//! Beat 単位の grid snap 計算。
//!
//! piano_roll / arrangement widget の drag overlay と commit 値を grid に吸着させるため
//! の純データ型 + 純関数。widget 側は `view.snap.snap_beat_delta(...)` を 1 行呼ぶだけで
//! grid 吸着が効く。
//!
//! デフォルト = `Adaptive` ON (DAW UI の業界標準。Cubase / Live と一致)。
//! 「絶対 snap させたくない」場面では `SnapConfig::OFF` を明示的に渡す。
//!
//! # 単位の semantics (DAW 業界標準と一致)
//!
//! label `"1/N"` は **N 分音符 (Nth note)** を表し、 quarter note (1/4) を 1 beat の
//! 基準とする (Cubase / Live / Reaper / FL Studio / REAPER manual 等で共通の慣行、
//! MIDI ticks per quarter note の業界標準とも整合)。
//!
//! - whole note (1/1) = 4 beats (= 1 bar @ 4/4)
//! - half note (1/2) = 2 beats
//! - quarter note (1/4) = 1 beat
//! - eighth note (1/8) = 0.5 beat
//! - sixteenth note (1/16) = 0.25 beat
//! - 32nd note (1/32) = 0.125 beat
//!
//! `Bars { count }` は別概念で `time_sig` 依存 (4/4 では 1 bar = 4 beats、 3/4 では 3 beats、
//! 6/8 では 3 beats)。 4/4 の場合 `Straight { div: 1 }` と `Bars { count: 1 }` は同値、
//! それ以外の拍子では分岐する。

/// snap mode。`Off` 以外で `enabled = true` のとき `snap_beat` が `raw` を round する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapMode {
    Off,
    /// `4/div` 拍 (= div 分音符、 div=4 → 1/4 note = 1 beat、 div=16 → 1/16 note = 0.25 beat)。
    /// DAW 業界標準 (Cubase / Live / Reaper) の "1/N" label 解釈と一致。
    Straight { div: u32 },
    /// `6/div` 拍 (= div 分音符の付点、 div=4 → 1/4. = 1.5 beat、 div=8 → 1/8. = 0.75 beat)。
    /// 付点係数 = 1.5、 base = `Straight` と共通の `4/div` 拍。
    Dotted { div: u32 },
    /// `(8/3)/div` 拍 (= div 分音符の 3 連符、 div=4 → 1/4T = 0.6667 beat)。
    /// 三連係数 = 2/3、 base = `Straight` と共通の `4/div` 拍。
    Triplet { div: u32 },
    /// (M14 Phase 61c / daw_01 #011) `count` bar 単位 snap。 1 bar の拍数は `SnapConfig.time_sig`
    /// から `numerator * 4 / denominator` で計算 (4/4 → 4 拍、 3/4 → 3 拍、 6/8 → 3 拍)。
    /// `count = 0` は `Off` 同等 (`beat_unit` が `None` を返す、 defensive)。
    /// 1/2 bar 等の分数 bar は表現不可 (実需要が出たら fraction Bars を別途検討)。
    Bars { count: u32 },
    /// zoom (px/beat) に応じて 1/N を自動選択 (`MIN_VISIBLE_GRID_PX = 12.0` 以上を満たす最大 unit)。
    Adaptive,
}

/// snap 設定。`mode` + `enabled` + `min_beat_unit` (snap unit の下限) + `time_sig` で表現。
///
/// `Eq` は `min_beat_unit: f64` を含むので derive 不可、`PartialEq` のみ。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapConfig {
    pub mode: SnapMode,
    pub enabled: bool,
    pub min_beat_unit: f64,
    /// (M14 Phase 61c / daw_01 #011) `Bars` mode の 1 bar 拍数計算用。
    /// `(numerator, denominator)`、 default `(4, 4)`。 caller (`PianoRollView` /
    /// `ArrangementView` を組む側) は既に `view.time_sig` を持つので、 SnapConfig 組み立て時に
    /// 同じ source から 1 行で渡す (二重持ちは caller 責務、 ずれても 1 frame 遅延のみで実害小)。
    /// `Bars` 以外の mode では使われない。
    pub time_sig: (u8, u8),
}

/// `Adaptive` の zoom 閾値。`zoom_x * unit >= MIN_VISIBLE_GRID_PX` を満たす最大 unit を選ぶ。
const MIN_VISIBLE_GRID_PX: f64 = 12.0;

impl SnapConfig {
    /// デフォルト snap (Adaptive ON、`min_beat_unit = 1/128`、 `time_sig = (4, 4)`)。
    /// `Default::default()` と同値。
    pub const DEFAULT: Self = Self {
        mode: SnapMode::Adaptive,
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
    };

    /// 明示的に snap を切る。caller が「絶対 snap させたくない」場面で渡す。
    pub const OFF: Self = Self {
        mode: SnapMode::Off,
        enabled: false,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
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
    /// `Bars { count: 0 }` も `None` (defensive、 caller の dropdown 等で 0 が漏れた場合)。
    #[must_use]
    pub fn beat_unit(&self, zoom_x_px_per_beat: f32) -> Option<f64> {
        if !self.is_active(false) {
            return None;
        }
        let raw_unit = match self.mode {
            SnapMode::Off => return None,
            // DAW 業界標準: "1/N" label = N 分音符。 whole note (= 4 quarter notes = 4 beats)
            // を base に `4/div` 拍。 div=4 → 1.0 beat (1/4 note = 1 beat = quarter note)。
            SnapMode::Straight { div } => 4.0 / f64::from(div.max(1)),
            // 三連係数 2/3 を Straight に乗算。 div=4 → (8/3)/4 = 0.6667 beat (1/4T)。
            SnapMode::Triplet { div } => (8.0 / 3.0) / f64::from(div.max(1)),
            // 付点係数 1.5 を Straight に乗算。 div=4 → 6/4 = 1.5 beat (1/4.)。
            SnapMode::Dotted { div } => 6.0 / f64::from(div.max(1)),
            // (M14 Phase 61c / daw_01 #011) Bars: 1 bar = `time_sig.0 * 4 / time_sig.1` 拍。
            // count = 0 は None で skip。 time_sig の各成分は 0 防御で max(1)。
            SnapMode::Bars { count } => {
                if count == 0 {
                    return None;
                }
                let num = f64::from(self.time_sig.0.max(1));
                let den = f64::from(self.time_sig.1.max(1));
                let beats_per_bar = num * 4.0 / den;
                beats_per_bar * f64::from(count)
            }
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
