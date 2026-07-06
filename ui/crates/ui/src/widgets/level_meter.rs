//! `level_meter_stereo` widget — Ableton Live 風のステレオ metering (M14 Phase 103 / daw_01 #074)。
//!
//! 利用者が毎フレーム L/R の `sample` (現在の音量、`-1.0..=1.0`) を渡すと、library 側で
//! peak / RMS / peak hold ballistic を L/R 個別に計算して **2 本の縦バー**で表示する。
//!
//! - `MeterBallistic::Peak`: そのフレームの peak をそのまま表示 + peak hold (~1 sec)
//! - `MeterBallistic::Rms`: 移動平均 (window) で滑らかに表示
//! - `MeterBallistic::Vu`: IEC 60268-17 風 VU 弾道 (2 次系、 ~300ms 立ち上がり +
//!   ~1.5% overshoot、 dt 駆動で frame-rate 非依存)
//!
//! audio thread 連携は library 責務外。利用者が audio buffer から L/R peak を抽出して
//! 毎フレーム渡す (CLAUDE.md L44 audio・IPC 不混入の原則)。
//!
//! ## scale (dB 目盛り) — Ableton Live 風 (daw_01 #074)
//!
//! `LevelMeterStyle.scale = Some(MeterScale)` で、 バー・dB 目盛り・数値ピークを **1 widget が
//! 同一の dB→位置マッピングで所有** する (SSoT、 daw_01 で目盛りを複製しない):
//! - レイアウト: rect 内を **`[tick (左) | L バー | R バー | dB 数字 (右)]`** に配置。
//! - **非線形スケール**: `MeterScale.curve` (breakpoint piecewise-linear、 top-weighted) で dB→高さを
//!   マップ。 バー塗り・tick・数字・0dB 線・peak hold 線すべて同一カーブ。 ラベル値が breakpoint なので
//!   数字は必ず tick に乗る。
//! - `emphasize_zero` で 0dB に **L/R 両バーを横切る横線** + 0 ラベルを明色。
//! - `peak_readout = true` で最大到達 dB の数値ピークホールドを上端帯に表示、 メーター click で reset。
//! - `scale = None` (default) → 目盛りなしの clean bar (L/R 2 本)。

use std::hash::Hash;
use std::time::Instant;

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};
use crate::theme;

use crate::id::WidgetId;
use crate::ui::Ui;

const RMS_WINDOW: usize = 32;
const PEAK_HOLD_DEFAULT_MS: u128 = 1500;

/// VU ballistic (E4 / r.md #8): IEC 60268-17 風 2 次系の自然角周波数 (≈300ms
/// 立ち上がり) と減衰比 (overshoot ≈1.5%)。
const VU_OMEGA: f32 = std::f32::consts::TAU / 0.3;
const VU_ZETA: f32 = 0.8;
/// VU 2 次系を semi-implicit Euler で積分する際の固定 sub-step (秒)。 安定限界
/// (この ω/ζ で ~0.0459s) に十分な余裕。 大 dt はこの刻みに分割して発散を防ぐ (M4→E4)。
const VU_MAX_STEP: f32 = 0.01;

/// `scale = Some` 時、 L バーの **左** に確保する tick 用ガター幅 (px)。
const SCALE_TICK_GUTTER_W: f32 = 6.0;
/// `scale = Some` 時、 R バーの **右** に確保する数字用ガター幅 (px)。
const SCALE_NUM_GUTTER_W: f32 = 18.0;
/// L / R バー間の隙間 (px)。
const STEREO_BAR_GAP: f32 = 1.0;
/// tick 線の長さ (px)。 L バー左端のすぐ左に引く短い線。
const SCALE_TICK_LEN: f32 = 5.0;
/// dB ラベルの font サイズ。
const SCALE_FONT_PX: f32 = 9.0;
/// peak readout の font サイズ。
const READOUT_FONT_PX: f32 = 10.0;
/// peak readout の暗チップ高さ。
const READOUT_H: f32 = 13.0;
/// peak readout 専用帯の高さ (チップ + 上下余白)。 バー/目盛りはこのぶん下げる。
const READOUT_BAND_H: f32 = READOUT_H + 3.0;
/// scale 時の上下パディング (px)。 端ラベル (+6 / -60) を rect の端に貼り付けない。
const SCALE_VPAD: f32 = 6.0;
/// 文字幅の近似係数 (固定幅前提、 chip 幅 / 中央寄せ用)。
const CHAR_W_RATIO: f32 = 0.62;

/// `MeterScale` の default ラベル dB 列 (上 → 下)。 均等 6dB 間隔 (+6 → -60)。
const DEFAULT_SCALE_DB: &[f32] =
    &[6.0, 0.0, -6.0, -12.0, -18.0, -24.0, -30.0, -36.0, -42.0, -48.0, -54.0, -60.0];

/// `MeterScale` の default 非線形カーブ (db, frac)。 上 (1.0) を引き伸ばし下 (0.0) を圧縮する
/// top-weighted。 ラベル値がそのまま breakpoint なので数字は必ず tick に乗る (daw_01 #074、
/// 実機でユーザーが視覚調整する前提の暫定値)。
const DEFAULT_CURVE: &[(f32, f32)] = &[
    (6.0, 1.00),
    (0.0, 0.89),
    (-6.0, 0.79),
    (-12.0, 0.68),
    (-18.0, 0.59),
    (-24.0, 0.49),
    (-30.0, 0.40),
    (-36.0, 0.31),
    (-42.0, 0.23),
    (-48.0, 0.15),
    (-54.0, 0.07),
    (-60.0, 0.00),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterBallistic {
    Peak,
    Rms,
    Vu,
}

/// メーターの dB 目盛り定義 (M14 Phase 103 / daw_01 #074)。
///
/// `labels_db` / `curve` を `&'static` にしているのは **`LevelMeterStyle` の `Copy` を維持する**
/// ため (`Vec` だと Copy を失い、 値を 2 度渡す caller が move-after-use で壊れる)。 配列リテラル
/// (`&[...]`) か [`MeterScale::default`] で渡す。
#[derive(Debug, Clone, Copy)]
pub struct MeterScale {
    /// ラベル表示する dB 値 (上 → 下)。 各 tick 位置は `curve` 上でバーと同一マッピングに置かれる。
    pub labels_db: &'static [f32],
    /// dB → 高さ (0.0..=1.0) の breakpoint piecewise-linear カーブ (db 降順、 frac 降順)。 非線形に
    /// 上を引き伸ばし下を圧縮する。 バー・tick・数字・0dB 線・peak hold が**すべてこのカーブ**で
    /// 位置決めされる (SSoT)。
    pub curve: &'static [(f32, f32)],
    /// `0dB` を強調する: 0dB に L/R 両バーを横切る横線を引き、 0 の tick / ラベルを明色にする。
    pub emphasize_zero: bool,
}

impl Default for MeterScale {
    fn default() -> Self {
        Self { labels_db: DEFAULT_SCALE_DB, curve: DEFAULT_CURVE, emphasize_zero: true }
    }
}

impl MeterScale {
    /// dB → fraction (0.0..=1.0)。 `self.curve` の piecewise-linear で変換 (SSoT)。
    /// 範囲外は端値に clamp。
    pub fn db_to_frac(&self, db: f32) -> f32 {
        curve_fraction(db, self.curve)
    }

    /// fraction (0.0..=1.0) → dB。 `db_to_frac` の逆写像 (curve が monotone なので一意)。
    /// `frac <= 0.0` は curve 下端 dB を返す。 `frac >= 1.0` は curve 上端 dB を返す。
    pub fn frac_to_db(&self, frac: f32) -> f32 {
        let curve = self.curve;
        let Some(&(top_db, top_f)) = curve.first() else {
            return 0.0;
        };
        if frac >= top_f {
            return top_db;
        }
        for w in curve.windows(2) {
            let (hd, hf) = w[0];
            let (ld, lf) = w[1];
            if frac <= hf && frac >= lf {
                let span = hf - lf;
                let t = if span.abs() < 1e-9 { 0.0 } else { (frac - lf) / span };
                return ld + t * (hd - ld);
            }
        }
        curve.last().map_or(0.0, |&(db, _)| db)
    }

    /// 線形振幅 (amp、 `1.0` = 0dB unity) → fraction (0.0..=1.0)。 fader / meter が
    /// 位置決めに使う frac は「dB taper 上のトラック位置」 なので、 amp を dB に
    /// 直してから [`db_to_frac`](Self::db_to_frac) に通す。 `amp <= 0` は無音 =
    /// `−∞ dB` として下端 (frac 0)。 mixer strip fader / arrangement track-volume
    /// band が振幅を frac 空間で描く単一 SSoT。
    pub fn amp_to_frac(&self, amp: f32) -> f32 {
        let db = if amp <= 0.0 { f32::NEG_INFINITY } else { 20.0 * amp.log10() };
        self.db_to_frac(db)
    }

    /// fraction (0.0..=1.0) → 線形振幅 (amp)。 [`amp_to_frac`](Self::amp_to_frac) の
    /// 逆。 `frac <= 0` は無音 (amp 0) を返す (curve 下端 dB は有限なので、 特別扱い
    /// しないと最下端が完全な無音にならない)。
    pub fn frac_to_amp(&self, frac: f32) -> f32 {
        if frac <= 0.0 {
            return 0.0;
        }
        10f32.powf(self.frac_to_db(frac) / 20.0)
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
    /// `scale = None` (clean bar) 時の線形 dB 範囲 (min, max)。例: (-60.0, 6.0)。
    /// `scale = Some` 時は `MeterScale.curve` が使われ、 この値は無視される。
    pub db_range: (f32, f32),
    /// peak hold の保持時間 (ms)
    pub peak_hold_ms: u128,
    /// `Some` のとき dB 目盛り (tick + 数字 + 0dB 線) を描く。 `None` (default) なら clean bar。
    pub scale: Option<MeterScale>,
    /// `true` のとき最大到達 dB の数値ピークホールドを上端帯に表示し、 click で reset。 default `false`。
    pub peak_readout: bool,
    /// dB ラベル文字色。
    pub scale_text_color: Color,
    /// tick 線の色。
    pub scale_tick_color: Color,
    /// 0dB 強調時の tick / ラベル / 横線の色 (`MeterScale.emphasize_zero`)。
    pub scale_zero_color: Color,
    /// peak readout 数値の通常色 (< 0dB)。
    pub peak_readout_color: Color,
    /// peak readout 数値の over 色 (>= 0dB)。
    pub peak_readout_over_color: Color,
}

impl Default for LevelMeterStyle {
    fn default() -> Self {
        Self {
            bg: theme::INSET_BG,
            low: theme::METER_GREEN,
            mid: theme::METER_YELLOW,
            high: theme::METER_ORANGE,
            clip: theme::METER_RED,
            peak_hold_color: theme::TEXT,
            db_range: (-60.0, 6.0),
            peak_hold_ms: PEAK_HOLD_DEFAULT_MS,
            scale: None,
            peak_readout: false,
            scale_text_color: theme::TEXT_DIM.with_alpha(0.95),
            scale_tick_color: theme::TEXT_DIM.with_alpha(0.95),
            scale_zero_color: theme::TEXT,
            peak_readout_color: theme::TEXT,
            peak_readout_over_color: theme::METER_RED,
        }
    }
}

/// 1 チャンネル (L または R) の ballistic state。
#[derive(Debug)]
pub(crate) struct ChannelMeter {
    peak: f32,
    peak_hold: f32,
    peak_hold_ts: Option<Instant>,
    rms_window: [f32; RMS_WINDOW],
    rms_idx: usize,
    // VU ballistic (E4 / r.md #8): IEC 60268-17 風の 2 次系 (位置 + 速度) を dt
    // 駆動で積分する。 旧実装は frame-rate 依存の対称指数平滑だった。
    vu_smoothed: f32,
    vu_velocity: f32,
    last_update: Option<Instant>,
}

impl Default for ChannelMeter {
    fn default() -> Self {
        Self {
            peak: 0.0,
            peak_hold: 0.0,
            peak_hold_ts: None,
            rms_window: [0.0; RMS_WINDOW],
            rms_idx: 0,
            vu_smoothed: 0.0,
            vu_velocity: 0.0,
            last_update: None,
        }
    }
}

impl ChannelMeter {
    /// 1 フレーム分 update して (display, peak_hold) を返す。
    fn update(&mut self, sample: f32, ballistic: MeterBallistic, peak_hold_ms: u128) -> (f32, f32) {
        let abs = sample.abs().min(2.0);

        // peak / peak_hold
        self.peak = self.peak.max(abs) * 0.92 + abs * 0.08; // 緩やかな peak decay
        let now = Instant::now();
        if abs >= self.peak_hold {
            self.peak_hold = abs;
            self.peak_hold_ts = Some(now);
        } else if let Some(ts) = self.peak_hold_ts
            && now.duration_since(ts).as_millis() > peak_hold_ms
        {
            self.peak_hold = abs;
            self.peak_hold_ts = Some(now);
        }

        // rms window
        self.rms_window[self.rms_idx] = abs * abs;
        self.rms_idx = (self.rms_idx + 1) % RMS_WINDOW;
        let rms = (self.rms_window.iter().sum::<f32>() / RMS_WINDOW as f32).sqrt();

        // VU ballistic (E4 / r.md #8): IEC 60268-17 の真の弾道に近づける。 旧実装は
        // frame-rate 依存の対称指数平滑 (≈ RMS) だった。 2 次系
        // `accel = ω²(in − x) − 2ζω·vel` を dt 駆動 semi-implicit Euler で積分する。
        // ω = 2π/0.3 (≈300ms 立ち上がり)、 ζ = 0.8 (overshoot ≈1.5%)。 dt は描画間隔
        // から実測 (frame-rate 非依存)、 数値発散防止に clamp。
        let dt = self
            .last_update
            .map_or(1.0 / 60.0, |t| now.duration_since(t).as_secs_f32())
            .clamp(0.0, 0.25);
        self.last_update = Some(now);
        // semi-implicit Euler の安定限界はこの 2 次系 (ω≈20.94, ζ=0.8) で
        // dt<~0.0459 s。 大 dt (低フレームレート) をそのまま積分すると発散する
        // ため、 固定 sub-step に分割して積分する。 こうすると発振せず、 かつ
        // frame-rate 非依存の正しい時定数を保つ (旧実装は 0.05 に clamp するだけで、
        // 上限が安定限界を超えており、 持続的に ≤~21fps だと発振→NaN で張り付いた)。
        let mut remaining = dt;
        while remaining > 0.0 {
            let step = remaining.min(VU_MAX_STEP);
            let accel = VU_OMEGA * VU_OMEGA * (abs - self.vu_smoothed)
                - 2.0 * VU_ZETA * VU_OMEGA * self.vu_velocity;
            self.vu_velocity += accel * step;
            self.vu_smoothed = (self.vu_smoothed + self.vu_velocity * step).max(0.0);
            remaining -= step;
        }

        let display = match ballistic {
            MeterBallistic::Peak => self.peak,
            MeterBallistic::Rms => rms,
            MeterBallistic::Vu => self.vu_smoothed,
        };
        (display, self.peak_hold)
    }
}

#[derive(Debug, Default)]
pub(crate) struct MeterState {
    l: ChannelMeter,
    r: ChannelMeter,
    /// L/R を通じた最大到達 amplitude (linear、 減衰なし)。 readout 用、 click で 0 に reset。
    long_peak: f32,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// ステレオ level meter widget (M14 Phase 103 / daw_01 #074)。`l` / `r` は現在の L/R 音量
    /// (`-1.0..=1.0`、 利用者が audio buffer から抽出)。`ballistic` で表示モード切替。
    ///
    /// `style.scale` で Ableton Live 風の目盛り (tick + 数字 + 0dB 横線、 非線形カーブ) を、
    /// `style.peak_readout` で数値ピーク (click reset) を有効化できる。 `peak_readout = true` の
    /// ときのみメーター rect 上の primary click を消費して long-term peak を reset する。
    pub fn level_meter_stereo(
        &mut self,
        id: impl Hash,
        rect: Rect,
        l: f32,
        r: f32,
        ballistic: MeterBallistic,
        style: LevelMeterStyle,
    ) {
        let wid = WidgetId::ROOT.child((b"level_meter", &id));
        // 縦の dB→y 領域は `rect` から導出 (peak_readout 帯 + scale 上下 vpad)。 standalone では
        // この領域が widget rect 内で完結する。
        let content = meter_content_region(rect, style.scale.is_some(), style.peak_readout);
        self.meter_body(wid, rect, content, l, r, ballistic, &style);
    }

    /// メーター本体 (reset click 消費 + state 更新 + 背景 + L/R バー + 目盛り + readout) を描く。
    ///
    /// **縦の dB→y マッピングは `content` (呼び出し側が渡す)** に従う。 横レイアウト
    /// (`[tick | L | R | 数字]`) と背景 / readout / reset の hit-test は `rect` (列全体) に従う。
    /// `level_meter_stereo` は `content = meter_content_region(rect, ..)` を渡して自己完結し、
    /// `channel_fader_meter` は **fader と共有する region** を `content` に渡して画素整合させる
    /// (= dB→y 写像の single source of truth、 daw_01 #083)。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn meter_body(
        &mut self,
        wid: WidgetId,
        rect: Rect,
        content: Rect,
        l: f32,
        r: f32,
        ballistic: MeterBallistic,
        style: &LevelMeterStyle,
    ) {
        let reset_clicked = style.peak_readout && self.take_primary_press_in_rect(rect).is_some();

        // 1. state 更新 (L/R 個別)。 long_peak は L/R の最大到達 (readout 用)。
        let (l_disp, l_hold, r_disp, r_hold, long_peak) = {
            let state: &mut MeterState = self.widget_state(wid);
            if reset_clicked {
                state.long_peak = 0.0;
            }
            let (ld, lh) = state.l.update(l, ballistic, style.peak_hold_ms);
            let (rd, rh) = state.r.update(r, ballistic, style.peak_hold_ms);
            state.long_peak = state.long_peak.max(l.abs().min(2.0)).max(r.abs().min(2.0));
            (ld, lh, rd, rh, state.long_peak)
        };

        // active なら自動 redraw (idle 時は電力節約)。
        if l_disp > 1e-4 || r_disp > 1e-4 || l_hold > 1e-4 || r_hold > 1e-4 {
            self.request_redraw();
        }

        // 2. 横レイアウト: [tick ガター(左) | L バー | R バー | 数字ガター(右)]。
        let has_scale = style.scale.is_some();
        let (tick_g, num_g) = if has_scale {
            let total = (SCALE_TICK_GUTTER_W + SCALE_NUM_GUTTER_W).min((rect.w - 4.0).max(0.0));
            let tg = SCALE_TICK_GUTTER_W.min(total);
            (tg, total - tg)
        } else {
            (0.0, 0.0)
        };
        // bar_each は利用可能幅から導出 (floor は max(0.0))。 これで 2*bar_each + gap == bars_w と
        // なり、 bars_right == rect.x + tick_g + bars_w <= rect 右端 = 横方向も矩形内に収まる
        // (degenerate な極小 rect.w でもバーが矩形外に出ない)。
        let bars_w = (rect.w - tick_g - num_g).max(0.0);
        let bar_each = (bars_w - STEREO_BAR_GAP).max(0.0) * 0.5;
        let left_x = rect.x + tick_g;
        let right_x = left_x + bar_each + STEREO_BAR_GAP;
        let bars_right = right_x + bar_each;

        // 背景 (rect 全体)
        self.push_rect(RectCommand {
            rect,
            fill: style.bg,
            border: theme::BORDER,
            border_width: 1.0,
            radius: [2.0; 4],
            clip_rect: None,
        });

        // 3. L / R 色帯バー + peak hold 線 (同一カーブ、 content の dB→y を共有)。
        self.draw_meter_bar(content, left_x, bar_each, l_disp, l_hold, style);
        self.draw_meter_bar(content, right_x, bar_each, r_disp, r_hold, style);

        // 4. dB 目盛り (tick = L バー左 / 数字 = R バー右 / 0dB 線 = 両バー横断)。
        if let Some(scale) = style.scale {
            self.draw_meter_scale(content, rect, left_x, bars_right, scale, style);
        }

        // 5. peak readout (上端の専用帯)。
        if style.peak_readout {
            self.draw_meter_readout(rect, long_peak, style);
        }
    }

    /// 1 本の色帯バー (dB→高さ) + peak hold 線を `content` の `[bar_x, bar_x+bar_w]` に描く。
    /// dB→frac は `meter_frac` (scale 有 = curve / 無 = 線形) を使う。
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
        let frac = meter_frac(db, style);
        let bar_h = (content.h * frac).clamp(0.0, content.h);
        if bar_h > 0.0 {
            // 色帯は **dB** で決める (clip の 0dBFS 境界と一貫させる)。 frac 閾値で決めると非線形
            // カーブでは橙域 (frac>0.9 ≒ db>+0.5) が clip (db>0) に完全に隠れて死ぬため。
            // 緑 → 黄 (-18dB〜) → 橙 (-6dB〜) → 赤 (>0dBFS clip)。
            let color = if db > 0.0 {
                style.clip
            } else if db > -6.0 {
                style.high
            } else if db > -18.0 {
                style.mid
            } else {
                style.low
            };
            self.push_rect(RectCommand {
                rect: Rect {
                    x: bar_x,
                    y: content.y + (content.h - bar_h),
                    w: bar_w,
                    h: bar_h,
                },
                fill: color,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }

        // peak hold の細い線 (tick と同じく整数 px に丸めて crisp + 0dB 線と揃える)
        let hold_frac = meter_frac(linear_to_db(peak_hold_value), style);
        if hold_frac > 0.001 {
            let hold_y = (content.y + content.h - content.h * hold_frac).round();
            self.push_rect(RectCommand {
                rect: Rect { x: bar_x, y: hold_y - 1.0, w: bar_w, h: 2.0 },
                fill: style.peak_hold_color,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }
    }

    /// dB 目盛り: **tick は L バーの左**、 **数字は R バーの右**、 **0dB はバーを横切る横線**。
    /// y は `meter_frac` (curve) をバーと共有するので必ず一致する (SSoT)。 `content` は ty マッピング、
    /// `clip` は外側 rect (端ラベルが vpad 領域でも見える)。
    ///
    /// (M14 Phase 123 / daw_01 #099) 高さが縮むと dB 数字が縦に重なるため、 **実ピクセル位置基準の
    /// 貪欲間引き** ([`greedy_thin_scale`]) を行う: ① 全 `labels_db` の `ty` を解決 → ② 0dB を
    /// アンカーに上下へ `|Δty| >= min_gap` (= line_height 相当) の要素のみ採用 → ③ 採用分だけ
    /// tick + 数字を描画。 非線形カーブにより上は細かく下は粗く間引かれる (望ましい)。 tick も数字も
    /// 一緒に gate する (採用集合で両方を制御)。 **0dB は常にアンカーで採用** + **0dB 横断線は採用に
    /// 関係なく常時描画**。 既存 `has_label_room` (横余白) は維持し、 数字は「採用 AND has_label_room」、
    /// tick は採用集合のみ。
    fn draw_meter_scale(
        &mut self,
        content: Rect,
        clip: Rect,
        left_x: f32,
        bars_right: f32,
        scale: MeterScale,
        style: &LevelMeterStyle,
    ) {
        let tick_right = left_x - 1.0; // L バー左端の 1px 左
        let label_left = bars_right + 3.0; // R バー右端の 3px 右
        let has_label_room = (clip.x + clip.w - label_left) > SCALE_FONT_PX * 0.8;

        // ① 全ラベルの ty を実ピクセルで解決 (labels_db と同順 = 上→下)。
        let tys: Vec<f32> = scale
            .labels_db
            .iter()
            .map(|&db| (content.y + content.h - content.h * meter_frac(db, style)).round())
            .collect();

        // ② 0dB をアンカーに貪欲間引き。 min_gap は line_height (= SCALE_FONT_PX + 2) 相当。
        //    アンカーは |dB| 最小 (= 0dB に最も近い) ラベル。 0dB が無くても中央寄りを 1 つ残す。
        let anchor = scale
            .labels_db
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.abs().partial_cmp(&b.abs()).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map_or(0, |(i, _)| i);
        let adopted = greedy_thin_scale(&tys, anchor, SCALE_FONT_PX + 2.0);

        // ③ 採用分だけ描画。 0dB 横断線のみ採用に関係なく常時描画。
        for (i, &tick_db) in scale.labels_db.iter().enumerate() {
            let ty = tys[i];
            let is_zero = scale.emphasize_zero && tick_db.abs() < 1e-3;
            // 0dB は L/R 両バーを横切る 3px 横線 (高さ極小でも必ず残る)。
            if is_zero {
                self.push_rect(RectCommand {
                    rect: Rect { x: left_x, y: ty - 1.5, w: (bars_right - left_x).max(1.0), h: 3.0 },
                    fill: style.scale_zero_color,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: Some(clip),
                });
            }
            if !adopted[i] {
                continue;
            }
            // 左 tick (2px、 アンチエイリアスで消えない)。 0dB は色だけ明色。
            let tick_left = (tick_right - SCALE_TICK_LEN).max(clip.x);
            self.push_rect(RectCommand {
                rect: Rect { x: tick_left, y: ty - 1.0, w: (tick_right - tick_left).max(1.0), h: 2.0 },
                fill: if is_zero { style.scale_zero_color } else { style.scale_tick_color },
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: Some(clip),
            });
            if has_label_room {
                let label = format_scale_db(tick_db);
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
        let text_w = text.chars().count() as f32 * READOUT_FONT_PX * CHAR_W_RATIO;
        let chip_w = (text_w + 6.0).min(rect.w);
        let chip_x = rect.x + ((rect.w - chip_w).max(0.0)) * 0.5;
        self.push_rect(RectCommand {
            rect: Rect { x: chip_x, y: rect.y + 1.0, w: chip_w, h: READOUT_H },
            fill: theme::BACKDROP.with_alpha(0.78),
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

/// 線形 dB→frac (clean bar 用)。 `range = (lo, hi)`。
fn db_to_fraction(db: f32, range: (f32, f32)) -> f32 {
    let (lo, hi) = range;
    ((db - lo) / (hi - lo)).clamp(0.0, 1.0)
}

/// breakpoint piecewise-linear で dB→frac。 `curve` は db 降順 (高い dB が先頭)。 範囲外は端値に clamp。
fn curve_fraction(db: f32, curve: &[(f32, f32)]) -> f32 {
    let Some(&(top_db, top_f)) = curve.first() else {
        return 0.0;
    };
    if db >= top_db {
        return top_f;
    }
    for w in curve.windows(2) {
        let (hd, hf) = w[0];
        let (ld, lf) = w[1];
        if db <= hd && db >= ld {
            let span = hd - ld;
            let t = if span.abs() < 1e-9 { 0.0 } else { (db - ld) / span };
            return lf + t * (hf - lf);
        }
    }
    curve.last().map_or(0.0, |&(_, f)| f)
}

/// `draw_meter_scale` の縦間引き (純粋ロジック、 test 可能、 daw_01 #099)。
///
/// `tys` は各 dB ラベルの実ピクセル y (`labels_db` と同順 = 上→下)、 `anchor` は常時採用する
/// index (= 0dB に最も近いラベル)。 戻り値は各ラベルを描画するか。 anchor から上下それぞれへ
/// 走査し、 **直近採用要素**との `|Δty| >= min_gap` を満たす要素のみ採用する貪欲法。 採用集合内の
/// 隣接要素は構造上必ず `>= min_gap` 離れる (= 数字が縦に重ならない)。 非線形カーブ下では上
/// (0dB 近傍) は細かく、 下 (-60dB 近傍) は粗く間引かれる。 高さ極小でも anchor は必ず採用される。
fn greedy_thin_scale(tys: &[f32], anchor: usize, min_gap: f32) -> Vec<bool> {
    let mut adopted = vec![false; tys.len()];
    if tys.is_empty() {
        return adopted;
    }
    let anchor = anchor.min(tys.len() - 1);
    adopted[anchor] = true;
    // anchor より上 (index 降順)。
    let mut last = tys[anchor];
    for i in (0..anchor).rev() {
        if (tys[i] - last).abs() >= min_gap {
            adopted[i] = true;
            last = tys[i];
        }
    }
    // anchor より下 (index 昇順)。
    last = tys[anchor];
    for i in (anchor + 1)..tys.len() {
        if (tys[i] - last).abs() >= min_gap {
            adopted[i] = true;
            last = tys[i];
        }
    }
    adopted
}

/// メーターの dB→frac。 `scale = Some` なら非線形カーブ、 `None` (clean bar) なら線形 `db_range`。
/// バー・tick・数字・0dB 線・peak すべてこの 1 関数を通すので必ず一致する (SSoT)。
fn meter_frac(db: f32, style: &LevelMeterStyle) -> f32 {
    match style.scale {
        Some(scale) => curve_fraction(db, scale.curve),
        None => db_to_fraction(db, style.db_range),
    }
}

/// `rect` から「縦の dB→y 領域」 (frac 0..1 を写す content rect) を導出する。
///
/// `peak_readout` 時は上端に `READOUT_BAND_H` の専用帯、 `has_scale` 時は端ラベルが切れない
/// `SCALE_VPAD` の上下余白を確保する。 `y(frac) = content.y + content.h * (1.0 - frac)`。
/// `level_meter_stereo` と `channel_fader_meter` がこの 1 関数から領域を得るので、 同じ `rect`
/// (= 同じ y / h) を渡せば fader ハンドルと meter バー・目盛りが必ず画素整合する (daw_01 #083)。
/// degenerate sizing でも content は rect 内に clamp される。
pub(crate) fn meter_content_region(rect: Rect, has_scale: bool, peak_readout: bool) -> Rect {
    let vpad = if has_scale { SCALE_VPAD } else { 0.0 };
    let band = if peak_readout { READOUT_BAND_H } else { 0.0 };
    let content_top = (rect.y + band + vpad).clamp(rect.y, rect.y + rect.h);
    let content_bottom = (rect.y + rect.h - vpad).clamp(content_top, rect.y + rect.h);
    Rect { x: rect.x, y: content_top, w: rect.w, h: content_bottom - content_top }
}

/// 目盛りラベル文字列。 Ableton Live と同じく **符号なしの絶対値**整数 (`"6"` / `"0"` / `"60"`)。
/// 正負は tick の位置 (0dB の上下) で読み取る。
fn format_scale_db(db: f32) -> String {
    format!("{}", db.abs().round() as i32)
}

/// peak readout 文字列と over フラグ。 無音なら `("-inf", false)`、 else `("{db:.1}", db >= 0.0)`。
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
    }

    #[test]
    fn meter_scale_default() {
        let s = MeterScale::default();
        assert_eq!(
            s.labels_db,
            &[6.0, 0.0, -6.0, -12.0, -18.0, -24.0, -30.0, -36.0, -42.0, -48.0, -54.0, -60.0]
        );
        assert_eq!(s.curve.first().unwrap().0, 6.0);
        assert_eq!(s.curve.last().unwrap().0, -60.0);
        assert!(s.emphasize_zero);
    }

    /// r.md #11: mixer fader / arrangement volume band が振幅を frac 空間で
    /// 扱う SSoT。 amp → frac → amp が round-trip し、 unity / +6dB 上端 / 無音の
    /// 境界が期待どおりであること。
    #[test]
    fn amp_frac_roundtrip_and_boundaries() {
        let s = MeterScale::default();
        // unity (0 dB): amp 1.0 round-trips.
        let f_unity = s.amp_to_frac(1.0);
        assert!((s.frac_to_amp(f_unity) - 1.0).abs() < 1e-3, "unity round-trip");
        // +6dB 上端: amp 2.0 (= +6.02 dB) は curve 上端 (frac 1.0) に張り付く。
        assert!(s.amp_to_frac(2.0) >= 0.999, "+6dB maps to the fader top");
        // 無音: amp 0 → frac 0 → amp 0 (下端の特別扱い)。
        assert_eq!(s.amp_to_frac(0.0), 0.0);
        assert_eq!(s.frac_to_amp(0.0), 0.0);
        // 中間 (-6dB, amp 0.5) も round-trip する。
        let f = s.amp_to_frac(0.5);
        assert!((s.frac_to_amp(f) - 0.5).abs() < 1e-3, "-6dB round-trip");
    }

    /// 非線形カーブは breakpoint 上で表通り、 中間は線形補間。 範囲外は端値 clamp。
    #[test]
    fn curve_fraction_breakpoints_and_interp() {
        let c = DEFAULT_CURVE;
        assert!((curve_fraction(6.0, c) - 1.00).abs() < 1e-4);
        assert!((curve_fraction(0.0, c) - 0.89).abs() < 1e-4);
        assert!((curve_fraction(-30.0, c) - 0.40).abs() < 1e-4);
        assert!((curve_fraction(-60.0, c) - 0.00).abs() < 1e-4);
        // 中間: -3dB は 0(0.89) と -6(0.79) の中点 = 0.84
        assert!((curve_fraction(-3.0, c) - 0.84).abs() < 1e-4);
        // 範囲外
        assert!((curve_fraction(20.0, c) - 1.00).abs() < 1e-4);
        assert!((curve_fraction(-200.0, c) - 0.00).abs() < 1e-4);
        // top-weighted: 上の 6dB 間隔 (+6→0 = 0.11) > 下の 6dB 間隔 (-54→-60 = 0.07)
        let top = curve_fraction(6.0, c) - curve_fraction(0.0, c);
        let bot = curve_fraction(-54.0, c) - curve_fraction(-60.0, c);
        assert!(top > bot, "top-weighted: {top} > {bot}");
    }

    /// `MeterScale::db_to_frac` / `frac_to_db` の往復と端値。
    #[test]
    fn meter_scale_db_to_frac_and_frac_to_db_roundtrip() {
        let s = MeterScale::default();
        // breakpoint 上では完全一致 (誤差 < 1e-3 dB)
        for &(db, frac) in DEFAULT_CURVE {
            assert!((s.db_to_frac(db) - frac).abs() < 1e-4, "db_to_frac({db}) failed");
            assert!((s.frac_to_db(frac) - db).abs() < 1e-3, "frac_to_db({frac}) failed");
        }
        // 往復: 任意の中間値 db → frac → db
        for db in [-3.0f32, -15.0, -45.0] {
            let frac = s.db_to_frac(db);
            let recovered = s.frac_to_db(frac);
            assert!((recovered - db).abs() < 1e-3, "roundtrip failed at db={db}: got {recovered}");
        }
        // 端値 clamp
        assert_eq!(s.db_to_frac(100.0), 1.0);
        assert_eq!(s.db_to_frac(-200.0), 0.0);
        assert_eq!(s.frac_to_db(2.0), 6.0);  // frac >= top_f → top_db
        assert_eq!(s.frac_to_db(-1.0), -60.0); // frac < bot_f → bot_db
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
        let (t, over) = format_peak_readout(0.0);
        assert_eq!(t, "-inf");
        assert!(!over);
        let (t, over) = format_peak_readout(1.0);
        assert_eq!(t, "0.0");
        assert!(over);
        let (t, over) = format_peak_readout(0.5);
        assert_eq!(t, "-6.0");
        assert!(!over);
    }

    fn run_stereo(
        host: &mut UiHost<()>,
        rect: Rect,
        l: f32,
        r: f32,
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
                ui.level_meter_stereo("m", rect, l, r, MeterBallistic::Peak, style);
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

    /// widget と同じレイアウト計算。
    fn stereo_geom(rect: Rect) -> (f32, f32, f32) {
        let total = (SCALE_TICK_GUTTER_W + SCALE_NUM_GUTTER_W).min((rect.w - 4.0).max(0.0));
        let tick_g = SCALE_TICK_GUTTER_W.min(total);
        let num_g = total - tick_g;
        let bars_w = (rect.w - tick_g - num_g).max(2.0);
        let bar_each = ((bars_w - STEREO_BAR_GAP) * 0.5).max(1.0);
        let left_x = rect.x + tick_g;
        let bars_right = left_x + bar_each + STEREO_BAR_GAP + bar_each;
        (left_x, bar_each, bars_right)
    }

    /// default (scale=None / peak_readout=false) = clean bar 2 本 = テキストなし。
    #[test]
    fn clean_bar_has_no_text() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 10.0, h: 200.0 };
        let scene = run_stereo(&mut host, rect, 0.5, 0.5, LevelMeterStyle::default(), PointerFrame::default());
        assert_eq!(scene.glyph_count(), 0, "clean bar はテキストを描かない");
    }

    /// L/R が異なるレベルなら 2 本の独立したバー rect が出る。
    #[test]
    fn stereo_draws_two_bars() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 12.0, h: 200.0 };
        // L=1.0 (frac 1.0)、 R=0.5 (frac 中) で高さが違う 2 本
        let scene = run_stereo(&mut host, rect, 1.0, 0.5, LevelMeterStyle::default(), PointerFrame::default());
        // バーのみを取る: bg (w == rect.w) と peak-hold 線 (h == 2.0) を除外。 h > 2.5 でバー本体だけ。
        let bars: Vec<_> = scene
            .iter_rects()
            .filter(|r| r.rect.w < rect.w && r.rect.h > 2.5 && r.rect.h < rect.h)
            .collect();
        assert_eq!(bars.len(), 2, "L/R ちょうど 2 本のバーが描かれる (got {})", bars.len());
        assert!(
            (bars[0].rect.h - bars[1].rect.h).abs() > 1.0,
            "L/R で高さが違う (got {:?})",
            bars.iter().map(|b| b.rect.h).collect::<Vec<_>>()
        );
    }

    /// scale=Some: tick が L バーの左、 数字が R バーの右、 0dB 線が両バーを横断。
    #[test]
    fn scale_layout_tick_left_numbers_right_zero_line() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 40.0, h: 240.0 };
        let style = LevelMeterStyle { scale: Some(MeterScale::default()), ..Default::default() };
        let scene = run_stereo(&mut host, rect, 0.0, 0.0, style, PointerFrame::default());
        let (left_x, _bar_each, bars_right) = stereo_geom(rect);
        // tick (h=2) が L バーの左
        assert!(
            scene.iter_rects().any(|r| (r.rect.h - 2.0).abs() < 0.01 && r.rect.x + r.rect.w <= left_x + 0.5),
            "tick が L バーの左 (<= {left_x})"
        );
        // 数字が R バーの右
        assert!(
            scene.iter_glyphs().any(|g| g.left >= bars_right - 0.5),
            "数字が R バーの右 (>= {bars_right})"
        );
        // 0dB 横線 (h=3) が両バー幅 (bars_right - left_x) を横切る
        let span = bars_right - left_x;
        assert!(
            scene.iter_rects().any(|r| {
                (r.rect.h - 3.0).abs() < 0.01
                    && (r.rect.x - left_x).abs() < 0.5
                    && (r.rect.w - span).abs() < 0.5
            }),
            "0dB 横線が L/R 両バー (幅 {span}) を横切る"
        );
        // 数字ラベルは符号なし
        let labels: Vec<&str> = scene.iter_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(labels.contains(&"0"));
        assert!(labels.contains(&"6"));
        assert!(labels.contains(&"60"));
    }

    /// scale ラベルが rect の上下端からはみ出さない (端ラベルも全体が見える)。
    #[test]
    fn scale_labels_stay_within_rect() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 40.0, h: 240.0 };
        let style = LevelMeterStyle { scale: Some(MeterScale::default()), ..Default::default() };
        let scene = run_stereo(&mut host, rect, 0.0, 0.0, style, PointerFrame::default());
        for g in scene.iter_glyphs() {
            assert!(
                g.top >= rect.y - 0.01 && g.top <= rect.y + rect.h - SCALE_FONT_PX + 0.01,
                "ラベル {:?} top={} が rect 内",
                g.text,
                g.top
            );
        }
    }

    /// peak_readout の数値が rect 内に収まる + click で reset。
    #[test]
    fn peak_readout_within_rect_and_resets_on_click() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 5.0, y: 0.0, w: 40.0, h: 200.0 };
        let style = LevelMeterStyle { peak_readout: true, ..Default::default() };
        // L=1.0 → long_peak 1.0 → "0.0"
        let scene = run_stereo(&mut host, rect, 1.0, 0.3, style, PointerFrame::default());
        let g = scene.iter_glyphs().find(|g| g.text.as_ref() == "0.0").expect("readout 0.0");
        let text_w = g.text.chars().count() as f32 * READOUT_FONT_PX * CHAR_W_RATIO;
        assert!(g.left >= rect.x - 0.01 && g.left + text_w <= rect.x + rect.w + 0.01, "readout が rect 内");
        // click で reset → "-inf"
        let scene = run_stereo(&mut host, rect, 0.0, 0.0, style, press_at((25.0, 100.0)));
        assert!(
            scene.iter_glyphs().any(|g| g.text.as_ref() == "-inf"),
            "click reset 後は -inf"
        );
    }

    // ============================================================
    // (M14 Phase 123 / daw_01 #099) 縦間引き (greedy_thin_scale + frame レベル)
    // ============================================================

    #[test]
    fn greedy_thin_scale_keeps_anchor_and_spaces_min_gap() {
        // 等間隔 5px の 10 要素、 anchor=中央 (idx 4)、 min_gap=11。
        let tys: Vec<f32> = (0..10).map(|i| i as f32 * 5.0).collect();
        let adopted = greedy_thin_scale(&tys, 4, 11.0);
        assert!(adopted[4], "anchor は必ず採用");
        // 採用要素の ty 差は全て >= min_gap (= 縦重なり無し)。
        let kept: Vec<f32> =
            tys.iter().zip(adopted.iter()).filter_map(|(&t, &a)| a.then_some(t)).collect();
        assert_eq!(kept, vec![5.0, 20.0, 35.0], "5px 刻みでは 3 つ飛ばしで採用");
        for w in kept.windows(2) {
            assert!(w[1] - w[0] >= 11.0 - 1e-3, "採用間隔 {} >= min_gap", w[1] - w[0]);
        }
    }

    #[test]
    fn greedy_thin_scale_empty_and_single_and_anchor_clamp() {
        assert!(greedy_thin_scale(&[], 0, 11.0).is_empty());
        assert_eq!(greedy_thin_scale(&[7.0], 0, 11.0), vec![true]);
        // anchor が範囲外でも clamp して anchor 採用。
        assert_eq!(greedy_thin_scale(&[7.0], 5, 11.0), vec![true]);
    }

    /// scale=Some を `rect.h` を変えて render し Scene を返す (silence = peak-hold 線なし)。
    fn scale_scene(h: f32) -> Scene {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let rect = Rect { x: 0.0, y: 0.0, w: 40.0, h };
        let style = LevelMeterStyle { scale: Some(MeterScale::default()), ..Default::default() };
        run_stereo(&mut host, rect, 0.0, 0.0, style, PointerFrame::default())
    }

    #[test]
    fn scale_thins_when_short_no_label_overlap() {
        // 小 rect.h=60 → 採用ラベルが減り、 隣接ラベル top 差は line_height (= 11px) 以上。
        let scene = scale_scene(60.0);
        let mut tops: Vec<f32> = scene.iter_glyphs().map(|g| g.top).collect();
        tops.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!(tops.len() >= 2, "少なくとも 2 ラベルは残る: got {}", tops.len());
        assert!(tops.len() < 12, "12 全ては出ない (間引かれる): got {}", tops.len());
        for w in tops.windows(2) {
            assert!(
                w[1] - w[0] >= (SCALE_FONT_PX + 2.0) - 0.01,
                "隣接ラベル top 差 {} >= min_gap",
                w[1] - w[0]
            );
        }
    }

    #[test]
    fn scale_tick_count_equals_label_count() {
        // tick (h≈2、 L バー左) と数字は同じ採用集合で gate されるので本数一致。
        let scene = scale_scene(60.0);
        let (left_x, _, _) = stereo_geom(Rect { x: 0.0, y: 0.0, w: 40.0, h: 60.0 });
        let ticks = scene
            .iter_rects()
            .filter(|r| (r.rect.h - 2.0).abs() < 0.01 && r.rect.x + r.rect.w <= left_x + 0.5)
            .count();
        let labels = scene.iter_glyphs().count();
        assert_eq!(ticks, labels, "tick {ticks} == label {labels}");
    }

    #[test]
    fn scale_keeps_zero_label_when_tiny() {
        // 極小 rect.h=24 → ほぼ 0dB のみ採用。 "0" は必ず残る。
        let scene = scale_scene(24.0);
        let labels: Vec<&str> = scene.iter_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(labels.contains(&"0"), "極小高さでも 0dB ラベルは残る: got {labels:?}");
    }

    #[test]
    fn scale_shows_all_labels_when_tall() {
        // 大 rect.h=400 → 全 12 ラベル (回帰)。
        let scene = scale_scene(400.0);
        assert_eq!(scene.iter_glyphs().count(), 12, "高さ十分なら全 12 ラベル");
    }
}
