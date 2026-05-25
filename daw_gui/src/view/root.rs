//! ルート view: 画面全体を Transport / Inspector / Arrangement / BottomPanel /
//! StatusBar に分割し、各 sub view を呼ぶ。Plugin picker / help は modal overlay。
//!
//! build_root の末尾で `Ui::take_shortcut` を順に消費し、AppEvent (or
//! `Ui::request_undo` 等) に変換する。global shortcut の dispatch はここに集約。

use daw_ui_core::{Edit, Orientation, Ui};
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
pub const INSPECTOR_W: f32 = 280.0;

/// arrangement (top) と bottom_panel (= piano_roll / mixer / audio_editor) の
/// 初期分割比率。 上が `default_ratio`、 下が `1.0 - default_ratio`。 0.65 で
/// 旧 BOTTOM_H = 240 / 典型 sh = 720 とおおよそ等価。 ユーザーが境界 handle
/// を drag すると gui_01 `split_view` widget が state に新比率を持って frame
/// 越しに保持する (= session 内のみ persist、 project save 不対応は別 phase)。
const ARRANGEMENT_SPLIT_DEFAULT_RATIO: f32 = 0.65;

pub fn build_root<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, screen: PhysicalSize) {
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
    let center_bottom_rect = Rect {
        x: 0.0,
        y: header_h,
        w: sw,
        h: (sh - header_h - STATUS_H).max(0.0),
    };
    let status_rect = Rect {
        x: 0.0,
        y: sh - STATUS_H,
        w: sw,
        h: STATUS_H,
    };

    draw_menu_bar(app, ui, menu_rect);
    transport::draw(app, ui, transport_rect);

    // arrangement (上) と bottom_panel (下) を縦分割。 gui_01 `split_view`
    // が 6px の handle を描画して drag 入力を扱う。 inspector は top pane
    // 内の左帯として配置 (= 旧レイアウトと互換、 bottom panel は full width
    // を維持)。 shortcut dispatch は bottom_rect を確定させてから呼びたいので
    // closure 内で実行。
    ui.split_view(
        "root_arrange_bottom",
        center_bottom_rect,
        Orientation::Vertical,
        ARRANGEMENT_SPLIT_DEFAULT_RATIO,
        |ui, top_rect, bottom_rect| {
            let inspector_rect = Rect {
                x: top_rect.x,
                y: top_rect.y,
                w: INSPECTOR_W,
                h: top_rect.h,
            };
            let arrangement_rect = Rect {
                x: top_rect.x + INSPECTOR_W,
                y: top_rect.y,
                w: (top_rect.w - INSPECTOR_W).max(0.0),
                h: top_rect.h,
            };

            // gui_01 widget (piano_roll 等) は `take_shortcut` を消費する側面が
            // あるため、 先に root レベルで shortcut を捌いて広域の挙動を確定
            // させる。 widget 描画時には消費済みになり、 widget 内蔵の同名
            // shortcut handler は no-op に縮退する。
            dispatch_shortcuts(app, ui, bottom_rect);

            track_inspector::draw(app, ui, inspector_rect);
            arrangement_view::draw(app, ui, arrangement_rect);
            bottom_panel::draw(app, ui, bottom_rect);
        },
    );

    status_bar::draw(app, ui, status_rect);

    // Modal: plugin picker。draw 関数内で modal の open/close を app.is_plugin_picker_open
    // と同期させる (常時呼び、内部で is_modal_open / open_modal を管理)。
    plugin_picker::draw(app, ui, screen);

    // Modal: recovery (起動時 or Open 時に検出された autosave 候補)。
    // app.show_recovery_modal を internal で監視するため常時呼び。
    recovery_modal::draw(app, ui, screen);
}

/// 上部 menu bar (File / Edit / View) を library widget で描画。
/// `Ui<'a, AppData>` の `'a` は `&AppData` borrow 寿命と同一なので、
/// `app: &'a AppData` を明示して menu の dynamic label (= `&app.recent_files_labels[i]`)
/// が `'a` に乗ることを borrow checker に伝える。
fn draw_menu_bar<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, rect: Rect) {
    // 「最近開いた / 保存した」 ファイルの label / path は AppData に
    // キャッシュ済 (= `recent_files_labels` / `recent_saved_labels` /
    // `recent_files.paths` / `recent_saved.paths`)。 menu_bar API が
    // `label: &'a str` を要求し 'a が `&AppData` の borrow と一致するため、
    // AppData 側で String を持つことで lifetime が解決する (= 別解として
    // String::leak で 'static 化する手もあるが per-frame leak になるので不可)。

    // M9 P1-5 (gui_01 側 breaking 変更): on_click closure に &mut Ui が渡る形に。
    ui.menu_bar(rect, |mb| {
        mb.menu("File", |m| {
            m.item("New", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::New)));
            });
            m.item("Open...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::Open)));
            });
            // Open Recent: 「最近開いた」 履歴 (= AppData.recent_files)。
            // 空のときは sub_menu を作らず disabled top-level item に置換
            // (= cascade を出さない)。 gui_01 cascade exclusivity bug
            // (= 兄弟 sub_menu の cascade が同時 open のまま重なる) の
            // workaround。 空 cascade を出さなければ他の sub_menu cascade
            // を上書きする事故も起きない。
            if app.recent_files_labels.is_empty() {
                m.item_with(daw_ui_core::MenuItemSpec {
                    label: "Open Recent (empty)",
                    on_click: Box::new(|_ui| {}),
                    enabled: false,
                    shortcut_hint: None,
                });
            } else {
                m.sub_menu("Open Recent", |sub| {
                    for (label, path) in app
                        .recent_files_labels
                        .iter()
                        .zip(app.recent_files.paths.iter())
                    {
                        let path_clone = path.clone();
                        sub.item(label.as_str(), move |ui| {
                            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::OpenRecent(path_clone.clone()))
                            }));
                        });
                    }
                });
            }
            m.item("Save", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::Save)));
            });
            m.item("Save As...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::SaveAs)));
            });
            // Recently Saved: 「最近保存した」 履歴 (= AppData.recent_saved)。
            // クリックで OpenRecent と同じ経路で開く (= 保存先 path はそのまま
            // 開けるはず)。 同上 workaround で空のときは disabled top-level
            // item に置換。
            if app.recent_saved_labels.is_empty() {
                m.item_with(daw_ui_core::MenuItemSpec {
                    label: "Recently Saved (empty)",
                    on_click: Box::new(|_ui| {}),
                    enabled: false,
                    shortcut_hint: None,
                });
            } else {
                m.sub_menu("Recently Saved", |sub| {
                    for (label, path) in app
                        .recent_saved_labels
                        .iter()
                        .zip(app.recent_saved.paths.iter())
                    {
                        let path_clone = path.clone();
                        sub.item(label.as_str(), move |ui| {
                            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::OpenRecent(path_clone.clone()))
                            }));
                        });
                    }
                });
            }
            m.item("Import Audio...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::OpenImportAudioDialog)
                }));
            });
            m.item("Import Video...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::OpenImportVideoDialog)
                }));
            });
            m.item("Export WAV...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ExportWav)));
            });
            m.item("Export Video...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::OpenExportMp4Dialog)
                }));
            });
            m.item("Export MIDI...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ExportMidi)));
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
            m.item("Toggle Video Preview", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::TogglePreviewWindow)
                }));
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
    if ui.take_shortcut("daw.loop_selected_clip") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::LoopSelectedClipToggle)
        }));
    }
    // Phase 7 B5 (`docs/plan_scale.html` §5.3): Shift+P で選択 clip の
    // note pitch を最寄り in-scale に一括補正。 selected_notes が空なら
    // clip 全 note、 そうでなければ選択 note のみ。
    if ui.take_shortcut("daw.quantize_pitches_to_scale") {
        let target = if app.selected_notes.is_empty() {
            crate::app::QuantizePitchTarget::SelectedClipAllNotes
        } else {
            crate::app::QuantizePitchTarget::SelectedNotes
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::QuantizePitchesToScale(target))
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
    // PR-V4: daw.synthesize_vocal shortcut は無効化 (builtin VOICEVOX
    // plugin が自動 synth するため explicit trigger 不要)。 user が
    // shortcut を押しても sync_vocal_metadata で再 flush が走るので、
    // 「再 synth したい」 場合は notes 編集すれば trigger される。

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
    // Copy: automation point 選択 → note 選択 の優先順。 同フレームに
    // 両方の selection が居ても、 直近で触っていそうな automation point
    // 側を優先する (= 後で UX に応じて pointer 位置や focus view で
    // 切り替える可能性あり、 まずは Phase 3 では automation 優先で固定)。
    if ui.take_shortcut("copy") {
        if let Some((json, count)) = app.copy_selected_automation_points_as_json() {
            ui.set_clipboard_text(json);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.status_message = format!("コピー: {count} オートメーションポイント");
            }));
        } else if let Some((json, count)) = app.copy_selected_notes_as_json() {
            ui.set_clipboard_text(json);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.status_message = format!("コピー: {count} ノート");
            }));
        }
    }
    // Paste: clipboard text を view まず note JSON として decode、 失敗
    // したら automation JSON として decode を試みる。 順序が逆だと
    // 「note paste 中なのに automation 側に行く」 ような微妙な事故が
    // 起きづらいので、 note を先に試す。 paste 側の decode は
    // `paste_*_from_json` が無効 JSON を silently 落とすので、 試行は
    // 安全。
    //
    // ただし「現在 automation clip / point が selected」 のときは
    // automation 優先で decode を試みる (= note JSON は受け付けない方が
    // user 期待に近い)。
    if ui.take_shortcut("paste")
        && let Some(text) = ui.take_clipboard_paste()
    {
        let prefer_automation =
            !app.selected_automation_clips.is_empty() || !app.selected_automation_points.is_empty();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if prefer_automation {
                app.paste_automation_points_from_json(&text);
            } else {
                app.paste_notes_from_json(&text);
            }
        }));
    }
    if ui.take_shortcut("delete") {
        // 優先順:
        //   1. Audio Editor 開いてて event 選択中 → DeleteAudioEvent
        //   2. automation point 選択あり → DeleteAutomationPoints (Phase 3)
        //   3. notes 選択あり → DeleteSelectedNotes
        //   4. automation clip 選択あり → DeleteAutomationClips (Phase 3)
        //   5. それ以外 → DeleteSelectedClip
        // 同 frame 内で重複 dispatch しないよう排他にする (= ノート
        // 選択中に Delete 押して clip も消える事故を防ぐ)。
        let audio_event_target = app
            .audio_editor_clip
            .zip(app.audio_editor_selected_event);
        let has_notes = !app.selected_notes.is_empty();
        let auto_points = if app.selected_automation_points.is_empty() {
            None
        } else {
            Some(app.selected_automation_points.clone())
        };
        let auto_clips = if app.selected_automation_clips.is_empty() {
            None
        } else {
            Some(app.selected_automation_clips.clone())
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if let Some((clip, event_idx)) = audio_event_target {
                app.handle_event(AppEvent::DeleteAudioEvent { clip, event_idx });
            } else if let Some(points) = auto_points {
                app.handle_event(AppEvent::DeleteAutomationPoints { points });
            } else if has_notes {
                app.handle_event(AppEvent::DeleteSelectedNotes);
            } else if let Some(keys) = auto_clips {
                app.handle_event(AppEvent::DeleteAutomationClips { keys });
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

    // ----- Automation: A キー (gui_01 #028 §7.3) ----------------------------
    // last-touched parameter (volume / pan / lane default knob 操作で更新) の
    // lane を所有 track に追加。 既存の lane は visible / enabled = true で
    // 復活、 該当 track の automation lane 群を即時展開。 `last_touched_param`
    // が None / stale な場合は handler 内で status_message を出して no-op。
    // gui_01 が text_input focus 中は自動 skip するので、 編集中に `a` を
    // 打っても発火しない。
    if ui.take_shortcut("daw.add_automation_from_last_touched") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::AddAutomationFromLastTouched);
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
