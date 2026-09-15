//! daw_gui ライブラリ (binary `daw_gui` と integration test 共通)。
//!
//! `cargo test --test <name>` で `tests/` 配下の integration test が
//! `use daw_gui::...` で `AppData` / `dispatcher` 等の API を参照できる
//! ようにするため、 binary `main.rs` から module 群を切り出して library
//! として公開する。
//!
//! production binary (`src/main.rs`) は `daw_gui::run` を呼ぶ薄い entry
//! 役、 integration test は `AppData::new` を直接呼んで一連の操作を
//! シミュレートする。 dispatcher trait のおかげで winit EventLoop 無しで
//! 構築できる。

pub mod app;
pub mod app_config;
pub mod app_types;
/// インスペクタの chain list の行モデル (`app_types` から切り出し)。
pub mod chain_rows;
/// 色編集の宛先 (`color_picker` overlay の対象、`app_types` から切り出し)。
pub mod color_target;
pub mod device_addr;
pub mod recent;
pub mod window_state;
#[cfg(test)]
mod app_tests;
#[cfg(test)]
pub(crate) mod test_support;
pub mod event;
/// 貼り付け / カット / コピーのイベント (`AppEvent::Clipboard` の中身)。
pub mod event_clipboard;
/// チェーン上のデバイス操作のイベント (`AppEvent::Device` の中身)。
pub mod event_device;
/// r.md #129: 内蔵 device / master Limiter の値編集 (`DeviceEvent::NativeEdit` の中身)。
pub mod event_native;
/// r.md #87: クリップランチャーのイベントと値型 (`AppEvent::Launcher` の中身)。
pub mod event_launcher;
/// 範囲選択の中身の移動 / 複製 / ミュート (`AppEvent::Range` の中身)。
pub mod event_range;
pub mod event_sampler;
/// Arranger セクション帯の編集 (`AppEvent::Section` の中身)。
pub mod event_section;
/// r.md #132: 分割 / 結合キーのイベント (`AppEvent::SplitJoin` の中身)。
pub mod event_split;
pub mod event_tabs;
pub mod event_virtual_keyboard;
pub mod handler;
pub mod audio_source_cache;
pub mod automation_label;
pub mod automation_value;
pub mod bootstrap;
pub mod clipboard;
pub mod dispatcher;
pub mod fuzzy;
pub mod group_compose;
pub mod image_compose;
pub mod import_audio;
pub mod import_image;
pub mod media_bundle;
pub mod media_dest;
pub mod launcher_time;
pub mod text_compose;
#[cfg(windows)]
pub mod import_video;
#[cfg(windows)]
pub mod libav_decoder;
#[cfg(windows)]
pub mod libav_encoder;
#[cfg(windows)]
pub mod render_video;
#[cfg(windows)]
pub mod video_playback;
#[cfg(windows)]
pub mod video_playback_worker;
pub mod job;
pub mod master_meter;
pub mod midi;
pub mod midi_export;
pub mod midi_import;
pub mod note_ops;
// `--script` headless テスト駆動。JS エンジン boa_engine を抱えるので `script`
// feature 有効時のみコンパイルする (default ビルドのコールド時間短縮)。
#[cfg(feature = "script")]
pub mod script;
/// r.md #61: Windows のサインアウト / シャットダウン (`WM_QUERYENDSESSION`)。
#[cfg(windows)]
pub mod session_end;
/// r.md #61: 終了シーケンスの状態機械 (全終了経路の合流点)。
pub mod shutdown;
pub mod single_instance;
pub mod state;
#[cfg(windows)]
pub mod smoke_test;
pub mod subprocess;
/// テスト fixture 用 `ffmpeg` CLI の解決とエンコーダ指定 (テストビルドのみ)。
#[cfg(test)]
pub mod test_ffmpeg;
pub mod theme;
pub mod video_fx;
pub mod view;
/// r.md #113: 仮想鍵盤の配列 (どの PC キーが何半音か) の SSoT。
pub mod virtual_keyboard;
pub mod voicevox_client;
pub mod voicevox_engine;
pub mod widgets;
