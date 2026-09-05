//! メインウィンドウの **画面上の置き場所** — 起動時の復元位置の妥当性検査
//! (r.md #106)、モニタ構成変更後の退避、終了時の geometry 保存。
//!
//! 「どのモニタにも掛かっていない窓を主モニタへ寄せる」判定は
//! [`crate::window_state::place_on_screen`] (純関数、テスト済み) の 1 本で、ここは
//! winit の monitor / window API との橋渡しだけを持つ。

use daw_ui_platform::WinitWindow;
use winit::dpi::PhysicalPosition as WinitPhysPos;
use winit::event_loop::ActiveEventLoop;
use winit::monitor::MonitorHandle;
use winit::window::WindowAttributes;

use crate::window_state::{PreviewGeometry, ScreenRect, place_on_screen};

/// winit の monitor を screen 座標矩形へ (physical px)。
fn monitor_rect(m: &MonitorHandle) -> ScreenRect {
    let p = m.position();
    let s = m.size();
    ScreenRect {
        x: p.x,
        y: p.y,
        w: i32::try_from(s.width).unwrap_or(i32::MAX),
        h: i32::try_from(s.height).unwrap_or(i32::MAX),
    }
}

/// いまのモニタ一覧と主モニタ (無ければ先頭)。モニタが 1 つも無ければ `None`。
fn monitor_layout(
    monitors: impl Iterator<Item = MonitorHandle>,
    primary: Option<MonitorHandle>,
) -> Option<(Vec<ScreenRect>, MonitorHandle)> {
    let handles: Vec<MonitorHandle> = monitors.collect();
    let primary = primary.or_else(|| handles.first().cloned())?;
    Some((handles.iter().map(monitor_rect).collect(), primary))
}

/// r.md #106 (起動時): 保存位置 / サイズがいまのモニタ構成で画面外なら、窓を作る前に
/// `attrs` を主モニタの中へ寄せる (作ってから動かすと一瞬画面外に出てから戻る)。
pub fn place_attrs_on_screen(attrs: &mut WindowAttributes, event_loop: &ActiveEventLoop) {
    let Some((monitors, primary)) =
        monitor_layout(event_loop.available_monitors(), event_loop.primary_monitor())
    else {
        return;
    };
    let (Some(pos), Some(size)) = (attrs.position, attrs.inner_size) else {
        return;
    };
    // 保存サイズは logical。画面外の窓は主モニタへ移すので主モニタの scale で物理化する。
    let scale = primary.scale_factor();
    let pos = pos.to_physical::<i32>(scale);
    let size = size.to_physical::<u32>(scale);
    let win = ScreenRect {
        x: pos.x,
        y: pos.y,
        w: i32::try_from(size.width).unwrap_or(i32::MAX),
        h: i32::try_from(size.height).unwrap_or(i32::MAX),
    };
    let Some(new) = place_on_screen(win, &monitors, monitor_rect(&primary)) else {
        return;
    };
    tracing::info!(?win, ?new, "saved window rect is off-screen; placing on primary monitor");
    attrs.position = Some(WinitPhysPos::new(new.x, new.y).into());
    attrs.inner_size = Some(
        winit::dpi::PhysicalSize::new(new.w.unsigned_abs(), new.h.unsigned_abs()).into(),
    );
}

/// r.md #106 (稼働中): モニタ構成が変わって窓がどのモニタにも掛からなくなったら
/// 主モニタへ戻す (main 窓・Video Preview 窓の両方)。最大化中は一度解いて置き直し、
/// また最大化する (最大化のまま `set_outer_position` しても OS に無視される)。
pub fn ensure_window_on_screen(win: &winit::window::Window) {
    let Some((monitors, primary)) = monitor_layout(win.available_monitors(), win.primary_monitor())
    else {
        return;
    };
    let Ok(pos) = win.outer_position() else {
        return;
    };
    let size = win.outer_size();
    let rect = ScreenRect {
        x: pos.x,
        y: pos.y,
        w: i32::try_from(size.width).unwrap_or(i32::MAX),
        h: i32::try_from(size.height).unwrap_or(i32::MAX),
    };
    let Some(new) = place_on_screen(rect, &monitors, monitor_rect(&primary)) else {
        return;
    };
    tracing::info!(?rect, ?new, "main window off-screen after display change; moving onto primary monitor");
    let was_maximized = win.is_maximized();
    if was_maximized {
        win.set_maximized(false);
    }
    win.set_outer_position(WinitPhysPos::new(new.x, new.y));
    if new.w < rect.w || new.h < rect.h {
        let _ = win.request_inner_size(winit::dpi::PhysicalSize::new(
            new.w.unsigned_abs(),
            new.h.unsigned_abs(),
        ));
    }
    if was_maximized {
        win.set_maximized(true);
    }
}

/// r.md #107: Video Preview 窓のいまの geometry (位置 = physical / サイズ = logical)。
/// 閉じる直前と終了時に読み、 `window_state.json` の `preview` として保存する。
#[must_use]
pub fn preview_geometry_of(win: &winit::window::Window) -> Option<PreviewGeometry> {
    let pos = win.outer_position().ok()?;
    let size = win.inner_size();
    let scale = win.scale_factor();
    Some(PreviewGeometry {
        x: pos.x,
        y: pos.y,
        width: f64::from(size.width) / scale,
        height: f64::from(size.height) / scale,
    })
}

/// メインウィンドウ (+ 直近の Video Preview 窓) の geometry を `window_state.json` に書く。
pub fn save_main_window_state(
    window: &WinitWindow,
    app_dirs: Option<&common::app_dirs::AppDirs>,
    preview: Option<PreviewGeometry>,
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
        preview,
    };
    if let Err(e) = crate::window_state::save(&path, &state) {
        tracing::warn!(error = ?e, "failed to save window_state.json");
    }
}
