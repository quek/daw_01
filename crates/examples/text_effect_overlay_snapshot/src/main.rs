//! daw_01 #084 (Phase 112) の visual verify: effect 付き text overlay を **同一フレームに 2 枚**
//! 焼いて、 それぞれ **固有の文字列** で描画されることを目視確認する (旧 bug は両方が「最後に
//! prepare された 1 枚」 の文字列に化けた)。 daw_01 実機 (20260512.daw beat 0..4) の症状を再現:
//! 上にクレジット、 下に歌詞を別サイズ・別色・shadow blur 付きで重ねる。
//!
//! 実行: `cargo run --bin text_effect_overlay_snapshot`
//!   → `<workspace>/target/text_effect_overlay_snapshot.png`。
#![allow(clippy::cast_precision_loss)]

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_renderer::{Color, GlyphArea, OffscreenRenderer, Scene};

fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 760;
    let height: u32 = 220;

    let mut r = OffscreenRenderer::new(width, height)?;

    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.08, 0.09, 0.11).to_wgpu();

    // クレジット (上): 白文字 + 黒 outline + 半透明 shadow blur。 幅広・小さめ。
    scene.push_text(GlyphArea {
        text: "ボーカル VOICEVOX:中国うさぎ".into(),
        left: 24.0,
        top: 26.0,
        font_size: 24.0,
        line_height: 30.0,
        color: Color::rgb(0.96, 0.97, 1.0),
        clip_rect: None,
        outline_color: Color::rgb(0.0, 0.0, 0.0),
        outline_width_px: 2.0,
        shadow_color: Color::rgba(0.0, 0.0, 0.0, 0.6),
        shadow_offset_px: (2.0, 2.0),
        shadow_blur_px: 3.0,
        rotation_radians: 0.0,
    });

    // 歌詞 (下): 黄文字 + 黒 outline + shadow blur。 幅狭・大きめ (composite size が上と異なる)。
    scene.push_text(GlyphArea {
        text: "茜咲く庭".into(),
        left: 24.0,
        top: 110.0,
        font_size: 48.0,
        line_height: 56.0,
        color: Color::rgb(1.0, 0.85, 0.30),
        clip_rect: None,
        outline_color: Color::rgb(0.0, 0.0, 0.0),
        outline_width_px: 2.5,
        shadow_color: Color::rgba(0.0, 0.0, 0.0, 0.6),
        shadow_offset_px: (2.0, 2.0),
        shadow_blur_px: 4.0,
        rotation_radians: 0.0,
    });

    let rgba = r.render_to_rgba(&scene)?;
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("text_effect_overlay_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("text effect overlay snapshot saved to {}", out_path.display());
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
