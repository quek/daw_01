//! `AppData` の event handler 群 (domain 別)。 app.rs の god-file を
//! docs/plan_arch_refactor.md §7 に沿って分割したもの。 dispatch は
//! app.rs の `handle_event`、 各 arm の本体がここのメソッド。
pub mod activity;
pub mod audio_editor;
pub mod automation;
pub mod automation_lanes;
pub mod bounce;
/// r.md #129: `Q` の宛先 (Mixer 帯 / マスターパネルの hover → device / Limiter)。
pub mod bypass_target;
pub mod clip_events;
/// `AppEvent::Clipboard(..)` の入口 (貼り付け / カット / コピー)。
pub mod clipboard_event;
pub mod clipboard_media;
pub mod clips;
pub mod colors;
/// `AppEvent::Device(..)` の dispatcher (`DeviceEvent` の振り分け)。
pub mod device_event;
/// r.md #129 (Q5): 組み込み内蔵 device を消せない / 包めない / 普通のドラッグで運べない絞り込み。
pub mod device_guard;
pub mod device_relocate;
pub mod devices;
pub mod export;
pub mod glue;
pub mod grouping;
/// 履歴ジャンプ (undo / redo / 履歴リストの行) の入口と、plugin state の往復を先に挟む判断。
pub mod history;
pub mod ipc;
/// r.md #87: クリップランチャーの発火 / 行の主導権 / 列 (シーン) の CRUD。
pub mod launcher;
/// r.md #87: ランチャーのセル CRUD とローンチ設定。
pub mod launcher_cells;
/// r.md #87: クリップをセルへ運ぶときのオートメーション追従 (`plan_range_selection.md` §5)。
pub mod launcher_cells_automation;
/// r.md #87: ランチャーのセルの copy / cut / paste。
pub mod launcher_clipboard;
pub mod loudness;
pub mod master_panel;
pub mod media;
pub mod midi;
pub mod sampler;
pub mod mixer;
pub mod modulation;
/// r.md #129: 内蔵 device / master Limiter の値編集と追加 (値 IPC の唯一の口)。
pub mod native_edit;
pub mod note_selection;
pub mod note_nudge;
pub mod notes;
pub mod project;
pub mod parallel;
pub mod param_gesture;
/// パラメーターの現在値と、 それを初期値にする `A` キーのレーン追加 (`automation_lanes.rs` から分離)。
pub mod param_value;
pub mod range_ops;
/// autosave / クラッシュ復旧 modal / recovery file の掃除 (`project.rs` から分離)。
pub mod recovery;
/// r.md #129: Rack の Par パネルの開閉 (見方の都合)。
pub mod rack_view;
pub mod save_bundle;
/// `AppEvent::Section(..)` の入口 (Arranger セクション帯の編集)。
pub mod section_event;
/// r.md #129: SC Listen (聴き方の都合、Song に書かない) の唯一の口。
pub mod sc_listen;
pub mod select_all;
pub mod selection_view;
/// r.md #61: 終了シーケンスの実行 (子プロセス teardown の待ち合わせ)。
pub mod shutdown;
/// r.md #132: 分割 (`E` / `Shift+E`) と結合 (`J`) の入口。
pub mod split;
pub mod sync;
pub mod tabs;
pub mod tick;
/// r.md #131: トラックの無効化 / 有効化 (編集の口と plugin host の追従)。
pub mod track_enable;
pub mod tracks;
pub mod transport;
/// r.md #130: グローバルトランスポーズ (基準値 / 追従 / 演奏プレビューの鍵盤 / ヘッダの印)。
pub mod transpose;
pub mod view_model;
/// `ViewState` の snapshot / restore (保存される表示状態の唯一の口)。
pub mod view_state;
/// r.md #113: PC キーボードによる仮想鍵盤 (PC キー → MIDI 入力と同じ入口)。
pub mod virtual_keyboard;
pub mod voicevox;
