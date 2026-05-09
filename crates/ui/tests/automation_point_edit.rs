//! M14 Phase 63n-2 (#028): lane header button (★/👁/✕) と lane body 内 point の add/move/delete +
//! curve type popup_request (右クリック) を end-to-end で検証する。
//!
//! Phase 63n-1 で追加された disclosure click test (`automation_lane_collapse.rs`) と同 pattern で、
//! `UiHost::frame` を直接呼んで `PointerFrame` を流し、 EditRequest の発行を観測する。

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use daw_ui_core::{
    ArrangementAutomationClip, ArrangementAutomationLane, ArrangementAutomationPoint,
    ArrangementClip, ArrangementCurveKind, ArrangementEditRequest, ArrangementStyle,
    ArrangementTrack, ArrangementView, AutomationClipKey, AutomationLaneKey, AutomationPointKey,
    Edit, FrameInput, MoveAutomationPointDelta, PointerFrame, SnapConfig, UiHost,
    automation_lane_header_layout, automation_point_at, visible_track_row_tops,
};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Color, Rect, Scene};

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };

/// 観測 model: 各種 EditRequest を vec に貯めて assert する。
#[derive(Default)]
struct ObsModel {
    tracks: Vec<ArrangementTrack>,
    lane_enabled: Vec<(AutomationLaneKey, bool)>,
    lane_visible: Vec<(AutomationLaneKey, bool)>,
    lane_default: Vec<(AutomationLaneKey, f32, f32)>,
    deleted_lanes: Vec<AutomationLaneKey>,
    added_points: Vec<(AutomationClipKey, f64, f32)>,
    moved_points: Vec<MoveAutomationPointDelta>,
    deleted_points: Vec<AutomationPointKey>,
    set_curves: Vec<(AutomationPointKey, ArrangementCurveKind, ArrangementCurveKind)>,
    /// `automation_point_rects` を観測 (毎 frame、 描画順に並ぶ)。
    point_rects: Vec<(AutomationPointKey, Rect)>,
}

fn make_lane(id: u32, enabled: bool) -> ArrangementAutomationLane {
    ArrangementAutomationLane {
        id,
        label: Arc::from(format!("Lane{id}")),
        icon_glyph: 'V',
        color: Color::rgb(0.55, 0.85, 1.0),
        enabled,
        visible: true,
        height_px: 60,
        default_value_norm: 0.5,
        clips: vec![ArrangementAutomationClip {
            id: 100,
            start_beat: 0.0,
            len_beats: 16.0,
            name: Arc::from("clip"),
            points: vec![
                ArrangementAutomationPoint {
                    time_beat: 0.0,
                    value_norm: 0.8,
                    curve: ArrangementCurveKind::Linear,
                },
                ArrangementAutomationPoint {
                    time_beat: 4.0,
                    value_norm: 0.3,
                    curve: ArrangementCurveKind::Linear,
                },
                ArrangementAutomationPoint {
                    time_beat: 8.0,
                    value_norm: 0.6,
                    curve: ArrangementCurveKind::Bezier { tension: 0.0 },
                },
            ],
            share_group_color: None,
        }],
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
        // **expanded** で start (Phase 63n-2 の lane 内 hit-test を試すため)
        automation_lanes_collapsed: false,
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
        header_w: 200.0,
        ruler_h: 0.0,
        playhead_beat: None,
        loop_range: None,
        data_generation: 0,
        bpm: 120.0,
        time_sig: (4, 4),
        // 数値検証 test は raw beat 値を期待するので明示 OFF。
        snap: SnapConfig::OFF,
    }
}

fn pointer_press(x: f32, y: f32, modifiers: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers,
        ..PointerFrame::default()
    }
}

fn pointer_release(x: f32, y: f32, modifiers: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_released: true,
        modifiers,
        ..PointerFrame::default()
    }
}

fn run_arrangement_frame(host: &mut UiHost<ObsModel>, m: &mut ObsModel, input: FrameInput) {
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: WIDGET_RECT.w as u32,
        height: WIDGET_RECT.h as u32,
    };
    let view = make_view();
    let style = ArrangementStyle::default();
    host.frame(m, &mut scene, screen, input, |model, ui| {
        let resp = ui.arrangement(
            "arr",
            WIDGET_RECT,
            &model.tracks,
            view,
            &[],
            &[],
            &style,
            |req| match req {
                ArrangementEditRequest::SetLaneEnabled { lane, enabled } => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.lane_enabled.push((lane, enabled));
                    })
                }
                ArrangementEditRequest::SetLaneVisible { lane, visible } => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.lane_visible.push((lane, visible));
                    })
                }
                ArrangementEditRequest::SetLaneDefault { lane, prev, next } => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.lane_default.push((lane, prev, next));
                    })
                }
                ArrangementEditRequest::DeleteLane(k) => Edit::mutate(move |mm: &mut ObsModel| {
                    mm.deleted_lanes.push(k);
                }),
                ArrangementEditRequest::AddAutomationPoint {
                    clip,
                    time_beat,
                    value_norm,
                } => Edit::mutate(move |mm: &mut ObsModel| {
                    mm.added_points.push((clip, time_beat, value_norm));
                }),
                ArrangementEditRequest::MoveAutomationPoints(deltas) => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.moved_points.extend(deltas);
                    })
                }
                ArrangementEditRequest::DeleteAutomationPoints(keys) => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.deleted_points.extend(keys);
                    })
                }
                ArrangementEditRequest::SetAutomationCurveType { point, prev, next } => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.set_curves.push((point, prev, next));
                    })
                }
                _ => Edit::mutate(|_| {}),
            },
        );
        // automation_point_rects を観測 (毎 frame、 caller が context_menu_for で popup anchor 用に使う)
        let rects = resp.automation_point_rects.clone();
        ui.push_edit(Edit::mutate(move |mm: &mut ObsModel| {
            mm.point_rects = rects;
        }));
    });
}

/// lane header `★` (enabled) の hit-test と SetLaneEnabled emit を end-to-end で検証。
#[test]
fn lane_enabled_icon_click_emits_set_lane_enabled() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];

    let view = make_view();
    let style = ArrangementStyle::default();
    // lane 行の y 範囲: track row (y=0..32) の下、 height_px=60 の lane 行 (y=32..92)。
    let tops = visible_track_row_tops(&m.tracks, 0.0, view.track_top, view.track_row_h);
    let lane_y = tops[0] + view.track_row_h; // = 32
    let header_rect = Rect { x: 0.0, y: lane_y, w: view.header_w, h: 60.0 };
    let layout = automation_lane_header_layout(header_rect, &style).expect("layout");
    let cx = layout.enabled_icon_rect.x + layout.enabled_icon_rect.w * 0.5;
    let cy = layout.enabled_icon_rect.y + layout.enabled_icon_rect.h * 0.5;

    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });

    assert_eq!(m.lane_enabled.len(), 1, "★ click が SetLaneEnabled を 1 度発火");
    assert_eq!(m.lane_enabled[0], (AutomationLaneKey { track: 1, lane: 10 }, false));
}

/// lane header `👁` (visible) の hit-test と SetLaneVisible emit を検証。
#[test]
fn lane_visible_icon_click_emits_set_lane_visible() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    let view = make_view();
    let style = ArrangementStyle::default();
    let lane_y = view.track_row_h;
    let header_rect = Rect { x: 0.0, y: lane_y, w: view.header_w, h: 60.0 };
    let layout = automation_lane_header_layout(header_rect, &style).unwrap();
    let cx = layout.visible_icon_rect.x + layout.visible_icon_rect.w * 0.5;
    let cy = layout.visible_icon_rect.y + layout.visible_icon_rect.h * 0.5;
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    assert_eq!(m.lane_visible.len(), 1);
    assert_eq!(m.lane_visible[0], (AutomationLaneKey { track: 1, lane: 10 }, false));
}

/// lane header `✕` (delete) の hit-test と DeleteLane emit を検証。
#[test]
fn lane_delete_icon_click_emits_delete_lane() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    let view = make_view();
    let style = ArrangementStyle::default();
    let lane_y = view.track_row_h;
    let header_rect = Rect { x: 0.0, y: lane_y, w: view.header_w, h: 60.0 };
    let layout = automation_lane_header_layout(header_rect, &style).unwrap();
    let cx = layout.delete_icon_rect.x + layout.delete_icon_rect.w * 0.5;
    let cy = layout.delete_icon_rect.y + layout.delete_icon_rect.h * 0.5;
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    assert_eq!(m.deleted_lanes, vec![AutomationLaneKey { track: 1, lane: 10 }]);
}

/// lane body 内 clip 上 **double click** → AddAutomationPoint (single click では発火しない)。
/// Phase 63n-2 当初は single click で発火していたが、 selection 操作と衝突するため Bitwig / Live と同じ
/// dblclick 経由に変更 (#028 follow-up)。
#[test]
fn lane_body_double_click_emits_add_automation_point() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    // lane body の左 = view.header_w (= 200)、 lane y range = [32, 92]。
    // body width = WIDGET_RECT.w - header_w = 600 → beat_to_px = 600/16 = 37.5
    // beat 6 (= 既存 point の中間、 4 と 8 のあいだ) は body 内座標で 6 * 37.5 = 225
    // → screen x = 200 + 225 = 425
    // y は lane_y + pad (= 6) から開始、 clip_h = 60 - 12 = 48、 mid (cy) を使うと value_norm = 0.5
    let cx = 425.0;
    let cy = 32.0 + 6.0 + 24.0; // mid

    // single click (press → release) では何も起こらない (selection 操作用に保留される)
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    assert_eq!(m.added_points.len(), 0, "single click では AddAutomationPoint 発火しない");

    // 2 click 目 (press → release) で UiHost が double click 判定 → take_double_click_in_rect が
    // Some を返し AddAutomationPoint が発火する。
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });

    assert_eq!(m.added_points.len(), 1, "dblclick で AddAutomationPoint を 1 度発火");
    let (clip, time_beat, value_norm) = m.added_points[0];
    assert_eq!(clip, AutomationClipKey { track: 1, lane: 10, clip: 100 });
    // time_beat ≈ 6.0 (clip-local、 abs 6 - clip start 0 = 6)
    assert!((time_beat - 6.0).abs() < 0.05, "time_beat ≈ 6.0、 actual {time_beat}");
    // value_norm ≈ 0.5 (lane mid)
    assert!((value_norm - 0.5).abs() < 0.05, "value_norm ≈ 0.5、 actual {value_norm}");
}

/// Alt + click on point → DeleteAutomationPoints (即時発火、 commit-by-release なし)。
#[test]
fn alt_click_on_point_emits_delete_automation_points() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    // 既存 point[1] (time_beat=4, value_norm=0.3) の screen 位置を計算。
    // lane body x range = [200, 800]、 beat_to_px = 600/16 = 37.5 → x = 200 + 4*37.5 = 350
    // lane body y range = [32, 92]、 clip_y = 32 + 6 = 38、 clip_h = 48
    // value_norm 0.3 → y = 38 + (1 - 0.3) * 48 = 38 + 33.6 = 71.6
    let cx = 350.0;
    let cy = 71.6;
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(
            cx,
            cy,
            Modifiers { alt: true, ..Modifiers::default() },
        ),
        ..FrameInput::default()
    });
    assert_eq!(m.deleted_points.len(), 1, "Alt+click が DeleteAutomationPoints を発火");
    assert_eq!(
        m.deleted_points[0],
        AutomationPointKey {
            clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
            point_idx: 1,
        }
    );
}

/// 全 visible automation point の rect が `automation_point_rects` に並ぶ (= caller が
/// `context_menu_for` で右クリック anchor として使う、 #028 §11.4 の確定 idiom)。 `clip_rects` と
/// 同 pattern で毎 frame 描画順に並ぶ。
#[test]
fn automation_point_rects_lists_all_visible_points_in_draw_order() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    // 1 frame 走らせて rects を取得 (lane は 3 point を持つ)
    run_arrangement_frame(&mut host, &mut m, FrameInput::default());
    assert_eq!(
        m.point_rects.len(),
        3,
        "全 3 point の rect が並ぶ: got {}",
        m.point_rects.len()
    );
    // point_idx 順 (= time_beat 順 = 描画順)
    for (i, (key, rect)) in m.point_rects.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let expected_idx = i as u32;
        assert_eq!(key.point_idx, expected_idx);
        assert_eq!(key.clip.track, 1);
        assert_eq!(key.clip.lane, 10);
        assert_eq!(key.clip.clip, 100);
        // anchor rect は point dot の周辺 (radius 4 → w/h ≈ 8)
        assert!(rect.w > 0.0 && rect.h > 0.0);
    }
    // x 座標は time_beat 順 (左→右)
    assert!(m.point_rects[0].1.x < m.point_rects[1].1.x);
    assert!(m.point_rects[1].1.x < m.point_rects[2].1.x);
}

/// point drag → release で MoveAutomationPoints が 1 件発火 (commit-by-release)。
#[test]
fn point_drag_release_emits_move_automation_points() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];

    // press at point[1] (350, 71.6)、 release at offset (+30 px, -10 px)。
    // beat_to_px = 37.5 → +30 px = +0.8 beat → next time_beat ≈ 4.8
    // clip_h = 48 → -10 px / 48 = +0.208 (1 - dy/h) → next value_norm ≈ 0.3 + 0.208 ≈ 0.508
    let press = pointer_press(350.0, 71.6, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });

    // release frame
    let release = pointer_release(380.0, 61.6, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert_eq!(m.moved_points.len(), 1, "drag→release で MoveAutomationPoints 1 件");
    let d = m.moved_points[0];
    assert_eq!(
        d.point,
        AutomationPointKey {
            clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
            point_idx: 1,
        }
    );
    assert!((d.prev_time_beat - 4.0).abs() < 1e-6, "prev_time_beat = 4.0");
    assert!((d.prev_value_norm - 0.3).abs() < 1e-3, "prev_value_norm = 0.3");
    assert!(
        (d.next_time_beat - 4.8).abs() < 0.1,
        "next_time_beat ≈ 4.8、 actual {}",
        d.next_time_beat
    );
    assert!(
        d.next_value_norm > d.prev_value_norm,
        "上方向 drag で value_norm 増加: prev {} next {}",
        d.prev_value_norm,
        d.next_value_norm
    );
}

/// `automation_point_at` の純粋 hit-test (point 半径の 2 倍 hit zone)。
#[test]
fn automation_point_at_hits_point_within_radius() {
    let view = make_view();
    let style = ArrangementStyle::default();
    let tracks = vec![make_track(1, vec![make_lane(10, true)])];
    let lanes = Rect {
        x: view.header_w,
        y: 0.0,
        w: WIDGET_RECT.w - view.header_w,
        h: WIDGET_RECT.h,
    };
    let tops = visible_track_row_tops(&tracks, lanes.y, view.track_top, view.track_row_h);
    // point[1] (time_beat=4, value_norm=0.3) の中央
    let cx = 350.0;
    let cy = 71.6;
    let hit = automation_point_at(
        &tracks, &tops, view.track_row_h, view, 0.0, view.header_w, lanes, cx, cy, &style,
    );
    let (key, _r) = hit.expect("point hit");
    assert_eq!(
        key,
        AutomationPointKey {
            clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
            point_idx: 1,
        }
    );
    // ちょっと外れた位置 (radius 4 の 2 倍 = 8 を超える) は hit しない
    let no_hit = automation_point_at(
        &tracks, &tops, view.track_row_h, view, 0.0, view.header_w, lanes, cx + 20.0, cy, &style,
    );
    assert!(no_hit.is_none(), "point から 20px 離れた位置は hit しない");
}

/// lane invisible (visible=false) の point は hit しない (描画されないので操作対象外)。
#[test]
fn invisible_lane_point_is_not_hit_tested() {
    let mut lane = make_lane(10, true);
    lane.visible = false;
    let view = make_view();
    let style = ArrangementStyle::default();
    let tracks = vec![make_track(1, vec![lane])];
    let lanes = Rect {
        x: view.header_w,
        y: 0.0,
        w: WIDGET_RECT.w - view.header_w,
        h: WIDGET_RECT.h,
    };
    let tops = visible_track_row_tops(&tracks, lanes.y, view.track_top, view.track_row_h);
    let cx = 350.0;
    let cy = 71.6;
    let hit = automation_point_at(
        &tracks, &tops, view.track_row_h, view, 0.0, view.header_w, lanes, cx, cy, &style,
    );
    assert!(hit.is_none(), "invisible lane の point は hit しない");
}
