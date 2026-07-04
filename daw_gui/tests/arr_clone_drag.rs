//! arrangement clip の **drag-modifier-aware EditRequest** (#019) regression test。
//!
//! 動作仕様:
//! - **Ctrl + drag** → `CloneClipsLinked(deltas)` (共有コピー意図、 daw_01 で content_id 共有)
//! - **Ctrl + Shift + drag** → `CloneClipsIndependent(deltas)` (独立コピー意図、 daw_01 で content fork)
//! - 修飾なし / Alt のみ → `MoveClips(deltas)` (現状動作維持)
//! - **modifier 判定 = drag release 時** (drag 開始時ではない、 mid-drag で意図変更可)
//! - Ctrl/Shift も winit 0.30 の `ModifiersChanged` が `MouseInput(Released)` より先に届く
//!   race を起こすので、 `last_ctrl` / `last_shift` を `last_alt` と同じ仕組み
//!   (continuation で update / release で skip) で保持する
//! - **resize (Left/Right)** は modifier 関与せず常に `ResizeClips` (Ctrl 上の resize は意味なし)
//! - **Alt は直交**: Ctrl+Alt+drag = `CloneClipsLinked` + snap 一時無効
//! - **short-click demote** (< 4px、 Move + Alt なし) は既存通り `SelectClips`、 Ctrl は demote 条件に
//!   入れず Ctrl+click は selection 変更 (Ableton/Bitwig と同じ)

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use daw_gui::widgets::arrangement::{arrangement, ArrangementClip, ArrangementEditRequest, ArrangementStyle, ArrangementTrack, ArrangementView, MoveClipDelta, TrackKind};
use daw_ui_core::{Edit, FrameInput, PointerFrame, SnapConfig, SnapMode, UiHost};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Rect, Scene};

// ============================================================
// 共通ヘルパ
// ============================================================

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
const ZOOM_X_PX_PER_BEAT: f32 = 64.0;

fn modifiers_csa(ctrl: bool, shift: bool, alt: bool) -> Modifiers {
    Modifiers { ctrl, shift, alt, ..Modifiers::empty() }
}

fn pointer_press(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers: m,
        ..PointerFrame::default()
    }
}

fn pointer_hold(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_pressed: true,
        modifiers: m,
        ..PointerFrame::default()
    }
}

fn pointer_release(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_released: true,
        modifiers: m,
        ..PointerFrame::default()
    }
}

// ============================================================
// minimal arrangement Model
// ============================================================

struct ArrModel {
    tracks: Vec<ArrangementTrack>,
    selected: Vec<daw_gui::widgets::arrangement::ClipKey>,
    last_move: Option<Vec<MoveClipDelta>>,
    last_clone_linked: Option<Vec<MoveClipDelta>>,
    last_clone_indep: Option<Vec<MoveClipDelta>>,
    last_resize: Option<Vec<daw_gui::widgets::arrangement::ResizeClipDelta>>,
    last_select: Option<Vec<daw_gui::widgets::arrangement::ClipKey>>,
}

fn arr_model() -> ArrModel {
    let track = ArrangementTrack {
        id: 1,
        name: Arc::from("t1"),
        muted: false,
        solo: false,
        armed: false,
        clips: vec![ArrangementClip {
            id: 100,
            start_beat: 4.0,
            len_beats: 2.0,
            name: Arc::from("c1"),
            color: None,
            share_group_color: None,
            audio_edit: None,
            thumbnail: None,
            in_active_group: false,
            muted: false,
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
    };
    ArrModel {
        tracks: vec![track],
        selected: Vec::new(),
        last_move: None,
        last_clone_linked: None,
        last_clone_indep: None,
        last_resize: None,
        last_select: None,
    }
}

fn arr_view(snap: SnapConfig) -> ArrangementView {
    ArrangementView {
        start_beat: 0.0,
        len_beats: f64::from(WIDGET_RECT.w / ZOOM_X_PX_PER_BEAT),
        track_top: 0.0,
        tracks_visible: 8.0,
        track_row_h: 32.0,
        header_w: 0.0,
        ruler_h: 0.0,
        playhead_beat: None,
        loop_range: None,
        data_generation: 0,
        bpm: 120.0,
        time_sig: (4, 4),
        snap,
        arranger_lane_h: 0.0,
    }
}

fn arr_frame(host: &mut UiHost<ArrModel>, m: &mut ArrModel, input: FrameInput, snap: SnapConfig) {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let view = arr_view(snap);
    let style = ArrangementStyle::default();
    host.frame(m, &mut scene, screen, input, |model, ui| {
        arrangement(ui, 
            "arr",
            WIDGET_RECT,
            &model.tracks,
            &[],
            view,
            &model.selected,
            &[],
            &[],
            &[],
            &style,
            None,
            |req| match req {
                ArrangementEditRequest::SelectClips { next, .. } => {
                    Edit::mutate(move |mm: &mut ArrModel| {
                        mm.last_select = Some(next.clone());
                        mm.selected = next;
                    })
                }
                ArrangementEditRequest::MoveClips(deltas) => {
                    Edit::mutate(move |mm: &mut ArrModel| {
                        mm.last_move = Some(deltas);
                    })
                }
                ArrangementEditRequest::CloneClipsLinked(deltas) => {
                    Edit::mutate(move |mm: &mut ArrModel| {
                        mm.last_clone_linked = Some(deltas);
                    })
                }
                ArrangementEditRequest::CloneClipsIndependent(deltas) => {
                    Edit::mutate(move |mm: &mut ArrModel| {
                        mm.last_clone_indep = Some(deltas);
                    })
                }
                ArrangementEditRequest::ResizeClips(deltas) => {
                    Edit::mutate(move |mm: &mut ArrModel| {
                        mm.last_resize = Some(deltas);
                    })
                }
                _ => Edit::mutate(|_| {}),
            },
        );
    });
}

const ARR_CLIP_CENTER_PX: (f32, f32) = (320.0, 16.0);
const ARR_DRAG_END_PX: (f32, f32) = (430.0, 16.0);
const ARR_EXPECTED_RAW_DELTA: f64 = 110.0 / 64.0;
const ARR_EXPECTED_SNAPPED_DELTA: f64 = 1.75;
const ARR_EXPECTED_RAW_NEW_START: f64 = 4.0 + ARR_EXPECTED_RAW_DELTA;
const ARR_EXPECTED_SNAPPED_NEW_START: f64 = 4.0 + ARR_EXPECTED_SNAPPED_DELTA;

const SNAP_16: SnapConfig = SnapConfig {
    mode: SnapMode::Straight { div: 16 },
    enabled: true,
    min_beat_unit: 1.0 / 128.0,
    time_sig: (4, 4),
};

// ============================================================
// Q1 / Q2: Ctrl+drag → CloneClipsLinked、 Ctrl+Shift+drag → CloneClipsIndependent
// ============================================================

/// Ctrl 押下中の drag → release で `CloneClipsLinked(deltas)` が発火する。
/// `MoveClips` は **発火しない** (排他)。
#[test]
fn arr_ctrl_drag_emits_clone_linked() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    let ctrl = modifiers_csa(true, false, false);

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(380.0, ARR_CLIP_CENTER_PX.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(m.last_move.is_none(), "Ctrl+drag では MoveClips は発火しない");
    let deltas = m.last_clone_linked.expect("Ctrl+drag → CloneClipsLinked が発火すべき");
    assert_eq!(deltas.len(), 1);
    let new_start = deltas[0].next_start_beat;
    assert!(
        (new_start - ARR_EXPECTED_SNAPPED_NEW_START).abs() < 1e-9,
        "Ctrl+drag (Alt なし) は snap 適用、 expected={ARR_EXPECTED_SNAPPED_NEW_START}, got {new_start}"
    );
    // source clip identity が伝わる (daw_01 は from で content_id を引く)。
    assert_eq!(deltas[0].from.clip, 100);
    assert_eq!(deltas[0].from.track, 1);
    // prev_start_beat = source clip 位置 (残置)、 doc 記載通り。
    assert!((deltas[0].prev_start_beat - 4.0).abs() < 1e-9);
}

/// Ctrl+Shift+drag → release で `CloneClipsIndependent(deltas)` が発火する。
#[test]
fn arr_ctrl_shift_drag_emits_clone_independent() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    let cs = modifiers_csa(true, true, false);

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, cs);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, cs);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, cs);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(m.last_move.is_none());
    assert!(
        m.last_clone_linked.is_none(),
        "Ctrl+Shift では Linked は発火しない (Independent と排他)"
    );
    let deltas = m
        .last_clone_indep
        .expect("Ctrl+Shift+drag → CloneClipsIndependent が発火すべき");
    assert_eq!(deltas.len(), 1);
    let new_start = deltas[0].next_start_beat;
    assert!(
        (new_start - ARR_EXPECTED_SNAPPED_NEW_START).abs() < 1e-9,
        "Ctrl+Shift+drag (Alt なし) は snap 適用、 expected={ARR_EXPECTED_SNAPPED_NEW_START}, got {new_start}"
    );
}

// ============================================================
// release frame modifier race (last_ctrl / last_shift 保持)
// ============================================================

/// Ctrl 押下中の drag、 release frame で `ctrl=false` (OS race) → `last_ctrl` が
/// release 直前 frame の値を保持して `CloneClipsLinked` を発火する。
#[test]
fn arr_ctrl_release_frame_modifier_race_still_emits_clone_linked() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    let ctrl = modifiers_csa(true, false, false);
    let none = modifiers_csa(false, false, false);

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    // Race: release frame に ctrl=false が混入。
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, none);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(
        m.last_clone_linked.is_some(),
        "OS race (ctrl=false at release) でも last_ctrl 保持で CloneClipsLinked 発火すべき"
    );
    assert!(m.last_move.is_none(), "Move には fallback しない");
}

/// Ctrl+Shift 押下中の drag、 release frame で `shift=false` (OS race) → `last_shift` が
/// 直前 frame 値 (true) を保持して `CloneClipsIndependent` を発火する。
#[test]
fn arr_ctrl_shift_release_frame_modifier_race_still_emits_clone_independent() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    let cs = modifiers_csa(true, true, false);
    let ctrl_only = modifiers_csa(true, false, false);

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, cs);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, cs);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    // Race: release frame で shift だけ false に化ける。
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, ctrl_only);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(
        m.last_clone_indep.is_some(),
        "OS race (shift=false at release) でも last_shift 保持で CloneClipsIndependent 発火すべき"
    );
    assert!(m.last_clone_linked.is_none());
}

// ============================================================
// short-click demote (Ctrl+click は selection、 clone ではない)
// ============================================================

/// Ctrl+click (< 4px) は demote されて `CloneClipsLinked` 発火しない。 selection 経路に流れる。
#[test]
fn arr_ctrl_short_click_does_not_emit_clone() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    let ctrl = modifiers_csa(true, false, false);

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_CLIP_CENTER_PX.0 + 2.0, ARR_CLIP_CENTER_PX.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_CLIP_CENTER_PX.0 + 2.0, ARR_CLIP_CENTER_PX.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(
        m.last_clone_linked.is_none(),
        "Ctrl+click (jitter 範囲内) は CloneClipsLinked を発火しない (Ableton 流: selection が priority)"
    );
    assert!(m.last_clone_indep.is_none());
    assert!(m.last_move.is_none());
    // selection が更新される (clip が selected に入る)。
    let sel = m.last_select.expect("Ctrl+click は selection を発火する");
    assert_eq!(sel.len(), 1);
    assert_eq!(sel[0].clip, 100);
}

// ============================================================
// Alt との直交性: Ctrl+Alt+drag = CloneClipsLinked + raw position (snap 一時無効)
// ============================================================

/// Ctrl+Alt+drag → release で `CloneClipsLinked` + **raw 位置** (Alt が snap を bypass)。
/// Alt は Ctrl 系と独立に動く設計の検証。
#[test]
fn arr_ctrl_alt_drag_emits_clone_linked_with_raw_position() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    let ctrl_alt = modifiers_csa(true, false, true);

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, ctrl_alt);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, ctrl_alt);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, ctrl_alt);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    let deltas = m
        .last_clone_linked
        .expect("Ctrl+Alt+drag → CloneClipsLinked が発火すべき (Alt は直交)");
    let new_start = deltas[0].next_start_beat;
    assert!(
        (new_start - ARR_EXPECTED_RAW_NEW_START).abs() < 1e-9,
        "Ctrl+Alt+drag は snap bypass、 expected raw new_start={ARR_EXPECTED_RAW_NEW_START}, got {new_start}"
    );
}

// ============================================================
// regression: 修飾なし drag は MoveClips のまま
// ============================================================

/// 修飾なし drag (regression check) → `MoveClips` のまま、 clone variant は発火しない。
#[test]
fn arr_no_modifier_drag_still_emits_move() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    let none = modifiers_csa(false, false, false);

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, none);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, none);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, none);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(m.last_clone_linked.is_none());
    assert!(m.last_clone_indep.is_none());
    let deltas = m.last_move.expect("修飾なしは MoveClips を発火 (regression)");
    let new_start = deltas[0].next_start_beat;
    assert!((new_start - ARR_EXPECTED_SNAPPED_NEW_START).abs() < 1e-9);
}

// ============================================================
// resize は Ctrl 関与せず常に ResizeClips
// ============================================================

/// Ctrl + resize handle drag → `ResizeClips` のまま (Ctrl は resize 中無視)。
/// clip = (start_beat=4.0, len_beats=2.0) → 右端 = beat 6.0 = 384 px。 resize handle 内 = 384±4 px。
#[test]
fn arr_ctrl_drag_on_resize_handle_still_emits_resize() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    let ctrl = modifiers_csa(true, false, false);
    // 右 resize handle 上 (clip 右端 = 384 px、 handle 4px) で press。
    let resize_press = (382.0_f32, 16.0_f32);

    let mut input = FrameInput::default();
    input.pointer = pointer_press(resize_press.0, resize_press.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(resize_press.0 + 32.0, resize_press.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(resize_press.0 + 32.0, resize_press.1, ctrl);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(
        m.last_clone_linked.is_none(),
        "resize handle 上の Ctrl+drag は clone を発火しない (resize 中 Ctrl は無視)"
    );
    assert!(m.last_clone_indep.is_none());
    assert!(m.last_resize.is_some(), "ResizeClips が発火すべき");
}

// ============================================================
// #021: Ctrl+Shift+drag が rect select に化けない (= SelectClips を emit しない)
// ============================================================

/// Ctrl+Shift+drag は **clip_drag セッションのみ** 起動し、 rect select セッションを
/// 同時起動してはいけない。 起動してしまうと release 時に `SelectClips` が `CloneClipsIndependent`
/// の後から push され、 daw_01 側 model で「clone 後 selection 上書き」 = 「rect select に
/// 化けて clone が起きていない」 ように見える symptom が出る。
///
/// 検証は (a) `last_clone_indep` が Some (b) `last_select` が None の両方。
/// drag 経路に **vertical 14px** を入れて drag rect h>0 にしている (h=0 だと rects_intersect が
/// 弾いて、 たとえ rect select が活きていても SelectClips が発火しない偶然の test pass を起こすため)。
#[test]
fn arr_ctrl_shift_drag_does_not_trigger_rect_select() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    let cs = modifiers_csa(true, true, false);

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, cs);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    // continuation で h>0 になるよう vertical も移動 (drag rect が clip rect と intersect する状況)。
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(380.0, 24.0, cs);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, 30.0, cs);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, 30.0, cs);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(
        m.last_clone_indep.is_some(),
        "Ctrl+Shift+drag → CloneClipsIndependent が emit されるべき"
    );
    assert!(
        m.last_select.is_none(),
        "Ctrl+Shift+drag は rect select に化けず SelectClips を emit してはいけない (#021)"
    );
}

/// **Shift-only** drag (Ctrl なし) は引き続き rect select として動作し、 clip 範囲交差で
/// `SelectClips` を emit する (#021 fix の回帰防止: ctrl 除外条件が shift-only を
/// 巻き添えで殺していないことを確認)。
#[test]
fn arr_shift_only_drag_still_triggers_rect_select() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    let shift = modifiers_csa(false, true, false);

    // clip rect = (256, 0, 128, 32)。 clip 左の空白 (40, 8) から press → clip 中央を巻き込む
    // (300, 24) まで drag。 drag rect = (40, 8, 260, 16)、 clip rect と intersect する。
    // press は clip 上ではない (x=40 は clip 左端 256 より左) ので clip_hit が None、
    // 結果 clip_drag セッションは起動せず rect_select 経路のみが通る。
    let mut input = FrameInput::default();
    input.pointer = pointer_press(40.0, 8.0, shift);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(200.0, 16.0, shift);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(300.0, 24.0, shift);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(
        m.last_clone_indep.is_none(),
        "Shift-only drag は CloneClipsIndependent を emit しない"
    );
    assert!(
        m.last_clone_linked.is_none(),
        "Shift-only drag は CloneClipsLinked を emit しない"
    );
    let sel = m.last_select.expect(
        "Shift-only drag は rect select で SelectClips を emit する (#021 regression check)",
    );
    assert_eq!(sel.len(), 1, "drag rect 内の clip 1 つが selected に入る");
    assert_eq!(sel[0].clip, 100);
}
