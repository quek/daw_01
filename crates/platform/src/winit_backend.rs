//! winit を `WindowBackend` / イベント駆動に橋渡しするバックエンド。

use std::sync::Arc;
use std::time::Instant;

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition as WinitPhysPos, PhysicalSize as WinitPhysSize};
use winit::event::{ElementState as WinitElemState, Ime as WinitIme, MouseButton as WinitMouseBtn, MouseScrollDelta, WindowEvent};
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

    fn set_ime_allowed(&self, allowed: bool) {
        self.inner.set_ime_allowed(allowed);
    }

    fn set_ime_cursor_area(&self, x: f64, y: f64, w: f64, h: f64) {
        use winit::dpi::PhysicalPosition as WinitPos;
        use winit::dpi::PhysicalSize as WinitSize;
        // 物理ピクセルで指定 (winit が IME に渡してくれる)。
        self.inner.set_ime_cursor_area(
            WinitPos::new(x, y),
            WinitSize::new(w.max(1.0), h.max(1.0)),
        );
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
                // Windows 対策: フォーカス取得を伴うクリック (例: Alt-Tab で戻った直後)
                // では `WM_MOUSEMOVE` が発火せず `CursorMoved` が来ないため、OS に
                // カーソル位置を問い合わせて synthetic `PointerMoved` を先に流す。
                // これがないとクリック位置が不明 (`cur_pos = None`) のまま widget に
                // 流れて、ボタン等の hit-test が空振りする。
                if let Some(window) = self.window.as_ref()
                    && let Some(pos) = query_cursor_pos_in_window(&window.inner)
                {
                    host.on_event(AppEvent::PointerMoved(pos));
                }
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
            WindowEvent::Ime(ime) => match ime {
                WinitIme::Preedit(text, cursor) => {
                    host.on_event(AppEvent::ImePreedit { text, cursor });
                }
                WinitIme::Commit(text) => {
                    host.on_event(AppEvent::ImeCommit(text));
                }
                // Enabled / Disabled は set_ime_allowed の応答でしかなく、ライブラリでは
                // 状態遷移を別途管理しているのでここで利用者に通知する必要はない。
                WinitIme::Enabled | WinitIme::Disabled => {}
            },
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

/// OS にカーソル位置を問い合わせて、ウィンドウ局所座標 (物理ピクセル) で返す。
///
/// **Windows のみ実装**: フォーカス取得を伴うクリックでは `WM_MOUSEMOVE` が
/// 来ない場合があり、winit の `cur_pos` が `None` のまま `MouseInput` が届くため、
/// `MouseInput` 直前に `GetCursorPos` で位置を補う必要がある。
/// 他プラットフォームは `None` を返す (該当の問題が同じ形で出るかは未検証)。
#[cfg(target_os = "windows")]
fn query_cursor_pos_in_window(window: &Window) -> Option<PhysicalPosition> {
    use std::ffi::c_void;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{HWND, POINT};
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let handle = window.window_handle().ok()?;
    let hwnd: HWND = match handle.as_raw() {
        RawWindowHandle::Win32(h) => h.hwnd.get() as *mut c_void,
        _ => return None,
    };
    let mut pt = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut pt) } == 0 {
        return None;
    }
    if unsafe { ScreenToClient(hwnd, &mut pt) } == 0 {
        return None;
    }
    Some(PhysicalPosition { x: f64::from(pt.x), y: f64::from(pt.y) })
}

#[cfg(not(target_os = "windows"))]
fn query_cursor_pos_in_window(_window: &Window) -> Option<PhysicalPosition> {
    None
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
