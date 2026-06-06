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
pub mod audio_source_cache;
pub mod bootstrap;
pub mod dispatcher;
pub mod fuzzy;
pub mod group_compose;
pub mod image_compose;
pub mod import_audio;
pub mod import_image;
pub mod text_compose;
#[cfg(windows)]
pub mod import_video;
#[cfg(windows)]
pub mod libav_encoder;
#[cfg(windows)]
pub mod render_video;
#[cfg(windows)]
pub mod video_playback;
#[cfg(windows)]
pub mod video_playback_worker;
pub mod job;
pub mod midi;
pub mod midi_export;
pub mod script;
#[cfg(windows)]
pub mod smoke_test;
pub mod subprocess;
pub mod view;
