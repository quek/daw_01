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

use crate::app::{AppData, AppEvent, ClipRef};
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
        diag_window_start: None,
        diag_window_count: 0,
        diag_window_max_dt: std::time::Duration::ZERO,
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
    /// docs/plan_video.md §3 P5: background worker thread that owns
    /// the per-source IMFSourceReader pool. The GUI thread sends
    /// decode requests via `worker.request(...)` (= non-blocking,
    /// latest-target-wins coalescing per source) and drains finished
    /// frames via `worker.drain_results()` each main-loop iteration.
    /// Replaces the previous synchronous `VideoPlaybackEngine` field
    /// — that path froze the GUI for 30-100ms per frame at 1080p60
    /// (= the user's "Stop button 5 sec freeze" pathology).
    #[cfg(windows)]
    playback_worker: crate::video_playback_worker::PreviewDecodeWorker,
    /// docs/plan_video_perf.md P4: latest decoded ring snapshot per
    /// source. Populated by `drain_preview_worker_results` from the
    /// worker's `DecodedRing` messages; consulted by
    /// `drive_preview_playback` to pick the ring slot whose
    /// `target_micros` is nearest to the current playhead. Frames are
    /// not stored here directly — only `(target_micros, slot_idx)`
    /// pairs — because the actual GPU textures live in
    /// `preview.frame_textures` keyed by `(source_id, slot_idx)`.
    #[cfg(windows)]
    cached_rings: std::collections::HashMap<
        common::model::VideoSourceId,
        Vec<CachedRingSlot>,
    >,
    /// docs/plan_video.md P5 perf: 直近に preview decode を駆動した時刻。
    /// `drive_preview_playback` を `Song.video_framerate` Hz に throttle
    /// する基準。 main loop は vsync で 60fps+ 回るが、 video preview は
    /// project framerate (typically 30fps) を超えて更新する意味がないので、
    /// 余剰呼び出しを skip して同期 decode の負荷を半減する。 background
    /// worker thread (= plan §3 P5 正式設計) への移行はまだ先。
    #[cfg(windows)]
    last_preview_drive_at: Option<Instant>,
    /// Decrement-on-upload counter. While > 0 each
    /// `drain_preview_worker_results` upload emits one
    /// `tracing::info!` line with `upload_ms`. Pairs with the
    /// `VideoPlaybackEngine` per-source `timing_log_remaining` to
    /// confirm whether the 0.25x playback bottleneck sits in decode
    /// (worker side) or upload (main thread, this side).
    #[cfg(windows)]
    preview_upload_log_remaining: u32,
    /// `docs/plan_image_overlay.md` §4 P5: preview window 上の cursor
    /// 位置 (logical px、 window 左上原点)。 `WindowEvent::CursorMoved`
    /// で更新、 drag 中に delta 計算で使う。 cursor が window 外なら
    /// `None`。 session-only (Undo 不要)。
    preview_cursor: Option<(f32, f32)>,
    /// PiP rect の drag 操作中ステート (session-only)。 `Some` は
    /// mouse button down 後 → release までの間だけ。 release で `None`
    /// に戻す。 drag 中の cursor delta を `start_rect` に積み上げて
    /// AppEvent::SetClipImage{X/Y/W/H} を毎フレーム発火する。
    preview_drag: Option<PreviewDragState>,
}

/// preview window 上で PiP rect を drag 中の状態 (`docs/plan_image_
/// overlay.md` §4 P5)。 cursor delta を normalized 0..=1 に変換して
/// event の x/y/w/h に積む。
#[derive(Debug, Clone, Copy)]
struct PreviewDragState {
    mode: PreviewDragMode,
    /// drag 開始時の cursor 位置 (preview window 内 logical px)。
    start_cursor: (f32, f32),
    /// drag 開始時の event rect (normalized 0..=1)。 cursor delta を
    /// この値に加減して新 rect を作る (= 累積誤差なし)。
    start_rect: (f32, f32, f32, f32),
    /// drag 開始時の rotation_radians (radians)。 Rotate mode で
    /// cursor 角度との差分を取って新 rotation を計算する。
    start_rotation_radians: f32,
    /// drag 開始時の「rect 中心から cursor への角度」 (radians)。
    /// Rotate mode で `current_angle - start_cursor_angle + start
    /// _rotation` で新 rotation を出す。
    start_cursor_angle: f32,
    /// 操作中の image clip。 drag 中に selected_clip が切り替えられても
    /// 当初の clip を編集対象として保持。
    target: crate::app::ClipRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewDragMode {
    /// rect 全体を平行移動。
    Move,
    /// corner handle を引っ張ってサイズ変更。 corner: 0=NW / 1=NE /
    /// 2=SW / 3=SE。
    Resize { corner: u8 },
    /// 上辺中点から 24 px 外側の rotate handle を引っ張って回転。
    Rotate,
}

/// docs/plan_video_perf.md P4: metadata for one slot in a cached ring
/// snapshot. The decoded frame's actual GPU texture lives in
/// `PreviewWindowState::frame_textures` keyed by
/// `(source_id, slot_idx)` — this struct is the lookup index the
/// composite pass uses to find it.
#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
struct CachedRingSlot {
    /// Source-side time the slot was decoded for. Used by
    /// `nearest_ring_slot` to pick the slot closest to the current
    /// playhead's `source_micros`.
    target_micros: u64,
    /// Index into `SharedPool::slots` for HW path, or the worker's
    /// round-robin counter on the Bgra fallback path. Either way,
    /// pairs with `source_id` to key the GPU texture cache.
    slot_idx: u8,
    /// Cached for the composite pass — avoids round-trip to
    /// `frame_textures` just to read width/height.
    width: u32,
    height: u32,
}

/// docs/plan_video_perf.md P4: pick the slot whose `target_micros` is
/// closest to the requested `target`. `slots` is assumed
/// `target_micros`-ascending (= `drain_preview_worker_results`
/// sort-by-keys before insert). Returns `None` for an empty slice;
/// the composite pass treats that as "no frame available, skip
/// layer".
#[cfg(windows)]
fn nearest_ring_slot(slots: &[CachedRingSlot], target: u64) -> Option<CachedRingSlot> {
    slots.iter().min_by_key(|s| s.target_micros.abs_diff(target)).copied()
}

/// preview window 内で `project_resolution` を letterbox 配置したときの
/// 描画区域 `(x, y, w, h)` (= aspect-fit、 px 単位)。 PiP rect の
/// normalized 0..=1 はこの box 内で展開する。 描画 / hit-test / drag delta
/// が同 box を使うので window resize でも一致が保たれる。
fn preview_project_box(
    screen: (f32, f32),
    project_resolution: (u32, u32),
) -> (f32, f32, f32, f32) {
    let (sw, sh) = screen;
    let (pw, ph) = project_resolution;
    if sw <= 0.0 || sh <= 0.0 || pw == 0 || ph == 0 {
        return (0.0, 0.0, sw.max(0.0), sh.max(0.0));
    }
    let dst_aspect = sw / sh;
    let src_aspect = pw as f32 / ph as f32;
    if src_aspect >= dst_aspect {
        let h = sw / src_aspect;
        (0.0, (sh - h) * 0.5, sw, h)
    } else {
        let w = sh * src_aspect;
        ((sw - w) * 0.5, 0.0, w, sh)
    }
}

/// `docs/plan_image_overlay.md` §4 P5: 5 個の handle (NW/NE/SW/SE +
/// 中央) と PiP rect 内部に対し hit-test を行い、 drag mode を返す。
/// rect 外 / handle 外なら `None`。 corner handle の hit-box 半径は
/// 描画サイズ (10 px) より少し広めの 14 px (= 端を掴みやすく)。
fn hit_test_handles(
    overlay: (f32, f32, f32, f32),
    rotation_radians: f32,
    cursor: (f32, f32),
    screen: (f32, f32),
    project_resolution: (u32, u32),
) -> Option<PreviewDragMode> {
    let (nx, ny, nw, nh) = overlay;
    // PiP rect は project_resolution の letterbox 内座標で展開
    // (= 描画と同 idiom)。 window resize で hit box がズレない。
    let project_box = preview_project_box(screen, project_resolution);
    let rx = project_box.0 + nx * project_box.2;
    let ry = project_box.1 + ny * project_box.3;
    let rw = nw * project_box.2;
    let rh = nh * project_box.3;
    let cx0 = rx + rw * 0.5;
    let cy0 = ry + rh * 0.5;
    let (sin_r, cos_r) = rotation_radians.sin_cos();
    let rotate_point = |lx: f32, ly: f32| -> (f32, f32) {
        (cx0 + lx * cos_r - ly * sin_r, cy0 + lx * sin_r + ly * cos_r)
    };
    let half_w = rw * 0.5;
    let half_h = rh * 0.5;
    let (curx, cury) = cursor;
    const HIT_R: f32 = 14.0;
    // Rotate handle (上辺中点から 24 px 外側)。 corner より優先。
    let (rot_x, rot_y) = rotate_point(0.0, -half_h - 24.0);
    if (curx - rot_x).hypot(cury - rot_y) <= HIT_R {
        return Some(PreviewDragMode::Rotate);
    }
    // 4 corner handles (回転後座標で square hit box)。
    let corners = [
        (rotate_point(-half_w, -half_h), 0u8), // NW
        (rotate_point(half_w, -half_h), 1),    // NE
        (rotate_point(-half_w, half_h), 2),    // SW
        (rotate_point(half_w, half_h), 3),     // SE
    ];
    for ((hx, hy), idx) in corners {
        if (curx - hx).abs() <= HIT_R && (cury - hy).abs() <= HIT_R {
            return Some(PreviewDragMode::Resize { corner: idx });
        }
    }
    // 内部 rect (= move handle)。 rotation を逆変換して cursor を rect
    // local 系に持ち込み、 axis-aligned 内外判定する。
    let lx = (curx - cx0) * cos_r + (cury - cy0) * sin_r;
    let ly = -(curx - cx0) * sin_r + (cury - cy0) * cos_r;
    if lx.abs() <= half_w && ly.abs() <= half_h {
        return Some(PreviewDragMode::Move);
    }
    None
}

/// drag delta を `start_rect` (normalized 0..=1) に積んで AppEvent::
/// SetClipImage{X/Y/W/H} を発火する。 lane 経由の override は handler
/// 側で「ImageEvent.field を直接書く」 動作で、 lane があれば lane
/// が override し続ける (= drag の見た目変化は lane を無効化しない限り
/// 隠れる)。 P5.3 段階では「lane があれば現在 playhead に point を
/// 打つ」 までは実装しないので、 lane がある field は drag が無視される
/// 形になる (P5 完了後のフォロー)。
fn handle_preview_drag(
    app: &AppData,
    proxy: &EventLoopProxy<AppEvent>,
    drag: &PreviewDragState,
    cursor: (f32, f32),
    screen: (f32, f32),
    project_resolution: (u32, u32),
) {
    let (sx, sy) = drag.start_cursor;
    // cursor delta も project_box の幅 / 高さで normalize (= 描画と同 idiom、
    // window resize でも drag 量が画像座標と一致する)。
    let project_box = preview_project_box(screen, project_resolution);
    let dx = (cursor.0 - sx) / project_box.2.max(1.0);
    let dy = (cursor.1 - sy) / project_box.3.max(1.0);
    let (sx0, sy0, sw0, sh0) = drag.start_rect;
    let (nx, ny, nw, nh) = match drag.mode {
        PreviewDragMode::Move => {
            // rect 全体を平行移動。 rect が画面外に出ないよう x/y を
            // [0, 1-w] / [0, 1-h] で clamp。 w/h は不変。
            let new_x = (sx0 + dx).clamp(0.0, (1.0 - sw0).max(0.0));
            let new_y = (sy0 + dy).clamp(0.0, (1.0 - sh0).max(0.0));
            (new_x, new_y, sw0, sh0)
        }
        PreviewDragMode::Resize { corner } => {
            // 各 corner で「対辺を固定して反対側を引っ張る」 動作。
            // 例: SE handle は x/y 不変、 w/h を増減。 NW handle は
            // x/y も移動 + w/h を反方向に増減。
            const MIN: f32 = 0.01; // 1% 未満には縮められない (= 視認可能性)
            match corner {
                0 => {
                    // NW
                    let nx = (sx0 + dx).clamp(0.0, sx0 + sw0 - MIN);
                    let ny = (sy0 + dy).clamp(0.0, sy0 + sh0 - MIN);
                    let nw = (sx0 + sw0) - nx;
                    let nh = (sy0 + sh0) - ny;
                    (nx, ny, nw, nh)
                }
                1 => {
                    // NE
                    let ny = (sy0 + dy).clamp(0.0, sy0 + sh0 - MIN);
                    let nw = (sw0 + dx).clamp(MIN, 1.0 - sx0);
                    let nh = (sy0 + sh0) - ny;
                    (sx0, ny, nw, nh)
                }
                2 => {
                    // SW
                    let nx = (sx0 + dx).clamp(0.0, sx0 + sw0 - MIN);
                    let nw = (sx0 + sw0) - nx;
                    let nh = (sh0 + dy).clamp(MIN, 1.0 - sy0);
                    (nx, sy0, nw, nh)
                }
                _ => {
                    // SE
                    let nw = (sw0 + dx).clamp(MIN, 1.0 - sx0);
                    let nh = (sh0 + dy).clamp(MIN, 1.0 - sy0);
                    (sx0, sy0, nw, nh)
                }
            }
        }
        PreviewDragMode::Rotate => {
            // Rotate mode: cursor の rect 中心からの角度差分で rotation
            // を更新。 rect 中心は project_box 内座標で計算 (描画と同 idiom)。
            let (nx0, ny0, nw0, nh0) = drag.start_rect;
            let cx0 = project_box.0 + nx0 * project_box.2 + nw0 * project_box.2 * 0.5;
            let cy0 = project_box.1 + ny0 * project_box.3 + nh0 * project_box.3 * 0.5;
            let cur_angle = (cursor.1 - cy0).atan2(cursor.0 - cx0);
            let new_rotation = drag.start_rotation_radians
                + (cur_angle - drag.start_cursor_angle);
            // 値が変わったときだけ発火 (= 0.001 rad ≒ 0.057° 未満は skip)。
            // 同 idiom で image / text どちらかの SetClip*Rotation を撃つ。
            let cur_rot = drag.start_rotation_radians;
            if (new_rotation - cur_rot).abs() > 1e-3 {
                let ev = match preview_drag_target_kind(app, drag.target) {
                    PreviewDragTargetKind::Text => AppEvent::SetClipTextRotation {
                        target: drag.target,
                        value: new_rotation,
                    },
                    _ => AppEvent::SetClipImageRotation {
                        target: drag.target,
                        value: new_rotation,
                    },
                };
                let _ = proxy.send_event(ev);
            }
            return;
        }
    };
    // 値が変わった field だけ AppEvent を発火 (= 無駄な undo step を
    // 発生させない)。 first event 比較。 image / text どちらかの events
    // を持つ clip を見つけ、 同 idiom の現値 (x, y, w, h) を返す。
    let target = drag.target;
    let content = app
        .song
        .tracks
        .get(target.track as usize)
        .and_then(|t| t.clips.get(target.clip as usize))
        .and_then(|c| app.song.clip_contents.get(&c.content_id));
    let kind = preview_drag_target_kind(app, target);
    let current = match content {
        Some(c) if matches!(kind, PreviewDragTargetKind::Text) => {
            c.text_events().and_then(|ev| ev.first()).map(|ev| (ev.x, ev.y, ev.w, ev.h))
        }
        Some(c) => {
            c.image_events().and_then(|ev| ev.first()).map(|ev| (ev.x, ev.y, ev.w, ev.h))
        }
        None => None,
    };
    let Some((cx, cy, cw, ch)) = current else {
        return;
    };
    let send = |ev: AppEvent| {
        let _ = proxy.send_event(ev);
    };
    if (nx - cx).abs() > 1e-5 {
        send(match kind {
            PreviewDragTargetKind::Text => AppEvent::SetClipTextX { target, value: nx },
            _ => AppEvent::SetClipImageX { target, value: nx },
        });
    }
    if (ny - cy).abs() > 1e-5 {
        send(match kind {
            PreviewDragTargetKind::Text => AppEvent::SetClipTextY { target, value: ny },
            _ => AppEvent::SetClipImageY { target, value: ny },
        });
    }
    if (nw - cw).abs() > 1e-5 {
        send(match kind {
            PreviewDragTargetKind::Text => AppEvent::SetClipTextW { target, value: nw },
            _ => AppEvent::SetClipImageW { target, value: nw },
        });
    }
    if (nh - ch).abs() > 1e-5 {
        send(match kind {
            PreviewDragTargetKind::Text => AppEvent::SetClipTextH { target, value: nh },
            _ => AppEvent::SetClipImageH { target, value: nh },
        });
    }
}

/// docs/plan_text_overlay.md §4 P6: preview drag target の clip kind
/// (image / text)。 `BeginImagePiPDrag` vs `BeginTextPiPDrag` 等、 同
/// idiom の event を分岐するのに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewDragTargetKind {
    Image,
    Text,
}

fn preview_drag_target_kind(app: &AppData, target: ClipRef) -> PreviewDragTargetKind {
    let content = app
        .song
        .tracks
        .get(target.track as usize)
        .and_then(|t| t.clips.get(target.clip as usize))
        .and_then(|c| app.song.clip_contents.get(&c.content_id));
    match content {
        Some(common::model::ClipContent::Text(_)) => PreviewDragTargetKind::Text,
        _ => PreviewDragTargetKind::Image,
    }
}

struct Runner {
    attrs: Option<WindowAttributes>,
    build_app: Option<Box<dyn FnOnce(EventLoopProxy<AppEvent>) -> AppData + Send>>,
    proxy: EventLoopProxy<AppEvent>,
    state: Option<RunnerState>,
    last_tick: Instant,
    /// Diagnostic: rolling 30-frame window for main thread render rate.
    /// `render_frame` accumulates `dt` since the last tick and emits one
    /// `tracing::info!` line per ~30 frames with mean fps + max dt so we
    /// can correlate worker-decode-throughput (~1.0x source-rate by the
    /// `decode timing` logs) with the actual main-thread frame rate
    /// (= what the user sees). If main loop drops below 10 fps, that
    /// explains the 0.25x preview perception even when the worker is
    /// keeping up.
    diag_window_start: Option<Instant>,
    diag_window_count: u32,
    diag_window_max_dt: std::time::Duration,
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
            playback_worker: crate::video_playback_worker::PreviewDecodeWorker::new(),
            #[cfg(windows)]
            cached_rings: std::collections::HashMap::new(),
            #[cfg(windows)]
            last_preview_drive_at: None,
            // 60 uploads ≈ 2 seconds at 30fps preview, mirroring the
            // per-decode log budget in `VideoPlaybackEngine`.
            #[cfg(windows)]
            preview_upload_log_remaining: 60,
            preview_cursor: None,
            preview_drag: None,
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
        let dt = now.duration_since(self.last_tick);
        self.last_tick = now;

        // Diagnostic: emit a summary every 30 render frames so we can
        // see whether the main thread loop matches worker decode
        // throughput. If main fps drops below 10, the user's "0.25x"
        // preview perception is explained: the worker delivers frames
        // at 1x but the main loop only paints them at 10Hz.
        if self.diag_window_start.is_none() {
            self.diag_window_start = Some(now);
        }
        self.diag_window_count = self.diag_window_count.saturating_add(1);
        if dt > self.diag_window_max_dt {
            self.diag_window_max_dt = dt;
        }
        if self.diag_window_count >= 30 {
            let elapsed_ms = self
                .diag_window_start
                .map(|s| now.duration_since(s).as_millis() as u64)
                .unwrap_or(0);
            let fps = if elapsed_ms > 0 {
                self.diag_window_count as f64 * 1000.0 / elapsed_ms as f64
            } else {
                0.0
            };
            let max_dt_ms = self.diag_window_max_dt.as_millis() as u64;
            let mean_dt_ms = elapsed_ms / self.diag_window_count as u64;
            tracing::info!(
                frames = self.diag_window_count,
                elapsed_ms,
                fps = format!("{fps:.1}"),
                mean_dt_ms,
                max_dt_ms,
                "main render fps"
            );
            self.diag_window_start = Some(now);
            self.diag_window_count = 0;
            self.diag_window_max_dt = std::time::Duration::ZERO;
        }

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
            // `docs/plan_image_overlay.md` §4 P5: PiP rect の drag 編集。
            // CursorMoved / MouseInput を捕捉して、 hit-test → drag
            // state 開始 → MouseMoved delta から normalized rect 更新
            // → AppEvent::SetClipImage{X,Y,W,H} 発火。
            WindowEvent::CursorMoved { position, .. } => {
                let cursor = (position.x as f32, position.y as f32);
                state.preview_cursor = Some(cursor);
                if let Some(drag) = state.preview_drag {
                    let size = preview.renderer.size();
                    let project_resolution = state.app.song.video_resolution;
                    handle_preview_drag(
                        &state.app,
                        &self.proxy,
                        &drag,
                        cursor,
                        (size.width as f32, size.height as f32),
                        project_resolution,
                    );
                }
            }
            WindowEvent::CursorLeft { .. } => {
                state.preview_cursor = None;
            }
            WindowEvent::MouseInput {
                state: button_state,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                let pressed = matches!(button_state, winit::event::ElementState::Pressed);
                if pressed {
                    if let Some(cursor) = state.preview_cursor
                        && let Some(overlay) = preview.selection_overlay
                        && let Some(target) = state.app.selected_clip
                    {
                        let size = preview.renderer.size();
                        let screen = (size.width as f32, size.height as f32);
                        let rotation = preview.selection_rotation_radians;
                        let project_resolution = state.app.song.video_resolution;
                        let mode = hit_test_handles(
                            overlay,
                            rotation,
                            cursor,
                            screen,
                            project_resolution,
                        );
                        if let Some(mode) = mode {
                            // rect 中心 (letterbox 内座標) を計算し、 cursor
                            // との角度を保存 (Rotate mode の delta 計算で使う)。
                            let (nx, ny, nw, nh) = overlay;
                            let project_box =
                                preview_project_box(screen, project_resolution);
                            let cx0 = project_box.0
                                + nx * project_box.2
                                + nw * project_box.2 * 0.5;
                            let cy0 = project_box.1
                                + ny * project_box.3
                                + nh * project_box.3 * 0.5;
                            let start_cursor_angle =
                                (cursor.1 - cy0).atan2(cursor.0 - cx0);
                            state.preview_drag = Some(PreviewDragState {
                                mode,
                                start_cursor: cursor,
                                start_rect: overlay,
                                start_rotation_radians: rotation,
                                start_cursor_angle,
                                target,
                            });
                            // drag begin: snapshot 1 個 + lane recording
                            // seed。 image / text で別 marker event を
                            // 撃つ (= 後者は `docs/plan_text_overlay.md`
                            // §4 P6)。 target の clip kind で振り分ける。
                            let drag_target_kind =
                                preview_drag_target_kind(&state.app, target);
                            let begin_ev = match drag_target_kind {
                                PreviewDragTargetKind::Text => AppEvent::BeginTextPiPDrag,
                                _ => AppEvent::BeginImagePiPDrag,
                            };
                            let _ = self.proxy.send_event(begin_ev);
                        }
                    }
                } else if let Some(drag) = state.preview_drag.take() {
                    // drag end: lane recording seed のクリア。 begin と同
                    // kind の End event を送る。
                    let end_ev = match preview_drag_target_kind(&state.app, drag.target) {
                        PreviewDragTargetKind::Text => AppEvent::EndTextPiPDrag,
                        _ => AppEvent::EndImagePiPDrag,
                    };
                    let _ = self.proxy.send_event(end_ev);
                }
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
                // main window の HWND を owner として渡して preview を
                // main の owned-window に。 Win32 仕様で owned は owner
                // の常に前面、 owner 最小化で owned も最小化、 タスクバー
                // にも乗らない (= MV プレビュー用の従属ウィンドウ動作)。
                #[cfg(windows)]
                let owner_hwnd = state.window.hwnd_isize();
                #[cfg(not(windows))]
                let owner_hwnd: Option<isize> = None;
                match crate::view::preview_window::PreviewWindowState::create(
                    event_loop,
                    initial_size,
                    owner_hwnd,
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

    /// docs/plan_video.md §3 P5: drive multi-clip playback for the
    /// preview window via the background decode worker.
    ///
    /// Each call:
    ///   1. Drains any decoded frames the worker has finished since
    ///      last cycle and uploads them into per-source GPU textures.
    ///   2. Computes the currently-active source list at the playhead.
    ///   3. Sends a decode request to the worker for each active
    ///      source (= latest-target-wins coalescing inside the worker).
    ///   4. Builds the composite layer list from whatever textures
    ///      are currently in `preview.frame_textures` — that's the
    ///      newest decoded frame per source, possibly slightly behind
    ///      the playhead if the worker hasn't caught up yet. The user
    ///      sees a held frame instead of a stutter.
    ///
    /// Throttled to `Song.video_framerate` (typical 30fps). The main
    /// loop runs at vsync; throttling avoids spamming the worker with
    /// requests that would just overwrite each other.
    /// docs/plan_image_overlay.md §P3: drain `AppData.pending_image_uploads`
    /// by uploading staged BGRA buffers into the preview window's
    /// per-`ImageSourceId` GPU texture, and mirror the resulting
    /// handle into `AppData.image_texture_cache` so the composite
    /// pass can look it up by source_id. Idempotent / cheap when the
    /// queue is empty.
    #[cfg(windows)]
    fn drain_image_uploads(state: &mut RunnerState) {
        if state.app.pending_image_uploads.is_empty() {
            return;
        }
        let Some(preview) = state.preview.as_mut() else {
            // Preview window is not open yet; keep the queue intact
            // and re-drain when it opens (= the user is allowed to
            // import images before opening the preview).
            return;
        };
        let pending: Vec<_> =
            state.app.pending_image_uploads.drain(..).collect();
        for image_source_id in pending {
            let Some((w, h, bgra)) =
                state.app.image_source_bgra.remove(&image_source_id)
            else {
                continue; // already uploaded (= rapid undo path)
            };
            let handle = preview.upload_image_bgra(image_source_id, w, h, &bgra);
            state.app.image_texture_cache.insert(image_source_id, handle);
        }
    }

    #[cfg(windows)]
    fn drive_preview_playback(state: &mut RunnerState) {
        // Step 1: always drain results first, even on throttled
        // frames. Decoded frames are precious — uploading them is
        // cheap (GPU memcpy) and keeps the texture fresh.
        Self::drain_preview_worker_results(state);
        // docs/plan_image_overlay.md §P3: also drain any pending
        // image uploads so a freshly-imported image appears on the
        // very next composite pass (= no 1-frame delay).
        Self::drain_image_uploads(state);

        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        // Throttle the request side. Use a small floor (24fps =
        // ~41ms) so a malformed project framerate (0 / NaN) still
        // permits some decoding.
        let frame_interval_ms = {
            let fps = state.app.song.video_framerate.max(1.0);
            (1000.0 / fps).round().max(33.0) as u64
        };
        let now = Instant::now();
        if let Some(last) = state.last_preview_drive_at
            && now.duration_since(last).as_millis() < frame_interval_ms as u128
        {
            return;
        }
        state.last_preview_drive_at = Some(now);

        let Some(playhead_beat) = state.app.playhead_beat.map(f64::from) else {
            preview.set_composite_layers(Vec::new());
            preview.set_text_layers(Vec::new());
            return;
        };
        let active = crate::video_playback::VideoPlaybackEngine::active_sources_at(
            &state.app.song,
            playhead_beat,
        );
        let project_dir = state
            .app
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));

        // docs/plan_video_perf.md P4: ring lookahead. step is derived
        // from the project framerate; the worker decodes
        // `PREVIEW_RING_SIZE` consecutive frames at
        // `center + i * step` and writes each into an independent
        // `SharedPool` slot. Skipped when no video is active (= the
        // worker has nothing to decode), so an image-only / text-only
        // project doesn't request empty ring decodes.
        if !active.is_empty() {
            let step_micros = {
                let fps = state.app.song.video_framerate.max(1.0) as f64;
                (1_000_000.0 / fps).round() as u64
            };
            for frame_info in &active {
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
                state.playback_worker.request(
                    frame_info.video_source_id,
                    abs_path,
                    frame_info.source_micros,
                    step_micros,
                );
            }
        }

        // Step 4: build composite layers by picking the cached ring
        // slot nearest to each active source's current `source_micros`.
        // docs/plan_video_perf.md P4: this is where the lookahead pays
        // off — when the worker is mid-decode for the next ring, the
        // composite still has the previous ring's nearest slot to show,
        // so frame-pacing is smooth even under decode jitter.
        let mut layers: Vec<crate::view::preview_window::CompositeLayer> =
            Vec::with_capacity(active.len());
        for frame_info in active {
            let Some(ring) = state.cached_rings.get(&frame_info.video_source_id) else {
                continue; // worker hasn't produced a ring yet
            };
            let Some(slot) = nearest_ring_slot(ring, frame_info.source_micros) else {
                continue; // ring is empty (= all slots failed to decode)
            };
            let key = (frame_info.video_source_id, slot.slot_idx);
            let Some((handle, w, h)) = preview.frame_textures.get(&key).copied() else {
                continue; // texture not yet imported for this slot
            };
            // Sanity: dimensions in the cached ring slot must match
            // the texture's. If they diverge the texture got
            // re-created underneath us — fall back to ring-slot dims.
            let _ = (w, h);
            layers.push(crate::view::preview_window::CompositeLayer {
                texture: handle,
                width: slot.width,
                height: slot.height,
                alpha: frame_info.alpha,
                // Video clips always letterbox; the PiP rect is the
                // image-overlay path only.
                pip_rect: None,
                rotation_radians: 0.0,
            });
        }

        // docs/plan_image_overlay.md §P3: image overlay layers.
        // `active_image_sources_at` returns frames already sorted
        // bottom→top by `z_index`; interleave them with video layers
        // by re-sorting the combined Vec on z_index ascending.
        let image_frames = crate::image_compose::active_image_sources_at(
            &state.app.song,
            playhead_beat,
        );
        for frame_info in image_frames {
            // `AppData::image_texture_cache` stores just the
            // TextureHandle (dimensions come from
            // `Song.image_sources`).
            let Some(handle) =
                state.app.image_texture_cache.get(&frame_info.image_source_id).copied()
            else {
                continue; // texture not yet uploaded
            };
            let dims = state
                .app
                .song
                .image_sources
                .get(&frame_info.image_source_id)
                .map(|s| (s.width, s.height))
                .unwrap_or((0, 0));
            if dims.0 == 0 || dims.1 == 0 {
                continue;
            }
            layers.push(crate::view::preview_window::CompositeLayer {
                texture: handle,
                width: dims.0,
                height: dims.1,
                alpha: frame_info.alpha,
                pip_rect: Some((frame_info.x, frame_info.y, frame_info.w, frame_info.h)),
                rotation_radians: frame_info.rotation_radians,
            });
        }
        // z_index ascending = bottom→top draw order. Video frames
        // populated `z_index` from `active_sources_at`; image frames
        // populated theirs from `active_image_sources_at` (same
        // counter convention). After this sort the composite pass
        // draws layers in the right order.
        //
        // Stable so identical z_index between video & image keeps
        // their relative order from the per-helper emit (= image
        // typically dropped at top track index 0, so it ends up
        // above the video in the composite naturally).
        //
        // Note: each helper computes its z_index independently, so a
        // mixed scene may produce duplicate z_index values that don't
        // perfectly reflect the user-visible track order. P4
        // (inspector) is the right place to consolidate this into a
        // single multi-kind active_sources iterator; for now the
        // image-on-top convention covers the MV-overlay use case.
        let _ = ();
        preview.set_composite_layers(layers);
        // docs/plan_text_overlay.md §4 P3: text overlay layers are
        // resolved independently (= no GPU texture, just font / color
        // / rect metadata) and rendered on top of every textured-quad
        // layer. The runner gathers them per frame so lane automation
        // tracks the playhead in real time.
        let text_frames = crate::text_compose::active_text_sources_at(
            &state.app.song,
            playhead_beat,
        );
        preview.set_text_layers(text_frames);
        // PiP rect の normalized 座標は project_resolution の letterbox
        // 内で展開される (= window resize しても画像 aspect が崩れない)。
        // Song.video_resolution を毎 frame 同期。
        preview.set_project_resolution(state.app.song.video_resolution);

        // `docs/plan_image_overlay.md` §4 P5: 選択中 image event の
        // PiP rect を preview window 上に縁取り + handle で描画する。
        // selected_clip が ClipContent::Image でなければ overlay は
        // `None` (= 非表示)。 lane override が effective なときは event
        // の生値ではなく resolve 後の rect を表示するため image_compose
        // と同じパスで「現在 frame の解決後 rect」 を再計算するのが
        // 理想だが、 frame collect は既に終わっており、 ここでは event
        // の生値ベースで縁取りを置く (= lane drag P5.3 で recording に
        // 切り替えた瞬間に event 値が override される動作で OK)。
        // docs/plan_text_overlay.md §4 P6: text clip も同 idiom で
        // overlay 縁取り + handle を出す。 image / text を順に try。
        let overlay_info = state
            .app
            .selected_clip
            .and_then(|cref| {
                let track = state.app.song.tracks.get(cref.track as usize)?;
                let clip = track.clips.get(cref.clip as usize)?;
                let content = state.app.song.clip_contents.get(&clip.content_id)?;
                if let Some(events) = content.image_events() {
                    let ev = events.first()?;
                    Some(((ev.x, ev.y, ev.w, ev.h), ev.rotation_radians))
                } else if let Some(events) = content.text_events() {
                    let ev = events.first()?;
                    Some(((ev.x, ev.y, ev.w, ev.h), ev.rotation_radians))
                } else {
                    None
                }
            });
        let (overlay_rect, rotation) = match overlay_info {
            Some((r, rot)) => (Some(r), rot),
            None => (None, 0.0),
        };
        preview.set_selection_overlay(overlay_rect, rotation);
    }

    /// docs/plan_video_perf.md P4: drain the worker's ring snapshots
    /// and upload each ring slot into the preview window's
    /// per-`(source_id, slot_idx)` texture cache, then record the
    /// ring slot metadata in `state.cached_rings` so the composite
    /// pass can pick the slot nearest to the current playhead.
    /// Idempotent / cheap when the worker is idle.
    #[cfg(windows)]
    fn drain_preview_worker_results(state: &mut RunnerState) {
        let rings = state.playback_worker.drain_results();
        if rings.is_empty() {
            return;
        }
        let Some(preview) = state.preview.as_mut() else {
            return; // preview window closed; drop the results.
        };
        for ring in rings {
            let source_id = ring.source_id;
            let mut cached: Vec<CachedRingSlot> = Vec::with_capacity(ring.slots.len());
            for slot in &ring.slots {
                let t_upload_start = std::time::Instant::now();
                let (variant_name, frame_bytes, w, h, slot_idx) = match &slot.frame {
                    crate::video_playback::DecodedFrame::Shared {
                        width,
                        height,
                        slot_idx,
                        ..
                    } => ("shared", 0_usize, *width, *height, *slot_idx),
                    crate::video_playback::DecodedFrame::Bgra {
                        width,
                        height,
                        bgra,
                    } => {
                        // CPU fallback: the variant has no slot field,
                        // so the worker writes all ring slots into the
                        // same `(source_id, 0)` texture. The composite
                        // pass treats this as "1-frame-latest" rather
                        // than a true ring (acceptable for HW-less
                        // environments — see plan §P4).
                        ("bgra", bgra.len(), *width, *height, 0_u8)
                    }
                };
                preview.upload_frame(source_id, slot_idx, &slot.frame);
                cached.push(CachedRingSlot {
                    target_micros: slot.target_micros,
                    slot_idx,
                    width: w,
                    height: h,
                });
                if slot.target_micros > 0 && state.preview_upload_log_remaining > 0 {
                    state.preview_upload_log_remaining =
                        state.preview_upload_log_remaining.saturating_sub(1);
                    let upload_ms = t_upload_start.elapsed().as_millis() as u64;
                    tracing::info!(
                        video_source_id = source_id,
                        source_micros = slot.target_micros,
                        variant = variant_name,
                        slot_idx,
                        width = w,
                        height = h,
                        frame_bytes,
                        upload_ms,
                        "preview upload timing"
                    );
                }
            }
            // Keep cached slots ordered by `target_micros` so the
            // nearest-slot binary search in `nearest_ring_slot` works.
            cached.sort_by_key(|s| s.target_micros);
            state.cached_rings.insert(source_id, cached);
        }
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

