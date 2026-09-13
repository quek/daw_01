//! Tone EQ の Par (Q12): カーブ (点は上下のみ) → 見出し `Low LoMid High` → 3 セル。

use common::model::{NativeParamId, NativeParams, RackPanelKey, ToneEqBand};
use daw_ui_core::Ui;

use super::PanelCtx;
use super::cell::{head_labels, param_cell};
use super::eq_graph::draw_eq_graph;
use super::layout::{CURVE_H, HEAD, PAD, ROW_GAP, curve_rect};
use crate::app::AppData;

pub(super) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &PanelCtx<'_>) {
    let NativeParams::ToneEq(_) = &ctx.dev.params else {
        return;
    };
    let key = RackPanelKey::Device(ctx.dev.id);
    let drags = draw_eq_graph(app, ui, ctx, curve_rect(ctx.rect));
    let mut y = ctx.rect.y + PAD + CURVE_H + ROW_GAP;
    head_labels(app, ui, ctx, key, y, &ToneEqBand::ALL.map(ToneEqBand::label));
    y += HEAD;
    for (col, band) in ToneEqBand::ALL.into_iter().enumerate() {
        let p = NativeParamId::ToneEq(band);
        param_cell(app, ui, ctx, p, col, y, false, drags.has(p));
    }
}
