//! winit::ApplicationHandler の実装。`AppEvent` を user event として走らせる。
//!
//! 役割:
//! - WindowAttributes でメインウィンドウを作る (resumed フェーズ)
//! - WindowEvent を gui_01 の AppEvent (`daw_ui_platform::AppEvent`) に変換し、
//!   `App::on_event` 相当の処理 (= InputAccumulator への ingest と redraw 要求) を行う
//! - background スレッドからの `AppEvent` (= `crate::app::AppEvent`) を user_event で受け、
//!   AppData::handle_event に流して redraw 要求
//!
//! gui_01 の `winit_backend::run_app` を直接使わない理由: あちらは
//! `EventLoop::<()>::new()` で user event を持たないため、background スレッド
//! からのイベント注入ができない。daw_gui は IPC bridge / autosave / midi / playhead
//! poll 等で多数の background スレッドからイベントを送るため独自に runner を持つ。

use std::sync::Arc;
use std::time::Instant;

use daw_ui_core::{InputAccumulator, UiHost};
use daw_ui_platform::{
    AppEvent as PlatformEvent, ElementState, KeyEvent, Modifiers, MouseButton, PhysicalKey,
    PhysicalPosition, PhysicalSize, ScrollDelta, WindowBackend,
};
use winit::keyboard::KeyCode as WinitKeyCode;
use daw_ui_renderer::{Renderer, Scene};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition as WinitPhysPos;
use winit::event::{
    ElementState as WinitElemState, Ime as WinitIme, MouseButton as WinitMouseBtn,
    MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey as WinitPhysKey};
use winit::window::{WindowAttributes, WindowId};

use crate::app::{AppData, AppEvent};
use crate::view::window::DawGuiWindow;

/// アプリ初期化で渡すパラメータ。`run` の中で main window を作って AppData を組み立てる。
pub struct RunnerInit {
    pub window_attrs: WindowAttributes,
    /// AppData をビルドする closure。引数:
    ///   - `EventLoopProxy<AppEvent>` を受けて AppData の内部に保持してもらう
    pub build_app: Box<dyn FnOnce(EventLoopProxy<AppEvent>) -> AppData + Send>,
}

pub fn run(init: RunnerInit) -> Result<(), winit::error::EventLoopError> {
    let event_loop = winit::event_loop::EventLoop::<AppEvent>::with_user_event().build()?;
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    let mut runner = Runner {
        attrs: Some(init.window_attrs),
        build_app: Some(init.build_app),
        proxy,
        state: None,
        last_tick: Instant::now(),
    };
    event_loop.run_app(&mut runner)
}

struct RunnerState {
    window: Arc<DawGuiWindow>,
    renderer: Renderer<DawGuiWindow>,
    ui: UiHost<AppData>,
    app: AppData,
    scene: Scene,
    input: InputAccumulator,
    /// 直前フレームで IME を有効化していたか (差分管理)。
    ime_enabled: bool,
    /// 直近に OS ウィンドウへ反映したタイトル。AppData の状態と差分を見て set_title。
    last_title: String,
    /// 現在の修飾キー状態 (ショートカット dispatch 用)。
    modifiers: Modifiers,
}

struct Runner {
    attrs: Option<WindowAttributes>,
    build_app: Option<Box<dyn FnOnce(EventLoopProxy<AppEvent>) -> AppData + Send>>,
    proxy: EventLoopProxy<AppEvent>,
    state: Option<RunnerState>,
    last_tick: Instant,
}

impl Runner {
    fn dispatch_platform_event(&mut self, ev: PlatformEvent) {
        let Some(state) = self.state.as_mut() else { return };
        state.input.ingest(&ev);
        match ev {
            PlatformEvent::Resized(size) => {
                state.renderer.resize(size);
                state.window.request_redraw();
            }
            PlatformEvent::PointerMoved(_)
            | PlatformEvent::PointerInput { .. }
            | PlatformEvent::Scroll(_)
            | PlatformEvent::Keyboard(_)
            | PlatformEvent::ImePreedit { .. }
            | PlatformEvent::ImeCommit(_)
            | PlatformEvent::ModifiersChanged(_) => {
                state.window.request_redraw();
            }
            _ => {}
        }
    }
}

impl ApplicationHandler<AppEvent> for Runner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = self.attrs.take().expect("WindowAttributes 既に消費");
        let window = event_loop
            .create_window(attrs)
            .expect("create_window 失敗");
        let window = Arc::new(window);
        let dwin = Arc::new(DawGuiWindow::new(window));
        let renderer = Renderer::new(dwin.clone()).expect("Renderer::new");

        let ui_window = dwin.clone();
        let ui = UiHost::<AppData>::new(move || ui_window.request_redraw());

        let build_app = self.build_app.take().expect("build_app 既に消費");
        let app = build_app(self.proxy.clone());

        self.state = Some(RunnerState {
            window: dwin,
            renderer,
            ui,
            app,
            scene: Scene::new(),
            input: InputAccumulator::new(),
            ime_enabled: false,
            last_title: "daw_01".to_string(),
            modifiers: Modifiers::default(),
        });
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                tracing::info!("window close requested");
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                self.dispatch_platform_event(PlatformEvent::Resized(PhysicalSize {
                    width: size.width,
                    height: size.height,
                }));
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.dispatch_platform_event(PlatformEvent::ScaleFactorChanged(scale_factor));
            }
            WindowEvent::CursorMoved { position, .. } => {
                let WinitPhysPos { x, y } = position;
                self.dispatch_platform_event(PlatformEvent::PointerMoved(PhysicalPosition {
                    x,
                    y,
                }));
            }
            WindowEvent::CursorEntered { .. } => {
                self.dispatch_platform_event(PlatformEvent::PointerEntered);
            }
            WindowEvent::CursorLeft { .. } => {
                self.dispatch_platform_event(PlatformEvent::PointerLeft);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(rs) = self.state.as_ref()
                    && let Some(pos) = query_cursor_pos_in_window(rs.window.inner())
                {
                    self.dispatch_platform_event(PlatformEvent::PointerMoved(pos));
                }
                self.dispatch_platform_event(PlatformEvent::PointerInput {
                    button: map_button(button),
                    state: map_state(state),
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let d = match delta {
                    MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines { x, y },
                    MouseScrollDelta::PixelDelta(p) => ScrollDelta::Pixels { x: p.x, y: p.y },
                };
                self.dispatch_platform_event(PlatformEvent::Scroll(d));
            }
            WindowEvent::Focused(f) => {
                self.dispatch_platform_event(PlatformEvent::Focus(f));
            }
            WindowEvent::ModifiersChanged(mods) => {
                let st = mods.state();
                let m = Modifiers {
                    ctrl: st.control_key(),
                    shift: st.shift_key(),
                    alt: st.alt_key(),
                    logo: st.super_key(),
                };
                if let Some(rs) = self.state.as_mut() {
                    rs.modifiers = m;
                }
                self.dispatch_platform_event(PlatformEvent::ModifiersChanged(m));
            }
            WindowEvent::Ime(ime) => match ime {
                WinitIme::Preedit(text, cursor) => {
                    self.dispatch_platform_event(PlatformEvent::ImePreedit { text, cursor });
                }
                WinitIme::Commit(text) => {
                    self.dispatch_platform_event(PlatformEvent::ImeCommit(text));
                }
                WinitIme::Enabled | WinitIme::Disabled => {}
            },
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = matches!(event.state, WinitElemState::Pressed);
                let key_code = match event.physical_key {
                    WinitPhysKey::Code(c) => Some(c),
                    _ => None,
                };
                // Global shortcut: 押下 + テキスト入力に focus が無いときだけ
                // AppEvent を発火する。focus がある (text_input 等) ときは widget に
                // キー入力を任せる。
                if pressed
                    && let Some(rs) = self.state.as_mut()
                    && rs.ui.focused_widget().is_none()
                    && let Some(code) = key_code
                {
                    let dispatched = dispatch_shortcut(code, rs.modifiers, &mut rs.app);
                    if dispatched {
                        rs.window.request_redraw();
                        return;
                    }
                }
                let key = KeyEvent {
                    state: map_state(event.state),
                    text: event.text.map(|s| s.to_string()),
                    physical_key: map_phys_key(event.physical_key),
                };
                self.dispatch_platform_event(PlatformEvent::Keyboard(key));
            }
            WindowEvent::RedrawRequested => {
                let request_more = self.render_frame();
                if request_more
                    && let Some(rs) = self.state.as_ref()
                {
                    rs.window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: AppEvent) {
        let Some(state) = self.state.as_mut() else { return };
        state.app.handle_event(event);
        state.window.request_redraw();
    }
}

impl Runner {
    fn render_frame(&mut self) -> bool {
        let Some(state) = self.state.as_mut() else { return false };
        let now = Instant::now();
        let _dt = now.duration_since(self.last_tick);
        self.last_tick = now;

        let screen = state.renderer.size();
        state.scene.clear();
        let input = state.input.take_input();

        state.ui.frame(
            &mut state.app,
            &mut state.scene,
            screen,
            input,
            |app, ui| {
                crate::view::root::build_root(app, ui, screen);
            },
        );

        if let Err(e) = state.renderer.render(&state.scene) {
            tracing::error!(error = ?e, "render error");
        }

        // タイトル差分反映 (現状は常に "daw_01"; 将来 dirty marker を入れる)。
        let new_title = "daw_01".to_string();
        if new_title != state.last_title {
            state.window.set_title(&new_title);
            state.last_title = new_title;
        }

        // IME 差分反映。
        match (state.ime_enabled, state.ui.ime_request()) {
            (false, Some(area)) => {
                state.window.set_ime_allowed(true);
                state.window.set_ime_cursor_area(
                    f64::from(area.x),
                    f64::from(area.y),
                    f64::from(area.w),
                    f64::from(area.h),
                );
                state.ime_enabled = true;
            }
            (true, Some(area)) => {
                state.window.set_ime_cursor_area(
                    f64::from(area.x),
                    f64::from(area.y),
                    f64::from(area.w),
                    f64::from(area.h),
                );
            }
            (true, None) => {
                state.window.set_ime_allowed(false);
                state.ime_enabled = false;
            }
            (false, None) => {}
        }

        state.app.is_playing
    }
}

fn map_button(b: WinitMouseBtn) -> MouseButton {
    match b {
        WinitMouseBtn::Left => MouseButton::Left,
        WinitMouseBtn::Right => MouseButton::Right,
        WinitMouseBtn::Middle => MouseButton::Middle,
        WinitMouseBtn::Other(n) => MouseButton::Other(n),
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

#[cfg(target_os = "windows")]
fn query_cursor_pos_in_window(window: &winit::window::Window) -> Option<PhysicalPosition> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::ffi::c_void;
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let handle = window.window_handle().ok()?;
    let hwnd = match handle.as_raw() {
        RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut c_void),
        _ => return None,
    };
    let mut pt = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&raw mut pt) }.is_err() {
        return None;
    }
    if unsafe { ScreenToClient(hwnd, &raw mut pt) }.as_bool() == false {
        return None;
    }
    Some(PhysicalPosition {
        x: f64::from(pt.x),
        y: f64::from(pt.y),
    })
}

#[cfg(not(target_os = "windows"))]
fn query_cursor_pos_in_window(_window: &winit::window::Window) -> Option<PhysicalPosition> {
    None
}

/// 修飾キー + 物理キーから AppEvent を引いて即時 dispatch。`true` を返したら
/// その KeyEvent はショートカットに消費されたものとして widget へは流さない。
fn dispatch_shortcut(code: WinitKeyCode, m: Modifiers, app: &mut AppData) -> bool {
    use WinitKeyCode as K;

    let only_ctrl = m.ctrl && !m.shift && !m.alt && !m.logo;
    let ctrl_shift = m.ctrl && m.shift && !m.alt && !m.logo;
    let only_shift = m.shift && !m.ctrl && !m.alt && !m.logo;
    let no_mod = !m.ctrl && !m.shift && !m.alt && !m.logo;

    let event = match code {
        // ----- File -----
        K::KeyN if only_ctrl => AppEvent::New,
        K::KeyO if only_ctrl => AppEvent::Open,
        K::KeyS if only_ctrl => AppEvent::Save,
        K::KeyS if ctrl_shift => AppEvent::SaveAs,
        K::KeyE if only_ctrl => AppEvent::ExportWav,
        // ----- Transport -----
        K::Space if no_mod => AppEvent::PlayToggle,
        K::KeyP if no_mod => AppEvent::ToggleLoop,
        K::KeyV if no_mod => AppEvent::SynthesizeVocal,
        // ----- Edit -----
        K::KeyZ if only_ctrl => AppEvent::Undo,
        K::KeyZ if ctrl_shift => AppEvent::Redo,
        K::KeyY if only_ctrl => AppEvent::Redo,
        K::KeyC if only_ctrl => AppEvent::CopySelectedNotes,
        K::KeyV if only_ctrl => AppEvent::PasteNotes,
        // ----- Selection -----
        K::Delete if no_mod => {
            // Both events are no-op when their target is empty so the order is
            // safe (notes are prioritised, then fall through to clip).
            app.handle_event(AppEvent::DeleteSelectedNotes);
            app.handle_event(AppEvent::DeleteSelectedClip);
            return true;
        }
        // ----- Help -----
        K::F1 if no_mod => AppEvent::ToggleHelp,
        K::Slash if only_shift => AppEvent::ToggleHelp,
        K::Escape if no_mod => AppEvent::CloseHelp,
        _ => return false,
    };
    app.handle_event(event);
    true
}
