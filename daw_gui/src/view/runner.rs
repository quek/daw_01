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
    AppEvent as PlatformEvent, KeyEvent, Modifiers, PhysicalPosition, PhysicalSize, ScrollDelta,
    WindowBackend,
};
use daw_ui_renderer::{Renderer, Scene};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition as WinitPhysPos;
use winit::event::{Ime as WinitIme, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::{WindowAttributes, WindowId};

use crate::app::{AppData, AppEvent, ClipRef};
use crate::view::shortcuts::daw_shortcut_map;
use daw_ui_platform::WinitWindow;
use daw_ui_platform::winit_backend::{
    map_button, map_phys_key, map_state, query_cursor_pos_in_window,
};

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
        fps_window_start: Instant::now(),
        fps_window_frames: 0,
    };
    event_loop.run_app(&mut runner)
}

struct RunnerState {
    window: Arc<WinitWindow>,
    renderer: Renderer<WinitWindow>,
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
    /// `AppData::ui_ephemeral.project_generation` のうち、GPU 側の解放を
    /// 済ませた世代 (`release_project_scoped_gpu_state`)。
    released_project_generation: u64,
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
    /// 立ち絵 group box を preview 上で drag 中の状態（clip drag とは別経路、
    /// `docs/plan_tachie_group_transform.md` §5.5）。
    preview_group_drag: Option<GroupDragState>,
    /// r.md #42: GPU 復旧の状態。`None` = 正常。
    gpu_recovery: Option<GpuRecovery>,
    /// main window の render error ログのレート制限 (秒 1 行 + 抑制件数)。
    /// これが無いと別要因の恒常エラーで再びログが 6MB 級に膨らむ
    /// (実際 daw_gui.2026-08-01 は 54,043 行 / 6.4MB)。
    render_error_log: RenderErrorLog,
    /// preview window 用の **独立した** レート制限器。
    ///
    /// main と共有すると、main が正常描画している限り毎フレーム `reset()` が走って
    /// preview 側の `last_at` が消え、抑制が完全に効かなくなる (60 行/秒)。
    /// main / preview は別々の wgpu Device を持つので片方だけ壊れる状態は構造的に
    /// あり得るため、レート制限器も窓ごとに分ける。
    preview_error_log: RenderErrorLog,
}

/// r.md #42: device lost からの復旧進行状態。
struct GpuRecovery {
    /// 消失を検出した時刻 (= [`GPU_RECOVERY_GIVEUP`] 判定の起点)。
    lost_at: Instant,
    /// 次に再試行する時刻。
    retry_at: Instant,
    /// 連続失敗回数 (backoff 段数)。
    attempts: u32,
    /// 「保存して再起動してください」 の OS ダイアログを既に出したか (1 回だけ)。
    giveup_notified: bool,
}

/// 再試行の backoff (250ms → 500ms → 1s → 2s で頭打ち)。
///
/// **無制限に毎フレーム再試行してはいけない**: present が起きないと vsync 律速が
/// 消えるので、 復旧できないまま回すと 860fps でスピンして CPU を焼き続ける
/// (実ログで 51,827 行/分)。 バッテリー駆動でファン全開になる形で顕在化する。
fn gpu_retry_backoff(attempts: u32) -> std::time::Duration {
    let ms = match attempts {
        0 => 250,
        1 => 500,
        2 => 1000,
        _ => 2000,
    };
    std::time::Duration::from_millis(ms)
}

/// この時間 GPU 復旧に失敗し続けたら、 OS ダイアログで「保存して再起動」 を促す。
///
/// 静かに再試行し続けるだけだと、 ユーザーから見て「固まったまま何も分からない」
/// (8/1 のログでは ✕ を 4 回押して諦め、 強制終了している)。 自前 UI は GPU が
/// 無いと描けないので、 **OS 側が描くメッセージボックス**で伝える。
const GPU_RECOVERY_GIVEUP: std::time::Duration = std::time::Duration::from_secs(30);

/// render error ログのレート制限器 (秒 1 行 + 抑制件数のサマリ)。
#[derive(Default)]
struct RenderErrorLog {
    last_at: Option<Instant>,
    suppressed: u32,
}

impl RenderErrorLog {
    /// 抑制間隔。 これより短い間隔で来た記録はカウントだけして出力しない。
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

    /// 1 件記録する。 直近 1 秒以内に出していれば抑制し、 次に出す行へ件数を載せる。
    /// 戻り値は **実際にログを出したか** (テスト用。 production では捨ててよい)。
    fn record(&mut self, now: Instant, what: &str, err: &daw_ui_renderer::RenderError) -> bool {
        if self.last_at.is_some_and(|t| now.duration_since(t) < Self::INTERVAL) {
            self.suppressed = self.suppressed.saturating_add(1);
            return false;
        }
        self.last_at = Some(now);
        let suppressed = std::mem::take(&mut self.suppressed);
        tracing::error!(error = %err, suppressed, "{what}");
        true
    }

    /// **その窓が**正常描画できたときに抑制状態を畳む。
    ///
    /// 呼ぶのは対応する窓の成功フレームだけ (main の成功で preview 用の状態を消すと
    /// preview 側の抑制が無効化される)。
    fn reset(&mut self) {
        self.last_at = None;
        self.suppressed = 0;
    }
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

/// preview window 上で立ち絵 group box を drag 中の状態
/// (`docs/plan_tachie_group_transform.md` §5.5)。clip drag (`PreviewDragState`)
/// とは別経路: target は track id、編集対象は base `GroupTransform`。
#[derive(Debug, Clone, Copy)]
struct GroupDragState {
    mode: PreviewDragMode,
    start_cursor: (f32, f32),
    /// drag 開始時の base transform（cursor delta をこれに積む = 累積誤差なし）。
    start_transform: common::model::GroupTransform,
    target_track_id: u32,
    /// drag 開始時の pivot（= anchor）screen 位置。Rotate / Resize の中心。
    pivot_screen: (f32, f32),
    /// Rotate 用: 開始時の pivot→cursor 角度。
    start_cursor_angle: f32,
    /// Resize 用: 開始時の pivot→cursor 距離（uniform scale の基準）。
    start_pivot_dist: f32,
}

/// 立ち絵 group box の handle hit-test（任意 pivot 回転に対応）。`transform`
/// は base、`project_box` は letterbox 区域。`group_quad_params` と同一の
/// rect / pivot / rotation を使うので overlay 描画と完全一致する。
fn group_hit_test(
    transform: &common::model::GroupTransform,
    project_box: (f32, f32, f32, f32),
    cursor: (f32, f32),
) -> Option<PreviewDragMode> {
    let (rx, ry, rw, rh, rot, px, py, _) =
        crate::group_compose::group_quad_params(transform, project_box);
    let pivx = rx + px;
    let pivy = ry + py;
    let (sin_r, cos_r) = rot.sin_cos();
    let rotate = |sx: f32, sy: f32| -> (f32, f32) {
        let lx = sx - pivx;
        let ly = sy - pivy;
        (pivx + lx * cos_r - ly * sin_r, pivy + lx * sin_r + ly * cos_r)
    };
    let (curx, cury) = cursor;
    const HIT_R: f32 = 14.0;
    // rotate handle（上辺中点から 24 px 外側、回転前 y を引いてから回転）。
    let (rotx, roty) = rotate(rx + rw * 0.5, ry - 24.0);
    if (curx - rotx).hypot(cury - roty) <= HIT_R {
        return Some(PreviewDragMode::Rotate);
    }
    let corners = [
        (rotate(rx, ry), 0u8),
        (rotate(rx + rw, ry), 1),
        (rotate(rx, ry + rh), 2),
        (rotate(rx + rw, ry + rh), 3),
    ];
    for ((hx, hy), idx) in corners {
        if (curx - hx).abs() <= HIT_R && (cury - hy).abs() <= HIT_R {
            return Some(PreviewDragMode::Resize { corner: idx });
        }
    }
    // body: cursor を pivot 基準で逆回転して unrotated rect に内外判定。
    let lx = (curx - pivx) * cos_r + (cury - pivy) * sin_r;
    let ly = -(curx - pivx) * sin_r + (cury - pivy) * cos_r;
    if lx >= -px && lx <= rw - px && ly >= -py && ly <= rh - py {
        return Some(PreviewDragMode::Move);
    }
    None
}

/// group drag delta を base transform に積んで `SetGroupTransformField` を発火。
/// Move=位置(x/y)、Rotate=pivot 周り回転、Resize=pivot 中心の uniform scale。
/// scale の非一様や anchor は inspector の数値で編集する。
fn handle_group_drag(
    proxy: &EventLoopProxy<AppEvent>,
    drag: &GroupDragState,
    cursor: (f32, f32),
    screen: (f32, f32),
    project_resolution: (u32, u32),
) {
    use common::model::GroupTransformParam as G;
    let t = drag.start_transform;
    let project_box = preview_project_box(screen, project_resolution);
    let send = |param: G, value: f32| {
        let _ = proxy.send_event(AppEvent::SetGroupTransformField {
            track_id: drag.target_track_id,
            param,
            value,
        });
    };
    match drag.mode {
        PreviewDragMode::Move => {
            let dx = (cursor.0 - drag.start_cursor.0) / project_box.2.max(1.0);
            let dy = (cursor.1 - drag.start_cursor.1) / project_box.3.max(1.0);
            send(G::X, t.x + dx);
            send(G::Y, t.y + dy);
        }
        PreviewDragMode::Rotate => {
            let cur_angle =
                (cursor.1 - drag.pivot_screen.1).atan2(cursor.0 - drag.pivot_screen.0);
            send(
                G::Rotation,
                t.rotation_radians + (cur_angle - drag.start_cursor_angle),
            );
        }
        PreviewDragMode::Resize { .. } => {
            // pivot（anchor）中心の uniform scale。pivot→cursor 距離比で倍率。
            let d1 =
                (cursor.0 - drag.pivot_screen.0).hypot(cursor.1 - drag.pivot_screen.1);
            let ratio = if drag.start_pivot_dist > 1.0 {
                d1 / drag.start_pivot_dist
            } else {
                1.0
            };
            send(G::ScaleX, (t.scale_x * ratio).clamp(0.1, 10.0));
            send(G::ScaleY, (t.scale_y * ratio).clamp(0.1, 10.0));
        }
    }
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
    /// Slot index paired with `source_id` to key the GPU texture cache.
    /// Always 0 with the libav BGRA sink (1-frame-latest per source).
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
    child_rotation_radians: f32,
    map: &crate::group_compose::CanvasMap,
    cursor: (f32, f32),
) -> Option<PreviewDragMode> {
    let (nx, ny, nw, nh) = overlay;
    // 子 PiP を canvas→screen 写像（通常 image は project_box 恒等、active
    // group の子は親 group の affine）。描画 (`draw_selection_overlay`) と
    // 同じ `CanvasMap` を使うので hit box が縁取り・ハンドルと完全一致する。
    let (cx0, cy0) = map.to_screen(nx + nw * 0.5, ny + nh * 0.5);
    let rw = nw * map.size.0;
    let rh = nh * map.size.1;
    // 総回転 = group 軸回転 + 子自身の回転。
    let (sin_r, cos_r) = (map.rotation + child_rotation_radians).sin_cos();
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
    map: &crate::group_compose::CanvasMap,
) {
    // cursor の screen delta を canvas-norm の子 PiP delta に逆写像する。
    // 通常 image は project_box 恒等写像（従来どおり）、active group の子は
    // 親 group の affine（回転 + 非一様 scale）を除去する。2 点の `from_screen`
    // の差を取るので pivot まわりの平行移動は相殺され、delta だけが残る。
    let (cu, cv) = map.from_screen(cursor.0, cursor.1);
    let (su, sv) = map.from_screen(drag.start_cursor.0, drag.start_cursor.1);
    let dx = cu - su;
    let dy = cv - sv;
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
            // Rotate mode: cursor の rect 中心からの角度差分で子の rotation を
            // 更新。rect 中心は `CanvasMap` で写像（group 空間なら group affine
            // 適用後の中心）。group_rot は drag 中一定なので角度差分はそのまま
            // 子 rotation の差分になる。
            let (nx0, ny0, nw0, nh0) = drag.start_rect;
            let (cx0, cy0) = map.to_screen(nx0 + nw0 * 0.5, ny0 + nh0 * 0.5);
            let cur_angle = (cursor.1 - cy0).atan2(cursor.0 - cx0);
            let new_rotation = drag.start_rotation_radians
                + (cur_angle - drag.start_cursor_angle);
            // 値が変わったときだけ発火 (= 0.001 rad ≒ 0.057° 未満は skip)。
            // Move/Resize と同じく **model の現値** と比較する (drag 開始値と
            // 比較すると静止カーソルでも同値イベントを再発火し続ける)。
            // 同 idiom で image / text どちらかの SetClip*Rotation を撃つ。
            let kind = preview_drag_target_kind(app, drag.target);
            let cur_rot = {
                let content = app
                    .song_doc.song()
                    .tracks
                    .get(drag.target.track as usize)
                    .and_then(|t| t.clips.get(drag.target.clip as usize))
                    .and_then(|c| app.song_doc.song().clip_contents.get(&c.content_id));
                match content {
                    Some(c) if matches!(kind, PreviewDragTargetKind::Text) => c
                        .text_events()
                        .and_then(|ev| ev.first())
                        .map(|ev| ev.rotation_radians),
                    Some(c) => c
                        .image_events()
                        .and_then(|ev| ev.first())
                        .map(|ev| ev.rotation_radians),
                    None => None,
                }
                .unwrap_or(drag.start_rotation_radians)
            };
            if (new_rotation - cur_rot).abs() > 1e-3 {
                let ev = match kind {
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
        .song_doc.song()
        .tracks
        .get(target.track as usize)
        .and_then(|t| t.clips.get(target.clip as usize))
        .and_then(|c| app.song_doc.song().clip_contents.get(&c.content_id));
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
        .song_doc.song()
        .tracks
        .get(target.track as usize)
        .and_then(|t| t.clips.get(target.clip as usize))
        .and_then(|c| app.song_doc.song().clip_contents.get(&c.content_id));
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
    /// r.md #49: ステータスバーの FPS 表示用の実測窓 (開始時刻と描いた本数)。
    /// per-frame dt ではなく本数 ÷ 経過時間で出す (`render_frame` のコメント参照)。
    fps_window_start: Instant,
    fps_window_frames: u32,
}

impl Runner {
    fn dispatch_platform_event(&mut self, ev: PlatformEvent) {
        let Some(state) = self.state.as_mut() else { return };
        state.input.ingest(&ev);
        match &ev {
            PlatformEvent::Resized(size) => state.renderer.resize(*size),
            // r.md #49: メインウィンドウの focus は「アプリがアクティブか」の材料。
            PlatformEvent::Focus(focused) => {
                state.app.activity.main_focused = *focused;
                state.app.sync_app_active_with_audio();
            }
            _ => {}
        }
        // r.md #49: **すべての** platform event で再描画する。
        //
        // 旧実装は pointer / key / scroll / IME / modifier だけを列挙し、残り
        // (`Focus` / `PointerEntered` / `PointerLeft` / `FileHovered` /
        // `FileHoverCancelled` / `FileDropped` / `ScaleFactorChanged`) を
        // `_ => {}` に落としていた。これらは **33ms tick の無条件再描画に
        // 救われて動いていただけ**で、tick を間引くと即座に体感バグになる
        // (Alt+Tab でドラッグ表示が貼り付く / ファイルのドロップ対象ハイライトが
        // 出ない・消えない / 窓外へ出た hover が残る)。
        //
        // そもそも platform event が届くのは **自分の窓が入力を受けたとき** =
        // アプリはアクティブなので、ここで省電力の判定を挟む意味も無い。
        state.window.request_redraw();
    }

    /// r.md #49: 「今この瞬間、画面を描く意味があるか」を評価し、背景スレッドと
    /// 共有するフラグを更新して、描画すべきかを返す。
    ///
    /// 描画条件と tick レートの条件は **同じ**。再生 / 録音中は非アクティブでも
    /// 描き続けるので (`should_keep_rendering`)、`on_tick` に同居する曲末の
    /// 自動停止判定・オートメーション録音の分解能・再生追従スクロールも自動的に
    /// 30Hz を保つ。
    fn refresh_activity(state: &mut RunnerState, now: Instant) -> bool {
        let keep = state.app.should_keep_rendering(now);
        state
            .app
            .activity
            .awake
            .store(keep, std::sync::atomic::Ordering::Release);
        keep
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
        let dwin = Arc::new(WinitWindow::new(window));
        let renderer = Renderer::new(dwin.clone()).expect("Renderer::new");

        // `with_window` で `set_cursor_request` callback を `WindowBackend::set_cursor`
        // に自動接続する。これが無いと widget 内の `Ui::set_cursor` 要求が OS まで
        // 届かず、ピアノロール / アレンジビューの hover / drag でカーソル形状が
        // 変わらない。
        // undo は daw_gui の SongDoc snapshot が SSoT (lib 側 undo は S4a で撤去)。
        let ui = UiHost::<AppData>::with_window(dwin.clone())
            .with_shortcut_map(daw_shortcut_map())
            .with_clipboard(ArboardClipboard::new());

        let build_app = self.build_app.take().expect("build_app 既に消費");
        let app = build_app(self.proxy.clone());
        // native file save dialog を background thread で owner-modal に開くため、
        // main window の HWND を AppData へ渡す (`action_open_export_mp4_dialog`)。
        #[cfg(windows)]
        let app = {
            let mut app = app;
            app.ui_ephemeral.main_window_hwnd = dwin.hwnd_isize();
            app
        };

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
            released_project_generation: 0,
            #[cfg(windows)]
            last_preview_drive_at: None,
            // 60 uploads ≈ 2 seconds at 30fps preview, mirroring the
            // per-decode log budget in `VideoPlaybackEngine`.
            #[cfg(windows)]
            preview_upload_log_remaining: 60,
            preview_cursor: None,
            preview_drag: None,
            preview_group_drag: None,
            gpu_recovery: None,
            render_error_log: RenderErrorLog::default(),
            preview_error_log: RenderErrorLog::default(),
        });
    }

    /// r.md #42: `ControlFlow::WaitUntil` で予約した GPU 復旧の再試行時刻に到達したら
    /// 再試行する。 winit は `WaitUntil` の期限到来を `new_events(ResumeTimeReached)` で
    /// 通知するので、 ここが「復旧リトライを時間駆動で回す」 唯一の入口。
    fn new_events(&mut self, event_loop: &ActiveEventLoop, _cause: winit::event::StartCause) {
        let due = self
            .state
            .as_ref()
            .and_then(|s| s.gpu_recovery.as_ref())
            .is_some_and(|r| Instant::now() >= r.retry_at);
        if due {
            self.attempt_gpu_recovery(event_loop);
        }
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
                // 未保存変更があれば確認モーダルを開き、 終了を保留する。
                // 変更が無ければ `request_close` が即 `should_quit` を立て、
                // 下の `quit_if_requested` が cleanup + exit する。
                if let Some(state) = self.state.as_mut() {
                    state.app.request_close();
                }
                self.quit_if_requested(event_loop);
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
                    // OS auto-repeat は shortcut 層 (`Ui::frame`) が落とす。 ここで捨てると
                    // text_input の Backspace / 矢印長押しまで死ぬので **通す**。
                    repeat: event.repeat,
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

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        let Some(state) = self.state.as_mut() else { return };
        // 別インスタンスが起動を試み、 既存 (= この) ウィンドウへ
        // 前面化を要求してきた。 window 操作なので AppData ではなく runner が
        // 直接処理する: 最小化なら復元してフォアグラウンドへ。
        if matches!(event, AppEvent::RaiseMainWindow) {
            let win = state.window.inner();
            win.set_minimized(false);
            win.focus_window();
            state.window.request_redraw();
            return;
        }
        // r.md #36: プラグインエディタ窓で押され、 **プラグインが消化しなかった** キー。
        // chord → shortcut 名の解決は `SHORTCUTS` (唯一の宣言) から引き、 解決済みの名前を
        // shortcut レイヤへ注入する。 これで着地点は `root.rs` の `take_shortcut` =
        // メインウィンドウで押した場合と完全に同一経路になり、 AppEvent の変換表も
        // 第 2 の dispatch も増えない。
        if let AppEvent::Plugin(common::protocol::PluginEvent::EditorKey { chord, .. }) = event {
            if let Some((_, name)) = crate::view::shortcuts::forwarded_editor_chords()
                .into_iter()
                .find(|(c, _)| *c == chord)
            {
                state.ui.inject_shortcut(name);
                state.window.request_redraw();
            }
            return;
        }
        // r.md #49: tick 系だけは「画面に出る値が変わったか」で再描画を決める。
        // 毎秒 30 回届くのに中身が同じ (停止中は playhead も peak も動かない) ため、
        // 従来はこれだけで 30fps 描き続けていた。他の event は**従来どおり無条件に
        // 再描画**する — 判定対象を 5 つに閉じ込めることで、残り数百の variant に
        // 「立て忘れ = 画面が固まる」という新しい失敗モードを持ち込まない。
        let is_tick = matches!(
            event,
            AppEvent::Tick { .. }
                | AppEvent::ModScalarsTick(_)
                | AppEvent::TrackPeaksTick(_)
                | AppEvent::MetricsTick { .. }
                | AppEvent::SystemMetricsTick { .. }
        );
        let before = is_tick.then(|| state.app.tick_visual_fingerprint());
        state.app.handle_event(event);
        let changed = match before {
            Some(before) => state.app.tick_visual_fingerprint() != before,
            None => true,
        };
        if Self::refresh_activity(state, Instant::now()) {
            if changed {
                state.window.request_redraw();
            }
        } else {
            // r.md #49: 子プロセスへの Song 同期は通常 `render_frame` の末尾で
            // 1 frame 1 回に coalesce される (docs/plan_arch_refactor.md §7.5)。
            // 省電力中はその口が来ないので、ここで流す。
            //
            // これが無いと、**裏で MIDI コントローラを触った編集が engine に
            // 届かない** (`AppEvent::MidiControlChange` → binding → param 変更は
            // フォーカスと無関係に届く)。`flush_song_sync` は epoch 差分ゲートなので、
            // 編集が無ければ no-op。
            //
            // **描画する側では呼ばない**。編集は必ず非 tick event か epoch を
            // 動かす tick から来るので `changed` が立ち、再描画 = frame flush が
            // 必ず後に続く。両方で呼ぶと、ドラッグ中に frame flush と tick flush が
            // 交互に走って `LoadSong` の送信回数が増える (coalesce の意味が薄れる)。
            state.app.flush_song_sync();
        }
        // 背景スレッド (IPC bridge) 経由の event で、 plugin state 取得待ちの
        // 非同期保存が完了して `should_quit` が立つことがある (= 「保存して
        // 終了」 で plugin 有り project)。 ここで終了を拾う。
        self.quit_if_requested(event_loop);
    }

    /// EventLoop 終了直前 (= 通常 close 経路、 process kill 以外で呼ばれる)。
    /// メインウィンドウの geometry を `%LOCALAPPDATA%\daw_01\window_state.json`
    /// に永続化して次回起動で復元する。 失敗は log のみ (起動を妨げない)。
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_ref() else { return };
        save_main_window_state(&state.window, state.app.ui_prefs.app_dirs.as_ref());
    }
}

fn save_main_window_state(
    window: &WinitWindow,
    app_dirs: Option<&common::app_dirs::AppDirs>,
) {
    let Some(path) = app_dirs.map(|d| d.window_state()) else { return };
    let win = window.inner();
    let size = win.inner_size();
    let scale = win.scale_factor();
    let pos = win.outer_position().unwrap_or(WinitPhysPos { x: 100, y: 100 });
    let state = crate::window_state::WindowState {
        width: f64::from(size.width) / scale,
        height: f64::from(size.height) / scale,
        x: pos.x,
        y: pos.y,
        maximized: win.is_maximized(),
    };
    if let Err(e) = crate::window_state::save(&path, &state) {
        tracing::warn!(error = ?e, "failed to save window_state.json");
    }
}

impl Runner {
    /// `AppData.should_quit` が立っていたら cleanup (recovery file 削除) して
    /// event loop を抜ける。 not-dirty close / 「保存せず終了」 / 保存完了の
    /// いずれかで立つ。 close 確認をキャンセルした場合は立たないので no-op。
    fn quit_if_requested(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_ref() else { return };
        if state.app.ui_ephemeral.should_quit {
            state.app.on_shutdown();
            event_loop.exit();
        }
    }

    /// r.md #42: device lost を検出した。 復旧シーケンスを開始する (既に進行中なら no-op)。
    fn begin_gpu_recovery(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else { return };
        if state.gpu_recovery.is_some() {
            return;
        }
        let now = Instant::now();
        tracing::warn!("gpu device lost — 復旧を開始します");
        state.app.ui_ephemeral.status_message = "GPU を再初期化しています…".into();
        state.gpu_recovery = Some(GpuRecovery {
            lost_at: now,
            retry_at: now,
            attempts: 0,
            giveup_notified: false,
        });
        self.attempt_gpu_recovery(event_loop);
    }

    /// GPU 資産を作り直す 1 回の試行。 成功したら復旧状態を畳んで再描画を要求し、
    /// 失敗したら backoff して `ControlFlow::WaitUntil` で次を予約する。
    fn attempt_gpu_recovery(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else { return };
        let Some(recovery) = state.gpu_recovery.as_mut() else { return };
        let now = Instant::now();

        // preview window も **同時に** 作り直す。 GPU リセットは両方の device を殺すので、
        // main だけ復旧して preview を放置すると preview 側だけ固まり続ける。
        // OS ウィンドウ自体は残す (位置とサイズを保つ / ちらつかせない)。
        // 片方だけ成功した状態で再試行が来ることがあるので、 既に生きている方は触らない。
        let preview_result = match state.preview.as_mut() {
            Some(p) if !p.renderer.is_live() => p.recreate_gpu(),
            _ => Ok(()),
        };
        let main_result = if state.renderer.is_live() {
            Ok(())
        } else {
            state
                .renderer
                .recreate()
                .map_err(|e| format!("Renderer::recreate: {e}"))
        };

        if let (Ok(()), Ok(())) = (&preview_result, &main_result) {
            // 粗粒度描画キャッシュに旧世代の TextureHandle が焼き込まれているので必ず捨てる。
            // これを忘れると build も test も clippy も通るのに絵だけ欠ける。
            state.ui.invalidate_scene_cache();
            // GPU 上にしか無かった派生データ (動画サムネイル / 画像) をディスクから再構築。
            state.app.rebuild_gpu_derived_caches();
            state.app.ui_ephemeral.status_message = "GPU を再初期化しました".into();
            state.gpu_recovery = None;
            state.render_error_log.reset();
            state.preview_error_log.reset();
            state.window.request_redraw();
            if let Some(p) = state.preview.as_ref() {
                p.window.request_redraw();
            }
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
            tracing::info!("gpu recovered");
            return;
        }

        for e in [preview_result.err(), main_result.err()].into_iter().flatten() {
            tracing::warn!(error = %e, attempts = recovery.attempts, "gpu 再初期化に失敗");
        }
        recovery.attempts = recovery.attempts.saturating_add(1);
        let backoff = gpu_retry_backoff(recovery.attempts);
        recovery.retry_at = now + backoff;

        // 30 秒失敗し続けたら OS ダイアログで伝える (自前 UI は GPU が無いと描けない)。
        if !recovery.giveup_notified && now.duration_since(recovery.lost_at) >= GPU_RECOVERY_GIVEUP {
            recovery.giveup_notified = true;
            #[cfg(windows)]
            let parent_hwnd = state.app.ui_ephemeral.main_window_hwnd;
            #[cfg(not(windows))]
            let parent_hwnd: Option<isize> = None;
            spawn_gpu_giveup_dialog(parent_hwnd);
        }
        state.app.ui_ephemeral.status_message =
            "GPU を再初期化しています… (応答がありません)".into();
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            now + backoff,
        ));
    }

    fn render_frame(&mut self, event_loop: &ActiveEventLoop) -> bool {
        // docs/plan_video.md P4: sync the preview window lifecycle
        // with `AppData.preview_window_visible` BEFORE everything
        // else — creating the window requires an ActiveEventLoop which
        // is only available here, and destroying it has to happen
        // before we lock other parts of state.
        self.sync_preview_window(event_loop);

        let Some(state) = self.state.as_mut() else { return false };
        let now = Instant::now();
        // この frame の overlay / clip スピナー / engine 未接続判定が
        // すべて同じ時刻を読むよう、frame 冒頭で 1 度だけ確定する (5s 境界の食い違い回避)。
        state.app.ui_ephemeral.frame_now = now;
        let dt = now.duration_since(self.last_tick);
        self.last_tick = now;

        // resource monitor (r.md #3): GUI の実フレームレートを AppData へ。
        // status bar 常駐メーターと詳細パネルが読む。
        //
        // r.md #49: **1 フレームごとの dt からではなく、一定時間の実測本数から出す**。
        // 描画がイベント駆動になると dt の分布が二極化する (連続描画中は 16ms、
        // アイドル明けは数秒) ため、per-frame dt を EMA に入れる方式は破綻する:
        // - そのまま入れる → アイドル明けの 1 サンプルで表示が 0 付近まで落ちる
        // - 長い dt を捨てる → **短い側だけが残って EMA が上に暴走する**
        //   (実測 3.9fps のとき 126fps と表示された。2026-08-15 実機検証)
        //
        // 本数 ÷ 経過時間なら、どちらの regime でも「秒間何枚描いたか」をそのまま
        // 表す。アイドル中に低い値が出るのは嘘ではなく、まさに省電力が効いている証拠。
        // r.md #49: この frame を描く意味があるかを **ui.frame の前に** 決める。
        //
        // daw-ui の `UiHost::frame` は末尾で自動 `request_redraw` を呼ぶ。発火条件には
        // **widget 発の継続アニメ要求** (レベルメーターの減衰 / peak hold) が含まれ、
        // widget は自分が見えているかを知らないので、非アクティブでも要求を出し続ける。
        // これを塞がないと、こちらのゲートを迂回して 60fps で回り続ける
        // (実際、停止 + 非アクティブでメーターが落ち切るまで 8 秒間 60fps だった。
        //  2026-08-15 実機検証)。
        let keep = Self::refresh_activity(state, now);
        state.ui.set_redraw_suppressed(!keep);

        const FPS_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
        if keep {
            self.fps_window_frames = self.fps_window_frames.saturating_add(1);
            let fps_elapsed = now.duration_since(self.fps_window_start);
            if fps_elapsed >= FPS_WINDOW {
                state.app.ipc.metrics.fps =
                    self.fps_window_frames as f32 / fps_elapsed.as_secs_f32();
                self.fps_window_frames = 0;
                self.fps_window_start = now;
            }
        } else {
            // 省電力に入るこの frame が「最後に描かれた絵」として残る。実測 0fps
            // なのに直前の 60 が焼き付くと、止まっている画面が「60fps 出ている」と
            // 読めてしまうので、ここで 0 に畳んでから描く。
            state.app.ipc.metrics.fps = 0.0;
            self.fps_window_frames = 0;
            self.fps_window_start = now;
        }

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
            // r.md #16: 再生 / lip-sync / synth アニメ中は毎 frame 再描画するので
            // これは ~2 行/秒で既定ログを埋める。 debug へ降格し (RUST_LOG=debug で
            // 復活)、 info の既定ログには出さない。
            tracing::debug!(
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

        // r.md #42: GPU 消失中は GPU を触る処理を全部飛ばす。 `ui.frame` は下で通常どおり
        // 走らせるので、 Space / 保存 / 終了は効き続ける (= 消失中の唯一の救い)。
        // ここで staging を drain してしまうと、 死んだ store へ upload して CPU 側の
        // 元データだけ失う (「GPU が SSoT」 設計の裏返し) ので必ず gate する。
        let gpu_live = state.renderer.is_live();
        let mut device_lost = !gpu_live;

        if gpu_live {
            // 別プロジェクトに切り替わったら GPU 側の Song スコープ状態も捨てる。
            // handle で表現できない preview 側 (frame textures / decode ring) を
            // 世代印で破棄する経路 (main renderer のテクスチャは下の破棄予約が担当)。
            Self::release_project_scoped_gpu_state(state);
            // AppData 側で参照を捨てた main renderer の texture を実際に解放する
            // (AppData は Renderer を持てないので破棄予約経由。upload の **前**に流して
            // 解放と確保の順序を固定する)。
            Self::drain_texture_destroys(&mut state.app, &mut state.renderer);

            // docs/plan_video.md P3.5: drain pending video thumbnail
            // uploads BEFORE this frame's `ui.frame()` so the arrangement
            // view can read the resulting `TextureHandle` immediately.
            // First frame after import shows the thumbnail with one
            // frame's latency (= imports landing during a frame are
            // queued for the next frame's drain).
            Self::drain_video_thumbnail_uploads(&mut state.app, &mut state.renderer);
            // 画像も同じ frame で main / preview 双方へ upload する (preview の有無に
            // 依らず arrangement のサムネイルが出るように)。
            Self::drain_image_uploads(state);

            // P5: drive playback decode + upload into the preview
            // window's frame_texture. When the playhead lands inside a
            // video clip, decode the corresponding source_micros frame
            // and re-upload; otherwise clear so the placeholder shows.
            #[cfg(windows)]
            if state.preview.is_some() {
                Self::drive_preview_playback(state);
            }

            // P4 baseline / P5 hand-off: render the preview window each
            // frame. `render` picks between the placeholder text and the
            // textured-quad path internally based on whether
            // `frame_texture` is Some.
            if let Some(preview) = state.preview.as_mut() {
                match preview.render(&state.app.theme) {
                    Ok(()) => {
                        // 抑制状態を畳むのは **preview 自身が成功したとき** だけ。
                        state.preview_error_log.reset();
                        // r.md #49: 成功時に `request_redraw` しない。ここで要求すると
                        // その redraw が `handle_preview_window_event` の
                        // `RedrawRequested` で **もう一度** `preview.render()` を呼び、
                        // main の 1 フレームにつき preview を 2 回描いていた。
                        // preview は main のフレームに従属して描かれれば足りる
                        // (OS 起因の再描画は `RedrawRequested` 側が拾う)。
                    }
                    Err(e) if e.is_device_lost() => device_lost = true,
                    // 一時障害 / validation は次フレームで再試行 (redraw は継続要求)。
                    Err(e) => {
                        state.preview_error_log.record(now, "preview render error", &e);
                        preview.window.request_redraw();
                    }
                }
            }
        }

        let screen = state.renderer.size();
        state.scene.clear();
        let input = state.input.take_input();

        // r.md #48: テーマの SSoT は `AppData.theme`。 UiHost へは **毎フレーム無条件に**
        // 同じ Arc を流し込む (同一なら no-op)。 こうしておけば「テーマを変えたのに
        // push し忘れた」 が構造的に起きない。 変化したフレームだけ描画キャッシュを捨てる —
        // `with_widget_node` の input_hash にも `HeavyCtx::cached` の viewport_key にも色は
        // 入っておらず、色は描画コマンドに焼き込まれて Scenegraph に残るため、捨てないと
        // アレンジ / ピアノロールだけ旧テーマの色で固まる。
        if state.ui.set_palette(state.app.theme.core.clone()) {
            state.ui.invalidate_scene_cache();
        }

        state.ui.frame_with_fonts(
            &mut state.app,
            &mut state.scene,
            screen,
            input,
            state.renderer.font_system_mut(),
            |app, ui| {
                crate::view::root::build_root(app, ui, screen);
            },
        );

        // 子プロセス sync の pull 一本化 (docs/plan_arch_refactor.md §7.5): この frame の
        // user_event dispatch と ui.frame 内の view Edit がすべて適用された後・render の
        // 直前に 1 回だけ flush する。 flush_song_sync は epoch 差 (edit_epoch !=
        // last_synced_epoch) を見て変化時のみ LoadSong を送るので、 1 frame 内の複数編集
        // (scrub / MIDI-CC flood 等) は 1 回の LoadSong に構造的に coalesce される。
        state.app.flush_song_sync();

        // close 確認モーダルの「保存して終了」(同期保存) /「保存せず終了」 は
        // この frame の `ui.frame` 内で Edit が適用され `should_quit` が立つ。
        // `state` を borrow 中なので helper を介さず直接 cleanup + exit する。
        if state.app.ui_ephemeral.should_quit {
            state.app.on_shutdown();
            event_loop.exit();
            return false;
        }

        if gpu_live {
            match state.renderer.render(&state.scene) {
                Ok(()) => state.render_error_log.reset(),
                Err(e) if e.is_device_lost() => device_lost = true,
                Err(e) => {
                    state.render_error_log.record(now, "render error", &e);
                }
            }
        }

        // タイトル差分反映: "<*>プロジェクト名"。未保存変更があれば先頭に * を付ける。
        // file_path 未設定 (新規未保存) は "Untitled"。 dirty は epoch 比較 O(1)
        // (SongDoc::is_dirty) なので毎フレーム読んでよい。
        let project_name = state
            .app
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled");
        let new_title = if state.app.song_doc.is_dirty() {
            format!("*{project_name}")
        } else {
            project_name.to_string()
        };
        if new_title != state.last_title {
            state.window.set_title(&new_title);
            state.last_title = new_title;
        }

        // plugin 追加 → load 完了で queue された GUI auto-open 要求を処理する (#6)。
        // window 生成を frame loop に置くことで headless test では window を作らない。
        state.app.drain_pending_gui_opens();

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

        // r.md #42: device lost を検出したらここで復旧を開始する (`state` の borrow を
        // 手放してから呼ぶ必要があるので frame 末尾)。
        if device_lost {
            self.begin_gpu_recovery(event_loop);
            // 復旧は `ControlFlow::WaitUntil` の backoff で駆動する。 ここで継続 redraw を
            // 要求すると present しないまま毎フレーム回り、 860fps でスピンして CPU を
            // 焼き続ける (実ログで 51,827 行/分)。
            return false;
        }

        // 再生中に加え、VOICEVOX 合成/口パク生成中も連続再描画を要求して
        // クリップ上スピナー + 全体オーバーレイを回す。engine 未接続が確定したら
        // `voicevox_animating` が false を返すので static 警告表示で再描画は止まる
        // (CPU/GPU を回し続けない)。overlay 描画と同じ `now` (= frame_now) を使う。
        //
        // r.md #49: 省電力中は継続要求そのものを打ち切る。`keep` は frame 冒頭で
        // 確定した値をそのまま使う (自動 redraw の抑止と同じ判断でなければ、
        // 「抑止したのに継続要求は出す」ような食い違いが生まれる)。
        //
        // frame 中に再生が始まった場合は `user_event` 側の `AppEvent::Play` 処理が
        // 改めて `refresh_activity` して redraw を要求するので取りこぼさない。
        //
        // r.md #51: 「走っているか」は `should_keep_rendering` と同じ
        // `transport_rolling()` で判定する (述語を 2 つ持たない)。
        let state = self.state.as_ref().expect("render_frame 内で state は生存");
        keep && (state.app.transport_rolling() || state.app.voicevox_animating(now))
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
                state.app.ui_prefs.preview_window_visible = false;
                // Lifecycle pass on the next render_frame drops the
                // preview state; nothing else to do here.
            }
            WindowEvent::Resized(size) => {
                preview.resize(daw_ui_platform::PhysicalSize {
                    width: size.width,
                    height: size.height,
                });
            }
            // r.md #49: preview 窓も daw_01 の窓なので、ここを触っている間は
            // アプリはアクティブ。これを拾わないと preview をクリックした瞬間に
            // main が `Focused(false)` を受けて「非アクティブ」と誤判定する
            // (preview 側の `Focused(true)` はどこにも届かない)。
            WindowEvent::Focused(focused) => {
                state.app.activity.preview_focused = focused;
                state.app.sync_app_active_with_audio();
                if focused {
                    state.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                // device lost はメインループ側の復旧シーケンスが拾う (ここで再帰的に
                // request_redraw しないことで、 消失中の preview スピンも止まる)。
                match preview.render(&state.app.theme) {
                    Ok(()) => state.preview_error_log.reset(),
                    // device lost はメインループ側の復旧シーケンスが拾う。
                    Err(e) if e.is_device_lost() => {}
                    Err(e) => {
                        state.preview_error_log.record(
                            Instant::now(),
                            "preview render error",
                            &e,
                        );
                    }
                }
            }
            // r.md #26: preview window にフォーカスがあるときの Space で
            // 再生 / 停止をトグル。 preview には text 入力欄が無いので無条件で
            // transport に割り当てられる。 auto-repeat (押しっぱなし) は無視して
            // 1 押下 1 トグルにする。 main window の `daw.play_toggle` と同じ
            // `AppEvent::PlayToggle` を proxy 経由で送る (user_event 経路を通り、
            // main window の redraw 要求と再生スレッド起動が同一 SSoT で走る)。
            WindowEvent::KeyboardInput { event, .. }
                if matches!(event.state, winit::event::ElementState::Pressed)
                    && !event.repeat
                    && matches!(
                        map_phys_key(event.physical_key),
                        daw_ui_platform::PhysicalKey::Space
                    ) =>
            {
                let _ = self.proxy.send_event(AppEvent::PlayToggle);
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
                    let project_resolution = state.app.song_doc.song().video_resolution;
                    let project_box = preview_project_box(
                        (size.width as f32, size.height as f32),
                        project_resolution,
                    );
                    // 描画 (`draw_selection_overlay`) と同じ **ライブの**
                    // `selection_group_transform` で逆写像する。こうすると描画・
                    // hit-test・drag が常に同一の group affine を共有し、再生中に
                    // automation が group を動かしてもハンドルが cursor とズレない
                    // （凍結値だと描画はライブ・drag は古い値で不一致になる）。
                    // 通常 image は None = 恒等写像。
                    let map = match preview.selection_group_transform {
                        Some(t) => crate::group_compose::CanvasMap::group(&t, project_box),
                        None => crate::group_compose::CanvasMap::project(project_box),
                    };
                    handle_preview_drag(&state.app, &self.proxy, &drag, cursor, &map);
                }
                if let Some(gdrag) = state.preview_group_drag {
                    let size = preview.renderer.size();
                    let project_resolution = state.app.song_doc.song().video_resolution;
                    handle_group_drag(
                        &self.proxy,
                        &gdrag,
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
                        && let Some(target) = state.app.selected_clip_ref()
                    {
                        let size = preview.renderer.size();
                        let screen = (size.width as f32, size.height as f32);
                        let rotation = preview.selection_rotation_radians;
                        let project_resolution = state.app.song_doc.song().video_resolution;
                        let project_box = preview_project_box(screen, project_resolution);
                        // 選択中 clip が active visual group の子なら親 group の
                        // affine を合成（= ハンドルが立ち絵に重なる）。drag 中は
                        // 毎 frame ライブの `selection_group_transform` を読み直す
                        // ので、ここでは凍結しない（描画と完全一致）。
                        let map = match preview.selection_group_transform {
                            Some(t) => crate::group_compose::CanvasMap::group(&t, project_box),
                            None => crate::group_compose::CanvasMap::project(project_box),
                        };
                        let mode = hit_test_handles(overlay, rotation, &map, cursor);
                        if let Some(mode) = mode {
                            // rect 中心 (canvas→screen 写像後) と cursor の角度を
                            // 保存 (Rotate mode の delta 計算で使う)。
                            let (nx, ny, nw, nh) = overlay;
                            let (cx0, cy0) = map.to_screen(nx + nw * 0.5, ny + nh * 0.5);
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
                    // Transform box drag begin（clip drag が始まらなかったときのみ）。
                    // 選択中トラックに Transform 配置 device が刺さって
                    // いれば対象（立ち絵 group も通常トラックも）。base group_transform は
                    // device 追加時に materialize 済なので overlay と同じ effective transform
                    // で hit-test する（枠が出れば必ず掴める）。
                    if state.preview_drag.is_none()
                        && let Some(cursor) = state.preview_cursor
                        && let Some(track_id) = state.app.cursor_track_id()
                        && let Some(track) = state.app.song_doc.song().track_by_id(track_id)
                        && let Some(transform) = crate::video_fx::resolve_track_transform(
                            state.app.song_doc.song(),
                            track,
                            state.app.transport.playhead_beat.map(f64::from).unwrap_or(0.0),
                            &state.app.transport.mod_scalars,
                        )
                    {
                        let size = preview.renderer.size();
                        let screen = (size.width as f32, size.height as f32);
                        let project_resolution = state.app.song_doc.song().video_resolution;
                        let project_box = preview_project_box(screen, project_resolution);
                        if let Some(mode) = group_hit_test(&transform, project_box, cursor) {
                            let (rx, ry, _rw, _rh, _rot, px, py, _) =
                                crate::group_compose::group_quad_params(
                                    &transform,
                                    project_box,
                                );
                            let pivx = rx + px;
                            let pivy = ry + py;
                            state.preview_group_drag = Some(GroupDragState {
                                mode,
                                start_cursor: cursor,
                                start_transform: transform,
                                target_track_id: track_id,
                                pivot_screen: (pivx, pivy),
                                start_cursor_angle: (cursor.1 - pivy)
                                    .atan2(cursor.0 - pivx),
                                start_pivot_dist: (cursor.0 - pivx)
                                    .hypot(cursor.1 - pivy),
                            });
                            let _ =
                                self.proxy.send_event(AppEvent::BeginGroupTransformDrag);
                        }
                    }
                } else {
                    if let Some(drag) = state.preview_drag.take() {
                        // drag end: lane recording seed のクリア。 begin と同
                        // kind の End event を送る。
                        let end_ev =
                            match preview_drag_target_kind(&state.app, drag.target) {
                                PreviewDragTargetKind::Text => AppEvent::EndTextPiPDrag,
                                _ => AppEvent::EndImagePiPDrag,
                            };
                        let _ = self.proxy.send_event(end_ev);
                    }
                    if state.preview_group_drag.take().is_some() {
                        let _ = self.proxy.send_event(AppEvent::EndGroupTransformDrag);
                    }
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
        let visible = state.app.ui_prefs.preview_window_visible;
        match (visible, state.preview.is_some()) {
            (true, false) => {
                let initial_size = state.app.song_doc.song().video_resolution;
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
                        // r.md #42: 画像の CPU staging は main への upload 時に捨てている
                        // (メモリ SSoT はディスク) ので、後から開いた preview は自前の
                        // texture を持っていない。ディスクから staging を作り直す
                        // (冪等: 既に staging 済みなら job は積まれない)。
                        // preview を閉じて開き直したときにも同じ経路で復元される。
                        state.app.begin_asset_decode("プレビュー用の画像を読込中");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "preview window create failed");
                        state.app.ui_prefs.preview_window_visible = false;
                        state.app.ui_ephemeral.status_message =
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
                // r.md #49: 無くなった窓はアクティブではない。閉じた瞬間に
                // focus が付いていると true のまま固着し、二度と省電力に入れない。
                state.app.activity.preview_focused = false;
                state.app.sync_app_active_with_audio();
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
    /// `AppData.pending_image_uploads` を drain し、staging された BGRA を
    /// **main / preview の両 renderer へそれぞれアップロード**する。
    ///
    /// # なぜ両方に上げるのか (r.md #42 レビュー指摘)
    ///
    /// `TextureHandle` は `NonZeroU32` だけを持ち、どの renderer のものかを持たない。
    /// `TextureStore` は renderer-local で id を 1 から採番するので、**preview が払い出した
    /// handle を main の scene に流すと別テクスチャに別名衝突する** (動画クリップの
    /// サムネイル位置に別の絵が出る、あるいは無言で描画 skip)。これは本 commit が
    /// [`TextureStore::new_starting_at`] の doc で「世代間で id を巻き戻すな」として
    /// 定義した欠陥クラスの **renderer 間版**。
    ///
    /// よって arrangement のサムネイル用 (main) と preview 合成用 (preview) は
    /// **それぞれの renderer で作った別々の handle** を持つ。`image_texture_cache` は
    /// main 専用 (arrangement が読む)、preview は自前の `image_textures` を持つ。
    ///
    /// CPU staging (`image_source_bgra`) はアップロード後に捨てる (メモリ SSoT はディスク)。
    /// preview を後から開いた場合は `sync_preview_window` が再 decode を起動する。
    fn drain_image_uploads(state: &mut RunnerState) {
        if state.app.media.pending_image_uploads.is_empty() {
            return;
        }
        let pending: Vec<_> = state.app.media.pending_image_uploads.drain(..).collect();
        for image_source_id in pending {
            let Some((w, h, bgra)) =
                state.app.media.image_source_bgra.remove(&image_source_id)
            else {
                continue; // already uploaded (= rapid undo path)
            };
            // main: arrangement クリップのサムネイル用。既に同 id の texture を持って
            // いれば作り直さない (preview を開いたときの再 staging で無駄に確保しない)。
            // 寸法が変わる再 import 等では旧 handle を destroy してから差し替える
            // (上書きするだけだと GPU 側 store に orphan が積み上がる)。
            let existing = state
                .app
                .ui_ephemeral
                .image_texture_cache
                .get(&image_source_id)
                .copied()
                .filter(|handle| state.renderer.texture_size(*handle) == Some((w, h)));
            let main_handle = match existing {
                Some(handle) => handle,
                None => {
                    if let Some(old) =
                        state.app.ui_ephemeral.image_texture_cache.remove(&image_source_id)
                    {
                        state.renderer.destroy_texture(old);
                    }
                    let handle = state.renderer.create_texture_bgra(w, h);
                    state
                        .app
                        .ui_ephemeral
                        .image_texture_cache
                        .insert(image_source_id, handle);
                    handle
                }
            };
            state.renderer.upload_texture_bgra(main_handle, &bgra);
            // preview: 合成用 (開いていれば)。preview 側は自前 map が SSoT。
            if let Some(preview) = state.preview.as_mut() {
                preview.upload_image_bgra(image_source_id, w, h, &bgra);
            }
        }
    }

    #[cfg(windows)]
    fn drive_preview_playback(state: &mut RunnerState) {
        // Step 1: always drain results first, even on throttled
        // frames. Decoded frames are precious — uploading them is
        // cheap (GPU memcpy) and keeps the texture fresh.
        Self::drain_preview_worker_results(state);

        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        // Throttle the request side. Use a small floor (24fps =
        // ~41ms) so a malformed project framerate (0 / NaN) still
        // permits some decoding.
        let frame_interval_ms = {
            let fps = state.app.song_doc.song().video_framerate.max(1.0);
            (1000.0 / fps).round().max(33.0) as u64
        };
        let now = Instant::now();
        if let Some(last) = state.last_preview_drive_at
            && now.duration_since(last).as_millis() < frame_interval_ms as u128
        {
            return;
        }
        state.last_preview_drive_at = Some(now);

        let Some(playhead_beat) = state.app.transport.playhead_beat.map(f64::from) else {
            preview.set_track_composites(Vec::new());
            return;
        };
        let active = crate::video_playback::VideoPlaybackEngine::active_sources_at(
            state.app.song_doc.song(),
            playhead_beat,
        );
        let project_dir = state
            .app
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));

        // Ask the worker to decode the center frame of each active source (the
        // libav BGRA sink is 1-frame-latest). Skipped when no video is active
        // (= the worker has nothing to decode), so an image-only / text-only
        // project doesn't request empty decodes.
        if !active.is_empty() {
            for frame_info in &active {
                let Some(abs_path) = resolve_video_path(
                    state.app.song_doc.song(),
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
                );
            }
        }

        // 動画フレーム / PiP 画像 / テキストを
        // **owning track ごと** に 1 枚の RGBA「トラック合成画」へ集約する。各 track の
        // 視覚アイテムを bucket に集め、track 順 (= z 順) に 1 TrackComposite として
        // preview に渡す。立ち絵 group の子画像は親 group の bucket へ吸収 (approach X)。
        // 効果も配置 transform も無い track は消費側が合成往復せず直接描く (plain track の
        // fast-path = 回帰なし・クリスプ・無コスト)。これで spatial 効果 (blur/歪み) が
        // 個別素材でなく「トラックの最終見た目 1 枚」に正しくかかる。
        use crate::group_compose::CompositeItem;
        let (proj_w, proj_h) = state.app.song_doc.song().video_resolution;
        let canvas = (proj_w.max(1) as f32, proj_h.max(1) as f32);
        let mut buckets: std::collections::HashMap<u32, Vec<CompositeItem>> =
            std::collections::HashMap::new();

        // 動画フレーム → owning track の bucket (canvas 内 aspect-fit normalized)。
        for frame_info in active {
            let Some(ring) = state.cached_rings.get(&frame_info.video_source_id) else {
                continue; // worker hasn't produced a ring yet
            };
            let Some(slot) = nearest_ring_slot(ring, frame_info.source_micros) else {
                continue; // ring is empty (= all slots failed to decode)
            };
            let key = (frame_info.video_source_id, slot.slot_idx);
            let Some((handle, _w, _h)) = preview.frame_textures.get(&key).copied() else {
                continue; // texture not yet imported for this slot
            };
            let dest = crate::group_compose::aspect_fit_norm(
                canvas,
                (slot.width as f32, slot.height as f32),
            );
            buckets.entry(frame_info.owning_track_id).or_default().push(
                CompositeItem::Quad { texture: handle, dest, alpha: frame_info.alpha, rotation_radians: 0.0 },
            );
        }

        // v19 (docs/plan_tachie_group_transform.md §5.6): visual group の active
        // transform を 1 回解決（group track id → resolved transform）。gate は
        // `group_has_visual_content`。transform / lane 未設定の visual group も identity
        // として含む。export と同一述語（SSoT）。
        let active_groups = crate::group_compose::active_visual_groups(
            state.app.song_doc.song(),
            playhead_beat,
            &state.app.transport.mod_scalars,
        );

        // PiP 画像 → 親が active group ならその group bucket へ吸収、さもなくば owning
        // track の bucket。
        let image_frames = crate::image_compose::active_image_sources_at(
            state.app.song_doc.song(),
            playhead_beat,
            &state.app.transport.mod_scalars,
        );
        for frame_info in image_frames {
            // **preview 自身の** texture を引く。`image_texture_cache` は main renderer 用
            // なので、ここで使うと renderer 間で id が別名衝突する (r.md #42 レビュー指摘)。
            let Some((handle, _, _)) =
                preview.image_textures.get(&frame_info.image_source_id).copied()
            else {
                continue; // texture not yet uploaded
            };
            let dims = state
                .app
                .song_doc.song()
                .media.image_sources
                .get(&frame_info.image_source_id)
                .map(|s| (s.width, s.height))
                .unwrap_or((0, 0));
            if dims.0 == 0 || dims.1 == 0 {
                continue;
            }
            let target_track = state
                .app
                .song_doc.song()
                .track_by_id(frame_info.owning_track_id)
                .and_then(|t| t.parent_group_id)
                .filter(|g| active_groups.contains_key(g))
                .unwrap_or(frame_info.owning_track_id);
            buckets.entry(target_track).or_default().push(CompositeItem::Quad {
                texture: handle,
                dest: (frame_info.x, frame_info.y, frame_info.w, frame_info.h),
                alpha: frame_info.alpha,
                rotation_radians: frame_info.rotation_radians,
            });
        }

        // テキスト → owning track の bucket (合成画に焼き込んで track 効果を乗せる)。
        let text_frames = crate::text_compose::active_text_sources_at(
            state.app.song_doc.song(),
            playhead_beat,
            &state.app.transport.mod_scalars,
        );
        for tf in text_frames {
            buckets.entry(tf.owning_track_id).or_default().push(CompositeItem::Text(tf));
        }

        // track 順 (bottom→top = rev) に TrackComposite を構築。group track は
        // 吸収した子 + group affine、通常 track は自分の視覚アイテム + identity 配置。
        // 選択中 group は children 空でも bounding box 用に emit。
        let mut composites: Vec<crate::group_compose::TrackComposite> = Vec::new();
        for track in state.app.song_doc.song().tracks.iter().rev() {
            let items = buckets.remove(&track.id).unwrap_or_default();
            // 配置 transform は **どのトラックでも** Transform device から
            // 解決（立ち絵 group も通常トラックも統一）。device が無ければ None = identity 配置。
            let transform = crate::video_fx::resolve_track_transform(
                state.app.song_doc.song(),
                track,
                playhead_beat,
                &state.app.transport.mod_scalars,
            );
            let selected = state.app.cursor_track_id() == Some(track.id);
            if items.is_empty() && !(transform.is_some() && selected) {
                continue;
            }
            let fx = crate::video_fx::resolve_track_effects(
                state.app.song_doc.song(),
                track,
                playhead_beat,
                &state.app.transport.mod_scalars,
            );
            composites.push(crate::group_compose::TrackComposite {
                track_id: track.id,
                items,
                transform,
                fx,
                selected,
            });
        }
        // 時間系効果（ノイズ/スキャンライン等）の `P.time`（秒）。
        // preview/export 一致のため wall-clock でなく song 時間。 tempo automation
        // がある曲は積分写像 (constant-bpm 線形だと export = TempoMap 積分と
        // 効果進行がズレる。 映像 source 時間の A4 と同じ扱い)。
        preview.fx_engine.set_time(common::tempo_map::song_beat_to_seconds(
            state.app.song_doc.song(),
            playhead_beat,
        ) as f32);
        preview.set_track_composites(composites);
        // マスター映像チェーン（master_fx_chain の映像 device）を解決して渡す。
        // 空でなければ preview が全トラック合成画を master canvas 1 枚に集約してから適用する。
        preview.set_master_fx(crate::video_fx::resolve_master_effects(
            state.app.song_doc.song(),
            playhead_beat,
            &state.app.transport.mod_scalars,
        ));
        // PiP rect の normalized 座標は project_resolution の letterbox
        // 内で展開される (= window resize しても画像 aspect が崩れない)。
        // Song.video_resolution を毎 frame 同期。
        preview.set_project_resolution(state.app.song_doc.song().video_resolution);

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
            .selected_clip_ref()
            .and_then(|cref| {
                let track = state.app.song_doc.song().tracks.get(cref.track as usize)?;
                let clip = track.clips.get(cref.clip as usize)?;
                let content = state.app.song_doc.song().clip_contents.get(&clip.content_id)?;
                if let Some(events) = content.image_events() {
                    let ev = events.first()?;
                    Some(((ev.x, ev.y, ev.w, ev.h), ev.rotation_radians))
                } else if let Some(events) = content.text_events() {
                    // (talk/v26) 字幕 (`builtin.video.subtitle`) device が刺さっている
                    // トラックの Text だけ画面に出る (`text_compose` の表示 gate と一致)。
                    // 出ない Text の選択枠 (縁取り + handle) を preview に出すと「刺して
                    // ないのに枠が出る」混乱になるので gate する (`docs/plan_voicevox_talk.md`)。
                    if track.has_subtitle_device() {
                        let ev = events.first()?;
                        Some(((ev.x, ev.y, ev.w, ev.h), ev.rotation_radians))
                    } else {
                        None
                    }
                } else {
                    None
                }
            });
        let (overlay_rect, rotation) = match overlay_info {
            Some((r, rot)) => (Some(r), rot),
            None => (None, 0.0),
        };
        // option A（plan_tachie_group_transform.md）: 選択中 clip が active
        // visual group の子（image）なら親 group の解決済み transform を渡し、
        // 選択オーバーレイを group 空間へ写像して追従させる。text overlay は
        // group 合成パス（`set_text_layers` 別経路）に乗らないので写像しない。
        // 判定条件（owning track の parent_group_id ∈ active_groups）は子を
        // group へ bucket する partition（上の image_frames ループ）と同一。
        let selection_group = state.app.selected_clip_ref().and_then(|cref| {
            let track = state.app.song_doc.song().tracks.get(cref.track as usize)?;
            let clip = track.clips.get(cref.clip as usize)?;
            let content = state.app.song_doc.song().clip_contents.get(&clip.content_id)?;
            // text overlay は group 合成パスに乗らないので image の子のみ写像。
            content.image_events()?;
            let gid = track.parent_group_id?;
            active_groups.get(&gid).copied()
        });
        preview.set_selection_overlay(overlay_rect, rotation, selection_group);
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
                // libav is a 1-frame-latest BGRA sink, so the worker only fills
                // slot 0 (docs/plan_video_decode_unify.md).
                let (frame_bytes, w, h, slot_idx) = (
                    slot.frame.bgra.len(),
                    slot.frame.width,
                    slot.frame.height,
                    0_u8,
                );
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
                        variant = "bgra",
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
        renderer: &mut Renderer<WinitWindow>,
    ) {
        if app.media.pending_thumbnail_uploads.is_empty() {
            return;
        }
        let pending: Vec<_> = app.media.pending_thumbnail_uploads.drain(..).collect();
        for video_source_id in pending {
            // It's possible the source was unloaded between the
            // import and the next frame (= rapid undo path). Just
            // skip — the GPU is the source of truth and a missing
            // RGBA staging means there's nothing to upload.
            let Some((w, h, rgba)) = app.media.video_thumbnail_rgba.remove(&video_source_id)
            else {
                continue;
            };
            // 同 id の旧 texture は必ず destroy してから差し替える。 サムネイルは
            // **ネイティブ解像度** (`extract_thumbnail` は downscale しない) なので
            // 4K なら 1 枚 33MB。 上書きするだけだと再 import / project 開き直しの
            // たびに GPU 側 store に orphan が積み上がる。
            if let Some(old) = app.ui_ephemeral.video_texture_cache.remove(&video_source_id) {
                renderer.destroy_texture(old);
            }
            let handle = renderer.create_texture(w, h);
            renderer.upload_texture_rgba(handle, rgba.as_slice());
            // 同 id への再 upload では旧 handle を必ず解放する (insert で
            // 上書きすると GPU テクスチャがそのまま漏れる)。
            if let Some(old) = app
                .ui_ephemeral
                .video_texture_cache
                .insert(video_source_id, handle)
            {
                renderer.destroy_texture(old);
            }
        }
    }

    /// r.md #42: `AppData` が参照を捨てた **main renderer** の `TextureHandle` を
    /// 実際に解放する。
    ///
    /// `AppData` は `Renderer` を持たない (持たせるとモデルが GPU に依存する) ので、
    /// 破棄は「予約を積む → runner が drain して destroy」 の 2 段で行う。
    /// これが無いと cache の `clear()` は参照を捨てるだけで GPU 側 store に
    /// entry が残り続け、 プロジェクトを開き直すたびに VRAM が単調増加する。
    ///
    /// project 切替 (`reset_song_scoped_state`) と GPU 復旧
    /// (`rebuild_gpu_derived_caches`) の **両方**がこの 1 本の口を使う
    /// (後者は project_id が変わらないので世代印では表現できない)。
    fn drain_texture_destroys(app: &mut AppData, renderer: &mut Renderer<WinitWindow>) {
        for handle in app.ui_ephemeral.pending_texture_destroys.drain(..) {
            renderer.destroy_texture(handle);
        }
    }

    /// プロジェクトが切り替わったら GPU 側の Song スコープ状態を解放する。
    ///
    /// `VideoSourceId` / `ImageSourceId` は Song スコープの名前 (project ごとに
    /// 1 から再採番) なので、テクスチャを id で引き継ぐと **前 project の
    /// サムネイル・画像・動画フレームが新 project のクリップに出る**。
    ///
    /// ここが担当するのは **`TextureHandle` として `AppData` から渡せないもの**
    /// だけ: preview window が自前で所有するフレーム / 画像テクスチャと、それを
    /// 指す decode ring スナップショット。main renderer 上のテクスチャは
    /// `reset_song_scoped_state` が破棄予約へ積み、[`Self::drain_texture_destroys`]
    /// が解放する (破棄の口を 2 つ持たない = 二重解放も取りこぼしも起きない)。
    fn release_project_scoped_gpu_state(state: &mut RunnerState) {
        let generation = state.app.ui_ephemeral.project_generation;
        if generation == state.released_project_generation {
            return;
        }
        state.released_project_generation = generation;
        // preview 側のフレーム / 画像テクスチャ (`(source_id, slot)` keyed) と、
        // それを指す ring スナップショットは対で捨てる。
        if let Some(preview) = state.preview.as_mut() {
            preview.clear_all();
        }
        #[cfg(windows)]
        state.cached_rings.clear();
    }
}

/// r.md #42: GPU 再初期化が [`GPU_RECOVERY_GIVEUP`] 以上失敗し続けたときに
/// 「保存して再起動してください」 を伝える **OS 描画の**メッセージボックス。
///
/// 自前のモーダルは `Renderer` が死んでいる間は 1 ピクセルも描けないので、 この状況で
/// ユーザーに何かを伝える手段は OS 側が描くダイアログしかない。 8/1 のログではこの
/// 通知が無かったために、 ユーザーは ✕ を 4 回押して諦め強制終了している。
///
/// **別スレッド + owner-modal** で開く (`spawn_file_dialog` と同じ理由: 同期に開くと
/// dialog 自身のメッセージポンプが GUI スレッドで回り、 復旧リトライが止まる)。
/// 作業内容は既存の autosave (90 秒間隔) が守っているので、 勝手な保存や終了はしない。
fn spawn_gpu_giveup_dialog(parent_hwnd: Option<isize>) {
    let dialog = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("GPU を再初期化できません")
        .set_description(
            "グラフィックデバイスが応答しないため画面を再描画できません。\n\
             プロジェクトを保存して daw_01 を再起動してください。\n\
             (再初期化は背後で試行を続けます)",
        )
        .set_buttons(rfd::MessageButtons::Ok);
    #[cfg(windows)]
    let dialog = match parent_hwnd {
        Some(hwnd) if hwnd != 0 => dialog.set_parent(&crate::app_types::Win32Parent { hwnd }),
        _ => dialog,
    };
    #[cfg(not(windows))]
    let _ = parent_hwnd;
    std::thread::spawn(move || {
        dialog.show();
    });
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
    let src = song.media.video_sources.get(&video_source_id)?;
    match &src.path {
        common::model::VideoSourcePath::Absolute(p) => Some(p.clone()),
        common::model::VideoSourcePath::ProjectRelative(rel) => {
            project_dir.map(|d| d.join(rel))
        }
    }
}

// map_button / map_state / map_phys_key / query_cursor_pos_in_window は
// daw_ui_platform::winit_backend に一本化 (Phase 4 で手写しミラーを撤去)。


#[cfg(test)]
mod tests {
    use super::*;
    use daw_ui_renderer::RenderError;

    fn err() -> RenderError {
        RenderError::Validation("test".into())
    }

    /// r.md #42 レビュー指摘: main の成功で preview の抑制状態が消えると、
    /// preview だけが恒常エラーのときレート制限が完全に無効になる (60 行/秒)。
    ///
    /// `RenderErrorLog` は窓ごとに独立していて、`reset()` は **その窓の** 状態しか
    /// 触らないことを固定する。
    #[test]
    fn render_error_log_is_per_window_and_reset_does_not_leak() {
        let t0 = Instant::now();
        let mut main_log = RenderErrorLog::default();
        let mut preview_log = RenderErrorLog::default();

        // preview の 1 件目は出力される。
        assert!(preview_log.record(t0, "preview render error", &err()));
        // main が成功して reset しても、preview の抑制状態は消えない。
        main_log.reset();
        // 同じ 1 秒窓の中の 2 件目は抑制される (= ここが以前は素通りしていた)。
        let t1 = t0 + std::time::Duration::from_millis(16);
        assert!(!preview_log.record(t1, "preview render error", &err()));
        assert!(!preview_log.record(
            t0 + std::time::Duration::from_millis(999),
            "preview render error",
            &err()
        ));
        // 窓を跨いだら再び出力し、抑制件数が畳まれる。
        let t2 = t0 + RenderErrorLog::INTERVAL;
        assert!(preview_log.record(t2, "preview render error", &err()));
        assert_eq!(preview_log.suppressed, 0);
    }

    /// 自分の窓が成功したときは抑制状態を畳む (= 次のエラーは即座に 1 行出る)。
    #[test]
    fn render_error_log_reset_clears_own_window() {
        let t0 = Instant::now();
        let mut log = RenderErrorLog::default();
        assert!(log.record(t0, "render error", &err()));
        assert!(!log.record(t0 + std::time::Duration::from_millis(1), "render error", &err()));
        log.reset();
        assert!(log.record(t0 + std::time::Duration::from_millis(2), "render error", &err()));
    }

    /// GPU 復旧の backoff は「毎フレーム再試行」 に戻らず、上限で頭打ちになること
    /// (present しないと vsync 律速が消えるので、無制限再試行は 860fps スピンになる)。
    #[test]
    fn gpu_retry_backoff_is_monotonic_and_capped() {
        let mut prev = std::time::Duration::ZERO;
        for attempts in 0..10 {
            let d = gpu_retry_backoff(attempts);
            assert!(d >= std::time::Duration::from_millis(250), "毎フレーム再試行に戻らない");
            assert!(d <= std::time::Duration::from_secs(2), "上限 2 秒");
            if attempts <= 3 {
                assert!(d >= prev, "段階的に伸びる");
            }
            prev = d;
        }
        assert_eq!(gpu_retry_backoff(3), gpu_retry_backoff(100), "上限で頭打ち");
    }
}
