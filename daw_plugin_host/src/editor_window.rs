// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Plugin-host-owned Win32 top-level window that hosts a plugin's editor.
//!
//! Previously daw_gui (the GUI process) created the editor's
//! container window and handed its HWND across IPC, so the plugin editor was
//! a child window whose top-level ancestor lived in *another process*. JUCE
//! plugins (e.g. Scaler 2) gate cascade sub-menus on
//! `Process::isForegroundProcess()` — which compares the owning process of
//! the system-wide foreground window to the plugin's own process. With the
//! container owned by daw_gui, that check is always false inside the
//! plugin-host process, so JUCE dismisses any sub-menu (`componentAttachedTo
//! == nullptr` for a sub-menu, so JUCE's `isEmbeddedInForegroundProcess`
//! escape hatch can't apply). See `docs/plan_plugin_editor_topwindow.md`.
//!
//! The fix is to create the editor's top-level window *here*, in the
//! plugin-host process, on the plugin-main thread (the one that runs the
//! `GetMessageW` pump). Clicking into the editor then activates a window this
//! process owns → this process becomes the foreground process → JUCE's
//! `isForegroundProcess()` is true → first-level AND cascade menus work.
//!
//! The window must be a standalone top-level window with **no owner** — if it
//! were owned by daw_gui's main window, `GetAncestor(.., GA_ROOTOWNER)` would
//! climb back into the GUI process and reintroduce the bug.
//!
//! # 窓契約 (r.md #65)
//!
//! この窓は「プラグインの view をぶら下げる箱」ではなく、**VST3 / CLAP が要求する
//! ホスト窓の契約を全部実装した窓**である。契約の正本は
//! `docs/plan_plugin_editor_topwindow.md` §窓契約。要点:
//!
//! - **スタイルは `canResize` / `can_resize` から決める**。プラグインが「ユーザーは
//!   リサイズ不可」と言ったら `WS_THICKFRAME` / `WS_MAXIMIZEBOX` を出さない
//!   (VST3 SDK editorhost `window.cpp` L107-131 と同じ)。attach 後にしかサイズを
//!   答えないプラグイン (Arturia 系) のために **attach 後に再 query して貼り替える**。
//! - **ホスト起点リサイズ** = `WM_SIZING` で矯正 (`checkSizeConstraint` /
//!   `adjust_size`) → OS が窓をリサイズ → `WM_SIZE` で通知 (`onSize` / `set_size`)。
//!   矯正は `WM_SIZING` だけ、通知は `WM_SIZE` だけ (両フォーマットのシーケンス図が
//!   一致)。
//! - **プラグイン起点リサイズ** = [`plugin_requested_resize`] が **同じコールスタックで**
//!   窓をリサイズし `WM_SIZE` 経由で `onSize` まで済ませる。`iplugview.h` の
//!   "Sizing of a view" が *"Afterwards, **in the same callstack**, the host has to
//!   call IPlugView::onSize ()"* と明記している。非同期に回すと `getSize` が旧サイズを
//!   返し続け、実測では Renoise Redux が **自分の view をコンテナから切り離して
//!   WS_POPUP の owned top-level に作り替える** (2026-08-22 `--editor-selftest`)。
//! - **フォーカスは子窓へ渡す**。`DefWindowProc` は `WM_ACTIVATE` でフォーカスを
//!   *アクティブ化された窓自身* に置く (MSDN WM_ACTIVATE Remarks) ので、明示的に
//!   `SetFocus(child)` しない限りプラグインは永久にキーを受け取らない。
//! - **modal move/size ループ中も plugin-main の周期処理を回す**。キャプション /
//!   サイズ枠のドラッグ中は `DefWindowProc` の内側の modal ループに入り、外側の
//!   `GetMessageW` ループが止まる (MSDN WM_ENTERSIZEMOVE *"The operation is complete
//!   when DefWindowProc returns."*)。`WM_ENTERSIZEMOVE` で `SetTimer` し、`WM_TIMER`
//!   から再入安全な部分だけ回す ([`crate::pump_host_during_modal_loop`])。

#![cfg(windows)]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use common::model::EditorWindowGeometry;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, HBRUSH, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CreateWindowExW, DefWindowProcW, DestroyWindow, GW_CHILD, GWLP_USERDATA, GWL_STYLE,
    GetClientRect, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    HICON, HMENU, HWND_TOP, IDC_ARROW, IsIconic, IsWindow, KillTimer, LoadCursorW, PostMessageW,
    RegisterClassExW, SIZE_MINIMIZED, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOCOPYBITS,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, WA_INACTIVE, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_ACTIVATE, WM_ACTIVATEAPP, WM_APP, WM_CLOSE, WM_ENTERSIZEMOVE,
    WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_KEYDOWN, WM_KEYUP, WM_MOVE, WM_PAINT, WM_SIZE, WM_SIZING,
    WM_TIMER, WMSZ_BOTTOM, WMSZ_BOTTOMLEFT, WMSZ_LEFT, WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT,
    WNDCLASSEXW,
    WS_CAPTION, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU,
    WS_THICKFRAME,
};
use windows::core::PCWSTR;

use crate::plugin_instance::EditorSizer;

/// r.md #36: コンテナ窓に返ってきた未消化キーを plugin-main ポンプへ渡すための
/// 内部メッセージ。 `wParam` / `lParam` は元の `WM_KEYDOWN` / `WM_KEYUP` のまま。
///
/// WNDPROC は `extern "system" fn` で `PluginHost` の状態に触れないので、 判定は
/// ポンプ側で行う。 `WM_COMMAND_WAKE` (= `WM_APP + 1`) と衝突しない値を使う。
///
/// **押下と解放で別 id にする**。 1 つにまとめると解放を押下と誤認して 2 回
/// 発火してしまう (`wParam` は仮想キーで埋まっており元 message を載せられない)。
pub const WM_EDITOR_KEY_RELAY_DOWN: u32 = WM_APP + 2;
/// 上の key-up 版。
pub const WM_EDITOR_KEY_RELAY_UP: u32 = WM_APP + 3;

/// r.md #65: modal move/size ループ中に plugin-main の周期処理を回すためのタイマ id。
/// `SetTimer(hwnd, ..)` の **窓タイマ**にする — スレッドタイマ (`hwnd = None`) は
/// `DispatchMessageW` が WNDPROC を呼ばないので modal ループ中に自分のコードへ戻れない。
const SIZEMOVE_TICK_ID: usize = 0x6501;
/// タイマ間隔。`USER_TIMER_MINIMUM` (10ms) より大きいので OS に clamp されない。
const SIZEMOVE_TICK_MS: u32 = 16;

/// C1 (r.md #8): HWND の DPI scale (= `GetDpiForWindow` / 96)。 取得失敗 (dpi 0) や
/// 非 HiDPI は 1.0。 plugin の `gui.set_scale` / VST3 `setContentScaleFactor` に渡し、
/// HiDPI で editor が極小 / ぼやけるのを防ぐ。
#[must_use]
pub fn window_dpi_scale(hwnd_u64: u64) -> f64 {
    let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 }
}

static CLASS_ATOM: OnceLock<u16> = OnceLock::new();
// Win32 stores only a pointer into its class table, so the class-name buffer
// must outlive every window created with it.
static CLASS_NAME: OnceLock<Vec<u16>> = OnceLock::new();

// --- activation / focus 診断トレース ---------------------------------------
//
// エディタ窓の activation / focus は **他プロセス (プラグインが自前で作る窓、
// daw_gui、シェル) が絡む**ので、コードを読むだけでは 「誰が activation を
// 奪ったか」 が原理的に分からない。相手 HWND の PID / TID / クラス名 / タイトルを
// その場で解決して出す口を常設し、`RUST_LOG` で必要なときだけ点ける。
//
//   RUST_LOG=info,editor_win=trace   (daw_gui から起動する子プロセスにも継承される)
//
// `tracing::enabled!` で早期に弾くので、無効時は文字列化も HWND 問い合わせも
// 走らない。

/// この target を `RUST_LOG` で trace にすると窓メッセージの診断が出る。
const TRACE_TARGET: &str = "editor_win";

/// 相手 HWND を `hwnd=0x... pid=.. tid=.. class="..." title="..."` に展開する。
/// `NULL` は `"none"`。診断専用 (有効時しか呼ばない)。
fn describe_hwnd(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetClassNameW, GetWindowTextW, IsWindowVisible,
    };
    if hwnd.0.is_null() {
        return "none".to_string();
    }
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return format!("{:#x}(dead)", hwnd.0 as usize);
    }
    let mut pid = 0u32;
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let mut class_buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut class_buf) };
    let class = String::from_utf16_lossy(&class_buf[..n.max(0) as usize]);
    let mut text_buf = [0u16; 128];
    let n = unsafe { GetWindowTextW(hwnd, &mut text_buf) };
    let title = String::from_utf16_lossy(&text_buf[..n.max(0) as usize]);
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    format!(
        "{:#x} pid={pid} tid={tid} class={class:?} title={title:?} style={style:#x} ex={ex_style:#x} visible={visible}",
        hwnd.0 as usize
    )
}

/// いま誰が foreground / active / focus を持っているかのスナップショット。
fn describe_input_state() -> String {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    format!(
        "fg=[{}] active=[{}] focus=[{}]",
        describe_hwnd(unsafe { GetForegroundWindow() }),
        describe_hwnd(unsafe { GetActiveWindow() }),
        describe_hwnd(unsafe { GetFocus() }),
    )
}

fn trace_enabled() -> bool {
    tracing::enabled!(target: TRACE_TARGET, tracing::Level::TRACE)
}

/// WNDPROC が受けた activation / focus / sizing 系メッセージを 1 行で記録する。
/// 有効でなければ即 return (相手 HWND の問い合わせもしない)。
fn trace_window_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) {
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_CAPTURECHANGED, WM_KILLFOCUS, WM_MOUSEACTIVATE, WM_NCACTIVATE, WM_NCLBUTTONDOWN,
        WM_SETFOCUS, WM_SHOWWINDOW, WM_WINDOWPOSCHANGED, WM_WINDOWPOSCHANGING,
    };
    if !trace_enabled() {
        return;
    }
    let name = match msg {
        WM_ACTIVATE => "WM_ACTIVATE",
        WM_ACTIVATEAPP => "WM_ACTIVATEAPP",
        WM_NCACTIVATE => "WM_NCACTIVATE",
        WM_SETFOCUS => "WM_SETFOCUS",
        WM_KILLFOCUS => "WM_KILLFOCUS",
        WM_MOUSEACTIVATE => "WM_MOUSEACTIVATE",
        WM_NCLBUTTONDOWN => "WM_NCLBUTTONDOWN",
        WM_WINDOWPOSCHANGING => "WM_WINDOWPOSCHANGING",
        WM_WINDOWPOSCHANGED => "WM_WINDOWPOSCHANGED",
        WM_CAPTURECHANGED => "WM_CAPTURECHANGED",
        WM_SHOWWINDOW => "WM_SHOWWINDOW",
        WM_ENTERSIZEMOVE => "WM_ENTERSIZEMOVE",
        WM_EXITSIZEMOVE => "WM_EXITSIZEMOVE",
        WM_SIZING => "WM_SIZING",
        WM_SIZE => "WM_SIZE",
        _ => return,
    };
    // 相手 HWND の在り処はメッセージごとに違う。
    let other = match msg {
        // wParam = activation state, lParam = 相手 top-level。
        WM_ACTIVATE => describe_hwnd(HWND(lparam.0 as *mut core::ffi::c_void)),
        // wParam = 描く状態, lParam = 相手 (-1 は「再描画するな」の sentinel)。
        WM_NCACTIVATE if lparam.0 != -1 => describe_hwnd(HWND(lparam.0 as *mut core::ffi::c_void)),
        // wParam = 相手 HWND。
        WM_SETFOCUS | WM_KILLFOCUS | WM_CAPTURECHANGED => {
            describe_hwnd(HWND(wparam.0 as *mut core::ffi::c_void))
        }
        // wParam = activate される top-level。
        WM_MOUSEACTIVATE => describe_hwnd(HWND(wparam.0 as *mut core::ffi::c_void)),
        _ => "-".to_string(),
    };
    tracing::trace!(
        target: TRACE_TARGET,
        hwnd = format!("{:#x}", hwnd.0 as usize),
        msg = name,
        wparam = format!("{:#x}", wparam.0),
        lparam = format!("{:#x}", lparam.0),
        other = %other,
        state = %describe_input_state(),
        "editor window message"
    );
}

/// r.md #49: このプロセスの窓がアクティブか。
///
/// **プロセス単位の状態であってウィンドウ単位ではない**。`WM_ACTIVATEAPP` は
/// 「別スレッドの窓へ activation が移った / から移ってきた」ときに、そのスレッドの
/// **全 top-level 窓へ**送られる。エディタを 2 枚開いていれば 2 枚とも同じ真偽値を
/// 受け取り、同一スレッド内の窓を行き来しても送られない。よって per-window に持つと
/// 同じ値の重複管理になるだけで、最後に届いた値がプロセスの答えになる。
static WINDOWS_ACTIVE: AtomicBool = AtomicBool::new(false);
/// 未報告の変化があるか。pump が `take_activation_change` で消費して daw_gui へ流す。
static ACTIVATION_DIRTY: AtomicBool = AtomicBool::new(false);

/// WNDPROC (= `extern "system"` で Rust 状態に触れない) から呼ぶ設定口。
/// 値が変わったときだけ dirty を立てる。
fn store_windows_active(active: bool) {
    if WINDOWS_ACTIVE.swap(active, Ordering::AcqRel) != active {
        ACTIVATION_DIRTY.store(true, Ordering::Release);
    }
}

/// 最後のエディタ窓を閉じた等、`WM_ACTIVATEAPP` が来ない経路で非アクティブへ
/// 落とすための強制口。窓が無いプロセスは定義上アクティブでない。
pub fn clear_windows_active() {
    store_windows_active(false);
}

/// 変化があれば新しい値を返して dirty を落とす。plugin-main の pump が毎周回で呼ぶ。
pub fn take_activation_change() -> Option<bool> {
    if ACTIVATION_DIRTY.swap(false, Ordering::AcqRel) {
        Some(WINDOWS_ACTIVE.load(Ordering::Acquire))
    } else {
        None
    }
}

/// Clamp a plugin-reported pixel dimension into a sane positive `i32`.
/// `gui_get_size` / `resizeView` come from the plugin, so a buggy or hostile
/// plugin could report a huge `u32` that would wrap to a negative value on a
/// bare `as i32`. Clamp to `[1, 16384]` — far beyond any real editor, well
/// under the Win32 window-size sanity limit.
fn clamp_dim(v: u32) -> i32 {
    v.clamp(1, 16_384) as i32
}

// --- client ⇄ window 変換 (この 2 関数に集約する) --------------------------

/// client サイズ (physical px) → window 外形サイズ。
///
/// **枠の厚みを `SM_CXSIZEFRAME` 等から手計算しない**。`AdjustWindowRectEx` は
/// *"This API is not DPI aware"* と MS doc が明記しているので、DPI 版
/// `AdjustWindowRectExForDpi` に一本化する (DPI unaware プロセスでは
/// `GetDpiForWindow` が 96 を返すので現状は同値、per-monitor 化しても壊れない)。
/// スタイルは **窓自身から読む** (= 窓が style の SSoT。別に持つと固定枠へ切り替えた
/// 瞬間にフレーム分ずれる)。
unsafe fn client_to_window(hwnd: HWND, client_w: i32, client_h: i32) -> (i32, i32) {
    use windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE;
    let mut rect = RECT { left: 0, top: 0, right: client_w, bottom: client_h };
    unsafe {
        let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
        let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
        let dpi = GetDpiForWindow(hwnd).max(96);
        let _ = AdjustWindowRectExForDpi(&mut rect, style, false, ex, dpi);
    }
    ((rect.right - rect.left).max(1), (rect.bottom - rect.top).max(1))
}

/// 非クライアント枠の厚み `(横, 縦)` = window 寸法 − client 寸法。
/// `client_to_window` の `.max(1)` (外形は 1px 未満にできない) を挟むと
/// 枠 0 の軸で 1px ずれるので、ここは差分を直接取る。
unsafe fn frame_extent(hwnd: HWND) -> (i32, i32) {
    use windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE;
    let mut rect = RECT::default();
    unsafe {
        let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
        let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
        let dpi = GetDpiForWindow(hwnd).max(96);
        let _ = AdjustWindowRectExForDpi(&mut rect, style, false, ex, dpi);
    }
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// 現在の client サイズ。
unsafe fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    (r.right - r.left, r.bottom - r.top)
}

/// client サイズが `w × h` になるよう窓の外形をリサイズする。既に一致していれば
/// **何もしない** (= `WM_SIZE` を起こさない — editorhost `Window::resize` と同じ
/// 冪等ガードで、プラグイン起点 resize の往復を 1 段目で止める)。
///
/// SWP フラグ: `NOMOVE` 位置維持 / `NOZORDER` z 順維持 / `NOACTIVATE`
/// リサイズでフォーカスを奪わない / `NOCOPYBITS` 旧内容コピーのゴースト抑止。
unsafe fn resize_client_area(hwnd: HWND, w: u32, h: u32) {
    let (cw, ch) = (clamp_dim(w), clamp_dim(h));
    unsafe {
        if client_size(hwnd) == (cw, ch) {
            return;
        }
        let (ww, wh) = client_to_window(hwnd, cw, ch);
        if let Err(e) = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            ww,
            wh,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOCOPYBITS,
        ) {
            tracing::warn!(error = ?e, w, h, "editor window SetWindowPos failed");
        }
    }
}

/// 窓のスタイルを `resizable` から決める。
///
/// VST3 SDK editorhost (`window.cpp` L107-131) は
/// `WS_CAPTION|WS_SYSMENU|WS_CLIPCHILDREN|WS_CLIPSIBLINGS` を基本に、resizable の
/// ときだけ `WS_SIZEBOX|WS_MAXIMIZEBOX` を足す。clap-wrapper の standalone も
/// *"We can't resize, so disable WS_THICKFRAME and WS_MAXIMIZEBOX"* と同じ結論。
/// `WS_MINIMIZEBOX` は仕様上どちらでもよいが、DAW の窓としては最小化できるほうが
/// 自然なので常に付ける (従来の `WS_OVERLAPPEDWINDOW` からの挙動維持でもある)。
///
/// `WS_CLIPCHILDREN` はプラグインの子 HWND の領域を親が描かないためのもので必須。
#[must_use]
pub fn editor_window_style(resizable: bool) -> WINDOW_STYLE {
    let base = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN | WS_CLIPSIBLINGS;
    if resizable {
        base | WS_THICKFRAME | WS_MAXIMIZEBOX
    } else {
        base
    }
}

/// WNDPROC (= `extern "system"` で Rust の状態に触れない) と `EditorWindow` の
/// 共有状態。`GWLP_USERDATA` に `Arc::into_raw` で貼り、`Drop` で回収する。
///
/// 全フィールドが plugin-main スレッド専用なので `Cell` で足りる。`Arc` に包むのは
/// 「WNDPROC が生ポインタで借りる」ためだけで、スレッドを跨がない。
struct EditorShared {
    /// Set by the WNDPROC when the user clicks the window's ✕. The
    /// plugin-main loop polls this each iteration and runs the close flow
    /// (`plugin.gui_destroy()` then drop this window) over IPC notify.
    close_requested: Cell<bool>,
    /// 窓の位置 / サイズが確定した (ドラッグ終了 / プラグイン起点 resize 完了)。
    /// pump が [`EditorWindow::take_geometry_change`] で拾って daw_gui へ流す。
    geometry_dirty: Cell<bool>,
    /// WNDPROC がプラグインを叩くための **借用**ポインタ。実体 (`Box`) は
    /// [`EditorWindow::sizer`] が所有する (窓と同じ寿命 = 窓が死ねば誰も参照しない)。
    /// `null` = まだ attach していない / builtin plugin。
    sizer: Cell<*const dyn EditorSizer>,
    /// [`plugin_requested_resize`] の再入ガード。窓リサイズ中にプラグインがまた
    /// `resizeView` を呼んだら拒否する (editorhost の `resizeViewRecursionGard`)。
    in_plugin_resize: Cell<bool>,
    /// `WM_ACTIVATE(WA_INACTIVE)` 時点でフォーカスを持っていた子窓。再アクティブ化で
    /// ここへ戻す (Raymond Chen "Dialog boxes return focus to the control that had
    /// focus when you last switched away" のパターン)。
    last_focus: Cell<isize>,
}

impl EditorShared {
    fn new() -> Self {
        Self {
            close_requested: Cell::new(false),
            geometry_dirty: Cell::new(false),
            sizer: Cell::new(std::ptr::null::<NoopSizer>() as *const dyn EditorSizer),
            in_plugin_resize: Cell::new(false),
            last_focus: Cell::new(0),
        }
    }

    /// 生きている sizer への参照。`gui_destroy` 済み / 未 attach なら `None`。
    fn sizer(&self) -> Option<&dyn EditorSizer> {
        let ptr = self.sizer.get();
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `sizer` は `EditorWindow::attach_sizer` が自分の `Box` から作った
        // ポインタで、`EditorWindow` が生きている間だけ有効。WNDPROC は窓が
        // 生きている間しか走らず、窓は `EditorWindow::drop` の中で `Box` より先に
        // `DestroyWindow` される。
        let sizer = unsafe { &*ptr };
        sizer.is_alive().then_some(sizer)
    }
}

/// `Cell::new` に渡す型付き null 用のダミー (値は作られない)。
struct NoopSizer;
impl EditorSizer for NoopSizer {
    fn constrain_client_size(&self, w: u32, h: u32) -> (u32, u32) {
        (w, h)
    }
    fn current_client_size(&self) -> Option<(u32, u32)> {
        None
    }
    fn notify_client_size(&self, _w: u32, _h: u32) {}
    fn can_resize(&self) -> bool {
        false
    }
    fn resize_hints(&self) -> Option<crate::plugin_instance::ResizeHints> {
        None
    }
    fn is_alive(&self) -> bool {
        false
    }
}

/// RAII wrapper for a plugin-host-owned editor container window. Created on
/// the plugin-main thread; never crosses threads.
///
/// v29: 所属は `InstanceRecord.editor` (device_id keyed の単一 map) が持つ
/// ので、旧 `plugin_id` フィールドと述語 matching は不要になった。
pub struct EditorWindow {
    hwnd: HWND,
    shared: Rc<EditorShared>,
    /// プラグインとのサイズ交渉口の **所有**。`shared.sizer` はこの `Box` を指す
    /// 借用ポインタ。`Drop` の本体で `DestroyWindow` してからこの field が落ちるので、
    /// WNDPROC が dangling を踏むことはない。
    sizer: Option<Box<dyn EditorSizer>>,
}

// HWND is !Send but EditorWindow is owned and used strictly on the
// plugin-main thread; it is never sent across threads.
unsafe impl Send for EditorWindow {}

impl EditorWindow {
    /// Create a standalone top-level container window with a `width × height`
    /// client area. **Must be called on the plugin-main thread** (the one
    /// running the `GetMessageW` pump) so its window messages land on that
    /// thread's queue. The window has no owner (see module docs).
    ///
    /// **窓は隠したまま返る**。表示は [`Self::show_and_focus`] で、プラグインを
    /// attach し終えてから 1 回だけ行う。VST3 SDK editorhost も
    /// `show()` の中で `setContentScaleFactor` → `setFrame` → `attached` を済ませて
    /// から `SWP_SHOWWINDOW` する。早く見せると (a) 空フレームが一瞬見え、
    /// (b) attach 中にプラグインが作る一時 top-level に activation を取られて
    /// **タイトルバーがアクティブ色になった直後に非アクティブへ戻る**。
    ///
    /// `position` は前回セッションの窓位置 (screen 座標)。`None` / 画面外なら既定位置。
    pub fn create(
        width: u32,
        height: u32,
        title: &str,
        resizable: bool,
        position: Option<(i32, i32)>,
    ) -> windows::core::Result<Self> {
        let atom = class_atom()?;
        let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }?;
        let title_utf16: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let style = editor_window_style(resizable);

        // Compute the outer size so the *client* area matches the plugin's
        // requested size (title bar + borders added on top). 窓がまだ無いので
        // ここだけは system DPI で計算する (`GetDpiForWindow` に渡す HWND が無い)。
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: clamp_dim(width),
            bottom: clamp_dim(height),
        };
        unsafe {
            let dpi = windows::Win32::UI::HiDpi::GetDpiForSystem().max(96);
            let _ = AdjustWindowRectExForDpi(&mut rect, style, false, WINDOW_EX_STYLE(0), dpi);
        }
        let outer_w = (rect.right - rect.left).max(1);
        let outer_h = (rect.bottom - rect.top).max(1);
        let (x, y) = position
            .filter(|&(x, y)| point_on_a_monitor(x, y))
            .unwrap_or((120, 120));

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(atom as usize as *const u16),
                PCWSTR(title_utf16.as_ptr()),
                style,
                // x, y, outer width, outer height:
                x,
                y,
                outer_w,
                outer_h,
                None, // no parent
                Some(HMENU(std::ptr::null_mut())),
                Some(hinstance.into()),
                None,
            )
        }?;

        let shared = Rc::new(EditorShared::new());
        unsafe {
            // Leak-into-pointer; reclaimed in Drop.
            let raw = Rc::into_raw(Rc::clone(&shared));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
        }
        Ok(Self { hwnd, shared, sizer: None })
    }

    pub fn hwnd_u64(&self) -> u64 {
        self.hwnd.0 as u64
    }

    /// プラグインとのサイズ交渉口を差し込む。**`attached` / `set_parent` を呼ぶ前に**
    /// 済ませること — VST3 の `attached` doc は *"Note that in this call the plug-in
    /// could call a IPlugFrame::resizeView ()!"* と明記しており、attach の内側から
    /// 飛んでくる resize を捌けるようにしておく必要がある。
    pub fn attach_sizer(&mut self, sizer: Box<dyn EditorSizer>) {
        let ptr: *const dyn EditorSizer = &*sizer;
        self.sizer = Some(sizer);
        self.shared.sizer.set(ptr);
    }

    /// True once if the user clicked ✕ since the last call. The plugin-main
    /// loop polls this to drive the close flow.
    pub fn take_close_request(&self) -> bool {
        self.shared.close_requested.replace(false)
    }

    /// 位置 / サイズが確定していれば現在のジオメトリを返して dirty を落とす。
    /// plugin-main の pump が毎周回で呼び、変化分だけ daw_gui へ流す。
    pub fn take_geometry_change(&self) -> Option<EditorWindowGeometry> {
        if !self.shared.geometry_dirty.replace(false) {
            return None;
        }
        self.persistable_geometry()
    }

    /// 現在の窓位置 (screen 座標) と client サイズ。
    #[must_use]
    pub fn geometry(&self) -> EditorWindowGeometry {
        let mut wr = RECT::default();
        let (cw, ch) = unsafe {
            let _ = GetWindowRect(self.hwnd, &mut wr);
            client_size(self.hwnd)
        };
        EditorWindowGeometry {
            x: wr.left,
            y: wr.top,
            width: cw.max(0) as u32,
            height: ch.max(0) as u32,
        }
    }

    /// **保存してよい**ジオメトリ。最小化中は `None`。
    ///
    /// 最小化された窓の `GetWindowRect` は `(-32000, -32000)`、`GetClientRect` は
    /// `0×0` を返す (`WM_SIZE` の lParam が 0 になるのと同じ理由)。これを永続化すると
    /// 次回 open で 1×1 のエディタ窓になる — 復元側で 0 を 1 に clamp すると
    /// **縮退値が有効値に化ける**ので、源泉で捨てるのが正しい。
    #[must_use]
    pub fn persistable_geometry(&self) -> Option<EditorWindowGeometry> {
        if unsafe { IsIconic(self.hwnd) }.as_bool() {
            return None;
        }
        let g = self.geometry();
        (g.width > 0 && g.height > 0).then_some(g)
    }

    /// attach 後に判明した `canResize` / `can_resize` を窓スタイルへ反映する。
    ///
    /// **attach 前の値を焼き込んではいけない**: Arturia Analog Lab 系は attach 前に
    /// サイズもリサイズ可否も答えられず、pre-attach の値で固定枠にすると恒久的な退行に
    /// なる。MS doc は `SetWindowLongPtr` の後に
    /// `SetWindowPos(SWP_NOMOVE|SWP_NOSIZE|SWP_NOZORDER|SWP_FRAMECHANGED)` を
    /// 名指しで要求している (それ無しでは非クライアント領域が再計算されない)。
    /// 枠の厚みが変わるので、最後に client サイズを保つよう外形を作り直す。
    pub fn set_resizable(&self, resizable: bool) {
        let want = editor_window_style(resizable);
        unsafe {
            let cur = GetWindowLongPtrW(self.hwnd, GWL_STYLE) as u32;
            // 可視状態など、こちらが決めない bit は保存する。
            let managed = (WS_THICKFRAME | WS_MAXIMIZEBOX).0;
            let next = (cur & !managed) | (want.0 & managed);
            if next == cur {
                return;
            }
            let (cw, ch) = client_size(self.hwnd);
            SetWindowLongPtrW(self.hwnd, GWL_STYLE, next as isize);
            if let Err(e) = SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            ) {
                tracing::warn!(error = ?e, "editor window SWP_FRAMECHANGED failed");
            }
            if cw > 0 && ch > 0 {
                resize_client_area(self.hwnd, cw as u32, ch as u32);
            }
        }
        tracing::debug!(resizable, "editor window style updated after attach");
    }

    #[allow(dead_code)]
    pub fn set_title(&self, title: &str) {
        let title_utf16: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let _ = SetWindowTextW(self.hwnd, PCWSTR(title_utf16.as_ptr()));
        }
    }

    /// Resize so the *client area* is `width × height` (the plugin paints
    /// into the client area). Title bar + borders are added via
    /// `AdjustWindowRectExForDpi`.
    pub fn set_client_size(&self, width: u32, height: u32) {
        unsafe { resize_client_area(self.hwnd, width, height) }
    }

    /// 窓を **初めて** 可視化し、前面へ出す。open シーケンスの最後に 1 回だけ呼ぶ。
    ///
    /// `SetForegroundWindow` の戻り値を必ず記録する: `false` は「Win32 の
    /// foreground 制限に弾かれた」で、MSDN が挙げる許可条件
    /// (呼び出し元が foreground / foreground プロセスに起動された / 最後の入力を
    /// 受けた / `AllowSetForegroundWindow` の許可) をどれも満たさなかったことを意味する。
    /// 許可はワンショット (*"loses the ability ... the next time that either the user
    /// generates input"*) なので、**この 1 回に賭ける**のが正しい設計。
    pub fn show_and_focus(&self) {
        unsafe {
            // `HWND_TOP` (= `SWP_NOZORDER` を付けない) は editorhost `Window::show` と
            // 同じ。`SetForegroundWindow` が拒否されても、少なくとも窓が他の窓の下に
            // 埋もれた状態で「開いた」ことにはならない。
            if let Err(e) = SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOCOPYBITS | SWP_SHOWWINDOW,
            ) {
                tracing::warn!(error = ?e, "editor window SWP_SHOWWINDOW failed");
            }
            let ok = SetForegroundWindow(self.hwnd).as_bool();
            if !ok {
                tracing::info!(
                    hwnd = self.hwnd.0 as usize,
                    "SetForegroundWindow refused (Win32 foreground restriction); \
                     the editor stays behind and Windows flashes its taskbar button"
                );
            }
            // フォーカスをプラグインの子窓へ。`WM_ACTIVATE` でも同じことをするが、
            // 表示直後は activation が既に載っていて `WM_ACTIVATE` が来ないことがある。
            focus_plugin_child(self.hwnd, &self.shared);
        }
    }

    /// Hide the window without tearing it down (used as an interim step;
    /// final teardown is the `Drop`).
    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl Drop for EditorWindow {
    fn drop(&mut self) {
        if !self.hwnd.0.is_null() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), SIZEMOVE_TICK_ID);
                // WNDPROC が二度と sizer を辿らないようにしてから窓を壊す。
                self.shared
                    .sizer
                    .set(std::ptr::null::<NoopSizer>() as *const dyn EditorSizer);
                // Reclaim the Arc we leaked into GWLP_USERDATA.
                let raw = GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) as *const EditorShared;
                if !raw.is_null() {
                    SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                    drop(Rc::from_raw(raw));
                }
                let _ = DestroyWindow(self.hwnd);
            }
            tracing::info!(hwnd = self.hwnd.0 as usize, "editor window destroyed");
        }
        // `self.sizer` (Box) はこの後 field drop で落ちる = DestroyWindow の後。
    }
}

/// `(x, y)` がいずれかのモニタ上にあるか。復元した窓位置が今のモニタ構成で
/// 画面外なら既定位置へ落とすための判定。
fn point_on_a_monitor(x: i32, y: i32) -> bool {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromPoint};
    // タイトルバーが掴める位置に居ることを見たいので、左上から少し内側を見る。
    let p = POINT { x: x + 32, y: y + 8 };
    !unsafe { MonitorFromPoint(p, MONITOR_DEFAULTTONULL) }.is_invalid()
}

/// プラグインが作った子窓へキーボードフォーカスを渡す。
///
/// `DefWindowProc` は `WM_ACTIVATE` でフォーカスを **アクティブ化された窓自身** に
/// 置く (MSDN: *"the DefWindowProc function sets the keyboard focus to the window"*)
/// ので、これを呼ばない限りプラグインは 1 打鍵も受け取らない。
///
/// - `GetWindow(GW_CHILD)` は Z 順先頭の直接の子 1 枚。プラグインは通常 `attached` /
///   `set_parent` 先に 1 枚だけ作る。**HWND をキャッシュしない** (作り直される)。
/// - `GetFocus() != target` のガードが必須。子の WNDPROC が親へ `SetFocus` を返す
///   フレームワークだと無限往復になる (プラグインは第三者コードなので有り得ないとは
///   仮定できない)。
/// - `SetFocus` は窓が呼び出しスレッドのキューに属していないと **NULL を返して黙って
///   失敗する**ので、失敗はログに出す。
fn focus_plugin_child(hwnd: HWND, shared: &EditorShared) {
    unsafe {
        let remembered = HWND(shared.last_focus.get() as *mut core::ffi::c_void);
        let target = if !remembered.0.is_null()
            && IsWindow(Some(remembered)).as_bool()
            && windows::Win32::UI::WindowsAndMessaging::IsChild(hwnd, remembered).as_bool()
        {
            remembered
        } else {
            GetWindow(hwnd, GW_CHILD).unwrap_or_default()
        };
        if target.0.is_null() || GetFocus() == target {
            return;
        }
        if let Err(e) = SetFocus(Some(target)) {
            tracing::debug!(
                child = target.0 as usize,
                error = ?e,
                "SetFocus to plugin child failed (window not on this thread?)"
            );
        }
    }
}

/// プラグイン起点のリサイズ (VST3 `IPlugFrame::resizeView` / CLAP
/// `clap_host_gui.request_resize`) を **同じコールスタックで** 完遂する。
///
/// 手順は VST3 SDK editorhost `WindowController::resizeView` (editorhost.cpp
/// L395-420) と dev portal の "Initiated from Plug-in" シーケンス図に一致させる:
///
/// 1. 再入中なら拒否 (`false` → VST3 は `kResultFalse`)
/// 2. 現在サイズと同じなら何もせず成功 (`onSize` も呼ばない)
/// 3. 窓をリサイズ → `WM_SIZE` が同期で飛び、`WM_SIZE` ハンドラが `onSize` /
///    `set_size` を呼ぶ
/// 4. それでもサイズが合っていなければ保険で直接通知する
///
/// **`checkSizeConstraint` / `adjust_size` は掛けない** — ヘッダにも dev portal の
/// 図にも editorhost / JUCE の実装にも、プラグイン起点フローでの矯正は登場しない
/// (矯正はホスト起点ドラッグ専用)。掛けると、プラグインが要求した値と `onSize` に
/// 渡す値がずれて `kResultFalse` を返される (実ログで全 VST3 に `onSize -> 0x1` の
/// WARN が出ていた原因)。
///
/// 結果は 3 状態。**`Rejected` と `NotApplicable` を混ぜてはいけない**: 再入拒否を
/// 非同期経路へ積み直すと、プラグインには「拒否」と伝えたリサイズが 1 周期後に
/// 実行され、プラグインの内部状態と窓サイズが食い違う (editorhost の
/// `resizeViewRecursionGard` は拒否したら何も残さない)。
#[must_use]
pub fn plugin_requested_resize(hwnd_u64: u64, width: u32, height: u32) -> PluginResizeOutcome {
    let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
    if hwnd.0.is_null() || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return PluginResizeOutcome::NotApplicable;
    }
    // 窓メッセージを扱えるのは窓を作ったスレッドだけ。CLAP の `request_resize` は
    // `[thread-safe]` なので任意スレッドから来る。
    if unsafe { GetWindowThreadProcessId(hwnd, None) } != unsafe { GetCurrentThreadId() } {
        return PluginResizeOutcome::NotApplicable;
    }
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const EditorShared;
    if raw.is_null() {
        return PluginResizeOutcome::NotApplicable;
    }
    // SAFETY: `EditorWindow` が生きている間だけ非 null (Drop が 0 に戻す)。
    let shared = unsafe { &*raw };
    if shared.in_plugin_resize.get() {
        return PluginResizeOutcome::Rejected;
    }
    let Some(sizer) = shared.sizer() else {
        return PluginResizeOutcome::NotApplicable;
    };
    if sizer.current_client_size() == Some((width, height)) {
        return PluginResizeOutcome::Applied; // 既に同じ = 何もしないのが正
    }
    shared.in_plugin_resize.set(true);
    unsafe { resize_client_area(hwnd, width, height) };
    // `SetWindowPos` が WM_SIZE を出さなかった (= サイズが変わらなかった) ケースの保険。
    if let Some(sizer) = shared.sizer()
        && sizer.current_client_size() != Some((width, height))
    {
        sizer.notify_client_size(width, height);
    }
    shared.in_plugin_resize.set(false);
    shared.geometry_dirty.set(true);
    PluginResizeOutcome::Applied
}

/// [`plugin_requested_resize`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginResizeOutcome {
    /// 窓を直し、`onSize` / `set_size` まで済ませた。
    Applied,
    /// **再入中なので拒否した**。呼び出し側はプラグインに拒否を伝えるだけで、
    /// 非同期経路へ積み直してはいけない。
    Rejected,
    /// この窓では処理できない (GUI 未 open / 窓が別スレッド所有 / sizer 未 attach)。
    /// 呼び出し側は非同期経路 (`HostCallbacks::on_request_resize`) へ回してよい。
    NotApplicable,
}

// --- Win32 class registration --------------------------------------------

fn class_atom() -> windows::core::Result<u16> {
    if let Some(atom) = CLASS_ATOM.get() {
        return Ok(*atom);
    }
    let (atom, name) = unsafe { register_class() }?;
    let _ = CLASS_NAME.set(name);
    let _ = CLASS_ATOM.set(atom);
    Ok(*CLASS_ATOM.get().unwrap())
}

unsafe fn register_class() -> windows::core::Result<(u16, Vec<u16>)> {
    let name: Vec<u16> = "daw_01_plugin_editor_window\0".encode_utf16().collect();
    let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }?;
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        // editorhost と同じく `CS_DBLCLKS` のみ。`CS_HREDRAW|CS_VREDRAW` を付けると
        // リサイズのたびに親の client 全域が無効化され、子 (プラグイン) の上でちらつく。
        style: CS_DBLCLKS,
        lpfnWndProc: Some(editor_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance.into(),
        hIcon: HICON(std::ptr::null_mut()),
        // NULL のままだと直前の窓のカーソル形状が残る。
        hCursor: cursor,
        // 背景ブラシ無し + WM_ERASEBKGND=TRUE で、client 領域はプラグインに完全に任せる。
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(name.as_ptr()),
        hIconSm: HICON(std::ptr::null_mut()),
    };
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        return Err(windows::core::Error::from_thread());
    }
    Ok((atom, name))
}

/// `GWLP_USERDATA` から共有状態を借りる。`CreateWindowExW` の内側から来る
/// メッセージ (WM_NCCREATE 等) ではまだ null。
fn shared_of(hwnd: HWND) -> Option<&'static EditorShared> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const EditorShared;
    if raw.is_null() {
        return None;
    }
    // SAFETY: `EditorWindow::drop` が `DestroyWindow` より前に 0 を書き戻すので、
    // 非 null の間は `Arc` が生きている。参照は WNDPROC の呼び出し内でのみ使う。
    Some(unsafe { &*raw })
}

/// ホスト起点 (ユーザーのドラッグ) の矩形矯正。`WM_SIZING` の drag rect は
/// **window 座標 / screen** なので、非クライアント枠の厚みを引いて client サイズにし、
/// プラグインへ通してから戻す。
///
/// `wParam` (`WMSZ_*`) を見て **掴んでいない辺を固定する**。editorhost は wParam を
/// 見ず常に left/top 固定なので、左辺 / 上辺ドラッグで窓が反対方向へ流れる。
fn constrain_drag_rect(hwnd: HWND, shared: &EditorShared, edge: u32, rect: &mut RECT) {
    let Some(sizer) = shared.sizer() else { return };
    let (fx, fy) = unsafe { frame_extent(hwnd) };
    let cw = ((rect.right - rect.left) - fx).max(1);
    let ch = ((rect.bottom - rect.top) - fy).max(1);
    let (mut aw, mut ah) = sizer.constrain_client_size(cw as u32, ch as u32);

    // CLAP `get_resize_hints`: 軸ごとのリサイズ可否とアスペクト比。VST3 は None
    // (`checkSizeConstraint` が丸めて返してくるのに任せる)。
    if let Some(hints) = sizer.resize_hints() {
        let cur = unsafe { client_size(hwnd) };
        if !hints.can_resize_horizontally && cur.0 > 0 {
            aw = cur.0 as u32;
        }
        if !hints.can_resize_vertically && cur.1 > 0 {
            ah = cur.1 as u32;
        }
        // ratio 値は preserve が true のときだけ有効 (ヘッダのコメント)。
        if hints.preserve_aspect_ratio
            && hints.can_resize_horizontally
            && hints.can_resize_vertically
            && hints.aspect_ratio_width > 0
            && hints.aspect_ratio_height > 0
        {
            let (rw, rh) = (
                i64::from(hints.aspect_ratio_width),
                i64::from(hints.aspect_ratio_height),
            );
            // 上下辺を掴んでいるときは高さが主、それ以外 (左右辺 / 角) は幅が主。
            let height_major = matches!(edge, WMSZ_TOP | WMSZ_BOTTOM);
            if height_major {
                aw = ((i64::from(ah) * rw) / rh).clamp(1, 16_384) as u32;
            } else {
                ah = ((i64::from(aw) * rh) / rw).clamp(1, 16_384) as u32;
            }
        }
    }

    let (nw, nh) = (clamp_dim(aw) + fx, clamp_dim(ah) + fy);
    if matches!(edge, WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT) {
        rect.left = rect.right - nw;
    } else {
        rect.right = rect.left + nw;
    }
    if matches!(edge, WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT) {
        rect.top = rect.bottom - nh;
    } else {
        rect.bottom = rect.top + nh;
    }
}

/// WNDPROC。窓契約の実体はモジュール doc を参照。
///
/// `WM_CLOSE` だけは `DefWindowProcW` に流してはいけない (`DestroyWindow` が
/// RAII wrapper の背後で HWND を壊す)。plugin-main の loop が flag を poll し、
/// `plugin.gui_destroy()` → `EditorWindow` drop の spec-correct な順で片付ける。
unsafe extern "system" fn editor_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    trace_window_message(hwnd, msg, wparam, lparam);
    // r.md #36 経路 B: JUCE / iPlug2 は **自分が消化しなかった** キーだけを
    // 親 / ルート HWND (= このコンテナ窓) へ返す規約を持つ。 よってここに
    // キーが届いた時点で 「プラグインが要らないと言った」 が確定する。
    //
    // JUCE は `PostMessage(GetParent(hwnd), ...)`、 iPlug2 は
    // `SendMessageW(GetAncestor(hWnd, GA_ROOT), ...)`。 前者はキューを経由して
    // ポンプ → `DispatchMessageW` でここに来るが、 後者は WNDPROC 直接呼び出しで
    // ポンプを通らない。 両者を 1 本にまとめるため、 ここでは判定せず
    // `WM_EDITOR_KEY_RELAY` として **自分のキューへ積み直す** だけにする。
    // 実際の転送判定は plugin-main ポンプ側 (`PluginHost` の状態が要る) が行う。
    // **`WM_SYSKEY*` (Alt 付き / F10) は横取りしない**。 それらは `DefWindowProc` が
    // Alt+F4 → `WM_SYSCOMMAND(SC_CLOSE)`、 Alt+Space → system menu、 F10 → メニュー活性
    // という既定動作を生成する起点で、 飲み込むとこの窓が Alt+F4 で閉じられなくなる。
    // 現状の転送対象 (Space / Ctrl+S) は Alt 修飾を持たないので、 素の
    // `WM_KEYDOWN` / `WM_KEYUP` だけ見れば足りる。
    if msg == WM_KEYDOWN || msg == WM_KEYUP {
        let relay = if msg == WM_KEYDOWN {
            WM_EDITOR_KEY_RELAY_DOWN
        } else {
            WM_EDITOR_KEY_RELAY_UP
        };
        unsafe {
            let _ = PostMessageW(Some(hwnd), relay, wparam, LPARAM(lparam.0));
        }
        // プラグインは既に 「要らない」 と言っている。 素のキーに `DefWindowProc` の
        // 既定動作は無いので、 ここで止めて二重処理を避ける。
        return LRESULT(0);
    }

    match msg {
        // client 領域はプラグインの子窓が全面を描く。ホストは一切塗らない
        // (`hbrBackground = NULL` と対で、リサイズ時の白/黒フラッシュを防ぐ)。
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            }
            return LRESULT(0);
        }

        // --- ホスト起点リサイズ ------------------------------------------
        // ライブドラッグ中の drag rect (window 座標 / screen) をプラグインが
        // 受け入れる client サイズへ丸めて書き戻す。**ここでは通知しない**
        // (通知は OS が窓を変えた後の WM_SIZE)。VST3 / CLAP どちらのシーケンス図も
        // 「制約 → 窓リサイズ → 通知」の 3 段で、制約と通知を混ぜない。
        WM_SIZING => {
            if lparam.0 != 0
                && let Some(shared) = shared_of(hwnd)
            {
                // SAFETY: WM_SIZING の lParam は OS が渡す有効な RECT*。
                let rect = unsafe { &mut *(lparam.0 as *mut RECT) };
                #[allow(clippy::cast_possible_truncation)]
                constrain_drag_rect(hwnd, shared, wparam.0 as u32, rect);
            }
            // doc: "An application should return TRUE if it processes this message."
            return LRESULT(1);
        }
        // 確定した client サイズをプラグインへ通知する。**プラグインの現在サイズと
        // 違うときだけ**呼ぶ (editorhost `WindowController::onResize` と同じ) —
        // これがプラグイン起点 resize との往復を止める唯一のガードになる。
        WM_SIZE => {
            if wparam.0 as u32 != SIZE_MINIMIZED
                && let Some(shared) = shared_of(hwnd)
                && let Some(sizer) = shared.sizer()
            {
                #[allow(clippy::cast_possible_truncation)]
                let cw = (lparam.0 & 0xFFFF) as u32;
                #[allow(clippy::cast_possible_truncation)]
                let ch = ((lparam.0 >> 16) & 0xFFFF) as u32;
                // 縮退サイズ (0x0) をプラグインへ流さない — サイズ依存の描画が
                // 0 次元で走る。
                if cw > 0 && ch > 0 && sizer.current_client_size() != Some((cw, ch)) {
                    sizer.notify_client_size(cw, ch);
                }
            }
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }

        // --- modal move/size ループ -------------------------------------
        // キャプション / サイズ枠のドラッグ中は `DefWindowProc` の内側の modal
        // ループに入り、外側の `GetMessageW` ループが止まる。窓タイマを仕掛けて
        // WNDPROC 経由で plugin-main の周期処理へ戻る口を作る。
        WM_ENTERSIZEMOVE => {
            unsafe { SetTimer(Some(hwnd), SIZEMOVE_TICK_ID, SIZEMOVE_TICK_MS, None) };
            return LRESULT(0);
        }
        WM_EXITSIZEMOVE => {
            unsafe {
                let _ = KillTimer(Some(hwnd), SIZEMOVE_TICK_ID);
            }
            if let Some(shared) = shared_of(hwnd) {
                shared.geometry_dirty.set(true);
            }
            return LRESULT(0);
        }
        WM_TIMER if wparam.0 == SIZEMOVE_TICK_ID => {
            crate::pump_host_during_modal_loop();
            return LRESULT(0);
        }
        // ドラッグ終了で位置が変わっていれば拾う (`WM_EXITSIZEMOVE` が来ない
        // プログラム移動もここで捕まえる)。値は pump が `GetWindowRect` で読む。
        // 最小化中は位置が `(-32000, -32000)` になるので dirty を立てない
        // (`WM_SIZE` が `SIZE_MINIMIZED` を弾くのと同じ理由)。
        WM_MOVE => {
            if !unsafe { IsIconic(hwnd) }.as_bool()
                && let Some(shared) = shared_of(hwnd)
            {
                shared.geometry_dirty.set(true);
            }
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }

        // --- フォーカス転送 ----------------------------------------------
        // Microsoft の正準パターン (Raymond Chen 2014-05-21) は WM_SETFOCUS では
        // なく WM_ACTIVATE 側。非アクティブ化で「どの子がフォーカスを持っていたか」を
        // 覚え、再アクティブ化でそこへ戻す。`WM_KILLFOCUS` の中では **絶対に**
        // 触らない (MSDN: *"do not make any function calls that display or activate
        // a window"*、`SetFocus` はアクティブ化の副作用を持つので直接抵触する)。
        WM_ACTIVATE => {
            let shared = shared_of(hwnd);
            let state = (wparam.0 & 0xFFFF) as u32;
            let minimized = (wparam.0 >> 16) != 0;
            // 非アクティブ化は **DefWindowProc より前**に読む。実測の順序は
            // `WM_ACTIVATE(WA_INACTIVE)` → `WM_KILLFOCUS` なので、この時点なら
            // まだ `GetFocus` が「どの子を触っていたか」を答える。
            if !minimized
                && state == WA_INACTIVE
                && let Some(shared) = shared
            {
                unsafe {
                    let f = GetFocus();
                    if !f.0.is_null()
                        && windows::Win32::UI::WindowsAndMessaging::IsChild(hwnd, f).as_bool()
                    {
                        shared.last_focus.set(f.0 as isize);
                    }
                }
            }
            // 既定処理を必ず通す (これを止めるとアクティブ化そのものが壊れる)。
            let res = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            // **DefWindowProc の後**に子へ移譲する。`DefWindowProc` は
            // *"sets the keyboard focus to the window"* (= コンテナ自身) なので、
            // 先に `SetFocus(child)` しても即座に上書きされてしまう
            // (実ログで `WM_ACTIVATE(WA_CLICKACTIVE)` の直後に
            //  `WM_SETFOCUS wparam=0` が来ているのがその上書き)。
            if !minimized
                && state != WA_INACTIVE
                && let Some(shared) = shared
            {
                focus_plugin_child(hwnd, shared);
            }
            return res;
        }

        // r.md #49: アプリ全体のアクティブ判定。daw_gui は自分の窓しか見えないので、
        // 「プラグインエディタを触っている間もアプリはアクティブ」をこちらから報告する。
        // `wparam != 0` = このスレッドの窓が activation を得た。
        WM_ACTIVATEAPP => {
            store_windows_active(wparam.0 != 0);
            // 既定動作 (フォーカス周りの内部処理) は潰さずそのまま流す。
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }

        WM_CLOSE => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            if let Some(shared) = shared_of(hwnd) {
                shared.close_requested.set(true);
            }
            return LRESULT(0);
        }
        _ => {}
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// r.md #65: `canResize()` が false のプラグイン (Renoise Redux 等) の窓は
    /// **ユーザーがリサイズできてはいけない**。VST3 SDK editorhost も
    /// clap-wrapper も「不可なら `WS_THICKFRAME` / `WS_MAXIMIZEBOX` を出さない」で
    /// 一致しており、これがこの項目のユーザー決定でもある。
    #[test]
    fn non_resizable_editor_has_no_resize_frame() {
        let fixed = editor_window_style(false);
        assert_eq!(fixed & WS_THICKFRAME, WINDOW_STYLE(0));
        assert_eq!(fixed & WS_MAXIMIZEBOX, WINDOW_STYLE(0));
        // 固定枠でもキャプション / システムメニュー / 最小化は残す (閉じられること)。
        assert_eq!(fixed & WS_CAPTION, WS_CAPTION);
        assert_eq!(fixed & WS_SYSMENU, WS_SYSMENU);

        let sizable = editor_window_style(true);
        assert_eq!(sizable & WS_THICKFRAME, WS_THICKFRAME);
        assert_eq!(sizable & WS_MAXIMIZEBOX, WS_MAXIMIZEBOX);
        // 子 (プラグイン窓) の領域を親が描かないための必須スタイルは両方に入る。
        for s in [fixed, sizable] {
            assert_eq!(s & WS_CLIPCHILDREN, WS_CLIPCHILDREN);
            assert_eq!(s & WS_CLIPSIBLINGS, WS_CLIPSIBLINGS);
        }
    }
}
