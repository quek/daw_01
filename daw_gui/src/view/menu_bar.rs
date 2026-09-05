//! 上部 menu bar (File / Edit / View / Help)。root.rs から分離 (r.md #96、サイズ budget)。
//! 各 item は `AppEvent` を push するだけで、状態は持たない。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::event_launcher::LauncherEvent;
use crate::view::shortcuts;

/// r.md #87: View メニューに出すランチャー帯の見せ方 3 項目 (計画書 Q5-b)。
/// 並びは `LauncherLayout::cycle` の巡回順と揃える (メニューとキーで順序が
/// 食い違うと「Tab を何回押せばどれになるか」が読めない)。
const LAUNCHER_LAYOUT_MENU: &[(&str, common::model::LauncherLayout)] = &[
    ("ランチャーとアレンジ (両方)", common::model::LauncherLayout::Both),
    ("ランチャーのみ", common::model::LauncherLayout::LauncherOnly),
    ("アレンジのみ", common::model::LauncherLayout::ArrangerOnly),
];

/// `AppEvent` を 1 つ投げるだけの menu item。 `shortcut` はその item と同じ event を
/// 発火するキーバインドの名前 (`shortcuts.rs` の `ShortcutDef.name`) で、 右端の hint は
/// そこから引く。 ラベルにキーを直書きしない (定義を変えたら hint だけ古くなる)。
fn event_item<'a>(
    m: &mut daw_ui_core::widgets::menu::MenuBuilder<'a, AppData>,
    label: &'a str,
    shortcut: &str,
    event: AppEvent,
) {
    m.item_with(daw_ui_core::MenuItemSpec {
        label,
        on_click: Box::new(move |ui| {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| app.handle_event(event)));
        }),
        enabled: true,
        shortcut_hint: shortcuts::shortcut_hint(shortcut),
    });
}

/// r.md #96: View メニューの「ミキサー」 (下部パネルの Mixer を開閉、`B` と同じ経路)。
/// 閉じている間はタブ帯ごと消えるので、マウスだけで戻す口をここに置く。
/// `draw` の中に直接書かないのは `add_launcher_layout_items` と同じ理由 (不変条件 9)。
fn add_mixer_toggle_item<'a>(m: &mut daw_ui_core::widgets::menu::MenuBuilder<'a, AppData>) {
    event_item(m, "ミキサー", "daw.toggle_mixer_panel", AppEvent::ToggleMixerPanel);
    // Global Sampler / MIDI Capture (`docs/plan_global_sampler.md`): 下部パネルの
    // タブ 2 / 3。ミキサーと同じトグル規則 (キーは割り当てない = hint 無し)。
    event_item(m, "Sampler", "daw.toggle_sampler_panel", AppEvent::Sampler(crate::event_sampler::SamplerEvent::TogglePanel));
    event_item(m, "MIDI Capture", "daw.toggle_midi_capture_panel", AppEvent::Sampler(crate::event_sampler::SamplerEvent::ToggleMidiCapturePanel));
}

/// r.md #87: View メニューの「両方 / ランチャーのみ / アレンジのみ」 3 項目。
///
/// `draw` の中に直接書かないのは不変条件 9 (インデント 6 段) のため
/// — メニュー構築は `menu_bar → menu → item → push_edit → handle_event` で
/// 既に 5 段あり、そこにループを足すと即座に超える。
fn add_launcher_layout_items<'a>(m: &mut daw_ui_core::widgets::menu::MenuBuilder<'a, AppData>) {
    for (label, layout) in LAUNCHER_LAYOUT_MENU {
        let layout = *layout;
        // 巡回キー (`Tab`) のヒントは 3 項目に共通なので、代表して
        // 「両方」の行にだけ出す。
        let hint = (layout == common::model::LauncherLayout::Both)
            .then(|| crate::view::shortcuts::shortcut_hint("daw.cycle_launcher_layout"))
            .flatten();
        m.item_with(daw_ui_core::MenuItemSpec {
            label,
            on_click: Box::new(move |ui| set_launcher_layout(ui, layout)),
            enabled: true,
            shortcut_hint: hint,
        });
    }
}

/// レイアウトを直接その状態にする (巡回順の SSoT は `LauncherLayout::cycle`)。
fn set_launcher_layout(ui: &mut Ui<'_, AppData>, layout: common::model::LauncherLayout) {
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::Launcher(LauncherEvent::SetLayout(layout)));
    }));
}

/// 「最近開いた / 最近保存した」 ファイルを 1 click で開く menu item を 1 つ足す。
/// File メニュー先頭の直接項目 (r.md #95) と「Recently Saved ►」「Open Recent ►」
/// の各行はすべてここを通り、 open 経路 (`AppEvent::OpenRecent` → dirty guard →
/// load) は 1 本だけ。
fn recent_open_item<'a>(
    m: &mut daw_ui_core::widgets::menu::MenuBuilder<'a, AppData>,
    label: &'a str,
    path: &std::path::Path,
) {
    let path = path.to_path_buf();
    m.item(label, move |ui| {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::OpenRecent(path))
        }));
    });
}

/// 上部 menu bar (File / Edit / View) を library widget で描画。
/// `Ui<'a, AppData>` の `'a` は `&AppData` borrow 寿命と同一なので、
/// `app: &'a AppData` を明示して menu の dynamic label (= `&app.ui_prefs.recent_files_labels[i]`)
/// が `'a` に乗ることを borrow checker に伝える。
pub fn draw<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, rect: Rect) {
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
                // r.md #95: 最後に保存したファイル (= Recently Saved の 1 件目) を
                // sub menu の **さらに上**に直接開ける 1 行として置く。cascade を
                // 開かずに 1 click で前回の続きへ戻る導線。label / path / click 経路は
                // すべて sub menu の 1 件目と同一 (`recent_open_item`) で、別経路を
                // 作らない。空のときはこの行を出さず、上の disabled 表示だけにする
                // (empty 表示が二重にならないように)。
                if let (Some(label), Some(path)) = (
                    app.ui_prefs.recent_saved_labels.first(),
                    app.ui_prefs.recent_saved.paths.first(),
                ) {
                    recent_open_item(m, label, path);
                }
                m.sub_menu("Recently Saved", |sub| {
                    for (label, path) in app
                        .ui_prefs.recent_saved_labels
                        .iter()
                        .zip(app.ui_prefs.recent_saved.paths.iter())
                    {
                        recent_open_item(sub, label, path);
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
                        recent_open_item(sub, label, path);
                    }
                });
            }
            m.separator();
            event_item(m, "New", "new", AppEvent::New);
            event_item(m, "Open...", "open", AppEvent::Open);
            m.separator();
            event_item(m, "Save", "save", AppEvent::Save);
            event_item(m, "Save As...", "save_as", AppEvent::SaveAs);
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
            event_item(m, "Export WAV...", "daw.export_wav", AppEvent::ExportWav);
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
            event_item(m, "終了", "quit", AppEvent::Quit(crate::shutdown::QuitRequest::USER));
        });
        mb.menu("Edit", |m| {
            event_item(m, "Undo", "undo", AppEvent::Undo);
            event_item(m, "Redo", "redo", AppEvent::Redo);
            m.item_with(daw_ui_core::MenuItemSpec {
                label: "Delete",
                on_click: Box::new(|ui| {
                    // キーボード Delete と同じ単一 arbiter (`edit_surface` → last-wins →
                    // `delete_for_surface`) を使う。 旧実装は notes + automation clip +
                    // clip を無条件連続発火する別ロジックで、 対象決定規則が 2 系統に
                    // 割れていた (SSoT 崩れ)。 menu 上の pointer は編集面に乗らないので
                    // is_pianoroll_active=false で選択集合ベースの解決になる。
                    ui.push_edit(Edit::mutate(|app: &mut AppData| {
                        app.delete_current_surface(false);
                    }));
                }),
                enabled: true,
                shortcut_hint: shortcuts::shortcut_hint("delete"),
            });
            // r.md #48: アプリ全体の設定 (テーマ選択)。 Ardour / Cubase の Windows 版が
            // Edit > Preferences なので、 DAW に慣れた人が最初に見る場所に置く。
            event_item(m, "設定...", "daw.toggle_settings", AppEvent::ToggleSettings);
        });
        mb.menu("View", |m| {
            // r.md #87: ランチャー帯とアレンジのレーンの見せ方 (Q5-b)。
            add_launcher_layout_items(m);
            m.separator();
            // r.md #29: 編集履歴パネルの開閉。 行 click でその時点へ一発 Undo/Redo。
            event_item(m, "編集履歴", "daw.toggle_undo_history", AppEvent::ToggleUndoHistory);
            // r.md #50: 画面右端のマスターパネル (フェーダー + 各種メーター)。
            event_item(m, "マスターパネル", "daw.toggle_master_panel", AppEvent::ToggleMasterPanel);
            add_mixer_toggle_item(m);
            // r.md #55: 開いているプラグインエディタ窓を一括で閉じる
            // (Cubase の Window > Close All Plug-in Windows 相当)。
            event_item(
                m,
                "プラグインウィンドウをすべて閉じる",
                "daw.close_all_plugin_editors",
                AppEvent::CloseAllPluginEditors,
            );
            event_item(m, "Video Preview", "daw.toggle_preview_window", AppEvent::TogglePreviewWindow);
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
            event_item(m, "ラウドネス解析...", "daw.analyze_loudness", AppEvent::AnalyzeLoudness);
            m.item("ラウドネスレポートを開く", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleLoudnessReport)
                }));
            });
        });
        // r.md #60: GPLv3 §0 は Appropriate Legal Notices を「便利かつ目立つ形で」出せと
        // 要求し、「メニュー等のリストなら目立つ項目 1 つで基準を満たす」と明記している。
        // なので奥のサブメニューではなく **トップレベルの Help メニュー**に置く。
        // ショートカット一覧 (F1) も Ardour / Cubase と同じくここへ集約した
        // (以前は View > Toggle Help にあった)。
        mb.menu("Help", |m| {
            event_item(m, "ショートカット一覧", "daw.toggle_help", AppEvent::ToggleHelp);
            m.item("バージョン情報", |ui| {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleAbout)
                }));
            });
        });
    });
}
