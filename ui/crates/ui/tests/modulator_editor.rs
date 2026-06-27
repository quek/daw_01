//! `mseg_editor` / `step_grid` のジェスチャを headless で検証する。
//! `UiHost::frame_to_edits` に `PointerFrame` を流し、発行された `Edit` を model に適用して
//! 観測する (automation_point_edit.rs と同 pattern)。

#![allow(clippy::field_reassign_with_default)]

use daw_ui_core::{Edit, FrameInput, MsegAction, MsegEditorStyle, MsegNode, PointerFrame, UiHost};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Rect, Scene};

const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 200.0, h: 100.0 };
const STEP_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 160.0, h: 100.0 };

#[derive(Default)]
struct Obs {
    moves: Vec<(usize, f32, f32)>,
    adds: Vec<(f32, f32)>,
    curves: Vec<(usize, f32)>,
    deletes: Vec<usize>,
    sets: Vec<(usize, f32)>,
}

fn nodes() -> Vec<MsegNode> {
    vec![
        MsegNode { time: 0.0, value: 0.2, curve: 0.0 },
        MsegNode { time: 0.5, value: 0.8, curve: 0.0 },
        MsegNode { time: 1.0, value: 0.2, curve: 0.0 },
    ]
}

/// linear なサンプル列 (tension handle の縦位置決定用)。
fn samples() -> Vec<(f32, f32)> {
    vec![(0.0, 0.2), (0.5, 0.8), (1.0, 0.2)]
}

fn press(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_just_pressed: true, primary_pressed: true, modifiers: m, ..PointerFrame::default() }
}
fn hold(x: f32, y: f32) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_pressed: true, ..PointerFrame::default() }
}
fn release(x: f32, y: f32) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_just_released: true, ..PointerFrame::default() }
}

fn run_mseg(host: &mut UiHost<Obs>, obs: &mut Obs, p: PointerFrame) {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 200, height: 100 };
    let node_list = nodes();
    let sample_list = samples();
    let edits = host.frame_to_edits(obs, &mut scene, screen, FrameInput { pointer: p, ..Default::default() }, |_obs, ui| {
        ui.mseg_editor("t", RECT, &node_list, &sample_list, None, MsegEditorStyle::default(), |act| {
            Edit::mutate(move |o: &mut Obs| match act {
                MsegAction::Move { index, time, value } => o.moves.push((index, time, value)),
                MsegAction::Add { time, value } => o.adds.push((time, value)),
                MsegAction::SetCurve { segment, curve } => o.curves.push((segment, curve)),
                MsegAction::Delete { index } => o.deletes.push(index),
            })
        });
    });
    for e in edits {
        e.apply(obs);
    }
}

#[test]
fn mseg_node_drag_emits_move_with_clamped_coords() {
    let mut host: UiHost<Obs> = UiHost::no_redraw();
    let mut obs = Obs::default();
    // node 1 は (0.5, 0.8) → px (100, 20)。 press → hold で (120, 60) へ。
    run_mseg(&mut host, &mut obs, press(100.0, 20.0, Modifiers::default()));
    run_mseg(&mut host, &mut obs, hold(120.0, 60.0));
    let last = obs.moves.last().copied().expect("move emitted");
    assert_eq!(last.0, 1, "node 1 を移動");
    assert!((last.1 - 0.6).abs() < 1e-3, "time = 120/200 = 0.6 (got {})", last.1);
    assert!((last.2 - 0.4).abs() < 1e-3, "value = 1 - 60/100 = 0.4 (got {})", last.2);
}

#[test]
fn mseg_endpoint_time_is_locked() {
    let mut host: UiHost<Obs> = UiHost::no_redraw();
    let mut obs = Obs::default();
    // node 0 は (0.0, 0.2) → px (0, 80)。 横に引きずっても time は 0 のまま。
    run_mseg(&mut host, &mut obs, press(0.0, 80.0, Modifiers::default()));
    run_mseg(&mut host, &mut obs, hold(60.0, 40.0));
    let last = obs.moves.last().copied().expect("move emitted");
    assert_eq!(last.0, 0);
    assert!((last.1 - 0.0).abs() < 1e-6, "端点の time は固定 (got {})", last.1);
    assert!((last.2 - 0.6).abs() < 1e-3, "value は自由 (got {})", last.2);
}

#[test]
fn mseg_double_click_empty_emits_add() {
    let mut host: UiHost<Obs> = UiHost::no_redraw();
    let mut obs = Obs::default();
    // 空白 (40, 40) で 2 連続 release → double-click。
    run_mseg(&mut host, &mut obs, release(40.0, 40.0));
    run_mseg(&mut host, &mut obs, release(40.0, 40.0));
    let add = obs.adds.last().copied().expect("add emitted on dbl-click");
    assert!((add.0 - 0.2).abs() < 1e-3, "time = 40/200 = 0.2 (got {})", add.0);
    assert!((add.1 - 0.6).abs() < 1e-3, "value = 1 - 40/100 = 0.6 (got {})", add.1);
}

#[test]
fn mseg_tension_drag_emits_set_curve() {
    let mut host: UiHost<Obs> = UiHost::no_redraw();
    let mut obs = Obs::default();
    // segment 0 の tension handle は中点 (0.25, lerp(0.2,0.8,0.5)=0.5) → px (50, 50)。
    run_mseg(&mut host, &mut obs, press(50.0, 50.0, Modifiers::default()));
    run_mseg(&mut host, &mut obs, hold(50.0, 30.0)); // 上に 20px = 凸方向。
    let last = obs.curves.last().copied().expect("set_curve emitted");
    assert_eq!(last.0, 0, "segment 0");
    // curve = anchor(0) - (30-50)*sens(3/100) = 0.6。
    assert!((last.1 - 0.6).abs() < 1e-3, "tension = 0.6 (got {})", last.1);
}

#[test]
fn mseg_tension_drag_falling_segment_is_not_inverted() {
    // 下降セグメント (seg 1 = node1(0.5,0.8)→node2(1,0.2)) では apply_tension の符号が
    // 逆なので、 widget は drag 符号を反転して「上げたら上に膨らむ」 を保つ。 上昇 seg0 で
    // +0.6 になる drag と同じ「上 20px」 が seg1 では -0.6 になる (= どちらも上に膨らむ)。
    let mut host: UiHost<Obs> = UiHost::no_redraw();
    let mut obs = Obs::default();
    // seg 1 の tension handle は中点 (0.75, lerp(0.8,0.2,0.5)=0.5) → px (150, 50)。
    run_mseg(&mut host, &mut obs, press(150.0, 50.0, Modifiers::default()));
    run_mseg(&mut host, &mut obs, hold(150.0, 30.0)); // 上に 20px。
    let last = obs.curves.last().copied().expect("set_curve emitted");
    assert_eq!(last.0, 1, "segment 1 (falling)");
    assert!(
        (last.1 - (-0.6)).abs() < 1e-3,
        "下降 seg は上げると curve 負 (= 上に膨らむ)。 got {}",
        last.1
    );
}

#[test]
fn mseg_alt_click_interior_node_emits_delete() {
    let mut host: UiHost<Obs> = UiHost::no_redraw();
    let mut obs = Obs::default();
    let mut alt = Modifiers::default();
    alt.alt = true;
    // node 1 (内側) を Alt+click → Delete。
    run_mseg(&mut host, &mut obs, press(100.0, 20.0, alt));
    assert_eq!(obs.deletes.last().copied(), Some(1), "内側ノードを削除");
    assert!(obs.moves.is_empty(), "Alt+click は drag を始めない");
}

#[test]
fn mseg_alt_click_endpoint_does_not_delete() {
    let mut host: UiHost<Obs> = UiHost::no_redraw();
    let mut obs = Obs::default();
    let mut alt = Modifiers::default();
    alt.alt = true;
    // node 0 (端点) を Alt+click → 削除されず、drag が始まる (Move 発火)。
    run_mseg(&mut host, &mut obs, press(0.0, 80.0, alt));
    assert!(obs.deletes.is_empty(), "端点は削除しない");
}

#[test]
fn step_grid_drag_emits_set() {
    let mut host: UiHost<Obs> = UiHost::no_redraw();
    let mut obs = Obs::default();
    let values = vec![0.5_f32; 4]; // cell = 40px。
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 160, height: 100 };
    // step 0 (x 0..40) の y=30 → value = 1 - 30/100 = 0.7。
    let edits = host.frame_to_edits(&obs, &mut scene, screen, FrameInput { pointer: press(20.0, 30.0, Modifiers::default()), ..Default::default() }, |_o, ui| {
        ui.step_grid("s", STEP_RECT, &values, Some(1), MsegEditorStyle::default(), |idx, v| {
            Edit::mutate(move |o: &mut Obs| o.sets.push((idx, v)))
        });
    });
    for e in edits {
        e.apply(&mut obs);
    }
    let last = obs.sets.last().copied().expect("set emitted");
    assert_eq!(last.0, 0, "step 0");
    assert!((last.1 - 0.7).abs() < 1e-3, "value = 0.7 (got {})", last.1);
}
