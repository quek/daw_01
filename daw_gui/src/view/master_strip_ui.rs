//! マスターパネル内のマスターストリップ UI (バスコンプ + トーン EQ + リミッター)。
//!
//! 設計正本は [docs/plan_master_strip.md](../../../docs/plan_master_strip.md) §3。
//! MASTER セクションの **数値欄の列**を上下に割った上側に置く (LU バーとフェーダーは
//! 全高のまま = コンプの GR とフェーダーが必ず並んで見える)。
//!
//! ```text
//! | COMP    ( 針メーター )  |   ← 上から信号順
//! | Thr Rat Atk Rel Gain   |
//! |------------------------|
//! | EQ    ~~ curve ~~      |
//! | Lo   LoMid   Hi        |
//! |------------------------|
//! | LIM  ########   -1.0   |
//! ```
//!
//! 値の持ち主は master の **組み込み Bus Comp / Tone EQ** (`Device::Native`) とフェーダー後の
//! `Song::master_limiter` (r.md #129)。ON/OFF ボタンは置かない — カーソルを乗せて `Q`
//! (通常 ch のストリップと同じ作法)。

use std::sync::Arc;

use common::automation::{norm_to_plain, plain_to_norm};
use common::dsp::{tone_eq_magnitude_db, tone_eq_stages};
use common::model::{
    AutomationTarget, BusCompParam, GR_METER_RANGE_DB, MASTER_TRACK_ID, MasterLimiterParam, NativeDevice,
    NativeKind, NativeParamId, NativeParams, TONE_EQ_LIMIT_DB, ToneEqBand,
};
use daw_ui_core::{Edit, KnobStyle, NeedleMeterStyle, NeedleScale, Ui};
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::app::{AppData, AppEvent};
use crate::automation_value::automation_value_display;
use crate::event_device::DeviceEvent;
use crate::event_native::{MasterLimiterEdit, NativeEdit};
use crate::handler::bypass_target::BypassTarget;

/// 針式 GR メーターの高さ (px)。マスターで最初に見る物なので、ノブ 2 行ぶんより
/// 大きく取る (文字盤の余白は widget 側で詰めてある)。
const METER_H: f32 = 72.0;
/// ノブ 1 個の直径 (px)。通常 ch のストリップと揃える。
const KNOB: f32 = 20.0;
/// ノブ同士の間隔。
const KNOB_GAP: f32 = 6.0;
/// ラベル / hover 読み出し行の高さ (= [`LABEL_FONT`] の行高)。
const LABEL_H: f32 = 13.0;
/// ラベル / hover 読み出しの font size。マスターパネルは strip より幅があるので
/// 通常 ch (10px) より 1 段大きい。
const LABEL_FONT: f32 = 11.0;
/// ノブ行の高さ (ラベル + ノブ + 隙間)。
const ROW_H: f32 = LABEL_H + KNOB + 3.0;
/// EQ カーブの高さ。
const CURVE_H: f32 = 40.0;
/// リミッターの GR セグメント行の高さ。
const LIM_BAR_H: f32 = 10.0;
/// ブロック間の隙間。
const BLOCK_GAP: f32 = 4.0;

/// Comp ブロックの高さ (針メーター + ノブ 2 行)。
const COMP_H: f32 = METER_H + ROW_H * 2.0;
/// EQ ブロックの高さ (カーブ + ノブ 1 行)。
const EQ_H: f32 = CURVE_H + ROW_H;
/// リミッターブロックの高さ (セグメント + ノブ 1 行)。
const LIM_H: f32 = LIM_BAR_H + ROW_H;

/// カーブの横軸 (Hz) と縦軸 (±dB)。トーン EQ は ±6dB なので縦軸もそれに合わせる。
const CURVE_F_MIN: f32 = 20.0;
/// [`CURVE_F_MIN`] の対。
const CURVE_F_MAX: f32 = 20_000.0;
/// カーブのサンプル点数。
const CURVE_POINTS: usize = 64;
/// カーブ描画に使うサンプリング周波数 (可聴域の形は実 SR に依らない)。
const CURVE_SR: f32 = 48_000.0;

/// リミッターの GR セグメント数 (1 セグメント = 1dB、Mixbus と同じ粒度)。
const LIM_SEGMENTS: usize = 12;

/// このストリップが要求する高さ (px)。`master_panel` がラウドネス数値欄との
/// 上下分割に使う。
#[must_use]
pub fn desired_height() -> f32 {
    COMP_H + EQ_H + LIM_H + BLOCK_GAP * 2.0
}

/// マスターストリップを描く。`rect` は数値欄の列の上側 (高さは caller が決める)。
///
/// 高さが足りないときは **下のブロックから諦める** (Comp → EQ → LIM の優先順)。
/// コンプの GR が最後まで残るのは、マスターで最初に見たいのがそれだから。
pub fn draw<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, rect: Rect) {
    let song = app.cur.song_doc.song();
    let bus = song.builtin_native(MASTER_TRACK_ID, NativeKind::BusComp).copied();
    let tone = song.builtin_native(MASTER_TRACK_ID, NativeKind::ToneEq).copied();
    let mut y = rect.y;
    let mut hovered: Option<BypassTarget> = None;
    let ptr = ui.pointer().pos;
    let hit = |r: Rect, t: Option<BypassTarget>, hovered: &mut Option<BypassTarget>| {
        if ptr.is_some_and(|(px, py)| r.contains(px, py)) && t.is_some() {
            *hovered = t;
        }
    };

    if y + COMP_H <= rect.y + rect.h {
        let block = Rect { y, h: COMP_H, ..rect };
        if let Some(bus) = bus {
            draw_comp(app, ui, block, &bus);
        }
        hit(block, bus.map(|d| BypassTarget::Device(d.id)), &mut hovered);
        y += COMP_H + BLOCK_GAP;
    }
    if y + EQ_H <= rect.y + rect.h {
        let block = Rect { y, h: EQ_H, ..rect };
        if let Some(tone) = tone {
            draw_eq(app, ui, block, &tone);
        }
        hit(block, tone.map(|d| BypassTarget::Device(d.id)), &mut hovered);
        y += EQ_H + BLOCK_GAP;
    }
    if y + LIM_H <= rect.y + rect.h {
        let block = Rect { y, h: LIM_H, ..rect };
        draw_limiter(app, ui, block);
        hit(block, Some(BypassTarget::MasterLimiter), &mut hovered);
    }

    // Q キー (= 「カーソル直下のものを無効化」) の対象。master パネルは毎フレーム
    // 描かれるので、ここの値が古くなることはない。
    if app.cur.peph.master_panel_hovered != hovered {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.cur.peph.master_panel_hovered = hovered;
        }));
    }
}

/// ブロックの「面」の色。**ON は窪んだ井戸 (`window_bg`) / OFF はパネルと同じ面
/// (`panel_raised`)** — 通常 ch の常設帯と同じ規則で、効いているかどうかを線や針の
/// 色だけでなく面の明暗でも読ませる (Q で切り替えた瞬間に一目で分かる)。
fn block_bg(app: &AppData, on: bool) -> Color {
    let p = &app.theme.core;
    if on { p.window_bg } else { p.panel_raised }
}

/// OFF のブロックのノブ行を半透明のパネル色で覆って沈める (= バイパスされた
/// プラグインがグレーアウトする DAW の作法)。ノブは触れるまま — 触ると自動で ON
/// になるので、沈んでいても操作の入口として残す。
fn dim_if_off(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect, on: bool) {
    if on {
        return;
    }
    ui.push_rect(RectCommand {
        rect,
        fill: Color { a: 0.6, ..app.theme.core.panel_raised },
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [2.0; 4],
        clip_rect: None,
    });
}

// ---------------------------------------------------------------------------
// Comp
// ---------------------------------------------------------------------------

fn draw_comp<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, rect: Rect, bus: &NativeDevice) {
    let p = &app.theme.core;
    let on = !bus.bypassed;
    // ---- 針式 GR メーター ----
    let meter = Rect { h: METER_H - 2.0, ..rect };
    let gr = app.cur.transport.native_gr.get(bus.id);
    let style = NeedleMeterStyle {
        bg: block_bg(app, on),
        needle: if on { app.theme.daw.strip_gr } else { p.text_dim },
        ..NeedleMeterStyle::from_palette(p)
    };
    ui.needle_meter(
        "master_comp_gr",
        meter,
        if on { gr } else { 0.0 },
        NeedleScale {
            range: (0.0, GR_METER_RANGE_DB),
            // Reason の文字盤と同じ刻み。
            ticks: &[(0.0, "0"), (2.0, "2"), (4.0, "4"), (8.0, "8"), (12.0, "12"), (20.0, "20")],
            // 単位ラベルは置かない (文字盤が小さく、数字と重なって読みにくい)。
            unit: "",
        },
        &style,
    );

    // ---- ノブ 2 行 ----
    let knob = |param| MasterKnob::Native(*bus, NativeParamId::BusComp(param));
    let rows: [(&[MasterKnob], &str); 2] = [
        (&[knob(BusCompParam::Threshold), knob(BusCompParam::Ratio), knob(BusCompParam::Attack)], "Thr Ratio Atk"),
        (&[knob(BusCompParam::Release), knob(BusCompParam::Makeup)], "Rel Gain"),
    ];
    let mut y = rect.y + METER_H;
    for (i, (knobs, name)) in rows.into_iter().enumerate() {
        let row = Rect { y, h: ROW_H, ..rect };
        let hover = knob_row(app, ui, ("master_comp_row", i), row, knobs);
        row_label(app, ui, ("master_comp_label", i), row, name, hover);
        y += ROW_H;
    }
    dim_if_off(app, ui, Rect { y: rect.y + METER_H, h: ROW_H * 2.0, ..rect }, on);
}

// ---------------------------------------------------------------------------
// EQ
// ---------------------------------------------------------------------------

fn draw_eq<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, rect: Rect, tone: &NativeDevice) {
    let p = &app.theme.core;
    let on = !tone.bypassed;
    let curve = Rect { h: CURVE_H - 2.0, ..rect };
    ui.panel("master_eq_curve_bg", curve, block_bg(app, on), 2.0);

    // 0dB 基準線。
    let mid_y = curve.y + curve.h * 0.5;
    ui.push_rect(RectCommand {
        rect: Rect { x: curve.x, y: mid_y, w: curve.w, h: 1.0 },
        fill: p.border,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });

    // 応答は daw_audio と同じ関数から取る (画面と音を別実装にしない)。OFF は平らな線。
    let stages = match tone.params {
        NativeParams::ToneEq(eq) if on => Some(tone_eq_stages(&eq, CURVE_SR)),
        _ => None,
    };
    let color = if on { app.theme.daw.strip_eq_curve } else { p.text_dim };
    let ratio = CURVE_F_MAX / CURVE_F_MIN;
    let mut segs: Vec<LineSegment> = Vec::with_capacity(CURVE_POINTS);
    let mut prev: Option<[f32; 2]> = None;
    for i in 0..=CURVE_POINTS {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f32 / CURVE_POINTS as f32;
        let f = CURVE_F_MIN * ratio.powf(t);
        let db = stages
            .as_ref()
            .map_or(0.0, |s| tone_eq_magnitude_db(s, CURVE_SR, f))
            .clamp(-TONE_EQ_LIMIT_DB, TONE_EQ_LIMIT_DB);
        let x = curve.x + curve.w * t;
        let y = mid_y - (db / TONE_EQ_LIMIT_DB) * (curve.h * 0.5 - 1.0);
        if let Some(pp) = prev {
            segs.push(LineSegment { a: pp, b: [x, y], color });
        }
        prev = Some([x, y]);
    }
    ui.push_lines(LineBatch {
        segments: Arc::from(segs),
        line_width_px: 1.0,
        clip_rect: Some(curve),
    });

    let row = Rect { y: rect.y + CURVE_H, h: ROW_H, ..rect };
    let knobs: Vec<MasterKnob> =
        ToneEqBand::ALL.into_iter().map(|b| MasterKnob::Native(*tone, NativeParamId::ToneEq(b))).collect();
    let hover = knob_row(app, ui, ("master_eq_row", 0), row, &knobs);
    row_label(app, ui, ("master_eq_label", 0), row, "Lo LoMid Hi", hover);
    dim_if_off(app, ui, row, on);
}

// ---------------------------------------------------------------------------
// Limiter
// ---------------------------------------------------------------------------

fn draw_limiter<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, rect: Rect) {
    let on = app.cur.song_doc.song().master_limiter.on;
    // ---- GR セグメント (1 個 = 1dB) ----
    let bar = Rect { h: LIM_BAR_H - 2.0, ..rect };
    ui.panel("master_lim_bar_bg", bar, block_bg(app, on), 2.0);
    let gr = if on { app.cur.transport.master_limiter_gr } else { 0.0 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lit = (gr.max(0.0) as usize).min(LIM_SEGMENTS);
    #[allow(clippy::cast_precision_loss)]
    let seg_w = (bar.w - 2.0) / LIM_SEGMENTS as f32;
    for i in 0..lit {
        #[allow(clippy::cast_precision_loss)]
        let x = bar.x + 1.0 + seg_w * i as f32;
        ui.push_rect(RectCommand {
            rect: Rect { x, y: bar.y + 1.0, w: seg_w - 1.0, h: bar.h - 2.0 },
            fill: app.theme.daw.strip_gr,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [1.0; 4],
            clip_rect: None,
        });
    }

    // ---- ノブ 1 個 + 常時表示の値 ----
    // この行はノブが 1 個で右側が空くので、hover を待たずに値をノブの横へ常に出す
    // (見出しは行名のまま固定)。
    let row = Rect { y: rect.y + LIM_BAR_H, h: ROW_H, ..rect };
    let knob = Rect {
        x: row.x + (row.w - KNOB).max(0.0) * 0.5,
        y: row.y + LABEL_H,
        w: KNOB,
        h: KNOB,
    };
    let readout = master_knob(app, ui, ("master_lim_row", 0, 0), knob, MasterKnob::Ceiling);
    let value_rect = Rect {
        x: knob.x + KNOB + KNOB_GAP,
        y: knob.y + (KNOB - LABEL_H) * 0.5,
        w: (row.x + row.w - (knob.x + KNOB + KNOB_GAP)).max(0.0),
        h: LABEL_H,
    };
    let p = &app.theme.core;
    let color = if readout.active { p.text } else { p.text_dim };
    ui.label_at_clipped(("master_lim_value", 0), &readout.value, value_rect, LABEL_FONT, color);
    row_label(app, ui, ("master_lim_label", 0), row, "Limiter Ceiling", None);
    dim_if_off(app, ui, row, on);
}

// ---------------------------------------------------------------------------
// 共通部品
// ---------------------------------------------------------------------------

/// マスターストリップのノブ 1 個の住所。
#[derive(Clone, Copy)]
enum MasterKnob {
    /// 組み込み Bus Comp / Tone EQ の値 (描画時点の device)。
    Native(NativeDevice, NativeParamId),
    /// フェーダー後 Limiter の Ceiling。
    Ceiling,
}

impl MasterKnob {
    fn target(self) -> AutomationTarget {
        match self {
            Self::Native(dev, param) => AutomationTarget::NativeParam { device_id: dev.id, param },
            Self::Ceiling => AutomationTarget::MasterLimiter(MasterLimiterParam::Ceiling),
        }
    }

    fn plain(self, app: &AppData) -> f32 {
        match self {
            Self::Native(dev, param) => dev.param(param).unwrap_or(0.0),
            Self::Ceiling => app.cur.song_doc.song().master_limiter.param(MasterLimiterParam::Ceiling),
        }
    }

    fn default_plain(self) -> f32 {
        match self {
            Self::Native(_, param) => param.default_plain().unwrap_or(0.0),
            Self::Ceiling => MasterLimiterParam::Ceiling.default_plain(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Native(_, param) => param.knob_label(),
            Self::Ceiling => MasterLimiterParam::Ceiling.label(),
        }
    }

    /// ゲイン系 (0 が中央) だけ bipolar。
    fn bipolar(self) -> bool {
        matches!(self, Self::Native(_, NativeParamId::ToneEq(_) | NativeParamId::BusComp(BusCompParam::Makeup)))
    }

    /// 値の編集イベント (自動 ON と値 IPC は handler が持つ)。
    fn edit(self, value: f32) -> AppEvent {
        match self {
            Self::Native(dev, param) => {
                AppEvent::Device(DeviceEvent::NativeEdit { device_id: dev.id, edit: NativeEdit::param(param, value) })
            }
            Self::Ceiling => AppEvent::Device(DeviceEvent::MasterLimiterEdit(MasterLimiterEdit::Ceiling(value))),
        }
    }
}

/// ノブを 1 行ぶん中央寄せで描く。戻り値は hover / drag 中のノブの読み出し文字列。
fn knob_row<'a>(
    app: &'a AppData,
    ui: &mut Ui<'a, AppData>,
    id: (&'static str, usize),
    row: Rect,
    knobs: &[MasterKnob],
) -> Option<String> {
    #[allow(clippy::cast_precision_loss)]
    let total = KNOB * knobs.len() as f32 + KNOB_GAP * (knobs.len() as f32 - 1.0);
    let start_x = row.x + (row.w - total).max(0.0) * 0.5;
    let mut hover = None;
    for (i, knob) in knobs.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let x = start_x + (KNOB + KNOB_GAP) * i as f32;
        let r = master_knob(app, ui, (id.0, id.1, i), Rect { x, y: row.y + LABEL_H, w: KNOB, h: KNOB }, *knob);
        if r.active {
            hover = Some(format!("{} {}", knob.label(), r.value));
        }
    }
    hover
}

/// [`master_knob`] の読み出し。`value` は現在 (drag 中は drag 中) の値の表示文字列、
/// `active` は hover / drag 中か。
struct KnobReadout {
    value: String,
    active: bool,
}

/// 行の見出し。ノブに触れていない間は行の名前、触れている間はその値。
fn row_label<'a>(
    app: &'a AppData,
    ui: &mut Ui<'a, AppData>,
    id: (&'static str, usize),
    row: Rect,
    default_text: &str,
    hover: Option<String>,
) {
    let p = &app.theme.core;
    let (text, color) = match &hover {
        Some(t) => (t.as_str(), p.text),
        None => (default_text, p.text_dim),
    };
    ui.label_at_clipped(id, text, Rect { h: LABEL_H, ..row }, LABEL_FONT, color);
}

/// マスターストリップのノブ 1 個。段階式パラメータは段へ丸まる (`NativeParams::set`)。
fn master_knob<'a>(
    app: &'a AppData,
    ui: &mut Ui<'a, AppData>,
    id: (&'static str, usize, usize),
    rect: Rect,
    knob: MasterKnob,
) -> KnobReadout {
    let target = knob.target();
    let norm = plain_to_norm(&target, f64::from(knob.plain(app)));
    let default_norm = plain_to_norm(&target, f64::from(knob.default_plain()));
    let base = if knob.bipolar() { KnobStyle::BIPOLAR } else { KnobStyle::UNIPOLAR };
    let resp = ui.knob_at(
        id,
        rect,
        norm,
        default_norm,
        &KnobStyle { surface: Some(app.theme.core.panel), ..base },
        {
            let target = target.clone();
            move |v| {
                #[allow(clippy::cast_possible_truncation)]
                let value = norm_to_plain(&target, v) as f32;
                let event = knob.edit(value);
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(event);
                })
            }
        },
        None,
    );
    let shown = norm_to_plain(&target, resp.displayed_value);
    KnobReadout {
        value: format_master_value(&target, shown),
        active: resp.hovered || resp.dragging,
    }
}

/// 段階式は段のラベル (`4:1` / `30ms` / `Auto`)、連続は数値 + 単位。
fn format_master_value(target: &AutomationTarget, plain: f64) -> String {
    if let AutomationTarget::NativeParam { param, .. } = target
        && let Some(labels) = param.step_labels()
    {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = (plain.round().max(0.0) as usize).min(labels.len() - 1);
        return labels[idx].to_string();
    }
    let desc = automation_value_display(target, None);
    format!("{}{}", desc.format.format_value(plain), desc.unit)
}
