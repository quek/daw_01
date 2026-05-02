//! トラック inspector (左サイドバー):
//! - 選択トラック名
//! - 「Chain」見出し
//! - MIDI FX → Instrument → FX のリスト (各行に GUI / × ボタン)
//! - + Instrument / + Effect / + MIDI FX ボタン

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent, PickerTarget};

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const TEXT_DIM: Color = Color { r: 0.62, g: 0.65, b: 0.70, a: 1.0 };
const ROW_BG: Color = Color { r: 0.20, g: 0.20, b: 0.24, a: 1.0 };
const SECTION_TEXT: Color = Color { r: 0.55, g: 0.62, b: 0.78, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    // 背景
    ui.heavy("inspector_bg", |hctx| {
        hctx.cached((area.w.to_bits(), area.h.to_bits()), |hctx| {
            hctx.push_rect(RectCommand {
                rect: area,
                fill: BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });

    let pad = 12.0;
    let mut y = area.y + pad;

    // 選択トラック名
    ui.label_at(
        "inspector_title",
        &app.selected_track_label(),
        area.x + pad,
        y,
        16.0,
        TEXT,
    );
    y += 28.0;

    // 「Chain」見出し
    ui.label_at(
        "inspector_chain_label",
        "Chain",
        area.x + pad,
        y,
        12.0,
        TEXT_DIM,
    );
    y += 18.0;

    // chain entries
    let chain = app.inspector_chain();
    let row_h = 26.0;
    let row_w = area.w - pad * 2.0;
    let btn_gui_w = 44.0;
    let btn_x_w = 30.0;

    for (idx, entry) in chain.iter().enumerate() {
        let row_y = y;
        if row_y + row_h > area.y + area.h - 70.0 {
            // Add ボタン用スペースを残す。
            break;
        }

        // 行背景
        ui.heavy(("inspector_row_bg", idx), |hctx| {
            hctx.cached((idx, entry.section_label.clone(), entry.plugin_name.clone()), |hctx| {
                hctx.push_rect(RectCommand {
                    rect: Rect { x: area.x + pad, y: row_y, w: row_w, h: row_h },
                    fill: ROW_BG,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [3.0; 4],
                    clip_rect: None,
                });
            });
        });

        // セクションラベル (MIDI FX / Instrument / FX)
        ui.label_at(
            ("inspector_row_section", idx),
            &entry.section_label,
            area.x + pad + 6.0,
            row_y + 8.0,
            10.0,
            SECTION_TEXT,
        );
        // プラグイン名
        ui.label_at(
            ("inspector_row_name", idx),
            &entry.plugin_name,
            area.x + pad + 60.0,
            row_y + 8.0,
            11.0,
            TEXT,
        );

        // GUI ボタン
        let gui_x = area.x + pad + row_w - btn_gui_w - btn_x_w - 4.0;
        let kind = entry.slot_kind;
        let index = entry.slot_index;
        ui.button_at(
            ("inspector_row_gui", idx),
            "GUI",
            Rect { x: gui_x, y: row_y + 2.0, w: btn_gui_w, h: row_h - 4.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleSlotGui {
                        slot_kind: kind,
                        slot_index: index,
                    })
                })
            },
        );

        // × (削除) ボタン
        let xb_x = area.x + pad + row_w - btn_x_w;
        ui.button_at(
            ("inspector_row_remove", idx),
            "x",
            Rect { x: xb_x, y: row_y + 2.0, w: btn_x_w, h: row_h - 4.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::RemoveSlot {
                        slot_kind: kind,
                        slot_index: index,
                    })
                })
            },
        );

        y += row_h + 3.0;
    }

    // 下端: + Instrument / + FX / + MIDI FX
    let btns_h = 26.0;
    let btns_y = area.y + area.h - btns_h - pad;
    let btn_w = (area.w - pad * 2.0 - 12.0) / 3.0;

    ui.button_at(
        "inspector_add_inst",
        "+ Inst",
        Rect { x: area.x + pad, y: btns_y, w: btn_w, h: btns_h },
        || {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Instrument))
            })
        },
    );
    ui.button_at(
        "inspector_add_fx",
        "+ FX",
        Rect { x: area.x + pad + btn_w + 6.0, y: btns_y, w: btn_w, h: btns_h },
        || {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Fx))
            })
        },
    );
    ui.button_at(
        "inspector_add_midi_fx",
        "+ MIDI",
        Rect {
            x: area.x + pad + (btn_w + 6.0) * 2.0,
            y: btns_y,
            w: btn_w,
            h: btns_h,
        },
        || {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::MidiFx))
            })
        },
    );
}
