use super::*;
use crate::widgets::text_input::TextInputStyle;
use daw_ui_renderer::Color;
use std::sync::Arc;

/// `widget_state` で書き戻した値が次フレームでも同型として読み取れる
/// (`Box<dyn WidgetState>` 自体への blanket impl が `as_any_mut` を奪わないことの回帰防止)。
#[test]
fn widget_state_round_trip_no_downcast_panic() {
    #[derive(Debug, Default)]
    struct MyState {
        count: u32,
    }

    struct Model;

    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let model = Model;
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };

    // フレーム 1: state を初期化して 1 回インクリメント。
    host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput::default(),
        |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 0);
            state.count += 1;
        },
    );

    // フレーム 2: 同じ id で同じ型を取り直すと値が保持されている。
    host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput::default(),
        |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 1);
            state.count += 1;
        },
    );

    host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput::default(),
        |_, ui| {
            let id = WidgetId::ROOT.child("ws-roundtrip");
            let state: &mut MyState = ui.widget_state(id);
            assert_eq!(state.count, 2);
        },
    );
}

/// 「hover フレームを描画 → 次フレームで同フレーム内 press + release」で
/// click が 1 回目から発火することを担保する。ユーザ報告:
/// 「ボタンにマウスを乗せた直後の最初のクリックでアクションが反応しない」
/// の回帰防止。
#[test]
fn button_click_fires_on_first_hover_then_click() {
    struct Counter {
        count: u32,
    }

    let mut host: UiHost<Counter> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let mut model = Counter { count: 0 };
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 100.0,
        h: 32.0,
    };

    // Frame 1: cursor をボタン上にホバー (まだクリック無し)。
    let pointer_hover = PointerFrame {
        pos: Some((50.0, 16.0)),
        ..PointerFrame::default()
    };
    let edits = host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput {
            pointer: pointer_hover,
            ..Default::default()
        },
        |_, ui| {
            ui.button_at("test", "click me", rect, || {
                Edit::mutate(|m: &mut Counter| m.count += 1)
            });
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(model.count, 0, "hover フレームでは click は出ない");

    // Frame 2: 同フレーム内で press + release (高速クリック相当)。
    let pointer_click = PointerFrame {
        pos: Some((50.0, 16.0)),
        primary_just_pressed: true,
        primary_just_released: true,
        ..PointerFrame::default()
    };
    let edits = host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput {
            pointer: pointer_click,
            ..Default::default()
        },
        |_, ui| {
            ui.button_at("test", "click me", rect, || {
                Edit::mutate(|m: &mut Counter| m.count += 1)
            });
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(
        model.count, 1,
        "hover 直後の最初のクリックで click が発火するべき"
    );
}

/// press と release が別フレームに分かれて届くケース (winit が press と release で
/// それぞれ別の redraw を発火するパターン) で、`press_started_inside` がフレーム間で
/// ちゃんと保持されて release フレームで click 発火することを担保する。
#[test]
fn button_click_fires_when_press_and_release_in_separate_frames() {
    struct Counter {
        count: u32,
    }

    let mut host: UiHost<Counter> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let mut model = Counter { count: 0 };
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 100.0,
        h: 32.0,
    };
    let render = |host: &mut UiHost<Counter>,
                  scene: &mut Scene,
                  model: &Counter,
                  pointer: PointerFrame|
     -> Vec<Edit<Counter>> {
        host.frame_to_edits(
            model,
            scene,
            screen,
            FrameInput {
                pointer,
                ..Default::default()
            },
            |_, ui| {
                ui.button_at("test", "click me", rect, || {
                    Edit::mutate(|m: &mut Counter| m.count += 1)
                });
            },
        )
    };

    // Frame 1: hover.
    let edits = render(
        &mut host,
        &mut scene,
        &model,
        PointerFrame {
            pos: Some((50.0, 16.0)),
            ..PointerFrame::default()
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(model.count, 0);

    // Frame 2: press フレーム (まだ release していない、ボタン押下中)。
    let edits = render(
        &mut host,
        &mut scene,
        &model,
        PointerFrame {
            pos: Some((50.0, 16.0)),
            primary_just_pressed: true,
            primary_just_released: false,
            primary_pressed: true,
            ..PointerFrame::default()
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(model.count, 0, "press フレームでは click は出ない");

    // Frame 3: release フレーム。
    let edits = render(
        &mut host,
        &mut scene,
        &model,
        PointerFrame {
            pos: Some((50.0, 16.0)),
            primary_just_pressed: false,
            primary_just_released: true,
            ..PointerFrame::default()
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(
        model.count, 1,
        "release フレームで click 発火するべき (press_started_inside が保持されている)"
    );
}

/// `Ui::set_focus` の効果が同フレームで `is_focused` に反映されること、
/// および次フレームでも維持されることを確認する。
#[test]
fn focus_set_persists_to_next_frame() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let id = WidgetId::ROOT.child("focus-target");

    // Frame 1: set_focus を呼ぶと **同フレーム内で** is_focused = true になる。
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        assert!(!ui.is_focused(id));
        ui.set_focus(id);
        assert!(
            ui.is_focused(id),
            "set_focus 後は同フレームで is_focused = true"
        );
    });
    assert_eq!(host.focused_widget(), Some(id));

    // Frame 2: 何もしないが focus は維持。
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        assert!(ui.is_focused(id));
    });
    assert_eq!(host.focused_widget(), Some(id));
}

/// フォーカスを取った widget の上でクリックされても、その widget が `set_focus` を
/// 呼び続ける限りフォーカスは保たれる。クリック先が誰も `set_focus` を呼ばない
/// (= フォーカス可能でない場所) ならフォーカスはクリアされる。
#[test]
fn click_outside_clears_focus_when_no_widget_claims() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let id = WidgetId::ROOT.child("focus-target");

    // Frame 1: フォーカスを取る。
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.set_focus(id);
    });
    assert_eq!(host.focused_widget(), Some(id));

    // Frame 2: クリック発生 (just_released=true) で誰も set_focus を呼ばない → blur。
    let click = PointerFrame {
        pos: Some((50.0, 50.0)),
        primary_just_released: true,
        ..PointerFrame::default()
    };
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            pointer: click,
            ..Default::default()
        },
        |(), _ui| {
            // 誰も set_focus / clear_focus を呼ばない。
        },
    );
    assert_eq!(
        host.focused_widget(),
        None,
        "誰もフォーカスを取り直さなかったので blur される"
    );
}

/// 同フレームで set_focus を呼んでいればクリックがあってもフォーカスは保たれる。
#[test]
fn focus_kept_when_widget_re_claims_on_click() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let id = WidgetId::ROOT.child("focus-target");

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.set_focus(id);
    });
    assert_eq!(host.focused_widget(), Some(id));

    let click = PointerFrame {
        pos: Some((50.0, 50.0)),
        primary_just_released: true,
        ..PointerFrame::default()
    };
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            pointer: click,
            ..Default::default()
        },
        |(), ui| {
            // クリックフレームで widget が再度 set_focus を呼ぶ (text_input が再クリックされたケース)。
            ui.set_focus(id);
        },
    );
    assert_eq!(
        host.focused_widget(),
        Some(id),
        "再 set_focus でフォーカス維持"
    );
}

/// text_input をクリックでフォーカスを取り、キー入力で text を編集できることを担保する。
/// click → focus → 'A' 入力 → モデルが "A" になる、という流れを通しで検証。
#[test]
fn text_input_click_focus_then_typing_modifies_text() {
    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

    struct Doc {
        text: String,
    }

    let mut host: UiHost<Doc> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let mut model = Doc {
        text: String::new(),
    };
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 200.0,
        h: 28.0,
    };

    // Frame 1: click で focus を取る (まだ text は空)。
    let click = PointerFrame {
        pos: Some((50.0, 14.0)),
        primary_just_pressed: true,
        primary_just_released: true,
        ..PointerFrame::default()
    };
    let edits = host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput {
            pointer: click,
            ..Default::default()
        },
        |_, ui| {
            ui.text_input_at("ti", rect, "", &TextInputStyle::default(), |new| {
                Edit::mutate(|m: &mut Doc| m.text = new)
            });
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(model.text, "");

    // Frame 2: 'A' のキー入力を流す (focus されているので消費される)。
    let keys = vec![KeyEvent {
        state: ElementState::Pressed,
        text: Some("A".to_string()),
        physical_key: PhysicalKey::Other(0x41), repeat: false
    }];
    let edits = host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput {
            keyboard: keys,
            ..Default::default()
        },
        |m, ui| {
            ui.text_input_at("ti", rect, &m.text, &TextInputStyle::default(), |new| {
                Edit::mutate(|m: &mut Doc| m.text = new)
            });
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(model.text, "A");

    // Frame 3: Backspace で 1 文字消える。
    let keys = vec![KeyEvent {
        state: ElementState::Pressed,
        text: None,
        physical_key: PhysicalKey::Backspace, repeat: false
    }];
    let edits = host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput {
            keyboard: keys,
            ..Default::default()
        },
        |m, ui| {
            ui.text_input_at("ti", rect, &m.text, &TextInputStyle::default(), |new| {
                Edit::mutate(|m: &mut Doc| m.text = new)
            });
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(model.text, "");
}

/// IME preedit イベントは focused text_input に届き、state.preedit に反映される
/// (model の text には反映されない)。Commit イベントは cursor 位置に挿入し
/// preedit をクリアして Edit を発行する。
#[test]
fn text_input_ime_preedit_then_commit() {
    use crate::input::ImeEvent;
    use daw_ui_platform::PhysicalSize as PS;

    struct Doc {
        text: String,
    }

    let mut host: UiHost<Doc> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let mut model = Doc {
        text: String::new(),
    };
    let screen = PS {
        width: 200,
        height: 100,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 200.0,
        h: 28.0,
    };

    // Frame 1: click で focus 取得。
    let click = PointerFrame {
        pos: Some((50.0, 14.0)),
        primary_just_pressed: true,
        primary_just_released: true,
        ..PointerFrame::default()
    };
    let edits = host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput {
            pointer: click,
            ..Default::default()
        },
        |_, ui| {
            ui.text_input_at("ti", rect, "", &TextInputStyle::default(), |new| {
                Edit::mutate(|m: &mut Doc| m.text = new)
            });
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(model.text, "");

    // Frame 2: preedit 「あ」が来る。model は変わらず、内部 state にだけ反映。
    let edits = host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput {
            ime: vec![ImeEvent::Preedit {
                text: "あ".to_string(),
                cursor: None,
            }],
            ..Default::default()
        },
        |m, ui| {
            ui.text_input_at("ti", rect, &m.text, &TextInputStyle::default(), |new| {
                Edit::mutate(|m: &mut Doc| m.text = new)
            });
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(model.text, "", "preedit 中は model に反映しない");

    // Frame 3: commit 「あ」が来る。model に確定挿入され、preedit はクリア。
    let edits = host.frame_to_edits(
        &model,
        &mut scene,
        screen,
        FrameInput {
            ime: vec![ImeEvent::Commit("あ".to_string())],
            ..Default::default()
        },
        |m, ui| {
            ui.text_input_at("ti", rect, &m.text, &TextInputStyle::default(), |new| {
                Edit::mutate(|m: &mut Doc| m.text = new)
            });
        },
    );
    for e in edits {
        e.apply(&mut model);
    }
    assert_eq!(model.text, "あ", "commit で model.text に挿入される");
}

/// IME イベントは focused widget にだけ届き、focused でない widget は空を受け取る。
#[test]
fn ime_events_delivered_only_to_focused() {
    use crate::input::ImeEvent;

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let id_a = WidgetId::ROOT.child("a");
    let id_b = WidgetId::ROOT.child("b");

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.set_focus(id_a);
    });

    let ime = vec![ImeEvent::Commit("z".to_string())];
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            ime,
            ..Default::default()
        },
        |(), ui| {
            let b_ime = ui.take_ime_events_if_focused(id_b);
            assert_eq!(b_ime.len(), 0);
            let a_ime = ui.take_ime_events_if_focused(id_a);
            assert_eq!(a_ime.len(), 1);
            let a_ime2 = ui.take_ime_events_if_focused(id_a);
            assert_eq!(a_ime2.len(), 0, "drain 後は空");
        },
    );
}

/// キー入力イベントは focused widget だけに届き、他の widget には空が返る。
#[test]
fn keyboard_events_delivered_only_to_focused() {
    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let id_a = WidgetId::ROOT.child("a");
    let id_b = WidgetId::ROOT.child("b");

    // a にフォーカスを置く。
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.set_focus(id_a);
    });

    // 次フレーム: キー入力を流す。a は受け取れる、b は受け取れない。
    let keys = vec![KeyEvent {
        state: ElementState::Pressed,
        text: Some("x".to_string()),
        physical_key: PhysicalKey::Other(0), repeat: false
    }];
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            keyboard: keys,
            ..Default::default()
        },
        |(), ui| {
            // b で先に呼んでも空 (フォーカスが a)。
            let b_keys = ui.take_keyboard_events_if_focused(id_b);
            assert_eq!(b_keys.len(), 0);
            // a が呼ぶと届く。
            let a_keys = ui.take_keyboard_events_if_focused(id_a);
            assert_eq!(a_keys.len(), 1);
            assert_eq!(a_keys[0].text.as_deref(), Some("x"));
            // 二度目に a が呼んでも空 (内部 buffer が drain 済み)。
            let a_keys2 = ui.take_keyboard_events_if_focused(id_a);
            assert_eq!(a_keys2.len(), 0);
        },
    );
}

/// M14 Phase 57: text_input が focus を持っている (= 前フレームに `set_typing_focus(true)`
/// が立った) フレームでは、shortcut layer が `delete` / `select_all` / `cut` / `copy`
/// / `paste` を `pending_shortcuts` に積まず `keyboard_events` に残す。これにより
/// piano_roll / arrangement の `take_shortcut("delete")` が誤発火しない。
#[test]
fn typing_focus_blocks_global_delete_shortcut() {
    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 800,
        height: 600,
    };

    // Frame 1: text_input_at_focused で focus 取得 + 描画中に set_typing_focus(true)
    // → frame 末尾で UiHost.last_typing_focus = true。
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.text_input_at_focused(
            "ti",
            Rect {
                x: 10.0,
                y: 10.0,
                w: 100.0,
                h: 24.0,
            },
            "x",
            &TextInputStyle::default(),
            |_| Edit::mutate(|()| {}),
        );
    });

    // Frame 2: Delete を送る。typing_lock が立っているので shortcut layer は delete を
    // pending_shortcuts に積まず、keyboard_events に残す。take_shortcut("delete") は false。
    let delete_ev = KeyEvent {
        state: ElementState::Pressed,
        text: None,
        physical_key: PhysicalKey::Delete, repeat: false
    };
    let outer_got_delete = std::cell::Cell::new(true);
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            keyboard: vec![delete_ev],
            ..Default::default()
        },
        |(), ui| {
            // 他の widget (piano_roll 役) が先に take_shortcut("delete") を呼んでも false。
            outer_got_delete.set(ui.take_shortcut("delete"));
            ui.text_input_at_focused(
                "ti",
                Rect {
                    x: 10.0,
                    y: 10.0,
                    w: 100.0,
                    h: 24.0,
                },
                "x",
                &TextInputStyle::default(),
                |_| Edit::mutate(|()| {}),
            );
        },
    );
    assert!(
        !outer_got_delete.get(),
        "typing_focus 中は take_shortcut(\"delete\") が false を返す (= 他 widget の note 削除等を防ぐ)"
    );
}

/// OS auto-repeat (押しっぱなし) は **global shortcut にしない**。
///
/// shortcut は Delete / D / E のような離散コマンドに bind されるので、 repeat で
/// 連射されると「Delete 長押しでトラックが次々消える」 類の破壊的挙動になる
/// (daw_01 r.md #43)。 ただし event 自体は `keyboard_events` に残さないと
/// text_input の Backspace / 矢印の長押しリピートが死ぬので、 **抑止は
/// shortcut 解決層だけ** に閉じることを固定する。
#[test]
fn auto_repeat_does_not_fire_shortcuts_but_still_reaches_widgets() {
    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};
    let mut host: UiHost<()> = UiHost::no_redraw();
    host.shortcut_map_mut().bind("delete", "Delete");
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 200, height: 100 };

    let key = |repeat: bool| KeyEvent {
        state: ElementState::Pressed,
        text: None,
        physical_key: PhysicalKey::Delete,
        repeat,
    };

    // 立ち上がり (repeat=false) は shortcut として発火する。
    let fired = std::cell::Cell::new(false);
    let leftover = std::cell::Cell::new(0_usize);
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput { keyboard: vec![key(false)], ..Default::default() },
        |(), ui| {
            fired.set(ui.take_shortcut("delete"));
            let id = WidgetId::ROOT.child("sink");
            ui.set_focus(id);
            leftover.set(ui.take_keyboard_events_if_focused(id).len());
        },
    );
    assert!(fired.get(), "立ち上がりの Delete は shortcut として発火する");
    assert_eq!(leftover.get(), 0, "発火したイベントは consume される");

    // auto-repeat は shortcut にならず、 keyboard_events に残って widget へ届く。
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput { keyboard: vec![key(true)], ..Default::default() },
        |(), ui| {
            fired.set(ui.take_shortcut("delete"));
            let id = WidgetId::ROOT.child("sink");
            ui.set_focus(id);
            leftover.set(ui.take_keyboard_events_if_focused(id).len());
        },
    );
    assert!(!fired.get(), "auto-repeat は shortcut を発火させない");
    assert_eq!(
        leftover.get(),
        1,
        "repeat イベントは keyboard_events に残る (text_input の長押しリピート用)"
    );
}

/// (daw_01 #056) text_input focus 中、素の文字キー (Ctrl/Alt/Logo 無し) に bind された
/// shortcut は global 消費されず文字が text_input に届く。daw_01 が R/D/V/... を素キーに
/// bind しても typing 中に文字入力が奪われないことを固定。
#[test]
fn typing_focus_keeps_bare_char_shortcut_for_text_input() {
    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

    let mut host: UiHost<()> = UiHost::no_redraw();
    host.shortcut_map_mut().bind("test.bare_r", "R"); // daw_01 流の素キー shortcut
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 800,
        height: 600,
    };

    // Frame 1: text_input focus → frame 末尾で last_typing_focus = true
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.text_input_at_focused(
            "ti",
            Rect {
                x: 10.0,
                y: 10.0,
                w: 100.0,
                h: 24.0,
            },
            "x",
            &TextInputStyle::default(),
            |_| Edit::mutate(|()| {}),
        );
    });

    // Frame 2: 素の R キー → bare_char_key で suppress、shortcut は発火しない
    let r_ev = KeyEvent {
        state: ElementState::Pressed,
        text: Some("r".to_string()),
        physical_key: PhysicalKey::Char('R'), repeat: false
    };
    let got_shortcut = std::cell::Cell::new(true);
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            keyboard: vec![r_ev],
            ..Default::default()
        },
        |(), ui| {
            got_shortcut.set(ui.take_shortcut("test.bare_r"));
            ui.text_input_at_focused(
                "ti",
                Rect {
                    x: 10.0,
                    y: 10.0,
                    w: 100.0,
                    h: 24.0,
                },
                "x",
                &TextInputStyle::default(),
                |_| Edit::mutate(|()| {}),
            );
        },
    );
    assert!(
        !got_shortcut.get(),
        "typing 中は素の文字キー shortcut が発火しない (文字が text_input に届く)"
    );
}

/// (daw_01 r.md #67) OS の auto-repeat は既定では shortcut にしない (r.md #43 の
/// 「Delete 長押しでトラックが次々消える」 保護)。 `set_repeatable` を宣言した name だけが
/// repeat でも発火し、 1 フレームに届いた回数は `take_shortcut_count` でまとめて取れる。
#[test]
fn repeat_fires_only_for_declared_repeatable_shortcuts() {
    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

    let mut host: UiHost<()> = UiHost::no_redraw();
    host.shortcut_map_mut().bind("test.nudge", "Right");
    host.shortcut_map_mut().set_repeatable("test.nudge");
    host.shortcut_map_mut().bind("test.discrete", "Left");
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 800, height: 600 };

    let repeat_key = |pk| KeyEvent {
        state: ElementState::Pressed,
        text: None,
        physical_key: pk,
        repeat: true,
    };
    let nudge_count = std::cell::Cell::new(0_usize);
    let discrete = std::cell::Cell::new(true);
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            keyboard: vec![
                repeat_key(PhysicalKey::ArrowRight),
                repeat_key(PhysicalKey::ArrowRight),
                repeat_key(PhysicalKey::ArrowRight),
                repeat_key(PhysicalKey::ArrowLeft),
            ],
            ..Default::default()
        },
        |(), ui| {
            nudge_count.set(ui.take_shortcut_count("test.nudge"));
            discrete.set(ui.take_shortcut("test.discrete"));
        },
    );
    assert_eq!(nudge_count.get(), 3, "repeatable は届いた回数ぶん取れる");
    assert!(!discrete.get(), "非 repeatable な shortcut は auto-repeat で発火しない");
}

/// (daw_01 r.md #67) 矢印のような **非 char キー** を shortcut に bind すると、
/// 宣言しない限り typing 中も global 消費されて text_input のカーソル移動が死ぬ
/// (`bare_char_key` の逃がしに該当しないため)。 `set_typing_only` の宣言でそれを防ぐ。
#[test]
fn typing_focus_blocks_declared_typing_only_arrow() {
    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

    let mut host: UiHost<()> = UiHost::no_redraw();
    host.shortcut_map_mut().bind("test.arrow_typing_only", "Right");
    host.shortcut_map_mut().set_typing_only("test.arrow_typing_only");
    host.shortcut_map_mut().bind("test.arrow_global", "Left");
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 800, height: 600 };
    let ti_rect = Rect { x: 10.0, y: 10.0, w: 100.0, h: 24.0 };

    // Frame 1: text_input に focus (frame 末尾で last_typing_focus = true)。
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.text_input_at_focused("ti", ti_rect, "x", &TextInputStyle::default(), |_| {
            Edit::mutate(|()| {})
        });
    });

    let key = |pk| KeyEvent {
        state: ElementState::Pressed,
        text: None,
        physical_key: pk,
        repeat: false,
    };
    let typing_only_fired = std::cell::Cell::new(true);
    let global_fired = std::cell::Cell::new(false);
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            keyboard: vec![key(PhysicalKey::ArrowRight), key(PhysicalKey::ArrowLeft)],
            ..Default::default()
        },
        |(), ui| {
            typing_only_fired.set(ui.take_shortcut("test.arrow_typing_only"));
            global_fired.set(ui.take_shortcut("test.arrow_global"));
            ui.text_input_at_focused("ti", ti_rect, "x", &TextInputStyle::default(), |_| {
            Edit::mutate(|()| {})
        });
        },
    );
    assert!(
        !typing_only_fired.get(),
        "typing_only 宣言した矢印は typing 中 global 発火しない (text_input のカーソル移動になる)"
    );
    assert!(
        global_fired.get(),
        "宣言していない矢印は typing 中でも global 発火してしまう (= 宣言が必要な理由の対)"
    );
}

/// (daw_01 #056) typing focus が無ければ素の文字キー shortcut は従来どおり global 発火する
/// (suppress は typing_lock 中のみ、非テキスト文脈の素キー shortcut を壊さない)。
#[test]
fn non_typing_bare_char_shortcut_still_fires() {
    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey};

    let mut host: UiHost<()> = UiHost::no_redraw();
    host.shortcut_map_mut().bind("test.bare_r", "R");
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 800,
        height: 600,
    };

    // text_input を出さない (typing_lock = false) フレームで素 R → 発火する
    let r_ev = KeyEvent {
        state: ElementState::Pressed,
        text: Some("r".to_string()),
        physical_key: PhysicalKey::Char('R'), repeat: false
    };
    let got_shortcut = std::cell::Cell::new(false);
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            keyboard: vec![r_ev],
            ..Default::default()
        },
        |(), ui| {
            got_shortcut.set(ui.take_shortcut("test.bare_r"));
        },
    );
    assert!(
        got_shortcut.get(),
        "typing focus が無ければ素キー shortcut は通常どおり global 発火"
    );
}

/// M4 Phase 11: 同じ wid + input_hash で 2 回呼ぶと、2 回目は draw_fn が実行されない
/// (キャッシュ命中で前フレームの commands が scene に append される)。
#[test]
fn with_widget_node_hit_skips_draw_fn() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let id = WidgetId::ROOT.child("cache-test");
    let test_rect = Rect {
        x: 10.0,
        y: 20.0,
        w: 30.0,
        h: 40.0,
    };

    // Frame 1: cache miss → draw_fn 実行、scene に rect が積まれる。
    let calls_1 = std::cell::Cell::new(0_u32);
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.with_widget_node(id, 0xCAFE, |ui| {
            calls_1.set(calls_1.get() + 1);
            ui.push_rect(RectCommand {
                rect: test_rect,
                fill: Color::rgb(1.0, 0.0, 0.0),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });
    assert_eq!(calls_1.get(), 1, "1 回目は draw_fn が実行される");
    assert_eq!(scene.rect_count(), 1);

    // Frame 2: 同じ wid + 同じ hash → cache hit、draw_fn は実行されない。
    scene.clear();
    let calls_2 = std::cell::Cell::new(0_u32);
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.with_widget_node(id, 0xCAFE, |ui| {
            calls_2.set(calls_2.get() + 1);
            ui.push_rect(RectCommand {
                rect: test_rect,
                fill: Color::rgb(1.0, 0.0, 0.0),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });
    assert_eq!(
        calls_2.get(),
        0,
        "2 回目は cache hit で draw_fn が実行されない"
    );
    // scene には cache 経由で同じ rect が積まれている。
    assert_eq!(scene.rect_count(), 1);
    assert_eq!(scene.iter_rects().next().unwrap().rect, test_rect);
}

/// M4 Phase 11: hash が変わると cache miss、draw_fn が再実行される。
#[test]
fn with_widget_node_miss_runs_draw_fn() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let id = WidgetId::ROOT.child("miss-test");

    let calls_1 = std::cell::Cell::new(0_u32);
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.with_widget_node(id, 0xAAAA, |ui| {
            calls_1.set(calls_1.get() + 1);
            ui.push_rect(RectCommand {
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 10.0,
                    h: 10.0,
                },
                fill: Color::rgb(1.0, 0.0, 0.0),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });

    // Frame 2: 異なる hash → cache miss、draw_fn が再実行される。
    scene.clear();
    let calls_2 = std::cell::Cell::new(0_u32);
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.with_widget_node(id, 0xBBBB, |ui| {
            calls_2.set(calls_2.get() + 1);
            ui.push_rect(RectCommand {
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 10.0,
                    h: 10.0,
                },
                fill: Color::rgb(0.0, 1.0, 0.0),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });
    assert_eq!(calls_1.get(), 1);
    assert_eq!(calls_2.get(), 1, "hash 変化で draw_fn が再実行される");
}

/// M4 Phase 11: 前フレームに登場した widget が次フレームで呼ばれなければ
/// scenegraph から eviction される。
#[test]
fn scenegraph_evicts_unseen_widgets() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };
    let id_a = WidgetId::ROOT.child("evict-a");
    let id_b = WidgetId::ROOT.child("evict-b");

    // Frame 1: a と b 両方を wrap → scenegraph に 2 entry。
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.with_widget_node(id_a, 1, |_| {});
        ui.with_widget_node(id_b, 2, |_| {});
    });

    // Frame 2: a だけ wrap → b は seen に入らないので eviction、a は残る。
    // 同 hash で再呼び出し → cache hit、draw_fn 実行されない。
    let a_calls = std::cell::Cell::new(0_u32);
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.with_widget_node(id_a, 1, |_| {
            a_calls.set(a_calls.get() + 1);
        });
    });
    assert_eq!(a_calls.get(), 0, "a は cache hit で draw_fn 不実行");

    // Frame 3: b を再 wrap → 一度 eviction されているので cache miss、draw_fn が走る。
    let b_calls = std::cell::Cell::new(0_u32);
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.with_widget_node(id_b, 2, |_| {
            b_calls.set(b_calls.get() + 1);
        });
    });
    assert_eq!(
        b_calls.get(),
        1,
        "b は eviction されたので cache miss、draw_fn が再実行"
    );
}

// ============================================================
// M9 Phase 41b: Ui::set_cursor + transient flush
// ============================================================

#[test]
fn ui_set_cursor_calls_callback_on_frame_end() {
    use std::sync::Mutex;
    let captured: Arc<Mutex<Option<CursorIcon>>> = Arc::new(Mutex::new(None));
    let captured_clone = Arc::clone(&captured);

    let mut host: UiHost<()> = UiHost::no_redraw();
    host.set_cursor_request = Some(Box::new(move |c| {
        *captured_clone.lock().unwrap() = Some(c);
    }));
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };

    host.frame(
        &mut (),
        &mut scene,
        screen,
        FrameInput::default(),
        |(), ui| {
            ui.set_cursor(CursorIcon::EwResize);
        },
    );

    assert_eq!(*captured.lock().unwrap(), Some(CursorIcon::EwResize));
}

#[test]
fn ui_set_cursor_no_op_when_callback_unset() {
    // no_redraw / new で構築した UiHost は set_cursor_request = None。
    // Ui::set_cursor を呼んでも panic せず、何も起きないことを確認。
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };

    host.frame(
        &mut (),
        &mut scene,
        screen,
        FrameInput::default(),
        |(), ui| {
            ui.set_cursor(CursorIcon::Move);
        },
    );
    // no panic
}

#[test]
fn ui_set_cursor_last_call_wins_within_frame() {
    use std::sync::Mutex;
    let captured: Arc<Mutex<Option<CursorIcon>>> = Arc::new(Mutex::new(None));
    let captured_clone = Arc::clone(&captured);

    let mut host: UiHost<()> = UiHost::no_redraw();
    host.set_cursor_request = Some(Box::new(move |c| {
        *captured_clone.lock().unwrap() = Some(c);
    }));
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };

    host.frame(
        &mut (),
        &mut scene,
        screen,
        FrameInput::default(),
        |(), ui| {
            ui.set_cursor(CursorIcon::EwResize);
            ui.set_cursor(CursorIcon::Move); // 後勝ち
            ui.set_cursor(CursorIcon::Pointer); // 後勝ち
        },
    );

    assert_eq!(*captured.lock().unwrap(), Some(CursorIcon::Pointer));
}

#[test]
fn ui_set_cursor_resets_between_frames() {
    use std::sync::Mutex;
    let captured: Arc<Mutex<Vec<CursorIcon>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);

    let mut host: UiHost<()> = UiHost::no_redraw();
    host.set_cursor_request = Some(Box::new(move |c| {
        captured_clone.lock().unwrap().push(c);
    }));
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };

    // Frame 1: set_cursor 呼ぶ
    host.frame(
        &mut (),
        &mut scene,
        screen,
        FrameInput::default(),
        |(), ui| {
            ui.set_cursor(CursorIcon::EwResize);
        },
    );
    // Frame 2: 呼ばない → **Default に戻す** (OS 側は state-full なので、送らないと
    // 前フレームの形が貼り付く。daw_01 r.md #50 でこの per-frame 仕様に変更)。
    host.frame(
        &mut (),
        &mut scene,
        screen,
        FrameInput::default(),
        |(), _ui| {},
    );
    // Frame 3: 同じく呼ばない → 既に Default なので**送り直さない** (dedup)。
    host.frame(
        &mut (),
        &mut scene,
        screen,
        FrameInput::default(),
        |(), _ui| {},
    );

    assert_eq!(
        *captured.lock().unwrap(),
        vec![CursorIcon::EwResize, CursorIcon::Default]
    );
}

// -------- M9 Phase 43: FrameStats / debug_overlay --------

#[test]
fn frame_stats_default_is_zero_before_first_frame() {
    let host: UiHost<()> = UiHost::no_redraw();
    let s = host.last_frame_stats();
    assert_eq!(s.cache_hits, 0);
    assert_eq!(s.cache_misses, 0);
    assert_eq!(s.widget_count, 0);
    assert_eq!(s.scenegraph_size, 0);
}

#[test]
fn frame_stats_tracks_widget_count_and_cache_miss_then_hit() {
    // 1 frame 目: button + label = 2 widget が miss、2 frame 目: 同じ input なら 2 hit
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 200,
        height: 100,
    };

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.label("title", "hello");
        ui.button("b", "click", || Edit::mutate(|()| {}));
    });
    let s1 = host.last_frame_stats();
    assert!(s1.widget_count >= 2, "label + button = 2 widget 以上");
    assert!(s1.cache_misses >= 2, "1 frame 目は全て miss");
    assert_eq!(s1.cache_hits, 0);

    scene.clear();
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.label("title", "hello");
        ui.button("b", "click", || Edit::mutate(|()| {}));
    });
    let s2 = host.last_frame_stats();
    assert!(s2.cache_hits >= 2, "2 frame 目は同じ input なので hit");
}

#[test]
fn frame_stats_cache_hit_rate_returns_zero_when_no_widgets() {
    let stats = FrameStats::default();
    assert!((stats.cache_hit_rate() - 0.0).abs() < 1e-6);
}

#[test]
fn frame_stats_cache_hit_rate_computes_ratio() {
    let stats = FrameStats {
        cache_hits: 3,
        cache_misses: 1,
        ..FrameStats::default()
    };
    assert!((stats.cache_hit_rate() - 0.75).abs() < 1e-6);
}

#[test]
fn debug_overlay_renders_rects_and_glyphs() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };

    // 1 frame 目: stats を生成 (label 1 個)
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.label("title", "hi");
    });
    scene.clear();
    // 2 frame 目: debug_overlay を呼ぶ
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.debug_overlay(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 400.0,
                h: 300.0,
            },
            5.5,
        );
    });
    // M9 Phase 44a: popup buffer (z-order 最前面) に rect + glyph が積まれる。
    // popup_glyph が独立 GlyphPipeline になったので base pass の glyph と干渉しない。
    // M9 Phase 45f: rect/glyph/line を統合した popup_primitives で count を見る。
    assert!(
        scene.popup_rect_count() >= 1,
        "debug_overlay は popup buffer の rect を 1 個以上積む"
    );
    // S4a: history 行を撤去したので frame_ms 含めて frame/cache/wgts/sg の 4 行。
    assert!(
        scene.popup_glyph_count() >= 4,
        "debug_overlay は popup buffer の glyph を 4 行以上積む (frame_ms 含む)"
    );
}

#[test]
fn debug_overlay_omits_frame_ms_when_zero() {
    // frame_ms = 0.0 を渡したら frame 行は省略 (= cache + wgts + sg の 3 行、S4a で hist 撤去)
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.debug_overlay(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 400.0,
                h: 300.0,
            },
            0.0,
        );
    });
    // popup buffer の glyph_areas に 3 行 (frame_ms 省略)。
    assert_eq!(
        scene.popup_glyph_count(),
        3,
        "frame_ms=0 で frame 行省略 → 3 行"
    );
}

// -------- M9 P1-4: take_double_click_in_rect --------

/// release frame で release pos を返すヘルパ。
fn release_at(x: f32, y: f32) -> FrameInput {
    FrameInput {
        pointer: PointerFrame {
            pos: Some((x, y)),
            primary_just_released: true,
            ..PointerFrame::default()
        },
        ..Default::default()
    }
}

// -------- M14 Phase 99 (#071): take_secondary_press_in_rect --------

fn secondary_press_at(x: f32, y: f32) -> FrameInput {
    FrameInput {
        pointer: PointerFrame {
            pos: Some((x, y)),
            secondary_just_pressed: true,
            ..PointerFrame::default()
        },
        ..Default::default()
    }
}

#[test]
fn take_secondary_press_in_rect_inside_returns_some() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        secondary_press_at(120.0, 80.0),
        |(), ui| {
            assert_eq!(ui.take_secondary_press_in_rect(rect), Some((120.0, 80.0)));
        },
    );
}

#[test]
fn take_secondary_press_in_rect_outside_rect_returns_none() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let small_rect = Rect {
        x: 200.0,
        y: 200.0,
        w: 50.0,
        h: 50.0,
    };
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        secondary_press_at(10.0, 10.0),
        |(), ui| {
            assert_eq!(
                ui.take_secondary_press_in_rect(small_rect),
                None,
                "rect 外 → None"
            );
        },
    );
}

#[test]
fn take_secondary_press_in_rect_ignores_primary_press() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };
    // primary press のみ (secondary なし) → secondary press 取得は None。
    let input = FrameInput {
        pointer: PointerFrame {
            pos: Some((100.0, 100.0)),
            primary_just_pressed: true,
            primary_pressed: true,
            ..PointerFrame::default()
        },
        ..Default::default()
    };
    host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
        assert_eq!(
            ui.take_secondary_press_in_rect(rect),
            None,
            "primary press は無視"
        );
    });
}

// -------- daw_01 r.md #35: 右クリック (context menu) と右ドラッグ (矩形選択) の分離 --------

fn secondary_release_at(x: f32, y: f32) -> FrameInput {
    FrameInput {
        pointer: PointerFrame {
            pos: Some((x, y)),
            secondary_just_released: true,
            ..PointerFrame::default()
        },
        ..Default::default()
    }
}

/// press → 動かさず release で右クリック確定。 座標は **press 位置**。
#[test]
fn secondary_click_press_then_release_without_moving_returns_press_pos() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 400, height: 300 };
    let rect = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };

    // press フレーム: まだ click は確定しない (menu は release で開く)。
    host.frame_to_edits(&(), &mut scene, screen, secondary_press_at(120.0, 80.0), |(), ui| {
        assert_eq!(ui.take_secondary_click_in_rect(rect), None, "press だけでは確定しない");
    });
    // release フレーム: 移動 0px → click 確定。
    host.frame_to_edits(&(), &mut scene, screen, secondary_release_at(120.0, 80.0), |(), ui| {
        assert_eq!(ui.take_secondary_click_in_rect(rect), Some((120.0, 80.0)));
    });
}

/// press → 大きく動かして release は **右ドラッグ** なので click にしない
/// (= context menu を出さず矩形選択に譲る)。
#[test]
fn secondary_click_is_not_emitted_after_a_drag() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 400, height: 300 };
    let rect = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };

    host.frame_to_edits(&(), &mut scene, screen, secondary_press_at(120.0, 80.0), |(), _ui| {});
    host.frame_to_edits(&(), &mut scene, screen, secondary_release_at(220.0, 180.0), |(), ui| {
        assert_eq!(ui.take_secondary_click_in_rect(rect), None, "drag したので click にしない");
    });
}

/// jitter (閾値未満の微小移動) は click のまま扱う。
#[test]
fn secondary_click_tolerates_jitter_below_threshold() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 400, height: 300 };
    let rect = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };

    host.frame_to_edits(&(), &mut scene, screen, secondary_press_at(120.0, 80.0), |(), _ui| {});
    host.frame_to_edits(&(), &mut scene, screen, secondary_release_at(122.0, 81.0), |(), ui| {
        assert_eq!(ui.take_secondary_click_in_rect(rect), Some((120.0, 80.0)), "2px は jitter");
    });
}

/// 右 drag は `take_secondary_drag_rect_in_rect` が矩形として返し、 release frame で
/// `finished` が 1 度だけ立つ。 press 時の修飾キーは snapshot される。
#[test]
fn secondary_drag_rect_reports_rect_and_finishes_on_release() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 400, height: 300 };
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let wid = WidgetId::ROOT.child(b"rmb_marquee");

    // press (Shift 保持) → session 開始。
    let press = FrameInput {
        pointer: PointerFrame {
            pos: Some((100.0, 100.0)),
            secondary_just_pressed: true,
            modifiers: daw_ui_platform::Modifiers {
                shift: true,
                ..daw_ui_platform::Modifiers::default()
            },
            ..PointerFrame::default()
        },
        ..Default::default()
    };
    host.frame_to_edits(&(), &mut scene, screen, press, |(), ui| {
        let drag = ui
            .take_secondary_drag_rect_in_rect(wid, bounds)
            .expect("press frame で session が立つ");
        assert!(!drag.finished);
        assert!(drag.modifiers.shift, "press 時の修飾を snapshot する");
    });
    // release → finished + 矩形確定。
    let release = FrameInput {
        pointer: PointerFrame {
            pos: Some((160.0, 140.0)),
            secondary_just_released: true,
            ..PointerFrame::default()
        },
        ..Default::default()
    };
    host.frame_to_edits(&(), &mut scene, screen, release, |(), ui| {
        let drag = ui
            .take_secondary_drag_rect_in_rect(wid, bounds)
            .expect("release frame で 1 度返る");
        assert!(drag.finished);
        let r = drag.rect();
        assert!((r.x - 100.0).abs() < 1e-5 && (r.y - 100.0).abs() < 1e-5);
        assert!((r.w - 60.0).abs() < 1e-5 && (r.h - 40.0).abs() < 1e-5);
        assert!(drag.modifiers.shift, "modifier は press 時のものを保持");
    });
}

/// 左 drag (primary) は secondary 版の session を起動しない (ボタンが独立)。
#[test]
fn secondary_drag_rect_ignores_primary_button() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 400, height: 300 };
    let bounds = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    let wid = WidgetId::ROOT.child(b"rmb_marquee2");

    let input = FrameInput {
        pointer: PointerFrame {
            pos: Some((100.0, 100.0)),
            primary_just_pressed: true,
            primary_pressed: true,
            ..PointerFrame::default()
        },
        ..Default::default()
    };
    host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
        assert!(ui.take_secondary_drag_rect_in_rect(wid, bounds).is_none());
    });
}

#[test]
fn take_double_click_in_rect_within_threshold_returns_some() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    // 1 度目: take は None (last_click が登録されるだけ)
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(100.0, 100.0),
        |(), ui| {
            assert_eq!(
                ui.take_double_click_in_rect(rect),
                None,
                "1st release → None"
            );
        },
    );
    // 2 度目: 同位置で release → double-click として Some 返却
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(100.0, 100.0),
        |(), ui| {
            assert_eq!(
                ui.take_double_click_in_rect(rect),
                Some((100.0, 100.0)),
                "2nd release が threshold 内 → Some"
            );
        },
    );
}

/// press ベース double-click: 1 度目 click (release) の直後に同位置で press すると
/// `take_double_click_press_in_rect` が Some を返す (放さず drag を始める起点)。
#[test]
fn take_double_click_press_in_rect_detects_second_press() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    // 1 度目: release で last_click 登録 (press 検出はまだ None)。
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(100.0, 100.0),
        |(), ui| {
            assert_eq!(
                ui.take_double_click_press_in_rect(rect),
                None,
                "1st release → press 検出 None"
            );
        },
    );
    // 2 度目: 同位置で press → press ベース double-click 成立。
    host.frame_to_edits(&(), &mut scene, screen, press_at(100.0, 100.0), |(), ui| {
        assert_eq!(
            ui.take_double_click_press_in_rect(rect),
            Some((100.0, 100.0)),
            "2nd press が threshold 内 → Some"
        );
    });
}

/// press ベース検出は release ベースを壊さない: 同じ double-click で
/// `take_double_click_in_rect` (release) も従来どおり成立する (arrangement 等の既存利用を保護)。
/// = press 検出が `last_click` を消費しない (additive・非破壊) ことの回帰防止。
#[test]
fn take_double_click_press_does_not_break_release_based() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(100.0, 100.0),
        |(), _ui| {},
    );
    // 2nd press: press 検出 Some (last_click は消費しない)。
    host.frame_to_edits(&(), &mut scene, screen, press_at(100.0, 100.0), |(), ui| {
        assert_eq!(
            ui.take_double_click_press_in_rect(rect),
            Some((100.0, 100.0))
        );
    });
    // 2nd release: release ベースも従来どおり Some (press 検出が last_click を消さない証拠)。
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(100.0, 100.0),
        |(), ui| {
            assert_eq!(
                ui.take_double_click_in_rect(rect),
                Some((100.0, 100.0)),
                "press 検出後も release ベース double-click は成立 (last_click 非破壊)"
            );
        },
    );
}

#[test]
fn take_double_click_in_rect_outside_position_returns_none() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(100.0, 100.0),
        |(), _ui| {},
    );
    // 2 度目が 10px ずれる → distance > 5px なので None
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(110.0, 100.0),
        |(), ui| {
            assert_eq!(ui.take_double_click_in_rect(rect), None);
        },
    );
}

#[test]
fn take_double_click_in_rect_outside_rect_returns_none() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    // double-click は発生するが、rect 外なら None
    let small_rect = Rect {
        x: 200.0,
        y: 200.0,
        w: 50.0,
        h: 50.0,
    };

    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(100.0, 100.0),
        |(), _ui| {},
    );
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(100.0, 100.0),
        |(), ui| {
            assert_eq!(
                ui.take_double_click_in_rect(small_rect),
                None,
                "double-click 位置が rect 外 → None"
            );
        },
    );
}

#[test]
fn take_double_click_in_rect_consumes() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(50.0, 50.0),
        |(), _ui| {},
    );
    host.frame_to_edits(&(), &mut scene, screen, release_at(50.0, 50.0), |(), ui| {
        assert!(
            ui.take_double_click_in_rect(rect).is_some(),
            "1 度目 take → Some"
        );
        assert_eq!(
            ui.take_double_click_in_rect(rect),
            None,
            "2 度目 take → None (consume 済)"
        );
    });
}

#[test]
fn take_double_click_in_rect_threshold_change_works() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };
    // 閾値を 10ms / 1px に厳しくする
    host.set_double_click_threshold(10, 1.0);

    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(50.0, 50.0),
        |(), _ui| {},
    );
    // 1px 超のずれ → double-click 不成立
    host.frame_to_edits(&(), &mut scene, screen, release_at(52.0, 50.0), |(), ui| {
        assert_eq!(
            ui.take_double_click_in_rect(rect),
            None,
            "threshold 1px なので 2px ずれは double-click 不成立"
        );
    });
}

#[test]
fn take_double_click_in_rect_triple_click_does_not_double_fire() {
    // 3 連続 release で「2 度目で double-click 成立 → 3 度目は double-click にならない」
    // (last_click は 2 度目で None にクリアされる)
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(50.0, 50.0),
        |(), _ui| {},
    );
    host.frame_to_edits(&(), &mut scene, screen, release_at(50.0, 50.0), |(), ui| {
        assert!(ui.take_double_click_in_rect(rect).is_some(), "2nd → Some");
    });
    // 3rd release: 2nd で last_click が None になっているので double-click 不成立
    host.frame_to_edits(&(), &mut scene, screen, release_at(50.0, 50.0), |(), ui| {
        assert_eq!(
            ui.take_double_click_in_rect(rect),
            None,
            "3rd release は 2nd の double-click 後なので Single click 扱い"
        );
    });
}

// -------- M9 Phase 46 (daw_01 #015): modal popup の下に隠れた widget の入力を遮断する --------

/// daw_01 #015 root cause: arrangement_view が plugin_picker (modal) より先に走り
/// `take_scroll_in_rect(lanes)` が pointer (modal panel 内) の scroll_delta を消費 →
/// list_view の scroll_area が呼ぶ頃には (0, 0) になっていた。
///
/// 修正: `take_scroll_in_rect` 冒頭で `pointer_blocked_by_modal_popup()` 判定 →
/// modal popup anchor 内 pointer かつ drawing_in_popup でない場合は (0, 0) を返す。
/// popup_layer 内の widget (modal の body) は drawing_in_popup=true で通常通り消費可能。
#[test]
fn take_scroll_returns_zero_when_under_modal_anchor_outside_popup_layer() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 800,
        height: 600,
    };
    let anchor = Rect {
        x: 100.0,
        y: 100.0,
        w: 200.0,
        h: 200.0,
    };
    let pos = (150.0, 150.0); // anchor 内

    // 1 frame目: open_popup で modal popup を開く (anchor 確定)
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.open_popup("test_modal", anchor, true);
    });

    // 2 frame目: pointer が anchor 内、scroll_delta あり。
    // 通常 widget (drawing_in_popup=false) の take_scroll_in_rect → (0, 0)
    // popup_layer 内 (drawing_in_popup=true) の take_scroll_in_rect → (0, -3)
    let outside_scroll = std::cell::Cell::new((0.0_f32, 0.0_f32));
    let inside_scroll = std::cell::Cell::new((0.0_f32, 0.0_f32));
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            pointer: PointerFrame {
                pos: Some(pos),
                scroll_delta: (0.0, -3.0),
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        },
        |(), ui| {
            // 通常 widget: anchor 内 pointer で消費しようとしても (0, 0)
            outside_scroll.set(ui.take_scroll_in_rect(anchor));
            // popup_layer 内: 通常通り消費可能
            ui.popup_layer("test_modal", |ui| {
                inside_scroll.set(ui.take_scroll_in_rect(anchor));
            });
        },
    );
    assert_eq!(
        outside_scroll.get(),
        (0.0, 0.0),
        "modal 下では scroll は消費されない"
    );
    assert_eq!(
        inside_scroll.get(),
        (0.0, -3.0),
        "popup_layer 内では消費される"
    );
}

/// `take_drag_rect_in_rect` も同じく modal anchor 下では drag を始めない。
#[test]
fn take_drag_rect_blocked_under_modal_anchor() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 800,
        height: 600,
    };
    let anchor = Rect {
        x: 100.0,
        y: 100.0,
        w: 200.0,
        h: 200.0,
    };
    let bounds = anchor; // 同じ rect で drag を始めようとしても block される
    let pos = (150.0, 150.0);

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.open_popup("modal2", anchor, true);
    });

    let outside_drag_some = std::cell::Cell::new(true);
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            pointer: PointerFrame {
                pos: Some(pos),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        },
        |(), ui| {
            let wid = WidgetId::ROOT.child(b"drag");
            outside_drag_some.set(ui.take_drag_rect_in_rect(wid, bounds).is_some());
        },
    );
    assert!(!outside_drag_some.get(), "modal 下では drag が始まらない");
}

/// `take_double_click_in_rect` も同じく modal anchor 下では double-click を返さない。
#[test]
fn take_double_click_blocked_under_modal_anchor() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 800,
        height: 600,
    };
    let anchor = Rect {
        x: 100.0,
        y: 100.0,
        w: 200.0,
        h: 200.0,
    };

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.open_popup("modal3", anchor, true);
    });

    // 1st release で last_click 登録、2nd release で double-click 成立する条件を満たすが、
    // anchor 下なので take_double_click_in_rect は None を返す。
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(150.0, 150.0),
        |(), _ui| {},
    );
    let observed = std::cell::Cell::new(Some((0.0_f32, 0.0_f32)));
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_at(150.0, 150.0),
        |(), ui| {
            observed.set(ui.take_double_click_in_rect(anchor));
        },
    );
    assert_eq!(
        observed.get(),
        None,
        "modal 下では double-click は返されない"
    );
}

// -------- M14 Phase 63a (daw_01 #014): popup overlay は外側 with_clip_rect から免除 --------

/// daw_01 #014 regression: piano_roll の snap dropdown が tab pane (with_clip_rect で
/// 囲まれた領域) 内で完全に消える bug。 root cause は `push_rect/text/lines` が
/// `drawing_in_popup` の真偽に関係なく `merge_clip(current_clip, ..)` を popup primitive
/// にも適用していたこと。 popup overlay は z-order 最前面の modal なので、 base scene の
/// clip 制約から免除されるべき (Cubase / Live / 一般 GUI toolkit と同 semantics)。
///
/// 修正: `popup_layer` entry で `current_clip` を `None` に一時退避し、 退出時 restore。
#[test]
fn popup_primitives_not_clipped_by_outer_with_clip_rect() {
    use daw_ui_renderer::{Color, RectCommand};

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 800,
        height: 600,
    };
    // pane_rect は piano_roll が tab pane で囲まれる典型 case を想定。
    let pane_rect = Rect {
        x: 0.0,
        y: 200.0,
        w: 800.0,
        h: 200.0,
    };
    let popup_anchor = Rect {
        x: 100.0,
        y: 280.0,
        w: 120.0,
        h: 24.0,
    };

    // 1 frame目: modal popup を open (anchor 確定)
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.open_popup("p_clip_test", popup_anchor, true);
    });

    // 2 frame目: with_clip_rect(pane_rect) 内で popup_layer 経由で rect を push。
    // 修正前は popup primitive の clip_rect が pane_rect を継承して画面に出ても見えなかった。
    // 修正後は popup_layer entry で current_clip = None なので、 popup primitive は
    // 外側 pane の clip 制約を受けない (renderer は popup pass で全画面に描画可能)。
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.with_clip_rect(pane_rect, |ui| {
            ui.popup_layer("p_clip_test", |ui| {
                ui.push_rect(RectCommand {
                    rect: Rect {
                        x: 100.0,
                        y: 50.0,
                        w: 120.0,
                        h: 480.0,
                    },
                    fill: Color::rgb(0.1, 0.1, 0.1),
                    border: Color::rgb(0.3, 0.3, 0.3),
                    border_width: 1.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            });
        });
    });

    let popup_rects = scene.popup_rects_vec();
    assert_eq!(popup_rects.len(), 1, "popup primitive が 1 件積まれた");
    assert_eq!(
        popup_rects[0].clip_rect, None,
        "popup primitive の clip_rect は外側 with_clip_rect (pane_rect) を継承しない"
    );
}

// -------- M14 Phase 63l (daw_01 #026): take_primary_press_in_rect / take_drag_in_rect --------

/// press frame で `pos` の primary just_pressed を返すヘルパ。
fn press_at(x: f32, y: f32) -> FrameInput {
    FrameInput {
        pointer: PointerFrame {
            pos: Some((x, y)),
            primary_just_pressed: true,
            primary_pressed: true,
            ..PointerFrame::default()
        },
        ..Default::default()
    }
}

/// drag 中 (= 既に press 済 + pointer 移動中) を表すヘルパ。
fn hold_at(x: f32, y: f32) -> FrameInput {
    FrameInput {
        pointer: PointerFrame {
            pos: Some((x, y)),
            primary_pressed: true,
            ..PointerFrame::default()
        },
        ..Default::default()
    }
}

/// drag release frame ヘルパ。
fn release_pressed_at(x: f32, y: f32) -> FrameInput {
    FrameInput {
        pointer: PointerFrame {
            pos: Some((x, y)),
            primary_just_released: true,
            ..PointerFrame::default()
        },
        ..Default::default()
    }
}

#[test]
fn take_primary_press_in_rect_returns_some_on_press_inside() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 50.0,
        y: 50.0,
        w: 100.0,
        h: 100.0,
    };

    let observed = std::cell::Cell::new(None);
    host.frame_to_edits(&(), &mut scene, screen, press_at(100.0, 100.0), |(), ui| {
        observed.set(ui.take_primary_press_in_rect(rect));
    });
    assert_eq!(observed.get(), Some((100.0, 100.0)));
}

#[test]
fn take_primary_press_in_rect_returns_none_outside_rect() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 50.0,
        y: 50.0,
        w: 50.0,
        h: 50.0,
    };

    let observed = std::cell::Cell::new(Some((0.0_f32, 0.0_f32)));
    // press は来るが rect 外
    host.frame_to_edits(&(), &mut scene, screen, press_at(200.0, 200.0), |(), ui| {
        observed.set(ui.take_primary_press_in_rect(rect));
    });
    assert_eq!(observed.get(), None);
}

#[test]
fn take_primary_press_in_rect_returns_none_without_just_pressed() {
    // primary_just_pressed が false (= primary_pressed のみ true) のフレームでは消費しない
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    let observed = std::cell::Cell::new(Some((0.0_f32, 0.0_f32)));
    host.frame_to_edits(&(), &mut scene, screen, hold_at(100.0, 100.0), |(), ui| {
        observed.set(ui.take_primary_press_in_rect(rect));
    });
    assert_eq!(observed.get(), None, "press transition なし → None");
}

#[test]
fn take_primary_press_in_rect_consumes_within_frame() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    let first = std::cell::Cell::new(None);
    let second = std::cell::Cell::new(Some((0.0_f32, 0.0_f32)));
    host.frame_to_edits(&(), &mut scene, screen, press_at(150.0, 150.0), |(), ui| {
        first.set(ui.take_primary_press_in_rect(rect));
        second.set(ui.take_primary_press_in_rect(rect));
    });
    assert_eq!(first.get(), Some((150.0, 150.0)), "1 度目 take → Some");
    assert_eq!(
        second.get(),
        None,
        "2 度目 take → None (consume_pointer_click 済)"
    );
}

#[test]
fn take_primary_press_in_rect_blocked_under_modal_anchor() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let anchor = Rect {
        x: 50.0,
        y: 50.0,
        w: 200.0,
        h: 200.0,
    };

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.open_popup("press_modal", anchor, true);
    });

    let observed = std::cell::Cell::new(Some((0.0_f32, 0.0_f32)));
    host.frame_to_edits(&(), &mut scene, screen, press_at(150.0, 150.0), |(), ui| {
        observed.set(ui.take_primary_press_in_rect(anchor));
    });
    assert_eq!(observed.get(), None, "modal 下では press は返されない");
}

#[test]
fn take_drag_in_rect_started_continuing_released_lifecycle() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    // frame 1: press → Started
    let phase1 = std::cell::Cell::new(None::<DragKind>);
    let anchor1 = std::cell::Cell::new(None::<(f32, f32)>);
    host.frame_to_edits(&(), &mut scene, screen, press_at(100.0, 100.0), |(), ui| {
        if let Some(d) = ui.take_drag_in_rect("session1", rect) {
            phase1.set(Some(d.kind));
            anchor1.set(Some(d.anchor));
        }
    });
    assert_eq!(phase1.get(), Some(DragKind::Started));
    assert_eq!(anchor1.get(), Some((100.0, 100.0)));

    // frame 2: hold (move) → Continuing + delta が更新
    let phase2 = std::cell::Cell::new(None::<DragKind>);
    let delta2 = std::cell::Cell::new((0.0_f32, 0.0_f32));
    host.frame_to_edits(&(), &mut scene, screen, hold_at(120.0, 110.0), |(), ui| {
        if let Some(d) = ui.take_drag_in_rect("session1", rect) {
            phase2.set(Some(d.kind));
            delta2.set(d.delta);
        }
    });
    assert_eq!(phase2.get(), Some(DragKind::Continuing));
    assert!((delta2.get().0 - 20.0).abs() < 1e-5);
    assert!((delta2.get().1 - 10.0).abs() < 1e-5);

    // frame 3: release → Released
    let phase3 = std::cell::Cell::new(None::<DragKind>);
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_pressed_at(130.0, 115.0),
        |(), ui| {
            if let Some(d) = ui.take_drag_in_rect("session1", rect) {
                phase3.set(Some(d.kind));
            }
        },
    );
    assert_eq!(phase3.get(), Some(DragKind::Released));

    // frame 4: idle → None
    let phase4 = std::cell::Cell::new(Some(DragKind::Released));
    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        phase4.set(ui.take_drag_in_rect("session1", rect).map(|d| d.kind));
    });
    assert_eq!(phase4.get(), None);
}

#[test]
fn take_drag_in_rect_starts_only_inside_rect() {
    // rect 外で press されても session は始まらない (anchor None のまま)
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 200.0,
        y: 200.0,
        w: 50.0,
        h: 50.0,
    };

    let observed = std::cell::Cell::new(Some(DragKind::Started));
    host.frame_to_edits(&(), &mut scene, screen, press_at(50.0, 50.0), |(), ui| {
        observed.set(ui.take_drag_in_rect("outside_press", rect).map(|d| d.kind));
    });
    assert_eq!(observed.get(), None);

    // 次フレームに hold で pointer が rect 内に入っても、 session は始まっていないので None
    let observed2 = std::cell::Cell::new(Some(DragKind::Started));
    host.frame_to_edits(&(), &mut scene, screen, hold_at(220.0, 220.0), |(), ui| {
        observed2.set(ui.take_drag_in_rect("outside_press", rect).map(|d| d.kind));
    });
    assert_eq!(observed2.get(), None, "rect 外 press は session を開かない");
}

#[test]
fn take_drag_in_rect_continues_when_pointer_leaves_rect() {
    // rect 内で press → 次フレームに rect 外に pointer が出ても Continuing で session 継続
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 100.0,
        y: 100.0,
        w: 50.0,
        h: 50.0,
    };

    let p1 = std::cell::Cell::new(None::<DragKind>);
    host.frame_to_edits(&(), &mut scene, screen, press_at(120.0, 120.0), |(), ui| {
        p1.set(ui.take_drag_in_rect("leave", rect).map(|d| d.kind));
    });
    assert_eq!(p1.get(), Some(DragKind::Started));

    // pointer が rect から出る位置 (300, 200) に移動
    let p2 = std::cell::Cell::new(None::<DragKind>);
    let delta2 = std::cell::Cell::new((0.0_f32, 0.0_f32));
    host.frame_to_edits(&(), &mut scene, screen, hold_at(300.0, 200.0), |(), ui| {
        if let Some(d) = ui.take_drag_in_rect("leave", rect) {
            p2.set(Some(d.kind));
            delta2.set(d.delta);
        }
    });
    assert_eq!(
        p2.get(),
        Some(DragKind::Continuing),
        "rect 外でも session 継続"
    );
    assert!((delta2.get().0 - 180.0).abs() < 1e-5);
    assert!((delta2.get().1 - 80.0).abs() < 1e-5);
}

#[test]
fn take_drag_in_rect_blocked_under_modal_anchor() {
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let anchor = Rect {
        x: 50.0,
        y: 50.0,
        w: 200.0,
        h: 200.0,
    };

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        ui.open_popup("drag_modal", anchor, true);
    });

    let observed = std::cell::Cell::new(Some(DragKind::Started));
    host.frame_to_edits(&(), &mut scene, screen, press_at(150.0, 150.0), |(), ui| {
        observed.set(ui.take_drag_in_rect("blocked", anchor).map(|d| d.kind));
    });
    assert_eq!(observed.get(), None, "modal 下では drag が始まらない");
}

#[test]
fn take_drag_in_rect_release_returned_only_once() {
    // Released を返した後の同 frame に同 id で再度呼ぶと None (state 既に clear)
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    // session を開始しておく
    host.frame_to_edits(&(), &mut scene, screen, press_at(100.0, 100.0), |(), ui| {
        assert!(ui.take_drag_in_rect("once", rect).is_some());
    });
    // release frame で 1 度目 Released、 2 度目は None
    let first = std::cell::Cell::new(None::<DragKind>);
    let second = std::cell::Cell::new(Some(DragKind::Released));
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        release_pressed_at(110.0, 110.0),
        |(), ui| {
            first.set(ui.take_drag_in_rect("once", rect).map(|d| d.kind));
            second.set(ui.take_drag_in_rect("once", rect).map(|d| d.kind));
        },
    );
    assert_eq!(first.get(), Some(DragKind::Released));
    assert_eq!(
        second.get(),
        None,
        "1 度 Released を返した後 anchor は cleared"
    );
}

#[test]
fn take_drag_in_rect_consumes_press_within_start_frame() {
    // drag 開始 frame に同じ rect で take_primary_press_in_rect を呼んでも consume 済 → None
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    let drag_phase = std::cell::Cell::new(None::<DragKind>);
    let press_after = std::cell::Cell::new(Some((0.0_f32, 0.0_f32)));
    host.frame_to_edits(&(), &mut scene, screen, press_at(100.0, 100.0), |(), ui| {
        drag_phase.set(ui.take_drag_in_rect("consume_test", rect).map(|d| d.kind));
        press_after.set(ui.take_primary_press_in_rect(rect));
    });
    assert_eq!(drag_phase.get(), Some(DragKind::Started));
    assert_eq!(
        press_after.get(),
        None,
        "drag 開始 frame に press は consume 済"
    );
}

#[test]
fn take_drag_in_rect_records_start_modifiers() {
    // start 時の Shift 押下が start_modifiers に記録され、 Continuing/Released まで保持される
    use daw_ui_platform::Modifiers;
    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize {
        width: 400,
        height: 300,
    };
    let rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    let shift_only = Modifiers {
        shift: true,
        ..Modifiers::empty()
    };

    // press 時に Shift 押下中
    let start_mods_p1 = std::cell::Cell::new(Modifiers::empty());
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            pointer: PointerFrame {
                pos: Some((100.0, 100.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: shift_only,
                ..PointerFrame::default()
            },
            ..Default::default()
        },
        |(), ui| {
            if let Some(d) = ui.take_drag_in_rect("mod_test", rect) {
                start_mods_p1.set(d.start_modifiers);
            }
        },
    );
    assert!(start_mods_p1.get().shift, "Started で Shift 記録");

    // Continuing で Shift を離しても start_modifiers は SHIFT のまま、 modifiers は empty
    let start_mods_p2 = std::cell::Cell::new(Modifiers::empty());
    let cur_mods_p2 = std::cell::Cell::new(shift_only);
    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput {
            pointer: PointerFrame {
                pos: Some((110.0, 110.0)),
                primary_pressed: true,
                modifiers: Modifiers::empty(),
                ..PointerFrame::default()
            },
            ..Default::default()
        },
        |(), ui| {
            if let Some(d) = ui.take_drag_in_rect("mod_test", rect) {
                start_mods_p2.set(d.start_modifiers);
                cur_mods_p2.set(d.modifiers);
            }
        },
    );
    assert!(
        start_mods_p2.get().shift,
        "Continuing でも start_modifiers は SHIFT 保持"
    );
    assert!(
        !cur_mods_p2.get().shift,
        "modifiers (現フレーム) は SHIFT 解除を反映"
    );
}
