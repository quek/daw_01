//! `daw-ui-platform::WindowBackend` を満たす winit window ラッパー。
//!
//! gui_01 (`daw-ui-platform::winit_backend::WinitWindow`) は内部 `new` が pub
//! でないため外部から構築できない。daw_gui は EventLoopProxy<AppEvent> を
//! 載せた独自イベントループを駆動する都合で WinitWindow は使えず、ここで
//! 同等の Wrapper を定義する。

use std::sync::Arc;

use daw_ui_platform::{CursorIcon, ImeTextEdit, PhysicalSize, TextDocument, WindowBackend};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use winit::dpi::{PhysicalPosition as WinitPhysPos, PhysicalSize as WinitPhysSize};
use winit::window::{CursorIcon as WinitCursor, Window};

// Windows TSF (`ITextStoreACP`) は STA / UI スレッド専有なので、COM オブジェクトを
// `DawGuiWindow` (Send 要求あり) に持たせず、イベントループスレッドの thread-local に置く。
// winit メッセージポンプ・`UiHost::frame` flush・msctf からの store 呼び出しはすべて
// このスレッド (`Runner` = winit `ApplicationHandler`)。gui_01 `WinitWindow` と同パターンだが、
// daw_gui は独自イベントループを駆動する都合で `WinitWindow` を使えず、その TSF 配線
// (`set_text_input_document` / `take_ime_text_edits`) もここで複製する。これが無いと rtry の
// まぜ書き変換 / MS-IME 再変換がアプリの text store を `GetText` で読めず、変換結果が壊れる
// (= まぜ書き不能。winit IMM の `Commit` だけが届き読みの一部しか確定しない)。
//
// `Failed` は apartment 衝突等で TSF を諦め winit IMM に degrade した状態。teardown は
// `TsfManager::Drop` (この thread-local の destructor) 任せ。
#[cfg(windows)]
enum TsfSlot {
    Untried,
    Failed,
    Active(daw_ui_platform::tsf::TsfManager),
}

#[cfg(windows)]
thread_local! {
    static TSF_MANAGER: std::cell::RefCell<TsfSlot> =
        const { std::cell::RefCell::new(TsfSlot::Untried) };
}

#[derive(Clone)]
pub struct DawGuiWindow {
    inner: Arc<Window>,
}

impl DawGuiWindow {
    pub fn new(window: Arc<Window>) -> Self {
        Self { inner: window }
    }

    pub fn inner(&self) -> &Arc<Window> {
        &self.inner
    }

    /// Windows 上で `HWND` を `isize` として返す (winit
    /// `WindowAttributesExtWindows::with_owner_window` 引数用)。 preview
    /// window を main window の owned-window として登録するための取得経路。
    /// raw-window-handle が Win32 variant でなければ `None`。
    #[cfg(windows)]
    pub fn hwnd_isize(&self) -> Option<isize> {
        use raw_window_handle::RawWindowHandle;
        let handle = self.inner.window_handle().ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        }
    }
}

impl HasWindowHandle for DawGuiWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.inner.window_handle()
    }
}

impl HasDisplayHandle for DawGuiWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.inner.display_handle()
    }
}

impl WindowBackend for DawGuiWindow {
    fn inner_size(&self) -> PhysicalSize {
        let s: WinitPhysSize<u32> = self.inner.inner_size();
        PhysicalSize {
            width: s.width,
            height: s.height,
        }
    }

    fn scale_factor(&self) -> f64 {
        self.inner.scale_factor()
    }

    fn request_redraw(&self) {
        self.inner.request_redraw();
    }

    fn set_cursor(&self, cursor: CursorIcon) {
        self.inner.set_cursor(map_cursor(cursor));
    }

    fn set_ime_allowed(&self, allowed: bool) {
        self.inner.set_ime_allowed(allowed);
    }

    fn set_ime_cursor_area(&self, x: f64, y: f64, w: f64, h: f64) {
        self.inner.set_ime_cursor_area(
            WinitPhysPos::new(x, y),
            WinitPhysSize::new(w.max(1.0), h.max(1.0)),
        );
    }

    fn set_text_input_document(&self, doc: Option<&TextDocument>) {
        #[cfg(windows)]
        {
            let hwnd = tsf_hwnd(&self.inner);
            let win = Arc::clone(&self.inner);
            TSF_MANAGER.with(|cell| {
                let mut slot = cell.borrow_mut();
                // text_input が実際に focus した (doc=Some) 初回にだけ TSF を init する
                // (text field を一切持たないセッションでは COM apartment を起こさない)。
                if matches!(*slot, TsfSlot::Untried) && doc.is_some() {
                    let init = hwnd.map(|h| {
                        // IME が store を編集したら redraw を要求し、次フレームで drain させる。
                        let redraw: std::rc::Rc<dyn Fn()> =
                            std::rc::Rc::new(move || win.request_redraw());
                        daw_ui_platform::tsf::TsfManager::new(h, redraw)
                    });
                    *slot = match init {
                        Some(Ok(mgr)) => TsfSlot::Active(mgr),
                        Some(Err(e)) => {
                            // TSF が使えない環境 (apartment 衝突等) は winit IMM に degrade。
                            tracing::warn!(error = %e, "TSF init failed; falling back to winit IMM");
                            TsfSlot::Failed
                        }
                        None => TsfSlot::Failed,
                    };
                }
                if let TsfSlot::Active(mgr) = &*slot {
                    mgr.set_document(doc);
                }
            });
        }
        #[cfg(not(windows))]
        let _ = doc;
    }

    fn take_ime_text_edits(&self) -> Vec<ImeTextEdit> {
        #[cfg(windows)]
        {
            TSF_MANAGER.with(|cell| {
                if let TsfSlot::Active(mgr) = &*cell.borrow() {
                    mgr.take_ime_edits()
                } else {
                    Vec::new()
                }
            })
        }
        #[cfg(not(windows))]
        Vec::new()
    }

    fn set_title(&self, title: &str) {
        self.inner.set_title(title);
    }
}

/// winit `Window` から TSF 用の `windows` crate `HWND` を取り出す (Windows のみ)。
#[cfg(windows)]
fn tsf_hwnd(window: &Window) -> Option<windows::Win32::Foundation::HWND> {
    use raw_window_handle::RawWindowHandle;
    let handle = window.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Some(windows::Win32::Foundation::HWND(
            h.hwnd.get() as *mut core::ffi::c_void,
        )),
        _ => None,
    }
}

fn map_cursor(c: CursorIcon) -> WinitCursor {
    match c {
        CursorIcon::Default | CursorIcon::Hidden => WinitCursor::Default,
        CursorIcon::Pointer => WinitCursor::Pointer,
        CursorIcon::Text => WinitCursor::Text,
        CursorIcon::Crosshair => WinitCursor::Crosshair,
        CursorIcon::EwResize => WinitCursor::EwResize,
        CursorIcon::NsResize => WinitCursor::NsResize,
        CursorIcon::Move => WinitCursor::Move,
    }
}
