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

use daw_ui_core::{ArboardClipboard, InputAccumulator, UiHost};
use daw_ui_platform::{
    AppEvent as PlatformEvent, ElementState, KeyEvent, Modifiers, MouseButton, PhysicalKey,
    PhysicalPosition, PhysicalSize, ScrollDelta, WindowBackend,
};
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
use crate::view::shortcuts::daw_shortcut_map;
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
    /// docs/plan_video.md P4: video preview window 用の第二 winit::Window
    /// および Renderer。 `AppData.preview_window_visible` が true の間
    /// だけ Some。 メインウィンドウとは独立して redraw / resize / close
    /// を受け付ける。 P5 で video frame、 P7 で composite texture を
    /// `scene` に push する経路がここに繋がる。
    preview: Option<crate::view::preview_window::PreviewWindowState>,
    /// docs/plan_video.md P5: per-source IMFSourceReader を抱える
    /// playback decoder。 process 起動時に作成、 各 render_frame で
    /// playhead を元に active video clip の current frame を decode
    /// → preview window の `frame_texture` に upload する。
    #[cfg(windows)]
    playback: crate::video_playback::VideoPlaybackEngine,
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

        // `with_window` で `set_cursor_request` callback を `WindowBackend::set_cursor`
        // に自動接続する。これが無いと widget 内の `Ui::set_cursor` 要求が OS まで
        // 届かず、ピアノロール / アレンジビューの hover / drag でカーソル形状が
        // 変わらない。
        let ui = UiHost::<AppData>::with_window(dwin.clone())
            .with_history_capacity(200)
            .with_shortcut_map(daw_shortcut_map())
            .with_clipboard(ArboardClipboard::new());

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
            preview: None,
            #[cfg(windows)]
            playback: crate::video_playback::VideoPlaybackEngine::new(),
        });
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // docs/plan_video.md P4: events from the preview window are
        // dispatched separately — we don't want main window input
        // accumulators or AppData edit-paths to receive pointer / key
        // events that happened over the preview surface.
        let is_preview = self
            .state
            .as_ref()
            .and_then(|s| s.preview.as_ref())
            .map(|p| p.window_id() == window_id)
            .unwrap_or(false);
        if is_preview {
            self.handle_preview_window_event(event);
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                tracing::info!("window close requested");
                if let Some(state) = self.state.as_ref() {
                    state.app.on_shutdown();
                }
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
            WindowEvent::HoveredFile(path) => {
                self.dispatch_platform_event(PlatformEvent::FileHovered(path));
            }
            WindowEvent::HoveredFileCancelled => {
                self.dispatch_platform_event(PlatformEvent::FileHoverCancelled);
            }
            WindowEvent::DroppedFile(path) => {
                self.dispatch_platform_event(PlatformEvent::FileDropped(path));
            }
            WindowEvent::ModifiersChanged(mods) => {
                let st = mods.state();
                let m = Modifiers {
                    ctrl: st.control_key(),
                    shift: st.shift_key(),
                    alt: st.alt_key(),
                    logo: st.super_key(),
                };
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
                // ShortcutMap への dispatch は library 側 (UiHost::frame) が処理し、
                // root.rs::build_root の末尾で `ui.take_shortcut(name)` で消費する。
                // ここでは PlatformEvent::Keyboard に変換するだけ。
                let key = KeyEvent {
                    state: map_state(event.state),
                    text: event.text.map(|s| s.to_string()),
                    physical_key: map_phys_key(event.physical_key),
                };
                self.dispatch_platform_event(PlatformEvent::Keyboard(key));
            }
            WindowEvent::RedrawRequested => {
                let request_more = self.render_frame(event_loop);
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
    fn render_frame(&mut self, event_loop: &ActiveEventLoop) -> bool {
        // docs/plan_video.md P4: sync the preview window lifecycle
        // with `AppData.preview_window_visible` BEFORE everything
        // else — creating the window requires an ActiveEventLoop which
        // is only available here, and destroying it has to happen
        // before we lock other parts of state.
        self.sync_preview_window(event_loop);

        let Some(state) = self.state.as_mut() else { return false };
        let now = Instant::now();
        let _dt = now.duration_since(self.last_tick);
        self.last_tick = now;

        // docs/plan_video.md P3.5: drain pending video thumbnail
        // uploads BEFORE this frame's `ui.frame()` so the arrangement
        // view can read the resulting `TextureHandle` immediately.
        // First frame after import shows the thumbnail with one
        // frame's latency (= imports landing during a frame are
        // queued for the next frame's drain).
        Self::drain_video_thumbnail_uploads(&mut state.app, &mut state.renderer);

        // P5: drive playback decode + upload into the preview
        // window's frame_texture. When the playhead lands inside a
        // video clip, decode the corresponding source_micros frame
        // and re-upload; otherwise clear so the placeholder shows.
        #[cfg(windows)]
        if state.preview.is_some() {
            Self::drive_preview_playback(state);
        }

        // P4 baseline / P5 hand-off: render the preview window each
        // frame. `render_placeholder` picks between the placeholder
        // text and the textured-quad path internally based on whether
        // `frame_texture` is Some.
        if let Some(preview) = state.preview.as_mut() {
            preview.render_placeholder();
            preview.window.request_redraw();
        }

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

    /// docs/plan_video.md P4: handle a WindowEvent dispatched against
    /// the preview window. Limited to lifecycle / display events —
    /// CloseRequested flips `AppData.preview_window_visible` to false
    /// so the next frame's lifecycle pass destroys the OS window;
    /// Resized synchronises the wgpu surface; RedrawRequested re-runs
    /// the placeholder (or, post P5/P7, the composited frame).
    fn handle_preview_window_event(&mut self, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                state.app.preview_window_visible = false;
                // Lifecycle pass on the next render_frame drops the
                // preview state; nothing else to do here.
            }
            WindowEvent::Resized(size) => {
                preview.resize(daw_ui_platform::PhysicalSize {
                    width: size.width,
                    height: size.height,
                });
            }
            WindowEvent::RedrawRequested => {
                preview.render_placeholder();
            }
            _ => {}
        }
    }

    /// docs/plan_video.md P4: keep the preview window in sync with
    /// `AppData.preview_window_visible`. Called once per frame from
    /// `render_frame`. Creating the window requires an
    /// `&ActiveEventLoop`, which is only available inside winit
    /// callbacks — `render_frame` is reached via `RedrawRequested`
    /// (a window_event with an active loop), so we pass the loop
    /// through.
    fn sync_preview_window(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let visible = state.app.preview_window_visible;
        match (visible, state.preview.is_some()) {
            (true, false) => {
                let initial_size = state.app.song.video_resolution;
                match crate::view::preview_window::PreviewWindowState::create(
                    event_loop,
                    initial_size,
                ) {
                    Ok(p) => {
                        // Immediately request a redraw so the placeholder
                        // appears without waiting for the next OS event.
                        p.window.request_redraw();
                        state.preview = Some(p);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "preview window create failed");
                        state.app.preview_window_visible = false;
                        state.app.status_message =
                            format!("Video preview の作成に失敗: {e}");
                    }
                }
            }
            (false, true) => {
                // Dropping `PreviewWindowState` releases the wgpu
                // Renderer first (struct field order) then the Arc
                // chain — winit closes the OS window when the last
                // Arc<Window> reference drops.
                state.preview = None;
            }
            _ => {}
        }
    }

    /// docs/plan_video.md P7: drive multi-clip playback decode +
    /// upload + composite for the preview window. Asks the engine
    /// for every active video clip at the playhead, decodes each one,
    /// uploads to per-source GPU textures, and hands the bottom→top
    /// composite list to the preview. When the playhead is outside
    /// every video clip, clears the composite list so the placeholder
    /// shows. Per-source decode failures log at warn level and skip
    /// just that layer — the rest of the composite still draws.
    #[cfg(windows)]
    fn drive_preview_playback(state: &mut RunnerState) {
        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        let Some(playhead_beat) = state.app.playhead_beat.map(f64::from) else {
            preview.set_composite_layers(Vec::new());
            return;
        };
        let active = crate::video_playback::VideoPlaybackEngine::active_sources_at(
            &state.app.song,
            playhead_beat,
        );
        if active.is_empty() {
            preview.set_composite_layers(Vec::new());
            return;
        }
        let project_dir = state
            .app
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
        let mut layers: Vec<crate::view::preview_window::CompositeLayer> =
            Vec::with_capacity(active.len());
        for frame_info in active {
            let Some(abs_path) = resolve_video_path(
                &state.app.song,
                frame_info.video_source_id,
                project_dir.as_deref(),
            ) else {
                tracing::warn!(
                    video_source_id = frame_info.video_source_id,
                    "video path unresolved (unsaved project + ProjectRelative?), \
                     skipping layer"
                );
                continue;
            };
            match state.playback.decode_at(
                frame_info.video_source_id,
                &abs_path,
                frame_info.source_micros,
            ) {
                Ok(frame) => {
                    let handle = preview.upload_frame(
                        frame_info.video_source_id,
                        frame.width,
                        frame.height,
                        &frame.rgba,
                    );
                    layers.push(crate::view::preview_window::CompositeLayer {
                        texture: handle,
                        width: frame.width,
                        height: frame.height,
                        alpha: frame_info.alpha,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        video_source_id = frame_info.video_source_id,
                        source_micros = frame_info.source_micros,
                        "preview decode failed, layer skipped"
                    );
                }
            }
        }
        preview.set_composite_layers(layers);
    }

    /// docs/plan_video.md P3.5: drain `AppData.pending_thumbnail_uploads`
    /// by creating GPU textures via the live `Renderer`. Each successful
    /// upload populates `video_texture_cache` for arrangement_view to
    /// read in the same frame (P3.6). The staged RGBA bytes are dropped
    /// from `video_thumbnail_rgba` once on GPU — no need to keep the
    /// CPU-side copy.
    fn drain_video_thumbnail_uploads(
        app: &mut AppData,
        renderer: &mut Renderer<DawGuiWindow>,
    ) {
        if app.pending_thumbnail_uploads.is_empty() {
            return;
        }
        let pending: Vec<_> = app.pending_thumbnail_uploads.drain(..).collect();
        for video_source_id in pending {
            // It's possible the source was unloaded between the
            // import and the next frame (= rapid undo path). Just
            // skip — the GPU is the source of truth and a missing
            // RGBA staging means there's nothing to upload.
            let Some((w, h, rgba)) = app.video_thumbnail_rgba.remove(&video_source_id)
            else {
                continue;
            };
            let handle = renderer.create_texture(w, h);
            renderer.upload_texture_rgba(handle, rgba.as_slice());
            app.video_texture_cache.insert(video_source_id, handle);
        }
    }
}

/// docs/plan_video.md P5: turn a `VideoSourcePath` into an on-disk
/// path the WMF reader can open. `Absolute` is direct; `ProjectRelative`
/// needs the saved project dir — unsaved projects with
/// `ProjectRelative` paths return None (= shouldn't happen since
/// `import_one_video` writes `Absolute` for unsaved projects, but be
/// defensive).
#[cfg(windows)]
fn resolve_video_path(
    song: &common::model::Song,
    video_source_id: common::model::VideoSourceId,
    project_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    let src = song.video_sources.get(&video_source_id)?;
    match &src.path {
        common::model::VideoSourcePath::Absolute(p) => Some(p.clone()),
        common::model::VideoSourcePath::ProjectRelative(rel) => {
            project_dir.map(|d| d.join(rel))
        }
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
        WinitPhysKey::Code(KeyCode::NumpadEnter) => PhysicalKey::NumpadEnter,
        WinitPhysKey::Code(KeyCode::Space) => PhysicalKey::Space,
        WinitPhysKey::Code(KeyCode::Tab) => PhysicalKey::Tab,
        WinitPhysKey::Code(KeyCode::Backspace) => PhysicalKey::Backspace,
        WinitPhysKey::Code(KeyCode::Delete) => PhysicalKey::Delete,
        WinitPhysKey::Code(KeyCode::Insert) => PhysicalKey::Insert,
        WinitPhysKey::Code(KeyCode::ArrowUp) => PhysicalKey::ArrowUp,
        WinitPhysKey::Code(KeyCode::ArrowDown) => PhysicalKey::ArrowDown,
        WinitPhysKey::Code(KeyCode::ArrowLeft) => PhysicalKey::ArrowLeft,
        WinitPhysKey::Code(KeyCode::ArrowRight) => PhysicalKey::ArrowRight,

        // Letter A-Z (gui_01 winit_backend に揃えて uppercase 表現)
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

        // Digit 0-9
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

        // Function keys F1-F24
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
    if !unsafe { ScreenToClient(hwnd, &raw mut pt) }.as_bool() {
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

