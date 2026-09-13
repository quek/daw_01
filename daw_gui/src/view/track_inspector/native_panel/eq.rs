//! EQ の Par (Q12): 全幅のカーブ → 見出し `HP LF LMF HMF HF LP` → Freq / Gain / Q の 3 段。
//!
//! | 列 | Freq 段 | Gain 段 | Q 段 |
//! |---|---|---|---|
//! | HP / LP | Freq | `[ON]` (`EqBandOn`) | 空き |
//! | LF / HF | Freq | Gain | `[Bell]` (`EqBell`) |
//! | LMF / HMF | Freq | Gain | Q |

use common::model::{EqBand, EqParam, NativeParamId, NativeParams, RackPanelKey};
use daw_ui_core::Ui;

use super::PanelCtx;
use super::cell::{head_labels, param_cell, switch};
use super::eq_graph::draw_eq_graph;
use super::layout::{CELL, CURVE_H, HEAD, PAD, ROW_GAP, curve_rect};
use crate::app::AppData;
use crate::event_device::DeviceEvent;
use crate::event_native::NativeEdit;

pub(super) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &PanelCtx<'_>) {
    let NativeParams::Eq(eq) = &ctx.dev.params else {
        return;
    };
    let id = ctx.dev.id;
    let key = RackPanelKey::Device(id);
    let drags = draw_eq_graph(app, ui, ctx, curve_rect(ctx.rect));
    let mut y = ctx.rect.y + PAD + CURVE_H + ROW_GAP;
    head_labels(app, ui, ctx, key, y, &EqBand::BY_FREQ.map(EqBand::label));
    y += HEAD;
    let [freq_y, gain_y, q_y] = [y, y + CELL + ROW_GAP, y + 2.0 * (CELL + ROW_GAP)];
    for (col, band) in EqBand::BY_FREQ.into_iter().enumerate() {
        let b = eq.band(band);
        let param = |param| NativeParamId::Eq { band, param };
        // OFF のバンドのつまみは沈める (触れば `NativeEdit::apply` がそのバンドを ON にする)。
        let dimmed = !b.on;
        let freq = param(EqParam::Freq);
        param_cell(app, ui, ctx, freq, col, freq_y, dimmed, drags.has(freq));
        if band.has_gain() {
            let gain = param(EqParam::Gain);
            param_cell(app, ui, ctx, gain, col, gain_y, dimmed, drags.has(gain));
        } else {
            let edit = NativeEdit::EqBandOn { band, on: !b.on };
            switch(app, ui, ctx.rect, key, "band_on", col, gain_y, CELL, "ON", b.on, DeviceEvent::NativeEdit { device_id: id, edit });
        }
        if band.has_q_knob() {
            let q = param(EqParam::Q);
            param_cell(app, ui, ctx, q, col, q_y, dimmed, drags.has(q));
        } else if band.has_bell_switch() {
            let edit = NativeEdit::EqBell { band, bell: !b.bell };
            switch(app, ui, ctx.rect, key, "bell", col, q_y, CELL, "Bell", b.bell, DeviceEvent::NativeEdit { device_id: id, edit });
        }
    }
}
