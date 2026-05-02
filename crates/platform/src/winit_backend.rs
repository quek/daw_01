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
    AppEvent, ElementState, KeyEvent, Modifiers, MouseButton, PhysicalKey, PhysicalPosition,
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

/// 1 フレーム分の駆動 (`AppEvent::Tick` + `on_render`) を行う。
///
/// winit の `RedrawRequested` ハンドラから直接呼ぶほか、baseview など別の
/// `WindowBackend` 実装からも呼べる (host 駆動の frame push に対応するため)。
///
/// 戻り値: `host.on_render()` が `true` を返した場合は再描画継続のリクエスト。
/// 呼び出し側は必要に応じて `WindowBackend::request_redraw` を呼ぶ。
pub fn drive_one_frame<H: AppHost>(host: &mut H, last_tick: &mut Instant) -> bool {
    let now = Instant::now();
    let dt = now.duration_since(*last_tick);
    *last_tick = now;
    host.on_event(AppEvent::Tick(dt));
    host.on_render()
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
            WindowEvent::ModifiersChanged(mods) => {
                let st = mods.state();
                host.on_event(AppEvent::ModifiersChanged(Modifiers {
                    ctrl: st.control_key(),
                    shift: st.shift_key(),
                    alt: st.alt_key(),
                    logo: st.super_key(),
                }));
            }
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
                let request_more = drive_one_frame(host, &mut self.last_tick);
                if request_more
                    && let Some(w) = self.window.as_ref()
                {
                    w.request_redraw();
                }
            }
            // M8 Phase 32: OS file drag&drop。winit が file を 1 つずつ通知する。
            // position は別途送られないため、`InputAccumulator` が直近の cur_pos を補う。
            WindowEvent::HoveredFile(path) => {
                host.on_event(AppEvent::FileHovered(path));
            }
            WindowEvent::HoveredFileCancelled => {
                host.on_event(AppEvent::FileHoverCancelled);
            }
            WindowEvent::DroppedFile(path) => {
                host.on_event(AppEvent::FileDropped(path));
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

#[allow(clippy::too_many_lines)]
fn map_phys_key(k: WinitPhysKey) -> PhysicalKey {
    match k {
        WinitPhysKey::Code(KeyCode::Escape) => PhysicalKey::Escape,
        WinitPhysKey::Code(KeyCode::Enter) => PhysicalKey::Enter,
        WinitPhysKey::Code(KeyCode::Space) => PhysicalKey::Space,
        WinitPhysKey::Code(KeyCode::Tab) => PhysicalKey::Tab,
        WinitPhysKey::Code(KeyCode::Backspace) => PhysicalKey::Backspace,
        WinitPhysKey::Code(KeyCode::Delete) => PhysicalKey::Delete,
        WinitPhysKey::Code(KeyCode::Home) => PhysicalKey::Home,
        WinitPhysKey::Code(KeyCode::End) => PhysicalKey::End,
        WinitPhysKey::Code(KeyCode::PageUp) => PhysicalKey::PageUp,
        WinitPhysKey::Code(KeyCode::PageDown) => PhysicalKey::PageDown,
        WinitPhysKey::Code(KeyCode::Insert) => PhysicalKey::Insert,
        WinitPhysKey::Code(KeyCode::ArrowUp) => PhysicalKey::ArrowUp,
        WinitPhysKey::Code(KeyCode::ArrowDown) => PhysicalKey::ArrowDown,
        WinitPhysKey::Code(KeyCode::ArrowLeft) => PhysicalKey::ArrowLeft,
        WinitPhysKey::Code(KeyCode::ArrowRight) => PhysicalKey::ArrowRight,
        // Latin alphabet (大文字に正規化)
        WinitPhysKey::Code(KeyCode::KeyA) => PhysicalKey::Char('A'),
        WinitPhysKey::Code(KeyCode::KeyB) => PhysicalKey::Char('B'),
        WinitPhysKey::Code(KeyCode::KeyC) => PhysicalKey::Char('C'),
        WinitPhysKey::Code(KeyCode::KeyD) => PhysicalKey::Char('D'),
        WinitPhysKey::Code(KeyCode::KeyE) => PhysicalKey::Char('E'),
        WinitPhysKey::Code(KeyCode::KeyF) => PhysicalKey::Char('F'),
        WinitPhysKey::Code(KeyCode::KeyG) => PhysicalKey::Char('G'),
        WinitPhysKey::Code(KeyCode::KeyH) => PhysicalKey::Char('H'),
        WinitPhysKey::Code(KeyCode::KeyI) => PhysicalKey::Char('I'),
        WinitPhysKey::Code(KeyCode::KeyJ) => PhysicalKey::Char('J'),
        WinitPhysKey::Code(KeyCode::KeyK) => PhysicalKey::Char('K'),
        WinitPhysKey::Code(KeyCode::KeyL) => PhysicalKey::Char('L'),
        WinitPhysKey::Code(KeyCode::KeyM) => PhysicalKey::Char('M'),
        WinitPhysKey::Code(KeyCode::KeyN) => PhysicalKey::Char('N'),
        WinitPhysKey::Code(KeyCode::KeyO) => PhysicalKey::Char('O'),
        WinitPhysKey::Code(KeyCode::KeyP) => PhysicalKey::Char('P'),
        WinitPhysKey::Code(KeyCode::KeyQ) => PhysicalKey::Char('Q'),
        WinitPhysKey::Code(KeyCode::KeyR) => PhysicalKey::Char('R'),
        WinitPhysKey::Code(KeyCode::KeyS) => PhysicalKey::Char('S'),
        WinitPhysKey::Code(KeyCode::KeyT) => PhysicalKey::Char('T'),
        WinitPhysKey::Code(KeyCode::KeyU) => PhysicalKey::Char('U'),
        WinitPhysKey::Code(KeyCode::KeyV) => PhysicalKey::Char('V'),
        WinitPhysKey::Code(KeyCode::KeyW) => PhysicalKey::Char('W'),
        WinitPhysKey::Code(KeyCode::KeyX) => PhysicalKey::Char('X'),
        WinitPhysKey::Code(KeyCode::KeyY) => PhysicalKey::Char('Y'),
        WinitPhysKey::Code(KeyCode::KeyZ) => PhysicalKey::Char('Z'),
        // 数字キー (上段、Digit0..Digit9)
        WinitPhysKey::Code(KeyCode::Digit0) => PhysicalKey::Digit(0),
        WinitPhysKey::Code(KeyCode::Digit1) => PhysicalKey::Digit(1),
        WinitPhysKey::Code(KeyCode::Digit2) => PhysicalKey::Digit(2),
        WinitPhysKey::Code(KeyCode::Digit3) => PhysicalKey::Digit(3),
        WinitPhysKey::Code(KeyCode::Digit4) => PhysicalKey::Digit(4),
        WinitPhysKey::Code(KeyCode::Digit5) => PhysicalKey::Digit(5),
        WinitPhysKey::Code(KeyCode::Digit6) => PhysicalKey::Digit(6),
        WinitPhysKey::Code(KeyCode::Digit7) => PhysicalKey::Digit(7),
        WinitPhysKey::Code(KeyCode::Digit8) => PhysicalKey::Digit(8),
        WinitPhysKey::Code(KeyCode::Digit9) => PhysicalKey::Digit(9),
        // ファンクションキー
        WinitPhysKey::Code(KeyCode::F1) => PhysicalKey::F(1),
        WinitPhysKey::Code(KeyCode::F2) => PhysicalKey::F(2),
        WinitPhysKey::Code(KeyCode::F3) => PhysicalKey::F(3),
        WinitPhysKey::Code(KeyCode::F4) => PhysicalKey::F(4),
        WinitPhysKey::Code(KeyCode::F5) => PhysicalKey::F(5),
        WinitPhysKey::Code(KeyCode::F6) => PhysicalKey::F(6),
        WinitPhysKey::Code(KeyCode::F7) => PhysicalKey::F(7),
        WinitPhysKey::Code(KeyCode::F8) => PhysicalKey::F(8),
        WinitPhysKey::Code(KeyCode::F9) => PhysicalKey::F(9),
        WinitPhysKey::Code(KeyCode::F10) => PhysicalKey::F(10),
        WinitPhysKey::Code(KeyCode::F11) => PhysicalKey::F(11),
        WinitPhysKey::Code(KeyCode::F12) => PhysicalKey::F(12),
        WinitPhysKey::Code(KeyCode::F13) => PhysicalKey::F(13),
        WinitPhysKey::Code(KeyCode::F14) => PhysicalKey::F(14),
        WinitPhysKey::Code(KeyCode::F15) => PhysicalKey::F(15),
        WinitPhysKey::Code(KeyCode::F16) => PhysicalKey::F(16),
        WinitPhysKey::Code(KeyCode::F17) => PhysicalKey::F(17),
        WinitPhysKey::Code(KeyCode::F18) => PhysicalKey::F(18),
        WinitPhysKey::Code(KeyCode::F19) => PhysicalKey::F(19),
        WinitPhysKey::Code(KeyCode::F20) => PhysicalKey::F(20),
        WinitPhysKey::Code(KeyCode::F21) => PhysicalKey::F(21),
        WinitPhysKey::Code(KeyCode::F22) => PhysicalKey::F(22),
        WinitPhysKey::Code(KeyCode::F23) => PhysicalKey::F(23),
        WinitPhysKey::Code(KeyCode::F24) => PhysicalKey::F(24),
        // M9 P0-1: ASCII 印字可能記号 11 種 (US 配列、shift なし時の char)
        WinitPhysKey::Code(KeyCode::Slash) => PhysicalKey::Char('/'),
        WinitPhysKey::Code(KeyCode::Semicolon) => PhysicalKey::Char(';'),
        WinitPhysKey::Code(KeyCode::Comma) => PhysicalKey::Char(','),
        WinitPhysKey::Code(KeyCode::Period) => PhysicalKey::Char('.'),
        WinitPhysKey::Code(KeyCode::Minus) => PhysicalKey::Char('-'),
        WinitPhysKey::Code(KeyCode::Equal) => PhysicalKey::Char('='),
        WinitPhysKey::Code(KeyCode::BracketLeft) => PhysicalKey::Char('['),
        WinitPhysKey::Code(KeyCode::BracketRight) => PhysicalKey::Char(']'),
        WinitPhysKey::Code(KeyCode::Backslash) => PhysicalKey::Char('\\'),
        WinitPhysKey::Code(KeyCode::Quote) => PhysicalKey::Char('\''),
        WinitPhysKey::Code(KeyCode::Backquote) => PhysicalKey::Char('`'),
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
    if unsafe { GetCursorPos(&raw mut pt) } == 0 {
        return None;
    }
    if unsafe { ScreenToClient(hwnd, &raw mut pt) } == 0 {
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
        // Hidden は visibility 制御だが winit の API は別経路なので Default で代用 (M1)
        CursorIcon::Default | CursorIcon::Hidden => WinitCursor::Default,
        CursorIcon::Pointer => WinitCursor::Pointer,
        CursorIcon::Text => WinitCursor::Text,
        CursorIcon::Crosshair => WinitCursor::Crosshair,
        CursorIcon::EwResize => WinitCursor::EwResize,
        CursorIcon::NsResize => WinitCursor::NsResize,
        CursorIcon::Move => WinitCursor::Move,
    }
}
