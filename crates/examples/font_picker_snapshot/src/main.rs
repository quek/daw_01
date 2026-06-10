//! M14 Phase 121 (daw_01 #096) の visual + pixel verify:
//! `available_font_families()` で列挙したフォントを `GlyphArea.font_family` に渡し、
//! **同一フレームに同じ文字列を別フォントで 3 行**焼いて、 per-area フォント指定が効くこと、
//! および font が cache key に入って collision しないことを確認する。
//!
//! 旧状態 (Phase 121 前) では GlyphArea に font_family が無く、 全テキストが
//! `DEFAULT_FONT_FAMILY` 固定だった。 この example は:
//!   1. font A 行 と font B 行 の pixel band が **異なる** ことを assert (= font が実際に効く)。
//!   2. PNG を出力して目視できるようにする。
//!
//! 実行: `cargo run --bin font_picker_snapshot`
//!   → `<workspace>/target/font_picker_snapshot.png` + stdout に使用フォント / 差分 pixel 数。
#![allow(clippy::cast_precision_loss)]

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_renderer::{available_font_families, Color, GlyphArea, OffscreenRenderer, Scene};

/// 視覚的に大きく異なる候補 (serif / sans / mono / casual)。 environment に在るものを優先採用。
const PREFERRED: &[&str] = &[
    "Arial",
    "Times New Roman",
    "Comic Sans MS",
    "Georgia",
    "Courier New",
    "Consolas",
    "Segoe UI",
    "Verdana",
];

const SAMPLE: &str = "Hamburgefonstiv 0123";
const WIDTH: u32 = 720;
const HEIGHT: u32 = 300;
const FONT_SIZE: f32 = 44.0;
const LINE_HEIGHT: f32 = 52.0;
const ROW_TOP: [f32; 3] = [30.0, 120.0, 210.0];

fn main() -> Result<(), Box<dyn Error>> {
    let families = available_font_families();
    println!("available_font_families(): {} 件", families.len());
    assert!(!families.is_empty(), "system fonts が列挙できない");

    // PREFERRED のうち存在する 2 つを採用。 足りなければ list の端から 2 つ。
    let chosen = pick_two(&families);
    println!("row1 = default (None / DEFAULT_FONT_FAMILY)");
    println!("row2 = {:?}", chosen.0);
    println!("row3 = {:?}", chosen.1);

    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.08, 0.09, 0.11).to_wgpu();

    let fonts: [Option<&str>; 3] = [None, Some(&chosen.0), Some(&chosen.1)];
    for (i, font) in fonts.iter().enumerate() {
        scene.push_text(GlyphArea {
            text: SAMPLE.into(),
            left: 16.0,
            top: ROW_TOP[i],
            font_size: FONT_SIZE,
            line_height: LINE_HEIGHT,
            color: Color::rgb(0.96, 0.97, 1.0),
            font_family: font.map(std::convert::Into::into),
            ..GlyphArea::default()
        });
    }

    let mut r = OffscreenRenderer::new(WIDTH, HEIGHT)?;
    let rgba = r.render_to_rgba(&scene)?;

    // PNG を **先に** 出力 (assert で落ちても目視できるように)。
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("font_picker_snapshot.png");
    save_png(&out_path, &rgba, WIDTH, HEIGHT)?;
    println!("font picker snapshot saved to {}", out_path.display());

    // --- pixel verify ---
    // 各行が非空 (文字が描かれている = 背景でない pixel が存在) であること。
    for (i, top) in ROW_TOP.iter().enumerate() {
        let ink = count_ink(&rgba, WIDTH, *top, FONT_SIZE);
        println!("row{} ink pixels = {ink}", i + 1);
        assert!(ink > 50, "row{} に文字が描かれていない (ink={ink})", i + 1);
    }

    // row2 (font A) と row3 (font B) の band が **異なる** こと (= per-area font が効いている &
    // 同 text+size でも cache collision していない)。
    let diff = band_diff(&rgba, WIDTH, ROW_TOP[1], ROW_TOP[2], FONT_SIZE);
    let band_px = (WIDTH as usize) * (FONT_SIZE as usize);
    let pct = 100.0 * diff as f64 / band_px as f64;
    println!("row2 vs row3 differing pixels = {diff} / {band_px} ({pct:.1}%)");
    assert!(
        diff > band_px / 50, // >2%
        "font A と font B の描画が殆ど同一 ({pct:.1}%)。 per-area font が効いていない / cache collision の疑い"
    );

    println!("OK: per-area font_family が効き、 font A != font B で描画された");
    Ok(())
}

/// PREFERRED から存在する distinct な 2 family を選ぶ。 足りなければ list 両端で代替。
fn pick_two(families: &[String]) -> (String, String) {
    let present: Vec<&String> = PREFERRED
        .iter()
        .filter_map(|p| families.iter().find(|f| f.as_str() == *p))
        .collect();
    if present.len() >= 2 {
        return (present[0].clone(), present[1].clone());
    }
    // fallback: 確実に異なる 2 つ (ソート済みなので先頭と末尾)。
    let first = families.first().cloned().unwrap_or_default();
    let last = families.last().cloned().unwrap_or_default();
    (first, last)
}

/// `(x,y)` の RGBA を取り出す。
fn px(bytes: &[u8], width: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let i = ((y * width + x) * 4) as usize;
    (bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3])
}

/// 背景 (暗い clear color) と十分異なる = 文字 ink とみなせる pixel 数。
fn count_ink(bytes: &[u8], width: u32, top: f32, height: f32) -> usize {
    let y0 = top as u32;
    let y1 = (top + height) as u32;
    let mut n = 0;
    for y in y0..y1 {
        for x in 0..width {
            let (r, g, b, _) = px(bytes, width, x, y);
            // 背景 gray (sRGB 変換後 ≈ (88,93,103) → sum ≈ 284) と白文字 (≈ 747) を分離する閾値。
            if i32::from(r) + i32::from(g) + i32::from(b) > 500 {
                n += 1;
            }
        }
    }
    n
}

/// 2 つの行 band (同じ x 範囲、 同じ高さ) で異なる pixel の数。
fn band_diff(bytes: &[u8], width: u32, top_a: f32, top_b: f32, height: f32) -> usize {
    let ya = top_a as u32;
    let yb = top_b as u32;
    let h = height as u32;
    let mut n = 0;
    for dy in 0..h {
        for x in 0..width {
            if px(bytes, width, x, ya + dy) != px(bytes, width, x, yb + dy) {
                n += 1;
            }
        }
    }
    n
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
