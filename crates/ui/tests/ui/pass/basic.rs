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
}

fn main() {
    let mut host: UiHost<Model> = UiHost::new();
    let mut scene = Scene::new();
    let mut model = Model {
        counter: 0,
        label: String::from("hi"),
        history: Vec::new(),
        volume: 0.5,
    };

    let edits = host.frame(
        &model,
        &mut scene,
        PhysicalSize { width: 800, height: 600 },
        PointerFrame::default(),
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
        },
    );

    for e in edits {
        e.apply(&mut model);
    }
}
