//! トラック inspector (左サイドバー):
//! - 選択トラック名
//! - 「Chain」見出し
//! - MIDI FX → Instrument → FX のリスト (各行に GUI / × ボタン、drag&drop で reorder)
//! - + Instrument / + Effect / + MIDI FX ボタン

use daw_ui_core::{
    Edit, ReorderableListEditRequest, ReorderableListStyle, Ui,
};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent, PickerTarget};

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const TEXT_DIM: Color = Color { r: 0.62, g: 0.65, b: 0.70, a: 1.0 };
const ROW_BG: Color = Color { r: 0.20, g: 0.20, b: 0.24, a: 1.0 };
const ROW_BG_HOVER: Color = Color { r: 0.24, g: 0.24, b: 0.30, a: 1.0 };
const ROW_BG_DRAGGING: Color = Color { r: 0.30, g: 0.40, b: 0.55, a: 0.85 };
const SECTION_TEXT: Color = Color { r: 0.55, g: 0.62, b: 0.78, a: 1.0 };
const DROP_INDICATOR: Color = Color { r: 0.55, g: 0.78, b: 0.95, a: 1.0 };

const CHAIN_LIST_STYLE: ReorderableListStyle = ReorderableListStyle {
    row_height: 26.0,
    row_gap: 3.0,
    row_bg: ROW_BG,
    row_bg_hover: ROW_BG_HOVER,
    row_bg_selected: ROW_BG,
    row_bg_dragging: ROW_BG_DRAGGING,
    drop_indicator_color: DROP_INDICATOR,
    drop_indicator_h: 2.0,
    radius: 3.0,
    drag_handle_w: 0.0,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("inspector_bg", area, BG, 0.0);

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

    let chain = app.inspector_chain();
    let btns_h = 26.0;
    let btns_y = area.y + area.h - btns_h - pad;

    let list_x = area.x + pad;
    let list_y = y;
    let list_w = area.w - pad * 2.0;
    let list_h = (btns_y - 6.0 - list_y).max(0.0);
    let list_rect = Rect { x: list_x, y: list_y, w: list_w, h: list_h };

    let btn_gui_w = 44.0;
    let btn_x_w = 30.0;

    ui.reorderable_list(
        "inspector_chain",
        list_rect,
        &chain,
        None,
        &CHAIN_LIST_STYLE,
        |req| match req {
            ReorderableListEditRequest::Reorder(order) => Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ReorderInspectorChain(order.clone()));
            }),
        },
        |ui, entry, idx, row_rect, _selected, _dragging| {
            ui.label_at(
                ("inspector_row_section", idx),
                &entry.section_label,
                row_rect.x + 6.0,
                row_rect.y + 8.0,
                10.0,
                SECTION_TEXT,
            );
            ui.label_at(
                ("inspector_row_name", idx),
                &entry.plugin_name,
                row_rect.x + 60.0,
                row_rect.y + 8.0,
                11.0,
                TEXT,
            );
            let kind = entry.slot_kind;
            let index = entry.slot_index;
            let gui_x = row_rect.x + row_rect.w - btn_gui_w - btn_x_w - 4.0;
            ui.button_at(
                ("inspector_row_gui", idx),
                "GUI",
                Rect {
                    x: gui_x,
                    y: row_rect.y + 2.0,
                    w: btn_gui_w,
                    h: row_rect.h - 4.0,
                },
                move || {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::ToggleSlotGui {
                            slot_kind: kind,
                            slot_index: index,
                        })
                    })
                },
            );
            let xb_x = row_rect.x + row_rect.w - btn_x_w;
            ui.button_at(
                ("inspector_row_remove", idx),
                "x",
                Rect {
                    x: xb_x,
                    y: row_rect.y + 2.0,
                    w: btn_x_w,
                    h: row_rect.h - 4.0,
                },
                move || {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::RemoveSlot {
                            slot_kind: kind,
                            slot_index: index,
                        })
                    })
                },
            );
        },
    );

    // 下端: + Instrument / + FX / + MIDI FX
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
