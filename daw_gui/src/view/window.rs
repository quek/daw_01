//! `daw-ui-platform::WindowBackend` を満たす winit window ラッパー。
//!
//! gui_01 (`daw-ui-platform::winit_backend::WinitWindow`) は内部 `new` が pub
//! でないため外部から構築できない。daw_gui は EventLoopProxy<AppEvent> を
//! 載せた独自イベントループを駆動する都合で WinitWindow は使えず、ここで
//! 同等の Wrapper を定義する。

use std::sync::Arc;

use daw_ui_platform::{CursorIcon, PhysicalSize, WindowBackend};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use winit::dpi::{PhysicalPosition as WinitPhysPos, PhysicalSize as WinitPhysSize};
use winit::window::{CursorIcon as WinitCursor, Window};

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

    fn set_title(&self, title: &str) {
        self.inner.set_title(title);
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
