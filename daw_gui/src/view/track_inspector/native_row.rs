//! chain list の内蔵 device の行 (`名前 + 小表示 + [SC▾] [Par] [x]`) とその直下の展開 (SC パネル /
//! Par)、および master の末尾 (「Post-Fader」の区切り + 固定の Limiter 行)
//! (Q3 / Q9 / Q10 / Q15、`docs/plan_rack_native_devices.md` §10.5)。
//!
//! 行のボタン列は右から plugin 行と同じ x に揃える。組み込みは × を出さず列だけ空ける (Q10: 組み込みと
//! 足した分は × の有無で見分ける)。ON/OFF のボタンは置かない (Q15: `Q` / 右クリック / 小表示の
//! ダブルクリック)。

use common::model::{
    GR_METER_RANGE_DB, MASTER_TRACK_ID, NativeDevice, NativeKind, RackPanelKey,
};
use daw_ui_core::{Edit, Ui, WidgetId};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, NativeRowEntry, ParamSurface};
use crate::event_device::DeviceEvent;
use crate::event_native::MasterLimiterEdit;
use crate::handler::bypass_target::BypassTarget;
use crate::view::native_device::{
    CurveLook, EqCurveSource, LIMITER_GR_SEGMENTS, draw_eq_curve, draw_gr_horizontal, draw_gr_segments, wid,
};

use super::chain_list::{ROW_GAP, ROW_H, RowCtx, draw_row_bg};
use super::native_panel::{self, PanelCtx, layout};
use super::plugin_row::draw_sidechain_panel;

/// ボタン列 (plugin 行と同じ寸法)。
const BTN_X_W: f32 = 26.0;
const BTN_PAR_W: f32 = 44.0;
const BTN_SC_W: f32 = 34.0;
const BTN_GAP: f32 = 2.0;
/// 小表示の幅 (Q9)、EQ カーブの高さ、GR バーの太さ。
const MINI_W: f32 = 80.0;
const MINI_CURVE_H: f32 = 18.0;
const MINI_GR_H: f32 = 8.0;
/// 小表示とボタン列の間。
const MINI_GAP: f32 = 4.0;
/// 行の名前の文字サイズ (plugin 行と同じ)。
const NAME_FONT: f32 = 11.0;
/// master 末尾の「Post-Fader」の区切りの高さ。
const DIVIDER_H: f32 = 16.0;

/// 内蔵 device の行 (26px) を描く。
pub(super) fn draw_native_row(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &RowCtx<'_>, entry: &NativeRowEntry, row: Rect) {
    let p = &app.theme.core;
    let id = entry.device_id;
    let key = RackPanelKey::Device(id);
    let popup_open = ctx.popup_open;
    let btn_h = ROW_H - 4.0;
    let by = row.y + 2.0;
    let mut right = row.x + row.w - BTN_X_W;
    // [x] (足した分だけ。組み込みは列を空ける)。
    if !entry.builtin {
        ui.button_at(wid(ParamSurface::Rack, key, "remove", ()), "x", Rect { x: right, y: by, w: BTN_X_W, h: btn_h }, move || {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: vec![id] }));
                }
            })
        });
    }
    // [Par]
    right -= BTN_PAR_W + BTN_GAP;
    ui.button_at(wid(ParamSurface::Rack, key, "par", ()), "Par", Rect { x: right, y: by, w: BTN_PAR_W, h: btn_h }, move || {
        Edit::mutate(move |app: &mut AppData| {
            if !popup_open {
                app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(key)));
            }
        })
    });
    // [SC▾] (Comp / Bus Comp、Q19)。配線済みは ON 色。
    if entry.kind.accepts_sidechain() {
        right -= BTN_SC_W + BTN_GAP;
        let open = app.cur.peph.open_sidechain_panel == Some(id);
        let r = Rect { x: right, y: by, w: BTN_SC_W, h: btn_h };
        ui.toggle_button_at(wid(ParamSurface::Rack, key, "sc", ()), if open { "SC\u{25B4}" } else { "SC\u{25BE}" }, r, entry.sc_wired || open, &ctx.keys_style, move |_| {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.cur.peph.open_sidechain_panel = if app.cur.peph.open_sidechain_panel == Some(id) { None } else { Some(id) };
                }
            })
        });
    }
    // 小表示 (EQ 系はミニカーブ、Comp 系は横 GR バー。OFF は共有部品が形を保って薄く描く)。
    // ダブルクリックで ON/OFF (Q15)。単クリックは drag_list の click (選択) に流す。
    right -= MINI_W + MINI_GAP;
    let mini_hit = Rect { x: right, y: row.y + 2.0, w: MINI_W, h: ROW_H - 4.0 };
    draw_mini(app, ui, ctx, entry, mini_hit);
    if !popup_open && ui.take_double_click_in_rect(mini_hit).is_some() {
        let bypassed = !entry.bypassed;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Device(DeviceEvent::SetDevicesBypassed { device_ids: vec![id], bypassed }));
        }));
    }
    // 名前 (「Comp」 / 「Comp 2」、OFF は薄く)。
    let name_x = row.x + 8.0;
    ui.label_at_clipped(
        wid(ParamSurface::Rack, key, "name", ()),
        &entry.name,
        Rect { x: name_x, y: row.y + 8.0, w: (right - 6.0 - name_x).max(1.0), h: NAME_FONT * 1.2 },
        NAME_FONT,
        if entry.bypassed { p.text_faint } else { p.text },
    );
}

/// 行の小表示。値はレーンを重ねた live 値、ON/OFF の見せ方は Song の静的な状態 (§12.2)。
fn draw_mini(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &RowCtx<'_>, entry: &NativeRowEntry, area: Rect) {
    let id = entry.device_id;
    let key = RackPanelKey::Device(id);
    let active = !entry.bypassed;
    match entry.kind {
        NativeKind::Eq | NativeKind::ToneEq => {
            let Some(dev) = row_native(app, ctx, id) else { return };
            let live = live_device(app, ctx, dev);
            let Some(src) = EqCurveSource::from_params(&live.params) else { return };
            let rect = Rect { y: area.y + (area.h - MINI_CURVE_H) * 0.5, h: MINI_CURVE_H, ..area };
            ui.panel(wid(ParamSurface::Rack, key, "mini_well", ()), rect, app.theme.core.inset_bg, 2.0);
            draw_eq_curve(app, ui, rect, &src, &CurveLook { active, spectrum_db: None });
        }
        NativeKind::Comp | NativeKind::BusComp => {
            let rect = Rect { y: area.y + (area.h - MINI_GR_H) * 0.5, h: MINI_GR_H, ..area };
            let gr = app.cur.transport.native_gr.get(id);
            draw_gr_horizontal(app, ui, wid(ParamSurface::Rack, key, "mini_gr", ()), rect, gr, active, GR_METER_RANGE_DB, None);
        }
    }
}

/// 行の device。行は表示中のチェーン (カーソルトラック) から組まれているので、全トラックの木を先頭から
/// 走査せず、そのチェーンの中だけを探す (行ごとに毎フレーム呼ばれる)。
fn row_native<'a>(app: &'a AppData, ctx: &RowCtx<'_>, id: u64) -> Option<&'a NativeDevice> {
    let chain = app.cur.song_doc.song().fx_chain_by_track_id(ctx.cursor_tid?)?;
    common::model::native_in(chain, id)
}

/// レーンを重ねた表示値 (持ち主の store が引けなければ Song の値)。
fn live_device(app: &AppData, ctx: &RowCtx<'_>, dev: &NativeDevice) -> NativeDevice {
    match ctx.owner {
        Some(owner) => app.live_native_device(ctx.scope, owner, dev),
        None => *dev,
    }
}

/// 内蔵 device の行の直下の展開 (SC パネル → Par)。
pub(super) fn draw_native_expansions(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &RowCtx<'_>, entry: &NativeRowEntry, content: Rect) {
    let id = entry.device_id;
    let mut ey = content.y + ROW_H;
    if ctx.sc_open == Some(id) && ctx.sc_panel_h > 0.0 {
        let rect = Rect { x: content.x, y: ey, w: content.w, h: ctx.sc_panel_h };
        claim_expansion_press(ui, rect, ("sc", id), ctx.popup_open);
        draw_sidechain_panel(app, ui, id, ctx.sc_ports, rect);
        ey += ctx.sc_panel_h;
    }
    let key = RackPanelKey::Device(id);
    if !app.rack_panel_open(key) {
        return;
    }
    let Some(dev) = row_native(app, ctx, id) else { return };
    let Some(owner) = ctx.owner else { return };
    let rect = Rect { x: content.x, y: ey, w: content.w, h: layout::panel_height(entry.kind) };
    let bg = draw_panel_bg(app, ui, key, rect, ctx.popup_open);
    let panel = PanelCtx { dev, live: app.live_native_device(ctx.scope, owner, dev), owner, scope: ctx.scope, rect, bg };
    native_panel::draw_native_panel(app, ui, &panel);
}

/// Par / SC パネルの面 (行の hover 色に左右されない固定の面) を塗り、余白の press を名乗る。
/// 戻り値 = 面の色 (つまみのリングのくり抜き色)。
fn draw_panel_bg(app: &AppData, ui: &mut Ui<'_, AppData>, key: RackPanelKey, rect: Rect, popup_open: bool) -> daw_ui_renderer::Color {
    let bg = app.theme.core.panel;
    ui.panel(wid(ParamSurface::Rack, key, "panel_bg", ()), Rect { x: rect.x + 1.0, y: rect.y, w: (rect.w - 2.0).max(0.0), h: (rect.h - 1.0).max(0.0) }, bg, 2.0);
    claim_expansion_press(ui, rect, ("par", key), popup_open);
    bg
}

/// 展開部の余白で押した press を名乗る (§10.5)。子 widget (つまみ / 点 / ボタン) は後から名乗るので
/// 子が勝ち、行の drag_list は次のフレームで session を捨てる — 行を掴めるのはヘッダ 26px だけになる
/// (余白を縦に動かしたら行が動く、を防ぐ)。
pub(super) fn claim_expansion_press(ui: &mut Ui<'_, AppData>, rect: Rect, key: impl std::hash::Hash, popup_open: bool) {
    let pointer = ui.pointer();
    if pointer.primary_just_pressed && !popup_open && pointer.pos.is_some_and(|(px, py)| rect.contains(px, py)) {
        ui.claim_press(WidgetId::ROOT.child((b"rack_expansion_bg", &key)));
    }
}

/// master の末尾 (drag_list の外): 「Post-Fader」の区切り → 固定の Limiter 行 (+ 開いていれば Par)。
/// 掴めず落とし先にもならないことが「drag_list の外に描く」で構造的に決まる (Q3)。
/// 戻り値 = (消費後の y、Limiter 行 (Par 込み) の上にカーソルがあれば `Q` の宛先)。
pub(super) fn draw_master_tail(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &RowCtx<'_>, x: f32, w: f32, mut y: f32) -> (f32, Option<BypassTarget>) {
    debug_assert_eq!(ctx.cursor_tid, Some(MASTER_TRACK_ID));
    let p = &app.theme.core;
    // ---- 区切り (入力は取らない) ----
    y += ROW_GAP;
    let label = "Post-Fader";
    let font = 10.0;
    let tw = ui.measure_text(label, font);
    let mid = y + DIVIDER_H * 0.5;
    let gap = 6.0;
    let lx = x + (w - tw) * 0.5;
    ui.panel("rack_post_fader_rule_l", Rect { x, y: mid.floor(), w: (lx - gap - x).max(0.0), h: 1.0 }, p.border, 0.0);
    ui.panel("rack_post_fader_rule_r", Rect { x: lx + tw + gap, y: mid.floor(), w: (x + w - (lx + tw + gap)).max(0.0), h: 1.0 }, p.border, 0.0);
    ui.label_at("rack_post_fader_label", label, lx, mid - font * 0.6, font, p.text_dim);
    y += DIVIDER_H + ROW_GAP;

    // ---- Limiter 行 ----
    let key = RackPanelKey::MasterLimiter;
    let open = app.rack_panel_open(key);
    let par_h = if open { layout::limiter_panel_height() } else { 0.0 };
    let row = Rect { x, y, w, h: ROW_H + par_h };
    let hovered = ui.hovers(row);
    draw_row_bg(ui, ("master_limiter", ()), row, false, hovered, false, p);
    let on = app.cur.song_doc.song().master_limiter.on;
    let popup_open = ctx.popup_open;
    let btn_h = ROW_H - 4.0;
    let by = y + 2.0;
    let mut right = x + w - BTN_X_W; // × の列は空ける (組み込みと同じ見分け)
    right -= BTN_PAR_W + BTN_GAP;
    ui.button_at(wid(ParamSurface::Rack, key, "par", ()), "Par", Rect { x: right, y: by, w: BTN_PAR_W, h: btn_h }, move || {
        Edit::mutate(move |app: &mut AppData| {
            if !popup_open {
                app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(key)));
            }
        })
    });
    right -= MINI_W + MINI_GAP;
    let mini_hit = Rect { x: right, y: y + 2.0, w: MINI_W, h: ROW_H - 4.0 };
    let seg = Rect { y: y + (ROW_H - MINI_GR_H) * 0.5, h: MINI_GR_H, ..mini_hit };
    draw_gr_segments(app, ui, wid(ParamSurface::Rack, key, "mini_gr", ()), seg, app.cur.transport.master_limiter_gr, on, LIMITER_GR_SEGMENTS);
    if !popup_open && ui.take_double_click_in_rect(mini_hit).is_some() {
        ui.push_edit(limiter_on_edit(!on));
    }
    let name_x = x + 8.0;
    ui.label_at_clipped(
        wid(ParamSurface::Rack, key, "name", ()),
        "Limiter",
        Rect { x: name_x, y: y + 8.0, w: (right - 6.0 - name_x).max(1.0), h: NAME_FONT * 1.2 },
        NAME_FONT,
        if on { p.text } else { p.text_faint },
    );
    // 右クリックは「有効化 / 無効化」だけ (動かせず消せない)。
    let base = Rect { h: ROW_H, ..row };
    let labels = [if on { "無効化" } else { "有効化" }];
    ui.context_menu_for(base, &labels, move |_, ui| {
        ui.push_edit(limiter_on_edit(!on));
    });
    if open {
        let rect = Rect { x, y: y + ROW_H, w, h: par_h };
        let bg = draw_panel_bg(app, ui, key, rect, popup_open);
        native_panel::draw_limiter_panel(app, ui, rect, bg, ctx.scope);
    }
    let hover = hovered.then_some(BypassTarget::MasterLimiter);
    (y + row.h, hover)
}

fn limiter_on_edit(on: bool) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::Device(DeviceEvent::MasterLimiterEdit(MasterLimiterEdit::On(on))));
    })
}
