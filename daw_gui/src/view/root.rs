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
    arrangement_view, bottom_panel, plugin_picker, recovery_modal, status_bar, track_inspector,
    transport,
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
    dispatch_shortcuts(app, ui, bottom_rect);

    draw_menu_bar(ui, menu_rect);
    transport::draw(app, ui, transport_rect);
    track_inspector::draw(app, ui, inspector_rect);
    arrangement_view::draw(app, ui, arrangement_rect);
    bottom_panel::draw(app, ui, bottom_rect);
    status_bar::draw(app, ui, status_rect);

    // Modal: plugin picker。draw 関数内で modal の open/close を app.is_plugin_picker_open
    // と同期させる (常時呼び、内部で is_modal_open / open_modal を管理)。
    plugin_picker::draw(app, ui, screen);

    // Modal: recovery (起動時 or Open 時に検出された autosave 候補)。
    // app.show_recovery_modal を internal で監視するため常時呼び。
    recovery_modal::draw(app, ui, screen);
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
            m.item("Import Audio...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::OpenImportAudioDialog)
                }));
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
///
/// `bottom_rect` は piano_roll active 判定用。マウスが bottom_panel 領域内 + Piano Roll
/// タブが選択中なら G/X/1/2/3 を piano_roll 系に流す。それ以外は arrange 系。
fn dispatch_shortcuts(app: &AppData, ui: &mut Ui<'_, AppData>, bottom_rect: Rect) {
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
    // Ableton Live's Cmd/Ctrl+G — group the selected tracks. gui_01
    // #016 で arrangement widget が track header の Shift/Ctrl クリック
    // 多重選択を実装したので、 selection は `selected_track_ids` から
    // 直接取れる。 空なら no-op。
    if ui.take_shortcut("daw.group_tracks") {
        let track_ids = app.selected_track_ids.clone();
        if !track_ids.is_empty() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::GroupSelectedTracks { track_ids });
            }));
        }
    }
    // Alt+G — ungroup the selected group tracks (Ableton Live の
    // Cmd/Ctrl+Shift+G に相当、 本 DAW はユーザー指定で Alt+G)。
    if ui.take_shortcut("daw.ungroup_tracks") {
        let track_ids = app.selected_track_ids.clone();
        if !track_ids.is_empty() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::UngroupTracks { track_ids });
            }));
        }
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

    // ----- Grid snap / fit -----
    // active view 判定: マウスが bottom_panel 領域内 AND Piano Roll タブ選択中なら
    // piano_roll 系、それ以外 (arrangement 領域 / Mixer タブ等) は arrange 系へ。
    // タブ選択だけで判定すると、Piano Roll タブを開いたまま arrangement を
    // 操作している時もショートカットが piano_roll 側に流れてしまう。
    // focus 中の text_input は ShortcutMap 側で is_typing_only 判定により抑止される。
    let pointer_in_bottom = ui
        .pointer()
        .pos
        .is_some_and(|(px, py)| bottom_rect.contains(px, py));
    let is_pianoroll_active = app.bottom_panel == 1 && pointer_in_bottom;
    if ui.take_shortcut("daw.toggle_snap") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_pianoroll_active {
                app.handle_event(AppEvent::TogglePianoRollSnap);
            } else {
                app.handle_event(AppEvent::ToggleArrangeSnap);
            }
        }));
    }
    if ui.take_shortcut("daw.fit_view") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_pianoroll_active {
                app.handle_event(AppEvent::FitPianoRollToClip);
            } else {
                app.handle_event(AppEvent::FitArrangeToContent);
            }
        }));
    }
    if ui.take_shortcut("daw.narrow_grid") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_pianoroll_active {
                app.handle_event(AppEvent::NarrowPianoRollGrid);
            } else {
                app.handle_event(AppEvent::NarrowArrangeGrid);
            }
        }));
    }
    if ui.take_shortcut("daw.widen_grid") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_pianoroll_active {
                app.handle_event(AppEvent::WidenPianoRollGrid);
            } else {
                app.handle_event(AppEvent::WidenArrangeGrid);
            }
        }));
    }
    if ui.take_shortcut("daw.toggle_triplet") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if is_pianoroll_active {
                app.handle_event(AppEvent::TogglePianoRollTriplet);
            } else {
                app.handle_event(AppEvent::ToggleArrangeTriplet);
            }
        }));
    }

    // ----- Clip duplicate (gui_01 #019) -----
    // D / Alt+D で選択中 clip の末尾直後に共有/独立コピーを生成。
    // 連打すると前回コピーが新たな選択になり、 後ろに連続して並ぶ
    // (REAPER / Ableton の Ctrl+D 流)。 複数選択中は各々の末尾直後に
    // 並列生成。 selected_clip が None なら no-op。
    if ui.take_shortcut("daw.duplicate_clip_shared") {
        let sources: Vec<crate::app::ClipRef> = app.selected_clips.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            for src in &sources {
                app.handle_event(AppEvent::DuplicateClipShared { source: *src });
            }
        }));
    }
    if ui.take_shortcut("daw.duplicate_clip_unique") {
        let sources: Vec<crate::app::ClipRef> = app.selected_clips.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            for src in &sources {
                app.handle_event(AppEvent::DuplicateClipUnique { source: *src });
            }
        }));
    }

    // ----- Split (E) / Glue (J) — Phase 1 PR7 -------------------------------
    // MIDI / Audio / Vocal すべての clip kind に対して動作する統合操作。
    // 詳細は `docs/plan_audio_clip.md` §3.3。
    if ui.take_shortcut("daw.split_clip_at_cursor") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::SplitClipAtPlayhead { snap: true });
        }));
    }
    if ui.take_shortcut("daw.split_clip_at_cursor_no_snap") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::SplitClipAtPlayhead { snap: false });
        }));
    }
    if ui.take_shortcut("daw.glue_selected_clips") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::GlueSelectedClips);
        }));
    }

    // ----- Help -----
    if ui.take_shortcut("daw.toggle_help") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleHelp)
        }));
    }
    // Phase 2 PR-D 段階 1: Audio Editor 開いているとき Ctrl+D で
    // 選択中 event を Duplicate。 audio_editor_clip is None のときは
    // 消費しない (= 既存 D / Alt+D の clip duplicate と紛らわしくない
    // よう、 Audio Editor 内限定の shortcut として gate する)。
    if app.audio_editor_clip.is_some() && ui.take_shortcut("daw.duplicate_audio_event") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::DuplicateAudioEditorEvent)
        }));
    }
    // PR-D 段階 2: Audio Editor 内 event 選択 navigation (Ctrl+] / Ctrl+[)。
    // 現選択 idx を ±1 wrap-around で移動。 audio_editor が開いてないと
    // 無効、 events が空なら no-op。
    if app.audio_editor_clip.is_some() && ui.take_shortcut("daw.next_audio_event") {
        let next = app.next_audio_editor_event_idx(1);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SelectAudioEditorEvent(next))
        }));
    }
    if app.audio_editor_clip.is_some() && ui.take_shortcut("daw.prev_audio_event") {
        let prev = app.next_audio_editor_event_idx(-1);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SelectAudioEditorEvent(prev))
        }));
    }
    // modal が開いている間は escape を消費しない (modal 側で close する)。
    // 優先度: rename mode → Audio Editor close → CloseHelp。 rename と
    // Audio Editor は同時には開かない (= rename は track header の上、
    // Audio Editor は bottom panel の中) ので順番でも実用 OK。
    if !app.is_plugin_picker_open && ui.take_shortcut("escape") {
        if app.track_rename_idx.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CancelRenameTrack)
            }));
        } else if app.audio_editor_clip.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CloseAudioEditor)
            }));
        } else {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CloseHelp)
            }));
        }
    }
}
