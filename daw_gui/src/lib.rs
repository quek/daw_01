// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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
pub mod recent;
pub mod window_state;
#[cfg(test)]
mod app_tests;
pub mod event;
pub mod handler;
pub mod audio_source_cache;
pub mod automation_value;
pub mod bootstrap;
pub mod clipboard;
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
// `--script` headless テスト駆動。JS エンジン boa_engine を抱えるので `script`
// feature 有効時のみコンパイルする (default ビルドのコールド時間短縮)。
#[cfg(feature = "script")]
pub mod script;
pub mod single_instance;
pub mod state;
#[cfg(windows)]
pub mod smoke_test;
pub mod subprocess;
pub mod theme;
pub mod video_fx;
pub mod view;
pub mod voicevox_client;
pub mod voicevox_engine;
pub mod widgets;
