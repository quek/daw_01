//! 基本 API (label / button / fader / Edit / frame) が **`Clone` も `PartialEq` も
//! `Hash` も `Default` も持たない Model 型** でコンパイルすることを確認する。
//!
//! ここでビルド失敗するなら、API シグネチャに余計な制約 (例: `M: Clone`) が
//! 紛れ込んでいるか、`Application::Message: Clone` のような型境界が露出している。

use daw_ui_core::{Edit, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

// 意図的に derive マクロを一切付けない。
// String / Vec などの非 Copy フィールドを混ぜて、Model 全体に Copy/Clone が
// 自動派生しないことも担保する。
struct Model {
    counter: u32,
    label: String,
    history: Vec<u32>,
    volume: f32,
    mute: bool,
    title: String,
}

fn main() {
    let mut host: UiHost<Model> = UiHost::new();
    let mut scene = Scene::new();
    let mut model = Model {
        counter: 0,
        label: String::from("hi"),
        history: Vec::new(),
        volume: 0.5,
        mute: false,
        title: String::from("untitled"),
    };

    let edits = host.frame(
        &model,
        &mut scene,
        PhysicalSize { width: 800, height: 600 },
        daw_ui_core::FrameInput::default(),
        |m, ui| {
            ui.label("title", &m.label);
            ui.button("inc", "increment", || {
                Edit::mutate(|m: &mut Model| {
                    m.counter += 1;
                    m.history.push(m.counter);
                })
            });
            // fader (M3): 矩形指定 + vstack 版の両方が non-Clone Model でコンパイルする。
            let _ = ui.fader_at(
                "vol",
                Rect { x: 0.0, y: 0.0, w: 32.0, h: 120.0 },
                m.volume,
                |v| Edit::mutate(move |m: &mut Model| m.volume = v),
            );
            let _ = ui.fader("vol2", m.volume, |v| {
                Edit::mutate(move |m: &mut Model| m.volume = v)
            });
            // knob (M3): 同様に non-Clone Model でコンパイルする。
            let _ = ui.knob_at(
                "pan",
                Rect { x: 0.0, y: 0.0, w: 64.0, h: 64.0 },
                m.volume,
                |v| Edit::mutate(move |m: &mut Model| m.volume = v),
            );
            let _ = ui.knob("pan2", m.volume, |v| {
                Edit::mutate(move |m: &mut Model| m.volume = v)
            });
            // checkbox (M3): non-Clone Model でコンパイルする。
            let _ = ui.checkbox_at(
                "mute",
                Rect { x: 0.0, y: 0.0, w: 100.0, h: 24.0 },
                m.mute,
                "Mute",
                |new| Edit::mutate(move |m: &mut Model| m.mute = new),
            );
            let _ = ui.checkbox("mute2", m.mute, "Mute", |new| {
                Edit::mutate(move |m: &mut Model| m.mute = new)
            });
            // text_input (M3 Phase 4b): non-Clone Model でコンパイルする。
            let _ = ui.text_input_at(
                "title",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 28.0 },
                &m.title,
                |new| Edit::mutate(move |m: &mut Model| m.title = new),
            );
            let _ = ui.text_input("title2", &m.title, |new| {
                Edit::mutate(move |m: &mut Model| m.title = new)
            });
        },
    );

    for e in edits {
        e.apply(&mut model);
    }
}
