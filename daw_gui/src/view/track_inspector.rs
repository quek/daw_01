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

    // Vocal source 編集 (Vocal track のときのみ)
    if let Some(track) = app.song.tracks.get(app.cursor_track_index().unwrap_or(0))
        && let common::model::InstrumentSource::Vocal { speaker_id, .. } = &track.source
    {
        ui.label_at(
            "inspector_vocal_label",
            "Vocal Speaker",
            area.x + pad,
            y,
            12.0,
            TEXT_DIM,
        );
        y += 18.0;
        let dropdown_rect = Rect {
            x: area.x + pad,
            y,
            w: area.w - pad * 2.0,
            h: 24.0,
        };
        // singers が空 (engine 未起動 / fetch 失敗) なら placeholder ラベルだけ
        if app.singers.is_empty() {
            ui.label_at(
                "inspector_vocal_placeholder",
                "(VOICEVOX engine 未起動 — speaker 一覧取得待ち)",
                dropdown_rect.x + 4.0,
                dropdown_rect.y + 6.0,
                11.0,
                TEXT_DIM,
            );
        } else {
            // 各 singer の各 style を 1 entry に flatten。
            // 「<キャラ名> - <スタイル名>」 を表示、 selected_idx は speaker_id 一致で決定
            let entries: Vec<(u32, String, String)> = app
                .singers
                .iter()
                .flat_map(|s| {
                    s.styles.iter().map(move |st| {
                        (
                            st.id,
                            s.name.clone(),
                            st.name.clone(),
                        )
                    })
                })
                .collect();
            let labels: Vec<String> = entries
                .iter()
                .map(|(_, n, sn)| format!("{n} - {sn}"))
                .collect();
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let selected_idx = entries
                .iter()
                .position(|(id, _, _)| *id == *speaker_id)
                .unwrap_or(0);
            if let Some(picked) = ui.dropdown(
                "inspector_vocal_dropdown",
                dropdown_rect,
                &label_refs,
                selected_idx,
            ) && let Some((id, _, style_name)) = entries.get(picked)
            {
                let track_idx = app.cursor_track_index().unwrap_or(0) as u32;
                let new_id = *id;
                let new_style = style_name.clone();
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetTrackSpeaker {
                        track: track_idx,
                        speaker_id: new_id,
                        style_name: new_style.clone(),
                    });
                }));
            }
        }
        y += 30.0;
    }

    // ---- Parent group dropdown ---------------------------------------
    // Reaper folder / Live group equivalent. The selected track can
    // optionally be reparented under any other track that already has
    // children (or any track really — the cycle check in
    // `action_set_track_parent` rejects bad picks). Master bus =
    // "(top-level)" sentinel.
    if let Some(track) = app.song.tracks.get(app.cursor_track_index().unwrap_or(0)) {
        // Candidates: tracks that already have at least one child (= are
        // groups in the Reaper-folder sense), excluding the selected
        // track itself and any of its descendants. Picking a non-group
        // track as parent is also valid, but the dropdown only surfaces
        // existing groups — to convert a regular track into a group,
        // the user picks it as parent here and the act of pointing at
        // it makes it one. PR2 phase 1 keeps the simpler "groups only"
        // candidate list; expand later if it surfaces as a friction.
        let groups: Vec<(u32, String)> = app
            .song
            .tracks
            .iter()
            .filter(|t| app.is_group_track(t.id) && t.id != track.id)
            .map(|t| (t.id, if t.name.is_empty() { format!("Group {}", t.id) } else { t.name.clone() }))
            .collect();

        ui.label_at(
            "inspector_parent_label",
            "Parent",
            area.x + pad,
            y,
            12.0,
            TEXT_DIM,
        );
        y += 18.0;

        let dropdown_rect = Rect {
            x: area.x + pad,
            y,
            w: area.w - pad * 2.0,
            h: 24.0,
        };

        // Build option list: "(top-level)" then every other group track.
        let mut labels: Vec<String> = Vec::with_capacity(groups.len() + 1);
        labels.push("(top-level)".into());
        labels.extend(groups.iter().map(|(_, n)| n.clone()));
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();

        let selected_idx = match track.parent_group_id {
            None => 0,
            Some(pid) => groups
                .iter()
                .position(|(id, _)| *id == pid)
                .map(|i| i + 1)
                .unwrap_or(0),
        };

        if let Some(picked) = ui.dropdown(
            "inspector_parent_dropdown",
            dropdown_rect,
            &label_refs,
            selected_idx,
        ) {
            let new_parent = if picked == 0 {
                None
            } else {
                groups.get(picked - 1).map(|(id, _)| *id)
            };
            let track_id = track.id;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetTrackParent {
                    track_id,
                    parent_id: new_parent,
                });
            }));
        }
        y += 30.0;
    }

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

    // 下端: + Inst / + FX / + MIDI FX。Reaper folder 流で group track
    // も全機能を持てる仕様 (plan_group_track.md §1)、よって group も
    // 普通 track と同じ 3 ボタン表示。
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
