//! M14 Phase 127 (daw_01 #105): Arranger レーン (section) の drag → `ArrangementEditRequest` emit を
//! `UiHost::frame` 経由で end-to-end 検証する。 widget は section を一切 mutate せず、 press →
//! (hold) → release のフレーム列で release frame に 1 件だけ意図を emit する (commit-by-release)。
//!
//! 検証ケース:
//! - 帯中央 drag → `MoveSection`、 Ctrl+drag → `DuplicateSection`
//! - 右端 drag → `ResizeSection` (len のみ)、 左端 drag → `ResizeSection` (start/len 両方)
//! - 帯中央の短 click (< 4px) → `SetPlayheadBeat` (帯ジャンプに demote)
//! - 空きレーンの範囲 drag → `CreateSection`

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use daw_gui::widgets::arrangement::{arrangement, ArrangementEditRequest, ArrangementStyle, ArrangementTrack, ArrangementView, SectionView, SelectModifier, TrackKind};
use daw_ui_core::{Edit, FrameInput, PointerFrame, SnapConfig, SnapMode, UiHost};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Rect, Scene};

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
/// 1 拍 = 64 px。 `header_w = 0` / `ruler_h = 0` なので arranger / lanes 幅 = 800 → len_beats = 12.5。
const ZOOM_X_PX_PER_BEAT: f32 = 64.0;
const ARRANGER_LANE_H: f32 = 20.0;
/// snap 無効 (raw delta で決定的に検証)。
const SNAP_OFF: SnapConfig = SnapConfig {
    mode: SnapMode::Straight { div: 16 },
    enabled: false,
    min_beat_unit: 1.0 / 128.0,
    time_sig: (4, 4),
};

fn modifiers(ctrl: bool, alt: bool) -> Modifiers {
    Modifiers { ctrl, alt, ..Modifiers::empty() }
}

fn press(x: f32, y: f32, ctrl: bool) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers: modifiers(ctrl, false),
        ..PointerFrame::default()
    }
}

fn hold(x: f32, y: f32, ctrl: bool) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_pressed: true,
        modifiers: modifiers(ctrl, false),
        ..PointerFrame::default()
    }
}

fn release(x: f32, y: f32, ctrl: bool) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_released: true,
        modifiers: modifiers(ctrl, false),
        ..PointerFrame::default()
    }
}

fn press_shift(x: f32, y: f32) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers: Modifiers { shift: true, ..Modifiers::empty() },
        ..PointerFrame::default()
    }
}

fn release_shift(x: f32, y: f32) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_released: true,
        modifiers: Modifiers { shift: true, ..Modifiers::empty() },
        ..PointerFrame::default()
    }
}

/// minimal Model: emit された section 系 request を捕まえる。
#[derive(Default)]
struct SecModel {
    sections: Vec<SectionView>,
    last: Option<ArrangementEditRequest>,
    /// M14 Phase 128 (#106): SelectSection を別 capture (短 click は Select + Playhead を併発するため
    /// `last` だけでは Select を取り逃す)。
    select: Option<(u32, SelectModifier)>,
}

fn sec_model(sections: Vec<SectionView>) -> SecModel {
    SecModel { sections, ..Default::default() }
}

fn track() -> ArrangementTrack {
    ArrangementTrack {
        id: 1,
        name: Arc::from("t1"),
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

fn view() -> ArrangementView {
    let mut v = ArrangementView::default();
    v.start_beat = 0.0;
    v.len_beats = f64::from(WIDGET_RECT.w / ZOOM_X_PX_PER_BEAT); // 12.5
    v.header_w = 0.0;
    v.ruler_h = 0.0;
    v.arranger_lane_h = ARRANGER_LANE_H;
    v.snap = SNAP_OFF;
    v
}

/// 1 frame 走らせ、 section 系 EditRequest を `m.last` に capture (最後の 1 件)。
fn sec_frame(host: &mut UiHost<SecModel>, m: &mut SecModel, input: FrameInput) {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let v = view();
    let style = ArrangementStyle::default();
    let tracks = vec![track()];
    host.frame(m, &mut scene, screen, input, |model, ui| {
        let sections = model.sections.clone();
        let _ = arrangement(ui, 
            "arr",
            WIDGET_RECT,
            &tracks,
            &sections,
            v,
            &[],
            &[],
            &[],
            &[],
            &style,
            None,
            |req| {
                // section 系のみ capture (他は no-op)。 値だけ取り出して move する。
                let captured: Option<ArrangementEditRequest> = match &req {
                    ArrangementEditRequest::CreateSection { start, len } => {
                        Some(ArrangementEditRequest::CreateSection { start: *start, len: *len })
                    }
                    ArrangementEditRequest::MoveSection { id, prev_start, next_start } => {
                        Some(ArrangementEditRequest::MoveSection {
                            id: *id,
                            prev_start: *prev_start,
                            next_start: *next_start,
                        })
                    }
                    ArrangementEditRequest::ResizeSection {
                        id,
                        prev_start,
                        prev_len,
                        next_start,
                        next_len,
                    } => Some(ArrangementEditRequest::ResizeSection {
                        id: *id,
                        prev_start: *prev_start,
                        prev_len: *prev_len,
                        next_start: *next_start,
                        next_len: *next_len,
                    }),
                    ArrangementEditRequest::DuplicateSection { id, dest_start } => {
                        Some(ArrangementEditRequest::DuplicateSection {
                            id: *id,
                            dest_start: *dest_start,
                        })
                    }
                    ArrangementEditRequest::SetPlayheadBeat(b) => {
                        Some(ArrangementEditRequest::SetPlayheadBeat(*b))
                    }
                    _ => None,
                };
                // SelectSection は last とは別 field に capture (短 click で Playhead と併発するため)。
                let select = if let ArrangementEditRequest::SelectSection { id, modifier } = &req {
                    Some((*id, *modifier))
                } else {
                    None
                };
                Edit::mutate(move |mm: &mut SecModel| {
                    if let Some(c) = captured {
                        mm.last = Some(c);
                    }
                    if let Some(s) = select {
                        mm.select = Some(s);
                    }
                })
            },
        );
    });
}

fn one_section() -> Vec<SectionView> {
    // start=2.0 → x=128、 len=4.0 → x 128..384 (center beat 4.0 = x 256、 right edge x=384)。
    vec![SectionView {
        id: 7,
        name: Arc::from("A"),
        color: [0.3, 0.4, 0.5],
        start_beat: 2.0,
        len_beats: 4.0,
        selected: false,
    }]
}

/// 帯中央 drag (Ctrl なし) → release で `MoveSection` (snap OFF なので raw delta)。
#[test]
fn section_center_drag_emits_move() {
    let mut host: UiHost<SecModel> = UiHost::no_redraw();
    let mut m = sec_model(one_section());
    sec_frame(&mut host, &mut m, frame(press(256.0, 10.0, false)));
    sec_frame(&mut host, &mut m, frame(hold(320.0, 10.0, false)));
    // +128px = +2.0 拍。
    sec_frame(&mut host, &mut m, frame(release(384.0, 10.0, false)));
    match m.last {
        Some(ArrangementEditRequest::MoveSection { id, next_start, .. }) => {
            assert_eq!(id, 7);
            assert!((next_start - 4.0).abs() < 1e-3, "2.0 + 2.0 = 4.0: got {next_start}");
        }
        other => panic!("expected MoveSection, got {other:?}"),
    }
}

/// 帯中央 Ctrl+drag → release で `DuplicateSection` (`dest_start` = snap 適用済移動先)。
#[test]
fn section_ctrl_drag_emits_duplicate() {
    let mut host: UiHost<SecModel> = UiHost::no_redraw();
    let mut m = sec_model(one_section());
    sec_frame(&mut host, &mut m, frame(press(256.0, 10.0, true)));
    sec_frame(&mut host, &mut m, frame(hold(320.0, 10.0, true)));
    sec_frame(&mut host, &mut m, frame(release(384.0, 10.0, true)));
    match m.last {
        Some(ArrangementEditRequest::DuplicateSection { id, dest_start }) => {
            assert_eq!(id, 7);
            assert!((dest_start - 4.0).abs() < 1e-3, "dest 2.0+2.0=4.0: got {dest_start}");
        }
        other => panic!("expected DuplicateSection, got {other:?}"),
    }
}

/// 右端 drag → release で `ResizeSection` (start 不変、 len 増加)。
#[test]
fn section_right_edge_drag_emits_resize() {
    let mut host: UiHost<SecModel> = UiHost::no_redraw();
    let mut m = sec_model(one_section());
    // 右端 x=384 で press → ResizeRight。
    sec_frame(&mut host, &mut m, frame(press(384.0, 10.0, false)));
    sec_frame(&mut host, &mut m, frame(hold(416.0, 10.0, false)));
    // +64px = +1.0 拍。
    sec_frame(&mut host, &mut m, frame(release(448.0, 10.0, false)));
    match m.last {
        Some(ArrangementEditRequest::ResizeSection { id, next_start, next_len, .. }) => {
            assert_eq!(id, 7);
            assert!((next_start - 2.0).abs() < 1e-3, "start 不変 2.0: got {next_start}");
            assert!((next_len - 5.0).abs() < 1e-3, "len 4.0+1.0=5.0: got {next_len}");
        }
        other => panic!("expected ResizeSection, got {other:?}"),
    }
}

/// 帯中央の短 click (移動 < 4px) → `SetPlayheadBeat(section.start)` に demote。
#[test]
fn section_short_click_emits_playhead_jump() {
    let mut host: UiHost<SecModel> = UiHost::no_redraw();
    let mut m = sec_model(one_section());
    sec_frame(&mut host, &mut m, frame(press(256.0, 10.0, false)));
    // ほぼ動かさず release (2px < 4px 閾値)。
    sec_frame(&mut host, &mut m, frame(release(258.0, 10.0, false)));
    match m.last {
        Some(ArrangementEditRequest::SetPlayheadBeat(b)) => {
            assert!((b - 2.0).abs() < 1e-3, "section.start 2.0 へジャンプ: got {b}");
        }
        other => panic!("expected SetPlayheadBeat, got {other:?}"),
    }
}

/// 空きレーンの範囲 drag → release で `CreateSection` (描いた範囲)。
#[test]
fn empty_range_drag_emits_create() {
    let mut host: UiHost<SecModel> = UiHost::no_redraw();
    let mut m = sec_model(Vec::new()); // section 無し = レーン全域が空き。
    // x=64 (beat 1.0) で press → Create session。
    sec_frame(&mut host, &mut m, frame(press(64.0, 10.0, false)));
    sec_frame(&mut host, &mut m, frame(hold(200.0, 10.0, false)));
    // x=320 (beat 5.0) で release → 範囲 1.0..5.0。
    sec_frame(&mut host, &mut m, frame(release(320.0, 10.0, false)));
    match m.last {
        Some(ArrangementEditRequest::CreateSection { start, len }) => {
            assert!((start - 1.0).abs() < 1e-3, "start 1.0: got {start}");
            assert!((len - 4.0).abs() < 1e-3, "len 4.0: got {len}");
        }
        other => panic!("expected CreateSection, got {other:?}"),
    }
}

// ===== M14 Phase 128 (daw_01 #106): section 選択 (短 click → SelectSection + SetPlayheadBeat 併発) =====

/// 修飾なし短 click → `SelectSection { Single }` と `SetPlayheadBeat(start)` の **2 件併発**。
#[test]
fn section_plain_short_click_selects_single_and_jumps() {
    let mut host: UiHost<SecModel> = UiHost::no_redraw();
    let mut m = sec_model(one_section());
    sec_frame(&mut host, &mut m, frame(press(256.0, 10.0, false)));
    sec_frame(&mut host, &mut m, frame(release(258.0, 10.0, false)));
    assert_eq!(m.select, Some((7, SelectModifier::Single)), "Single 選択");
    match m.last {
        Some(ArrangementEditRequest::SetPlayheadBeat(b)) => {
            assert!((b - 2.0).abs() < 1e-3, "ジャンプも併発 (start 2.0): got {b}");
        }
        other => panic!("SetPlayheadBeat も併発するはず, got {other:?}"),
    }
}

/// Shift+短 click → `SelectSection { RangeFromAnchor }`。
#[test]
fn section_shift_short_click_selects_range() {
    let mut host: UiHost<SecModel> = UiHost::no_redraw();
    let mut m = sec_model(one_section());
    sec_frame(&mut host, &mut m, frame(press_shift(256.0, 10.0)));
    sec_frame(&mut host, &mut m, frame(release_shift(258.0, 10.0)));
    assert_eq!(m.select, Some((7, SelectModifier::RangeFromAnchor)));
}

/// Ctrl+短 click (drag でない) → `SelectSection { Toggle }`。 Duplicate ではない (複製は Ctrl+drag のみ)。
#[test]
fn section_ctrl_short_click_selects_toggle_not_duplicate() {
    let mut host: UiHost<SecModel> = UiHost::no_redraw();
    let mut m = sec_model(one_section());
    sec_frame(&mut host, &mut m, frame(press(256.0, 10.0, true)));
    sec_frame(&mut host, &mut m, frame(release(258.0, 10.0, true)));
    assert_eq!(m.select, Some((7, SelectModifier::Toggle)), "Ctrl+click は Toggle 選択");
    assert!(
        !matches!(m.last, Some(ArrangementEditRequest::DuplicateSection { .. })),
        "Ctrl+短 click は Duplicate しない (複製は Ctrl+drag のみ): got {:?}",
        m.last
    );
}

/// 帯 drag (>= 4px) は `SelectSection` を emit しない (drag = Move/Duplicate、 選択は短 click のみ)。
#[test]
fn section_drag_does_not_select() {
    let mut host: UiHost<SecModel> = UiHost::no_redraw();
    let mut m = sec_model(one_section());
    sec_frame(&mut host, &mut m, frame(press(256.0, 10.0, false)));
    sec_frame(&mut host, &mut m, frame(hold(320.0, 10.0, false)));
    sec_frame(&mut host, &mut m, frame(release(384.0, 10.0, false)));
    assert_eq!(m.select, None, "drag では SelectSection を emit しない");
    assert!(
        matches!(m.last, Some(ArrangementEditRequest::MoveSection { .. })),
        "drag は MoveSection: got {:?}",
        m.last
    );
}

fn frame(pointer: PointerFrame) -> FrameInput {
    let mut input = FrameInput::default();
    input.pointer = pointer;
    input
}
