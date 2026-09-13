//! EQ / Tone EQ の合成カーブ (+ スペクトラム) と、カーブ上の点の座標 (§10.4)。
//!
//! 応答は `common::dsp` の係数と振幅応答から描く — daw_audio が音に使うのと同じ関数なので、
//! 画面の線と実際の音が食い違わない (GUI 側に DSP を複製しない)。
//!
//! 描画 ([`draw_eq_curve`]) と点の位置 ([`curve_handles`]) と点ドラッグの逆写像 (Rack の
//! `eq_graph`) は、すべて同じ [`CurveAxes`] を通す — 片方だけ写像を持つと点がカーブから浮く。

use std::sync::Arc;

use common::dsp::{Biquad, EQ_STAGES, eq_magnitude_db, eq_stages, tone_eq_magnitude_db, tone_eq_stages};
use common::model::{EqBand, EqSettings, NativeParams, TONE_EQ_LIMIT_DB, ToneEqBand, ToneEqSettings};
use daw_ui_core::{SpectrumStyle, Ui};
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::app::AppData;
use crate::master_meter::spectrum::{F_MAX, F_MIN};

/// EQ の縦軸 (±dB)。ゲインの上下限 ±15 dB に、HP/LP の裾とバンドの重なりが見える余白を足す。
const CHANNEL_DB_RANGE: f32 = 18.0;
/// 応答を評価するサンプリング周波数。**音の実 SR ではない** — 描くのは 20Hz〜20kHz の形で、
/// 係数の bilinear warping の差が出るのは Nyquist 付近だけ。
const CURVE_SR: f32 = 48_000.0;
/// OFF (bypass) のカーブとスペクトラムの不透明度の倍率。形は変えずに薄くする。
const INACTIVE_ALPHA: f32 = 0.45;
/// 背後のスペクトラム (その EQ を通った後の音、Q14) の塗りの不透明度。
const SPECTRUM_ALPHA: f32 = 0.15;
/// 縦の目盛り線を引く周波数。
const GRID_HZ: [f32; 3] = [100.0, 1_000.0, 10_000.0];

/// 描くカーブの値。
#[derive(Debug, Clone, Copy)]
pub enum EqCurveSource<'a> {
    /// 6 バンド EQ (HP/LP を含む)。
    Channel(&'a EqSettings),
    /// 固定周波数 3 バンドの Tone EQ。
    Tone(&'a ToneEqSettings),
}

impl<'a> EqCurveSource<'a> {
    /// device の値からカーブの値を取る (`None` = カーブを持たない種類)。
    #[must_use]
    pub fn from_params(params: &'a NativeParams) -> Option<Self> {
        match params {
            NativeParams::Eq(eq) => Some(Self::Channel(eq)),
            NativeParams::ToneEq(eq) => Some(Self::Tone(eq)),
            NativeParams::Comp(_) | NativeParams::BusComp(_) => None,
        }
    }
}

/// カーブの座標系。横 = 周波数 (対数)、縦 = ゲイン (線形、0 dB が縦の中央)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurveAxes {
    pub f_min: f32,
    pub f_max: f32,
    /// 縦軸の片側の幅 (dB)。上端 = `+db_range`、下端 = `-db_range`。
    pub db_range: f32,
}

impl CurveAxes {
    /// 周波数の範囲はマスターのスペクトラム (`master_meter::spectrum::F_MIN/F_MAX`) が SSoT
    /// (背後に重ねるスペクトラムと横軸を揃える)。縦は EQ = ±18 dB、Tone EQ = ±`TONE_EQ_LIMIT_DB`。
    #[must_use]
    pub fn for_source(src: &EqCurveSource<'_>) -> Self {
        let db_range = match src {
            EqCurveSource::Channel(_) => CHANNEL_DB_RANGE,
            EqCurveSource::Tone(_) => TONE_EQ_LIMIT_DB,
        };
        Self { f_min: F_MIN, f_max: F_MAX, db_range }
    }

    /// 周波数 (Hz) → `rect` 内の x。範囲外は端に丸める。
    #[must_use]
    pub fn freq_to_x(&self, rect: Rect, hz: f32) -> f32 {
        let t = (hz.max(self.f_min) / self.f_min).ln() / (self.f_max / self.f_min).ln();
        rect.x + rect.w * t.clamp(0.0, 1.0)
    }

    /// `rect` 内の x → 周波数 (Hz)。[`Self::freq_to_x`] の逆。
    #[must_use]
    pub fn x_to_freq(&self, rect: Rect, x: f32) -> f32 {
        if !x.is_finite() || rect.w <= 0.0 {
            return self.f_min;
        }
        let t = ((x - rect.x) / rect.w).clamp(0.0, 1.0);
        self.f_min * (self.f_max / self.f_min).powf(t)
    }

    /// ゲイン (dB) → `rect` 内の y。範囲外は端に丸める (上下 1px は線が切れないよう空ける)。
    #[must_use]
    pub fn db_to_y(&self, rect: Rect, db: f32) -> f32 {
        let t = if db.is_finite() && self.db_range > 0.0 { (db / self.db_range).clamp(-1.0, 1.0) } else { 0.0 };
        rect.y + rect.h * 0.5 - t * half_span(rect)
    }

    /// `rect` 内の y → ゲイン (dB)。[`Self::db_to_y`] の逆。
    #[must_use]
    pub fn y_to_db(&self, rect: Rect, y: f32) -> f32 {
        let span = half_span(rect);
        if !y.is_finite() || span <= 0.0 {
            return 0.0;
        }
        ((rect.y + rect.h * 0.5 - y) / span).clamp(-1.0, 1.0) * self.db_range
    }
}

/// 0 dB 線から上端 / 下端までの px。
fn half_span(rect: Rect) -> f32 {
    (rect.h * 0.5 - 1.0).max(0.0)
}

/// カーブの見せ方。
#[derive(Debug, Clone, Copy)]
pub struct CurveLook<'a> {
    /// device が効いているか。`false` は形を変えずに線とスペクトラムを薄くする。
    pub active: bool,
    /// 背後に薄く描くスペクトラム (`SpectrumAnalyzer` の `SPECTRUM_BANDS` 対数帯の dB、
    /// `AppData::device_spectrum_db`)。`None` なら描かない。
    pub spectrum_db: Option<&'a [f32]>,
}

/// 合成カーブを描く。重ね順はスペクトラム → 目盛り線 (100 / 1k / 10k Hz と 0 dB) → カーブ。
/// 背景は描かない (面の色は呼び出し側の文脈で決まる: Mixer 帯の井戸 / Par / 行)。
pub fn draw_eq_curve(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    rect: Rect,
    src: &EqCurveSource<'_>,
    look: &CurveLook<'_>,
) {
    if rect.w < 2.0 || rect.h < 2.0 {
        return;
    }
    let axes = CurveAxes::for_source(src);
    let color = app.theme.daw.strip_eq_curve;
    let alpha = if look.active { 1.0 } else { INACTIVE_ALPHA };

    if let Some(bands) = look.spectrum_db {
        // 塗りだけを使う (枠 / グリッド / ラベル / 輪郭は透明)。レンジはマスターのスペクトラムと
        // 同じ設定 (device の解析器も同じ `MeterSettings` で動く)。
        let style = SpectrumStyle {
            bg: Color::TRANSPARENT,
            border: Color::TRANSPARENT,
            fill: color.with_alpha(color.a * SPECTRUM_ALPHA * alpha),
            outline: Color::TRANSPARENT,
            hold: Color::TRANSPARENT,
            grid: Color::TRANSPARENT,
            label: Color::TRANSPARENT,
            floor_db: -app.ui_prefs.meter_settings.spectrum_range_db,
            f_min: axes.f_min,
            f_max: axes.f_max,
            show_labels: false,
        };
        ui.spectrum_analyzer("native_eq_spectrum", rect, bands, &[], &style);
    }
    draw_grid(app, ui, rect, &axes);

    let stages = Stages::of(src);
    let points = ((rect.w * 0.5).round() as usize).clamp(24, 256);
    let line = color.with_alpha(color.a * alpha);
    let mut segs: Vec<LineSegment> = Vec::with_capacity(points);
    let mut prev: Option<[f32; 2]> = None;
    for i in 0..points {
        let x = rect.x + rect.w * i as f32 / (points - 1) as f32;
        let y = axes.db_to_y(rect, stages.magnitude_db(axes.x_to_freq(rect, x)));
        if let Some(a) = prev {
            segs.push(LineSegment { a, b: [x, y], color: line });
        }
        prev = Some([x, y]);
    }
    ui.push_lines(LineBatch { segments: Arc::from(segs), line_width_px: 1.0, clip_rect: Some(rect) });
}

/// 100 / 1k / 10k Hz の縦線と、カーブが上下どちらへ振れているかを読む 0 dB 線。
fn draw_grid(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect, axes: &CurveAxes) {
    let p = &app.theme.core;
    let line = |ui: &mut Ui<'_, AppData>, r: Rect, fill: Color| {
        ui.push_rect(RectCommand {
            rect: r,
            fill,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: Some(rect),
        });
    };
    for hz in GRID_HZ {
        let x = axes.freq_to_x(rect, hz).round();
        line(ui, Rect { x, y: rect.y, w: 1.0, h: rect.h }, p.grid_line);
    }
    line(ui, Rect { x: rect.x, y: axes.db_to_y(rect, 0.0), w: rect.w, h: 1.0 }, p.border);
}

/// カーブの係数 (1 フレームに 1 回だけ組む)。
enum Stages {
    Channel([Biquad; EQ_STAGES]),
    Tone([Biquad; 3]),
}

impl Stages {
    fn of(src: &EqCurveSource<'_>) -> Self {
        match *src {
            EqCurveSource::Channel(eq) => Self::Channel(eq_stages(eq, CURVE_SR)),
            EqCurveSource::Tone(eq) => Self::Tone(tone_eq_stages(eq, CURVE_SR)),
        }
    }

    fn magnitude_db(&self, hz: f32) -> f32 {
        match self {
            Self::Channel(s) => eq_magnitude_db(s, CURVE_SR, hz),
            Self::Tone(s) => tone_eq_magnitude_db(s, CURVE_SR, hz),
        }
    }
}

/// カーブ上の点が指すバンド (widget id の鍵にもなる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CurveBand {
    Eq(EqBand),
    Tone(ToneEqBand),
}

/// 点を動かせる向き (Q13)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleAxes {
    /// 左右 = Freq、上下 = Gain (LF / LMF / HMF / HF)。
    Both,
    /// 0 dB 線上を左右だけ (HP / LP)。
    Horizontal,
    /// 固定周波数で上下だけ (Tone EQ)。
    Vertical,
}

/// カーブ上の点 1 個。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurveHandle {
    pub band: CurveBand,
    /// `rect` 内の座標 (Freq → x、Gain → y。ゲインを持たない HP / LP は 0 dB 線上)。
    pub pos: (f32, f32),
    pub axes: HandleAxes,
    /// ホイールで Q を変えられるか (Q つまみを持つ LMF / HMF だけ)。
    pub wheel_q: bool,
    /// バンドが ON か (Tone EQ はバンドの ON/OFF を持たないので常に `true`)。
    pub on: bool,
}

/// カーブ上の点。EQ は `EqBand::BY_FREQ` 順 (Par の列順)、Tone EQ は `ToneEqBand::ALL` 順。
#[must_use]
pub fn curve_handles(src: &EqCurveSource<'_>, axes: &CurveAxes, rect: Rect) -> Vec<CurveHandle> {
    match *src {
        EqCurveSource::Channel(eq) => EqBand::BY_FREQ
            .into_iter()
            .map(|band| {
                let b = eq.band(band);
                let gain = if band.has_gain() { b.gain_db } else { 0.0 };
                CurveHandle {
                    band: CurveBand::Eq(band),
                    pos: (axes.freq_to_x(rect, b.freq_hz), axes.db_to_y(rect, gain)),
                    axes: if band.has_gain() { HandleAxes::Both } else { HandleAxes::Horizontal },
                    wheel_q: band.has_q_knob(),
                    on: b.on,
                }
            })
            .collect(),
        EqCurveSource::Tone(eq) => ToneEqBand::ALL
            .into_iter()
            .map(|band| CurveHandle {
                band: CurveBand::Tone(band),
                pos: (axes.freq_to_x(rect, band.freq_hz()), axes.db_to_y(rect, eq.gain_db(band))),
                axes: HandleAxes::Vertical,
                wheel_q: false,
                on: true,
            })
            .collect(),
    }
}
