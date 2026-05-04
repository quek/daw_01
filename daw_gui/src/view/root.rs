//! ルート view: 画面全体を Transport / Inspector / Arrangement / BottomPanel /
//! StatusBar に分割し、各 sub view を呼ぶ。Plugin picker / help は modal overlay。
//!
//! build_root の末尾で `Ui::take_shortcut` を順に消費し、AppEvent (or
//! `Ui::request_undo` 等) に変換する。global shortcut の dispatch はここに集約。

use daw_ui_core::{Edit, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent};
use crate::view::{
    arrangement_view, bottom_panel, plugin_picker, status_bar, track_inspector, transport,
};

pub const MENU_H: f32 = 24.0;
pub const TRANSPORT_H: f32 = 44.0;
pub const STATUS_H: f32 = 24.0;
pub const BOTTOM_H: f32 = 240.0;
pub const INSPECTOR_W: f32 = 280.0;

pub fn build_root(app: &AppData, ui: &mut Ui<'_, AppData>, screen: PhysicalSize) {
    let sw = screen.width as f32;
    let sh = screen.height as f32;

    // 全画面背景。
    ui.panel(
        "root_bg",
        Rect { x: 0.0, y: 0.0, w: sw, h: sh },
        Color::rgb(0.10, 0.10, 0.12),
        0.0,
    );

    // ----- レイアウト計算 -----
    let menu_rect = Rect { x: 0.0, y: 0.0, w: sw, h: MENU_H };
    let transport_rect = Rect { x: 0.0, y: MENU_H, w: sw, h: TRANSPORT_H };
    let header_h = MENU_H + TRANSPORT_H;
    let bottom_rect_h = BOTTOM_H.min((sh - header_h - STATUS_H - 100.0).max(120.0));
    let center_h = (sh - header_h - bottom_rect_h - STATUS_H).max(0.0);
    let inspector_rect = Rect {
        x: 0.0,
        y: header_h,
        w: INSPECTOR_W,
        h: center_h,
    };
    let arrangement_rect = Rect {
        x: INSPECTOR_W,
        y: header_h,
        w: (sw - INSPECTOR_W).max(0.0),
        h: center_h,
    };
    let bottom_rect = Rect {
        x: 0.0,
        y: header_h + center_h,
        w: sw,
        h: bottom_rect_h,
    };
    let status_rect = Rect {
        x: 0.0,
        y: sh - STATUS_H,
        w: sw,
        h: STATUS_H,
    };

    // gui_01 widget (piano_roll 等) は `take_shortcut` を消費する側面があるため、
    // 先に root レベルで shortcut を捌いて広域の挙動を確定させる。widget 描画時には
    // 消費済みになり、widget 内蔵の同名 shortcut handler は no-op に縮退する。
    dispatch_shortcuts(app, ui);

    draw_menu_bar(ui, menu_rect);
    transport::draw(app, ui, transport_rect);
    track_inspector::draw(app, ui, inspector_rect);
    arrangement_view::draw(app, ui, arrangement_rect);
    bottom_panel::draw(app, ui, bottom_rect);
    status_bar::draw(app, ui, status_rect);

    // Modal: plugin picker。draw 関数内で modal の open/close を app.is_plugin_picker_open
    // と同期させる (常時呼び、内部で is_modal_open / open_modal を管理)。
    plugin_picker::draw(app, ui, screen);
}

/// 上部 menu bar (File / Edit / View) を library widget で描画。
fn draw_menu_bar(ui: &mut Ui<'_, AppData>, rect: Rect) {
    // M9 P1-5 (gui_01 側 breaking 変更): on_click closure に &mut Ui が渡る形に。
    ui.menu_bar(rect, |mb| {
        mb.menu("File", |m| {
            m.item("New", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::New)));
            });
            m.item("Open...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::Open)));
            });
            m.item("Save", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::Save)));
            });
            m.item("Save As...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::SaveAs)));
            });
            m.item("Export WAV...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ExportWav)));
            });
        });
        mb.menu("Edit", |m| {
            m.item("Undo", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::Undo)));
            });
            m.item("Redo", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::Redo)));
            });
            m.item("Delete", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::DeleteSelectedNotes);
                    app.handle_event(AppEvent::DeleteSelectedClip);
                }));
            });
        });
        mb.menu("View", |m| {
            m.item("Toggle Help", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ToggleHelp)));
            });
        });
    });
}

/// `ShortcutMap` ルックアップで判定済みの shortcut name を pull して AppEvent / undo
/// 要求に変換する。`Ui::take_shortcut` は 1 度だけ消費するので、各 name について
/// この関数で一括処理する。`app` は immut で受けて、コピーや状態判定のみで使う
/// (mutation は `Ui::push_edit(Edit::mutate(...))` 経由)。
fn dispatch_shortcuts(app: &AppData, ui: &mut Ui<'_, AppData>) {
    // ----- Transport -----
    if ui.take_shortcut("daw.play_toggle") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::PlayToggle)
        }));
    }
    if ui.take_shortcut("daw.toggle_loop") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleLoop)
        }));
    }
    if ui.take_shortcut("daw.synthesize_vocal") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::SynthesizeVocal)
        }));
    }

    // ----- File -----
    if ui.take_shortcut("new") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::New)
        }));
    }
    if ui.take_shortcut("open") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Open)
        }));
    }
    if ui.take_shortcut("save") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Save)
        }));
    }
    if ui.take_shortcut("save_as") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::SaveAs)
        }));
    }
    if ui.take_shortcut("daw.export_wav") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ExportWav)
        }));
    }

    // ----- Edit -----
    // Task A 段階: 既存の自前 Undo/Redo (AppEvent::Undo/Redo) に流す。
    // Task B で `ui.request_undo()` / `ui.request_redo()` に切り替える。
    if ui.take_shortcut("undo") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Undo)
        }));
    }
    if ui.take_shortcut("redo") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Redo)
        }));
    }
    if ui.take_shortcut("copy")
        && let Some((json, count)) = app.copy_selected_notes_as_json()
    {
        ui.set_clipboard_text(json);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.status_message = format!("コピー: {count} ノート");
        }));
    }
    if ui.take_shortcut("paste")
        && let Some(text) = ui.take_clipboard_paste()
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.paste_notes_from_json(&text);
        }));
    }
    if ui.take_shortcut("delete") {
        // ノート選択があればノート削除、無ければ clip 削除。
        // 両方 dispatch するとノート選択中でも clip が消えてしまう。
        let has_notes = !app.selected_notes.is_empty();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if has_notes {
                app.handle_event(AppEvent::DeleteSelectedNotes);
            } else {
                app.handle_event(AppEvent::DeleteSelectedClip);
            }
        }));
    }

    // ----- Help -----
    if ui.take_shortcut("daw.toggle_help") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleHelp)
        }));
    }
    // modal が開いている間は escape を消費しない (modal 側で close する)。
    if !app.is_plugin_picker_open && ui.take_shortcut("escape") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::CloseHelp)
        }));
    }
}
