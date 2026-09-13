//! Comp の Par (Q12): `[LEV|CMP|LIM]` + 横 GR バー (dB) → 見出し `Thr Rat Atk Rel SC Gain` → 6 セル →
//! SC 列の下に `[Listen]`。モードに上書きされるつまみ (`CompMode::overrides`) は沈める。

use common::model::{CompMode, CompParam, GR_METER_RANGE_DB, NativeParamId, NativeParams, RackPanelKey};
use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use super::PanelCtx;
use super::cell::{head_labels, param_cell, switch, switch_style};
use super::layout::{BAR_H, CELL, HEAD, KNOB_GAP, NUM_H, PAD, ROW_GAP};
use crate::app::{AppData, AppEvent, ParamSurface};
use crate::event_device::DeviceEvent;
use crate::event_native::NativeEdit;
use crate::view::native_device::{draw_gr_horizontal, wid};

/// 列の並び (Q12)。
const COLUMNS: [CompParam; 6] =
    [CompParam::Threshold, CompParam::Ratio, CompParam::Attack, CompParam::Release, CompParam::ScFreq, CompParam::Makeup];
/// `[LEV|CMP|LIM]` の 1 ボタンの幅。
const MODE_W: f32 = 40.0;
/// GR バーの太さ。
const GR_H: f32 = 8.0;
/// `[Listen]` を置く列 (SC 列)。
const LISTEN_COL: usize = 4;

pub(super) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &PanelCtx<'_>) {
    let NativeParams::Comp(c) = &ctx.dev.params else {
        return;
    };
    let id = ctx.dev.id;
    let key = RackPanelKey::Device(id);
    let (x, w) = (ctx.rect.x, ctx.rect.w);
    let mut y = ctx.rect.y + PAD;

    let style = switch_style(app);
    for (k, mode) in CompMode::ALL.into_iter().enumerate() {
        let r = Rect { x: x + k as f32 * MODE_W, y, w: MODE_W, h: BAR_H };
        ui.toggle_button_at(wid(ParamSurface::Rack, key, "mode", k), mode.label(), r, c.mode == mode, &style, move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Device(DeviceEvent::NativeEdit { device_id: id, edit: NativeEdit::CompMode(mode) }));
            })
        });
    }
    let gr_x = x + 3.0 * MODE_W + 8.0;
    let gr = Rect { x: gr_x, y: y + (BAR_H - GR_H) * 0.5, w: (x + w - gr_x).max(1.0), h: GR_H };
    let gr_db = app.cur.transport.native_gr.get(id);
    draw_gr_horizontal(app, ui, wid(ParamSurface::Rack, key, "gr", ()), gr, gr_db, !ctx.dev.bypassed, GR_METER_RANGE_DB, Some(10.0));
    y += BAR_H + ROW_GAP;

    head_labels(app, ui, ctx, key, y, &COLUMNS.map(CompParam::label));
    y += HEAD;
    for (col, p) in COLUMNS.into_iter().enumerate() {
        param_cell(app, ui, ctx, NativeParamId::Comp(p), col, y, c.mode.overrides(p), false);
    }

    // Listen: 点灯 = この Comp を Listen 中で、かつ効いている (Q で OFF にすると engine は素通しする)。
    let lit = app.cur.peph.sc_listen_device == Some(id) && !ctx.dev.bypassed;
    let listen_y = y + CELL + KNOB_GAP;
    let event = DeviceEvent::SetScListen { device_id: if lit { None } else { Some(id) } };
    switch(app, ui, ctx.rect, key, "listen", LISTEN_COL, listen_y, NUM_H, "Listen", lit, event);
}
