//! Phase 18 Example — `OffscreenRenderer` でプラグイン UI 埋め込み環境を模擬。
//!
//! - `EmbeddedHostWindow` は **外部 crate での `WindowBackend` 自前実装** の例。
//!   実 DAW プラグイン環境では親プロセスから受け取った `RawWindowHandle` を保持する
//!   型になるが、本 example では runtime には使わず、compile-time に
//!   `EmbeddedHostWindow: WindowBackend + Send + Sync + 'static` を確認するだけ
//!   (= 「raw-window-handle 受け渡し API は trait bound として既に公開されている」
//!   ことの実証)。
//! - `main` は [`OffscreenRenderer`] で window なしで 1 フレームを RGBA bytes に
//!   render し、`<workspace>/target/embedded_host_snapshot.png` に保存する。

use std::error::Error;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};

use daw_ui_platform::{CursorIcon, PhysicalSize, WindowBackend};
use daw_ui_renderer::{Color, GlyphArea, OffscreenRenderer, Rect, RectCommand, Scene, TexturedQuad};

/// 自前 `WindowBackend` 実装の例。実 DAW プラグイン環境では親プロセスから受け取った
/// `RawWindowHandle` を保持する型になり、`window_handle` で
/// `unsafe { WindowHandle::borrow_raw(self.raw_handle) }` を返す。
///
/// 本 example では `OffscreenRenderer` が handle を不要にするため、
/// `window_handle` / `display_handle` は `Err(HandleError::NotSupported)` の dummy 実装。
struct EmbeddedHostWindow {
    size: PhysicalSize,
}

impl HasWindowHandle for EmbeddedHostWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Err(HandleError::NotSupported)
    }
}

impl HasDisplayHandle for EmbeddedHostWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Err(HandleError::NotSupported)
    }
}

impl WindowBackend for EmbeddedHostWindow {
    fn inner_size(&self) -> PhysicalSize {
        self.size
    }
    fn scale_factor(&self) -> f64 {
        1.0
    }
    fn request_redraw(&self) {}
    fn set_cursor(&self, _: CursorIcon) {}
    fn set_ime_allowed(&self, _: bool) {}
    fn set_ime_cursor_area(&self, _: f64, _: f64, _: f64, _: f64) {}
    fn set_title(&self, _: &str) {}
}

/// compile-time に `EmbeddedHostWindow: WindowBackend + Send + Sync + 'static` を確認。
/// 関数本体は呼ばれてもよいが副作用なし (型 assert のみ)。
fn assert_embedded_window_backend() {
    fn check<W: WindowBackend + Send + Sync + 'static>(_: &W) {}
    let w = EmbeddedHostWindow { size: PhysicalSize { width: 800, height: 600 } };
    check(&w);
}

// 各 Phase の smoke を flat に並べる demo なので helper 抽出より直線的な scene 構築の方が
// 例として読みやすい (Phase 71 RGBA / 73 BGRA / 76 rotation の 3 demo で 100 行超過)。
#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    // 外部 crate (= この example crate) でも `WindowBackend` を自前実装できることを
    // compile-time に確認 (= raw-window-handle 受け渡し API が公開されている実証)。
    assert_embedded_window_backend();

    let width: u32 = 800;
    let height: u32 = 600;
    let mut renderer = OffscreenRenderer::new(width, height)?;

    // 簡易 mixer 風 Scene を構築 (Ui を介さず Scene::push_rect 等で直接組む)。
    let mut scene = Scene::new();
    for ch in 0..8 {
        let x = 50.0 + (ch as f32) * 90.0;
        // fader trough
        scene.push_rect(RectCommand {
            rect: Rect::new(x, 60.0, 60.0, 480.0),
            fill: Color::rgb(0.15, 0.15, 0.18),
            border: Color::rgb(0.3, 0.3, 0.35),
            border_width: 1.0,
            radius: [4.0; 4],
            clip_rect: None,
        });
        // fader handle (channel ごとに位置を変えてバリエーションを出す)
        let handle_y = 200.0 + (ch as f32) * 24.0;
        scene.push_rect(RectCommand::uniform_radius(
            Rect::new(x + 10.0, handle_y, 40.0, 12.0),
            Color::rgb(0.3, 0.6, 0.9),
            2.0,
        ));
    }
    scene.push_text(GlyphArea {
        text: "daw-ui Phase 18 — Plugin UI Embedding (OffscreenRenderer)".into(),
        left: 30.0,
        top: 20.0,
        font_size: 16.0,
        line_height: 20.0,
        color: Color::WHITE,
        clip_rect: None,
        ..GlyphArea::default()
    });

    // ============================================================
    // M14 Phase 71 (daw_01 #043): texture pipeline smoke test
    // ============================================================
    //
    // 4x4 RGBA8 のチェック柄 texture を作り、 fader 群の右側に 120x120 px で拡大描画。
    // PNG snapshot 右下にチェック柄が見えれば texture pipeline は動作している。
    // 続けて crossfade を模擬: 同 rect に alpha=0.5 の単色 texture を重ねて混色確認。
    let checker_tex = renderer.create_texture(4, 4);
    let checker_rgba = make_checker_rgba();
    renderer.upload_texture_rgba(checker_tex, &checker_rgba);
    assert_eq!(renderer.texture_size(checker_tex), Some((4, 4)));

    let crossfade_tex = renderer.create_texture(2, 2);
    let solid_blue = [
        0x10, 0x60, 0xFF, 0xFF, 0x10, 0x60, 0xFF, 0xFF,
        0x10, 0x60, 0xFF, 0xFF, 0x10, 0x60, 0xFF, 0xFF,
    ];
    renderer.upload_texture_rgba(crossfade_tex, &solid_blue);

    let texture_rect = Rect::new(width as f32 - 150.0, height as f32 - 150.0, 120.0, 120.0);
    scene.push_textured_quad(TexturedQuad::new(texture_rect, checker_tex));
    scene.push_textured_quad(TexturedQuad {
        rect: texture_rect,
        texture: crossfade_tex,
        alpha: 0.5,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        clip_rect: None,
        rotation_radians: 0.0,
    });
    scene.push_text(GlyphArea {
        text: "Phase 71 (#043): texture pipeline (4x4 checker + 0.5 blue overlay)".into(),
        left: width as f32 - 540.0,
        top: height as f32 - 30.0,
        font_size: 12.0,
        line_height: 14.0,
        color: Color::rgb(0.7, 0.85, 1.0),
        clip_rect: None,
        ..GlyphArea::default()
    });

    // ============================================================
    // M14 Phase 73 (daw_01 #045): BGRA8UnormSrgb texture smoke test
    // ============================================================
    //
    // RGBA checker (上で作成) と **同じ色** を BGRA bytes (= channel swap して) で渡す。
    // BGRA path が正しく実装されていれば widget 側で再 swap が起きないので、 PNG snapshot 上で
    // BGRA tile が RGBA tile と完全同色になる (= channel swap が widget 側で起きない実証)。
    // 失敗例: RGBA path 経由で誤って BGRA bytes を upload した場合、 red と blue が入れ替わる。
    let bgra_tex = renderer.create_texture_bgra(4, 4);
    let bgra_bytes = make_checker_bgra();
    renderer.upload_texture_bgra(bgra_tex, &bgra_bytes);
    // format 確認は `Renderer::texture_format` で可能だが、 wgpu 直接依存を増やさないため
    // embedded_host では skip。 visual snapshot で「BGRA tile と RGBA tile が同色」 を見れば
    // BGRA path 正常動作が分かる。
    let bgra_rect = Rect::new(width as f32 - 290.0, height as f32 - 150.0, 120.0, 120.0);
    scene.push_textured_quad(TexturedQuad::new(bgra_rect, bgra_tex));
    scene.push_text(GlyphArea {
        text: "Phase 73 (#045): BGRA path (中) ↔ Phase 71 RGBA path (右、 同色のはず)".into(),
        left: width as f32 - 540.0,
        top: height as f32 - 165.0,
        font_size: 12.0,
        line_height: 14.0,
        color: Color::rgb(0.85, 0.95, 0.7),
        clip_rect: None,
        ..GlyphArea::default()
    });

    // ============================================================
    // M14 Phase 76 (daw_01 #047): rotated TexturedQuad smoke test
    // ============================================================
    //
    // 同じ checker texture を 30° clockwise 回転して描画。 PNG snapshot 上で:
    //  - rect 中心が回転前と一致 (= pivot は rect 中心)
    //  - 4 角が axis-aligned rect の外に膨らみ AABB が拡大している (= 回転が pixel 空間で実施)
    //  - texture の red/green/blue/yellow tile が rect 4 隅に "stuck" して一緒に回る
    //    (= UV mapping は un-rotated corner で計算)
    // が確認できれば pipeline が期待通り動作している。
    let rotated_rect = Rect::new(width as f32 - 430.0, height as f32 - 150.0, 120.0, 120.0);
    scene.push_textured_quad(TexturedQuad {
        rect: rotated_rect,
        texture: checker_tex,
        alpha: 1.0,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        clip_rect: None,
        rotation_radians: std::f32::consts::FRAC_PI_6, // 30°
    });
    scene.push_text(GlyphArea {
        text: "Phase 76 (#047): rotation π/6 (30° clockwise, pivot=rect center)".into(),
        left: width as f32 - 540.0,
        top: height as f32 - 300.0,
        font_size: 12.0,
        line_height: 14.0,
        color: Color::rgb(0.95, 0.85, 0.6),
        clip_rect: None,
        ..GlyphArea::default()
    });

    // ============================================================
    // M14 Phase 78 (daw_01 #049): text effect smoke (outline / shadow / blur / rotation)
    // ============================================================
    //
    // 4 種の text effect を縦に並べて PNG snapshot で目視確認:
    //  (a) effect なし baseline (= byte 完全互換確認)
    //  (b) outline 2px (= 黒い縁取り)
    //  (c) shadow offset(4,4) blur=0 (= hard shadow)
    //  (d) shadow offset(0,0) blur=8 (= soft blur shadow)
    //  (e) rotation π/6 + outline + soft shadow (= 全 effect combine)
    let effect_x = 20.0_f32;
    let mut effect_y = 350.0_f32;
    let effect_font = 24.0_f32;
    let effect_line_h = 30.0_f32;
    let label = |y, text: &str| GlyphArea {
        text: text.into(),
        left: effect_x,
        top: y,
        font_size: 10.0,
        line_height: 12.0,
        color: Color::rgb(0.6, 0.6, 0.7),
        clip_rect: None,
        ..GlyphArea::default()
    };
    // (a) baseline (effect なし) — Phase 71 と同じ既存 path で描画される (has_effects = false)
    scene.push_text(label(effect_y, "(a) baseline:"));
    scene.push_text(GlyphArea {
        text: "Hello".into(),
        left: effect_x + 100.0,
        top: effect_y - 5.0,
        font_size: effect_font,
        line_height: effect_line_h,
        color: Color::WHITE,
        clip_rect: None,
        ..GlyphArea::default()
    });
    effect_y += 36.0;
    // (b) outline 2px (= 黒い縁取り)
    scene.push_text(label(effect_y, "(b) outline 2px:"));
    scene.push_text(GlyphArea {
        text: "Hello".into(),
        left: effect_x + 100.0,
        top: effect_y - 5.0,
        font_size: effect_font,
        line_height: effect_line_h,
        color: Color::WHITE,
        clip_rect: None,
        outline_color: Color::BLACK,
        outline_width_px: 2.0,
        ..GlyphArea::default()
    });
    effect_y += 36.0;
    // (c) hard shadow (offset=(4,4) blur=0)
    scene.push_text(label(effect_y, "(c) hard shadow:"));
    scene.push_text(GlyphArea {
        text: "Hello".into(),
        left: effect_x + 100.0,
        top: effect_y - 5.0,
        font_size: effect_font,
        line_height: effect_line_h,
        color: Color::WHITE,
        clip_rect: None,
        shadow_color: Color::rgba(0.0, 0.0, 0.0, 0.5),
        shadow_offset_px: (4.0, 4.0),
        ..GlyphArea::default()
    });
    effect_y += 36.0;
    // (d) soft (blurred) shadow (= separable gaussian 5-tap @ blur=8)
    scene.push_text(label(effect_y, "(d) soft shadow blur=8:"));
    scene.push_text(GlyphArea {
        text: "Hello".into(),
        left: effect_x + 130.0,
        top: effect_y - 5.0,
        font_size: effect_font,
        line_height: effect_line_h,
        color: Color::WHITE,
        clip_rect: None,
        shadow_color: Color::rgba(0.0, 0.0, 0.0, 0.7),
        shadow_offset_px: (0.0, 0.0),
        shadow_blur_px: 8.0,
        ..GlyphArea::default()
    });
    effect_y += 36.0;
    // (e) combine: rotation + outline + soft shadow
    scene.push_text(label(effect_y, "(e) rot π/6 + outline + shadow:"));
    scene.push_text(GlyphArea {
        text: "Hello".into(),
        left: effect_x + 160.0,
        top: effect_y - 5.0,
        font_size: effect_font,
        line_height: effect_line_h,
        color: Color::WHITE,
        clip_rect: None,
        outline_color: Color::BLACK,
        outline_width_px: 2.0,
        shadow_color: Color::rgba(0.0, 0.0, 0.0, 0.5),
        shadow_offset_px: (3.0, 3.0),
        shadow_blur_px: 4.0,
        rotation_radians: std::f32::consts::FRAC_PI_6,
    });

    // 1 フレーム render → RGBA bytes (sRGB encoded、行 stride = width * 4)
    let rgba = renderer.render_to_rgba(&scene)?;
    assert_eq!(rgba.len(), (width as usize) * (height as usize) * 4);

    // PNG として workspace の target/ に保存
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target");
    fs::create_dir_all(&target_dir)?;
    let out_path = target_dir.join("embedded_host_snapshot.png");
    save_png(&out_path, &rgba, width, height)?;

    println!("snapshot saved to {}", out_path.display());
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

/// 4x4 の RGBA8 チェック柄 (red / green / blue / yellow を 2x2 タイル状に並べる)。
/// linear filter で拡大すると色の補間が見える = pipeline 動作 + sampler 動作の確認。
fn make_checker_rgba() -> Vec<u8> {
    let colors: [[u8; 4]; 4] = [
        [0xE0, 0x40, 0x40, 0xFF], // red
        [0x40, 0xE0, 0x40, 0xFF], // green
        [0x40, 0x40, 0xE0, 0xFF], // blue
        [0xE0, 0xE0, 0x40, 0xFF], // yellow
    ];
    let mut out = Vec::with_capacity(4 * 4 * 4);
    for y in 0..4 {
        for x in 0..4 {
            let tile = ((y / 2) * 2 + (x / 2)) as usize;
            out.extend_from_slice(&colors[tile]);
        }
    }
    out
}

/// M14 Phase 73 (daw_01 #045): 4x4 の BGRA8 チェック柄。 `make_checker_rgba` と **同じ色**
/// (red / green / blue / yellow) を表現するが、 各 pixel の byte 順を B G R A に swap。
/// = PNG snapshot 上で RGBA tile と完全同色になることが BGRA path 正常動作の証明。
fn make_checker_bgra() -> Vec<u8> {
    // RGBA から B G R A 順への swap (= 同じ色を BGRA で表現)。
    let colors: [[u8; 4]; 4] = [
        [0x40, 0x40, 0xE0, 0xFF], // red    (B=40, G=40, R=E0)
        [0x40, 0xE0, 0x40, 0xFF], // green  (B=40, G=E0, R=40)
        [0xE0, 0x40, 0x40, 0xFF], // blue   (B=E0, G=40, R=40)
        [0x40, 0xE0, 0xE0, 0xFF], // yellow (B=40, G=E0, R=E0)
    ];
    let mut out = Vec::with_capacity(4 * 4 * 4);
    for y in 0..4 {
        for x in 0..4 {
            let tile = ((y / 2) * 2 + (x / 2)) as usize;
            out.extend_from_slice(&colors[tile]);
        }
    }
    out
}
