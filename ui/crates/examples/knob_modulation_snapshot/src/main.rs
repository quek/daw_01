//! knob の Bitwig 流 modulation (daw_01 #109) の offscreen pixel verify。
//!
//! `UiHost::no_redraw` で modulation 付き knob を数パターン描き、 OffscreenRenderer で PNG 化して
//! リング上の色弧 / 可動 live 半径マーク / depth-edit 枠強調 + 編集弧のレイアウトを目視確認する。
//! 値も depth も knob と同じ 0..=1 正規化ドメイン (= 弧 = 0..=1 そのもの、 range 引数不要)。
//! 実行: `cargo run --bin knob_modulation_snapshot` → `<workspace>/target/knob_modulation_snapshot.png`。

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_core::{Edit, FrameInput, ModEdit, ModEntry, Modulation, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, OffscreenRenderer, Rect, Scene};

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 470;
    let height: u32 = 150;

    let cyan = Color::rgb(0.20, 0.80, 1.0);
    let orange = Color::rgb(1.0, 0.55, 0.18);
    let green = Color::rgb(0.35, 0.85, 0.40);

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.16, 0.17, 0.20).to_wgpu();
    let screen = PhysicalSize { width, height };

    let label_color = Color::rgb(0.80, 0.80, 0.84);
    let noop = |_v: f32| Edit::mutate(|(): &mut ()| {});
    let on_mod = |_d: f64| Edit::mutate(|(): &mut ()| {});

    let knob_w = 64.0_f32;
    let y = 42.0_f32;
    let step = 90.0_f32;
    let x0 = 16.0_f32;
    let xs = [x0, x0 + step, x0 + step * 2.0, x0 + step * 3.0, x0 + step * 4.0];

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        // (1) 2 source の色弧 (cyan +0.30 / orange -0.15) + live 半径マーク。
        ui.label_at("l1", "2 src + live", xs[0], 20.0, 11.0, label_color);
        let e1 = [
            ModEntry { color: cyan, depth: 0.30 },
            ModEntry { color: orange, depth: -0.15 },
        ];
        ui.knob_at(
            "k1",
            Rect { x: xs[0], y, w: knob_w, h: knob_w },
            0.50,
            0.5,
            "pan",
            noop,
            Some(Modulation { entries: &e1, live_value: Some(0.68), edit: None }),
        );

        // (2) depth-edit (arm) 中: green 枠強調 + 編集弧 + live マーク。
        ui.label_at("l2", "armed", xs[1], 20.0, 11.0, label_color);
        let e2 = [ModEntry { color: green, depth: 0.35 }];
        ui.knob_at(
            "k2",
            Rect { x: xs[1], y, w: knob_w, h: knob_w },
            0.40,
            0.5,
            "pan",
            noop,
            Some(Modulation {
                entries: &e2,
                live_value: Some(0.62),
                edit: Some(ModEdit {
                    source_color: green,
                    current_depth: 0.35,
                    depth_range: Some((-1.0, 1.0)),
                    depth_sensitivity: None,
                    on_mod_change: &on_mod,
                }),
            }),
        );

        // (3) 負 depth のみ (base 角から反時計回りへ伸びる弧)。
        ui.label_at("l3", "neg depth", xs[2], 20.0, 11.0, label_color);
        let e3 = [ModEntry { color: orange, depth: -0.40 }];
        ui.knob_at(
            "k3",
            Rect { x: xs[2], y, w: knob_w, h: knob_w },
            0.70,
            0.5,
            "pan",
            noop,
            Some(Modulation { entries: &e3, live_value: None, edit: None }),
        );

        // (4) modulation None (= 完全回帰、 弧なし)。
        ui.label_at("l4", "no mod", xs[3], 20.0, 11.0, label_color);
        ui.knob_at(
            "k4",
            Rect { x: xs[3], y, w: knob_w, h: knob_w },
            0.50,
            0.5,
            "pan",
            noop,
            None,
        );

        // (5) 3 source の重畳 (リング帯を内側へ 3 分割)。
        ui.label_at("l5", "3 src", xs[4], 20.0, 11.0, label_color);
        let e5 = [
            ModEntry { color: cyan, depth: 0.20 },
            ModEntry { color: green, depth: 0.35 },
            ModEntry { color: orange, depth: -0.25 },
        ];
        ui.knob_at(
            "k5",
            Rect { x: xs[4], y, w: knob_w, h: knob_w },
            0.45,
            0.5,
            "pan",
            noop,
            Some(Modulation { entries: &e5, live_value: Some(0.50), edit: None }),
        );
    });

    let mut renderer = OffscreenRenderer::new(width, height)?;
    let rgba = renderer.render_to_rgba(&scene)?;

    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("knob_modulation_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("knob modulation snapshot saved to {}", out_path.display());
    Ok(())
}

fn save_png(path: &Path, rgba: &[u8], width: u32, height: u32) -> Result<(), Box<dyn Error>> {
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    Ok(())
}
