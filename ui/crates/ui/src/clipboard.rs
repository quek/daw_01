// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! M8 Phase 31: clipboard 抽象。
//!
//! `ClipboardProvider` trait は OS clipboard との interaction を library から切り離す。
//! UiHost は構築時に provider を渡し (`UiHost::with_clipboard(provider)`)、Ui::frame 内で
//! request/response 形式で読み書きする。
//!
//! - text 取得: shortcut "paste" がマッチしたフレームの開始時に provider.get_text() で読み出し
//!   → `Ui::take_clipboard_paste()` で widget が 1 度だけ消費
//! - text 書込: `Ui::set_clipboard_text(s)` でフレーム末尾の "pending write" に積む
//!   → `UiHost::frame` 末尾で provider.set_text(s)
//!
//! M13 baseview backend 移行時には `ClipboardProvider` の別実装に差し替え可能。
//! winit backend では `daw_ui_platform::ArboardClipboard` (feature `clipboard` 有効時) を
//! `UiHost::with_clipboard(ArboardClipboard::new())` で渡す。

/// OS clipboard と読み書きする trait。
pub trait ClipboardProvider: Send {
    /// 現在の clipboard の text を取得。失敗時 (Linux で xclip 不在等) は None を返す。
    fn get_text(&mut self) -> Option<String>;

    /// clipboard に text を書き込む。失敗は内部で握りつぶす (no-op に degrade)。
    fn set_text(&mut self, text: String);
}

/// 動作不能な環境用 (test / clipboard 無効化したい environment / Linux で xclip 不在等) の
/// no-op provider。`get_text` は常に None、`set_text` は何もしない。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopClipboard;

impl ClipboardProvider for NoopClipboard {
    fn get_text(&mut self) -> Option<String> {
        None
    }
    fn set_text(&mut self, _text: String) {}
}

/// M8 Phase 31: arboard backed clipboard provider (feature `clipboard` 有効時のみ)。
///
/// `Clipboard::new()` は OS によって失敗する可能性がある (Linux で xclip / xsel / wl-clipboard
/// 未インストール等)。失敗時は `inner = None` で no-op に degrade、UI 側からは `set_text` が
/// 黙って捨てられ `get_text` が常に None を返す。これにより clipboard 不在環境でも main UI が
/// 動作する。失敗時は eprintln でログを残す (tracing crate 未導入のため)。
#[cfg(feature = "clipboard")]
pub struct ArboardClipboard {
    inner: Option<arboard::Clipboard>,
}

#[cfg(feature = "clipboard")]
impl ArboardClipboard {
    /// 新しい clipboard provider を構築する。失敗時は eprintln でログを残し no-op に degrade。
    #[must_use]
    pub fn new() -> Self {
        match arboard::Clipboard::new() {
            Ok(c) => Self { inner: Some(c) },
            Err(e) => {
                eprintln!("[daw-ui-core] ArboardClipboard::new failed: {e}; clipboard will be no-op");
                Self { inner: None }
            }
        }
    }

    /// Provider が機能しているかを判定 (`new()` が成功したか)。
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.inner.is_some()
    }
}

#[cfg(feature = "clipboard")]
impl Default for ArboardClipboard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "clipboard")]
impl ClipboardProvider for ArboardClipboard {
    fn get_text(&mut self) -> Option<String> {
        let c = self.inner.as_mut()?;
        match c.get_text() {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("[daw-ui-core] ArboardClipboard::get_text failed: {e}");
                None
            }
        }
    }
    fn set_text(&mut self, text: String) {
        if let Some(c) = self.inner.as_mut()
            && let Err(e) = c.set_text(text)
        {
            eprintln!("[daw-ui-core] ArboardClipboard::set_text failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_clipboard_returns_none_and_swallows_writes() {
        let mut c = NoopClipboard;
        assert!(c.get_text().is_none());
        c.set_text("hello".into());
        assert!(c.get_text().is_none());
    }
}
