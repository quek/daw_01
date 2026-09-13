//! master のフェーダー後 Limiter の Par (Q3 / Q12): GR セグメント (12 段、1 段 = 1 dB) + dB → 見出し
//! `Ceiling` → Ceiling の 1 セル。Limiter はチェーンの外なので `NativeDevice` を持たず、値は
//! `Song.master_limiter`、つまみは `limiter_knob`。

use common::model::{MasterLimiterParam, RackPanelKey};
use daw_ui_core::Ui;
use daw_ui_renderer::{Color, Rect};

use super::layout::{BAR_H, HEAD, HEAD_FONT, KNOB, KNOB_GAP, NUM_H, NUM_W, PAD, ROW_GAP, column_x};
use crate::app::{AppData, ParamSurface};
use crate::handler::view_model::LiveParamScope;
use crate::view::native_device::{LIMITER_GR_SEGMENTS, draw_gr_segments, gr_text, limiter_knob, wid};

/// GR セグメントの太さ。
const GR_H: f32 = 8.0;
/// セグメントの右に置く dB 表記の幅。
const GR_TEXT_W: f32 = 40.0;

/// `rect` に Limiter の Par を描く。`bg` は Par の面の色。
pub(in super::super) fn draw_limiter_panel(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    rect: Rect,
    bg: Color,
    scope: &LiveParamScope,
) {
    let key = RackPanelKey::MasterLimiter;
    let on = app.cur.song_doc.song().master_limiter.on;
    let gr_db = app.cur.transport.master_limiter_gr;
    let mut y = rect.y + PAD;
    let seg = Rect { x: rect.x, y: y + (BAR_H - GR_H) * 0.5, w: (rect.w - GR_TEXT_W).max(1.0), h: GR_H };
    draw_gr_segments(app, ui, wid(ParamSurface::Rack, key, "gr", ()), seg, gr_db, on, LIMITER_GR_SEGMENTS);
    let p = &app.theme.core;
    let text = format!("{} dB", gr_text(gr_db));
    ui.label_at(
        wid(ParamSurface::Rack, key, "gr_value", ()),
        &text,
        seg.x + seg.w + 4.0,
        y + (BAR_H - HEAD_FONT * 1.2) * 0.5,
        HEAD_FONT,
        if on { p.text_dim } else { p.text_faint },
    );
    y += BAR_H + ROW_GAP;

    let label = MasterLimiterParam::Ceiling.label();
    let w = ui.measure_text(label, HEAD_FONT);
    ui.label_at(wid(ParamSurface::Rack, key, "head", 0), label, column_x(rect, 0, w), y, HEAD_FONT, p.text_dim);
    y += HEAD;
    let knob = Rect { x: column_x(rect, 0, KNOB), y, w: KNOB, h: KNOB };
    let value = Rect { x: column_x(rect, 0, NUM_W), y: y + KNOB + KNOB_GAP, w: NUM_W, h: NUM_H };
    limiter_knob(app, ui, ParamSurface::Rack, knob, bg, scope, Some(value));
}
