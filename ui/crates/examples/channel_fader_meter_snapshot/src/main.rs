//! `channel_fader_meter` (M14 Phase 111 / daw_01 #083) の offscreen pixel verify。
//!
//! UiHost::no_redraw で複数の channel_fader_meter strip を組み、 OffscreenRenderer で PNG 化する。
//! **検証ポイント**: fader thumb 中心と meter の 0dB 横線が同一の dB→y 写像から配置されること。
//! 全 strip を横切る magenta の参照ガイドを「0dB の y」 に引いてあるので、 0dB strip では
//! thumb / meter 0dB 線 / magenta ガイドの 3 本が同じ高さに、 +6dB strip では thumb がガイドより
//! 上、 -6 / -24 / -inf strip では thumb がガイドより下に来るのが一目で確認できる
//! (旧 fader_at + level_meter_stereo 別置きでは thumb と 0dB 線が ~13px ズレた)。
//!
//! 実行: `cargo run --bin channel_fader_meter_snapshot` → `<workspace>/target/channel_fader_meter_snapshot.png`。

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_core::{
    FrameInput, LevelMeterStyle, MeterBallistic, MeterScale, UiHost,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, OffscreenRenderer, Rect, RectCommand, Scene};

/// level_meter.rs の内部定数のミラー (参照ガイドの 0dB y を計算するためだけに使う)。
/// READOUT_BAND_H = READOUT_H(13) + 3、 SCALE_VPAD = 6。
const READOUT_BAND_H: f32 = 16.0;
const SCALE_VPAD: f32 = 6.0;

const GROUP_W: f32 = 55.0;
const FADER_W: f32 = 18.0;

fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 360;
    let height: u32 = 320;

    let strip_y = 16.0;
    let strip_h = 280.0;

    // 参照ガイドの 0dB y (全 strip 共通の dB→y 写像)。
    let region_y = strip_y + READOUT_BAND_H + SCALE_VPAD;
    let region_h = strip_h - READOUT_BAND_H - 2.0 * SCALE_VPAD;
    let guide_y = region_y + region_h * (1.0 - MeterScale::default().db_to_frac(0.0));

    // (volume_db, L, R) の strip 群。-inf / -24 / -6 / 0 / +6 dB。
    let strips = [
        (f32::NEG_INFINITY, 0.95f32, 0.9f32), // 無音 fader + clip 級メーター
        (-24.0, 0.12, 0.18),
        (-6.0, 0.5, 0.45),
        (0.0, 0.7, 0.62), // ← thumb が 0dB 線とガイドに一致するはずの strip
        (6.0, 0.3, 0.35),
    ];

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.16, 0.17, 0.20).to_wgpu();
    let screen = PhysicalSize { width, height };

    host.frame_to_edits(
        &(),
        &mut scene,
        screen,
        FrameInput::default(),
        |(), ui| {
            for (i, &(vol_db, l, r)) in strips.iter().enumerate() {
                let x = 16.0 + i as f32 * (GROUP_W + 14.0);
                ui.channel_fader_meter(
                    ("strip", i),
                    Rect { x, y: strip_y, w: GROUP_W, h: strip_h },
                    FADER_W,
                    vol_db,
                    0.0,
                    l,
                    r,
                    MeterBallistic::Peak,
                    LevelMeterStyle {
                        scale: Some(MeterScale::default()),
                        peak_readout: true,
                        ..LevelMeterStyle::default()
                    },
                    "Volume",
                    |_new_db| daw_ui_core::Edit::mutate(|()| {}),
                    None,
                );
            }

            // 0dB 参照ガイド (全幅 magenta 2px)。thumb / 0dB 線がこの高さに乗るかを目視確認する。
            ui.push_rect(RectCommand {
                rect: Rect { x: 0.0, y: guide_y - 1.0, w: width as f32, h: 2.0 },
                fill: Color::rgb(1.0, 0.0, 0.85),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        },
    );

    let mut renderer = OffscreenRenderer::new(width, height)?;
    let rgba = renderer.render_to_rgba(&scene)?;

    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("channel_fader_meter_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("channel_fader_meter snapshot saved to {}", out_path.display());
    println!("0dB reference guide at y = {guide_y:.2}");
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
