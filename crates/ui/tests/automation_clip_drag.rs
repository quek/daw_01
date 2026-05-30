//! M14 Phase 63n-3 (#028): lane 内 automation clip の Move / Resize / Clone drag を end-to-end で検証。
//!
//! Phase 63n-2 の point edit test (`automation_point_edit.rs`) と同 pattern で `UiHost::frame` を直接
//! 呼び、 `PointerFrame` を流して EditRequest 発火を観測する。 lane 跨ぎ drag も verify (Move のとき
//! release y から target lane を resolve)。

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use daw_ui_core::{
    ArrangementAutomationClip, ArrangementAutomationLane, ArrangementAutomationPoint,
    ArrangementClip, ArrangementCurveKind, ArrangementEditRequest, ArrangementStyle,
    ArrangementTrack, ArrangementView, AutomationClipKey, AutomationLaneKey, ClipDragKind, Edit,
    FrameInput, MoveAutomationClipDelta, PointerFrame, ResizeAutomationClipDelta, SnapConfig,
    TrackKind, UiHost, automation_clip_zone_at, automation_lane_key_at_y, visible_track_row_tops,
};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Color, Rect, Scene};

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };

#[derive(Default)]
struct ObsModel {
    tracks: Vec<ArrangementTrack>,
    moved_clips: Vec<MoveAutomationClipDelta>,
    cloned_linked: Vec<MoveAutomationClipDelta>,
    cloned_indep: Vec<MoveAutomationClipDelta>,
    resized_clips: Vec<ResizeAutomationClipDelta>,
    deleted_clips: Vec<AutomationClipKey>,
    /// `automation_clip_rects` を毎 frame 観測 (右クリック anchor 用に caller が使う想定)。
    clip_rects: Vec<(AutomationClipKey, Rect)>,
    /// `dragging_automation_clip` を毎 frame 観測 (cursor / status indicator 用)。
    dragging_kind: Option<ClipDragKind>,
}

fn make_clip(id: u32, start: f64, len: f64) -> ArrangementAutomationClip {
    // point は clip-local 中央 + 上方 (value_norm=0.9) に置いて、 clip の resize/move drag テストの
    // hit-test と衝突しない位置にする (= clip 左端 / 右端 / 中央 mid_y は points に当たらない)。
    // automation_point_at は radius 2 倍 (= 8px @ default radius=4) で hit するため、 point dot から
    // 離した position で clip drag press / release を行えば point drag が起動しない。
    ArrangementAutomationClip {
        id,
        start_beat: start,
        len_beats: len,
        name: Arc::from(format!("clip{id}")),
        points: vec![ArrangementAutomationPoint {
            time_beat: len * 0.5,
            value_norm: 0.9,
            curve: ArrangementCurveKind::Linear,
        }],
        share_group_color: None,
    }
}

fn make_lane(id: u32, clips: Vec<ArrangementAutomationClip>) -> ArrangementAutomationLane {
    ArrangementAutomationLane {
        id,
        label: Arc::from(format!("Lane{id}")),
        icon_glyph: 'V',
        color: Color::rgb(0.55, 0.85, 1.0),
        enabled: true,
        visible: true,
        height_px: 60,
        default_value_norm: 0.5,
        clips,
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

fn pointer_drag(x: f32, y: f32, modifiers: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
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

fn run_frame(host: &mut UiHost<ObsModel>, m: &mut ObsModel, input: FrameInput) {
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
            &[],
            &[],
            &style,
            None,
            |req| match req {
                ArrangementEditRequest::MoveAutomationClips(deltas) => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.moved_clips.extend(deltas);
                    })
                }
                ArrangementEditRequest::CloneAutomationClipsLinked(deltas) => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.cloned_linked.extend(deltas);
                    })
                }
                ArrangementEditRequest::CloneAutomationClipsIndependent(deltas) => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.cloned_indep.extend(deltas);
                    })
                }
                ArrangementEditRequest::ResizeAutomationClips(deltas) => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.resized_clips.extend(deltas);
                    })
                }
                ArrangementEditRequest::DeleteAutomationClips(keys) => {
                    Edit::mutate(move |mm: &mut ObsModel| {
                        mm.deleted_clips.extend(keys);
                    })
                }
                _ => Edit::mutate(|_| {}),
            },
        );
        let rects = resp.automation_clip_rects.clone();
        let drag = resp.dragging_automation_clip;
        ui.push_edit(Edit::mutate(move |mm: &mut ObsModel| {
            mm.clip_rects = rects;
            mm.dragging_kind = drag;
        }));
    });
}

/// `automation_clip_zone_at` 純粋 hit-test: clip 中央 = Move、 左右 edge = ResizeLeft / ResizeRight。
#[test]
fn automation_clip_zone_at_classifies_zones() {
    let view = make_view();
    let style = ArrangementStyle::default();
    // clip start=4 len=4 → body x = 200 + 4*(600/16) = 350、 w = 4*37.5 = 150 → x range [350, 500]
    let tracks = vec![make_track(1, vec![make_lane(10, vec![make_clip(100, 4.0, 4.0)])])];
    let tops = visible_track_row_tops(&tracks, 0.0, view.track_top, view.track_row_h);
    let lanes = Rect { x: 200.0, y: 0.0, w: 600.0, h: 600.0 };
    // lane y = tops[0] + track_row_h = 32、 clip_y = 32 + 6 = 38、 clip_h = 60 - 12 = 48
    // mid y = 38 + 24 = 62
    let cy_mid = 62.0;

    // mid (Move)
    let hit = automation_clip_zone_at(
        &tracks, &tops, view.track_row_h, view, 0.0, view.header_w, lanes, &style, 425.0, cy_mid,
        style.resize_handle_px,
    );
    assert!(hit.is_some(), "clip 中央が hit する");
    let (key, kind, _r, _b) = hit.unwrap();
    assert_eq!(key, AutomationClipKey { track: 1, lane: 10, clip: 100 });
    assert_eq!(kind, ClipDragKind::Move);

    // 左 edge (ResizeLeft) — clip x = 350、 edge=4 → x in [346, 354) は ResizeLeft
    let hit_left = automation_clip_zone_at(
        &tracks, &tops, view.track_row_h, view, 0.0, view.header_w, lanes, &style, 351.0, cy_mid,
        style.resize_handle_px,
    );
    assert_eq!(hit_left.unwrap().1, ClipDragKind::ResizeLeft);

    // 右 edge (ResizeRight) — clip x_end = 500、 edge=4 → x in [496, 504) は ResizeRight
    let hit_right = automation_clip_zone_at(
        &tracks, &tops, view.track_row_h, view, 0.0, view.header_w, lanes, &style, 499.0, cy_mid,
        style.resize_handle_px,
    );
    assert_eq!(hit_right.unwrap().1, ClipDragKind::ResizeRight);

    // clip 外 (gap) は None
    let hit_gap = automation_clip_zone_at(
        &tracks, &tops, view.track_row_h, view, 0.0, view.header_w, lanes, &style, 100.0, cy_mid,
        style.resize_handle_px,
    );
    assert!(hit_gap.is_none(), "clip 外は None");

    // clip 上下 padding 領域 (clip_y より上) は None
    let hit_pad = automation_clip_zone_at(
        &tracks, &tops, view.track_row_h, view, 0.0, view.header_w, lanes, &style, 425.0, 35.0,
        style.resize_handle_px,
    );
    assert!(hit_pad.is_none(), "padding 領域は None");
}

/// `automation_lane_key_at_y` で cursor y から target lane を返す。
#[test]
fn automation_lane_key_at_y_resolves_target_lane() {
    let view = make_view();
    let style = ArrangementStyle::default();
    // lane 1 (id=10, height_px=60) → y range [32, 92]
    // lane 2 (id=20, height_px=60) → y range [92, 152]
    let tracks = vec![make_track(
        1,
        vec![make_lane(10, vec![]), make_lane(20, vec![])],
    )];
    let tops = visible_track_row_tops(&tracks, 0.0, view.track_top, view.track_row_h);
    let r1 = automation_lane_key_at_y(
        &tracks, &tops, view.track_row_h, 0.0, view.header_w, 200.0, 600.0, &style, 60.0,
    );
    assert_eq!(r1.unwrap().0, AutomationLaneKey { track: 1, lane: 10 });
    let r2 = automation_lane_key_at_y(
        &tracks, &tops, view.track_row_h, 0.0, view.header_w, 200.0, 600.0, &style, 120.0,
    );
    assert_eq!(r2.unwrap().0, AutomationLaneKey { track: 1, lane: 20 });
    // 範囲外 (track row 内 = y=10、 lane 群より下 = y=200)
    let r_top = automation_lane_key_at_y(
        &tracks, &tops, view.track_row_h, 0.0, view.header_w, 200.0, 600.0, &style, 10.0,
    );
    assert!(r_top.is_none(), "track row 内は None");
    let r_bot = automation_lane_key_at_y(
        &tracks, &tops, view.track_row_h, 0.0, view.header_w, 200.0, 600.0, &style, 200.0,
    );
    assert!(r_bot.is_none(), "lane 群より下は None");
}

/// Move drag (clip 中央 press → 横移動 → release) で MoveAutomationClips が 1 件発火。
#[test]
fn move_drag_release_emits_move_automation_clips() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, vec![make_clip(100, 4.0, 4.0)])])];

    // beat_to_px = 600/16 = 37.5
    // clip rect: x range [350, 500]、 mid = 425、 lane y = 32、 mid = 62
    // Move drag: press at (425, 62)、 release at (+75 px, +0) → +2 beat → next start = 6
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(425.0, 62.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_drag(500.0, 62.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(500.0, 62.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );

    assert_eq!(m.moved_clips.len(), 1, "drag→release で MoveAutomationClips 1 件");
    let d = m.moved_clips[0];
    assert_eq!(d.from, AutomationClipKey { track: 1, lane: 10, clip: 100 });
    assert_eq!(d.to_lane, AutomationLaneKey { track: 1, lane: 10 });
    assert!((d.prev_start_beat - 4.0).abs() < 1e-6);
    assert!(
        (d.next_start_beat - 6.0).abs() < 0.1,
        "next_start ≈ 6.0、 actual {}",
        d.next_start_beat
    );
    // Clone variants are not invoked
    assert!(m.cloned_linked.is_empty());
    assert!(m.cloned_indep.is_empty());
    assert!(m.resized_clips.is_empty());
}

/// Move + Ctrl drag → CloneAutomationClipsLinked。
#[test]
fn ctrl_move_drag_emits_clone_linked() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, vec![make_clip(100, 4.0, 4.0)])])];
    let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
    run_frame(
        &mut host,
        &mut m,
        FrameInput { pointer: pointer_press(425.0, 62.0, ctrl), ..FrameInput::default() },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput { pointer: pointer_drag(500.0, 62.0, ctrl), ..FrameInput::default() },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput { pointer: pointer_release(500.0, 62.0, ctrl), ..FrameInput::default() },
    );
    assert_eq!(m.cloned_linked.len(), 1, "Ctrl+drag が CloneAutomationClipsLinked 発火");
    assert!(m.moved_clips.is_empty());
    assert!(m.cloned_indep.is_empty());
}

/// Move + Ctrl + Shift drag → CloneAutomationClipsIndependent。
#[test]
fn ctrl_shift_move_drag_emits_clone_independent() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, vec![make_clip(100, 4.0, 4.0)])])];
    let cs = Modifiers { ctrl: true, shift: true, ..Modifiers::default() };
    run_frame(
        &mut host,
        &mut m,
        FrameInput { pointer: pointer_press(425.0, 62.0, cs), ..FrameInput::default() },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput { pointer: pointer_drag(500.0, 62.0, cs), ..FrameInput::default() },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput { pointer: pointer_release(500.0, 62.0, cs), ..FrameInput::default() },
    );
    assert_eq!(m.cloned_indep.len(), 1, "Ctrl+Shift+drag が CloneAutomationClipsIndependent 発火");
    assert!(m.moved_clips.is_empty());
    assert!(m.cloned_linked.is_empty());
}

/// ResizeRight drag → ResizeAutomationClips が 1 件発火 (next_start == prev_start)。
#[test]
fn resize_right_drag_emits_resize_automation_clips() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, vec![make_clip(100, 4.0, 4.0)])])];
    // clip x_end = 500、 ResizeRight zone = [496, 504)
    let cy_mid = 62.0;
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(499.0, cy_mid, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    // drag to 575 → +75 px = +2 beat → next_len = 6
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_drag(575.0, cy_mid, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(575.0, cy_mid, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    assert_eq!(m.resized_clips.len(), 1);
    let d = m.resized_clips[0];
    assert!((d.next_start - d.prev_start).abs() < 1e-6, "ResizeRight は start 不変");
    assert!(
        (d.next_len - 6.0).abs() < 0.1,
        "next_len ≈ 6.0、 actual {}",
        d.next_len
    );
    assert!(m.moved_clips.is_empty());
}

/// ResizeLeft drag → ResizeAutomationClips (start と len 両方変化)。
#[test]
fn resize_left_drag_emits_resize_automation_clips_both_change() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, vec![make_clip(100, 4.0, 4.0)])])];
    // clip x_start = 350、 ResizeLeft zone = [346, 354)
    let cy_mid = 62.0;
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(351.0, cy_mid, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    // drag to 426 → +75 px = +2 beat → next_start = 6, next_len = 2
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_drag(426.0, cy_mid, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(426.0, cy_mid, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    assert_eq!(m.resized_clips.len(), 1);
    let d = m.resized_clips[0];
    assert!(
        (d.next_start - 6.0).abs() < 0.1,
        "next_start ≈ 6.0、 actual {}",
        d.next_start
    );
    assert!(
        (d.next_len - 2.0).abs() < 0.1,
        "next_len ≈ 2.0、 actual {}",
        d.next_len
    );
}

/// 短 drag (< 4px) は demote されて何も emit しない (Move + Alt なし jitter 閾値、 既存 MIDI clip と同 idiom)。
#[test]
fn short_move_drag_demotes_to_click_no_emit() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, vec![make_clip(100, 4.0, 4.0)])])];
    // 2 px しか動かない (jitter 相当)
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(425.0, 62.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(427.0, 62.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    assert!(m.moved_clips.is_empty(), "短 drag は demote されて MoveAutomationClips 発火しない");
    assert!(m.resized_clips.is_empty());
}

/// 短 drag でも ResizeRight は閾値関係なく commit (resize handle 上の click は意味がない)。
#[test]
fn short_resize_right_drag_still_commits() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, vec![make_clip(100, 4.0, 4.0)])])];
    // 1 px だけ動く resize drag → commit される
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(499.0, 62.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(503.0, 62.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    assert_eq!(m.resized_clips.len(), 1, "ResizeRight 短 drag も commit");
}

/// cross-lane drag (Move) — 上のlane → 下のlane に drop。 release y から target lane を resolve。
#[test]
fn move_drag_across_lanes_resolves_target_lane() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    // 2 lane: lane 10 (y 32-92)、 lane 20 (y 92-152)
    m.tracks = vec![make_track(
        1,
        vec![
            make_lane(10, vec![make_clip(100, 4.0, 4.0)]),
            make_lane(20, vec![]),
        ],
    )];
    // Move drag: press at lane 10 内 (425, 62)、 release at lane 20 内 (425, 122)。
    // 横移動 0、 縦に lane 跨ぎ。 drop lane = lane 20。
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(425.0, 62.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_drag(425.0, 122.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(425.0, 122.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    assert_eq!(m.moved_clips.len(), 1, "lane 跨ぎでも Move 1 件発火");
    let d = m.moved_clips[0];
    assert_eq!(d.from, AutomationClipKey { track: 1, lane: 10, clip: 100 });
    assert_eq!(
        d.to_lane,
        AutomationLaneKey { track: 1, lane: 20 },
        "drop 先 lane = lane 20"
    );
    assert!(
        (d.next_start_beat - 4.0).abs() < 0.1,
        "横移動 0 → next_start ≈ prev_start"
    );
}

/// `automation_clip_rects` が draw 順 (visible-tracks / lane 順) で並ぶ (caller の context_menu_for 用)。
#[test]
fn automation_clip_rects_lists_all_visible_clips_in_draw_order() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(
        1,
        vec![
            make_lane(10, vec![make_clip(100, 0.0, 4.0), make_clip(101, 6.0, 4.0)]),
            make_lane(20, vec![make_clip(200, 2.0, 4.0)]),
        ],
    )];
    run_frame(&mut host, &mut m, FrameInput::default());
    assert_eq!(m.clip_rects.len(), 3, "全 3 clip の rect が並ぶ");
    // draw 順: lane 10 → lane 20、 lane 内は clip id 順 (= 入った順)
    let keys: Vec<AutomationClipKey> = m.clip_rects.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys[0], AutomationClipKey { track: 1, lane: 10, clip: 100 });
    assert_eq!(keys[1], AutomationClipKey { track: 1, lane: 10, clip: 101 });
    assert_eq!(keys[2], AutomationClipKey { track: 1, lane: 20, clip: 200 });
    // 各 rect は描画される lane body 内の縦 padding 適用済範囲 (positive width / height)
    for (_, r) in &m.clip_rects {
        assert!(r.w > 0.0 && r.h > 0.0);
    }
}

/// drag 中は `dragging_automation_clip` が `Some(kind)`、 release frame の **次** frame で `None`。
/// release frame 自身では state.automation_clip_drag を snapshot してから take するため response は Some
/// (= 既存 `dragging` MIDI clip 用 field と同 semantics、 cursor / status indicator が release frame で
/// chatter しない)。
#[test]
fn dragging_automation_clip_reflects_session_state() {
    let mut host: UiHost<ObsModel> = UiHost::no_redraw();
    let mut m = ObsModel::default();
    m.tracks = vec![make_track(1, vec![make_lane(10, vec![make_clip(100, 4.0, 4.0)])])];
    // 初期状態: None
    run_frame(&mut host, &mut m, FrameInput::default());
    assert_eq!(m.dragging_kind, None);

    // press: drag 開始 → Move
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_press(425.0, 62.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    assert_eq!(m.dragging_kind, Some(ClipDragKind::Move));

    // release frame: 既存 MIDI clip と同じく response は Some を返す (同 frame 内の snapshot)
    run_frame(
        &mut host,
        &mut m,
        FrameInput {
            pointer: pointer_release(500.0, 62.0, Modifiers::empty()),
            ..FrameInput::default()
        },
    );
    assert_eq!(m.dragging_kind, Some(ClipDragKind::Move));

    // 次の frame: drag session は既に take 済 → None
    run_frame(&mut host, &mut m, FrameInput::default());
    assert_eq!(m.dragging_kind, None);
}
