//! M14 Phase 63n-1 (#028): track 行右端の lane disclosure ▶/▼ click が
//! `ArrangementEditRequest::ToggleTrackAutomationCollapsed { track }` を発火することを
//! end-to-end で検証する。 また `automation_lanes` が空の track では disclosure を描画せず
//! click にも反応しないことを negative test で固定する。

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use daw_ui_core::{
    ArrangementAutomationLane, ArrangementClip, ArrangementEditRequest, ArrangementStyle,
    ArrangementTrack, ArrangementView, Edit, FrameInput, PointerFrame, SnapConfig, UiHost,
    lane_disclosure_rect_for,
};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Color, Rect, Scene};

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };

/// disclosure click を捕まえる minimal Model。
struct LaneToggleModel {
    tracks: Vec<ArrangementTrack>,
    last_toggle: Option<u32>,
}

fn make_lane(id: u32, label: &str) -> ArrangementAutomationLane {
    ArrangementAutomationLane {
        id,
        label: Arc::from(label),
        icon_glyph: 'V',
        color: Color::rgb(0.55, 0.85, 1.0),
        enabled: true,
        visible: true,
        height_px: 60,
        default_value_norm: 0.5,
        clips: Vec::new(),
    }
}

fn make_track(id: u32, lanes: Vec<ArrangementAutomationLane>) -> ArrangementTrack {
    ArrangementTrack {
        id,
        name: Arc::from(format!("t{id}")),
        muted: false,
        solo: false,
        clips: Vec::<ArrangementClip>::new(),
        volume: 1.0,
        parent_id: None,
        depth: 0,
        collapsed: false,
        automation_lanes_collapsed: true, // 起動時は全 collapsed
        automation_lanes: lanes,
    }
}

fn make_track_expanded(id: u32, lanes: Vec<ArrangementAutomationLane>) -> ArrangementTrack {
    ArrangementTrack {
        id,
        name: Arc::from(format!("t{id}")),
        muted: false,
        solo: false,
        clips: Vec::<ArrangementClip>::new(),
        volume: 1.0,
        parent_id: None,
        depth: 0,
        collapsed: false,
        automation_lanes_collapsed: false, // expanded で start
        automation_lanes: lanes,
    }
}

fn make_view() -> ArrangementView {
    ArrangementView {
        start_beat: 0.0,
        len_beats: 16.0,
        track_top: 0.0,
        tracks_visible: 8.0,
        track_row_h: 32.0,
        // header_w を 200px 固定 → disclosure rect が track header (= 0..200) の右端に出る。
        header_w: 200.0,
        ruler_h: 0.0,
        playhead_beat: None,
        loop_range: None,
        data_generation: 0,
        bpm: 120.0,
        time_sig: (4, 4),
        snap: SnapConfig::OFF,
    }
}

fn pointer_press(x: f32, y: f32) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers: Modifiers::empty(),
        ..PointerFrame::default()
    }
}

fn pointer_release(x: f32, y: f32) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_released: true,
        modifiers: Modifiers::empty(),
        ..PointerFrame::default()
    }
}

fn run_arrangement_frame(
    host: &mut UiHost<LaneToggleModel>,
    m: &mut LaneToggleModel,
    input: FrameInput,
) {
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
            &style,
            |req| match req {
                ArrangementEditRequest::ToggleTrackAutomationCollapsed { track } => {
                    Edit::mutate(move |mm: &mut LaneToggleModel| {
                        mm.last_toggle = Some(track);
                    })
                }
                _ => Edit::mutate(|_| {}),
            },
        );
    });
}

/// disclosure ▶ click → `ToggleTrackAutomationCollapsed { track }` が 1 度発火する。
#[test]
fn lane_disclosure_click_emits_toggle_track_automation_collapsed() {
    let mut host: UiHost<LaneToggleModel> = UiHost::no_redraw();
    let mut m = LaneToggleModel {
        tracks: vec![make_track(1, vec![make_lane(1, "Volume")])],
        last_toggle: None,
    };
    let style = ArrangementStyle::default();
    let view = make_view();
    // track 0 の row = (x=0, y=0, w=200, h=32) → lane disclosure rect は右端 (size = 12px、 pad 4px)
    let row0 = Rect { x: 0.0, y: 0.0, w: view.header_w, h: view.track_row_h };
    let disc = lane_disclosure_rect_for(row0, &style);
    let cx = disc.x + disc.w * 0.5;
    let cy = disc.y + disc.h * 0.5;
    // press → release (single click)
    run_arrangement_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(cx, cy),
            ..FrameInput::default()
        },
    );
    run_arrangement_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(cx, cy),
            ..FrameInput::default()
        },
    );
    assert_eq!(m.last_toggle, Some(1), "lane disclosure click が track 1 の toggle を発火する");
}

/// `automation_lanes` が空の track では disclosure を描画せず → 同位置の click でも発火しない。
#[test]
fn no_lane_track_does_not_emit_toggle_on_disclosure_position() {
    let mut host: UiHost<LaneToggleModel> = UiHost::no_redraw();
    let mut m = LaneToggleModel {
        tracks: vec![make_track(1, vec![])], // lane 0 個 (= disclosure 描画されない)
        last_toggle: None,
    };
    let style = ArrangementStyle::default();
    let view = make_view();
    let row0 = Rect { x: 0.0, y: 0.0, w: view.header_w, h: view.track_row_h };
    let disc = lane_disclosure_rect_for(row0, &style);
    let cx = disc.x + disc.w * 0.5;
    let cy = disc.y + disc.h * 0.5;
    run_arrangement_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(cx, cy),
            ..FrameInput::default()
        },
    );
    run_arrangement_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(cx, cy),
            ..FrameInput::default()
        },
    );
    assert_eq!(m.last_toggle, None, "lane なし track では disclosure click が発火しない");
}

/// 別 track の disclosure を click → 当該 track の id だけが返る (隣の track は無関係)。
#[test]
fn disclosure_click_targets_clicked_track_only() {
    let mut host: UiHost<LaneToggleModel> = UiHost::no_redraw();
    let mut m = LaneToggleModel {
        tracks: vec![
            make_track(10, vec![make_lane(1, "Volume")]),
            make_track(20, vec![make_lane(1, "Volume")]),
        ],
        last_toggle: None,
    };
    let style = ArrangementStyle::default();
    let view = make_view();
    // track 1 の row = y=32 (= 0 + 1 * row_h)
    let row1 = Rect {
        x: 0.0,
        y: view.track_row_h,
        w: view.header_w,
        h: view.track_row_h,
    };
    let disc = lane_disclosure_rect_for(row1, &style);
    let cx = disc.x + disc.w * 0.5;
    let cy = disc.y + disc.h * 0.5;
    run_arrangement_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(cx, cy),
            ..FrameInput::default()
        },
    );
    run_arrangement_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(cx, cy),
            ..FrameInput::default()
        },
    );
    assert_eq!(m.last_toggle, Some(20), "track 1 (id=20) の disclosure click が発火する");
}

/// **expanded** 状態 (= ▼ 表示) の disclosure を click → collapse 方向の `ToggleTrackAutomationCollapsed`
/// が発火する (= 双方向に動作する)。 user feedback で「▼ で lane が畳まれない」 報告があった bug の
/// 回帰テスト (Phase 63n-2 follow-up)。
#[test]
fn lane_disclosure_click_in_expanded_state_emits_toggle() {
    let mut host: UiHost<LaneToggleModel> = UiHost::no_redraw();
    let mut m = LaneToggleModel {
        // expanded で start (= ▼ が表示されている状態)
        tracks: vec![make_track_expanded(1, vec![make_lane(1, "Volume")])],
        last_toggle: None,
    };
    let style = ArrangementStyle::default();
    let view = make_view();
    // disclosure rect は track 行右端の正方形 (lane が expanded でも track row の y range 内、 track_row_h)
    let row0 = Rect { x: 0.0, y: 0.0, w: view.header_w, h: view.track_row_h };
    let disc = lane_disclosure_rect_for(row0, &style);
    let cx = disc.x + disc.w * 0.5;
    let cy = disc.y + disc.h * 0.5;
    run_arrangement_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(cx, cy),
            ..FrameInput::default()
        },
    );
    run_arrangement_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(cx, cy),
            ..FrameInput::default()
        },
    );
    assert_eq!(
        m.last_toggle,
        Some(1),
        "expanded 状態 (▼) の disclosure click が collapse 方向の toggle を発火する"
    );
}
