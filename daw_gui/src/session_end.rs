//! r.md #61: Windows のサインアウト / シャットダウンに対応する。
//!
//! # なぜ subclass が要るか
//!
//! winit 0.30.13 の win32 backend は `WM_QUERYENDSESSION` / `WM_ENDSESSION` を
//! **一切扱わない** (`platform_impl/windows/event_loop.rs` を grep しても 0 件)。
//! 扱うのは `WM_CLOSE` → `CloseRequested` だけなので、OS がセッションを終わらせる
//! ときは `CloseRequested` が発火せず、**未保存確認すら出ないまま殺される**。
//!
//! そこでメインウィンドウの WNDPROC を差し替えて自分でこの 2 つを拾う。
//!
//! # なぜ `GWLP_USERDATA` を使わないか
//!
//! `daw_plugin_host::editor_window` の idiom (`GWLP_USERDATA` に `Arc` を leak) は
//! **自分で `RegisterClassExW` した窓専用**。メインウィンドウは winit が作り、
//! winit 自身が `GWLP_USERDATA` に `WindowData` ポインタを入れて全メッセージ処理に
//! 使っている (`WM_NCDESTROY` で 0 に戻す)。ここを奪うと winit が壊れる。
//!
//! 一方 `GWLP_WNDPROC` は winit が触らない (WNDPROC はクラス登録時に固定し、
//! `set_window_long` は `GWL_USERDATA` / `GWL_STYLE` にしか使わない) ので、
//! `SetWindowLongPtrW(GWLP_WNDPROC)` + `CallWindowProcW` の古典的 subclass が
//! 競合なく成立する。`comctl32` の `SetWindowSubclass` でも良いが、そちらは
//! `Win32_UI_Shell` feature (57k 行) を丸ごと有効化することになる。
//!
//! # なぜ WNDPROC から `AppData` を触らないか
//!
//! `WM_QUERYENDSESSION` は winit の pump の `DispatchMessageW` から **同期に**
//! 呼ばれる。つまり `ApplicationHandler::window_event` のスタックの内側で発火し、
//! その時点で `RunnerState.app` は上位フレームに `&mut` で借用されている。
//! よってここから `AppData` には**原理的に触れない**。
//!
//! # 応答の設計 (MSDN の指定どおり)
//!
//! - **`WM_QUERYENDSESSION` は即答する**。
//!   > Applications should respect the user's intentions and return **TRUE**. …
//!   > Each application should return TRUE or FALSE immediately upon receiving this
//!   > message, and **defer any cleanup operations until it receives the
//!   > WM_ENDSESSION message**.
//!
//!   ここで **0 (FALSE) を返すのは重い**:
//!   > **If any application returns zero, the session is not ended. The system stops
//!   > sending WM_QUERYENDSESSION messages as soon as one application returns zero.**
//!
//!   つまり FALSE はセッション終了を取り消すだけでなく、**まだ聞かれていない
//!   他のアプリに WM_QUERYENDSESSION が届かなくなる** (= 隣で開いている未保存の
//!   Word が保存確認を出す機会を奪う)。だから FALSE を返してよいのは
//!   **こちらに未保存の変更があるときだけ**にする。
//!
//! - **後始末は `WM_ENDSESSION`** で行う。ただしここでも WNDPROC の中で
//!   子プロセスを畳む実装は書かない (書けば `AppData` を触れないぶん別実装になり、
//!   「終わり方」が 2 つに割れる)。`AppEvent::Quit` を投げて **通常の終了
//!   シーケンス** ([`crate::shutdown`]) に任せ、WNDPROC は即 return する。
//!   その後もイベントループは回り続けるので、シーケンスが完走してプロセスが
//!   終わる。Windows はアプリの exit を待ってくれる (待ちきれなければブロッカー
//!   画面を出す = 事実がそのまま表示されるだけ)。
//!
//! - **ブロック理由の登録は WNDPROC の中ではなく「中断できない状態に入った
//!   とき」**。`ShutdownBlockReasonCreate` の Remarks:
//!   > Applications should call this function **as they begin an operation that
//!   > cannot be interrupted**, such as burning a CD or DVD.
//!
//!   daw_01 にとってのそれは「未保存の変更を抱えている」なので、dirty ミラーの
//!   更新 ([`set_dirty`]) がそのまま登録 / 解除になる。
//!
//! 参照:
//! - <https://learn.microsoft.com/en-us/windows/win32/shutdown/wm-queryendsession>
//! - <https://learn.microsoft.com/en-us/windows/win32/shutdown/wm-endsession>
//! - <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-shutdownblockreasoncreate>

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{
    GetLastError, HWND, LPARAM, LRESULT, SetLastError, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, SetWindowLongPtrW, WM_ENDSESSION,
    WM_QUERYENDSESSION, WNDPROC,
};
use windows::core::HSTRING;
use winit::event_loop::EventLoopProxy;

use crate::event::AppEvent;
use crate::shutdown::QuitRequest;

/// シャットダウン画面に出す理由。MSDN 曰く「ユーザーは数秒しか見ないので短く」。
const BLOCK_REASON: &str = "未保存の変更があります";

/// WNDPROC から参照できる最小限の材料。窓は 1 つしか無いのでプロセス global。
struct SessionEnd {
    hwnd: isize,
    proxy: EventLoopProxy<AppEvent>,
    /// 未保存か。`AppData` には触れないので、runner が毎フレームここへ写す
    /// (`ActivityState::awake` と同じ idiom)。
    dirty: AtomicBool,
    /// `ShutdownBlockReasonCreate` 済みか (二重登録 / 消し忘れの防止)。
    blocked: AtomicBool,
}

static STATE: OnceLock<SessionEnd> = OnceLock::new();
/// 差し替え前の WNDPROC (`CallWindowProcW` に渡す)。0 = 未 install。
static PREV_WNDPROC: AtomicIsize = AtomicIsize::new(0);

/// メインウィンドウの WNDPROC を差し替える。`resumed` で窓を作った直後に 1 度だけ。
pub fn install(hwnd: isize, proxy: EventLoopProxy<AppEvent>) {
    if STATE.get().is_some() {
        return;
    }
    let _ = STATE.set(SessionEnd {
        hwnd,
        proxy,
        dirty: AtomicBool::new(false),
        blocked: AtomicBool::new(false),
    });
    let proc_ptr = subclass_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
    // `SetWindowLongPtrW` は「0 を返した」だけでは失敗と断定できない (直前の値が
    // 0 だった可能性がある) ので、MSDN 指定どおり `SetLastError(0)` してから呼び、
    // 0 + エラーコード有りのときだけ失敗と判定する。
    // <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowlongptrw>
    let prev = unsafe {
        SetLastError(WIN32_ERROR(0));
        SetWindowLongPtrW(as_hwnd(hwnd), GWLP_WNDPROC, proc_ptr as *const () as isize)
    };
    if prev == 0 && unsafe { GetLastError() } != WIN32_ERROR(0) {
        tracing::warn!("failed to subclass the main window; OS session-end will not be handled");
        return;
    }
    PREV_WNDPROC.store(prev, Ordering::Release);
    tracing::info!("installed WM_QUERYENDSESSION handler on the main window");
}

/// 差し替えを外し、ブロック理由も消す。`exiting` (= 通常終了) で呼ぶ。
/// OS に強制終了される経路では呼ばれないが、そのときはプロセスごと消える。
pub fn uninstall() {
    let Some(state) = STATE.get() else { return };
    clear_block_reason(state);
    let prev = PREV_WNDPROC.swap(0, Ordering::AcqRel);
    if prev != 0 {
        unsafe { SetWindowLongPtrW(as_hwnd(state.hwnd), GWLP_WNDPROC, prev) };
    }
}

/// 未保存かのミラーを更新する。runner が毎フレーム呼ぶ。
///
/// **同時に `ShutdownBlockReasonCreate` / `Destroy` を維持する**のがここの役目。
/// MSDN は「中断できない操作に入るときに登録しろ」と言っており、daw_01 に
/// とってのそれは「未保存の変更を抱えている」。WNDPROC の中で登録するのは
/// 仕様の使い方ではない (あちらは即答すべき場所)。
pub fn set_dirty(dirty: bool) {
    let Some(state) = STATE.get() else { return };
    if state.dirty.swap(dirty, Ordering::AcqRel) == dirty {
        return; // 変化なし (毎フレーム呼ばれるので早期 return)。
    }
    if dirty {
        set_block_reason(state);
    } else {
        clear_block_reason(state);
    }
}

fn as_hwnd(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

fn set_block_reason(state: &SessionEnd) {
    if state.blocked.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(e) =
        unsafe { ShutdownBlockReasonCreate(as_hwnd(state.hwnd), &HSTRING::from(BLOCK_REASON)) }
    {
        tracing::warn!(error = %e, "ShutdownBlockReasonCreate failed");
        state.blocked.store(false, Ordering::Release);
    }
}

fn clear_block_reason(state: &SessionEnd) {
    if !state.blocked.swap(false, Ordering::AcqRel) {
        return;
    }
    if let Err(e) = unsafe { ShutdownBlockReasonDestroy(as_hwnd(state.hwnd)) } {
        tracing::warn!(error = %e, "ShutdownBlockReasonDestroy failed");
    }
}

/// 通常の終了シーケンスを起こす。WNDPROC はここから先を待たない
/// (イベントループが回り続けて完走する)。
fn request_quit(state: &SessionEnd) {
    if state.proxy.send_event(AppEvent::Quit(QuitRequest::USER)).is_err() {
        tracing::warn!("event loop already closed; cannot start the shutdown sequence");
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => {
            let Some(state) = STATE.get() else {
                return LRESULT(1); // 材料が無ければ止める理由も無い。
            };
            let dirty = state.dirty.load(Ordering::Acquire);
            tracing::info!(dirty, "WM_QUERYENDSESSION: OS session is ending");
            if !dirty {
                // 何も失うものが無いので **ユーザーの意図を尊重して TRUE**。
                // 後始末は WM_ENDSESSION で行う (MSDN の指定どおり)。
                return LRESULT(1);
            }
            // 未保存の変更がある。FALSE を返してセッション終了を止め、
            // 登録済みの理由 (`BLOCK_REASON`) をシャットダウン画面に出させる。
            // ユーザーが「キャンセル」で戻れるよう、同時に確認モーダルを開く。
            //
            // FALSE は「他のアプリへの照会も止める」重い応答なので、**ここでしか
            // 返さない** (module doc 参照)。
            request_quit(state);
            LRESULT(0)
        }
        WM_ENDSESSION => {
            let Some(state) = STATE.get() else {
                return LRESULT(0);
            };
            if wparam.0 == 0 {
                // セッション終了は取り消された。ブロック理由は dirty ミラーが
                // 管理しているのでここで触ることは無い。
                tracing::info!("WM_ENDSESSION: session end was cancelled");
                return LRESULT(0);
            }
            // 本当に終わる。**ここで子プロセスを畳む実装は書かない** —
            // 通常の終了シーケンスを起こして即 return し、以後もイベントループが
            // 回って完走する (window_state.json の保存も `Runner::exiting` が担う)。
            // Windows はアプリの exit を待つ。
            tracing::warn!("WM_ENDSESSION: session is ending now; starting the shutdown sequence");
            request_quit(state);
            LRESULT(0)
        }
        _ => {
            let prev = PREV_WNDPROC.load(Ordering::Acquire);
            if prev == 0 {
                // 元の WNDPROC が分からない (install 直後 / uninstall 済み) 状態で
                // メッセージが来た。`LRESULT(0)` を返すと窓の既定動作を全部潰して
                // しまうので、必ず既定へ落とす。
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            // SAFETY: `prev` は `SetWindowLongPtrW` が返した元の WNDPROC の関数
            // ポインタ。`WNDPROC = Option<unsafe extern "system" fn(..)>` は
            // null 許容なので `isize` からの transmute はサイズ・表現とも一致する
            // (0 は上で弾いてあるので `Some` になる)。
            let prev: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(prev) };
            unsafe { CallWindowProcW(prev, hwnd, msg, wparam, lparam) }
        }
    }
}
