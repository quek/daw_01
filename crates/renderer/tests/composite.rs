//! M14 Phase 93 (daw_01 #063) + Phase 92 (#064) の GPU pixel verify。
//!
//! `OffscreenRenderer::composite_scene_to_texture` で焼いた GPU 常駐 texture を `TexturedQuad`
//! として再描画し、 readback bytes を pixel 単位で検証する。 GPU adapter が無い環境
//! (headless CI 等) では `OffscreenRenderer::new` が `Err` を返すので graceful skip する。
//!
//! memory: `feedback_no_excuse_pixel_verify` (「見える」 で済まさず pixel 単位で確認)。

use daw_ui_renderer::{Color, OffscreenRenderer, Rect, RectCommand, Scene, TextureHandle};

/// GPU が無ければ skip するための helper。 `Some(renderer)` か `None`。
fn try_renderer(w: u32, h: u32) -> Option<OffscreenRenderer> {
    match OffscreenRenderer::new(w, h) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("skip composite GPU test: no adapter/device ({e})");
            None
        }
    }
}

/// `width × height` を一色で塗った child scene を composite して handle を返す。
fn composite_solid(r: &mut OffscreenRenderer, w: u32, h: u32, fill: Color) -> TextureHandle {
    let mut child = Scene::new();
    child.push_rect(RectCommand::uniform_radius(
        Rect::new(0.0, 0.0, w as f32, h as f32),
        fill,
        0.0,
    ));
    r.composite_scene_to_texture(&child, w, h)
        .expect("composite within max texture size")
}

/// readback bytes (RGBA8, stride = width*4) から pixel (x, y) を取り出す。
fn px(bytes: &[u8], width: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let i = ((y * width + x) * 4) as usize;
    (bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3])
}

fn is_red(p: (u8, u8, u8, u8)) -> bool {
    p.0 > 200 && p.1 < 80 && p.2 < 80
}
fn is_blue(p: (u8, u8, u8, u8)) -> bool {
    p.2 > 200 && p.0 < 80 && p.1 < 80
}

/// composite した GPU 常駐 texture が、 そのまま `TexturedQuad.texture` として再 sample できる。
#[test]
fn composite_round_trips_as_textured_quad() {
    let Some(mut r) = try_renderer(16, 16) else { return };
    let tex = composite_solid(&mut r, 16, 16, Color::rgb(1.0, 0.0, 0.0));

    let mut base = Scene::new();
    base.clear_color = Color::BLACK.to_wgpu();
    base.push_textured_quad(daw_ui_renderer::TexturedQuad::new(
        Rect::new(0.0, 0.0, 16.0, 16.0),
        tex,
    ));
    let bytes = r.render_to_rgba(&base).expect("render");

    assert!(
        is_red(px(&bytes, 16, 8, 8)),
        "composite した red texture が base scene で red にならない: {:?}",
        px(&bytes, 16, 8, 8)
    );
}

/// **同一サイズ**の composite を 1 フレーム内で 2 回呼んでも、 pool が別 target を払い出すので
/// 後者が前者を上書きしない (naive な size-key cache だと両方が後者の色に化ける)。
#[test]
fn composite_pool_keeps_distinct_targets_same_size_in_one_frame() {
    let Some(mut r) = try_renderer(32, 16) else { return };
    let red = composite_solid(&mut r, 16, 16, Color::rgb(1.0, 0.0, 0.0));
    let blue = composite_solid(&mut r, 16, 16, Color::rgb(0.0, 0.0, 1.0));
    assert_ne!(
        red.raw(),
        blue.raw(),
        "同 size の連続 composite が同 handle を返した (pool が collision している)"
    );

    let mut base = Scene::new();
    base.clear_color = Color::BLACK.to_wgpu();
    base.push_textured_quad(daw_ui_renderer::TexturedQuad::new(
        Rect::new(0.0, 0.0, 16.0, 16.0),
        red,
    ));
    base.push_textured_quad(daw_ui_renderer::TexturedQuad::new(
        Rect::new(16.0, 0.0, 16.0, 16.0),
        blue,
    ));
    let bytes = r.render_to_rgba(&base).expect("render");

    assert!(
        is_red(px(&bytes, 32, 8, 8)),
        "左半分が red でない (pool collision で blue に化けた?): {:?}",
        px(&bytes, 32, 8, 8)
    );
    assert!(
        is_blue(px(&bytes, 32, 24, 8)),
        "右半分が blue でない: {:?}",
        px(&bytes, 32, 24, 8)
    );
}

/// #064: `rotation_pivot` を corner にすると、 中心 pivot とは別の位置に回転する。
/// quad rect (12,8,8,8) を 90° clockwise 回転:
///
/// - pivot = Some((0,0)) (rect 左上) → x∈[4,12] に着地 (左へ寄る)
/// - pivot = None (中心) → x∈[12,20] に着地 (同じ正方形)
///
/// 検証点 (6,12) と (16,12) が pivot で red/透明が入れ替わることで、 pivot が効いているのを示す。
#[test]
fn rotation_pivot_corner_differs_from_center() {
    let Some(mut r) = try_renderer(24, 24) else { return };
    let tex = composite_solid(&mut r, 8, 8, Color::rgb(1.0, 0.0, 0.0));
    let quad_rect = Rect::new(12.0, 8.0, 8.0, 8.0);
    let theta = std::f32::consts::FRAC_PI_2; // 90°

    // corner pivot: rect 左上 (0,0) 相対 = abs (12,8) 周りに回転 → x∈[4,12]
    let mut corner_scene = Scene::new();
    corner_scene.clear_color = Color::BLACK.to_wgpu();
    corner_scene.push_textured_quad(daw_ui_renderer::TexturedQuad {
        rect: quad_rect,
        texture: tex,
        alpha: 1.0,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        clip_rect: None,
        rotation_radians: theta,
        rotation_pivot: Some((0.0, 0.0)),
    });
    let corner = r.render_to_rgba(&corner_scene).expect("render corner");

    // center pivot (None): rect 中心 (16,12) 周りに回転 → x∈[12,20]
    let mut center_scene = Scene::new();
    center_scene.clear_color = Color::BLACK.to_wgpu();
    center_scene.push_textured_quad(daw_ui_renderer::TexturedQuad {
        rect: quad_rect,
        texture: tex,
        alpha: 1.0,
        uv_min: (0.0, 0.0),
        uv_max: (1.0, 1.0),
        clip_rect: None,
        rotation_radians: theta,
        rotation_pivot: None,
    });
    let center = r.render_to_rgba(&center_scene).expect("render center");

    // (6,12): corner では red、 center では透明 (black)
    assert!(
        is_red(px(&corner, 24, 6, 12)),
        "corner pivot で (6,12) が red でない: {:?}",
        px(&corner, 24, 6, 12)
    );
    assert!(
        !is_red(px(&center, 24, 6, 12)),
        "center pivot で (6,12) が red になっている (pivot が効いていない?): {:?}",
        px(&center, 24, 6, 12)
    );
    // (16,12): center では red、 corner では透明
    assert!(
        is_red(px(&center, 24, 16, 12)),
        "center pivot で (16,12) が red でない: {:?}",
        px(&center, 24, 16, 12)
    );
    assert!(
        !is_red(px(&corner, 24, 16, 12)),
        "corner pivot で (16,12) が red になっている: {:?}",
        px(&corner, 24, 16, 12)
    );
}
