//! Alt+drag による snap 一時無効化の regression test。
//!
//! M9 Phase 60 までは drag overlay が `pointer.modifiers.alt` (frame snapshot)、
//! release commit が `nd.any_alt` (sticky) と **二重の真値ソース** を持っていたため
//! 以下の不整合が発生していた:
//!
//! - Alt を mid-drag で離した場合、 overlay と commit が乖離 (overlay は snap、 commit は raw)。
//! - OS event 順序で release frame に `pointer.modifiers.alt = false` が来た場合、 release で
//!   grid に飛ぶ (any_alt が機能しない race)。
//!
//! 修正後は `nd.last_alt` を **単一真値** とし、 overlay と commit の両方が `last_alt` を
//! 読むことで `pointer.modifiers.alt` への直接依存を排除。 `last_alt` は continuation frame で
//! update され release frame では update を skip することで OS event 順序に依存しない。
//!
//! このファイルは arrangement (clip move) と piano_roll (note move) それぞれで
//! 4 シナリオ × 2 widget = 8 ケースを `UiHost::frame` 経由で end-to-end 検証する。

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use daw_ui_core::{
    ArrangementClip, ArrangementEditRequest, ArrangementStyle, ArrangementTrack, ArrangementView,
    Edit, FrameInput, MoveClipDelta, MoveDelta, Note, NotesEditRequest, PianoRollStyle,
    PianoRollView, PointerFrame, SnapConfig, SnapMode, UiHost,
};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Rect, Scene};

// ============================================================
// 共通ヘルパ
// ============================================================

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
/// `Straight { div: 16 }` 用 zoom: 1 拍 = 64 px → grid 幅 800px で 12.5 拍。 1/16 = 0.25 拍
/// (DAW 業界標準: 1/N = N 分音符、 1/16 = 16 分音符 = 0.25 beat)。
/// この zoom で `view.len_beats` を逆算して `lanes.w / view.len_beats == 64` にする。
const ZOOM_X_PX_PER_BEAT: f32 = 64.0;

fn modifiers_alt(alt: bool) -> Modifiers {
    Modifiers { alt, ..Modifiers::empty() }
}

fn pointer_press(x: f32, y: f32, alt: bool) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers: modifiers_alt(alt),
        ..PointerFrame::default()
    }
}

fn pointer_hold(x: f32, y: f32, alt: bool) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_pressed: true,
        modifiers: modifiers_alt(alt),
        ..PointerFrame::default()
    }
}

fn pointer_release(x: f32, y: f32, alt: bool) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_released: true,
        modifiers: modifiers_alt(alt),
        ..PointerFrame::default()
    }
}

// ============================================================
// Arrangement (clip move) シナリオ
// ============================================================

/// arrangement 用 minimal Model。 `last_move` に最新の `MoveClips` の delta 列を捕まえる。
struct ArrModel {
    tracks: Vec<ArrangementTrack>,
    selected: Vec<daw_ui_core::ClipKey>,
    last_move: Option<Vec<MoveClipDelta>>,
}

fn arr_model() -> ArrModel {
    let track = ArrangementTrack {
        id: 1,
        name: Arc::from("t1"),
        muted: false,
        solo: false,
        clips: vec![ArrangementClip {
            id: 100,
            start_beat: 4.0,
            len_beats: 2.0,
            name: Arc::from("c1"),
            color: None,
            share_group_color: None,
            audio_edit: None,
        }],
        volume: 1.0,
        parent_id: None,
        depth: 0,
        collapsed: false,
    };
    ArrModel { tracks: vec![track], selected: Vec::new(), last_move: None }
}

/// `view.len_beats` を `lanes.w / ZOOM_X_PX_PER_BEAT` にする。 `header_w = 0`、 `ruler_h = 0` で
/// rect 全幅が lanes になるので `lanes.w == WIDGET_RECT.w`。
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
    }
}

/// arrangement を 1 frame 走らせる。 `make_edit` は `MoveClips` を `m.last_move` に capture。
fn arr_frame(host: &mut UiHost<ArrModel>, m: &mut ArrModel, input: FrameInput, snap: SnapConfig) {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let view = arr_view(snap);
    let style = ArrangementStyle::default();
    host.frame(m, &mut scene, screen, input, |model, ui| {
        ui.arrangement(
            "arr",
            WIDGET_RECT,
            &model.tracks,
            view,
            &model.selected,
            &[],
            &style,
            |req| match req {
                ArrangementEditRequest::SelectClips { next, .. } => {
                    Edit::mutate(move |mm: &mut ArrModel| {
                        mm.selected = next;
                    })
                }
                ArrangementEditRequest::MoveClips(deltas) => {
                    Edit::mutate(move |mm: &mut ArrModel| {
                        mm.last_move = Some(deltas);
                    })
                }
                _ => Edit::mutate(|_| {}),
            },
        );
    });
}

/// clip (start_beat=4.0, len_beats=2.0) の中央 px 位置 = beat 5.0 → 320 px。
const ARR_CLIP_CENTER_PX: (f32, f32) = (320.0, 16.0);
/// drag 終点 px。 +110 px = +1.71875 拍 (snap unit 1/16=0.25 にちょうど合わない位置)。
const ARR_DRAG_END_PX: (f32, f32) = (430.0, 16.0);
/// 期待 raw delta = 110 / 64 = 1.71875。
const ARR_EXPECTED_RAW_DELTA: f64 = 110.0 / 64.0;
/// 期待 snapped delta = round(1.71875 / 0.25) * 0.25 = round(6.875) * 0.25 = 7 * 0.25 = 1.75。
const ARR_EXPECTED_SNAPPED_DELTA: f64 = 1.75;
/// 開始 start_beat + raw delta = 4.0 + 1.71875 = 5.71875。
const ARR_EXPECTED_RAW_NEW_START: f64 = 4.0 + ARR_EXPECTED_RAW_DELTA;
/// 開始 start_beat + snapped delta = 4.0 + 1.75 = 5.75。
const ARR_EXPECTED_SNAPPED_NEW_START: f64 = 4.0 + ARR_EXPECTED_SNAPPED_DELTA;

const SNAP_16: SnapConfig = SnapConfig {
    mode: SnapMode::Straight { div: 16 },
    enabled: true,
    min_beat_unit: 1.0 / 128.0,
    time_sig: (4, 4),
};

/// Alt 持ち続け → release で raw commit (snap bypass)。
#[test]
fn arr_alt_held_throughout_release_commits_raw() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();

    // Frame 1: press at clip center (alt 押下中)。
    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    // Frame 2: hold-move to 中間点 (alt 押下継続)。
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(380.0, ARR_CLIP_CENTER_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    // Frame 3: hold-move to drag 終点 (alt 押下継続)。
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    // Frame 4: release at 終点 (alt まだ押下、 OS event 順序が「正しい」 ケース)。
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("MoveClips Edit was emitted on release");
    assert_eq!(move_deltas.len(), 1);
    let new_start = move_deltas[0].next_start_beat;
    assert!(
        (new_start - ARR_EXPECTED_RAW_NEW_START).abs() < 1e-9,
        "Alt held throughout: expected raw new_start={ARR_EXPECTED_RAW_NEW_START}, got {new_start}"
    );
}

/// Alt 持ち続け、 ただし OS が release frame で先に ModifiersChanged(alt=false) を dispatch
/// する race を再現。 `last_alt` が release 直前 frame で `true` のまま保持され、 raw commit。
#[test]
fn arr_alt_release_frame_modifier_race_still_commits_raw() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();

    // Frame 1-3: alt 押下中で press / hold / hold。
    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(380.0, ARR_CLIP_CENTER_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    // Frame 4: release frame に **alt=false** が混入 (OS が ModifiersChanged を MouseInput より
    // 先に dispatch した想定)。 last_alt は frame 3 で true のまま保持されるべき。
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("MoveClips Edit was emitted on release");
    let new_start = move_deltas[0].next_start_beat;
    assert!(
        (new_start - ARR_EXPECTED_RAW_NEW_START).abs() < 1e-9,
        "OS race (alt=false at release): last_alt should preserve true, expected raw new_start={ARR_EXPECTED_RAW_NEW_START}, got {new_start}"
    );
}

/// Alt 一切なし → release で snap commit (regression なし)。
#[test]
fn arr_no_alt_release_commits_snapped() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(380.0, ARR_CLIP_CENTER_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("MoveClips Edit was emitted on release");
    let new_start = move_deltas[0].next_start_beat;
    assert!(
        (new_start - ARR_EXPECTED_SNAPPED_NEW_START).abs() < 1e-9,
        "No alt: expected snapped new_start={ARR_EXPECTED_SNAPPED_NEW_START}, got {new_start}"
    );
}

/// Alt 押下で **短 drag (< 4px)** でも release で free commit (jitter 閾値も Alt なら skip)。
#[test]
fn arr_alt_jitter_drag_skips_click_demotion_and_commits_raw() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();

    // press at clip center (alt 押下)
    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    // hold +2px (jitter range の短 drag、 alt 押下)
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_CLIP_CENTER_PX.0 + 2.0, ARR_CLIP_CENTER_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_CLIP_CENTER_PX.0 + 2.0, ARR_CLIP_CENTER_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m
        .last_move
        .expect("Alt 押下 jitter drag (2px) でも MoveClips Edit が発行されるべき");
    assert_eq!(move_deltas.len(), 1);
    let raw_short_delta = 2.0_f64 / 64.0;
    let expected_new_start = 4.0 + raw_short_delta;
    let new_start = move_deltas[0].next_start_beat;
    assert!(
        (new_start - expected_new_start).abs() < 1e-9,
        "Alt jitter drag: expected raw new_start={expected_new_start}, got {new_start}"
    );
}

/// Alt なし + **jitter range (< 4px)** drag → click 化される (mouse jitter ignore)。
#[test]
fn arr_no_alt_jitter_drag_is_demoted_to_click() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_CLIP_CENTER_PX.0 + 2.0, ARR_CLIP_CENTER_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_CLIP_CENTER_PX.0 + 2.0, ARR_CLIP_CENTER_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(
        m.last_move.is_none(),
        "Alt なし jitter drag (2px) は click 化されるべき"
    );
}

/// Alt なし + **jitter 範囲を超える短 drag (>= 4px)** → Move Edit 発行 + snap 適用。
/// 旧実装の 16px 閾値は過剰で、 5-15px の「短いけど明確な drag」 を click に格下げしていた。
#[test]
fn arr_no_alt_short_drag_above_jitter_commits_snapped() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();

    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_CLIP_CENTER_PX.0 + 8.0, ARR_CLIP_CENTER_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_CLIP_CENTER_PX.0 + 8.0, ARR_CLIP_CENTER_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("8px drag は Move Edit を発行すべき (jitter 4px 超)");
    let new_start = move_deltas[0].next_start_beat;
    // raw_delta = 8/64 = 0.125 拍。 absolute snap: pivot=4.0、 raw_end=4.125。
    // snap unit 1/16 = 0.25 → round(4.125/0.25) = round(16.5) = 17 → 17*0.25 = 4.25。
    // adjusted_delta = 0.25、 new_start = 4.0 + 0.25 = 4.25。
    let expected = 4.0 + 0.25;
    assert!(
        (new_start - expected).abs() < 1e-9,
        "Alt なし short drag (8px) snapped: expected new_start={expected}, got {new_start}"
    );
}

/// 絶対 snap: anchor が grid 外に既にずれている (例: 前回 Alt+drag で +0.078 拍ずれた) 状態から
/// Alt なし drag → release で **anchor 0 が grid 上に着地する** こと。 旧 delta-snap 実装は
/// raw_delta だけを round するので、 anchor のずれが永久残った (user 報告の本丸)。
#[test]
fn arr_no_alt_drag_from_off_grid_anchor_lands_on_grid() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();
    // anchor を grid 外 (4.078 拍 = 旧 4.0 から +0.078 ずれた状態) に設定。
    m.tracks[0].clips[0].start_beat = 4.078_125; // = 4.0 + 5/64

    // Alt なしで +30px (= 0.46875 拍) drag。 raw 終点 = 4.078125 + 0.46875 = 4.546875。
    // 1/16 grid (0.25 倍数) で round → round(18.1875) = 18 → 4.5。
    // adjusted_delta = 4.5 - 4.078125 = 0.421875。
    // press は clip 中央 ((4.078125 + 1.0) * 64 = 325.0 px) に置く (resize handle 4px 内側を避ける)。
    let press_x = (4.078_125_f32 + 1.0) * ZOOM_X_PX_PER_BEAT;
    let mut input = FrameInput::default();
    input.pointer = pointer_press(press_x, 16.0, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(press_x + 30.0, 16.0, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(press_x + 30.0, 16.0, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("MoveClips should be emitted");
    let new_start = move_deltas[0].next_start_beat;
    let expected = 4.5_f64; // 4.078125 + 0.421875 = 4.5 (grid 上、 snap unit 0.25)
    assert!(
        (new_start - expected).abs() < 1e-9,
        "off-grid anchor + Alt なし drag は new_start={expected} (grid 上) に着地すべき、 got {new_start}"
    );
    // 重要: anchor のずれ (0.078125) が解消されて grid に吸着したことを assert。
    let grid_unit = 0.25_f64; // 1/16 note = 0.25 beat (DAW 業界標準)
    let on_grid_residual = (new_start / grid_unit).round() * grid_unit;
    assert!(
        (new_start - on_grid_residual).abs() < 1e-9,
        "new_start={new_start} は 1/16 grid 上であるべき"
    );
}

/// Alt 押下で press → mid-drag で alt 離す → release で snap commit (UX 連続性)。
#[test]
fn arr_alt_released_mid_drag_release_commits_snapped() {
    let mut host: UiHost<ArrModel> = UiHost::no_redraw();
    let mut m = arr_model();

    // Frame 1: press at center (alt 押下)。
    let mut input = FrameInput::default();
    input.pointer = pointer_press(ARR_CLIP_CENTER_PX.0, ARR_CLIP_CENTER_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    // Frame 2: hold mid (alt 押下継続)。
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(380.0, ARR_CLIP_CENTER_PX.1, true);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    // Frame 3: hold 終点 (alt 離す = false)。 last_alt = false に更新。
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);
    // Frame 4: release (alt = false)。
    let mut input = FrameInput::default();
    input.pointer = pointer_release(ARR_DRAG_END_PX.0, ARR_DRAG_END_PX.1, false);
    arr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("MoveClips Edit was emitted on release");
    let new_start = move_deltas[0].next_start_beat;
    assert!(
        (new_start - ARR_EXPECTED_SNAPPED_NEW_START).abs() < 1e-9,
        "Alt released mid-drag: expected snapped new_start={ARR_EXPECTED_SNAPPED_NEW_START}, got {new_start}"
    );
}

// ============================================================
// Piano roll (note move) シナリオ
// ============================================================

/// piano_roll 用 minimal Model。 `last_move` に最新の `Move` の delta 列を捕まえる。
struct PrModel {
    notes: Vec<Note>,
    selected: Vec<daw_ui_core::NoteId>,
    last_move: Option<Vec<MoveDelta>>,
}

fn pr_model() -> PrModel {
    let note = Note {
        id: 42,
        start_beat: 4.0,
        len_beats: 2.0,
        pitch: 60,
        velocity: 100,
        lyric: None,
    };
    PrModel { notes: vec![note], selected: vec![42], last_move: None }
}

fn pr_view(snap: SnapConfig) -> PianoRollView {
    PianoRollView {
        start_beat: 0.0,
        // keyboard_w = 0 にして grid.w == WIDGET_RECT.w。 zoom 64 → len_beats = 800/64 = 12.5。
        len_beats: f64::from(WIDGET_RECT.w / ZOOM_X_PX_PER_BEAT),
        pitch_top: 72.0,
        pitch_visible: 24.0,
        keyboard_w: 0.0,
        notes_generation: 0,
        velocity_lane_h: 0.0,
        playhead_beat: None,
        ruler_h: 0.0,
        bpm: 120.0,
        time_sig: (4, 4),
        snap,
    }
}

/// note (start_beat=4.0, len_beats=2.0) → grid x: 4.0 * 64 = 256, len = 128。
/// note 中央 = 256 + 64 = 320 px。 pitch=60、 pitch_top=72、 pitch_visible=24 → row_h = 600/24=25 px、
/// note y = (72 - 60) * 25 = 300 px。
const PR_NOTE_CENTER_PX: (f32, f32) = (320.0, 300.0);
/// drag 終点 px (横方向のみ)。 +110 px = +1.71875 拍 (arrangement と同じ)。
const PR_DRAG_END_PX: (f32, f32) = (430.0, 300.0);
const PR_EXPECTED_RAW_NEW_START: f64 = 4.0 + ARR_EXPECTED_RAW_DELTA;
const PR_EXPECTED_SNAPPED_NEW_START: f64 = 4.0 + ARR_EXPECTED_SNAPPED_DELTA;

fn pr_frame(host: &mut UiHost<PrModel>, m: &mut PrModel, input: FrameInput, snap: SnapConfig) {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let view = pr_view(snap);
    let style = PianoRollStyle::default();
    host.frame(m, &mut scene, screen, input, |model, ui| {
        ui.piano_roll("pr", WIDGET_RECT, &model.notes, view, &model.selected, &style, |req| {
            match req {
                NotesEditRequest::Select { next, .. } => Edit::mutate(move |mm: &mut PrModel| {
                    mm.selected = next;
                }),
                NotesEditRequest::Move(deltas) => Edit::mutate(move |mm: &mut PrModel| {
                    mm.last_move = Some(deltas);
                }),
                _ => Edit::mutate(|_| {}),
            }
        });
    });
}

#[test]
fn pr_alt_held_throughout_release_commits_raw() {
    let mut host: UiHost<PrModel> = UiHost::no_redraw();
    let mut m = pr_model();

    let mut input = FrameInput::default();
    input.pointer = pointer_press(PR_NOTE_CENTER_PX.0, PR_NOTE_CENTER_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(380.0, PR_NOTE_CENTER_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(PR_DRAG_END_PX.0, PR_DRAG_END_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(PR_DRAG_END_PX.0, PR_DRAG_END_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("Move Edit was emitted on release");
    assert_eq!(move_deltas.len(), 1);
    let new_start = move_deltas[0].3; // (id, prev_start, prev_pitch, next_start, next_pitch)
    assert!(
        (new_start - PR_EXPECTED_RAW_NEW_START).abs() < 1e-9,
        "Alt held throughout (piano_roll): expected raw new_start={PR_EXPECTED_RAW_NEW_START}, got {new_start}"
    );
}

#[test]
fn pr_alt_release_frame_modifier_race_still_commits_raw() {
    let mut host: UiHost<PrModel> = UiHost::no_redraw();
    let mut m = pr_model();

    let mut input = FrameInput::default();
    input.pointer = pointer_press(PR_NOTE_CENTER_PX.0, PR_NOTE_CENTER_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(380.0, PR_NOTE_CENTER_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(PR_DRAG_END_PX.0, PR_DRAG_END_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);

    // Race: release frame で alt=false が来る。
    let mut input = FrameInput::default();
    input.pointer = pointer_release(PR_DRAG_END_PX.0, PR_DRAG_END_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("Move Edit was emitted on release");
    let new_start = move_deltas[0].3;
    assert!(
        (new_start - PR_EXPECTED_RAW_NEW_START).abs() < 1e-9,
        "OS race (piano_roll): expected raw new_start={PR_EXPECTED_RAW_NEW_START}, got {new_start}"
    );
}

#[test]
fn pr_no_alt_release_commits_snapped() {
    let mut host: UiHost<PrModel> = UiHost::no_redraw();
    let mut m = pr_model();

    let mut input = FrameInput::default();
    input.pointer = pointer_press(PR_NOTE_CENTER_PX.0, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(380.0, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(PR_DRAG_END_PX.0, PR_DRAG_END_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(PR_DRAG_END_PX.0, PR_DRAG_END_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("Move Edit was emitted on release");
    let new_start = move_deltas[0].3;
    assert!(
        (new_start - PR_EXPECTED_SNAPPED_NEW_START).abs() < 1e-9,
        "No alt (piano_roll): expected snapped new_start={PR_EXPECTED_SNAPPED_NEW_START}, got {new_start}"
    );
}

/// piano_roll: Alt 押下 + jitter drag (< 4px) → Move Edit が発行される。
#[test]
fn pr_alt_jitter_drag_skips_click_demotion_and_commits_raw() {
    let mut host: UiHost<PrModel> = UiHost::no_redraw();
    let mut m = pr_model();

    let mut input = FrameInput::default();
    input.pointer = pointer_press(PR_NOTE_CENTER_PX.0, PR_NOTE_CENTER_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(PR_NOTE_CENTER_PX.0 + 2.0, PR_NOTE_CENTER_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(PR_NOTE_CENTER_PX.0 + 2.0, PR_NOTE_CENTER_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m
        .last_move
        .expect("Alt 押下 jitter drag (2px、 piano_roll) でも Move Edit が発行されるべき");
    assert_eq!(move_deltas.len(), 1);
    let raw_short_delta = 2.0_f64 / 64.0;
    let expected_new_start = 4.0 + raw_short_delta;
    let new_start = move_deltas[0].3;
    assert!(
        (new_start - expected_new_start).abs() < 1e-9,
        "Alt jitter drag (piano_roll): expected raw new_start={expected_new_start}, got {new_start}"
    );
}

/// piano_roll: Alt なし + jitter (< 4px) → click 化 (mouse jitter ignore)。
#[test]
fn pr_no_alt_jitter_drag_is_demoted_to_click() {
    let mut host: UiHost<PrModel> = UiHost::no_redraw();
    let mut m = pr_model();

    let mut input = FrameInput::default();
    input.pointer = pointer_press(PR_NOTE_CENTER_PX.0, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(PR_NOTE_CENTER_PX.0 + 2.0, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(PR_NOTE_CENTER_PX.0 + 2.0, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);

    assert!(
        m.last_move.is_none(),
        "Alt なし jitter drag (piano_roll、 2px) は click 化されるべき"
    );
}

/// piano_roll: Alt なし + jitter 超え (>= 4px) short drag → Move + snap commit。
#[test]
fn pr_no_alt_short_drag_above_jitter_commits_snapped() {
    let mut host: UiHost<PrModel> = UiHost::no_redraw();
    let mut m = pr_model();

    let mut input = FrameInput::default();
    input.pointer = pointer_press(PR_NOTE_CENTER_PX.0, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(PR_NOTE_CENTER_PX.0 + 8.0, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(PR_NOTE_CENTER_PX.0 + 8.0, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m
        .last_move
        .expect("Alt なし 8px drag (piano_roll) は Move Edit を発行すべき");
    let new_start = move_deltas[0].3;
    // arr 版と同じ: pivot=4.0、 raw_end=4.125、 snap unit 0.25 → 4.25。
    let expected = 4.0 + 0.25;
    assert!(
        (new_start - expected).abs() < 1e-9,
        "piano_roll Alt なし short drag (8px): expected new_start={expected}, got {new_start}"
    );
}

/// piano_roll: 絶対 snap (off-grid anchor → drag → grid 着地)。
#[test]
fn pr_no_alt_drag_from_off_grid_anchor_lands_on_grid() {
    let mut host: UiHost<PrModel> = UiHost::no_redraw();
    let mut m = pr_model();
    m.notes[0].start_beat = 4.078_125;

    let press_x = (4.078_125_f32 + 1.0) * ZOOM_X_PX_PER_BEAT;
    let mut input = FrameInput::default();
    input.pointer = pointer_press(press_x, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(press_x + 30.0, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(press_x + 30.0, PR_NOTE_CENTER_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("Move should be emitted");
    let new_start = move_deltas[0].3;
    // arr 版と同じ: 4.078125 + 0.46875 = 4.546875、 snap unit 0.25 で round → 4.5。
    let expected = 4.5_f64;
    assert!(
        (new_start - expected).abs() < 1e-9,
        "piano_roll off-grid anchor: expected new_start={expected}, got {new_start}"
    );
}

#[test]
fn pr_alt_released_mid_drag_release_commits_snapped() {
    let mut host: UiHost<PrModel> = UiHost::no_redraw();
    let mut m = pr_model();

    let mut input = FrameInput::default();
    input.pointer = pointer_press(PR_NOTE_CENTER_PX.0, PR_NOTE_CENTER_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(380.0, PR_NOTE_CENTER_PX.1, true);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_hold(PR_DRAG_END_PX.0, PR_DRAG_END_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);
    let mut input = FrameInput::default();
    input.pointer = pointer_release(PR_DRAG_END_PX.0, PR_DRAG_END_PX.1, false);
    pr_frame(&mut host, &mut m, input, SNAP_16);

    let move_deltas = m.last_move.expect("Move Edit was emitted on release");
    let new_start = move_deltas[0].3;
    assert!(
        (new_start - PR_EXPECTED_SNAPPED_NEW_START).abs() < 1e-9,
        "Alt released mid-drag (piano_roll): expected snapped new_start={PR_EXPECTED_SNAPPED_NEW_START}, got {new_start}"
    );
}
