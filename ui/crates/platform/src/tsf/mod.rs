// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows TSF (Text Services Framework) 連携。
//!
//! gui_01 の `text_input` を TSF `ITextStoreACP` document store として OS IME に公開し、
//! rtry (Try-Code TIP) のまぜ書き変換 / ストロークヘルプや MS-IME 再変換が、アプリの text store
//! から `GetText` でカーソル前テキストを読めるようにする。
//!
//! 構成:
//! - [`doc_state`] — COM 非依存の純粋な store ロジック (キャッシュ snapshot + 編集キュー)。
//! - [`text_store`] — `#[implement(ITextStoreACP)]` の薄い COM shim。
//! - [`thread_mgr`] — `ITfThreadMgr` の activate / context / focus 配線。
//!
//! `#[cfg(target_os = "windows")]` でのみコンパイルされる (`crate::lib` で gate)。

pub mod doc_state;
pub mod text_store;
pub mod thread_mgr;

pub use doc_state::{DocState, Notify};
pub use thread_mgr::TsfManager;
