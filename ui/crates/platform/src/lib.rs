// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! プラットフォーム層 — ウィンドウとイベントの中立抽象。
//!
//! 設計目的:
//! - winit / baseview の両方に乗れるよう、`WindowBackend` trait と中立な `AppEvent`
//!   enum を提供し、上位層 (renderer / ui) が winit の型を直接知らなくて済むようにする。
//! - raw-window-handle 経由で wgpu と接続するため、surface 作成に必要な情報を露出する。

pub mod acp_map;
pub mod event;
pub mod text_document;
#[cfg(target_os = "windows")]
pub mod tsf;
pub mod window;
pub mod winit_backend;

pub use event::*;
pub use text_document::{ImeTextEdit, RectPx, TextDocument};
pub use window::*;
// 独自イベントループを持つ consumer (daw_01 runner) が直接構築・利用できるよう
// root に re-export する (`run_app` 経由でない使い方)。
pub use winit_backend::WinitWindow;
