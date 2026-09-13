//! chain list の plugin 行 (`名前 + [SC▾] [⌨] [Par|GUI] [x]`) と、 その直下の展開
//! (SC パネル / param パネル)。 chain list 本体 (`chain_list.rs`) から切り出したもの
//! (サイズ budget、 不変条件 9)。

use daw_ui_core::{Edit, ToggleButtonStyle, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, ChainEntry};
use crate::event_device::DeviceEvent;
use common::model::{RackPanelKey, TapPoint, TapSource};

use super::chain_list::{ROW_H, RowCtx, panel_height};
use super::device_panel;
use super::native_row::claim_expansion_press;

/// SC パネル 1 port 行の高さ。
pub(super) const SC_PORT_H: f32 = 24.0;
pub(super) const SC_PAD: f32 = 6.0;

/// plugin 行の直下の展開 (SC パネル / param パネル)。
pub(super) fn draw_plugin_expansions(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    ctx: &RowCtx<'_>,
    device_id: u64,
    content: Rect,
) {
    let mut ey = content.y + ROW_H;
    if ctx.sc_open == Some(device_id) && ctx.sc_panel_h > 0.0 {
        let rect = Rect { x: content.x, y: ey, w: content.w, h: ctx.sc_panel_h };
        claim_expansion_press(ui, rect, ("sc", device_id), ctx.popup_open);
        draw_sidechain_panel(app, ui, device_id, ctx.sc_ports, rect);
        ey += ctx.sc_panel_h;
    }
    let key = RackPanelKey::Device(device_id);
    if app.rack_panel_open(key) {
        let exp_rect = Rect { x: content.x, y: ey, w: content.w, h: panel_height(app, device_id) };
        claim_expansion_press(ui, exp_rect, ("par", key), ctx.popup_open);
        let measured =
            (device_panel::draw_device_panel(app, ui, ctx.area, ctx.pad, exp_rect, device_id) - exp_rect.y).max(0.0);
        // 展開部の実消費高を device ごとに測って次フレームの行高に使う (lag-by-one)。
        if app.cur.peph.rack_panel_heights.get(&key).is_none_or(|h| (h - measured).abs() > 0.5) {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.cur.peph.rack_panel_heights.insert(key, measured);
            }));
        }
    }
}

/// plugin 行: 名前 + [SC▾] [⌨] [Par|GUI] [x]。
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_plugin_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    i: usize,
    entry: &ChainEntry,
    row: Rect,
    popup_open: bool,
    keys_style: &ToggleButtonStyle,
) {
    let p = &app.theme.core;
    let device_id = entry.device_id;
    let btn_gui_w = 44.0;
    let btn_x_w = 26.0;
    let btn_keys_w = 26.0;
    let btn_sc_w = 34.0;
    let btn_h = ROW_H - 4.0;
    let by = row.y + 2.0;
    let mut right = row.x + row.w - btn_x_w;
    // [x]
    ui.button_at(
        ("inspector_row_remove", i),
        "x",
        Rect { x: right, y: by, w: btn_x_w, h: btn_h },
        move || {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: vec![device_id] }));
                }
            })
        },
    );
    // [Par | GUI]
    if entry.shows_button() {
        right -= btn_gui_w + 2.0;
        let label = if entry.shows_param_panel() { "Par" } else { "GUI" };
        ui.button_at(
            ("inspector_row_gui", i),
            label,
            Rect { x: right, y: by, w: btn_gui_w, h: btn_h },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    if !popup_open {
                        app.handle_event(AppEvent::Device(DeviceEvent::ToggleSlotGui { device_id }));
                    }
                })
            },
        );
    }
    // [⌨] (埋め込みエディタ窓を開く device だけ)
    if entry.has_embedded_gui && !entry.shows_param_panel() {
        right -= btn_keys_w + 2.0;
        let next = !entry.send_all_keys;
        ui.toggle_button_at(
            ("inspector_row_keys", i),
            "\u{2328}",
            Rect { x: right, y: by, w: btn_keys_w, h: btn_h },
            entry.send_all_keys,
            keys_style,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    if !popup_open {
                        app.handle_event(AppEvent::Device(DeviceEvent::SetPluginSendAllKeys { device_id, enabled: next }));
                    }
                })
            },
        );
    }
    // [SC] (aux 入力 port を持つ plugin だけ)。 配線済みは ON 色。
    if entry.aux_input_count > 0 {
        right -= btn_sc_w + 2.0;
        let open = app.cur.peph.open_sidechain_panel == Some(device_id);
        ui.toggle_button_at(
            ("inspector_row_sc", i),
            if open { "SC\u{25B4}" } else { "SC\u{25BE}" },
            Rect { x: right, y: by, w: btn_sc_w, h: btn_h },
            entry.sc_wired || open,
            keys_style,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    if popup_open {
                        return;
                    }
                    app.cur.peph.open_sidechain_panel =
                        if app.cur.peph.open_sidechain_panel == Some(device_id) { None } else { Some(device_id) };
                })
            },
        );
    }
    // 名前 (ボタンの手前で打ち切る)。
    let failed = entry.load_error.is_some();
    let display_name: std::borrow::Cow<'_, str> = if failed {
        format!("[未ロード] {}", entry.plugin_name).into()
    } else {
        entry.plugin_name.as_str().into()
    };
    let name_x = row.x + 8.0;
    ui.label_at_clipped(
        ("inspector_row_name", i),
        &display_name,
        Rect { x: name_x, y: row.y + 8.0, w: (right - 6.0 - name_x).max(1.0), h: 11.0 * 1.2 },
        11.0,
        if failed {
            p.text_error
        } else if entry.bypassed {
            p.text_faint
        } else {
            p.text
        },
    );
}

/// SC パネル: port ごとに `In N: [source ▾] [tap ▾]` (plugin と内蔵 Comp / Bus Comp 共通)。
pub(super) fn draw_sidechain_panel(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    device_id: u64,
    ports: &[crate::app_types::SidechainPort],
    rect: Rect,
) {
    let p = &app.theme.core;
    let choices = app.sidechain_source_choices(device_id);
    let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
    const TAP_POINTS: [TapPoint; 3] = [TapPoint::PreFx, TapPoint::PostFx, TapPoint::PostFader];
    let tap_labels = ["Pre-FX", "Post-FX", "Post-Fdr"];
    let label_w = 34.0;
    let tap_w = 84.0;
    let x0 = rect.x + 8.0;
    let src_x = x0 + label_w;
    let src_w = (rect.w - 8.0 - label_w - tap_w - 10.0).max(40.0);
    let tap_x = src_x + src_w + 4.0;
    for (k, port) in ports.iter().enumerate() {
        let y = rect.y + SC_PAD * 0.5 + k as f32 * SC_PORT_H;
        ui.label_at(("inspector_sc_in", device_id as usize, k), &format!("In {}", port.port + 1), x0, y + 6.0, 11.0, p.text_dim);
        let sel = choices
            .iter()
            .position(|c| c.source == port.source)
            .unwrap_or(0);
        if let Some(picked) = ui.dropdown(
            ("inspector_sc_src", device_id as usize, k),
            Rect { x: src_x, y, w: src_w, h: SC_PORT_H - 2.0 },
            &labels,
            sel,
        ) && let Some(choice) = choices.get(picked)
        {
            let source: Option<TapSource> = choice.source;
            let port_no = port.port;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Device(DeviceEvent::SetSidechainSource { device_id, port: port_no, source }));
            }));
        }
        // 自 track の入力を key にする port は Pre-FX 固定 (dropdown を出さない)。
        if port.source == app.cursor_track_id().map(TapSource::Track) {
            ui.label_at(("inspector_sc_tap_fixed", device_id as usize, k), "Pre-FX", tap_x + 6.0, y + 6.0, 11.0, p.text_dim);
            continue;
        }
        let tap_sel = TAP_POINTS.iter().position(|t| *t == port.tap_point).unwrap_or(2);
        if let Some(picked) = ui.dropdown(
            ("inspector_sc_tap", device_id as usize, k),
            Rect { x: tap_x, y, w: tap_w, h: SC_PORT_H - 2.0 },
            &tap_labels,
            tap_sel,
        ) && let Some(&tp) = TAP_POINTS.get(picked)
        {
            let port_no = port.port;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Device(DeviceEvent::SetAuxInputTapPoint {
                    device_id,
                    port: port_no,
                    tap_point: tp,
                }));
            }));
        }
    }
}
