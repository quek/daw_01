//! ルート view: 画面全体を Transport / Inspector / Arrangement / BottomPanel /
//! StatusBar に分割し、各 sub view を呼ぶ。Plugin picker / help は modal overlay。
//!
//! build_root の末尾で `Ui::take_shortcut` を順に消費し、AppEvent (or
//! `Ui::request_undo` 等) に変換する。global shortcut の dispatch はここに集約。

use daw_ui_core::{Edit, Orientation, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, EditSurface};
use crate::view::{
    arrangement_view, bottom_panel, dirty_guard_modal, export_overlay, export_range_modal,
    shutdown_overlay,
    font_picker, load_overlay, loudness_report, master_panel, plugin_picker, recovery_modal,
    resource_monitor,
    settings, shortcuts_help, snap, status_bar, track_inspector, track_picker, transport,
    undo_history, voicevox_overlay,
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

    // 全画面背景 = アプリの床。 全 panel がこの上に浮いて見える (`Palette::is_dark`
    // の判定基準でもあるので、 テーマ切替はまずここが変わる)。
    ui.panel(
        "root_bg",
        Rect { x: 0.0, y: 0.0, w: sw, h: sh },
        app.theme.core.window_bg,
        0.0,
    );

    // r.md #29: 編集履歴 window が開いていれば、 その rect 分だけ背後の pointer を
    // 占有する予約を **背景 widget 描画より前** に行う (= window の上の click が
    // アレンジ等に漏れず、 window の外は通常操作できる true floating)。 本体描画は
    // build_root 末尾で `undo_history::draw`。
    // r.md #54: 解析の走査中は他の floating window を出さない。これらは
    // `with_floating_region` で raw pointer に戻すので、暗転の下でも押せてしまう
    // (編集履歴の行を click すると走査中に Song が飛ぶ)。
    let floating_ok = !app.loudness.phase.is_busy();
    if floating_ok {
        undo_history::reserve(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });
        // r.md #48: 設定 window も同じ true-floating 機構 (背景を暗転しないので、
        // テーマを選んだ瞬間に背後の全画面が切り替わるのを見ながら選べる)。
        settings::reserve(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });
    }
    // r.md #54: ラウドネスレポート window。走査中は **画面全体** を予約して
    // 背景を丸ごと inert にする (暗転と入力遮断が同じ 1 つの根拠から出る)。
    loudness_report::reserve(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });

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

    // inspector を左カラムにフル高さで配置し、 その右で arrangement
    // (上) と bottom_panel (= mixer / piano_roll / audio editor、 下) を縦分割する。
    // 旧レイアウト (inspector は上ペイン内の左帯、 bottom panel は全幅) から
    // 「左=inspector フル高 / 右上=arrangement / 右下=bottom panel」 へ再編。
    // gui_01 `split_view` が 6px の handle を描画して上下 drag を扱う。
    // r.md #50: 右端にマスターパネルを切り出してから、残りを inspector +
    // split(arrangement / bottom) に配る。**アレンジと Mixer/MIDI エディタの
    // 両方の右**に常駐させるので、切り出すのは center_bottom 帯の全高。
    let master_w = master_panel::panel_width(app).min((center_bottom_rect.w - INSPECTOR_W).max(0.0));
    let master_rect = Rect {
        x: center_bottom_rect.x + center_bottom_rect.w - master_w,
        y: center_bottom_rect.y,
        w: master_w,
        h: center_bottom_rect.h,
    };
    let inspector_rect = Rect {
        x: center_bottom_rect.x,
        y: center_bottom_rect.y,
        w: INSPECTOR_W,
        h: center_bottom_rect.h,
    };
    let right_rect = Rect {
        x: center_bottom_rect.x + INSPECTOR_W,
        y: center_bottom_rect.y,
        w: (center_bottom_rect.w - INSPECTOR_W - master_w).max(0.0),
        h: center_bottom_rect.h,
    };

    // inspector はフル高の左カラム。 global shortcut を消費する widget では
    // ないので split (= arrangement / bottom panel widget) より前に描いてよい。
    track_inspector::draw(app, ui, inspector_rect);

    ui.split_view(
        "root_arrange_bottom",
        right_rect,
        Orientation::Vertical,
        ARRANGEMENT_SPLIT_DEFAULT_RATIO,
        |ui, arrangement_rect, bottom_rect| {
            // gui_01 widget (piano_roll 等) は `take_shortcut` を消費する側面が
            // あるため、 先に root レベルで shortcut を捌いて広域の挙動を確定
            // させる。 widget 描画時には消費済みになり、 widget 内蔵の同名
            // shortcut handler は no-op に縮退する。 bottom_rect 確定後に呼ぶ
            // (piano_roll active 判定に使う)。
            dispatch_shortcuts(app, ui, bottom_rect);

            arrangement_view::draw(app, ui, arrangement_rect);
            bottom_panel::draw(app, ui, bottom_rect);
        },
    );

    // r.md #50: マスターパネル (右端フル高)。split の後に描くのは、
    // 分割 handle の hit 判定より手前で pointer を受けたいから。
    master_panel::draw(app, ui, master_rect);

    status_bar::draw(app, ui, status_rect);

    // resource monitor (r.md #3): 詳細パネル (non-modal overlay)。 開いている時だけ
    // 描画する。 modal より前に呼ぶので、 modal が出れば自然に隠れる (意図どおり)。
    resource_monitor::draw(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });

    // Undo 履歴 window (r.md #29): inline な true-floating window の本体描画
    // (背景描画の後 = z-order 最前面)。 pointer 占有予約は build_root 冒頭の
    // `undo_history::reserve`。
    // r.md #54: 走査中は描かない (reserve と対) — 暗転の下に操作可能な窓を残さない。
    if !app.loudness.phase.is_busy() {
        undo_history::draw(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });

        // 設定 window (r.md #48): 同上、 背景描画の後 = z-order 最前面。
        settings::draw(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });
    }

    // ラウドネスレポート window (r.md #54): 同上。走査中はここで暗転も描く。
    loudness_report::draw(app, ui, Rect { x: 0.0, y: 0.0, w: sw, h: sh });

    // Modal: plugin picker。draw 関数内で modal の open/close を app.ui_ephemeral.is_plugin_picker_open
    // と同期させる (常時呼び、内部で is_modal_open / open_modal を管理)。
    plugin_picker::draw(app, ui, screen);

    // Modal: font picker (Text クリップのフォント選択)。is_font_picker_open と同期。
    font_picker::draw(app, ui, screen);

    // 非ブロック overlay: プロジェクトロードの進捗。
    load_overlay::draw(app, ui, screen);

    // 非ブロック overlay: VOICEVOX wav 合成 / 口パク生成の進行状態。
    voicevox_overlay::draw(app, ui, screen);

    // Modal: send 宛先トラックピッカー。app.ui_ephemeral.send_picker == Some(..) のとき開く。
    track_picker::draw(app, ui, screen);

    // Modal: recovery (起動時 or Open 時に検出された autosave 候補)。
    // app.ui_ephemeral.show_recovery_modal を internal で監視するため常時呼び。
    recovery_modal::draw(app, ui, screen);

    // Modal: 未保存変更ありで「プロジェクトを破棄する操作」 (終了 / New /
    // Open / Open Recent) をしようとしたときの「保存して続行 / 保存せず続行 /
    // キャンセル」 確認。 app.ui_ephemeral.dirty_guard を監視。
    dirty_guard_modal::draw(app, ui, screen);

    // Modal: 書き出し範囲ピッカー。app.ui_ephemeral.export_range_picker == Some の
    // とき開く。 export 実行前なので export_overlay より前に描いてよい。
    export_range_modal::draw(app, ui, screen);

    // Overlay: WAV / Video export 中の進捗 + Cancel。app.transport.export_stage を監視。
    export_overlay::draw(app, ui, screen);

    // Overlay: F1 ショートカット / マウス操作一覧。app.ui_prefs.is_help_open と
    // 同期。最前面に出すため他の modal / overlay より後に描く。
    shortcuts_help::draw(app, ui, screen);

    // r.md #61: Overlay:「終了処理中…」。子プロセスがプラグインを畳んで exit
    // するのを待っている間だけ出す。**最後に描く** — 終了に入ったら他の
    // modal / overlay より前面で、下の操作を全部塞ぐ。
    shutdown_overlay::draw(app, ui, screen);
}

/// 上部 menu bar (File / Edit / View) を library widget で描画。
/// `Ui<'a, AppData>` の `'a` は `&AppData` borrow 寿命と同一なので、
/// `app: &'a AppData` を明示して menu の dynamic label (= `&app.ui_prefs.recent_files_labels[i]`)
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
            // r.md #69: 「最近保存した」「最近開いた」を **File メニューの先頭**へ。
            // DAW を開いたら大抵は前回の続きをやるので、最も使う導線を一番上に置く
            // (Ableton Live / Bitwig / Studio One の起動 Hub と同じ発想)。
            // New / Open... の相対順序は変えず、2 段ずつ繰り下がるだけ。
            //
            // 空のときに項目ごと消さず disabled で残すのは、(a)「そういう機能がある」
            // と気づけること、(b) 状況で項目位置が動くと誤クリックの元になること、
            // の 2 点による。空の `sub_menu` は `h: 0` の退化 popup になるので
            // 代わりに disabled の top-level item へ差し替える。
            if app.ui_prefs.recent_saved_labels.is_empty() {
                m.item_with(daw_ui_core::MenuItemSpec {
                    label: "Recently Saved (empty)",
                    on_click: Box::new(|_ui| {}),
                    enabled: false,
                    shortcut_hint: None,
                });
            } else {
                m.sub_menu("Recently Saved", |sub| {
                    for (label, path) in app
                        .ui_prefs.recent_saved_labels
                        .iter()
                        .zip(app.ui_prefs.recent_saved.paths.iter())
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
            if app.ui_prefs.recent_files_labels.is_empty() {
                m.item_with(daw_ui_core::MenuItemSpec {
                    label: "Open Recent (empty)",
                    on_click: Box::new(|_ui| {}),
                    enabled: false,
                    shortcut_hint: None,
                });
            } else {
                m.sub_menu("Open Recent", |sub| {
                    for (label, path) in app
                        .ui_prefs.recent_files_labels
                        .iter()
                        .zip(app.ui_prefs.recent_files.paths.iter())
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
            m.separator();
            m.item("New", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::New)));
            });
            m.item("Open...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::Open)));
            });
            m.separator();
            m.item("Save", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::Save)));
            });
            m.item("Save As...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::SaveAs)));
            });
            m.separator();
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
            m.item("Import Image...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::OpenImportImageDialog)
                }));
            });
            // r.md #66: Export MIDI と対称の取り込み導線 (D&D と同じ pipeline)。
            m.item("Import MIDI...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::OpenImportMidiDialog)
                }));
            });
            // Text クリップは File メニューではなく、 アレンジの空きレーン右クリック →
            // "Text クリップ" で生成する (docs/plan_text_clip_creation.md)。 text トラックは
            // 存在せず、 他 clip と同じくタイムライン上で生成する。
            m.separator();
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
            // r.md #61: 終了導線。旧来は ✕ / Alt+F4 しか無く、メニューにも
            // ショートカットにも終了が無かった。最下段に置くのは Windows /
            // Ardour / Cubase の File メニュー慣習どおり。
            m.separator();
            m.item_with(daw_ui_core::MenuItemSpec {
                label: "終了",
                on_click: Box::new(|ui| {
                    ui.push_edit(Edit::mutate(|app: &mut AppData| {
                        app.handle_event(AppEvent::Quit(crate::shutdown::QuitRequest::USER))
                    }));
                }),
                enabled: true,
                shortcut_hint: crate::view::shortcuts::shortcut_hint("quit"),
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
                // キーボード Delete と同じ単一 arbiter (`edit_surface` → last-wins →
                // `delete_for_surface`) を使う。 旧実装は notes + automation clip +
                // clip を無条件連続発火する別ロジックで、 対象決定規則が 2 系統に
                // 割れていた (SSoT 崩れ)。 menu 上の pointer は編集面に乗らないので
                // is_pianoroll_active=false で選択集合ベースの解決になる。
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.delete_current_surface(false);
                }));
            });
            // r.md #48: アプリ全体の設定 (テーマ選択)。 Ardour / Cubase の Windows 版が
            // Edit > Preferences なので、 DAW に慣れた人が最初に見る場所に置く。
            m.item("設定...", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleSettings)
                }));
            });
        });
        mb.menu("View", |m| {
            // r.md #29: 編集履歴パネルの開閉。 行 click でその時点へ一発 Undo/Redo。
            m.item("編集履歴", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleUndoHistory)
                }));
            });
            // r.md #50: 画面右端のマスターパネル (フェーダー + 各種メーター)。
            m.item("マスターパネル", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleMasterPanel)
                }));
            });
            // r.md #55: 開いているプラグインエディタ窓を一括で閉じる
            // (Cubase の Window > Close All Plug-in Windows 相当)。
            m.item("プラグインウィンドウをすべて閉じる", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CloseAllPluginEditors)
                }));
            });
            m.item("Toggle Help", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ToggleHelp)));
            });
            m.item("Toggle Video Preview", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::TogglePreviewWindow)
                }));
            });
            // resource monitor (r.md #3): status bar 常駐メーターの on/off (永続化)
            // と、 詳細パネルの開閉。
            m.item("Toggle Resource Monitor", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleResourceMonitor)
                }));
            });
            m.item("Performance Panel", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleResourcePanel)
                }));
            });
        });
        // r.md #54: 解析系はここに集約する (今後の解析機能もこのメニューへ)。
        mb.menu("解析", |m| {
            m.item("ラウドネス解析... (Ctrl+L)", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::AnalyzeLoudness)
                }));
            });
            m.item("ラウドネスレポートを開く", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleLoudnessReport)
                }));
            });
        });
    });
}

/// Ctrl+C: 対象面の選択を clipboard envelope にして OS clipboard へ。トラックだけは
/// plugin state 収集が非同期なので `AppData::copy_tracks` 経由 (結果は
/// `pending_clipboard_write` から flush される)。
fn copy_for_surface(app: &AppData, ui: &mut Ui<'_, AppData>, surface: Option<EditSurface>) {
    let Some(surface) = surface else {
        return;
    };
    let synced: Option<(String, usize, &'static str)> = match surface {
        EditSurface::AudioEvents => app.copy_events_clip().map(|(j, c)| (j, c, "イベント")),
        EditSurface::Notes => app.copy_notes_clip().map(|(j, c)| (j, c, "ノート")),
        EditSurface::AutomationPoints => app
            .copy_points_clip()
            .map(|(j, c)| (j, c, "オートメーションポイント")),
        EditSurface::Clips => app.copy_clips_clip().map(|(j, c)| (j, c, "クリップ")),
        EditSurface::AutomationClips => app
            .copy_automation_clips_clip()
            .map(|(j, c)| (j, c, "オートメーションクリップ")),
        EditSurface::Tracks | EditSurface::Sections => None,
    };
    if let Some((json, count, label)) = synced {
        ui.set_clipboard_text(json);
        let label = label.to_string();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.status_message = format!("コピー: {count} {label}");
        }));
        return;
    }
    if matches!(surface, EditSurface::Tracks) {
        let ids = app.selection.selected_track_ids.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.copy_tracks(ids);
        }));
    }
}

/// Ctrl+X: copy (clipboard へ) + 対象の削除を 1 undo step。clipboard 書込は undo 対象外、
/// 削除イベントが自前で undo snapshot を積む。トラックは `AppData::cut_tracks` で
/// 非同期に copy+削除。
fn cut_for_surface(app: &AppData, ui: &mut Ui<'_, AppData>, surface: Option<EditSurface>) {
    let Some(surface) = surface else {
        return;
    };
    let synced: Option<(String, usize, &'static str)> = match surface {
        EditSurface::AudioEvents => app.copy_events_clip().map(|(j, c)| (j, c, "イベント")),
        EditSurface::Notes => app.copy_notes_clip().map(|(j, c)| (j, c, "ノート")),
        EditSurface::AutomationPoints => app
            .copy_points_clip()
            .map(|(j, c)| (j, c, "オートメーションポイント")),
        EditSurface::Clips => app.copy_clips_clip().map(|(j, c)| (j, c, "クリップ")),
        EditSurface::AutomationClips => app
            .copy_automation_clips_clip()
            .map(|(j, c)| (j, c, "オートメーションクリップ")),
        EditSurface::Tracks | EditSurface::Sections => None,
    };
    if let Some((json, count, label)) = synced {
        ui.set_clipboard_text(json);
        let del = match surface {
            EditSurface::AudioEvents => AppEvent::DeleteAudioEditorSelection,
            EditSurface::Notes => AppEvent::DeleteSelectedNotes,
            EditSurface::AutomationPoints => AppEvent::DeleteAutomationPoints {
                points: app.selection.selected_automation_points.clone(),
            },
            EditSurface::Clips => AppEvent::DeleteSelectedClip,
            EditSurface::AutomationClips => AppEvent::DeleteAutomationClips {
                keys: app.selection.selected_automation_clips.clone(),
            },
            _ => return,
        };
        let label = label.to_string();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(del);
            app.ui_ephemeral.status_message = format!("カット: {count} {label}");
        }));
        return;
    }
    if matches!(surface, EditSurface::Tracks) {
        let ids = app.selection.selected_track_ids.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.cut_tracks(ids);
        }));
    }
}

/// Ctrl+V: clipboard envelope を種別で分岐し、**ポインタが合う面の上**でのみマウス位置に
/// 貼る。合わなければ no-op + status (再生ヘッドへの fallback はしない)。
fn paste_from_clipboard(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    text: &str,
    is_pianoroll_active: bool,
) {
    let Some(env) = crate::clipboard::ClipboardEnvelope::from_json(text) else {
        return; // 他アプリ text / 旧 format → 黙って無視
    };
    let src_pid = env.source_project_id;
    use crate::clipboard::ClipboardPayload as P;
    match env.payload {
        P::Notes(notes) => {
            if is_pianoroll_active
                && app.ui_ephemeral.audio_editor_clip.is_none()
                && let Some(at) = app.ui_ephemeral.pianoroll_hover_beat
            {
                let notes = crate::clipboard::sanitize_notes(notes);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let n = app.paste_notes_at(notes, at);
                    if n > 0 {
                        app.ui_ephemeral.status_message = format!("貼り付け: {n} ノート");
                    }
                }));
                return;
            }
            paste_noop(ui);
        }
        P::AudioEvents(events) => {
            if is_pianoroll_active
                && app.ui_ephemeral.audio_editor_clip.is_some()
                && let Some(at) = app.ui_ephemeral.audio_editor_hover_beat_in_clip
            {
                let events = crate::clipboard::sanitize_audio_events(events);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let n = app.paste_events_at(events, at);
                    if n > 0 {
                        app.ui_ephemeral.status_message = format!("貼り付け: {n} イベント");
                    }
                }));
                return;
            }
            paste_noop(ui);
        }
        P::AutomationPoints(points) => {
            if let (Some(lane), Some(at)) = (
                app.ui_ephemeral.arrange_hovered_automation_lane,
                app.ui_ephemeral.arrangement_hover_beat,
            ) {
                let points = crate::clipboard::sanitize_points(points);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let n = app.paste_points_at(points, lane, at);
                    if n > 0 {
                        app.ui_ephemeral.status_message =
                            format!("貼り付け: {n} オートメーションポイント");
                    }
                }));
                return;
            }
            paste_noop(ui);
        }
        P::Clips(clips) => {
            if !is_pianoroll_active
                && app.ui_ephemeral.arrange_hovered_automation_lane.is_none()
                && let (Some(track), Some(at)) =
                    (app.ui_ephemeral.arrange_hovered_track, app.ui_ephemeral.arrangement_hover_beat)
            {
                let clips = crate::clipboard::sanitize_clips(clips);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let n = app.paste_clips_at(clips, src_pid, track, at);
                    if n > 0 {
                        app.ui_ephemeral.status_message = format!("貼り付け: {n} クリップ");
                    }
                }));
                return;
            }
            paste_noop(ui);
        }
        P::AutomationClips(clips) => {
            // automation clip は「マウス下の automation lane」へ、 hover 拍を基準に貼る
            // (= automation point paste と同じく lane + beat が揃ったときのみ)。
            if let (Some(lane), Some(at)) = (
                app.ui_ephemeral.arrange_hovered_automation_lane,
                app.ui_ephemeral.arrangement_hover_beat,
            ) {
                let clips = crate::clipboard::sanitize_automation_clips(clips);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let n = app.paste_automation_clips_at(clips, lane, at);
                    if n > 0 {
                        app.ui_ephemeral.status_message =
                            format!("貼り付け: {n} オートメーションクリップ");
                    }
                }));
                return;
            }
            paste_noop(ui);
        }
        P::Tracks(tracks) => {
            if !is_pianoroll_active
                && let Some(above) = app.ui_ephemeral.arrange_hovered_track
            {
                let tracks = crate::clipboard::sanitize_tracks(tracks);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let n = app.paste_tracks_at(tracks, src_pid, above);
                    if n > 0 {
                        app.ui_ephemeral.status_message = format!("貼り付け: {n} トラック");
                    }
                }));
                return;
            }
            paste_noop(ui);
        }
    }
}

fn paste_noop(ui: &mut Ui<'_, AppData>) {
    ui.push_edit(Edit::mutate(|app: &mut AppData| {
        app.ui_ephemeral.status_message =
            "ここには貼り付けできません (貼り先の上にカーソルを置いてください)".to_string();
    }));
}

/// `ShortcutMap` ルックアップで判定済みの shortcut name を pull して AppEvent / undo
/// 要求に変換する。`Ui::take_shortcut` は 1 度だけ消費するので、各 name について
/// この関数で一括処理する。`app` は immut で受けて、コピーや状態判定のみで使う
/// (mutation は `Ui::push_edit(Edit::mutate(...))` 経由)。
///
/// `bottom_rect` は piano_roll active 判定用。マウスが bottom_panel 領域内 + Piano Roll
/// タブが選択中なら G/X/1/2/3 を piano_roll 系に流す。それ以外は arrange 系。
fn dispatch_shortcuts(app: &AppData, ui: &mut Ui<'_, AppData>, bottom_rect: Rect) {
    // r.md #54: 範囲ラウドネス解析の走査中は **Esc (= 中止) 以外の shortcut を
    // 一切通さない**。
    //
    // レポート窓の全画面 `reserve_floating_region` が落とすのは pointer だけで、
    // `take_shortcut` は素通りする。そのままだと Ctrl+Z / Ctrl+Y / 履歴ジャンプが
    // freewheel 走査の最中に Song を差し替え、次フレームの `flush_song_sync` が
    // 走査中の daw_audio へ `LoadSong` を、plugin_host へ `SetupAraDocument`
    // (= プラグイン再初期化) を送ってしまう (undo / redo は `edit_song` を通らない
    // ので編集ロックでも止まらない)。Ctrl+E は走査中に `ReinitAllPlugins` を撃つ。
    // 書き出しは `export_overlay` が真のモーダル (capture_keyboard) なので同じ
    // 事故が起きない — 解析だけがこの保護を欠いていた。
    if app.loudness.phase.is_busy() {
        if ui.take_shortcut("escape") {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CancelLoudnessAnalysis)
            }));
        }
        return;
    }
    // 編集面 arbiter (clipboard / delete / `Z` zoom / `R` loop が共有)。 関数冒頭で
    // 1 度算出し、 先頭の `R` ブロックから末尾の全選択ブロックまで全シーケンスで使う。
    // `is_pianoroll_active`: マウスが bottom_panel 内 + Piano Roll タブ選択中か。
    let pointer_in_bottom = ui
        .pointer()
        .pos
        .is_some_and(|(px, py)| bottom_rect.contains(px, py));
    let is_pianoroll_active = app.ui_prefs.bottom_panel == 1 && pointer_in_bottom;
    let surface = app.edit_surface(is_pianoroll_active);
    // `Z` 段階ズーム / `R` loop の対象面 (通常 clip / automation clip) は
    // copy / cut / delete と同じ `edit_surface` arbiter で解決する (last-selection-wins)。
    // これで「MIDI clip を選んでも残存 automation 選択へズームしてしまう」 を防ぐ。
    let zoom_automation = matches!(surface, Some(EditSurface::AutomationClips));

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
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::LoopSelectedClipToggle {
                automation: zoom_automation,
            })
        }));
    }
    // Phase 7 B5 (`docs/plan_scale.html` §5.3): Shift+P で選択 clip の
    // note pitch を最寄り in-scale に一括補正。 selected_notes が空なら
    // clip 全 note、 そうでなければ選択 note のみ。
    if ui.take_shortcut("daw.quantize_pitches_to_scale") {
        let target = if app.selection.selected_notes.is_empty() {
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
        let track_ids = app.selection.selected_track_ids.clone();
        if !track_ids.is_empty() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::GroupSelectedTracks { track_ids });
            }));
        }
    }
    // Alt+G — ungroup the selected group tracks (Ableton Live の
    // Cmd/Ctrl+Shift+G に相当、 本 DAW はユーザー指定で Alt+G)。
    if ui.take_shortcut("daw.ungroup_tracks") {
        let track_ids = app.selection.selected_track_ids.clone();
        if !track_ids.is_empty() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::UngroupTracks { track_ids });
            }));
        }
    }
    // Ctrl+T: 新規トラックを末尾に追加 (vocal は instrument に VOICEVOX を挿して作る)。
    if ui.take_shortcut("daw.add_track") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::AddInstrumentTrack);
        }));
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
    // r.md #61: Ctrl+Q。File > 終了 / ✕ / Alt+F4 と同じ `AppEvent::Quit` に合流する。
    if ui.take_shortcut("quit") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Quit(crate::shutdown::QuitRequest::USER))
        }));
    }
    // ----- Edit -----
    // undo/redo は daw_gui の SongDoc snapshot が SSoT (AppEvent::Undo/Redo → song_doc.undo())。
    // lib 側 undo は S4a で撤去したので、ここで shortcut を拾って自前 undo に流すのが最終形。
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
    // r.md #29: Ctrl+Alt+Z で編集履歴パネルを開閉。
    if ui.take_shortcut("daw.toggle_undo_history") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleUndoHistory)
        }));
    }
    // r.md #50: Ctrl+Alt+M でマスターパネルを開閉 (REAPER の Master Track トグルと同キー)。
    if ui.take_shortcut("daw.toggle_master_panel") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleMasterPanel)
        }));
    }
    // r.md #55: Ctrl+Shift+W で開いているプラグインエディタ窓を全部閉じる。
    // 転送対象なので、エディタ窓にフォーカスがある状態で押しても同じ経路に合流する。
    if ui.take_shortcut("daw.close_all_plugin_editors") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::CloseAllPluginEditors)
        }));
    }
    // r.md #48: Ctrl+, で設定を開閉 (Edit メニュー「設定...」 と同じイベント)。
    if ui.take_shortcut("daw.toggle_settings") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ToggleSettings)
        }));
    }
    // r.md #54: Ctrl+L で範囲ラウドネス解析 (解析メニューと同じイベント)。
    if ui.take_shortcut("daw.analyze_loudness") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::AnalyzeLoudness)
        }));
    }
    // ----- Clipboard / Delete (統一 arbiter) -----
    // ポインタが乗っている編集面 → なければ選択優先順、で対象面を一意に決める
    // (grill-me 2026-06-11)。copy / cut / paste / delete が同じ arbiter を共有。
    // text_input focus 中は gui_01 が cut/copy/paste/delete を自動 suppress するので
    // typing guard は不要。
    // (`is_pianoroll_active` / `surface` / `zoom_automation` は関数冒頭で算出済 —
    // `R` loop が先頭ブロックで使うため。)

    // f キー。カーソル直下の拍 (song-absolute) を現在の snap 設定で吸着して
    // プレイヘッドを移動し再生する。piano_roll active ならピアノロールの hover (song-raw)、
    // それ以外はアレンジの hover (raw) を使い、view 層でここで snap + routing を解決して
    // song-absolute beat にする。どちらの grid 外でも hover は None なので no-op。
    // Alt はライブ取得 (一時 snap 解除)。再生中 seek 継続 / 停止中 play() は handler 側。
    if ui.take_shortcut("daw.play_from_cursor") {
        let alt = ui.pointer().modifiers.alt;
        let target_beat = if is_pianoroll_active {
            app.ui_ephemeral.pianoroll_hover_beat_song_raw.map(|raw| {
                snap::piano_roll_snap_config(app).snap_beat(raw, alt, app.pianoroll_zoom_x())
            })
        } else {
            app.ui_ephemeral.arrangement_hover_beat_raw.map(|raw| {
                snap::arrange_snap_config(app).snap_beat(raw, alt, app.ui_prefs.arrange_zoom_x.max(1.0))
            })
        };
        if let Some(beat) = target_beat {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::PlayFromCursor { beat });
            }));
        }
    }

    // Home / End: プレイヘッドをタイムラインの端へ移動 (r.md #10)。 typing 中は
    // `is_typing_only_shortcut` で抑止され text_input のカーソル移動になるので、
    // ここに来るのは非 typing 時のみ。 seek は handler が停止/再生とも面倒を見る。
    if ui.take_shortcut("daw.goto_timeline_home") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::GotoTimelineHome)
        }));
    }
    if ui.take_shortcut("daw.goto_timeline_end") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::GotoTimelineEnd)
        }));
    }

    // Alt+F: 再生追従スクロールの方式を循環 (OFF → 連続 → ページ)。
    if ui.take_shortcut("daw.cycle_arrange_follow") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::CycleArrangeFollow);
        }));
    }

    // トラック copy/cut の非同期結果 (plugin state 収集後) を OS clipboard へ flush。
    if let Some(text) = app.ui_ephemeral.pending_clipboard_write.clone() {
        ui.set_clipboard_text(text);
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.ui_ephemeral.pending_clipboard_write = None;
        }));
    }

    if ui.take_shortcut("copy") {
        copy_for_surface(app, ui, surface);
    }
    if ui.take_shortcut("cut") {
        cut_for_surface(app, ui, surface);
    }
    if ui.take_shortcut("paste")
        && let Some(text) = ui.take_clipboard_paste()
    {
        paste_from_clipboard(app, ui, &text, is_pianoroll_active);
    }
    if ui.take_shortcut("delete") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.delete_current_surface(is_pianoroll_active);
        }));
    }

    // ----- Grid snap / fit -----
    // active view 判定は上で算出した `is_pianoroll_active` を共有する
    // (マウスが bottom_panel 領域内 AND Piano Roll タブ選択中)。
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
                // arrangement: 直前のズームに戻る (履歴が空なら全体フィット)。
                app.handle_event(AppEvent::ArrangeZoomBack);
            }
        }));
    }
    // Z: arrangement の段階ズーム (1 回目=横、 2 回目=縦)。 piano roll が active
    // (= pointer が piano roll 上) のときは clip ズーム概念が無いので発火しない。
    // text_input focus 中は gui_01 が単キーを抑制する。
    if ui.take_shortcut("daw.zoom_selected_clip") {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if !is_pianoroll_active {
                app.handle_event(AppEvent::ZoomArrangeToSelectedClip {
                    automation: zoom_automation,
                });
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

    // ----- Track solo (S): piano roll + arrangement / mixer -----
    // S キーで「マウス直下のトラック」を solo toggle する (mixer / arrangement の
    // S ボタンと同じ ToggleTrackSolo を発火)。 対象 track は pointer の位置で決まる:
    // - piano roll active (= pointer が bottom panel 内 + Piano Roll タブ。 audio
    //   editor は MIDI 編集文脈ではないので除外): 編集中 clip の所属 track
    //   (ClipRef.track は index なので id へ解決)。
    // - mixer (= pointer が bottom panel 内 + Mixer タブ): マウス直下のストリップ
    //   (`mixer_hovered_track`)。 master strip / strip 外は None で no-op。
    // - それ以外 (= pointer がアレンジ上): マウス直下のトラック
    //   (`arrange_hovered_track`)。 ヘッダ列でもクリップレーン上でも
    //   同じトラック行を返し、 ruler / master 行 / トラック外は None で no-op。
    //   いずれも選択トラックではなく「カーソルがあるトラック」を solo する。
    // text_input focus 中は gui_01 が単キーを抑制するので rename / 歌詞編集中は発火しない。
    if ui.take_shortcut("daw.toggle_track_solo") {
        let target_track_id = if is_pianoroll_active {
            if app.ui_ephemeral.audio_editor_clip.is_some() {
                None
            } else {
                app.selected_clip_ref()
                    .and_then(|c| app.song_doc.song().tracks.get(c.track as usize))
                    .map(|t| t.id)
            }
        } else if app.ui_prefs.bottom_panel == 0 && pointer_in_bottom {
            app.ui_ephemeral.mixer_hovered_track
        } else {
            app.ui_ephemeral.arrange_hovered_track
        };
        if let Some(track_id) = target_track_id {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleTrackSolo(track_id));
            }));
        }
    }

    // ----- Mute (Q) -----
    // 「選択中のものがあればそれらを、 無ければマウスカーソル直下のものを」 mute toggle。
    // 対象は文脈で決まる:
    // - piano roll active (= bottom panel が piano roll タブ + pointer が bottom 内、 ただし
    //   audio editor を開いていない MIDI 編集文脈): note を対象。 選択 note (`selected_notes`)
    //   があればそれら、 無ければカーソル直下 note (`pianoroll_hover_note`)。
    // - それ以外 (アレンジ / audio editor): clip を対象。 audio editor 中はその clip、
    //   そうでなければ選択 clip (`selected_clips`)、 無ければカーソル直下 clip
    //   (`arrangement_hover_clip`)。
    // toggle 方向は「対象が全部 muted なら unmute、 1 つでも非 muted なら全 mute」。
    // text_input フォーカス中は gui_01 が単キーを抑制する。
    if ui.take_shortcut("daw.toggle_mute") {
        if is_pianoroll_active && app.ui_ephemeral.audio_editor_clip.is_none() {
            // note 群は packed note id (`selected_notes` / `pianoroll_hover_note` は
            // 表示中全クリップに跨る packed id)。所属クリップは handler が decode するので、
            // ここで単一 anchor clip に縛らない (複数クリップ同時 mute を保つ)。
            let notes: Vec<u32> = if !app.selection.selected_notes.is_empty() {
                app.selection.selected_notes.clone()
            } else {
                app.ui_ephemeral.pianoroll_hover_note.into_iter().collect()
            };
            if !notes.is_empty() {
                let new_muted = !app.all_notes_muted(&notes);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNotesMuted {
                        notes,
                        muted: new_muted,
                    });
                }));
            }
        } else {
            let targets: Vec<crate::app::ClipRef> = if is_pianoroll_active {
                // audio waveform editor を開いている: その clip を mute。
                app.ui_ephemeral.audio_editor_clip.into_iter().collect()
            } else if app.selection.selected_clip.is_some() || !app.selection.selected_clips.is_empty() {
                app.selected_clip_refs()
            } else {
                app.ui_ephemeral.arrangement_hover_clip.into_iter().collect()
            };
            if !targets.is_empty() {
                let new_muted = !app.all_clips_muted(&targets);
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetClipsMuted {
                        targets,
                        muted: new_muted,
                    });
                }));
            }
        }
    }

    // ----- Ctrl+A: 文脈別全選択 (grill-me 2026-06-09) -----
    // マウス位置で対象を判定する (選択前なので Delete の「非空セット」判定は
    // 使えず pointer 位置で振り分け)。 下部パネル + audio editor 開: 全 event、
    // 下部パネル + piano roll: 全ノート、 それ以外 (アレンジ): 全クリップ。
    // (automation lane 上の「全ポイント → 全クリップ」段階拡大は後続で追加。)
    if ui.take_shortcut("select_all") {
        if is_pianoroll_active && app.ui_ephemeral.audio_editor_clip.is_some() {
            let indices = app.all_audio_event_indices();
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetAudioEditorEventSelection(indices.clone()));
            }));
        } else if is_pianoroll_active {
            let ids = app.all_shown_pianoroll_note_ids();
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetNoteSelection(ids.clone()));
            }));
        } else if let Some(lane) = app.ui_ephemeral.arrange_hovered_automation_lane {
            // automation lane 上: 段階拡大 (#071 で clip 段を追加)。
            //   1 回目 = lane の全ポイント
            //   2 回目 (全ポイント選択済 or ポイント無し) = lane の全 automation clip
            //   3 回目 (全 clip 選択済 or clip 無し)     = 曲全体の全 (通常) クリップ
            // tier2 で点とクリップが両方選択された状態になるが、 直近選択 (= clip) が
            // last-wins で copy/cut/delete の対象になる (edit_surface 参照)。
            let all_points = app.all_automation_points_in_lane(lane);
            let points_done = all_points.is_empty()
                || (app.selection.selected_automation_points.len() == all_points.len() && {
                    let cur: std::collections::HashSet<_> =
                        app.selection.selected_automation_points.iter().collect();
                    all_points.iter().all(|p| cur.contains(p))
                });
            if !points_done {
                let prev = app.selection.selected_automation_points.clone();
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SelectAutomationPoints {
                        prev: prev.clone(),
                        next: all_points.clone(),
                    });
                }));
            } else {
                let all_clips = app.all_automation_clips_in_lane(lane);
                let clips_done = all_clips.is_empty()
                    || (app.selection.selected_automation_clips.len() == all_clips.len() && {
                        let cur: std::collections::HashSet<_> =
                            app.selection.selected_automation_clips.iter().collect();
                        all_clips.iter().all(|c| cur.contains(c))
                    });
                if !clips_done {
                    let prev = app.selection.selected_automation_clips.clone();
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SelectAutomationClips {
                            prev: prev.clone(),
                            next: all_clips.clone(),
                        });
                    }));
                } else {
                    ui.push_edit(Edit::mutate(|app: &mut AppData| {
                        app.handle_event(AppEvent::SelectAllClips);
                    }));
                }
            }
        } else {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::SelectAllClips);
            }));
        }
    }

    // ----- Clip duplicate (gui_01 #019) -----
    // D / Alt+D で選択中 clip 群をまとめて共有/独立コピー。 選択ブロック全体の
    // span だけ後ろにずらして相対位置を保ったまま複製する (REAPER / Ableton の
    // Ctrl+D 流、 Ctrl+drag と同じセマンティクス)。 複製は全選択になり連打で
    // 後方連鎖。 selected が空なら no-op。
    if ui.take_shortcut("daw.duplicate_clip_shared") {
        // 対象面は copy/cut/delete と同じ last-wins (`edit_surface`)。トラック面なら
        // トラックをリンク複製 (D = 共有、 クリップ複製と同規約)。 それ以外は従来通り。
        if matches!(app.edit_surface(is_pianoroll_active), Some(EditSurface::Tracks)) {
            let ids = app.selection.selected_track_ids.clone();
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::DuplicateTracksShared(ids));
            }));
        } else if is_pianoroll_active && !app.selection.selected_notes.is_empty() {
            // ピアノロール上 + ノート選択中なら D = ノート複製。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::DuplicateSelectedNotes);
            }));
        } else {
            // それ以外 (アレンジ文脈 / ピアノロールでもノート未選択) は選択中の
            // MIDI/Audio/Vocal clip と automation clip の両方を同時に共有複製
            // (Ableton/REAPER 流)。
            let midi_sources: Vec<crate::app::ClipRef> = app.selected_clip_refs();
            let automation_sources: Vec<common::model::AutomationClipKey> =
                app.selection.selected_automation_clips.clone();
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                if !midi_sources.is_empty() {
                    app.handle_event(AppEvent::DuplicateClipsShared(midi_sources));
                }
                if !automation_sources.is_empty() {
                    app.handle_event(AppEvent::DuplicateAutomationClipsShared(automation_sources));
                }
            }));
        }
    }
    if ui.take_shortcut("daw.duplicate_clip_unique") {
        // トラック面なら Alt+D = トラックを独立複製 (クリップ複製と同規約)。
        if matches!(app.edit_surface(is_pianoroll_active), Some(EditSurface::Tracks)) {
            let ids = app.selection.selected_track_ids.clone();
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::DuplicateTracksUnique(ids));
            }));
        } else {
            let midi_sources: Vec<crate::app::ClipRef> = app.selected_clip_refs();
            let automation_sources: Vec<common::model::AutomationClipKey> =
                app.selection.selected_automation_clips.clone();
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                if !midi_sources.is_empty() {
                    app.handle_event(AppEvent::DuplicateClipsUnique(midi_sources));
                }
                if !automation_sources.is_empty() {
                    app.handle_event(AppEvent::DuplicateAutomationClipsUnique(automation_sources));
                }
            }));
        }
    }

    // ----- Clip rename (F2) -------------------------------------------------
    // 選択中 clip を inline rename (右クリックメニュー "Rename" と同経路)。
    // rename は単一対象なので selected_clip (= 末尾カーソル clip) を使う。
    // 選択 clip が無ければ no-op。 text_input focus 中は gui_01 が shortcut を
    // 抑制するので rename 編集中の F2 は発火しない。
    // clip が選択されていれば clip rename、 そうでなければ
    // (track header のみ選択 / フォーカス時) cursor track の名前を rename。
    // どちらも単一対象 (selected_clip = 末尾カーソル clip、 track は
    // cursor_track_index)。 track header の double-click が効かない場面でも
    // F2 で確実に rename を開始できる。
    if ui.take_shortcut("daw.rename_clip") {
        if let Some(target) = app.selected_clip_ref() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BeginRenameClip(target));
            }));
        } else if let Some(track_id) = app.cursor_track_id() {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BeginRenameTrack(track_id));
            }));
        }
    }

    // ----- 共有を一括選択 (Shift+L) ------------------------------------------
    // 選択中 clip と同じ content_id の linked clip group をまとめて選択
    // (右クリックメニュー「共有を一括選択」と同経路)。 selected_clip が
    // 無ければ no-op。 text_input focus 中は gui_01 が shortcut を抑制する
    // ので rename 編集中の Shift+L は発火しない。
    if ui.take_shortcut("daw.select_linked_clips")
        && let Some(target) = app.selected_clip_ref()
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SelectLinkedClips(target));
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
    // ----- ビデオプレビューウィンドウ (F12) -----
    if ui.take_shortcut("daw.toggle_preview_window") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::TogglePreviewWindow)
        }));
    }
    // Phase 2 PR-D 段階 1: Audio Editor 開いているとき Ctrl+D で
    // 選択中 event を Duplicate。 audio_editor_clip is None のときは
    // 消費しない (= 既存 D / Alt+D の clip duplicate と紛らわしくない
    // よう、 Audio Editor 内限定の shortcut として gate する)。
    if app.ui_ephemeral.audio_editor_clip.is_some() && ui.take_shortcut("daw.duplicate_audio_event") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::DuplicateAudioEditorEvent)
        }));
    }
    // PR-D 段階 2: Audio Editor 内 event 選択 navigation (Ctrl+] / Ctrl+[)。
    // 現選択 idx を ±1 wrap-around で移動。 audio_editor が開いてないと
    // 無効、 events が空なら no-op。
    if app.ui_ephemeral.audio_editor_clip.is_some() && ui.take_shortcut("daw.next_audio_event") {
        let next = app.next_audio_editor_event_idx(1);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SelectAudioEditorEvent(next))
        }));
    }
    if app.ui_ephemeral.audio_editor_clip.is_some() && ui.take_shortcut("daw.prev_audio_event") {
        let prev = app.next_audio_editor_event_idx(-1);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SelectAudioEditorEvent(prev))
        }));
    }
    // B12 (r.md #8): 選択オーディオクリップを auto-warp (transient を拍グリッドに整列)。
    // arrangement / audio editor どちらの選択でも可 (handler が audio 以外は no-op)。
    if ui.take_shortcut("daw.auto_warp_clip") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::AutoWarpSelectedClip)
        }));
    }
    // modal が開いている間は escape を消費しない (modal 側で close する)。
    // 優先度: track rename → clip rename → Audio Editor close → CloseHelp。
    // rename (track header / clip rect の inline text_input) と Audio Editor
    // (bottom panel) は同時には開かないので順番でも実用 OK。
    // plugin picker / send picker が開いている間は escape を消費しない
    // (= track_picker / plugin_picker の modal が close_on_escape で閉じる)。
    //
    // piano_roll の歌詞 inline 編集中も escape を消費しない。 この
    // `dispatch_shortcuts` は `bottom_panel::draw` (= piano_roll widget) より前に走るため、
    // ここで `take_shortcut("escape")` を消費すると widget の歌詞キャンセルハンドラ
    // (piano_roll.rs) に escape が届かず、 代わりに下の選択解除 branch が走って編集中
    // clip が deselect → MIDI エディタが空表示になってしまう。 編集中はここで消費せず
    // widget に委ねる (widget が `take_shortcut("escape")` で歌詞編集を cancel する)。
    // 条件は piano_roll widget が実際に走る状況 (Piano Roll タブ + Audio Editor 非表示) に
    // 一致させる (`app.ui_ephemeral.piano_roll_lyric_editing` 単独だと stale-true で誤委譲しうる)。
    let pianoroll_lyric_editing =
        app.ui_prefs.bottom_panel == 1 && app.ui_ephemeral.audio_editor_clip.is_none() && app.ui_ephemeral.piano_roll_lyric_editing;
    if !app.ui_ephemeral.is_plugin_picker_open
        && !app.ui_ephemeral.is_font_picker_open
        && app.ui_ephemeral.send_picker.is_none()
        && !pianoroll_lyric_editing
        && ui.take_shortcut("escape")
    {
        if app.ui_ephemeral.track_rename_id.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CancelRenameTrack)
            }));
        } else if app.ui_ephemeral.section_rename_id.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CancelRenameSection)
            }));
        } else if app.ui_ephemeral.clip_rename.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CancelRenameClip)
            }));
        } else if app.ui_ephemeral.audio_editor_clip.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CloseAudioEditor)
            }));
        } else if app.ui_ephemeral.resource_panel_open {
            // resource monitor (r.md #3): 詳細パネルが開いていれば Esc で閉じる
            // (rename / audio editor の後、 選択解除より優先)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleResourcePanel)
            }));
        } else if app.ui_prefs.loudness_report_open {
            // r.md #54: レポート window が開いていれば Esc で閉じる (同順)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleLoudnessReport)
            }));
        } else if app.ui_prefs.undo_history_open {
            // r.md #29: 編集履歴 window が開いていれば Esc で閉じる
            // (resource panel と同順、 選択解除より優先)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleUndoHistory)
            }));
        } else if app.ui_prefs.settings_open {
            // r.md #48: 設定 window が開いていれば Esc で閉じる (同順)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ToggleSettings)
            }));
        } else if !app.selection.selected_clips.is_empty()
            || app.selection.selected_clip.is_some()
            || !app.selection.selected_notes.is_empty()
            || !app.selection.selected_automation_points.is_empty()
            || !app.selection.selected_automation_clips.is_empty()
        {
            // Escape で選択解除 (clip / note / automation point / clip)。
            // 死蔵だった ClearSelection / ClearNoteSelection を生かす。
            // audio editor は上の分岐で先に閉じるので、 ここに来る時点で
            // audio event 選択は対象外 (close 時に clear 済)。
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::ClearSelection);
                app.handle_event(AppEvent::ClearNoteSelection);
                app.selection.selected_automation_points.clear();
                app.selection.selected_automation_clips.clear();
            }));
        } else {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CloseHelp)
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use common::protocol::{AudioCommand, PluginCommand};
    use daw_ui_core::{FrameInput, UiHost};
    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};
    use daw_ui_renderer::Scene;
    use tokio::sync::mpsc;

    use crate::app::ClipRef;
    use crate::dispatcher::{
        BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
    };

    fn build_app() -> AppData {
        let (audio_tx, _audio_rx) = mpsc::unbounded_channel::<AudioCommand>();
        let (plugin_tx, _plugin_rx) = mpsc::unbounded_channel::<PluginCommand>();
        let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
        let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
        AppData::new(
            audio_tx,
            plugin_tx,
            None,
            None,
            event_dispatcher,
            job_dispatcher,
            None,
            None,
            common::audio_bridge::DEFAULT_SAMPLE_RATE,
        )
    }

    /// Esc 押下 1 フレームを `dispatch_shortcuts` に通し、 push された Edit を app に適用する。
    /// `UiHost::no_redraw()` の default binding は "escape" = Escape を含むので、
    /// Escape KeyEvent を渡すと frame 頭で "escape" shortcut が pending に積まれる。
    fn dispatch_escape(app: &mut AppData) {
        let mut host: UiHost<AppData> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 1280, height: 720 };
        let bottom_rect = Rect { x: 0.0, y: 400.0, w: 1280.0, h: 320.0 };
        let input = FrameInput {
            keyboard: vec![KeyEvent {
                state: ElementState::Pressed,
                text: None,
                physical_key: PhysicalKey::Escape, repeat: false
            }],
            ..FrameInput::default()
        };
        let edits = host.frame_to_edits(app, &mut scene, screen, input, |app, ui| {
            dispatch_shortcuts(app, ui, bottom_rect);
        });
        for e in edits {
            e.apply(app);
        }
    }

    /// piano_roll の歌詞 inline 編集中の Esc は global の `dispatch_shortcuts`
    /// で消費されず piano_roll widget に委ねられる。 ここで消費 (選択解除) されると
    /// 編集中 clip が deselect → MIDI エディタが空表示になる回帰を防ぐ。
    #[test]
    fn escape_during_lyric_edit_is_not_consumed_by_global_dispatch() {
        let mut app = build_app();
        app.ui_prefs.bottom_panel = 1; // Piano Roll タブ
        app.ui_ephemeral.piano_roll_lyric_editing = true; // 歌詞編集中
        app.selection.selected_notes = vec![1];
        dispatch_escape(&mut app);
        assert_eq!(
            app.selection.selected_notes,
            vec![1],
            "歌詞編集中の Esc は global dispatch で消費されず note 選択は維持される",
        );
    }

    /// 対の保証: 歌詞編集中でなければ Esc は従来どおり global dispatch が消費して
    /// note 選択を解除する (既存挙動を壊していない)。
    #[test]
    fn escape_clears_note_selection_when_not_lyric_editing() {
        let mut app = build_app();
        app.ui_prefs.bottom_panel = 1;
        app.ui_ephemeral.piano_roll_lyric_editing = false; // 非編集
        app.selection.selected_notes = vec![1];
        dispatch_escape(&mut app);
        assert!(
            app.selection.selected_notes.is_empty(),
            "非編集時の Esc は従来どおり note 選択を解除する",
        );
    }

    /// Audio Editor が開いている間は piano_roll widget が走らないので、 歌詞フラグが
    /// stale-true でも委譲してはならない (= Esc が宙に浮く)。 ガードの
    /// `audio_editor_clip.is_none()` 項が効いて、 Esc は従来どおり Audio Editor を閉じる。
    #[test]
    fn escape_closes_audio_editor_even_if_lyric_flag_is_stale() {
        let mut app = build_app();
        app.ui_prefs.bottom_panel = 1;
        app.ui_ephemeral.piano_roll_lyric_editing = true; // stale-true を想定
        app.ui_ephemeral.audio_editor_clip = Some(ClipRef { track: 0, clip: 0 });
        dispatch_escape(&mut app);
        assert!(
            app.ui_ephemeral.audio_editor_clip.is_none(),
            "Audio Editor 表示中の Esc は歌詞フラグに関わらず Audio Editor を閉じる",
        );
    }

    /// resource monitor (r.md #3) 詳細パネルが開いている間の Esc はパネルを
    /// 閉じ、 選択は維持する (audio editor の後、 選択解除より優先 = 2 段階で
    /// 次の Esc が選択解除に回る)。
    #[test]
    fn escape_closes_resource_panel_before_clearing_selection() {
        let mut app = build_app();
        app.ui_ephemeral.resource_panel_open = true;
        app.selection.selected_notes = vec![1];
        dispatch_escape(&mut app);
        assert!(!app.ui_ephemeral.resource_panel_open, "Esc は開いている詳細パネルを閉じる");
        assert_eq!(
            app.selection.selected_notes,
            vec![1],
            "パネルを閉じる Esc は選択を解除しない",
        );
    }

    /// r.md #48: 設定 window が開いている間の Esc は window を閉じ、選択は維持する
    /// (編集履歴 window / resource panel と同順)。 ここが抜けると Esc で閉じられず、
    /// 代わりに選択が消える。
    #[test]
    fn escape_closes_settings_window_before_clearing_selection() {
        let mut app = build_app();
        app.ui_prefs.settings_open = true;
        app.selection.selected_notes = vec![1];
        dispatch_escape(&mut app);
        assert!(!app.ui_prefs.settings_open, "Esc は開いている設定 window を閉じる");
        assert_eq!(
            app.selection.selected_notes,
            vec![1],
            "設定 window を閉じる Esc は選択を解除しない",
        );
    }
}
