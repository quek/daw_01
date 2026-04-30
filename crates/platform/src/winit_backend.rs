//! winit を `WindowBackend` / イベント駆動に橋渡しするバックエンド。

use std::sync::Arc;
use std::time::Instant;

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition as WinitPhysPos, PhysicalSize as WinitPhysSize};
use winit::event::{ElementState as WinitElemState, MouseButton as WinitMouseBtn, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey as WinitPhysKey};
use winit::window::{CursorIcon as WinitCursor, Window, WindowAttributes, WindowId};

use crate::event::{
    AppEvent, ElementState, KeyEvent, MouseButton, PhysicalKey, PhysicalPosition,
    PhysicalSize, ScrollDelta,
};
use crate::window::{AppHost, CursorIcon, WindowBackend};

/// winit の `Window` を `WindowBackend` 実装でラップしたもの。
#[derive(Clone)]
pub struct WinitWindow {
    inner: Arc<Window>,
}

impl WinitWindow {
    fn new(window: Arc<Window>) -> Self {
        Self { inner: window }
    }
}

impl HasWindowHandle for WinitWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.inner.window_handle()
    }
}

impl HasDisplayHandle for WinitWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.inner.display_handle()
    }
}

impl WindowBackend for WinitWindow {
    fn inner_size(&self) -> PhysicalSize {
        let s: WinitPhysSize<u32> = self.inner.inner_size();
        PhysicalSize { width: s.width, height: s.height }
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

    fn set_ime_position(&self, _x: f64, _y: f64) {
        // M1 では no-op。後で Cursor area を winit::Window::set_ime_cursor_area で設定する。
    }

    fn set_title(&self, title: &str) {
        self.inner.set_title(title);
    }
}

/// アプリ起動エントリ — `AppHost` を渡して winit イベントループを走らせる。
///
/// `factory` は `WindowBackend` を受け取って `AppHost` を組み立てるクロージャ。
/// これによりアプリ側は winit を直接知らずに済む。
///
/// # Errors
/// イベントループ初期化失敗時。
pub fn run_app<H, F>(window_attrs: WindowAttributes, factory: F) -> Result<(), winit::error::EventLoopError>
where
    H: AppHost + 'static,
    F: FnOnce(WinitWindow) -> H + 'static,
{
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);

    let mut runner = WinitRunner::<H, F> {
        attrs: Some(window_attrs),
        factory: Some(factory),
        host: None,
        window: None,
        last_tick: Instant::now(),
    };
    event_loop.run_app(&mut runner)
}

struct WinitRunner<H: AppHost, F: FnOnce(WinitWindow) -> H> {
    attrs: Option<WindowAttributes>,
    factory: Option<F>,
    host: Option<H>,
    window: Option<WinitWindow>,
    last_tick: Instant,
}

impl<H: AppHost, F: FnOnce(WinitWindow) -> H> ApplicationHandler for WinitRunner<H, F> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = self.attrs.take().expect("WindowAttributes 既に消費");
        let window = event_loop.create_window(attrs).expect("create_window 失敗");
        let win = WinitWindow::new(Arc::new(window));
        let factory = self.factory.take().expect("factory 既に消費");
        let host = factory(win.clone());
        self.window = Some(win);
        self.host = Some(host);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(host) = self.host.as_mut() else { return };

        match event {
            WindowEvent::CloseRequested => {
                host.on_event(AppEvent::CloseRequested);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                host.on_event(AppEvent::Resized(PhysicalSize { width: size.width, height: size.height }));
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                host.on_event(AppEvent::ScaleFactorChanged(scale_factor));
            }
            WindowEvent::CursorMoved { position, .. } => {
                let WinitPhysPos { x, y } = position;
                host.on_event(AppEvent::PointerMoved(PhysicalPosition { x, y }));
            }
            WindowEvent::CursorEntered { .. } => host.on_event(AppEvent::PointerEntered),
            WindowEvent::CursorLeft { .. } => host.on_event(AppEvent::PointerLeft),
            WindowEvent::MouseInput { state, button, .. } => {
                host.on_event(AppEvent::PointerInput {
                    button: map_button(button),
                    state: map_state(state),
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let d = match delta {
                    MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines { x, y },
                    MouseScrollDelta::PixelDelta(p) => ScrollDelta::Pixels { x: p.x, y: p.y },
                };
                host.on_event(AppEvent::Scroll(d));
            }
            WindowEvent::Focused(f) => host.on_event(AppEvent::Focus(f)),
            WindowEvent::KeyboardInput { event, .. } => {
                let key = KeyEvent {
                    state: map_state(event.state),
                    text: event.text.map(|s| s.to_string()),
                    physical_key: map_phys_key(event.physical_key),
                };
                host.on_event(AppEvent::Keyboard(key));
            }
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt = now.duration_since(self.last_tick);
                self.last_tick = now;
                host.on_event(AppEvent::Tick(dt));
                let request_more = host.on_render();
                if request_more {
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // 連続描画が必要ならここで request_redraw する設計だが、
        // M1 では event-driven 再描画 (RedrawRequested) のみで十分。
    }
}

fn map_button(b: WinitMouseBtn) -> MouseButton {
    match b {
        WinitMouseBtn::Left => MouseButton::Left,
        WinitMouseBtn::Right => MouseButton::Right,
        WinitMouseBtn::Middle => MouseButton::Middle,
        WinitMouseBtn::Other(n) => MouseButton::Other(n),
        // Back / Forward は Other に畳む (M1)
        _ => MouseButton::Other(0xffff),
    }
}

fn map_state(s: WinitElemState) -> ElementState {
    match s {
        WinitElemState::Pressed => ElementState::Pressed,
        WinitElemState::Released => ElementState::Released,
    }
}

fn map_phys_key(k: WinitPhysKey) -> PhysicalKey {
    match k {
        WinitPhysKey::Code(KeyCode::Escape) => PhysicalKey::Escape,
        WinitPhysKey::Code(KeyCode::Enter) => PhysicalKey::Enter,
        WinitPhysKey::Code(KeyCode::Space) => PhysicalKey::Space,
        WinitPhysKey::Code(KeyCode::Tab) => PhysicalKey::Tab,
        WinitPhysKey::Code(KeyCode::Backspace) => PhysicalKey::Backspace,
        WinitPhysKey::Code(KeyCode::ArrowUp) => PhysicalKey::ArrowUp,
        WinitPhysKey::Code(KeyCode::ArrowDown) => PhysicalKey::ArrowDown,
        WinitPhysKey::Code(KeyCode::ArrowLeft) => PhysicalKey::ArrowLeft,
        WinitPhysKey::Code(KeyCode::ArrowRight) => PhysicalKey::ArrowRight,
        WinitPhysKey::Code(c) => PhysicalKey::Other(c as u32),
        WinitPhysKey::Unidentified(_) => PhysicalKey::Other(0),
    }
}

fn map_cursor(c: CursorIcon) -> WinitCursor {
    match c {
        CursorIcon::Default => WinitCursor::Default,
        CursorIcon::Pointer => WinitCursor::Pointer,
        CursorIcon::Text => WinitCursor::Text,
        CursorIcon::Crosshair => WinitCursor::Crosshair,
        CursorIcon::EwResize => WinitCursor::EwResize,
        CursorIcon::NsResize => WinitCursor::NsResize,
        CursorIcon::Move => WinitCursor::Move,
        // Hidden は visibility 制御だが winit の API は別経路なので Default で代用 (M1)
        CursorIcon::Hidden => WinitCursor::Default,
    }
}
