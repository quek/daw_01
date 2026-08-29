//! `AppData` の event handler 群 (domain 別)。 app.rs の god-file を
//! docs/plan_arch_refactor.md §7 に沿って分割したもの。 dispatch は
//! app.rs の `handle_event`、 各 arm の本体がここのメソッド。
pub mod activity;
pub mod audio_editor;
pub mod automation;
pub mod automation_lanes;
pub mod bounce;
pub mod clip_events;
pub mod clips;
pub mod device_relocate;
pub mod devices;
pub mod export;
pub mod glue;
pub mod grouping;
pub mod ipc;
/// r.md #87: クリップランチャーの発火 / 行の主導権 / 列 (シーン) の CRUD。
pub mod launcher;
/// r.md #87: ランチャーのセル CRUD とローンチ設定。
pub mod launcher_cells;
/// r.md #87: ランチャーのセルの copy / cut / paste。
pub mod launcher_clipboard;
pub mod loudness;
pub mod master_panel;
pub mod media;
pub mod midi;
pub mod mixer;
pub mod modulation;
pub mod notes;
pub mod project;
pub mod selection_view;
/// r.md #61: 終了シーケンスの実行 (子プロセス teardown の待ち合わせ)。
pub mod shutdown;
pub mod sync;
pub mod tick;
pub mod tracks;
pub mod transport;
pub mod view_model;
/// `ViewState` の snapshot / restore (保存される表示状態の唯一の口)。
pub mod view_state;
pub mod voicevox;
