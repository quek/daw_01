//! Integration test: フォントピッカーの絞り込み + デフォルト行 + カーソル
//! (`docs/plan_font_picker.md`)。
//!
//! 検証する model 操作:
//! - `FontFamiliesLoaded` で visible 構築、 先頭にデフォルト行 (`""`)
//! - `SetFontPickerQuery` で subsequence 絞り込み + デフォルト行は非表示
//! - `MoveFontPickerCursor` で `[0, visible.len()-1]` clamp
//!
//! 各行の実フォント描画 / ↑↓・ホバーのライブプレビュー / commit-cancel は
//! visual & stateful なので手動確認 (CLAUDE.md run before commit)。

use std::sync::Arc;

use common::protocol::MainToChild;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn build_app() -> (
    AppData,
    UnboundedReceiver<MainToChild>,
    UnboundedReceiver<MainToChild>,
) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
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
        // app_dirs: None = 永続化なし。 実 %LOCALAPPDATA%/daw_01/recent*.json を汚染しない。
        None,
        48_000, // (A1 r.md #8) test sample rate
    );
    (app, audio_rx, plugin_rx)
}

fn families() -> Vec<String> {
    vec![
        "Arial".into(),
        "Times New Roman".into(),
        "Yu Gothic".into(),
    ]
}

#[test]
fn loaded_families_show_with_default_row_first() {
    let (mut app, _, _) = build_app();
    app.handle_event(AppEvent::FontFamiliesLoaded(families()));
    // query 空 → 先頭にデフォルト行 ("") + 全フォント。
    assert_eq!(
        app.font_picker_visible,
        vec![
            String::new(),
            "Arial".to_string(),
            "Times New Roman".to_string(),
            "Yu Gothic".to_string(),
        ],
    );
    assert_eq!(app.font_picker_cursor, 0);
    assert!(!app.font_picker_loading);
}

#[test]
fn query_filters_and_drops_default_row() {
    let (mut app, _, _) = build_app();
    app.handle_event(AppEvent::FontFamiliesLoaded(families()));
    // "Aria" は "Arial" にのみ subsequence マッチ (case-insensitive)。
    app.handle_event(AppEvent::SetFontPickerQuery("Aria".into()));
    assert_eq!(app.font_picker_visible, vec!["Arial".to_string()]);
    assert_eq!(app.font_picker_cursor, 0);
    // 空クエリに戻すとデフォルト行 + 全件が復活。
    app.handle_event(AppEvent::SetFontPickerQuery(String::new()));
    assert_eq!(app.font_picker_visible.len(), 4);
    assert_eq!(app.font_picker_visible[0], "");
}

#[test]
fn cursor_clamps_within_visible() {
    let (mut app, _, _) = build_app();
    app.handle_event(AppEvent::FontFamiliesLoaded(families()));
    app.handle_event(AppEvent::MoveFontPickerCursor(100));
    assert_eq!(app.font_picker_cursor, 3); // 4 件 (default + 3) → max index 3
    app.handle_event(AppEvent::MoveFontPickerCursor(-100));
    assert_eq!(app.font_picker_cursor, 0);
}
