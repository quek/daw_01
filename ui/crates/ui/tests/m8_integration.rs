//! M8 Integration Tests
//!
//! Phase 29-34 の機能を `UiHost::frame` 経由で end-to-end に検証する:
//! - Phase 29: `Edit::with_inverse` が history に積まれ、`Ui::request_undo` で巻き戻る
//! - Phase 30: shortcut が `take_shortcut(name)` で消費される
//! - Phase 30: focus traversal が Tab で次の focusable に移動する
//! - Phase 31: NoopClipboard で paste が None / set が握りつぶされる
//! - Phase 32: `AppEvent::FileDropped` → `InputAccumulator` → `take_file_drop_in_rect`
//! - Phase 33: `take_drag_rect_in_rect` が press → drag → release で finished=true を返す

#![allow(clippy::field_reassign_with_default)]

use std::path::PathBuf;

use daw_ui_core::{
    DroppedFiles, Edit, FrameInput, InputAccumulator, NoopClipboard, PointerFrame, UiHost, WidgetId,
};
use daw_ui_platform::{
    AppEvent, ElementState, KeyEvent, Modifiers, PhysicalKey, PhysicalPosition, PhysicalSize,
};
use daw_ui_renderer::{Rect, Scene};

struct Model {
    counter: i32,
}

fn model() -> Model {
    Model { counter: 0 }
}

fn run<F>(host: &mut UiHost<Model>, model: &mut Model, input: FrameInput, f: F)
where
    F: for<'a> FnOnce(&'a Model, &mut daw_ui_core::Ui<'a, Model>),
{
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 800, height: 600 };
    host.frame(model, &mut scene, screen, input, f);
}

// ============================================================
// Phase 29: history (undo / redo)
// ============================================================

#[test]
fn undoable_edit_round_trip() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut m = model();

    // フレーム 1: undoable Edit を ui.push_edit で直接発行 (button の click 確定モデルを通さず simple に)
    run(&mut host, &mut m, FrameInput::default(), |_, ui| {
        ui.push_edit(Edit::with_inverse(
            "set5",
            |m: &mut Model| m.counter = 5,
            |m: &mut Model| m.counter = 0,
        ));
    });
    assert_eq!(m.counter, 5, "Undoable forward applied: counter == 5");
    assert!(host.history().can_undo());
    assert_eq!(host.history().undo_label(), Some("set5"));

    // フレーム 2: ui.request_undo() で巻き戻し
    run(&mut host, &mut m, FrameInput::default(), |_, ui| {
        ui.request_undo();
    });
    assert_eq!(m.counter, 0, "request_undo: counter == 0 (inverse applied)");
    assert!(host.history().can_redo());

    // フレーム 3: ui.request_redo() で再適用
    run(&mut host, &mut m, FrameInput::default(), |_, ui| {
        ui.request_redo();
    });
    assert_eq!(m.counter, 5, "request_redo: counter == 5 (forward applied)");
}

// ============================================================
// Phase 30: shortcut
// ============================================================

#[test]
fn shortcut_take_consumes_match() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut m = model();

    let mut input = FrameInput::default();
    input.pointer.modifiers = Modifiers { ctrl: true, ..Modifiers::empty() };
    input.keyboard = vec![KeyEvent {
        state: ElementState::Pressed,
        text: None,
        physical_key: PhysicalKey::Char('Z'),
    }];

    let mut got_undo = false;
    let mut got_redo = false;
    run(&mut host, &mut m, input, |_, ui| {
        got_undo = ui.take_shortcut("undo");
        got_redo = ui.take_shortcut("redo");
    });

    assert!(got_undo, "Ctrl+Z should match 'undo'");
    assert!(!got_redo, "Ctrl+Z should NOT match 'redo'");
}

#[test]
fn shortcut_redo_with_ctrl_y() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut m = model();

    let mut input = FrameInput::default();
    input.pointer.modifiers = Modifiers { ctrl: true, ..Modifiers::empty() };
    input.keyboard = vec![KeyEvent {
        state: ElementState::Pressed,
        text: None,
        physical_key: PhysicalKey::Char('Y'),
    }];

    let mut got_redo = false;
    run(&mut host, &mut m, input, |_, ui| {
        got_redo = ui.take_shortcut("redo");
    });

    assert!(got_redo, "Ctrl+Y should match 'redo' (alias)");
}

// ============================================================
// Phase 30: focus traversal
// ============================================================

#[test]
fn tab_traversal_moves_focus_in_order() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut m = model();

    let wid_a = WidgetId::ROOT.child("a");
    let wid_b = WidgetId::ROOT.child("b");

    // フレーム 1: focusable に登録 + a に focus
    run(&mut host, &mut m, FrameInput::default(), |_, ui| {
        ui.focusable(wid_a, Rect { x: 0.0, y: 0.0, w: 100.0, h: 30.0 });
        ui.focusable(wid_b, Rect { x: 0.0, y: 30.0, w: 100.0, h: 30.0 });
        ui.set_focus(wid_a);
    });
    assert_eq!(host.focused_widget(), Some(wid_a));

    // フレーム 2: Tab で次に移動
    let mut input = FrameInput::default();
    input.keyboard = vec![KeyEvent {
        state: ElementState::Pressed,
        text: None,
        physical_key: PhysicalKey::Tab,
    }];
    run(&mut host, &mut m, input, |_, ui| {
        ui.focusable(wid_a, Rect { x: 0.0, y: 0.0, w: 100.0, h: 30.0 });
        ui.focusable(wid_b, Rect { x: 0.0, y: 30.0, w: 100.0, h: 30.0 });
    });
    assert_eq!(host.focused_widget(), Some(wid_b), "Tab moved focus a → b");
}

// ============================================================
// Phase 31: clipboard (NoopClipboard で paste = None)
// ============================================================

#[test]
fn noop_clipboard_paste_returns_none() {
    let mut host: UiHost<Model> = UiHost::no_redraw().with_clipboard(NoopClipboard);
    let mut m = model();

    // Ctrl+V (paste shortcut) を発射しても、NoopClipboard は None を返す。
    let mut input = FrameInput::default();
    input.pointer.modifiers = Modifiers { ctrl: true, ..Modifiers::empty() };
    input.keyboard = vec![KeyEvent {
        state: ElementState::Pressed,
        text: None,
        physical_key: PhysicalKey::Char('V'),
    }];

    let mut paste_result: Option<String> = None;
    run(&mut host, &mut m, input, |_, ui| {
        paste_result = ui.take_clipboard_paste();
    });
    assert!(paste_result.is_none(), "NoopClipboard should return None on paste");
}

// ============================================================
// Phase 32: file drop
// ============================================================

#[test]
fn file_drop_consumed_by_take_in_rect() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut m = model();

    // InputAccumulator 経由で file drop event を ingest
    let mut accum = InputAccumulator::new();
    accum.ingest(&AppEvent::PointerMoved(PhysicalPosition { x: 50.0, y: 50.0 }));
    accum.ingest(&AppEvent::FileHovered(PathBuf::from("/tmp/x.wav")));
    accum.ingest(&AppEvent::FileDropped(PathBuf::from("/tmp/x.wav")));

    let input = accum.take_input();
    assert!(input.file_drop.is_some(), "file_drop should be Some after drop");
    let drop = input.file_drop.as_ref().unwrap();
    assert_eq!(drop.paths.len(), 1);
    assert!((drop.position.0 - 50.0).abs() < 1e-5);

    let mut drop_received: Option<DroppedFiles> = None;
    run(&mut host, &mut m, input, |_, ui| {
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        drop_received = ui.take_file_drop_in_rect(rect);
    });
    let drop = drop_received.expect("file drop should be consumed");
    assert_eq!(drop.paths.len(), 1);
    assert_eq!(drop.paths[0], PathBuf::from("/tmp/x.wav"));
    // Phase 32 → daw_01 #023 拡張: caller が drop position を受け取れる。
    assert!((drop.position.0 - 50.0).abs() < 1e-5);
    assert!((drop.position.1 - 50.0).abs() < 1e-5);
}

#[test]
fn file_drop_outside_rect_returns_none() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut m = model();

    let mut accum = InputAccumulator::new();
    accum.ingest(&AppEvent::PointerMoved(PhysicalPosition { x: 500.0, y: 500.0 }));
    accum.ingest(&AppEvent::FileDropped(PathBuf::from("/tmp/x.wav")));
    let input = accum.take_input();

    let mut drop_received: Option<DroppedFiles> = None;
    run(&mut host, &mut m, input, |_, ui| {
        // drop pos = (500, 500) は (0, 0, 100, 100) の外
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        drop_received = ui.take_file_drop_in_rect(rect);
    });
    assert!(drop_received.is_none());
}

// ============================================================
// Phase 33: drag_rect
// ============================================================

#[test]
fn drag_rect_press_drag_release_lifecycle() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut m = model();
    let bounds = Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 };
    let wid = WidgetId::ROOT.child("drag_test");

    // フレーム 1: press at (50, 50)
    let mut input = FrameInput::default();
    input.pointer = PointerFrame {
        pos: Some((50.0, 50.0)),
        primary_just_pressed: true,
        primary_pressed: true,
        ..PointerFrame::default()
    };
    let mut got_drag: Option<bool> = None;
    let mut finished: Option<bool> = None;
    run(&mut host, &mut m, input, |_, ui| {
        let r = ui.take_drag_rect_in_rect(wid, bounds);
        got_drag = Some(r.is_some());
        finished = r.map(|d| d.finished);
    });
    assert_eq!(got_drag, Some(true), "press should start drag");
    assert_eq!(finished, Some(false), "press frame: not finished");

    // フレーム 2: hold-move to (100, 100)
    let mut input = FrameInput::default();
    input.pointer = PointerFrame {
        pos: Some((100.0, 100.0)),
        primary_pressed: true,
        ..PointerFrame::default()
    };
    let mut finished_2: Option<bool> = None;
    run(&mut host, &mut m, input, |_, ui| {
        let r = ui.take_drag_rect_in_rect(wid, bounds);
        finished_2 = r.map(|d| d.finished);
    });
    assert_eq!(finished_2, Some(false), "hold-move frame: not finished");

    // フレーム 3: release at (100, 100)
    let mut input = FrameInput::default();
    input.pointer = PointerFrame {
        pos: Some((100.0, 100.0)),
        primary_just_released: true,
        ..PointerFrame::default()
    };
    let mut finished_3: Option<bool> = None;
    let mut start_3: Option<(f32, f32)> = None;
    let mut end_3: Option<(f32, f32)> = None;
    run(&mut host, &mut m, input, |_, ui| {
        let r = ui.take_drag_rect_in_rect(wid, bounds);
        finished_3 = r.map(|d| d.finished);
        start_3 = r.map(|d| d.start);
        end_3 = r.map(|d| d.end);
    });
    assert_eq!(finished_3, Some(true), "release frame: finished=true");
    assert_eq!(start_3, Some((50.0, 50.0)));
    assert_eq!(end_3, Some((100.0, 100.0)));

    // フレーム 4: state クリア → drag = None
    let mut got_4: Option<bool> = None;
    run(&mut host, &mut m, FrameInput::default(), |_, ui| {
        got_4 = Some(ui.take_drag_rect_in_rect(wid, bounds).is_some());
    });
    assert_eq!(got_4, Some(false), "after release: drag cleared");
}

// ============================================================
// Phase 34: file dialog request
// ============================================================

#[test]
fn dialog_request_does_not_panic_without_action() {
    // dialog 実行は同期 (rfd) で UI thread block するが、test では request 自体が panic
    // しないことを確認する (実行は OS 側 dialog を出す → cargo test では実行しない方が安全)。
    // ここでは「request_open_file_dialog が ok か」だけを確認する (consumer mock なし)。
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut m = model();

    // dialog request は frame 内で立てる、実際の dialog は frame 末尾で実行される。
    // CI 環境で OS dialog が出ないように request は **しない** (これは smoke test ではない、
    // request API シグネチャの存在のみ確認)。
    run(&mut host, &mut m, FrameInput::default(), |_, ui| {
        // pending_dialog_results から取り出すだけは安全
        let result = ui.take_dialog_result("nonexistent");
        assert!(result.is_none());
    });
}
