//! `channel_fader_meter` の Bitwig 流 modulation (daw_01 #110) の offscreen pixel verify。
//!
//! `UiHost::no_redraw` で modulation 付き channel_fader_meter strip を数パターン描き、 OffscreenRenderer
//! で PNG 化して 縦トラックの色帯 / 可動水平 live マーク / depth-edit 枠強調 + 編集帯 が dB 目盛り・
//! メーター・peak 表示と共存することを目視確認する。 modulation の値は dB でなく **フェーダーの正規化
//! トラック位置 0..=1** (= つまみの 0=最下端〜1=最上端) で渡す。
//! 実行: `cargo run --bin fader_modulation_snapshot` → `<workspace>/target/fader_modulation_snapshot.png`。

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_core::{
    Edit, FrameInput, LevelMeterStyle, MeterBallistic, MeterScale, ModEdit, ModEntry, Modulation,
    UiHost,
};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, OffscreenRenderer, Rect, Scene};

const GROUP_W: f32 = 55.0;
const FADER_W: f32 = 18.0;

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 320;
    let height: u32 = 320;

    let cyan = Color::rgb(0.20, 0.80, 1.0);
    let orange = Color::rgb(1.0, 0.55, 0.18);
    let green = Color::rgb(0.35, 0.85, 0.40);

    let strip_y = 16.0_f32;
    let strip_h = 280.0_f32;

    let mut host: UiHost<()> = UiHost::no_redraw();
    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.16, 0.17, 0.20).to_wgpu();
    let screen = PhysicalSize { width, height };

    let style = || LevelMeterStyle {
        scale: Some(MeterScale::default()),
        peak_readout: true,
        ..LevelMeterStyle::default()
    };
    let noop = |_db: f32| Edit::mutate(|(): &mut ()| {});
    let on_mod = |_d: f64| Edit::mutate(|(): &mut ()| {});
    let step = GROUP_W + 14.0;
    let x0 = 16.0_f32;

    host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
        let rect = |i: usize| Rect { x: x0 + i as f32 * step, y: strip_y, w: GROUP_W, h: strip_h };

        // (1) 2 source の色帯 (cyan +0.25 / orange -0.15 frac) + live 水平マーク。
        let e1 = [
            ModEntry { color: cyan, depth: 0.25 },
            ModEntry { color: orange, depth: -0.15 },
        ];
        ui.channel_fader_meter(
            ("strip", 0usize),
            rect(0),
            FADER_W,
            0.0, // 0dB
            0.0,
            0.5,
            0.45,
            MeterBallistic::Peak,
            style(),
            "Volume",
            noop,
            Some(Modulation { entries: &e1, live_value: Some(0.65), edit: None }),
        );

        // (2) depth-edit (arm) 中: green 枠強調 + 編集帯 + live マーク。
        let e2 = [ModEntry { color: green, depth: 0.30 }];
        ui.channel_fader_meter(
            ("strip", 1usize),
            rect(1),
            FADER_W,
            -6.0,
            0.0,
            0.3,
            0.28,
            MeterBallistic::Peak,
            style(),
            "Volume",
            noop,
            Some(Modulation {
                entries: &e2,
                live_value: Some(0.55),
                edit: Some(ModEdit {
                    source_color: green,
                    current_depth: 0.30,
                    depth_range: Some((-1.0, 1.0)),
                    depth_sensitivity: None,
                    on_mod_change: &on_mod,
                }),
            }),
        );

        // (3) 負 depth のみ (base 位置から下へ伸びる帯)。
        let e3 = [ModEntry { color: orange, depth: -0.35 }];
        ui.channel_fader_meter(
            ("strip", 2usize),
            rect(2),
            FADER_W,
            3.0,
            0.0,
            0.6,
            0.55,
            MeterBallistic::Peak,
            style(),
            "Volume",
            noop,
            Some(Modulation { entries: &e3, live_value: None, edit: None }),
        );

        // (4) modulation None (= 完全回帰、 帯なし)。
        ui.channel_fader_meter(
            ("strip", 3usize),
            rect(3),
            FADER_W,
            -12.0,
            0.0,
            0.2,
            0.22,
            MeterBallistic::Peak,
            style(),
            "Volume",
            noop,
            None,
        );
    });

    let mut renderer = OffscreenRenderer::new(width, height)?;
    let rgba = renderer.render_to_rgba(&scene)?;

    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("fader_modulation_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("fader modulation snapshot saved to {}", out_path.display());
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
