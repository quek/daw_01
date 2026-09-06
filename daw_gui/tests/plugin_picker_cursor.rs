//! Integration test: プラグインピッカーの上下キーカーソル選択 (daw_01 #057 wire / gui_01 Phase 86)。
//!
//! 検証する model 操作:
//! - 初期値は cursor = 0
//! - `MovePluginPickerCursor` で +/- 移動、`[0, visible.len()-1]` で clamp
//! - `SetPluginPickerQuery` で cursor が 0 にリセット
//! - `OpenPluginPicker` で cursor が 0 にリセット
//! - 空 visible での Move は no-op
//!
//! UI 側 (text_input の `nav_up/nav_down` 読み取り、 list_view の selected ハイライト)
//! は visual なので smoke test ではなく手動確認 (CLAUDE.md run before commit)。

use std::sync::Arc;

use common::plugin_db::{PluginDatabase, PluginEntry};
use common::plugin_format::PluginFormat;
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn make_plugin_db_with_n_instruments(n: usize) -> Arc<PluginDatabase> {
    let mut entries = Vec::with_capacity(n);
    for i in 0..n {
        entries.push(PluginEntry {
            id: format!("test.synth.{i}"),
            format: PluginFormat::Clap,
            name: format!("Synth {i}"),
            vendor: "Test".into(),
            version: "1.0".into(),
            features: vec!["instrument".into()],
            path: std::path::PathBuf::from(format!("C:/fake/synth_{i}.clap")),
            descriptor_index: 0,
            has_note_input: true,
            has_note_output: false,
            has_audio_output: true,
            // instrument: audio を生成するだけ → audio 入力なし。
            has_audio_input: false,
            has_video_input: false,
            has_video_output: false,
        });
    }
    Arc::new(PluginDatabase { entries, scanned_at: None, port_probe_version: 0 })
}

fn build_app(
    n_instruments: usize,
) -> (
    AppData,
    UnboundedReceiver<AudioCommand>,
    UnboundedReceiver<PluginCommand>,
) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        Some(make_plugin_db_with_n_instruments(n_instruments)),
        event_dispatcher,
        job_dispatcher,
        None,
        // app_dirs: None = 永続化なし。 実 %LOCALAPPDATA%/daw_01/recent*.json を汚染しない。
        None,
        48_000, // (A1 r.md #8) test sample rate
    );
    (app, audio_rx, plugin_rx)
}

#[test]
fn cursor_starts_at_zero_when_picker_opens() {
    let (mut app, _, _) = build_app(5);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 0);
    // r.md #110: 5 plugin + 「Parallel」。
    assert_eq!(app.ui_ephemeral.plugin_picker_visible.len(), 6);
}

#[test]
fn cursor_moves_down_within_bounds() {
    let (mut app, _, _) = build_app(5);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    app.handle_event(AppEvent::MovePluginPickerCursor(1));
    app.handle_event(AppEvent::MovePluginPickerCursor(1));
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 2);
}

#[test]
fn cursor_clamps_at_lower_bound() {
    let (mut app, _, _) = build_app(5);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    for _ in 0..10 {
        app.handle_event(AppEvent::MovePluginPickerCursor(-1));
    }
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 0);
}

#[test]
fn cursor_clamps_at_upper_bound() {
    let (mut app, _, _) = build_app(5);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    for _ in 0..10 {
        app.handle_event(AppEvent::MovePluginPickerCursor(1));
    }
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 5); // visible.len() - 1 (5 plugin + Parallel)
}

#[test]
fn cursor_resets_to_zero_when_query_changes() {
    let (mut app, _, _) = build_app(5);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    app.handle_event(AppEvent::MovePluginPickerCursor(1));
    app.handle_event(AppEvent::MovePluginPickerCursor(1));
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 2);
    // クエリを入れると visible が再計算され cursor が 0 に戻る
    app.handle_event(AppEvent::SetPluginPickerQuery("synth".into()));
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 0);
}

#[test]
fn cursor_resets_to_zero_when_picker_reopened() {
    let (mut app, _, _) = build_app(5);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    app.handle_event(AppEvent::MovePluginPickerCursor(1));
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 1);
    // 開き直す → cursor が 0 に戻る
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 0);
}

#[test]
fn move_is_noop_when_visible_is_empty() {
    let (mut app, _, _) = build_app(0);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    // r.md #110: plugin が 0 件でも 「Parallel」 は常に 1 件並ぶ。
    assert_eq!(app.ui_ephemeral.plugin_picker_visible.len(), 1);
    app.handle_event(AppEvent::MovePluginPickerCursor(1));
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 0);
}

#[test]
fn large_delta_clamps_to_bounds() {
    let (mut app, _, _) = build_app(3);
    app.handle_event(AppEvent::OpenPluginPicker { chain: None });
    app.handle_event(AppEvent::MovePluginPickerCursor(100));
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 3); // 3 plugin + Parallel

    app.handle_event(AppEvent::MovePluginPickerCursor(-100));
    assert_eq!(app.ui_ephemeral.plugin_picker_cursor, 0);
}
