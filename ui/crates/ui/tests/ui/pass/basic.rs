//! 基本 API (label / button / fader / Edit / frame) が **`Clone` も `PartialEq` も
//! `Hash` も `Default` も持たない Model 型** でコンパイルすることを確認する。
//!
//! ここでビルド失敗するなら、API シグネチャに余計な制約 (例: `M: Clone`) が
//! 紛れ込んでいるか、`Application::Message: Clone` のような型境界が露出している。

use daw_ui_core::{
    ColorPickerStyle, Edit, KnobStyle, ReorderableListEditRequest,
    ReorderableListStyle, ScrubableNumberFormat, ScrubableNumberStyle, UiHost,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect, Scene};

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
    // M11 Phase 51: reorderable_list widget 用 (non-Clone Model でも呼べることを担保)
    chain: Vec<String>,
}

fn main() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let mut model = Model {
        counter: 0,
        label: String::from("hi"),
        history: Vec::new(),
        volume: 0.5,
        mute: false,
        title: String::from("untitled"),
        chain: Vec::new(),
    };

    let edits = host.frame_to_edits(
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
            // M14 Phase 105 (daw_01 #076): button_at_clicked_sized (font_size 可変版、track 名を
            // style.track_text_size に追従させる用) も non-Clone Model でコンパイルすることを CI 固定。
            let _ = ui.button_at_clicked_sized(
                "inc_sized",
                "increment",
                Rect { x: 0.0, y: 0.0, w: 100.0, h: 28.0 },
                12.0,
            );
            // fader (M3): 矩形指定 + vstack 版の両方が non-Clone Model でコンパイルする。
            // default_value (4 番目の引数) はダブルクリックリセット用 (M3 Phase 4d)。
            // on_change は `Fn(f32) -> Edit<M>` のみ (S4a で lib undo 撤去、Clone 拘束不要)。
            let _ = ui.fader_at(
                "vol",
                Rect { x: 0.0, y: 0.0, w: 32.0, h: 120.0 },
                m.volume,
                0.0,
                None,
                |v| Edit::mutate(move |m: &mut Model| m.volume = v),
                None,
            );
            let _ = ui.fader("vol2", m.volume, 0.0, None, |v| {
                Edit::mutate(move |m: &mut Model| m.volume = v)
            });
            // knob (M3): 同様に non-Clone Model でコンパイルする。
            let _ = ui.knob_at(
                "pan",
                Rect { x: 0.0, y: 0.0, w: 64.0, h: 64.0 },
                m.volume,
                0.5,
                &KnobStyle::BIPOLAR,
                |v| Edit::mutate(move |m: &mut Model| m.volume = v),
                None,
            );
            let _ = ui.knob("pan2", m.volume, 0.5, &KnobStyle::BIPOLAR, |v| {
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
            // M11 Phase 52: text_input_at_focused (open 時自動 focus 版) も non-Clone Model で
            // コンパイルする。
            let _ = ui.text_input_at_focused(
                "rename",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 28.0 },
                &m.title,
                |new| Edit::mutate(move |m: &mut Model| m.title = new),
            );
            // M9 Phase 43: debug_overlay も non-Clone Model でコンパイルする。
            ui.debug_overlay(Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 }, 5.5);
            // M11 Phase 51: reorderable_list widget が non-Clone Model でコンパイルする。
            let rl_style = ReorderableListStyle::default();
            let _ = ui.reorderable_list(
                "rl",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                &m.chain,
                None,
                &rl_style,
                |req| match req {
                    ReorderableListEditRequest::Reorder(_) => Edit::mutate(|_m: &mut Model| {}),
                },
                |_ui, _name: &String, _i, _row, _sel, _drag| {},
            );
            // M14 Phase 64a (daw_01 #035): scrubable_number widget が non-Clone Model でコンパイルする。
            let scn_style = ScrubableNumberStyle::default();
            let _ = ui.scrubable_number_at(
                "scn",
                Rect { x: 0.0, y: 0.0, w: 80.0, h: 28.0 },
                120.0,
                120.0,
                ScrubableNumberFormat::Decimal(1),
                &scn_style,
                |_v: f64| Edit::mutate(|_m: &mut Model| {}),
                None,
                None,
            );
            // M14 Phase 88 (daw_01 #058): color_picker widget が non-Clone Model でコンパイルする。
            // Model に一切触れない (response を返すだけ) ので構造的に no-Clone 安全だが、API が
            // 露出していることを CI 固定する。
            let cp_style = ColorPickerStyle::default();
            let _ = ui.color_picker(
                "cp",
                Rect { x: 0.0, y: 0.0, w: 50.0, h: 20.0 },
                Color::rgb(0.5, 0.5, 0.5),
                &[Color::rgb(1.0, 0.0, 0.0), Color::rgb(0.0, 1.0, 0.0)],
                &cp_style,
            );
        },
    );

    for e in edits {
        e.apply(&mut model);
    }
}
