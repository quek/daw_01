//! Plugin picker (modal overlay)。半透明背景 + 中央パネル + plugin リスト。
//!
//! is_plugin_picker_open == true のときだけ root.rs から呼ぶ。

use daw_ui_core::{Edit, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent};

const COLOR_OVERLAY: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 0.6 };
const COLOR_PANEL_BG: Color = Color { r: 0.18, g: 0.18, b: 0.22, a: 1.0 };
const COLOR_TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const COLOR_TEXT_DIM: Color = Color { r: 0.65, g: 0.68, b: 0.72, a: 1.0 };
const COLOR_TEXT_FORMAT: Color = Color { r: 0.55, g: 0.78, b: 0.95, a: 1.0 };
const COLOR_ROW_BG: Color = Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 };

const PANEL_W: f32 = 520.0;
const PANEL_H: f32 = 460.0;
const TITLE_H: f32 = 36.0;
const ROW_H: f32 = 26.0;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: PhysicalSize) {
    let sw = screen.width as f32;
    let sh = screen.height as f32;

    // 半透明全画面オーバーレイ
    ui.heavy("pp_overlay", |hctx| {
        hctx.cached((sw.to_bits(), sh.to_bits()), |hctx| {
            hctx.push_rect(RectCommand {
                rect: Rect { x: 0.0, y: 0.0, w: sw, h: sh },
                fill: COLOR_OVERLAY,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });

    let panel_x = (sw - PANEL_W) * 0.5;
    let panel_y = (sh - PANEL_H) * 0.5;
    let panel = Rect { x: panel_x, y: panel_y, w: PANEL_W, h: PANEL_H };

    // パネル背景
    ui.heavy("pp_panel_bg", |hctx| {
        hctx.cached(0u8, |hctx| {
            hctx.push_rect(RectCommand {
                rect: panel,
                fill: COLOR_PANEL_BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [6.0; 4],
                clip_rect: None,
            });
        });
    });

    let pad = 12.0;
    // タイトル + Rescan / Close
    ui.label_at(
        "pp_title",
        "Select Plugin",
        panel_x + pad,
        panel_y + pad,
        16.0,
        COLOR_TEXT,
    );
    let rescan_w = 90.0;
    let close_w = 32.0;
    let rescan_x = panel_x + PANEL_W - pad - rescan_w - 6.0 - close_w;
    let close_x = panel_x + PANEL_W - pad - close_w;
    let is_rescanning = app.is_rescanning;
    ui.button_at(
        "pp_rescan",
        if is_rescanning { "Rescanning" } else { "Rescan" },
        Rect { x: rescan_x, y: panel_y + pad - 2.0, w: rescan_w, h: 24.0 },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::RescanPluginDb)),
    );
    ui.button_at(
        "pp_close",
        "x",
        Rect { x: close_x, y: panel_y + pad - 2.0, w: close_w, h: 24.0 },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ClosePluginPicker)),
    );

    // 一覧 (リストを下方向に配置)
    let list_x = panel_x + pad;
    let list_y = panel_y + TITLE_H + 6.0;
    let list_w = PANEL_W - pad * 2.0;
    let list_h = PANEL_H - TITLE_H - 6.0 - pad;
    let visible = &app.plugin_picker_visible;
    if visible.is_empty() {
        ui.label_at(
            "pp_empty",
            "(\u{8a72}\u{5f53}\u{30d7}\u{30e9}\u{30b0}\u{30a4}\u{30f3}\u{306a}\u{3057})",
            list_x,
            list_y + 8.0,
            12.0,
            COLOR_TEXT_DIM,
        );
        return;
    }

    let max_rows = (list_h / (ROW_H + 2.0)).floor() as usize;
    for (i, entry) in visible.iter().take(max_rows).enumerate() {
        let row_y = list_y + (i as f32) * (ROW_H + 2.0);
        let row_rect = Rect { x: list_x, y: row_y, w: list_w, h: ROW_H };
        let id_clone = entry.id.clone();

        // 行背景
        ui.heavy(("pp_row_bg", i), |hctx| {
            hctx.cached(i, |hctx| {
                hctx.push_rect(RectCommand {
                    rect: row_rect,
                    fill: COLOR_ROW_BG,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [3.0; 4],
                    clip_rect: None,
                });
            });
        });

        // 名前 (clickable button) — 行全幅
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

        // ベンダ + フォーマットラベル overlay (button のテキストの右側、装飾のみ)
        ui.label_at(
            ("pp_row_vendor", i),
            &entry.vendor,
            list_x + list_w - 200.0,
            row_y + 6.0,
            10.0,
            COLOR_TEXT_DIM,
        );
        ui.label_at(
            ("pp_row_format", i),
            &entry.format_label,
            list_x + list_w - 50.0,
            row_y + 6.0,
            10.0,
            COLOR_TEXT_FORMAT,
        );
    }

    if visible.len() > max_rows {
        ui.label_at(
            "pp_more",
            &format!(
                "({} \u{4ef6}\u{8868}\u{793a}中 / 全 {} 件)",
                max_rows,
                visible.len()
            ),
            list_x,
            panel_y + PANEL_H - pad - 12.0,
            10.0,
            COLOR_TEXT_DIM,
        );
    }
}
