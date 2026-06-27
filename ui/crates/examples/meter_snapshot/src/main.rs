//! level_meter (Ableton 風拡張 #073 / Phase 102) の offscreen pixel verify。
//!
//! UiHost::no_redraw で level_meter の Scene を組み、 OffscreenRenderer で PNG 化して
//! 実際のレイアウト (バー / 目盛り / 数値ピーク) を目視確認する。
//! 実行: `cargo run --bin meter_snapshot` → `<workspace>/target/meter_snapshot.png`。

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_core::{FrameInput, LevelMeterStyle, MeterBallistic, MeterScale, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{OffscreenRenderer, Rect, Scene};

fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 260;
    let height: u32 = 320;

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = daw_ui_renderer::theme::PANEL.to_wgpu();
    let screen = PhysicalSize { width, height };

    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput::default(),
        |(), ui| {
            // (1) ステレオ scale + peak_readout。 L 低 / R 中レベルで L/R 差を可視化。
            ui.level_meter_stereo(
                "master",
                Rect { x: 40.0, y: 24.0, w: 38.0, h: 272.0 },
                0.06,
                0.35,
                MeterBallistic::Peak,
                LevelMeterStyle {
                    scale: Some(MeterScale::default()),
                    peak_readout: true,
                    ..LevelMeterStyle::default()
                },
            );
            // (2) clean bar (default): 数字なし narrow stereo。
            ui.level_meter_stereo(
                "clean",
                Rect { x: 120.0, y: 24.0, w: 12.0, h: 272.0 },
                0.5,
                0.3,
                MeterBallistic::Peak,
                LevelMeterStyle::default(),
            );
            // (3) scale のみ (readout なし) で +6 が最上端 + 高レベル。
            ui.level_meter_stereo(
                "scale_only",
                Rect { x: 160.0, y: 24.0, w: 38.0, h: 272.0 },
                0.95,
                0.7,
                MeterBallistic::Peak,
                LevelMeterStyle {
                    scale: Some(MeterScale::default()),
                    ..LevelMeterStyle::default()
                },
            );
        },
    );

    let mut renderer = OffscreenRenderer::new(width, height)?;
    let rgba = renderer.render_to_rgba(&scene)?;

    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("meter_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("meter snapshot saved to {}", out_path.display());
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
