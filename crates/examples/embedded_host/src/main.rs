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
    });
    scene.push_text(GlyphArea {
        text: "Phase 71 (#043): texture pipeline (4x4 checker + 0.5 blue overlay)".into(),
        left: width as f32 - 540.0,
        top: height as f32 - 30.0,
        font_size: 12.0,
        line_height: 14.0,
        color: Color::rgb(0.7, 0.85, 1.0),
        clip_rect: None,
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
