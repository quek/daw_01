// gui_01 (daw-ui) ベースの view モジュール群。

pub mod about;
pub mod arrangement_view;
pub mod audio_editor;
pub mod bottom_panel;
/// Ctrl+C / Ctrl+X / Ctrl+V / D — 編集面ごとのクリップボード操作 (root.rs から分離)。
pub mod clipboard_ops;
pub mod dirty_guard_modal;
/// r.md #74: 開閉マーク (disclosure triangle) の glyph 規則の SSoT。
pub mod disclosure;
pub mod export_overlay;
/// r.md #61: 終了処理中オーバーレイ。
pub mod shutdown_overlay;
pub mod export_range_modal;
pub mod font_picker;
/// r.md #87: クリップランチャーのキーボード操作と widget イベントの流し込み。
pub mod launcher_keys;
/// ランチャー widget の intent を `AppEvent` へ橋渡しする (view → handler の唯一の口)。
pub mod launcher_bridge;
pub mod load_overlay;
pub mod loudness_report;
pub mod master_panel;
pub mod mixer_strips;
pub mod modulation;
pub mod param_gesture;
pub mod plugin_picker;
pub mod preview_window;
pub mod recovery_modal;
pub mod resource_monitor;
pub mod root;
pub mod runner;
pub mod scrub_gesture;
pub mod settings;
pub mod shortcuts;
pub mod shortcuts_help;
pub mod snap;
pub mod status_bar;
pub mod track_color;
pub mod undo_history;
pub mod track_inspector;
pub mod track_picker;
pub mod transport;
pub mod voicevox_overlay;
