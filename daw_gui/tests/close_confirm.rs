//! 未保存変更ありで閉じようとしたときの「保存確認」 close フローの回帰テスト。
//!
//! 検証する状態機械 (`AppData`):
//! - `request_close`: dirty なら確認モーダルを開く / clean なら即終了
//! - `CloseConfirmDiscard`: 保存せず終了
//! - `CloseConfirmCancel`: 終了取りやめ
//! - `CloseConfirmSave` (同期): plugin 無し project は即保存 → 終了
//! - `CloseConfirmSave` (非同期): plugin 有り project は plugin state 取得
//!   (`AllStatesReceived`) を待ってから保存 → 終了

use std::sync::Arc;

use common::plugin_db::{PluginDatabase, PluginEntry};
use common::plugin_format::PluginFormat;
use common::protocol::MainToChild;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn make_plugin_db() -> Arc<PluginDatabase> {
    Arc::new(PluginDatabase {
        entries: vec![PluginEntry {
            id: "test.synth".into(),
            format: PluginFormat::Clap,
            name: "Test Synth".into(),
            vendor: "Test".into(),
            version: "1.0".into(),
            features: vec!["instrument".into()],
            path: "C:/fake/synth.clap".into(),
            descriptor_index: 0,
            has_note_input: true,
            has_note_output: false,
            has_audio_output: true,
            // instrument: audio を生成するだけ → audio 入力なし。
            has_audio_input: false,
            has_video_input: false,
            has_video_output: false,
        }],
        scanned_at: None,
        port_probe_version: 0,
    })
}

fn build_app() -> (AppData, UnboundedReceiver<MainToChild>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let event_dispatcher_dyn: Arc<dyn BackgroundDispatcher> = event_dispatcher.clone();
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        Some(make_plugin_db()),
        event_dispatcher_dyn,
        job_dispatcher,
        None,
        // app_dirs: None = 永続化なし。 実 %LOCALAPPDATA%/daw_01/recent*.json を汚染しない。
        None,
    );
    (app, plugin_rx)
}

fn load_instrument(app: &mut AppData) {
    let track_id = app.song.tracks[0].id;
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });
    app.handle_event(AppEvent::SlotPluginLoadedFromChild {
        track: track_id,
        // 単一デバイスチェーン: picker は末尾 append、 空チェーンなので index 0。
        index: 0,
        id: "test.synth".into(),
        name: "Test Synth".into(),
        plugin_id: 100,
        shmem_id: String::new(),
        state_load_error: None,
    });
}

#[test]
fn not_dirty_close_quits_immediately() {
    let (mut app, _rx) = build_app();
    app.is_dirty = false;

    app.request_close();

    assert!(app.should_quit, "clean project closes immediately");
    assert!(!app.show_close_confirm, "no confirm modal when clean");
}

#[test]
fn dirty_close_opens_confirm_modal() {
    let (mut app, _rx) = build_app();
    app.is_dirty = true;

    app.request_close();

    assert!(app.show_close_confirm, "dirty project opens confirm modal");
    assert!(!app.should_quit, "must not quit before user decides");
}

#[test]
fn discard_quits_without_saving() {
    let (mut app, _rx) = build_app();
    app.is_dirty = true;
    app.request_close();

    app.handle_event(AppEvent::CloseConfirmDiscard);

    assert!(app.should_quit, "discard quits");
    assert!(!app.show_close_confirm, "modal closed after discard");
    assert!(app.is_dirty, "discard does not save (still dirty)");
}

#[test]
fn cancel_keeps_app_running() {
    let (mut app, _rx) = build_app();
    app.is_dirty = true;
    app.request_close();

    app.handle_event(AppEvent::CloseConfirmCancel);

    assert!(!app.should_quit, "cancel keeps running");
    assert!(!app.show_close_confirm, "modal closed after cancel");
    assert!(app.is_dirty, "cancel does not save");
}

#[test]
fn save_without_plugins_saves_synchronously_then_quits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    app.file_path = Some(path.clone());
    app.is_dirty = true;
    app.request_close();

    app.handle_event(AppEvent::CloseConfirmSave);

    assert!(path.exists(), "project file written: {}", path.display());
    assert!(!app.is_dirty, "is_dirty cleared after save");
    assert!(app.should_quit, "sync save quits immediately");
    assert!(!app.show_close_confirm, "modal closed");
    assert!(!app.quit_after_save, "no async wait needed");
}

#[test]
fn save_with_plugins_waits_for_states_then_quits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    load_instrument(&mut app);
    assert!(
        app.pending_state_queue.is_empty(),
        "queue empty after plugin load"
    );
    app.file_path = Some(path.clone());
    app.is_dirty = true;
    app.request_close();

    // 「保存して終了」: plugin 有りなので save は非同期 (state 取得待ち)。
    app.handle_event(AppEvent::CloseConfirmSave);
    assert!(!app.should_quit, "must wait for plugin states before quitting");
    assert!(app.quit_after_save, "marked to quit after async save");
    assert!(!app.show_close_confirm, "modal closed");
    assert!(!path.exists(), "not saved yet (awaiting states)");

    // plugin state 到着 → 保存実行 → 終了確定。
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));

    assert!(path.exists(), "project saved after states arrive");
    assert!(!app.is_dirty, "is_dirty cleared after async save");
    assert!(app.should_quit, "quits after async save completes");
    assert!(!app.quit_after_save, "async-quit intent cleared");
}
