//! M14 Phase 104 (daw_01 #075): arrangement の **track header pane 上**でもマウスホイール縦
//! スクロール / 縦ズームが効くことを end-to-end で検証する。
//!
//! 仕様 (#075):
//! - ruler 下の content 全域 (= header pane + lanes canvas) で wheel を取得。
//! - plain wheel → 縦スクロール (`SetTrackTop`)。 header / lanes どちらの上でも同一挙動。
//! - Alt+wheel → 縦ズーム (`SetTrackRowH`)。 同上 (header 上でも効く)。
//! - Ctrl (zoom_x) / Shift (scroll_x) は時間軸操作なので **header 上では無視** (lanes 上は従来どおり)。
//!
//! `automation_point_edit.rs` と同 pattern で `UiHost::frame` を直接呼び `PointerFrame.scroll_delta`
//! を流し、 発行された `ArrangementEditRequest` を観測する。

use daw_ui_core::{
    ArrangementEditRequest, ArrangementStyle, ArrangementTrack, ArrangementView, Edit, FrameInput,
    PointerFrame, SnapConfig, TrackKind, UiHost,
};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Rect, Scene};
use std::sync::Arc;

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };

// view header_w=200 → header pane x range = [0, 200)、 lanes x range = [200, 800)。
const HEADER_X: f32 = 100.0; // header pane 内
const LANES_X: f32 = 400.0; // lanes canvas 内
const CONTENT_Y: f32 = 300.0; // ruler (h=20) より下

/// 観測 model: scroll/zoom 系 EditRequest だけ貯めて assert する。
#[derive(Default)]
struct ObsModel {
    tracks: Vec<ArrangementTrack>,
    track_tops: Vec<f32>,
    zoom_x: Vec<f32>,
    scroll_x: Vec<f64>,
    row_h: Vec<f32>,
}

fn make_track(id: u32) -> ArrangementTrack {
    ArrangementTrack {
        id,
        name: Arc::from(format!("t{id}")),
        muted: false,
        solo: false,
        armed: false,
        clips: Vec::new(),
        volume: 1.0,
        parent_id: None,
        depth: 0,
        collapsed: false,
        kind: TrackKind::Audio,
        automation_lanes_collapsed: true,
        automation_lanes: Vec::new(),
        row_h: None,
        color: None,
    }
}

fn make_view() -> ArrangementView {
    ArrangementView {
        start_beat: 0.0,
        len_beats: 16.0,
        track_top: 0.0,
        tracks_visible: 8.0,
        track_row_h: 32.0,
        header_w: 200.0,
        ruler_h: 20.0,
        playhead_beat: None,
        loop_range: None,
        data_generation: 0,
        bpm: 120.0,
        time_sig: (4, 4),
        snap: SnapConfig::OFF,
    }
}

/// `scroll_delta = (0, dy)` の wheel frame (modifiers 付き)。
fn wheel(x: f32, y: f32, dy: f32, modifiers: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        scroll_delta: (0.0, dy),
        modifiers,
        ..PointerFrame::default()
    }
}

fn run_frame(host: &mut UiHost<ObsModel>, m: &mut ObsModel, input: FrameInput) {
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: WIDGET_RECT.w as u32,
        height: WIDGET_RECT.h as u32,
    };
    let view = make_view();
    let style = ArrangementStyle::default();
    host.frame(m, &mut scene, screen, input, |model, ui| {
        ui.arrangement(
            "arr",
            WIDGET_RECT,
            &model.tracks,
            view,
            &[],
            &[],
            &[],
            &[],
            &style,
            None,
            |req| match req {
                ArrangementEditRequest::SetTrackTop(t) => Edit::mutate(move |mm: &mut ObsModel| {
                    mm.track_tops.push(t);
                }),
                ArrangementEditRequest::SetZoomX(z) => Edit::mutate(move |mm: &mut ObsModel| {
                    mm.zoom_x.push(z);
                }),
                ArrangementEditRequest::SetScrollX(s) => Edit::mutate(move |mm: &mut ObsModel| {
                    mm.scroll_x.push(s);
                }),
                ArrangementEditRequest::SetTrackRowH(h) => Edit::mutate(move |mm: &mut ObsModel| {
                    mm.row_h.push(h);
                }),
                _ => Edit::mutate(|_| {}),
            },
        );
    });
}

fn fresh() -> (UiHost<ObsModel>, ObsModel) {
    let m = ObsModel {
        tracks: vec![make_track(1), make_track(2)],
        ..ObsModel::default()
    };
    (UiHost::no_redraw(), m)
}

// ---- plain wheel: header / lanes どちらでも縦スクロール ----

#[test]
fn plain_wheel_over_header_pane_scrolls_vertically() {
    let (mut host, mut m) = fresh();
    run_frame(&mut host, &mut m, FrameInput {
        pointer: wheel(HEADER_X, CONTENT_Y, -1.0, Modifiers::empty()),
        ..FrameInput::default()
    });
    // M14 Phase 115 (daw_01 #088): ×8 二重スケール撤去後は scroll_area と同じく px delta 直使用。
    // new_top = (track_top(0) - dy(-1)).max(0) = 1。 header 上でも縦スクロールが発火する。
    assert_eq!(m.track_tops, vec![1.0], "header 上 plain wheel で SetTrackTop が発火");
    assert!(m.zoom_x.is_empty() && m.scroll_x.is_empty() && m.row_h.is_empty());
}

#[test]
fn plain_wheel_over_lanes_still_scrolls_vertically() {
    let (mut host, mut m) = fresh();
    run_frame(&mut host, &mut m, FrameInput {
        pointer: wheel(LANES_X, CONTENT_Y, -1.0, Modifiers::empty()),
        ..FrameInput::default()
    });
    // M14 Phase 115 (daw_01 #088): ×8 撤去で px delta 直使用 (lanes 上も header と同一量 = 1)。
    assert_eq!(m.track_tops, vec![1.0], "lanes 上 plain wheel は従来どおり SetTrackTop");
}

// ---- Alt+wheel: header / lanes どちらでも縦ズーム ----

#[test]
fn alt_wheel_over_header_pane_zooms_rows() {
    let (mut host, mut m) = fresh();
    let alt = Modifiers { alt: true, ..Modifiers::default() };
    run_frame(&mut host, &mut m, FrameInput {
        pointer: wheel(HEADER_X, CONTENT_Y, -1.0, alt),
        ..FrameInput::default()
    });
    assert_eq!(m.row_h.len(), 1, "header 上 Alt+wheel で SetTrackRowH が発火");
    // factor = exp(-0.0015) ≈ 0.9985 → new_h ≈ 31.95 (< 32)。 縦ズームが効いている。
    assert!(m.row_h[0] < 32.0 && m.row_h[0] > 31.0, "row_h ≈ 31.95、 actual {}", m.row_h[0]);
    // spec「マウス Y を anchor」: Alt+wheel は画面位置維持のため SetTrackTop も同 frame で発火する
    // (track_row_h>0 かつ pointer.pos=Some)。 header 上でも anchor 維持挙動が効くことを pin する。
    assert_eq!(m.track_tops.len(), 1, "header 上 Alt+wheel は anchor 維持の SetTrackTop も発火");
    // 横軸操作は発火しない。
    assert!(m.zoom_x.is_empty() && m.scroll_x.is_empty());
}

#[test]
fn alt_wheel_over_lanes_still_zooms_rows() {
    let (mut host, mut m) = fresh();
    let alt = Modifiers { alt: true, ..Modifiers::default() };
    run_frame(&mut host, &mut m, FrameInput {
        pointer: wheel(LANES_X, CONTENT_Y, -1.0, alt),
        ..FrameInput::default()
    });
    assert_eq!(m.row_h.len(), 1, "lanes 上 Alt+wheel は従来どおり SetTrackRowH");
}

// ---- Ctrl (zoom_x): header 上は無視、 lanes 上は従来どおり ----

#[test]
fn ctrl_wheel_over_header_pane_is_ignored() {
    let (mut host, mut m) = fresh();
    let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
    run_frame(&mut host, &mut m, FrameInput {
        pointer: wheel(HEADER_X, CONTENT_Y, -1.0, ctrl),
        ..FrameInput::default()
    });
    assert!(
        m.zoom_x.is_empty() && m.scroll_x.is_empty() && m.track_tops.is_empty(),
        "header 上 Ctrl+wheel は zoom_x も track_top も発火しない"
    );
}

#[test]
fn ctrl_wheel_over_lanes_still_zooms_x() {
    let (mut host, mut m) = fresh();
    let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
    run_frame(&mut host, &mut m, FrameInput {
        pointer: wheel(LANES_X, CONTENT_Y, -1.0, ctrl),
        ..FrameInput::default()
    });
    assert_eq!(m.zoom_x.len(), 1, "lanes 上 Ctrl+wheel は従来どおり SetZoomX");
    // mouse anchor zoom なので同 frame で SetScrollX も発行される (lanes 上のみ)。
    assert_eq!(m.scroll_x.len(), 1, "lanes 上 Ctrl+wheel は anchor 維持の SetScrollX も発火");
}

// ---- Shift (scroll_x): header 上は無視、 lanes 上は従来どおり ----

#[test]
fn shift_wheel_over_header_pane_is_ignored() {
    let (mut host, mut m) = fresh();
    let shift = Modifiers { shift: true, ..Modifiers::default() };
    run_frame(&mut host, &mut m, FrameInput {
        pointer: wheel(HEADER_X, CONTENT_Y, -1.0, shift),
        ..FrameInput::default()
    });
    assert!(
        m.scroll_x.is_empty() && m.track_tops.is_empty(),
        "header 上 Shift+wheel は scroll_x も track_top も発火しない"
    );
}

#[test]
fn shift_wheel_over_lanes_still_scrolls_x() {
    let (mut host, mut m) = fresh();
    let shift = Modifiers { shift: true, ..Modifiers::default() };
    run_frame(&mut host, &mut m, FrameInput {
        pointer: wheel(LANES_X, CONTENT_Y, -1.0, shift),
        ..FrameInput::default()
    });
    assert_eq!(m.scroll_x.len(), 1, "lanes 上 Shift+wheel は従来どおり SetScrollX");
}
