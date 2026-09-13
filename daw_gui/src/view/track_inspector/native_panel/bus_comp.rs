//! Bus Comp の Par (Q12): 全幅の GR バー → 見出し `Thr Ratio Atk Rel Makeup` → Thr / Atk / Rel / Makeup の
//! セル (Atk / Rel は段階式) と、Ratio 列の `[2|4|10]` + 値ラベル。

use common::model::{BusCompParam, BusCompRatio, GR_METER_RANGE_DB, NativeParamId, NativeParams, RackPanelKey};
use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use super::PanelCtx;
use super::cell::{head_labels, param_cell, switch_style};
use super::layout::{BAR_H, HEAD, HEAD_FONT, KNOB, KNOB_GAP, NUM_H, PAD, ROW_GAP, column_x};
use crate::app::{AppData, AppEvent, ParamSurface};
use crate::automation_value::automation_value_display;
use crate::event_device::DeviceEvent;
use crate::event_native::NativeEdit;
use crate::view::native_device::{draw_gr_horizontal, wid};

/// 列の並び (Q12)。
const COLUMNS: [BusCompParam; 5] =
    [BusCompParam::Threshold, BusCompParam::Ratio, BusCompParam::Attack, BusCompParam::Release, BusCompParam::Makeup];
/// Ratio の段ボタンの表記 (`BusCompRatio::ALL` の順)。
const RATIO_BUTTONS: [&str; BusCompRatio::ALL.len()] = ["2", "4", "10"];
/// Ratio の段ボタン 1 個の幅と高さ。
const RATIO_BTN: f32 = 18.0;
/// GR バーの太さ。
const GR_H: f32 = 8.0;

pub(super) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &PanelCtx<'_>) {
    let NativeParams::BusComp(_) = &ctx.dev.params else {
        return;
    };
    let id = ctx.dev.id;
    let key = RackPanelKey::Device(id);
    let mut y = ctx.rect.y + PAD;
    let gr = Rect { x: ctx.rect.x, y: y + (BAR_H - GR_H) * 0.5, w: ctx.rect.w, h: GR_H };
    let gr_db = app.cur.transport.native_gr.get(id);
    draw_gr_horizontal(app, ui, wid(ParamSurface::Rack, key, "gr", ()), gr, gr_db, !ctx.dev.bypassed, GR_METER_RANGE_DB, Some(10.0));
    y += BAR_H + ROW_GAP;

    head_labels(app, ui, ctx, key, y, &COLUMNS.map(BusCompParam::label));
    y += HEAD;
    for (col, p) in COLUMNS.into_iter().enumerate() {
        if p == BusCompParam::Ratio {
            draw_ratio(app, ui, ctx, col, y);
        } else {
            param_cell(app, ui, ctx, NativeParamId::BusComp(p), col, y, false, false);
        }
    }
}

/// Ratio 列: 段ボタン 3 個 + 今の段の表記 (「4:1」)。押すと 1 イベント (= undo 1 step) で段を選ぶ。
fn draw_ratio(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &PanelCtx<'_>, col: usize, y: f32) {
    let id = ctx.dev.id;
    let key = RackPanelKey::Device(id);
    let param = NativeParamId::BusComp(BusCompParam::Ratio);
    let owner = ctx.owner;
    let current = app.live_native_param(ctx.scope, owner, ctx.dev, param);
    let style = switch_style(app);
    let x0 = column_x(ctx.rect, col, RATIO_BTN * RATIO_BUTTONS.len() as f32);
    for (k, label) in RATIO_BUTTONS.into_iter().enumerate() {
        let r = Rect { x: x0 + k as f32 * RATIO_BTN, y: y + (KNOB - RATIO_BTN) * 0.5, w: RATIO_BTN, h: RATIO_BTN };
        let lit = current.round() as usize == k;
        let step = k as f32;
        ui.toggle_button_at(wid(ParamSurface::Rack, key, "ratio", k), label, r, lit, &style, move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Device(DeviceEvent::NativeEdit { device_id: id, edit: NativeEdit::param(param, step) }));
            })
        });
    }
    let text = automation_value_display(&common::model::AutomationTarget::NativeParam { device_id: id, param }, None)
        .format_with_unit(f64::from(current));
    let w = ui.measure_text(&text, HEAD_FONT);
    let color = app.theme.core.text;
    ui.label_at(wid(ParamSurface::Rack, key, "ratio_value", ()), &text, column_x(ctx.rect, col, w), y + KNOB + KNOB_GAP + (NUM_H - HEAD_FONT * 1.2) * 0.5, HEAD_FONT, color);
}
