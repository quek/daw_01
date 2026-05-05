//! Plugin picker (modal overlay)。`Ui::modal` + `Ui::list_view` で構築。
//!
//! root.rs から常時呼ばれる。app.is_plugin_picker_open == true のときに
//! modal を開き、ESC / outside click / Close ボタンで閉じる。

use daw_ui_core::{Edit, ListViewStyle, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent};

const COLOR_TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const COLOR_TEXT_DIM: Color = Color { r: 0.65, g: 0.68, b: 0.72, a: 1.0 };
const COLOR_TEXT_FORMAT: Color = Color { r: 0.55, g: 0.78, b: 0.95, a: 1.0 };

const PANEL_W: f32 = 520.0;
const PANEL_H: f32 = 460.0;
const TITLE_H: f32 = 36.0;

const MODAL_STYLE: ModalStyle = ModalStyle {
    overlay_color: Color { r: 0.0, g: 0.0, b: 0.0, a: 0.6 },
    panel_bg: Color { r: 0.18, g: 0.18, b: 0.22, a: 1.0 },
    panel_radius: 6.0,
    close_on_outside_click: true,
    close_on_escape: true,
};

const LIST_STYLE: ListViewStyle = ListViewStyle {
    row_height: 26.0,
    row_gap: 2.0,
    row_bg: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    row_bg_hover: Color { r: 0.27, g: 0.27, b: 0.32, a: 1.0 },
    row_bg_selected: Color { r: 0.32, g: 0.55, b: 0.85, a: 1.0 },
    radius: 3.0,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    if app.is_plugin_picker_open && !ui.is_modal_open("plugin_picker") {
        ui.open_modal("plugin_picker");
    }

    ui.modal(
        "plugin_picker",
        (PANEL_W, PANEL_H),
        &MODAL_STYLE,
        Some(Box::new(|| {
            Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ClosePluginPicker))
        })),
        |ui, panel| {
            let pad = 12.0;

            // タイトル + Rescan / Close
            ui.label_at(
                "pp_title",
                "Select Plugin",
                panel.x + pad,
                panel.y + pad,
                16.0,
                COLOR_TEXT,
            );
            let rescan_w = 90.0;
            let close_w = 32.0;
            let rescan_x = panel.x + panel.w - pad - rescan_w - 6.0 - close_w;
            let close_x = panel.x + panel.w - pad - close_w;
            let is_rescanning = app.is_rescanning;
            ui.button_at(
                "pp_rescan",
                if is_rescanning { "Rescanning" } else { "Rescan" },
                Rect { x: rescan_x, y: panel.y + pad - 2.0, w: rescan_w, h: 24.0 },
                || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::RescanPluginDb)),
            );
            // ✕ ボタンは button_at_clicked + close_modal で閉じる。
            // close_modal が popup state を直接 remove → modal の on_close (上で渡した)
            // が次フレームで AppEvent::ClosePluginPicker を 1 度発火するため、 button click
            // 経路では Edit を発行しない (二重発火回避)。 gui_01 conversation #015 参照。
            if ui.button_at_clicked(
                "pp_close",
                "x",
                Rect { x: close_x, y: panel.y + pad - 2.0, w: close_w, h: 24.0 },
            ) {
                ui.close_modal("plugin_picker");
            }

            // 一覧
            let list_rect = Rect {
                x: panel.x + pad,
                y: panel.y + TITLE_H + 6.0,
                w: panel.w - pad * 2.0,
                h: panel.h - TITLE_H - 6.0 - pad,
            };
            let visible = &app.plugin_picker_visible;
            if visible.is_empty() {
                ui.label_at(
                    "pp_empty",
                    "(\u{8a72}\u{5f53}\u{30d7}\u{30e9}\u{30b0}\u{30a4}\u{30f3}\u{306a}\u{3057})",
                    list_rect.x,
                    list_rect.y + 8.0,
                    12.0,
                    COLOR_TEXT_DIM,
                );
                return;
            }

            ui.list_view(
                "pp_list",
                list_rect,
                visible,
                None,
                &LIST_STYLE,
                |ui, entry, i, row_rect, _selected| {
                    let id_clone = entry.id.clone();
                    ui.button_at(
                        ("pp_row_btn", i),
                        &entry.name,
                        row_rect,
                        move || {
                            let id_clone = id_clone.clone();
                            Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::SelectPluginFromDb(id_clone))
                            })
                        },
                    );
                    ui.label_at(
                        ("pp_row_vendor", i),
                        &entry.vendor,
                        row_rect.x + row_rect.w - 200.0,
                        row_rect.y + 6.0,
                        10.0,
                        COLOR_TEXT_DIM,
                    );
                    ui.label_at(
                        ("pp_row_format", i),
                        &entry.format_label,
                        row_rect.x + row_rect.w - 50.0,
                        row_rect.y + 6.0,
                        10.0,
                        COLOR_TEXT_FORMAT,
                    );
                },
            );
        },
    );
}
