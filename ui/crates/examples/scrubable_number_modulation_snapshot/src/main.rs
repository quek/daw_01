//! scrubable_number の Bitwig 流 modulation (daw_01 #107) の offscreen pixel verify。
//!
//! `UiHost::no_redraw` で modulation 付き scrubable_number を数パターン描き、 OffscreenRenderer で
//! PNG 化して 色帯 / base マーカー / live tick / depth-edit 枠強調のレイアウトを目視確認する。
//! 実行: `cargo run --bin scrubable_number_modulation_snapshot` → `<workspace>/target/scrubable_number_modulation_snapshot.png`。

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_core::{
    Edit, FrameInput, ModEdit, ModEntry, Modulation, ScrubableNumberFormat, ScrubableNumberStyle,
    UiHost,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, OffscreenRenderer, Rect, Scene};

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 380;
    let height: u32 = 220;

    let cyan = Color::rgb(0.20, 0.80, 1.0);
    let orange = Color::rgb(1.0, 0.55, 0.18);
    let green = Color::rgb(0.35, 0.85, 0.40);

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.16, 0.17, 0.20).to_wgpu();
    let screen = PhysicalSize { width, height };

    // vol/pan 風の 0..1 param。
    let style = ScrubableNumberStyle {
        range: Some((0.0, 1.0)),
        font_size: 13.0,
        ..ScrubableNumberStyle::default()
    };
    let label_color = Color::rgb(0.80, 0.80, 0.84);
    let noop = |_v: f64| Edit::mutate(|(): &mut ()| {});
    let on_mod = |_d: f64| Edit::mutate(|(): &mut ()| {});
    let row_x = 150.0_f32;
    let row_w = 200.0_f32;
    let row_h = 26.0_f32;

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        // (1) 2 source の色帯 (cyan +0.30 / orange -0.15) + live tick。
        ui.label_at("l1", "2 sources + live", 16.0, 30.0, 12.0, label_color);
        let e1 = [
            ModEntry { color: cyan, depth: 0.30 },
            ModEntry { color: orange, depth: -0.15 },
        ];
        ui.scrubable_number_at(
            "p1",
            Rect { x: row_x, y: 24.0, w: row_w, h: row_h },
            0.50,
            0.50,
            ScrubableNumberFormat::Decimal(2),
            &style,
            "x",
            noop,
            None,
            Some(Modulation { entries: &e1, live_value: Some(0.62), edit: None }),
        );

        // (2) depth-edit (arm) 中: green 枠強調 + 編集中 depth 帯 + live tick。
        ui.label_at("l2", "armed (depth-edit)", 16.0, 70.0, 12.0, label_color);
        let e2 = [ModEntry { color: green, depth: 0.40 }];
        ui.scrubable_number_at(
            "p2",
            Rect { x: row_x, y: 64.0, w: row_w, h: row_h },
            0.40,
            0.50,
            ScrubableNumberFormat::Decimal(2),
            &style,
            "x",
            noop,
            None,
            Some(Modulation {
                entries: &e2,
                live_value: Some(0.55),
                edit: Some(ModEdit {
                    source_color: green,
                    current_depth: 0.40,
                    depth_range: Some((-1.0, 1.0)),
                    depth_sensitivity: None,
                    on_mod_change: &on_mod,
                }),
            }),
        );

        // (3) 負 depth のみ (base から左へ伸びる帯)。
        ui.label_at("l3", "negative depth", 16.0, 110.0, 12.0, label_color);
        let e3 = [ModEntry { color: orange, depth: -0.35 }];
        ui.scrubable_number_at(
            "p3",
            Rect { x: row_x, y: 104.0, w: row_w, h: row_h },
            0.70,
            0.50,
            ScrubableNumberFormat::Decimal(2),
            &style,
            "x",
            noop,
            None,
            Some(Modulation { entries: &e3, live_value: None, edit: None }),
        );

        // (4) modulation None (= 完全回帰、 帯なし)。
        ui.label_at("l4", "no modulation", 16.0, 150.0, 12.0, label_color);
        ui.scrubable_number_at(
            "p4",
            Rect { x: row_x, y: 144.0, w: row_w, h: row_h },
            0.50,
            0.50,
            ScrubableNumberFormat::Decimal(2),
            &style,
            "x",
            noop,
            None,
            None,
        );

        // (5) 3 source の重畳 (狭い strip を縦 3 分割)。
        ui.label_at("l5", "3 sources stacked", 16.0, 190.0, 12.0, label_color);
        let e5 = [
            ModEntry { color: cyan, depth: 0.20 },
            ModEntry { color: green, depth: 0.35 },
            ModEntry { color: orange, depth: -0.25 },
        ];
        ui.scrubable_number_at(
            "p5",
            Rect { x: row_x, y: 184.0, w: row_w, h: row_h },
            0.45,
            0.50,
            ScrubableNumberFormat::Decimal(2),
            &style,
            "x",
            noop,
            None,
            Some(Modulation { entries: &e5, live_value: Some(0.50), edit: None }),
        );
    });

    let mut renderer = OffscreenRenderer::new(width, height)?;
    let rgba = renderer.render_to_rgba(&scene)?;

    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("scrubable_number_modulation_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("scrubable_number modulation snapshot saved to {}", out_path.display());
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
