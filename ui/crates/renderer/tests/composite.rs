//! M14 Phase 93 (daw_01 #063) + Phase 92 (#064) の GPU pixel verify。
//!
//! `OffscreenRenderer::composite_scene_to_texture` で焼いた GPU 常駐 texture を `TexturedQuad`
//! として再描画し、 readback bytes を pixel 単位で検証する。 GPU adapter が無い環境
//! (headless CI 等) では `OffscreenRenderer::new` が `Err` を返すので graceful skip する。
//!
//! memory: `feedback_no_excuse_pixel_verify` (「見える」 で済まさず pixel 単位で確認)。

use daw_ui_renderer::{
    Color, GlyphArea, HAlign, OffscreenRenderer, Rect, RectCommand, Scene, TextureHandle, VAlign,
};

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
fn is_green(p: (u8, u8, u8, u8)) -> bool {
    p.1 > 200 && p.0 < 80 && p.2 < 80
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

/// render cycle を跨いで同 size を composite すると、 `end_cycle` が前 cycle の target を解放して
/// いるので **同じ handle が再利用**される (= 毎フレーム新規確保で pool が膨らむ leak の回帰防止)。
/// `end_cycle` が走らなければ前 target が in-use のまま残り、 新規 handle が払い出されて assert が落ちる。
#[test]
fn composite_pool_reuses_target_across_render_cycles() {
    let Some(mut r) = try_renderer(16, 16) else { return };
    let mut base = Scene::new();
    base.clear_color = Color::BLACK.to_wgpu();

    let h1 = composite_solid(&mut r, 16, 16, Color::rgb(1.0, 0.0, 0.0));
    let _ = r.render_to_rgba(&base).expect("render cycle 1"); // end_cycle が h1 の target を解放
    let h2 = composite_solid(&mut r, 16, 16, Color::rgb(0.0, 0.0, 1.0)); // 解放済 target を再利用
    let _ = r.render_to_rgba(&base).expect("render cycle 2");
    let h3 = composite_solid(&mut r, 16, 16, Color::rgb(0.0, 1.0, 0.0));

    assert_eq!(
        h1.raw(),
        h2.raw(),
        "render cycle 後の同 size composite が target を再利用していない (pool leak)"
    );
    assert_eq!(h2.raw(), h3.raw(), "target 再利用が継続していない");
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

// ============================================================
// M14 Phase 106 (daw_01 #077): async double-buffer readback
// ============================================================

/// `submit_readback` + `finish_readback` の出力が、 同 scene を `render_to_rgba` した同期版と
/// **bit 単位で完全一致**する (export/preview byte parity 要件、 共有 `encode_scene_into` 経路の証明)。
#[test]
fn async_readback_matches_sync_byte_for_byte() {
    let Some(mut r) = try_renderer(40, 24) else { return };
    let mut scene = Scene::new();
    scene.clear_color = Color::BLACK.to_wgpu();
    scene.push_rect(RectCommand::uniform_radius(
        Rect::new(4.0, 4.0, 20.0, 14.0),
        Color::rgb(1.0, 0.0, 0.0),
        3.0,
    ));
    scene.push_rect(RectCommand::uniform_radius(
        Rect::new(18.0, 8.0, 18.0, 12.0),
        Color::rgb(0.0, 0.0, 1.0),
        0.0,
    ));

    let sync = r.render_to_rgba(&scene).expect("sync render");
    let pending = r.submit_readback(&scene).expect("submit_readback");
    let async_bytes = r.finish_readback(pending).expect("finish_readback");

    assert_eq!(
        sync.len(),
        async_bytes.len(),
        "async readback の byte 数が同期版と違う"
    );
    assert_eq!(
        sync, async_bytes,
        "async readback bytes が同期 render_to_rgba と bit 単位で一致しない"
    );
}

/// 非黒 clear + rect + textured-quad (export の主役: video frame) + plain text (glyph) + outline text
/// (text_effect) を 1 scene に詰め、 `encode_scene_into` の全 export-relevant 分岐を通しても async
/// readback が同期 `render_to_rgba` と **bit 単位で一致**する。 = byte-parity が「rect だけ」 でなく
/// clear/texture/glyph/text_effect cache lifecycle 全体で成立する証明。
/// (popup pass は export では使われない (`render_video` は `scene.primitives` のみ構築) ので対象外。)
#[test]
fn async_readback_matches_sync_with_text_and_effects() {
    let Some(mut r) = try_renderer(96, 40) else { return };
    // export の video frame に相当する uploaded texture (緑単色 4x4)。 両経路で同 handle を sample。
    let tex = r.create_texture(4, 4);
    let green = [0u8, 200, 0, 255].repeat(4 * 4);
    r.upload_texture_rgba(tex, &green);

    let mut scene = Scene::new();
    scene.clear_color = Color::rgb(0.05, 0.10, 0.15).to_wgpu(); // 非黒 clear で clear 経路も検証
    scene.push_rect(RectCommand::uniform_radius(
        Rect::new(2.0, 2.0, 92.0, 36.0),
        Color::rgb(0.1, 0.1, 0.12),
        2.0,
    ));
    scene.push_textured_quad(daw_ui_renderer::TexturedQuad::new(
        Rect::new(60.0, 4.0, 28.0, 28.0),
        tex,
    ));
    // plain text (glyph pipeline 経路)。
    scene.push_text(GlyphArea {
        text: "Export".into(),
        left: 6.0,
        top: 6.0,
        font_size: 14.0,
        line_height: 16.0,
        color: Color::rgb(1.0, 1.0, 1.0),
        font_family: None,
        clip_rect: None,
        outline_color: Color::rgba(0.0, 0.0, 0.0, 0.0),
        outline_width_px: 0.0,
        shadow_color: Color::rgba(0.0, 0.0, 0.0, 0.0),
        shadow_offset_px: (0.0, 0.0),
        shadow_blur_px: 0.0,
        rotation_radians: 0.0,
        ..GlyphArea::default()
    });
    // outline 付き text (text_effect compositor で offscreen 焼き → Primitive::Texture substitution)。
    scene.push_text(GlyphArea {
        text: "RGBA".into(),
        left: 6.0,
        top: 22.0,
        font_size: 14.0,
        line_height: 16.0,
        color: Color::rgb(1.0, 0.8, 0.2),
        font_family: None,
        clip_rect: None,
        outline_color: Color::rgb(0.0, 0.0, 0.0),
        outline_width_px: 2.0,
        shadow_color: Color::rgba(0.0, 0.0, 0.0, 0.0),
        shadow_offset_px: (0.0, 0.0),
        shadow_blur_px: 0.0,
        rotation_radians: 0.0,
        ..GlyphArea::default()
    });

    let sync = r.render_to_rgba(&scene).expect("sync render");
    let pending = r.submit_readback(&scene).expect("submit_readback");
    let async_bytes = r.finish_readback(pending).expect("finish_readback");

    assert_eq!(
        sync, async_bytes,
        "text + outline を含む scene で async readback が同期版と bit 一致しない (glyph/text_effect cache 経路の食い違い?)"
    );
    // 描画が空でない (全黒だと parity が trivially 成立してしまうので非黒 pixel を 1 つ確認)。
    assert!(
        (0..sync.len()).step_by(4).any(|i| sync[i] > 40 || sync[i + 1] > 40 || sync[i + 2] > 40),
        "scene が全黒に近い (text が描画されていない?)"
    );
}

/// double-buffer: `submit(A) → submit(B) → finish(A) → finish(B)`。 B の composite が end_cycle で
/// 解放された A の pool target を **再利用**しても、 GPU submit 順 (composite(A)→render(A)→
/// composite(B)→render(B)) により A の readback は A の内容 (red) を保つ。 = async overlap が
/// composite pool と安全に共存する証明。
#[test]
fn double_buffer_async_readback_keeps_frames_distinct() {
    let Some(mut r) = try_renderer(16, 16) else { return };

    // frame A: red を composite して参照。
    let tex_a = composite_solid(&mut r, 16, 16, Color::rgb(1.0, 0.0, 0.0));
    let mut scene_a = Scene::new();
    scene_a.clear_color = Color::BLACK.to_wgpu();
    scene_a.push_textured_quad(daw_ui_renderer::TexturedQuad::new(
        Rect::new(0.0, 0.0, 16.0, 16.0),
        tex_a,
    ));
    let pa = r.submit_readback(&scene_a).expect("submit A");

    // frame B: blue を composite (A の解放済 pool target を再利用する) して参照。
    let tex_b = composite_solid(&mut r, 16, 16, Color::rgb(0.0, 0.0, 1.0));
    let mut scene_b = Scene::new();
    scene_b.clear_color = Color::BLACK.to_wgpu();
    scene_b.push_textured_quad(daw_ui_renderer::TexturedQuad::new(
        Rect::new(0.0, 0.0, 16.0, 16.0),
        tex_b,
    ));
    let pb = r.submit_readback(&scene_b).expect("submit B");

    let a = r.finish_readback(pa).expect("finish A");
    let b = r.finish_readback(pb).expect("finish B");

    assert!(
        is_red(px(&a, 16, 8, 8)),
        "frame A が red でない (B の composite が A を破壊した?): {:?}",
        px(&a, 16, 8, 8)
    );
    assert!(
        is_blue(px(&b, 16, 8, 8)),
        "frame B が blue でない: {:?}",
        px(&b, 16, 8, 8)
    );
}

/// double-buffer で **glyph pipeline** が frame をまたいで leak しないことの adversarial 検証。
/// glyph pipeline は composite target (per-call) と違い base/popup で **単一 instance buffer** を
/// 共有するので、 LAST WRITE WINS trap の最有力候補。 `submit(A の赤 text) → submit(B の青 text) →
/// finish(A)` で A の readback に B の青 text が混入しないこと (= `queue.write_buffer` が submit ごとに
/// flush される「別 submit なら安全」 原理) を pixel 計数で確認する。
#[test]
fn double_buffer_glyph_does_not_leak_across_frames() {
    let Some(mut r) = try_renderer(200, 40) else { return };

    let mut scene_a = Scene::new();
    scene_a.clear_color = Color::BLACK.to_wgpu();
    scene_a.push_text(GlyphArea::new(
        std::sync::Arc::from("A"),
        4.0,
        4.0,
        24.0,
        28.0,
        Color::rgb(1.0, 0.0, 0.0),
    ));
    let pa = r.submit_readback(&scene_a).expect("submit A");

    let mut scene_b = Scene::new();
    scene_b.clear_color = Color::BLACK.to_wgpu();
    scene_b.push_text(GlyphArea::new(
        std::sync::Arc::from("B"),
        4.0,
        4.0,
        24.0,
        28.0,
        Color::rgb(0.0, 0.0, 1.0),
    ));
    let pb = r.submit_readback(&scene_b).expect("submit B");

    let a = r.finish_readback(pa).expect("finish A");
    let b = r.finish_readback(pb).expect("finish B");

    // 明るい pixel のうち red 寄り / blue 寄りを数える。
    let count = |bytes: &[u8], red: bool| {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| {
                let bright = u32::from(px[0]) + u32::from(px[1]) + u32::from(px[2]) > 60;
                bright && if red { px[0] > px[2] } else { px[2] > px[0] }
            })
            .count()
    };
    let a_red = count(&a, true);
    let a_blue = count(&a, false);
    let b_blue = count(&b, false);
    assert!(a_red > 5, "frame A の赤 text が無い: red={a_red} blue={a_blue}");
    assert!(a_blue < a_red, "frame A に B の青 text が leak した: red={a_red} blue={a_blue}");
    assert!(b_blue > 5, "frame B の青 text が無い: blue={b_blue}");
}

/// 多数フレームを `submit → finish` 直列で回すと slot が再利用される (= unmap 済 slot を再 acquire
/// できる回帰防止)。 reuse が壊れていれば map 済 buffer の再利用などで panic / error する。
#[test]
fn sequential_async_readback_reuses_slot() {
    let Some(mut r) = try_renderer(8, 8) else { return };
    for i in 0..6 {
        let fill = if i % 2 == 0 {
            Color::rgb(1.0, 0.0, 0.0)
        } else {
            Color::rgb(0.0, 0.0, 1.0)
        };
        let mut scene = Scene::new();
        scene.clear_color = Color::BLACK.to_wgpu();
        scene.push_rect(RectCommand::uniform_radius(
            Rect::new(0.0, 0.0, 8.0, 8.0),
            fill,
            0.0,
        ));
        let pending = r.submit_readback(&scene).expect("submit");
        let bytes = r.finish_readback(pending).expect("finish");
        if i % 2 == 0 {
            assert!(is_red(px(&bytes, 8, 4, 4)), "frame {i} が red でない: {:?}", px(&bytes, 8, 4, 4));
        } else {
            assert!(is_blue(px(&bytes, 8, 4, 4)), "frame {i} が blue でない: {:?}", px(&bytes, 8, 4, 4));
        }
    }
}

// ============================================================
// M14 Phase 112 (daw_01 #084): text_effect compositor の per-pass renderer/viewport pool
// ============================================================

/// effect 付き `GlyphArea` が **同一フレームに複数枚同時 active** なとき、 各 offscreen glyph pass が
/// 専用の `TextRenderer` (= 1 内部 vertex_buffer) + `Viewport` (= 1 resolution uniform) を持つので、
/// 各 overlay が **それぞれ固有の文字** を **固有サイズの target** に焼く。 単一 instance 使い回し時代
/// (#084) は submit 時の LAST WRITE WINS で全枚が「最後に prepare された 1 枚」 の文字列・resolution で
/// 焼けていた (daw_01 実機: クレジットと歌詞が両方クレジット文字列に化ける)。
///
/// 3 枚を別 region に push (= pool が 3 slot に grow): **赤・幅広** (outline のみ) / **青・幅狭**
/// (shadow **blur** あり = H/V blur pass も通る) / **緑・大きめ** (descender、 composite size が他と異なる)。
/// 最後に push する緑がバグ時の「勝者」。 修正後は各 region が固有色を多数持ち他色を持たない:
/// - renderer pool が無いと全 region が **緑** (= 最後の overlay の文字) に化ける。
/// - viewport 共有だと幅/高さの違う overlay が緑の resolution で NDC 変換され mis-scale / off-target。
///   いずれの回帰でも「各 region が自色を多数 + 他色なし」 assert で検出できる。 glyphon 0.11 の
///   resolution 依存箇所は text_render.rs:146 (prepare の bounds clamp) / :350 + shader.wgsl:65 (submit 時 NDC)。
#[test]
fn text_effect_multiple_effectful_overlays_render_distinct_text() {
    const W: u32 = 220;
    const H: u32 = 170;
    let Some(mut r) = try_renderer(W, H) else { return };

    // (text, top, color, font_size, outline_px, blur_px) → effect 付き GlyphArea。
    let area = |text: &str, top: f32, color: Color, font: f32, outline: f32, blur: f32| GlyphArea {
        text: text.into(),
        left: 8.0,
        top,
        font_size: font,
        line_height: font * 1.2,
        color,
        font_family: None,
        clip_rect: None,
        outline_color: Color::rgb(0.0, 0.0, 0.0),
        outline_width_px: outline,
        shadow_color: if blur > 0.0 {
            Color::rgba(0.0, 0.0, 0.0, 0.7)
        } else {
            Color::rgba(0.0, 0.0, 0.0, 0.0)
        },
        shadow_offset_px: (0.0, 0.0),
        shadow_blur_px: blur,
        rotation_radians: 0.0,
        ..GlyphArea::default()
    };

    let mut scene = Scene::new();
    scene.clear_color = Color::BLACK.to_wgpu();
    // 別 region・別サイズ・別 effect。 最後 (緑) がバグ時に全枚へ漏れる「勝者」。
    scene.push_text(area("WWWWWW", 8.0, Color::rgb(1.0, 0.0, 0.0), 20.0, 2.0, 0.0)); // 幅広 outline
    scene.push_text(area("lij", 64.0, Color::rgb(0.0, 0.0, 1.0), 20.0, 1.0, 3.0)); // 幅狭 + blur
    scene.push_text(area("gpqy", 116.0, Color::rgb(0.0, 1.0, 0.0), 30.0, 2.0, 0.0)); // 大きめ descender

    let bytes = r.render_to_rgba(&scene).expect("render");

    let count = |pred: fn((u8, u8, u8, u8)) -> bool, y0: u32, y1: u32| -> usize {
        let mut n = 0;
        for y in y0..y1 {
            for x in 0..W {
                if pred(px(&bytes, W, x, y)) {
                    n += 1;
                }
            }
        }
        n
    };

    // region A 赤 (y[0,56)) / B 青 (y[58,112)) / C 緑 (y[112,170))。 想定: A 数百 red、 B 数十 blue
    // (blur は shadow のみ、 fill は sharp blue)、 C 数百 green。 緑漏れ (= renderer 共有) を各 region で否定。
    assert!(count(is_red, 0, 56) > 30, "A region に赤 (幅広 outline overlay) が無い: {}", count(is_red, 0, 56));
    assert!(count(is_green, 0, 56) < 5, "A region に緑が漏れた (最後の overlay が renderer 共有で焼かれた #084): {}", count(is_green, 0, 56));
    assert!(count(is_blue, 58, 112) > 10, "B region に青 (幅狭 + blur overlay) が無い: {}", count(is_blue, 58, 112));
    assert!(count(is_green, 58, 112) < 5, "B region に緑が漏れた #084: {}", count(is_green, 58, 112));
    assert!(count(is_green, 112, H) > 30, "C region に緑 (大きめ overlay) が無い: {}", count(is_green, 112, H));
}

/// cache-hit の effect area は `render_effect` 冒頭で early-return し offscreen glyph pass を発行しない
/// (= renderer/viewport slot を消費しない) ので、 同フレームの cache-miss area の pool 払い出しを乱さない。
/// frame1 で A(赤)+B(青) を bake、 frame2 で A(同一 params = cache hit) + C(緑、 新規 miss) を描く。
/// frame2 の出力は A が frame1 の baked red を保持 (cache が壊れていない) + C が緑で正しく焼ける。
#[test]
fn text_effect_cache_hit_coexists_with_miss() {
    const W: u32 = 160;
    const H: u32 = 110;
    let Some(mut r) = try_renderer(W, H) else { return };

    let area = |text: &str, top: f32, color: Color| GlyphArea {
        text: text.into(),
        left: 8.0,
        top,
        font_size: 20.0,
        line_height: 24.0,
        color,
        font_family: None,
        clip_rect: None,
        outline_color: Color::rgb(0.0, 0.0, 0.0),
        outline_width_px: 2.0,
        shadow_color: Color::rgba(0.0, 0.0, 0.0, 0.0),
        shadow_offset_px: (0.0, 0.0),
        shadow_blur_px: 0.0,
        rotation_radians: 0.0,
        ..GlyphArea::default()
    };

    // frame 1: A(赤, 上) + B(青, 下) を bake (両方 cache-miss)。
    let mut f1 = Scene::new();
    f1.clear_color = Color::BLACK.to_wgpu();
    f1.push_text(area("AAAA", 8.0, Color::rgb(1.0, 0.0, 0.0)));
    f1.push_text(area("BBBB", 58.0, Color::rgb(0.0, 0.0, 1.0)));
    let _ = r.render_to_rgba(&f1).expect("frame 1");

    // frame 2: A(同一 params = cache hit、 slot 非消費) + C(緑, 下 = 新規 cache miss)。
    let mut f2 = Scene::new();
    f2.clear_color = Color::BLACK.to_wgpu();
    f2.push_text(area("AAAA", 8.0, Color::rgb(1.0, 0.0, 0.0)));
    f2.push_text(area("CCCC", 58.0, Color::rgb(0.0, 1.0, 0.0)));
    let bytes = r.render_to_rgba(&f2).expect("frame 2");

    let count = |pred: fn((u8, u8, u8, u8)) -> bool, y0: u32, y1: u32| -> usize {
        let mut n = 0;
        for y in y0..y1 {
            for x in 0..W {
                if pred(px(&bytes, W, x, y)) {
                    n += 1;
                }
            }
        }
        n
    };

    assert!(count(is_red, 0, 44) > 20, "frame2: cache-hit A の赤が無い (cache が壊れた?): {}", count(is_red, 0, 44));
    assert!(count(is_green, 0, 44) < 5, "frame2: A region に C の緑が漏れた: {}", count(is_green, 0, 44));
    assert!(count(is_green, 48, H) > 20, "frame2: cache-miss C の緑が無い: {}", count(is_green, 48, H));
}

/// M14 Phase 122 (daw_01 #097): effect 付き box-center text が **cache HIT 経路でも** box 中心に
/// 配置され続ける回帰テスト。 frame1 = cache miss (bake、 aligned)、 frame2 = cache hit。 align は
/// baked texture を変えないので EffectKey に align を入れず cache 共有しているが、 配置は hit 経路
/// でも実測寸法 (`CachedEffect.text_w/h`) から `aligned_origin` で再計算する必要がある。 これが無い
/// と hit frame で left/top 原点に戻る (review 指摘の blocker)。 frame2 の ink 水平中心 = box 中心。
#[test]
fn text_effect_box_center_align_holds_on_cache_hit() {
    const W: u32 = 280;
    const H: u32 = 90;
    let Some(mut r) = try_renderer(W, H) else { return };

    let box_x = 20.0_f32;
    let box_w = 240.0_f32;
    let box_center = box_x + box_w * 0.5; // 140

    let area = || GlyphArea {
        text: "あいう".into(),
        left: box_x,
        top: 12.0,
        font_size: 22.0,
        line_height: 28.0,
        color: Color::rgb(1.0, 1.0, 1.0),
        box_width: Some(box_w),
        box_height: Some(64.0),
        align_h: HAlign::Center,
        align_v: VAlign::Center,
        outline_color: Color::rgb(0.0, 0.0, 0.0),
        outline_width_px: 2.0,
        ..GlyphArea::default()
    };

    // frame 1: cache miss (bake)。
    let mut f1 = Scene::new();
    f1.clear_color = Color::BLACK.to_wgpu();
    f1.push_text(area());
    let _ = r.render_to_rgba(&f1).expect("frame 1 (miss)");

    // frame 2: cache hit。 ここで align が落ちる回帰を検出する。
    let mut f2 = Scene::new();
    f2.clear_color = Color::BLACK.to_wgpu();
    f2.push_text(area());
    let bytes = r.render_to_rgba(&f2).expect("frame 2 (hit)");

    // ink (= 明るい white text pixel) の水平範囲を scan。 background / outline は BLACK で除外。
    let (mut lo, mut hi): (Option<u32>, Option<u32>) = (None, None);
    for y in 0..H {
        for x in 0..W {
            let (rr, gg, bb, _) = px(&bytes, W, x, y);
            let lum = 0.299 * f32::from(rr) + 0.587 * f32::from(gg) + 0.114 * f32::from(bb);
            if lum > 150.0 {
                lo = Some(lo.map_or(x, |m| m.min(x)));
                hi = Some(hi.map_or(x, |m| m.max(x)));
            }
        }
    }
    let (lo, hi) = (lo.expect("no ink in cache-hit frame"), hi.expect("no ink"));
    let center = (lo + hi) as f32 * 0.5;
    assert!(
        (center - box_center).abs() <= 14.0,
        "cache-hit frame で box-center align がずれた: ink_center={center:.1} vs box_center={box_center:.1} (ink {lo}..{hi})"
    );
}

/// `clear_readback_cache` 後の古い `PendingReadback` (世代不一致) を `finish_readback` に渡すと
/// panic せず `Err` を返す (stale token guard)。
#[test]
fn stale_pending_after_cache_clear_errors() {
    let Some(mut r) = try_renderer(8, 8) else { return };
    let mut scene = Scene::new();
    scene.clear_color = Color::BLACK.to_wgpu();
    scene.push_rect(RectCommand::uniform_radius(
        Rect::new(0.0, 0.0, 8.0, 8.0),
        Color::rgb(0.0, 1.0, 0.0),
        0.0,
    ));

    let stale = r.submit_readback(&scene).expect("submit stale"); // slot 0, gen 1
    r.clear_readback_cache(); // slots を空に (gen は単調増加継続)
    let fresh = r.submit_readback(&scene).expect("submit fresh"); // slot 0 再生成、 gen != 1

    assert!(
        r.finish_readback(stale).is_err(),
        "clear 後の stale token が Err にならず回収できてしまった"
    );
    // fresh は正常回収できる (clear / stale 検出が live slot を巻き込んでいない)。
    let bytes = r.finish_readback(fresh).expect("finish fresh");
    assert!(
        is_green(px(&bytes, 8, 4, 4)),
        "fresh readback が green でない: {:?}",
        px(&bytes, 8, 4, 4)
    );
}
