//! M14 Phase 122 (daw_01 #097) の visual + pixel verify: `GlyphArea` の box 内アライメント。
//!
//! daw_01 はこれまで `approx_text_w = font_size * 文字数 * 0.55` で水平中央を自前計算しており、
//! 全角 CJK (実幅 ≈ 1.0 em) を大幅に過小評価して日本語タイトルの center がずれていた。 本 example は
//! `box_width` + `align_h/align_v` を使い、 renderer が **実測 advance** で配置することを確認する:
//!
//!   1. 同一 CJK 文字列を Left / Center / Right で並べ、 各 box (幅 600 / 中心 x=450) 内に配置。
//!   2. center は effect 無し (plain glyph path) と outline 付き (offscreen effect path) の両方。
//!   3. 出力 PNG を Read して目視 + **ink (= 明るい text pixel) の水平範囲を scan して**
//!      center 行の ink 中心が box 中心 ±tol、 left 行が box 左端、 right 行が box 右端に一致する
//!      ことを assert (旧 0.55 推定なら center が右へずれて fail する)。
//!
//! 実行: `cargo run --bin text_align_snapshot`
//!   → `<workspace>/target/text_align_snapshot.png`。 assert に失敗すると非 0 終了。
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_renderer::{
    Color, GlyphArea, HAlign, OffscreenRenderer, Rect, RectCommand, Scene, VAlign,
};

const WIDTH: u32 = 900;
const HEIGHT: u32 = 440;

const BOX_X: f32 = 150.0;
const BOX_W: f32 = 600.0;
const BOX_H: f32 = 72.0;
const BOX_CENTER: f32 = BOX_X + BOX_W * 0.5; // 450
const BOX_RIGHT: f32 = BOX_X + BOX_W; // 750

const TITLE: &str = "ボーカルにVOICEVOX中国うさぎ";

/// 1 行分の配置記述。 `effect` true で outline 付き (offscreen effect path)。
struct Row {
    y: f32,
    text: &'static str,
    align: HAlign,
    effect: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let rows = [
        Row { y: 30.0, text: TITLE, align: HAlign::Left, effect: false },
        Row { y: 120.0, text: TITLE, align: HAlign::Center, effect: false },
        Row { y: 210.0, text: TITLE, align: HAlign::Right, effect: false },
        Row { y: 320.0, text: "茜咲く庭 Center+outline", align: HAlign::Center, effect: true },
    ];

    let mut r = OffscreenRenderer::new(WIDTH, HEIGHT)?;
    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.08, 0.09, 0.11).to_wgpu();

    // box 中心の magenta guide を最背面に (luminance ≈ 105 < ink 閾値 → scan で除外、 text は上に乗る)。
    scene.push_rect(RectCommand::uniform_radius(
        Rect::new(BOX_CENTER - 1.0, 20.0, 2.0, HEIGHT as f32 - 40.0),
        Color::rgb(0.9, 0.1, 0.9),
        0.0,
    ));
    for row in &rows {
        // box 領域を薄い暗色 fill で可視化 (luminance < ink 閾値なので scan には影響しない)。
        scene.push_rect(RectCommand::uniform_radius(
            Rect::new(BOX_X, row.y, BOX_W, BOX_H),
            Color::rgb(0.16, 0.17, 0.20),
            3.0,
        ));
        scene.push_text(text_area(row));
    }

    // 2 フレーム描画して **2 枚目 (= effect compositor cache HIT 経路)** で検証する。 effect 付き行は
    // 1 枚目が cache miss (bake)、 2 枚目が hit。 align は baked texture を変えないので cache は共有され、
    // 配置は hit 経路でも毎フレーム実測値から再計算される必要がある (miss だけ aligned で hit が
    // left/top に戻る回帰を防ぐ、 daw_01 #097 review 指摘)。
    let _ = r.render_to_rgba(&scene)?;
    let rgba = r.render_to_rgba(&scene)?;
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("text_align_snapshot.png");
    save_png(&out_path, &rgba, WIDTH, HEIGHT)?;
    println!("text align snapshot saved to {}", out_path.display());

    // ---- pixel-level assertions ----
    let mut failed = false;
    for row in &rows {
        let band = scan_ink_band(&rgba, row.y as u32 + 6, (row.y + BOX_H) as u32 - 6);
        let Some((min_x, max_x)) = band else {
            println!("  [{:?}{}] no ink found!", row.align, eff(row.effect));
            failed = true;
            continue;
        };
        let center = (min_x + max_x) as f32 * 0.5;
        let (label, ok, detail) = match row.align {
            HAlign::Center => {
                let ok = (center - BOX_CENTER).abs() <= 16.0;
                ("center", ok, format!("ink_center={center:.0} vs box_center={BOX_CENTER:.0}"))
            }
            HAlign::Left => {
                let ok = (min_x as f32 - BOX_X).abs() <= 16.0;
                ("left", ok, format!("ink_left={min_x} vs box_left={BOX_X:.0}"))
            }
            HAlign::Right => {
                let ok = (max_x as f32 - BOX_RIGHT).abs() <= 16.0;
                ("right", ok, format!("ink_right={max_x} vs box_right={BOX_RIGHT:.0}"))
            }
        };
        println!(
            "  [{label}{}] {} -> {}",
            eff(row.effect),
            detail,
            if ok { "OK" } else { "FAIL" }
        );
        failed |= !ok;
    }

    if failed {
        return Err("text align assertions failed (see log)".into());
    }
    println!("all alignment assertions passed");
    Ok(())
}

fn eff(effect: bool) -> &'static str {
    if effect { "+outline" } else { "" }
}

fn text_area(row: &Row) -> GlyphArea {
    let base = GlyphArea {
        text: row.text.into(),
        left: BOX_X,
        top: row.y,
        font_size: 26.0,
        line_height: 32.0,
        color: Color::rgb(0.97, 0.98, 1.0),
        box_width: Some(BOX_W),
        box_height: Some(BOX_H),
        align_h: row.align,
        align_v: VAlign::Center,
        ..GlyphArea::default()
    };
    if row.effect {
        GlyphArea {
            outline_color: Color::rgb(0.0, 0.0, 0.0),
            outline_width_px: 2.0,
            ..base
        }
    } else {
        base
    }
}

/// `[y0, y1)` の水平帯を scan し、 luminance が閾値超の column が存在する最小/最大 x を返す。
/// magenta guide (~105) / box fill (~44) / 背景は閾値未満なので除外され、 白系 text のみ拾う。
fn scan_ink_band(rgba: &[u8], y0: u32, y1: u32) -> Option<(u32, u32)> {
    const THRESHOLD: f32 = 150.0;
    let mut min_x: Option<u32> = None;
    let mut max_x: Option<u32> = None;
    for y in y0..y1.min(HEIGHT) {
        for x in 0..WIDTH {
            let i = ((y * WIDTH + x) * 4) as usize;
            let (rr, gg, bb) =
                (f32::from(rgba[i]), f32::from(rgba[i + 1]), f32::from(rgba[i + 2]));
            let lum = 0.299 * rr + 0.587 * gg + 0.114 * bb;
            if lum > THRESHOLD {
                min_x = Some(min_x.map_or(x, |m| m.min(x)));
                max_x = Some(max_x.map_or(x, |m| m.max(x)));
            }
        }
    }
    Some((min_x?, max_x?))
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
