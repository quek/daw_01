//! r.md #48: テーマ切替の統合テスト。
//!
//! `AppEvent::SetTheme` が
//! 1. `AppData.theme` を差し替え (= 描画が読む色の SSoT が変わる)、
//! 2. **id だけ**を `app_config.json` に永続化し (色を焼き込まない)、
//! 3. Song を dirty にしない (テーマは「見方の都合」 なのでプロジェクトに書かない)
//!
//! ことを、注入した tempdir に隔離して検証する。ユーザーが `themes/*.json` を置いた
//! ケース (組込みでない id) も含める。

use std::sync::Arc;

use common::app_dirs::AppDirs;
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn build_app(app_dirs: Option<AppDirs>) -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        app_dirs,
        48_000,
    );
    (app, plugin_rx)
}

#[test]
fn set_theme_swaps_the_palette_and_persists_only_the_id() {
    let data_dir = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(data_dir.path());
    let (mut app, _rx) = build_app(Some(dirs.clone()));

    assert_eq!(app.theme.id, "dark", "初回起動は既定テーマ");
    assert!(app.theme.core.is_dark());
    let dark_panel = app.theme.core.panel;

    app.handle_event(AppEvent::SetTheme("light".to_string()));

    assert_eq!(app.theme.id, "light");
    assert!(!app.theme.core.is_dark(), "ライトに切り替わっている");
    assert_ne!(app.theme.core.panel, dark_panel, "パネル色が実際に変わる");
    assert!(!app.song_doc.is_dirty(), "テーマ切替は Song を dirty にしない");

    // 永続化されるのは id だけ (色は焼き込まない = テーマファイルを編集したら次回反映される)。
    let cfg_text = std::fs::read_to_string(dirs.app_config()).expect("app_config が書かれる");
    assert!(cfg_text.contains("\"theme\": \"light\""), "id が保存される: {cfg_text}");
    assert!(!cfg_text.contains("panel"), "色トークンは保存されない: {cfg_text}");

    // 再起動相当: 同じ dirs で組み直すと保存済み id から復元される。
    let (restarted, _rx2) = build_app(Some(dirs));
    assert_eq!(restarted.theme.id, "light");
    assert!(!restarted.theme.core.is_dark());
}

#[test]
fn a_user_theme_file_can_be_selected_and_survives_a_restart() {
    let data_dir = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(data_dir.path());
    std::fs::create_dir_all(dirs.themes_dir()).unwrap();
    std::fs::write(
        dirs.themes_dir().join("midnight.json"),
        r##"{ "name": "Midnight", "base": "dark", "colors": { "accent": "#ff00ff" } }"##,
    )
    .unwrap();

    let (mut app, _rx) = build_app(Some(dirs.clone()));
    app.handle_event(AppEvent::SetTheme("midnight".to_string()));

    assert_eq!(app.theme.id, "midnight");
    assert_eq!(app.theme.name, "Midnight");
    assert_eq!(app.theme.core.accent, daw_ui_renderer::Color::rgb(1.0, 0.0, 1.0));
    // 書かなかったトークンはベース (dark) から継承される。
    assert_eq!(app.theme.core.panel, daw_ui_core::Palette::dark().panel);

    let (restarted, _rx2) = build_app(Some(dirs));
    assert_eq!(restarted.theme.id, "midnight", "ユーザーテーマも再起動を跨いで復元される");
}

#[test]
fn a_deleted_user_theme_falls_back_to_the_default_instead_of_failing_to_start() {
    let data_dir = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(data_dir.path());
    std::fs::create_dir_all(dirs.themes_dir()).unwrap();
    let theme_path = dirs.themes_dir().join("gone.json");
    std::fs::write(&theme_path, r#"{ "colors": {} }"#).unwrap();

    let (mut app, _rx) = build_app(Some(dirs.clone()));
    app.handle_event(AppEvent::SetTheme("gone".to_string()));
    assert_eq!(app.theme.id, "gone");

    // ユーザーがテーマファイルを消した状態で起動しても落ちず、既定テーマになる。
    std::fs::remove_file(&theme_path).unwrap();
    let (restarted, _rx2) = build_app(Some(dirs));
    assert_eq!(restarted.theme.id, "dark");
}

#[test]
fn toggling_the_settings_window_persists_without_dirtying_the_song() {
    let data_dir = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(data_dir.path());
    let (mut app, _rx) = build_app(Some(dirs.clone()));

    assert!(!app.ui_prefs.settings_open, "既定は閉じている");
    app.handle_event(AppEvent::ToggleSettings);
    assert!(app.ui_prefs.settings_open);
    assert!(!app.song_doc.is_dirty(), "window の開閉は Song を dirty にしない");

    let (restarted, _rx2) = build_app(Some(dirs));
    assert!(restarted.ui_prefs.settings_open, "開閉状態が再起動を跨いで復元される");
}
