// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #36: プラグインエディタ窓のキーを 「**プラグインが消化しなかったものだけ**」
//! ホスト (daw_gui) へ返すための判定。
//!
//! # なぜ判定が要るのか
//!
//! エディタ窓にフォーカスがある間、 `WM_KEYDOWN` は plugin-main スレッドのキューに入り
//! そのままプラグインへ dispatch される。 ここで Space を無条件に横取りすると、
//! プラグインのプリセット名入力欄に空白が打てなくなる。 逆に何もしないと
//! 「エディタを開いている間だけ Space で再生できない」。
//!
//! # どうやって判定するか (一次情報)
//!
//! Win32 にも CLAP にも 「フォーカス中のコントロールがこのキーを欲しがっているか」 を
//! 一般に問い合わせる手段は無い (CLAP は `include/clap/ext` / `ext/draft` を全列挙しても
//! キーボード関連の拡張がゼロ)。 実際に効くのは次の 2 経路で、 **両方入れて初めて主要
//! フレームワークが揃う**。
//!
//! ## 経路 B (主): 未消化キーの親窓バブリングを拾う
//!
//! - **JUCE**: `HWNDComponentPeer::peerWindowProc` は
//!   `if (doKeyDown (wParam)) return 0; forwardMessageToParent (...)` で、
//!   未消化キーを `PostMessage(GetParent(hwnd), ...)` する。 そして
//!   `juce::TextEditor::keyStateChanged` は修飾なしの全キーで `true` を返す
//!   (JUCE 本体のコメントに "overridden to avoid forwarding key events to the parent")。
//!   ⇒ **テキスト欄にフォーカスがあるときの Space は親に来ない / 無いときだけ来る**。
//!   我々のエディタ窓 = その親なので、 [`EditorWindow`] の WNDPROC に届いた時点で
//!   「プラグインが要らないと言った」 が確定する。
//! - **iPlug2**: `IGraphicsWin::WndProc` が未処理キーを
//!   `SendMessageW(GetAncestor(hWnd, GA_ROOT), msg, ...)` で投げ返す。 同じく確定。
//!
//! ## 経路 A (副): `WM_GETDLGCODE` でフォーカス窓に問い合わせる
//!
//! - **VSTGUI** (Steinberg 系 VST3 の多く): `Win32Frame` は `WM_GETDLGCODE` を
//!   **実装していない** ので `DefWindowProc` が 0 を返す (= 「要らない」)。 一方
//!   文字編集中は `win32textedit.cpp` が本物の `"EDIT"` を `CreateWindowEx` して
//!   `SetFocus` し、 そのサブクラスが `DLGC_WANTALLKEYS` を返す。 ⇒ 完全に判別できる。
//! - **注意**: JUCE と iPlug2 のメイン窓は **フォーカス状態に関係なく無条件で
//!   `DLGC_WANTALLKEYS`** を返す。 経路 A 単独だとこの 2 つから永久にキーを取れない。
//!   だから経路 B が主で A が副。
//! - VSTGUI の EDIT サブクラスは `DLGC_WANTCHARS` を落として `DLGC_WANTALLKEYS` だけを
//!   返すので、 `WANTCHARS` 単独判定は誤り。 両方見る。
//!
//! ## 判定できないもの
//!
//! Dear ImGui / GLFW / 自前 OpenGL 系は `WM_GETDLGCODE` に応答せず (DefWindowProc の 0)、
//! 親へも転送せず、 Win32 の caret も作らないので **外から入力中か知る手段が無い**。
//! REAPER も自動交渉を諦めて FX ごとの 「Send all keyboard input to plug-in」 トグルを
//! 持っている。 本実装も同じ逃げ道 ([`PluginCommand::SetEditorSendAllKeys`]) を用意する。

use common::protocol::KeyChord;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, DLGC_WANTALLKEYS, DLGC_WANTCHARS, MSG, SMTO_ABORTIFHUNG, SMTO_BLOCK,
    WM_GETDLGCODE, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// キー押下 / 解放メッセージか。 `Alt` 修飾付きは `WM_SYSKEY*` で届くので両方見る。
#[must_use]
pub fn is_key_down(message: u32) -> bool {
    message == WM_KEYDOWN || message == WM_SYSKEYDOWN
}

/// 対になる key-up。 down だけ奪って up をプラグインに流すと、 押しっぱなし状態を
/// 追うプラグインが 「押されたまま」 と誤認するので、 奪うなら対で奪う。
#[must_use]
pub fn is_key_up(message: u32) -> bool {
    message == WM_KEYUP || message == WM_SYSKEYUP
}

/// メッセージが自動リピート由来か (lParam bit30 = 直前の押下状態)。
/// 1 押下 1 発火にするため、 リピートは転送しない (`runner.rs` の
/// プレビュー窓が `!event.repeat` を見るのと同じ規約)。
#[must_use]
pub fn is_auto_repeat(lparam: LPARAM) -> bool {
    (lparam.0 >> 30) & 1 == 1
}

/// 現在の修飾キー状態を載せた chord を組み立てる。
///
/// `GetKeyState` は **呼び出しスレッドのキュー基準**の状態を返すので、 plugin-main の
/// メッセージポンプ内で呼ぶ限り 「そのキーが押された瞬間の修飾」 と一致する。
#[must_use]
pub fn chord_of(msg: &MSG) -> KeyChord {
    let down = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| -> bool {
        // 最上位ビットが立っていれば押下中。
        unsafe { GetKeyState(i32::from(vk.0)) < 0 }
    };
    KeyChord {
        vk: u16::try_from(msg.wParam.0).unwrap_or(0),
        ctrl: down(VK_CONTROL),
        shift: down(VK_SHIFT),
        alt: down(VK_MENU),
    }
}

/// フォーカス窓がこのキーを欲しがっているか (経路 A)。
///
/// `hwnd` はメッセージの宛先窓 (= フォーカス窓)。 キーボードメッセージは
/// 「フォーカス窓を作ったスレッドのキュー」 に入るので、 plugin-main のポンプが
/// 取り出せた時点でその窓は同一スレッド所有であり、 `SendMessage` は WNDPROC の
/// 直接呼び出しになる (デッドロックしない)。 それでも防御的に timeout 付きを使う。
#[must_use]
pub fn window_wants_key(hwnd: HWND, msg: &MSG) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let mut result: usize = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETDLGCODE,
            WPARAM(msg.wParam.0),
            LPARAM(std::ptr::from_ref(msg) as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            50,
            Some(&mut result),
        )
    };
    if ok.0 == 0 {
        // 応答なし = 判定不能。 安全側 (プラグインに渡す) へ倒す。
        return true;
    }
    #[allow(clippy::cast_possible_truncation)]
    let code = result as u32;
    // VSTGUI の EDIT サブクラスは WANTCHARS を落として WANTALLKEYS だけ返すので両方見る。
    code & (DLGC_WANTALLKEYS | DLGC_WANTCHARS) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_repeat_reads_lparam_bit30() {
        assert!(!is_auto_repeat(LPARAM(0)));
        assert!(is_auto_repeat(LPARAM(1 << 30)));
    }

    #[test]
    fn key_message_classification() {
        assert!(is_key_down(WM_KEYDOWN));
        assert!(is_key_down(WM_SYSKEYDOWN));
        assert!(!is_key_down(WM_KEYUP));
        assert!(is_key_up(WM_KEYUP));
        assert!(is_key_up(WM_SYSKEYUP));
    }
}
