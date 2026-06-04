//! `level_meter` widget — リアルタイム metering (M7 Phase 28 / Ableton 風拡張 M14 Phase 102)。
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
//!
//! ## Ableton 風オプション (M14 Phase 102 / daw_01 #073)
//!
//! `LevelMeterStyle` に以下を持たせると、 メーターのバー・目盛り・数値ピークを **1 widget が
//! 同一の dB→位置マッピングで所有** する (SSoT):
//! - `scale: Some(MeterScale)` → rect 内を **tick (バー左) + バー + dB 数字 (バー右)** に
//!   レイアウト。 tick 位置は内部 `db_to_fraction` をバーと共有するので必ず一致する。
//! - `peak_readout: true` → 最大到達 dB の **数値ピークホールド** を上端にオーバーレイ表示し、
//!   **メーターを click すると reset** する (widget 内部の long-term peak を 0 に戻す)。
//! - `scale: None` (default) → 目盛りも per-meter 数値ラベルも描かない **clean bar**。 narrow
//!   (4px 等) メーターで数字が潰れて「点」になる問題はこれで起きない。

use std::hash::Hash;
use std::time::Instant;

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::id::WidgetId;
use crate::ui::Ui;

const RMS_WINDOW: usize = 32;
const PEAK_HOLD_DEFAULT_MS: u128 = 1500;

/// `scale = Some` 時、 バーの **左** に確保する tick 用ガター幅 (px)。
const SCALE_TICK_GUTTER_W: f32 = 8.0;
/// `scale = Some` 時、 バーの **右** に確保する数字用ガター幅 (px)。
const SCALE_NUM_GUTTER_W: f32 = 18.0;
/// tick 線の長さ (px)。 バー左端のすぐ左に引く短い線。
const SCALE_TICK_LEN: f32 = 5.0;
/// scale 時の上下パディング (px)。 端ラベル (+6 / -60) を rect の端に貼り付けない。
const SCALE_VPAD: f32 = 6.0;
/// dB ラベルの font サイズ。
const SCALE_FONT_PX: f32 = 9.0;
/// peak readout の font サイズ。
const READOUT_FONT_PX: f32 = 10.0;
/// peak readout の暗チップ高さ。
const READOUT_H: f32 = 13.0;
/// peak readout 専用帯の高さ (チップ + 上下余白)。 バー/目盛りはこのぶん下げる。
const READOUT_BAND_H: f32 = READOUT_H + 3.0;
/// 文字幅の近似係数 (固定幅前提、 chip 幅 / 中央寄せ用)。
const CHAR_W_RATIO: f32 = 0.62;

/// `MeterScale` の default ラベル dB 列 (上 → 下)。 Ableton Live のマスターメーターと同じ
/// 均等 6dB 間隔 (+6 → -60)。
const DEFAULT_SCALE_DB: &[f32] =
    &[6.0, 0.0, -6.0, -12.0, -18.0, -24.0, -30.0, -36.0, -42.0, -48.0, -54.0, -60.0];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterBallistic {
    Peak,
    Rms,
    Vu,
}

/// メーター右側に描く dB 目盛りの定義 (M14 Phase 102 / daw_01 #073)。
///
/// `labels_db` を `&'static [f32]` にしているのは **`LevelMeterStyle` の `Copy` を維持する**
/// ため。 `Vec<f32>` だと `LevelMeterStyle` が `Copy` を失い、 `let s = ...default(); /* 2 度
/// 使う */` のような既存利用が move-after-use でコンパイル不能になる。 ラベルは静的配列リテラル
/// (`&[6.0, 0.0, ...]`) か [`MeterScale::default`] で渡す。
#[derive(Debug, Clone, Copy)]
pub struct MeterScale {
    /// ラベル表示する dB 値 (描画順は問わない)。 各 tick 位置は `db_range` 上の `db_to_fraction`
    /// でバーと同一マッピングに置かれる。
    pub labels_db: &'static [f32],
    /// `0dB` を強調する: 左 tick / ラベルを明色 (`scale_zero_color`) にし、 バーを横切る 0dB
    /// 基準線 (3px) を重ねる。
    pub emphasize_zero: bool,
}

impl Default for MeterScale {
    fn default() -> Self {
        Self { labels_db: DEFAULT_SCALE_DB, emphasize_zero: true }
    }
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
    /// `Some` のとき rect 右側に dB 目盛り (tick + ラベル) を描く (M14 Phase 102 / daw_01 #073)。
    /// `None` (default) なら目盛りも per-meter 数値ラベルも描かない clean bar。
    pub scale: Option<MeterScale>,
    /// `true` のとき最大到達 dB の数値ピークホールドを上端にオーバーレイ表示し、 click で reset
    /// する (M14 Phase 102 / daw_01 #073)。 default `false`。
    pub peak_readout: bool,
    /// dB ラベル文字色。
    pub scale_text_color: Color,
    /// tick 線の色。
    pub scale_tick_color: Color,
    /// 0dB 強調時の tick / ラベル / 基準線の色 (`MeterScale.emphasize_zero`)。
    pub scale_zero_color: Color,
    /// peak readout 数値の通常色 (< 0dB)。
    pub peak_readout_color: Color,
    /// peak readout 数値の over 色 (>= 0dB)。
    pub peak_readout_over_color: Color,
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
            scale: None,
            peak_readout: false,
            scale_text_color: Color::rgba(0.74, 0.78, 0.84, 0.95),
            scale_tick_color: Color::rgba(0.66, 0.70, 0.77, 0.95),
            scale_zero_color: Color::rgb(0.90, 0.92, 0.98),
            peak_readout_color: Color::rgb(0.86, 0.89, 0.94),
            peak_readout_over_color: Color::rgb(0.95, 0.35, 0.32),
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
    /// 最大到達 amplitude (linear、 減衰なし)。 peak_readout の数値表示用、 click で 0 に reset。
    pub long_peak: f32,
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
            long_peak: 0.0,
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// level_meter widget。`sample` は現在の音量 (`-1.0..=1.0`、利用者が audio buffer から抽出)。
    /// `ballistic` で表示モード切替 (Peak / Rms / Vu)。
    ///
    /// `style.scale` / `style.peak_readout` で Ableton 風の目盛り + 数値ピークを有効化できる
    /// (M14 Phase 102 / daw_01 #073)。 `peak_readout = true` のときのみメーター rect 上の
    /// primary click を **消費** して long-term peak を reset する (clean bar のときは消費しない)。
    pub fn level_meter(
        &mut self,
        id: impl Hash,
        rect: Rect,
        sample: f32,
        ballistic: MeterBallistic,
        style: LevelMeterStyle,
    ) {
        let wid = WidgetId::ROOT.child((b"level_meter", &id));

        // peak_readout のときだけメーター click を消費して long-term peak を reset する。
        // clean bar (peak_readout=false) は interactive でないので pointer を奪わない。
        let reset_clicked = style.peak_readout && self.take_primary_press_in_rect(rect).is_some();

        // 1. state 更新
        let (display_value, peak_hold_value, long_peak_value) = {
            let state: &mut MeterState = self.widget_state(wid);
            if reset_clicked {
                state.long_peak = 0.0;
            }
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

            // long-term peak hold (最大到達、 減衰なし)
            state.long_peak = state.long_peak.max(abs);

            let display = match ballistic {
                MeterBallistic::Peak => state.peak,
                MeterBallistic::Rms => rms,
                MeterBallistic::Vu => state.vu_smoothed,
            };
            (display, state.peak_hold, state.long_peak)
        };

        // M7 後改善: peak / peak_hold が active なら自動 redraw 要求
        // (idle 時 = display も peak_hold もほぼ 0 なら redraw 不要、電力節約)。
        if display_value > 1e-4 || peak_hold_value > 1e-4 {
            self.request_redraw();
        }

        // 2. レイアウト: scale=Some なら **バーの左に tick ガター・右に数字ガター** を確保し、
        //    バーはその間に置く (tick がバーの左、 数字がバーの右)。 scale なし = clean bar は全幅。
        let has_scale = style.scale.is_some();
        let (left_g, right_g) = if has_scale {
            let total = (SCALE_TICK_GUTTER_W + SCALE_NUM_GUTTER_W).min((rect.w - 3.0).max(0.0));
            let lg = SCALE_TICK_GUTTER_W.min(total);
            (lg, total - lg)
        } else {
            (0.0, 0.0)
        };
        let bar_x = rect.x + left_g;
        let bar_w = (rect.w - left_g - right_g).max(1.0);

        // peak_readout 時は上端に数値専用帯を確保。 scale 時は上下に縦パディングを入れて端ラベル
        // (+6 / -60) を rect の端に貼り付けない。 バーも同 content 領域にマップして目盛りと整合させる。
        // content_top/bottom を rect 内に clamp し、 degenerate sizing (rect.h が小さすぎ) でも
        // バー/目盛りが widget 矩形の外に出ないようにする (content.h は 0 まで縮み得る = 何も描かない)。
        let vpad = if has_scale { SCALE_VPAD } else { 0.0 };
        let band = if style.peak_readout { READOUT_BAND_H } else { 0.0 };
        let content_top = (rect.y + band + vpad).clamp(rect.y, rect.y + rect.h);
        let content_bottom = (rect.y + rect.h - vpad).clamp(content_top, rect.y + rect.h);
        let content = Rect { x: rect.x, y: content_top, w: rect.w, h: content_bottom - content_top };

        // 背景 (rect 全体)
        self.push_rect(RectCommand {
            rect,
            fill: style.bg,
            border: Color::rgb(0.20, 0.22, 0.26),
            border_width: 1.0,
            radius: [2.0; 4],
            clip_rect: None,
        });

        // 色帯バー + peak hold 線 (content 領域、 右側 bar_x から bar_w 幅)。
        self.draw_meter_bar(content, bar_x, bar_w, display_value, peak_hold_value, &style);

        // 3. dB 目盛り (tick = バー左 / 数字 = バー右)。 db_to_fraction をバーと共有 = 必ず一致。
        //    content は ty マッピング用 (vpad で inset)、 clip は外側 rect (端ラベルが vpad 領域に
        //    はみ出しても見えるように)。
        if let Some(scale) = style.scale {
            self.draw_meter_scale(content, rect, bar_x, bar_w, scale, &style);
        }

        // 4. peak readout (上端の専用帯)。
        if style.peak_readout {
            self.draw_meter_readout(rect, long_peak_value, &style);
        }
    }

    /// 色帯バー (dB→高さ) + peak hold 線を `content` 右側 (`bar_x` 左端、 `bar_w` 幅) に描く。
    /// `content` は rect 内に clamp 済 (degenerate sizing でもバーは widget 矩形内に収まる)。
    fn draw_meter_bar(
        &mut self,
        content: Rect,
        bar_x: f32,
        bar_w: f32,
        display_value: f32,
        peak_hold_value: f32,
        style: &LevelMeterStyle,
    ) {
        let db = linear_to_db(display_value);
        let frac = db_to_fraction(db, style.db_range);
        let bar_h = (content.h * frac).clamp(0.0, content.h);
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
            self.push_rect(RectCommand {
                rect: Rect {
                    x: bar_x + 1.0,
                    y: content.y + (content.h - bar_h),
                    w: (bar_w - 2.0).max(1.0),
                    h: bar_h,
                },
                fill: color,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [1.0; 4],
                clip_rect: None,
            });
        }

        // peak hold の細い線 (バー領域内)
        let hold_frac = db_to_fraction(linear_to_db(peak_hold_value), style.db_range);
        if hold_frac > 0.001 {
            let hold_y = content.y + content.h - content.h * hold_frac;
            self.push_rect(RectCommand {
                rect: Rect {
                    x: bar_x + 1.0,
                    y: hold_y - 1.0,
                    w: (bar_w - 2.0).max(1.0),
                    h: 2.0,
                },
                fill: style.peak_hold_color,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }
    }

    /// dB 目盛りを描く: **tick はバーの左**、 **数字はバーの右**。 tick の y は `db_to_fraction`
    /// をバーと共有するので必ずバーと一致する (SSoT)。 tick はアンチエイリアスで消えないよう 2px
    /// 以上 + 整数 px に丸めて crisp に。
    fn draw_meter_scale(
        &mut self,
        content: Rect,
        clip: Rect,
        bar_x: f32,
        bar_w: f32,
        scale: MeterScale,
        style: &LevelMeterStyle,
    ) {
        let bar_right = bar_x + bar_w;
        let tick_right = bar_x - 1.0; // バー左端の 1px 左
        let label_left = bar_right + 3.0; // バー右端の 3px 右 (数字は左寄せ)
        let has_label_room = (clip.x + clip.w - label_left) > SCALE_FONT_PX * 0.8;
        for &tick_db in scale.labels_db {
            let f = db_to_fraction(tick_db, style.db_range);
            let ty = (content.y + content.h - content.h * f).round(); // 整数 px で crisp
            let is_zero = scale.emphasize_zero && tick_db.abs() < 1e-3;
            // 左 tick: 0dB も通常と同じ 2px 太さ・長さ。 0dB は色だけ明色。 全 tick 2px で
            // アンチエイリアス消失を防ぐ。
            let tick_left = (tick_right - SCALE_TICK_LEN).max(clip.x);
            self.push_rect(RectCommand {
                rect: Rect {
                    x: tick_left,
                    y: ty - 1.0,
                    w: (tick_right - tick_left).max(1.0),
                    h: 2.0,
                },
                fill: if is_zero { style.scale_zero_color } else { style.scale_tick_color },
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: Some(clip),
            });
            // 0dB はバーを横切る基準線 (3px、 バー幅) を追加で重ねる (Ableton 風 0dB ライン)。
            if is_zero {
                self.push_rect(RectCommand {
                    rect: Rect { x: bar_x, y: ty - 1.5, w: bar_w, h: 3.0 },
                    fill: style.scale_zero_color,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: Some(clip),
                });
            }
            if has_label_room {
                let label = format_scale_db(tick_db);
                // ラベルは tick 中心 (ty) に合わせる。 clip は外側 rect なので端ラベルが vpad 領域に
                // 入っても見える (= +6 / -60 が tick とズレない)。 念のため外 rect 内に clamp。
                let label_top =
                    (ty - SCALE_FONT_PX * 0.62).clamp(clip.y, clip.y + clip.h - SCALE_FONT_PX);
                self.push_text(GlyphArea {
                    text: label.into(),
                    left: label_left,
                    top: label_top,
                    font_size: SCALE_FONT_PX,
                    line_height: SCALE_FONT_PX + 2.0,
                    color: if is_zero { style.scale_zero_color } else { style.scale_text_color },
                    clip_rect: Some(clip),
                    ..GlyphArea::default()
                });
            }
        }
    }

    /// 最大到達 dB の数値ピークを rect 上端の専用帯に描く (暗チップ + 数値、 全幅中央寄せ)。
    fn draw_meter_readout(&mut self, rect: Rect, long_peak_value: f32, style: &LevelMeterStyle) {
        let (text, over) = format_peak_readout(long_peak_value);
        let color = if over { style.peak_readout_over_color } else { style.peak_readout_color };
        // チップは文字幅に合わせ、 メーター全幅 (バー + ガター) で中央寄せ。
        let text_w = text.chars().count() as f32 * READOUT_FONT_PX * CHAR_W_RATIO;
        let chip_w = (text_w + 6.0).min(rect.w);
        let chip_x = rect.x + ((rect.w - chip_w).max(0.0)) * 0.5;
        self.push_rect(RectCommand {
            rect: Rect { x: chip_x, y: rect.y + 1.0, w: chip_w, h: READOUT_H },
            fill: Color::rgba(0.0, 0.0, 0.0, 0.78),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [2.0; 4],
            clip_rect: Some(rect),
        });
        self.push_text(GlyphArea {
            text: text.into(),
            left: chip_x + (chip_w - text_w).max(0.0) * 0.5,
            top: rect.y + 1.0 + (READOUT_H - READOUT_FONT_PX) * 0.5 - 1.0,
            font_size: READOUT_FONT_PX,
            line_height: READOUT_H,
            color,
            clip_rect: Some(rect),
            ..GlyphArea::default()
        });
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

/// 目盛りラベル文字列。 Ableton Live と同じく **符号なしの絶対値**整数 (`"6"` / `"0"` / `"60"`)。
/// 正負は tick の位置 (0dB の上下) で読み取る。
fn format_scale_db(db: f32) -> String {
    format!("{}", db.abs().round() as i32)
}

/// peak readout 文字列と over フラグ。 `long_peak` が無音なら `("-inf", false)`、 そうでなければ
/// `("{db:.1}", db >= 0.0)`。
fn format_peak_readout(long_peak_linear: f32) -> (String, bool) {
    if long_peak_linear <= 1e-6 {
        return ("-inf".to_string(), false);
    }
    let db = linear_to_db(long_peak_linear);
    (format!("{db:.1}"), db >= 0.0)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::Scene;

    use super::*;
    use crate::FrameInput;
    use crate::input::PointerFrame;
    use crate::ui::UiHost;

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

    #[test]
    fn meter_scale_default_labels() {
        let s = MeterScale::default();
        assert_eq!(
            s.labels_db,
            &[6.0, 0.0, -6.0, -12.0, -18.0, -24.0, -30.0, -36.0, -42.0, -48.0, -54.0, -60.0]
        );
        assert!(s.emphasize_zero);
    }

    #[test]
    fn format_scale_db_no_sign() {
        assert_eq!(format_scale_db(6.0), "6");
        assert_eq!(format_scale_db(0.0), "0");
        assert_eq!(format_scale_db(-6.0), "6");
        assert_eq!(format_scale_db(-60.0), "60");
    }

    #[test]
    fn format_peak_readout_table() {
        // 無音 → -inf / not over
        let (t, over) = format_peak_readout(0.0);
        assert_eq!(t, "-inf");
        assert!(!over);
        // 0dBFS (linear 1.0) → "0.0" / over
        let (t, over) = format_peak_readout(1.0);
        assert_eq!(t, "0.0");
        assert!(over);
        // -6dB (linear 0.5) → "-6.0" / not over
        let (t, over) = format_peak_readout(0.5);
        assert_eq!(t, "-6.0");
        assert!(!over);
        // +6dB (linear ~2.0) → "6.0" / over
        let (t, over) = format_peak_readout(2.0);
        assert_eq!(t, "6.0");
        assert!(over);
    }

    fn run_meter(
        host: &mut UiHost<()>,
        rect: Rect,
        sample: f32,
        style: LevelMeterStyle,
        pointer: PointerFrame,
    ) -> Scene {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 300 };
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |(), ui| {
                ui.level_meter("m", rect, sample, MeterBallistic::Peak, style);
            },
        );
        scene
    }

    fn press_at(pos: (f32, f32)) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_just_pressed: true,
            primary_pressed: true,
            ..PointerFrame::default()
        }
    }

    /// default (scale=None / peak_readout=false) は clean bar = テキストを一切描かない。
    /// これが 4px narrow メーターの「ドット」解消の回帰固定 (#073 要件 3)。
    #[test]
    fn clean_bar_has_no_text_by_default() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 8.0, h: 200.0 };
        let scene = run_meter(&mut host, rect, 0.5, LevelMeterStyle::default(), PointerFrame::default());
        assert_eq!(scene.glyph_count(), 0, "clean bar は per-meter ラベルを描かない");
    }

    /// scale=Some で dB ラベルが描かれる (tick はバーと同一マッピング)。
    #[test]
    fn scale_some_draws_tick_labels() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 40.0, h: 220.0 };
        let style = LevelMeterStyle { scale: Some(MeterScale::default()), ..Default::default() };
        let scene = run_meter(&mut host, rect, 0.0, style, PointerFrame::default());
        let labels: Vec<&str> = scene.iter_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(labels.contains(&"0"), "0dB ラベルがある (got {labels:?})");
        assert!(labels.contains(&"6"), "6dB ラベル (符号なし) がある (got {labels:?})");
        assert!(labels.contains(&"60"), "60dB ラベル (符号なし) がある (got {labels:?})");
    }

    /// peak_readout: 最大到達 dB を表示し、 click で reset (full cycle)。
    #[test]
    fn peak_readout_resets_on_click() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 40.0, h: 200.0 };
        let style = LevelMeterStyle { peak_readout: true, ..Default::default() };

        // Frame 1: 0dBFS が来て long_peak=1.0 → 数値 "0.0"
        let scene = run_meter(&mut host, rect, 1.0, style, PointerFrame::default());
        let texts: Vec<&str> = scene.iter_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(texts.contains(&"0.0"), "最大到達 0.0dB を表示 (got {texts:?})");

        // Frame 2: 無音 + メーター click → long_peak reset → "-inf"
        let scene = run_meter(&mut host, rect, 0.0, style, press_at((20.0, 100.0)));
        let texts: Vec<&str> = scene.iter_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(texts.contains(&"-inf"), "click reset 後は -inf (got {texts:?})");
    }

    /// 端 (+6 / -60) を含む全 scale ラベルが rect の上下端からはみ出さない (top を clamp)。
    #[test]
    fn scale_labels_stay_within_rect() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 10.0, y: 20.0, w: 40.0, h: 220.0 };
        let style = LevelMeterStyle { scale: Some(MeterScale::default()), ..Default::default() };
        let scene = run_meter(&mut host, rect, 0.0, style, PointerFrame::default());
        let top_lo = rect.y;
        let top_hi = rect.y + rect.h - SCALE_FONT_PX;
        for g in scene.iter_glyphs() {
            assert!(
                g.top >= top_lo - 0.01 && g.top <= top_hi + 0.01,
                "ラベル {:?} の top={} が rect [{top_lo}, {top_hi}] を外れる",
                g.text, g.top
            );
        }
    }

    /// peak_readout 有効時、 +6 ラベルは readout 帯の **直下にフル表示** され衝突しない (Live 風)。
    #[test]
    fn peak_readout_shows_plus6_below_band() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 46.0, h: 220.0 };
        let style = LevelMeterStyle {
            scale: Some(MeterScale::default()),
            peak_readout: true,
            ..Default::default()
        };
        let scene = run_meter(&mut host, rect, 0.0, style, PointerFrame::default());
        // +6 (frac=1.0) は content 上端 = readout 帯 + 縦パディングの直下にフル表示される。
        let content_top = rect.y + READOUT_BAND_H + SCALE_VPAD;
        let plus6 = scene
            .iter_glyphs()
            .find(|g| g.text.as_ref() == "6" && g.top < content_top + 4.0)
            .expect("+6 ラベル (符号なし '6') が readout 帯直下に表示される");
        assert!(
            plus6.top >= rect.y + READOUT_BAND_H - 0.01,
            "+6 ラベル top={} は readout 帯 (>= {}) の下にある",
            plus6.top,
            rect.y + READOUT_BAND_H
        );
    }

    /// 数値 readout は文字幅に合わせ rect 内に収まる (チップ背景からはみ出さない)。
    #[test]
    fn peak_readout_text_fits_within_rect() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 5.0, y: 0.0, w: 46.0, h: 200.0 };
        let style = LevelMeterStyle { peak_readout: true, ..Default::default() };
        let scene = run_meter(&mut host, rect, 1.0, style, PointerFrame::default());
        let g = scene
            .iter_glyphs()
            .find(|g| g.text.as_ref() == "0.0")
            .expect("readout 数値 0.0 がある");
        let text_w = g.text.chars().count() as f32 * READOUT_FONT_PX * CHAR_W_RATIO;
        assert!(g.left >= rect.x - 0.01, "readout left={} が rect 左端より内側", g.left);
        assert!(
            g.left + text_w <= rect.x + rect.w + 0.01,
            "readout 右端 {} が rect 右端 {} を超えない",
            g.left + text_w,
            rect.x + rect.w
        );
    }

    /// widget と同じレイアウト計算 (bar_x / bar_w)。
    fn bar_geom(rect: Rect) -> (f32, f32) {
        let total = (SCALE_TICK_GUTTER_W + SCALE_NUM_GUTTER_W).min((rect.w - 3.0).max(0.0));
        let left_g = SCALE_TICK_GUTTER_W.min(total);
        let bar_x = rect.x + left_g;
        let bar_w = (rect.w - total).max(1.0);
        (bar_x, bar_w)
    }

    /// 目盛り tick はバーの **左** (x+w <= bar_x)、 数字はバーの **右** (left >= bar_right)、
    /// かつ +6 の tick と数字の y が揃う (user が直したジオメトリの回帰固定)。
    #[test]
    fn scale_tick_left_of_bar_numbers_right_and_aligned() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 40.0, h: 240.0 };
        let style = LevelMeterStyle { scale: Some(MeterScale::default()), ..Default::default() };
        let scene = run_meter(&mut host, rect, 0.0, style, PointerFrame::default());
        let (bar_x, bar_w) = bar_geom(rect);
        let bar_right = bar_x + bar_w;
        // 2px tick がバーの左
        let ticks: Vec<_> = scene
            .iter_rects()
            .filter(|r| (r.rect.h - 2.0).abs() < 0.01 && r.rect.x + r.rect.w <= bar_x + 0.5)
            .collect();
        assert!(!ticks.is_empty(), "2px tick がバーの左 (x+w <= {bar_x}) に描かれる");
        // 数字はバーの右
        assert!(
            scene.iter_glyphs().any(|g| g.left >= bar_right - 0.5),
            "数字がバーの右 (left >= {bar_right}) に描かれる"
        );
        // +6 の tick (最上段) と +6 数字の y center が揃う
        let top_tick = ticks.iter().min_by(|a, b| a.rect.y.total_cmp(&b.rect.y)).unwrap();
        let top_six = scene
            .iter_glyphs()
            .filter(|g| g.text.as_ref() == "6")
            .min_by(|a, b| a.top.total_cmp(&b.top))
            .expect("+6 ラベル");
        let tick_center = top_tick.rect.y + top_tick.rect.h * 0.5;
        let label_center = top_six.top + SCALE_FONT_PX * 0.62;
        assert!(
            (tick_center - label_center).abs() < 2.5,
            "+6 tick (center {tick_center}) と数字 (center {label_center}) の y が揃う"
        );
    }

    /// `emphasize_zero` で 0dB はバーを横切る 3px 基準線 (バー幅) を引く。
    #[test]
    fn zero_db_line_crosses_bar() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 40.0, h: 240.0 };
        let style = LevelMeterStyle { scale: Some(MeterScale::default()), ..Default::default() };
        let scene = run_meter(&mut host, rect, 0.0, style, PointerFrame::default());
        let (bar_x, bar_w) = bar_geom(rect);
        let has_line = scene.iter_rects().any(|r| {
            (r.rect.h - 3.0).abs() < 0.01
                && (r.rect.x - bar_x).abs() < 0.5
                && (r.rect.w - bar_w).abs() < 0.5
        });
        assert!(has_line, "0dB 基準線 (バー幅 {bar_w}・3px) がバーを横切る");
    }
}
