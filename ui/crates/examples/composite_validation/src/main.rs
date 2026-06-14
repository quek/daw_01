//! M14 Phase 93 (daw_01 #063) + Phase 92 (#064) の視覚検証 example。
//!
//! 「立ち絵 group transform」 の中核フローを最小再現する:
//!   1. 子 quad 群 (4 象限カラー = group の子パーツに見立てる) を 1 枚の **GPU 常駐 sampleable
//!      texture** に `composite_scene_to_texture` で合成する (#063)。
//!   2. 合成済 1 枚に親 affine の回転を **任意 pivot** でかけて `TexturedQuad` として描く (#064)。
//!
//! 4 象限 (TL=red / TR=green / BL=blue / BR=yellow) なので、 回転後どの角がどこへ行ったか・
//! どこを軸に回ったかが一目で分かる。 3 通り並べて比較する:
//!   (a) 回転なし          — composite がそのまま再 sample できる
//!   (b) 35° / pivot=中心   — 既存 (#047) と同じ中心 pivot
//!   (c) 35° / pivot=左上角 — #064。 元 footprint の左上を軸に回る (= 親アンカー)
//!
//! 実行: `cargo run --bin composite_validation` → `<workspace>/target/composite_validation.png`。

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use daw_ui_renderer::{
    Color, GlyphArea, OffscreenRenderer, Rect, RectCommand, Scene, TextureHandle, TexturedQuad,
};

const TILE: f32 = 60.0; // 各象限の 1 辺
const SPRITE: u32 = (TILE as u32) * 2; // composite 先 texture の 1 辺 (120)

fn main() -> Result<(), Box<dyn Error>> {
    let width: u32 = 720;
    let height: u32 = 320;
    let mut renderer = OffscreenRenderer::new(width, height)?;

    // ── 1. 子 quad 群 (group の子パーツ) を 1 枚に composite (#063) ──
    // 4 象限を別々の rect として push = 「複数の子を z 順に 1 枚へ合成」 の最小形。
    let sprite = composite_quadrant_sprite(&mut renderer);

    // ── 2. base scene に 3 通りで再描画 (#064) ──
    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.10, 0.11, 0.13).to_wgpu();

    scene.push_text(text(
        20.0,
        18.0,
        16.0,
        Color::WHITE,
        "Phase 93 (#063) composite-to-texture + Phase 92 (#064) rotation_pivot",
    ));

    let theta = 35.0_f32.to_radians();
    let top = 70.0;
    // (a) 回転なし
    draw_case(&mut scene, sprite, 40.0, top, 0.0, None, "(a) composite (no rotation)");
    // (b) 中心 pivot (None)
    draw_case(&mut scene, sprite, 280.0, top, theta, None, "(b) 35deg  pivot = center");
    // (c) 左上角 pivot
    draw_case(
        &mut scene,
        sprite,
        520.0,
        top,
        theta,
        Some((0.0, 0.0)),
        "(c) 35deg  pivot = top-left",
    );

    // ── 3. render → PNG + pixel verify ──
    let rgba = renderer.render_to_rgba(&scene)?;
    assert_eq!(rgba.len(), (width as usize) * (height as usize) * 4);

    // sRGB target なので象限色 (linear 0.20 等) は byte 124 程度に乗る。 絶対値でなく
    // 「r が g/b を有意に上回る」 相対優位で red を判定する。
    let red_ish = |p: (u8, u8, u8, u8)| {
        p.0 > 170 && i32::from(p.0) - i32::from(p.1) > 55 && i32::from(p.0) - i32::from(p.2) > 55
    };
    // (a) 無回転: 左上象限 (40,70)..(100,130) は red のはず。
    let a_tl = px(&rgba, width, 70, 100);
    assert!(red_ish(a_tl), "(a) 左上象限が red でない: {a_tl:?}");
    // (c) 左上角 pivot: 元 footprint 左上角の少し内側 (526,76) は回転後も red quadrant のまま
    // (= 左上角を軸に red が固定)。
    let c_anchor = px(&rgba, width, 526, 76);
    assert!(
        red_ish(c_anchor),
        "(c) 左上角 pivot なのに anchor 付近 (526,76) が red で固定されていない: {c_anchor:?}"
    );
    // (b) 中心 pivot: 同じ相対位置 (286,76) は中心回転で footprint 外に出る (= base bg、 red でない)。
    let b_anchor = px(&rgba, width, 286, 76);
    assert!(
        !red_ish(b_anchor),
        "(b) 中心 pivot なのに左上角付近が red のまま (pivot が効いていない?): {b_anchor:?}"
    );

    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("composite_validation.png");
    save_png(&out_path, &rgba, width, height)?;
    println!("composite validation snapshot saved to {}", out_path.display());
    Ok(())
}

/// 4 象限カラーの子 quad 群を `SPRITE × SPRITE` の GPU 常駐 texture に composite して返す (#063)。
fn composite_quadrant_sprite(renderer: &mut OffscreenRenderer) -> TextureHandle {
    let mut child = Scene::new();
    // clear_color は composite では無視され常に透明 clear (#063)。 余白は透明のまま残る。
    let quad = |x: f32, y: f32, c: Color| RectCommand::uniform_radius(Rect::new(x, y, TILE, TILE), c, 0.0);
    child.push_rect(quad(0.0, 0.0, Color::rgb(0.90, 0.20, 0.20))); // TL red
    child.push_rect(quad(TILE, 0.0, Color::rgb(0.25, 0.80, 0.30))); // TR green
    child.push_rect(quad(0.0, TILE, Color::rgb(0.25, 0.45, 0.95))); // BL blue
    child.push_rect(quad(TILE, TILE, Color::rgb(0.95, 0.85, 0.25))); // BR yellow
    renderer
        .composite_scene_to_texture(&child, SPRITE, SPRITE)
        .expect("composite quadrant sprite")
}

/// 合成済 sprite を `(x, y)` に `theta` / `pivot` で描く。 元 footprint を薄い枠で示し、
/// pivot 点を白ドットで示すことで「どこを軸に回ったか」 を可視化する。
fn draw_case(
    scene: &mut Scene,
    sprite: TextureHandle,
    x: f32,
    y: f32,
    theta: f32,
    pivot: Option<(f32, f32)>,
    label: &str,
) {
    let side = SPRITE as f32;
    // 元 (無回転) footprint の薄い枠。
    scene.push_rect(RectCommand {
        rect: Rect::new(x, y, side, side),
        fill: Color::TRANSPARENT,
        border: Color::rgba(1.0, 1.0, 1.0, 0.30),
        border_width: 1.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    // 合成済 sprite を回転付きで描く (#064)。
    scene.push_textured_quad(TexturedQuad {
        rect: Rect::new(x, y, side, side),
        texture: sprite,
        alpha: 1.0,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        clip_rect: None,
        rotation_radians: theta,
        rotation_pivot: pivot,
    });
    // pivot 点 (白ドット 6x6)。 None = 中心。
    let (pvx, pvy) = pivot.unwrap_or((side * 0.5, side * 0.5));
    scene.push_rect(RectCommand::uniform_radius(
        Rect::new(x + pvx - 3.0, y + pvy - 3.0, 6.0, 6.0),
        Color::WHITE,
        3.0,
    ));
    // 角 pivot 回転は footprint より下へ膨らむので、 label は十分下げて被りを避ける。
    scene.push_text(text(x, y + side + 52.0, 11.0, Color::rgb(0.75, 0.78, 0.85), label));
}

fn text(left: f32, top: f32, size: f32, color: Color, s: &str) -> GlyphArea {
    GlyphArea {
        text: s.into(),
        left,
        top,
        font_size: size,
        line_height: size + 4.0,
        color,
        clip_rect: None,
        ..GlyphArea::default()
    }
}

fn px(bytes: &[u8], width: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let i = ((y * width + x) * 4) as usize;
    (bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3])
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
