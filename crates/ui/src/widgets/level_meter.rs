//! `level_meter` widget — リアルタイム metering (M7 Phase 28)。
//!
//! 利用者が毎フレーム `sample` (現在の音量、`-1.0..=1.0`) を渡すと、library 側で
//! peak / RMS / peak hold ballistic を計算して縦バーで表示する。
//!
//! - `MeterBallistic::Peak`: そのフレームの peak をそのまま表示 + peak hold (~1 sec)
//! - `MeterBallistic::Rms`: 移動平均 (window) で滑らかに表示
//! - `MeterBallistic::Vu`: 300ms 上昇 / 300ms 下降の VU 風 (簡易実装)
//!
//! audio thread 連携は library 責務外。利用者が audio buffer から peak を抽出して
//! 毎フレーム渡す (CLAUDE.md L44 audio・IPC 不混入の原則)。

use std::hash::Hash;
use std::time::Instant;

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::id::WidgetId;
use crate::ui::Ui;

const RMS_WINDOW: usize = 32;
const PEAK_HOLD_DEFAULT_MS: u128 = 1500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterBallistic {
    Peak,
    Rms,
    Vu,
}

#[derive(Debug, Clone, Copy)]
pub struct LevelMeterStyle {
    pub bg: Color,
    pub low: Color,
    pub mid: Color,
    pub high: Color,
    pub clip: Color,
    pub peak_hold_color: Color,
    /// dB 範囲 (min, max)。例: (-60.0, 6.0)
    pub db_range: (f32, f32),
    /// peak hold の保持時間 (ms)
    pub peak_hold_ms: u128,
}

impl Default for LevelMeterStyle {
    fn default() -> Self {
        Self {
            bg: Color::rgb(0.08, 0.09, 0.11),
            low: Color::rgb(0.30, 0.85, 0.35),
            mid: Color::rgb(0.95, 0.85, 0.30),
            high: Color::rgb(0.95, 0.55, 0.25),
            clip: Color::rgb(0.95, 0.30, 0.30),
            peak_hold_color: Color::rgb(0.95, 0.97, 1.0),
            db_range: (-60.0, 6.0),
            peak_hold_ms: PEAK_HOLD_DEFAULT_MS,
        }
    }
}

#[derive(Debug)]
pub(crate) struct MeterState {
    pub peak: f32,
    pub peak_hold: f32,
    pub peak_hold_ts: Option<Instant>,
    pub rms_window: [f32; RMS_WINDOW],
    pub rms_idx: usize,
    pub vu_smoothed: f32,
}

impl Default for MeterState {
    fn default() -> Self {
        Self {
            peak: 0.0,
            peak_hold: 0.0,
            peak_hold_ts: None,
            rms_window: [0.0; RMS_WINDOW],
            rms_idx: 0,
            vu_smoothed: 0.0,
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// level_meter widget。`sample` は現在の音量 (`-1.0..=1.0`、利用者が audio buffer から抽出)。
    /// `ballistic` で表示モード切替 (Peak / Rms / Vu)。
    pub fn level_meter(
        &mut self,
        id: impl Hash,
        rect: Rect,
        sample: f32,
        ballistic: MeterBallistic,
        style: LevelMeterStyle,
    ) {
        let wid = WidgetId::ROOT.child((b"level_meter", &id));

        // 1. state 更新
        let (display_value, peak_hold_value) = {
            let state: &mut MeterState = self.widget_state(wid);
            let abs = sample.abs().min(2.0);

            // peak / peak_hold
            state.peak = state.peak.max(abs) * 0.92 + abs * 0.08; // 緩やかな peak decay
            let now = Instant::now();
            if abs >= state.peak_hold {
                state.peak_hold = abs;
                state.peak_hold_ts = Some(now);
            } else if let Some(ts) = state.peak_hold_ts
                && now.duration_since(ts).as_millis() > style.peak_hold_ms
            {
                state.peak_hold = abs;
                state.peak_hold_ts = Some(now);
            }

            // rms window
            state.rms_window[state.rms_idx] = abs * abs;
            state.rms_idx = (state.rms_idx + 1) % RMS_WINDOW;
            let sum_sq: f32 = state.rms_window.iter().sum();
            let rms = (sum_sq / RMS_WINDOW as f32).sqrt();

            // vu (300ms 移動平均風: 0.95 重み付け)
            state.vu_smoothed = state.vu_smoothed * 0.95 + abs * 0.05;

            let display = match ballistic {
                MeterBallistic::Peak => state.peak,
                MeterBallistic::Rms => rms,
                MeterBallistic::Vu => state.vu_smoothed,
            };
            (display, state.peak_hold)
        };

        // M7 後改善: peak / peak_hold が active なら自動 redraw 要求
        // (idle 時 = display も peak_hold もほぼ 0 なら redraw 不要、電力節約)。
        if display_value > 1e-4 || peak_hold_value > 1e-4 {
            self.request_redraw();
        }

        // 2. 描画
        // 背景
        self.push_rect(RectCommand {
            rect,
            fill: style.bg,
            border: Color::rgb(0.20, 0.22, 0.26),
            border_width: 1.0,
            radius: [2.0; 4],
            clip_rect: None,
        });

        // dB scale 上で fraction (0..1) を計算
        let db = linear_to_db(display_value);
        let frac = db_to_fraction(db, style.db_range);
        let bar_h = (rect.h * frac).clamp(0.0, rect.h);
        if bar_h > 0.0 {
            // 色帯: 0..0.7 緑、0.7..0.9 黄、0.9..1.0 オレンジ、>1.0 赤
            let color = if display_value > 1.0 {
                style.clip
            } else if frac > 0.9 {
                style.high
            } else if frac > 0.7 {
                style.mid
            } else {
                style.low
            };
            let bar_rect = Rect {
                x: rect.x + 1.0,
                y: rect.y + (rect.h - bar_h),
                w: (rect.w - 2.0).max(1.0),
                h: bar_h,
            };
            self.push_rect(RectCommand {
                rect: bar_rect,
                fill: color,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [1.0; 4],
                clip_rect: None,
            });
        }

        // peak hold の細い線
        let hold_db = linear_to_db(peak_hold_value);
        let hold_frac = db_to_fraction(hold_db, style.db_range);
        if hold_frac > 0.001 {
            let hold_y = rect.y + rect.h - rect.h * hold_frac;
            self.push_rect(RectCommand {
                rect: Rect {
                    x: rect.x + 1.0,
                    y: hold_y - 1.0,
                    w: (rect.w - 2.0).max(1.0),
                    h: 2.0,
                },
                fill: style.peak_hold_color,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }

        // dB 数値ラベル (右下)
        if rect.h > 40.0 {
            let label = format!("{db:.1}");
            self.push_text(GlyphArea {
                text: label,
                left: rect.x + 2.0,
                top: rect.y + rect.h - 14.0,
                font_size: 10.0,
                line_height: 12.0,
                color: Color::rgba(0.85, 0.88, 0.92, 0.85),
                clip_rect: Some(rect),
            });
        }
    }
}

fn linear_to_db(linear: f32) -> f32 {
    if linear <= 1e-6 {
        return -120.0;
    }
    20.0 * linear.log10()
}

fn db_to_fraction(db: f32, range: (f32, f32)) -> f32 {
    let (lo, hi) = range;
    ((db - lo) / (hi - lo)).clamp(0.0, 1.0)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn linear_to_db_zero_is_minus_120() {
        assert!((linear_to_db(0.0) + 120.0).abs() < 1e-3);
    }

    #[test]
    fn linear_to_db_one_is_zero() {
        assert!(linear_to_db(1.0).abs() < 1e-3);
    }

    #[test]
    fn db_to_fraction_min_max() {
        assert_eq!(db_to_fraction(-60.0, (-60.0, 6.0)), 0.0);
        assert_eq!(db_to_fraction(6.0, (-60.0, 6.0)), 1.0);
        assert!((db_to_fraction(0.0, (-60.0, 6.0)) - 60.0 / 66.0).abs() < 1e-5);
    }
}
