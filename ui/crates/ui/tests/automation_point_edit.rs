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
    Edit, FrameInput, MoveAutomationPointDelta, PointerFrame, SetAutomationCurveParamKind,
    SnapConfig, TrackKind, UiHost, automation_lane_header_layout, automation_point_at,
    visible_track_row_tops,
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
    /// M14 Phase 63n-4 (#029): lane body 空き dblclick → CreateAutomationClip 観測。
    created_clips: Vec<(AutomationLaneKey, f64, f64)>,
    /// M14 Phase 63n-5 (#030): lane 下端 splitter drag → SetLaneHeight 観測 (per-frame + release 全列)。
    lane_heights: Vec<(AutomationLaneKey, u16, u16)>,
    /// M14 Phase 63n-6 (#031): per-track row 下端 splitter / Alt+drag → SetSingleTrackRowH 観測
    /// (per-frame 全列、 (track_id, height_px) の tuple)。 caller として該当 track の `row_h = Some(h)`
    /// を mutate するため `tracks` 自身が live preview の SSoT。
    row_heights: Vec<(u32, u16)>,
    /// M14 Phase 63n-8 (#033): point selection の永続 SSoT (widget へ毎 frame 渡す、 SelectAutomationPoints
    /// 受信時に `selected_points = next` で上書き)。 lasso test では事前に push、 short click test では空。
    selected_points: Vec<AutomationPointKey>,
    /// M14 Phase 63n-8 (#033): 観測した `SelectAutomationPoints` の (prev, next) 列。
    select_points_events: Vec<(Vec<AutomationPointKey>, Vec<AutomationPointKey>)>,
    /// M14 Phase 63n-8 (#033): 観測した `automation_lasso_active` (drag 中 true)。
    lasso_active_frames: Vec<bool>,
    /// M14 Phase 63n-9 (#033): 観測した SetAutomationCurveParam の (point, kind, prev, next) 列。
    curve_param_events: Vec<(AutomationPointKey, SetAutomationCurveParamKind, f32, f32)>,
    /// M14 Phase 99 (#071): 観測した `SecondaryClickEmpty` の (track, beat, pos) 列。
    secondary_empty: Vec<(u32, f64, (f32, f32))>,
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
        armed: false,
        clips: Vec::<ArrangementClip>::new(),
        volume: 1.0,
        parent_id: None,
        depth: 0,
        collapsed: false,
        kind: TrackKind::Audio,
        // **expanded** で start (Phase 63n-2 の lane 内 hit-test を試すため)
        automation_lanes_collapsed: false,
        automation_lanes: lanes,
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
        ruler_h: 0.0,
        playhead_beat: None,
        loop_range: None,
        data_generation: 0,
        bpm: 120.0,
        time_sig: (4, 4),
        // 数値検証 test は raw beat 値を期待するので明示 OFF。
        snap: SnapConfig::OFF,
        arranger_lane_h: 0.0,
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

/// M14 Phase 99 (#071): 右クリック (secondary press) frame。
fn pointer_secondary_press(x: f32, y: f32) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        secondary_just_pressed: true,
        ..PointerFrame::default()
    }
}

/// main row に clip を 1 つ持ち automation lane を持たない track (clip exclusion test 用)。
fn make_clip_track(id: u32) -> ArrangementTrack {
    ArrangementTrack {
        id,
        name: Arc::from(format!("t{id}")),
        muted: false,
        solo: false,
        armed: false,
        clips: vec![ArrangementClip {
            id: 100,
            start_beat: 0.0,
            len_beats: 8.0,
            name: Arc::from("c1"),
            color: None,
            share_group_color: None,
            audio_edit: None,
            thumbnail: None,
            in_active_group: false,
        }],
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

#[allow(clippy::too_many_lines)]
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
            &[],
            view,
            &[],
            &[],
            &[],
            &model.selected_points,
            &style,
            None,
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
                ArrangementEditRequest::CreateAutomationClip {
                    lane,
                    start_beat,
                    len_beats,
                } => Edit::mutate(move |mm: &mut ObsModel| {
                    mm.created_clips.push((lane, start_beat, len_beats));
                }),
                ArrangementEditRequest::SetLaneHeight { lane, prev, next } => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.lane_heights.push((lane, prev, next));
                        // caller として lane.height_px を即座に反映 (drag 中の live preview を再現、
                        // widget の anchor は press 時 height_px を保持しているので drag 継続中に
                        // height を更新しても anchor 計算は壊れない)。
                        if let Some(t) = mm.tracks.iter_mut().find(|t| t.id == lane.track)
                            && let Some(l) =
                                t.automation_lanes.iter_mut().find(|l| l.id == lane.lane)
                        {
                            l.height_px = next;
                        }
                    })
                }
                ArrangementEditRequest::SetSingleTrackRowH { track, prev: _, next } => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        // M14 Phase 63n-6 (#031): per-track row 高さ override 観測。 caller-side
                        // clamp 16..1000 (daw_prototype と同 idiom)、 該当 track の `row_h = Some(h)` を更新。
                        let new_h = next.clamp(16, 1000);
                        mm.row_heights.push((track, new_h));
                        if let Some(t) = mm.tracks.iter_mut().find(|t| t.id == track) {
                            t.row_h = Some(new_h);
                        }
                    })
                }
                // M14 Phase 63n-8 (#033): SelectAutomationPoints 観測 + selected_points 更新 (= caller idiom)。
                ArrangementEditRequest::SelectAutomationPoints { prev, next } => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.select_points_events.push((prev, next.clone()));
                        mm.selected_points = next;
                    })
                }
                // M14 Phase 63n-9 (#033): handle drag release で SetAutomationCurveParam 観測。
                ArrangementEditRequest::SetAutomationCurveParam {
                    point,
                    kind,
                    prev_value,
                    next_value,
                } => Edit::mutate(move |mm: &mut ObsModel| {
                    mm.curve_param_events.push((point, kind, prev_value, next_value));
                    // tracks 側 curve を即座に反映 (= caller idiom)、 次 frame で drag overlay の base に
                    // 反映されないと test の連続発火が壊れる。
                    if let Some(t) = mm.tracks.iter_mut().find(|t| t.id == point.clip.track)
                        && let Some(l) = t
                            .automation_lanes
                            .iter_mut()
                            .find(|l| l.id == point.clip.lane)
                        && let Some(c) = l.clips.iter_mut().find(|c| c.id == point.clip.clip)
                        && let Some(p) = c.points.get_mut(point.point_idx as usize)
                    {
                        p.curve = match kind {
                            SetAutomationCurveParamKind::BezierTension => {
                                ArrangementCurveKind::Bezier { tension: next_value }
                            }
                            SetAutomationCurveParamKind::ExponentialBend => {
                                ArrangementCurveKind::Exponential { bend: next_value }
                            }
                        };
                    }
                }),
                ArrangementEditRequest::SecondaryClickEmpty { track, beat, pos } => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.secondary_empty.push((track, beat, pos));
                    })
                }
                _ => Edit::mutate(|_| {}),
            },
        );
        // automation_point_rects を観測 (毎 frame、 caller が context_menu_for で popup anchor 用に使う)
        let rects = resp.automation_point_rects.clone();
        let lasso_active = resp.automation_lasso_active;
        ui.push_edit(Edit::mutate(move |mm: &mut ObsModel| {
            mm.point_rects = rects;
            mm.lasso_active_frames.push(lasso_active);
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

/// M14 Phase 63n-4 (#029): lane body 内 clip ギャップ (= 既存 clip と x 範囲が重ならない) で
/// dblclick → CreateAutomationClip 発火。 既存 clip 上 dblclick は priority 排他で
/// AddAutomationPoint のまま、 CreateAutomationClip は発火しない (regression check)。
#[test]
fn lane_body_dblclick_in_clip_gap_emits_create_automation_clip() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    // clip を [0..6] に短縮 (= len_beats=6) して beat 10 を「lane body 内 + clip ギャップ」 にする。
    let mut lane = make_lane(10, true);
    lane.clips[0].len_beats = 6.0;
    m.tracks = vec![make_track(1, vec![lane])];

    // beat 10 = screen x = 200 + 10*37.5 = 575、 lane body の cy mid (= 32 + 30 = 62)
    let cx = 575.0;
    let cy = 32.0 + 30.0;

    // 1 click 目: single click では発火しない (clip ギャップでも AddAutomationPoint と同様 dblclick 必須)。
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    assert_eq!(m.created_clips.len(), 0, "single click では CreateAutomationClip 発火しない");

    // 2 click 目 (UiHost が dblclick 判定) → CreateAutomationClip が発火。
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });

    assert_eq!(m.created_clips.len(), 1, "dblclick で CreateAutomationClip を 1 度発火");
    let (lane_key, start_beat, len_beats) = m.created_clips[0];
    assert_eq!(lane_key, AutomationLaneKey { track: 1, lane: 10 });
    // SnapConfig::OFF なので raw beat 10 がそのまま入る。
    assert!((start_beat - 10.0).abs() < 0.05, "start_beat ≈ 10.0、 actual {start_beat}");
    // len_beats は style 既定値 (= 4.0)。
    assert!((len_beats - 4.0).abs() < 1e-6, "style 既定 len_beats=4.0、 actual {len_beats}");
    // AddAutomationPoint は **発火しない** (priority 排他)。
    assert_eq!(
        m.added_points.len(),
        0,
        "clip ギャップ dblclick では AddAutomationPoint は発火しない"
    );
}

/// M14 Phase 63n-4 (#029) regression: 既存 clip 上の dblclick は priority 1 (clip hit)
/// で `DoubleClickClip` が、 priority 2 (lane body 内 clip 内) で `AddAutomationPoint` が
/// 発火する path のまま、 `CreateAutomationClip` は発火しない。 priority 1 (clip_hit) は
/// track row 内の clip rect だけを見るため、 lane body 内 clip は priority 2 の
/// `AddAutomationPoint` に行く。
#[test]
fn lane_body_dblclick_on_existing_clip_does_not_emit_create() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    // 既存 point の中間 (beat 6) で lane body 内、 clip 内。
    let cx = 425.0;
    let cy = 32.0 + 6.0 + 24.0;

    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });

    assert_eq!(m.added_points.len(), 1, "既存 clip 内 dblclick は AddAutomationPoint を発火");
    assert_eq!(
        m.created_clips.len(),
        0,
        "既存 clip 上 dblclick では CreateAutomationClip は発火しない (priority 排他)"
    );
}

/// M14 Phase 63n-5 (#030): lane 下端 splitter drag で `SetLaneHeight` が drag 中 per-frame +
/// release で発火。 widget は [min, max] (style 既定 30/200) に clamp 済の `next` を渡す。
#[test]
fn lane_bottom_splitter_drag_emits_set_lane_height() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];

    // lane の bottom edge: track row 32 + lane height 60 = y=92。 hot zone は [92-handle, 92) =
    // [88, 92)。 px は body x range の中央付近 (lanes.x = 200、 lanes.w = 600 → cx=400)。
    let cx = 400.0;
    let press_y = 90.0; // splitter 内
    let drag_y = 110.0; // press から +20 px (= 高さ +20 → 60→80)

    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, press_y, Modifiers::empty()),
        ..FrameInput::default()
    });
    // drag 中の continuation frame (primary_pressed のまま position 移動)
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: PointerFrame {
            pos: Some((cx, drag_y)),
            primary_pressed: true,
            ..PointerFrame::default()
        },
        ..FrameInput::default()
    });
    // release frame (primary_just_released)
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, drag_y, Modifiers::empty()),
        ..FrameInput::default()
    });

    // 少なくとも 1 度は per-frame emit、 最後 1 回は release で発火 (合計 ≥ 2 件)。
    assert!(
        m.lane_heights.len() >= 2,
        "drag 中 per-frame + release で 2 件以上発火、 actual {} 件",
        m.lane_heights.len()
    );
    let final_emit = *m.lane_heights.last().expect("≥ 2 件あるので非空");
    assert_eq!(final_emit.0, AutomationLaneKey { track: 1, lane: 10 });
    assert_eq!(final_emit.1, 60, "prev は drag 開始時の anchor (= 60 px)");
    assert_eq!(final_emit.2, 80, "next は anchor + dy = 60 + 20 = 80 px");
}

/// M14 Phase 63n-5 (#030): drag を min より低く / max より高く引っ張っても widget が clamp する。
#[test]
fn lane_bottom_splitter_drag_clamps_to_style_min_max() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];

    let cx = 400.0;
    // press 位置は splitter (lane bottom = 92)
    let press_y = 90.0;
    // 上に 200 px 引っ張る (height = 60 - 200 = -140 → clamp で min=30 に)
    let drag_up_y = press_y - 200.0;
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, press_y, Modifiers::empty()),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: PointerFrame {
            pos: Some((cx, drag_up_y)),
            primary_pressed: true,
            ..PointerFrame::default()
        },
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, drag_up_y, Modifiers::empty()),
        ..FrameInput::default()
    });

    // 全 emit は min (= 30) 以上に clamp 済
    assert!(!m.lane_heights.is_empty(), "drag で SetLaneHeight が発火");
    for (_, _, next) in &m.lane_heights {
        assert!(*next >= 30, "next ({next} px) は min=30 以上に clamp");
    }
    // 最後の emit は min ぴったり (release 時点で raw が min を大きく下回っている)
    assert_eq!(m.lane_heights.last().expect("non-empty").2, 30);
}

/// M14 Phase 63n-6 (#031): lane body 内の **任意位置** で Alt+drag → `SetLaneHeight` 発火 (= splitter
/// と Alt+drag の両方併用)。 splitter (= lane bottom 4px hot zone) の外 + clip / point 上でも無い
/// 位置で Alt+vertical drag が動くことを検証。
#[test]
fn lane_body_alt_drag_emits_set_lane_height() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    // make_lane(10, true) は clip [0..16] の 1 lane。 cursor を beat 16 より右の **clip 外** + lane
    // 上端 (= padding zone を避けて splitter の外) に置く。 lane body x range = [200, 800]、 beat_to_px
    // = 600/16 = 37.5、 beat 16 = lane の右端で screen x = 200+600 = 800。 lane の clip 範囲は [0, 16]
    // = body 全幅なので、 「clip 外 x range」 が無い。 そこで clip を短縮 ([0..6] beats) して beat 10 を
    // 「clip 外 + lane body」 にする。
    let mut lane = make_lane(10, true);
    lane.clips[0].len_beats = 6.0;
    m.tracks = vec![make_track(1, vec![lane])];

    // beat 10 = screen x = 200 + 10*37.5 = 575、 lane body の cy mid (= 32 + 30 = 62) — splitter zone
    // (88..92) の外、 clip 外、 point の半径 (8px) からも離れている。
    let cx = 575.0;
    let press_y = 62.0;
    let drag_y = 80.0; // press から +18 px (= height 60→78)

    let alt = Modifiers { alt: true, ..Modifiers::default() };
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, press_y, alt),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: PointerFrame {
            pos: Some((cx, drag_y)),
            primary_pressed: true,
            modifiers: alt,
            ..PointerFrame::default()
        },
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, drag_y, alt),
        ..FrameInput::default()
    });

    assert!(
        m.lane_heights.len() >= 2,
        "Alt+drag で per-frame + release ≥2 件、 actual {}",
        m.lane_heights.len()
    );
    let final_emit = *m.lane_heights.last().expect("≥ 2 件あるので非空");
    assert_eq!(final_emit.0, AutomationLaneKey { track: 1, lane: 10 });
    assert_eq!(final_emit.1, 60, "prev = anchor (= 60 px)");
    assert_eq!(final_emit.2, 78, "next = 60 + 18 = 78 px");
}

/// M14 Phase 63n-6 (#031): track row 下端 splitter drag → `SetTrackRowH(f32)` 発火 (per-frame
/// live update + caller-side clamp 16..1000)。 lane 無し track の row (高さ 32 px) の下端 ±4 px
/// hot zone (= y in [28, 32)) を press → drag → release で 1 件以上 emit、 final ≈ 52 (= 32 + 20)。
#[test]
fn track_row_bottom_splitter_drag_emits_set_track_row_h() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    // lane 無し track 1 個 (row 高さ = view.track_row_h = 32 px)。
    m.tracks = vec![make_track(1, vec![])];

    // splitter hot zone: row_bottom - handle .. row_bottom = 28..32 (handle=4.0)
    // x は lanes range 内の任意位置 (header_w=200, lanes=200..800)。
    let cx = 400.0;
    let press_y = 30.0; // 中央 of [28, 32)
    let drag_y = 50.0; // press から +20 px → row_h = 32 + 20 = 52

    let none = Modifiers::empty();
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, press_y, none),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: PointerFrame {
            pos: Some((cx, drag_y)),
            primary_pressed: true,
            modifiers: none,
            ..PointerFrame::default()
        },
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, drag_y, none),
        ..FrameInput::default()
    });

    assert!(
        !m.row_heights.is_empty(),
        "splitter drag で per-frame SetSingleTrackRowH ≥1 件、 actual {}",
        m.row_heights.len()
    );
    let (track_id, final_h) = *m.row_heights.last().expect("non-empty");
    assert_eq!(track_id, 1, "press した track 1 のみが emit 対象");
    assert_eq!(final_h, 52, "final row_h = 32 + 20 = 52 px");
}

/// M14 Phase 63n-6 (#031): track row body 内の **任意位置** で Alt+drag → `SetSingleTrackRowH` 発火
/// (= splitter と Alt+drag の両方併用、 per-track のみ resize)。 splitter zone (= row 下端 4 px) の外で
/// Alt+vertical drag が動くことを検証。
#[test]
fn track_row_body_alt_drag_emits_set_single_track_row_h() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![])];

    // splitter zone (28..32) の外、 row body 中央 y=16 に cursor を置く。 lanes range 内の任意 x。
    let cx = 400.0;
    let press_y = 16.0;
    let drag_y = 36.0; // +20 px → row_h = 32 + 20 = 52

    let alt = Modifiers { alt: true, ..Modifiers::default() };
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, press_y, alt),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: PointerFrame {
            pos: Some((cx, drag_y)),
            primary_pressed: true,
            modifiers: alt,
            ..PointerFrame::default()
        },
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, drag_y, alt),
        ..FrameInput::default()
    });

    assert!(
        !m.row_heights.is_empty(),
        "Alt+drag で per-frame SetSingleTrackRowH ≥1 件、 actual {}",
        m.row_heights.len()
    );
    let (track_id, final_h) = *m.row_heights.last().expect("non-empty");
    assert_eq!(track_id, 1, "press した track 1 のみが emit 対象");
    assert_eq!(final_h, 52, "final row_h = 32 + 20 = 52 px");
}

/// M14 Phase 63n-5 (#030): splitter 外 (= lane body の上方 / 縦 padding) を click しても
/// resize drag は起動せず、 既存挙動が維持される。
#[test]
fn lane_body_press_outside_splitter_does_not_emit_set_lane_height() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];

    // lane body 中央 (= y=62) で press → splitter (y=88..92) 外なので resize 発火しない
    let cx = 400.0;
    let cy = 62.0;
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_press(cx, cy, Modifiers::empty()),
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: PointerFrame {
            pos: Some((cx, cy + 20.0)),
            primary_pressed: true,
            ..PointerFrame::default()
        },
        ..FrameInput::default()
    });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_release(cx, cy + 20.0, Modifiers::empty()),
        ..FrameInput::default()
    });
    assert_eq!(
        m.lane_heights.len(),
        0,
        "splitter 外 press では SetLaneHeight 発火しない"
    );
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

// ============================================================
// M14 Phase 63n-8 (#033): lasso + selection visual + multi-select drag
// ============================================================

/// lasso 試験用 lane: 短い clip (4..10 beat) + 3 points (abs 4, 6, 9)。 clip 前後に空き zone がある。
/// - 空き zone 前: x = 200..350 (= beat 0..4)
/// - clip 内: x = 350..575 (= beat 4..10)
/// - 空き zone 後: x = 575..800 (= beat 10..16)
/// - point 中心 x: 350 (idx 0, abs 4)、 425 (idx 1, abs 6)、 537.5 (idx 2, abs 9)
/// - point 中心 y: 47.6 (val 0.8)、 71.6 (val 0.3)、 57.2 (val 0.6) — make_lane と同 value 構成
fn make_lasso_lane(id: u32) -> ArrangementAutomationLane {
    ArrangementAutomationLane {
        id,
        label: Arc::from(format!("Lane{id}")),
        icon_glyph: 'V',
        color: Color::rgb(0.55, 0.85, 1.0),
        enabled: true,
        visible: true,
        height_px: 60,
        default_value_norm: 0.5,
        clips: vec![ArrangementAutomationClip {
            id: 100,
            start_beat: 4.0,
            len_beats: 6.0,
            name: Arc::from("clip"),
            points: vec![
                ArrangementAutomationPoint {
                    time_beat: 0.0,
                    value_norm: 0.8,
                    curve: ArrangementCurveKind::Linear,
                },
                ArrangementAutomationPoint {
                    time_beat: 2.0,
                    value_norm: 0.3,
                    curve: ArrangementCurveKind::Linear,
                },
                ArrangementAutomationPoint {
                    time_beat: 5.0,
                    value_norm: 0.6,
                    curve: ArrangementCurveKind::Bezier { tension: 0.0 },
                },
            ],
            share_group_color: None,
        }],
    }
}

/// 空き lane zone から drag → release で `SelectAutomationPoints` を発火する。
/// 修飾なし lasso → next = lasso 内 points (replace)。
///
/// `make_lasso_lane`: 空き zone (x>575、 beat>10) で drag 開始、 clip 内まで戻して point 2 (x=537.5) を拾う。
#[test]
fn lasso_empty_zone_drag_emits_select_automation_points() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lasso_lane(10)])];

    // press at (600, 50) = clip 後の空き zone (clip ends at x=575)、 point 上ではない
    let press = pointer_press(600.0, 50.0, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    // drag continuation で session を維持、 clip 内まで戻して point 2 (537.5, 57.2) を拾う
    let cont = PointerFrame {
        pos: Some((500.0, 80.0)),
        primary_pressed: true,
        modifiers: Modifiers::empty(),
        ..PointerFrame::default()
    };
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: cont,
        ..FrameInput::default()
    });
    // release at (500, 80) — lasso rect は (500, 50, 100, 30)、 point 2 (537.5, 57.2) が中に入る
    let release = pointer_release(500.0, 80.0, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert_eq!(
        m.select_points_events.len(),
        1,
        "lasso release で SelectAutomationPoints 1 件: events={:?}",
        m.select_points_events
    );
    let (prev, next) = &m.select_points_events[0];
    assert!(prev.is_empty(), "prev は空");
    assert_eq!(next.len(), 1, "lasso 内に point 2 が含まれる: {next:?}");
    assert_eq!(
        next[0],
        AutomationPointKey {
            clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
            point_idx: 2,
        }
    );
}

/// Shift+lasso → next = prev ∪ lasso 内 points (union)、 旧 selection は保持。
#[test]
fn lasso_with_shift_unions_selection() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lasso_lane(10)])];
    // 事前に point 0 を選択済として渡す
    m.selected_points = vec![AutomationPointKey {
        clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
        point_idx: 0,
    }];

    let shift = Modifiers { shift: true, ..Modifiers::empty() };
    let press = pointer_press(600.0, 50.0, shift);
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    let release = pointer_release(500.0, 80.0, shift);
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert_eq!(m.select_points_events.len(), 1, "Shift+lasso で 1 件発火");
    let (_prev, next) = &m.select_points_events[0];
    assert_eq!(next.len(), 2, "prev (point 0) ∪ lasso (point 2): {next:?}");
    assert!(
        next.contains(&AutomationPointKey {
            clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
            point_idx: 0
        }) && next.contains(&AutomationPointKey {
            clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
            point_idx: 2
        }),
        "union に point 0 と point 2 が含まれる: {next:?}"
    );
}

/// Ctrl+lasso → next = prev XOR lasso (toggle)、 lasso 内で prev に在った点は除外。
#[test]
fn lasso_with_ctrl_toggles_selection() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lasso_lane(10)])];
    // 事前に point 2 を選択済 (lasso が拾う予定の点)
    m.selected_points = vec![AutomationPointKey {
        clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
        point_idx: 2,
    }];

    let ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };
    let press = pointer_press(600.0, 50.0, ctrl);
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    let release = pointer_release(500.0, 80.0, ctrl);
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert_eq!(m.select_points_events.len(), 1, "Ctrl+lasso で 1 件発火");
    let (_prev, next) = &m.select_points_events[0];
    assert!(next.is_empty(), "XOR で point 2 が除外され空: {next:?}");
}

/// point 上の短 click (drag<4px) で `SelectAutomationPoints` を single select として発火。
#[test]
fn short_click_on_point_replaces_selection() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];

    let press = pointer_press(350.0, 71.6, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    // release at almost same point (drag<4px)
    let release = pointer_release(351.0, 72.0, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert!(
        m.moved_points.is_empty(),
        "短 click で MoveAutomationPoints は発火しない"
    );
    assert_eq!(
        m.select_points_events.len(),
        1,
        "短 click で SelectAutomationPoints 1 件"
    );
    let (prev, next) = &m.select_points_events[0];
    assert!(prev.is_empty(), "prev 空");
    assert_eq!(next.len(), 1, "single select");
    assert_eq!(next[0].point_idx, 1);
}

/// Shift+短 click on point → toggle (XOR)、 prev に在った点を除く。
#[test]
fn short_click_on_point_with_shift_toggles_selection() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    m.selected_points = vec![AutomationPointKey {
        clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
        point_idx: 1,
    }];

    let press = pointer_press(350.0, 71.6, Modifiers { shift: true, ..Modifiers::empty() });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    let release = pointer_release(351.0, 72.0, Modifiers { shift: true, ..Modifiers::empty() });
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert_eq!(m.select_points_events.len(), 1);
    let (_prev, next) = &m.select_points_events[0];
    assert!(next.is_empty(), "Shift+短 click で point 1 が除外され空");
}

/// 空き zone の短 click (drag<4px、 修飾なし) → selection clear (next = vec![])。
#[test]
fn short_click_on_empty_clears_selection() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lasso_lane(10)])];
    m.selected_points = vec![AutomationPointKey {
        clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
        point_idx: 2,
    }];

    // 空き zone (clip 後ろの empty zone)
    let press = pointer_press(600.0, 50.0, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    let release = pointer_release(601.0, 51.0, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert_eq!(m.select_points_events.len(), 1);
    let (_prev, next) = &m.select_points_events[0];
    assert!(next.is_empty(), "空き短 click で clear: {next:?}");
}

/// 複数選択中の 1 point を drag → 全 selected 点が同 delta で move。
#[test]
fn multi_select_drag_moves_all_selected_points() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    // point 0 と point 1 の 2 つを選択済として渡す
    m.selected_points = vec![
        AutomationPointKey {
            clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
            point_idx: 0,
        },
        AutomationPointKey {
            clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
            point_idx: 1,
        },
    ];

    // press at point[1] (350, 71.6)、 drag +30px / -10px
    let press = pointer_press(350.0, 71.6, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    let release = pointer_release(380.0, 61.6, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    // 2 deltas 期待 (= multi-select drag、 selected 2 件分)
    assert_eq!(
        m.moved_points.len(),
        2,
        "multi-select drag で 2 deltas: got {:?}",
        m.moved_points
    );
    // 各 delta の x/y delta が同じ (= 同 (adjusted_dt, dv) で全選択が動く)
    let dt_0 = m.moved_points[0].next_time_beat - m.moved_points[0].prev_time_beat;
    let dt_1 = m.moved_points[1].next_time_beat - m.moved_points[1].prev_time_beat;
    assert!((dt_0 - dt_1).abs() < 1e-6, "全選択点で同 dt: {dt_0} vs {dt_1}");
}

// ============================================================
// M14 Phase 63n-9 (#033): tension/bend handle press / drag / release
// ============================================================

/// `make_lane` の point idx 2 (time=8, val=0.6, Bezier { 0.0 }) の handle 位置を計算する helper。
/// midpoint x = (350+500)/2 = 425、 mid_y = evaluate_bezier_y(71.6, 57.2, 0.0, 0.5) ≈ 64.4、
/// handle y = mid_y - offset 10 ≈ 54.4。 selected_points に point idx 2 が含まれている前提。
fn handle_pos_for_point_idx_2() -> (f32, f32) {
    // Linear midpoint of y (tension=0 → linear): (71.6 + 57.2) / 2 = 64.4
    (425.0, 64.4 - 10.0)
}

/// selected point の Bezier 入射 segment 中央 handle を press → drag → release で
/// `SetAutomationCurveParam { kind: BezierTension }` 1 件発火。
/// 下方向 30px drag → value delta = -30 * 2 / 60 = -1.0、 clamp で -1.0 (= overshoot 反転)。
#[test]
fn handle_drag_release_emits_set_automation_curve_param() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    // point idx 2 (Bezier { 0.0 }) を選択
    m.selected_points = vec![AutomationPointKey {
        clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
        point_idx: 2,
    }];

    let (hx, hy) = handle_pos_for_point_idx_2();
    let press = pointer_press(hx, hy, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    // drag down 30px (= value -1.0 with default sensitivity)
    let release = pointer_release(hx, hy + 30.0, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert_eq!(
        m.curve_param_events.len(),
        1,
        "handle drag release で SetAutomationCurveParam 1 件: {:?}",
        m.curve_param_events
    );
    let (point, kind, prev, next) = &m.curve_param_events[0];
    assert_eq!(point.point_idx, 2);
    assert_eq!(*kind, SetAutomationCurveParamKind::BezierTension);
    assert!((prev - 0.0).abs() < 1e-6, "prev=0.0");
    assert!((next - (-1.0)).abs() < 1e-3, "next ≈ -1.0 (overshoot 反転): got {next}");
}

/// Alt 押下 drag は × 0.2 sensitivity (= 5x 精細)。 30px drag → -0.2。
#[test]
fn handle_drag_alt_sensitivity_is_one_fifth() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    m.selected_points = vec![AutomationPointKey {
        clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
        point_idx: 2,
    }];

    let (hx, hy) = handle_pos_for_point_idx_2();
    let alt = Modifiers { alt: true, ..Modifiers::empty() };
    let press = pointer_press(hx, hy, alt);
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    // continuation で Alt 状態を session に伝える (release frame は update skip pattern)
    let cont = PointerFrame {
        pos: Some((hx, hy + 30.0)),
        primary_pressed: true,
        modifiers: alt,
        ..PointerFrame::default()
    };
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: cont,
        ..FrameInput::default()
    });
    let release = pointer_release(hx, hy + 30.0, alt);
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert_eq!(m.curve_param_events.len(), 1);
    let (_, _, _prev, next) = &m.curve_param_events[0];
    // 30 * 2 / 60 * 0.2 = 0.2 (下方向 = - value)
    assert!(
        (next - (-0.2)).abs() < 1e-2,
        "Alt 30px drag → -0.2: got {next}"
    );
}

/// 同位置で release (drag<1e-4 value) → SetAutomationCurveParam **非発火** (= click 相当 no-op)。
#[test]
fn handle_click_without_drag_does_not_emit() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    m.selected_points = vec![AutomationPointKey {
        clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
        point_idx: 2,
    }];

    let (hx, hy) = handle_pos_for_point_idx_2();
    let press = pointer_press(hx, hy, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    let release = pointer_release(hx, hy, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert!(
        m.curve_param_events.is_empty(),
        "0px drag で SetAutomationCurveParam 非発火: {:?}",
        m.curve_param_events
    );
}

/// 未選択 point の handle は hit しない (= curve param drag 起動しない、 lasso か click にフォールバック)。
#[test]
fn handle_not_hit_when_point_not_selected() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    // selected_points 空 (= handle 描画も hit 対象も無い)
    let (hx, hy) = handle_pos_for_point_idx_2();
    let press = pointer_press(hx, hy, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    let release = pointer_release(hx, hy + 30.0, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert!(
        m.curve_param_events.is_empty(),
        "未選択 point の handle 位置 click では curve param event 非発火"
    );
}

/// Hold / Linear curve の point は handle が無い (= 入射 segment が直線/階段で param なし)。
/// selected であっても press at handle position は別 zone (clip 内空き) 扱い、 curve param event 非発火。
#[test]
fn handle_not_shown_for_hold_or_linear_curve() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];
    // point 0/1 は Linear、 point 1 (Linear) を選択 — 入射 segment は Linear で handle なし
    m.selected_points = vec![AutomationPointKey {
        clip: AutomationClipKey { track: 1, lane: 10, clip: 100 },
        point_idx: 1,
    }];

    // point 0 から point 1 への中点付近で handle 想定位置を試す (実際には Linear なので handle なし)
    // midpoint x = (200+350)/2 = 275、 y mid = (47.6+71.6)/2 = 59.6、 handle y = 59.6 - 10 = 49.6
    let press = pointer_press(275.0, 49.6, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    let release = pointer_release(275.0, 79.6, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: release,
        ..FrameInput::default()
    });

    assert!(
        m.curve_param_events.is_empty(),
        "Linear point の入射 segment は handle なし → SetAutomationCurveParam 非発火"
    );
}

/// drag 中は `response.automation_lasso_active = true` (release frame までは active)。
#[test]
fn lasso_active_flag_set_during_drag() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lasso_lane(10)])];

    // press 空き zone
    let press = pointer_press(600.0, 50.0, Modifiers::empty());
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: press,
        ..FrameInput::default()
    });
    // continuation frame
    let cont = PointerFrame {
        pos: Some((550.0, 60.0)),
        primary_pressed: true,
        modifiers: Modifiers::empty(),
        ..PointerFrame::default()
    };
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: cont,
        ..FrameInput::default()
    });

    // press frame と continuation frame の 2 回、 lasso_active = true が観測される
    // (press frame は automation_lasso_session が Some なので true、 continuation も同様)
    assert!(
        m.lasso_active_frames.iter().filter(|&&a| a).count() >= 2,
        "press + continuation で active=true が 2 回以上観測される: {:?}",
        m.lasso_active_frames
    );
}

// ===== M14 Phase 99 (daw_01 #071): 空きレーン右クリック → SecondaryClickEmpty =====
//
// lanes pane は x=[200,800) (header_w=200)、 main track row は y=[0,32) (track_row_h=32)、
// automation lane 行は y=[32,92) (height_px=60)。 beat は snap OFF で raw、
// px_to_beat(cx)=(cx-200)/600*16。 cx=350 → beat 4.0。

/// main row 空白の右クリック → SecondaryClickEmpty を track / snapped beat / pos 付きで発火。
#[test]
fn secondary_click_on_empty_track_row_emits() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    // 自動 lane 持ちだが main row には clip なし → main row (y<32) は真の空き。
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];

    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_secondary_press(350.0, 16.0),
        ..FrameInput::default()
    });

    assert_eq!(m.secondary_empty.len(), 1, "空き右クリックで 1 度発火: {:?}", m.secondary_empty);
    let (track, beat, pos) = m.secondary_empty[0];
    assert_eq!(track, 1, "track id");
    assert!((beat - 4.0).abs() < 0.01, "snapped beat ≈ 4.0: {beat}");
    assert_eq!(pos, (350.0, 16.0), "pos = 右クリック viewport 座標");
}

/// clip 上の右クリック → SecondaryClickEmpty は発火しない (caller の clip context menu 用)。
#[test]
fn secondary_click_on_clip_does_not_emit() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    // main row に clip [0..8] beat。 cx=350 (beat 4.0) は clip 内。
    m.tracks = vec![make_clip_track(1)];

    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_secondary_press(350.0, 16.0),
        ..FrameInput::default()
    });

    assert!(m.secondary_empty.is_empty(), "clip 上では発火しない: {:?}", m.secondary_empty);
}

/// automation lane 上の右クリック → SecondaryClickEmpty は発火しない (lane に吸収)。
#[test]
fn secondary_click_on_automation_lane_does_not_emit() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, true)])];

    // y=60 は lane 行 (y=[32,92)) 内。
    run_arrangement_frame(&mut host, &mut m, FrameInput {
        pointer: pointer_secondary_press(350.0, 60.0),
        ..FrameInput::default()
    });

    assert!(m.secondary_empty.is_empty(), "automation lane 上では発火しない: {:?}", m.secondary_empty);
}
