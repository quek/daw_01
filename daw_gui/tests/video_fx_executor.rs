//! docs/plan_video_fx.md: GPU 効果実行基盤 ([`daw_gui::video_fx::VideoFxEngine`])
//! の end-to-end pixel verify。
//!
//! gui_01 #111 の interop primitive (`raw_texture` / `create_render_target`) の上に daw_01 が組んだ
//! effect pipeline が、 **実際に src を sample して効果を適用し ping-pong で正しく合成する**ことを
//! pixel 単位で確認する (memory: feedback_verify_actual_content — 「在る」 でなく動的挙動を検証)。
//!
//! GPU adapter が無い環境では `OffscreenRenderer::new` が `Err` を返すので graceful skip。

use common::video_fx::def_by_id;
use daw_gui::video_fx::{ResolvedEffect, VideoFxEngine};
use daw_ui_renderer::{Color, OffscreenRenderer, Rect, Scene, TexturedQuad};

fn try_renderer(w: u32, h: u32) -> Option<OffscreenRenderer> {
    match OffscreenRenderer::new(w, h) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("skip video_fx GPU test: no adapter/device ({e})");
            None
        }
    }
}

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
fn is_cyan(p: (u8, u8, u8, u8)) -> bool {
    p.0 < 80 && p.1 > 200 && p.2 > 200
}
fn is_yellow(p: (u8, u8, u8, u8)) -> bool {
    p.0 > 200 && p.1 > 200 && p.2 < 80
}

/// 左 red / 右 blue (= トラック合成画 / 動画フレームの代理)。一色でなく 2 色にするのは
/// 効果が定数でなく実 sample していることを左右の色保存で示すため。
fn red_left_blue_right(w: u32, h: u32) -> Vec<u8> {
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            if x < w / 2 {
                data[i] = 255;
            } else {
                data[i + 2] = 255;
            }
            data[i + 3] = 255;
        }
    }
    data
}

/// 最終 handle を base scene に push して RGBA8 を読み戻す共通経路 (gui_01 #111 D2: 効果 submit の後)。
fn readback(r: &mut OffscreenRenderer, handle: daw_ui_renderer::TextureHandle, w: u32, h: u32) -> Vec<u8> {
    let mut base = Scene::new();
    base.clear_color = Color::BLACK.to_wgpu();
    base.push_textured_quad(TexturedQuad::new(Rect::new(0.0, 0.0, w as f32, h as f32), handle));
    r.render_to_rgba(&base).expect("render base scene")
}

fn invert(amount: f32) -> ResolvedEffect {
    ResolvedEffect {
        def: def_by_id("builtin.video.invert").expect("invert def"),
        params: vec![amount],
    }
}

/// INVERT (amount=1) を 1 パス適用すると red→cyan / blue→yellow。 raw_texture の実 sample +
/// create_render_target への描画 + uniform param 配線 + submit 順序を一括検証。
#[test]
fn invert_effect_transforms_colors() {
    const W: u32 = 16;
    const H: u32 = 16;
    let Some(mut r) = try_renderer(W, H) else { return };

    let src = r.create_texture(W, H);
    r.upload_texture_rgba(src, &red_left_blue_right(W, H));

    let mut engine = VideoFxEngine::new();
    let out = engine.apply_chain(&mut r, src, W, H, &[invert(1.0)], 0);

    let bytes = readback(&mut r, out, W, H);
    assert!(
        is_cyan(px(&bytes, W, 4, 8)),
        "左 red が invert で cyan にならない: {:?}",
        px(&bytes, W, 4, 8)
    );
    assert!(
        is_yellow(px(&bytes, W, 12, 8)),
        "右 blue が invert で yellow にならない: {:?}",
        px(&bytes, W, 12, 8)
    );

    engine.clear(&mut r);
    r.destroy_texture(src);
}

/// amount=0 は完全 passthrough (mix(src, 1-src, 0) = src)。param 値が uniform に正しく
/// 届いていることを確認 (0 と 1 で結果が変わる)。
#[test]
fn invert_amount_zero_is_passthrough() {
    const W: u32 = 16;
    const H: u32 = 16;
    let Some(mut r) = try_renderer(W, H) else { return };

    let src = r.create_texture(W, H);
    r.upload_texture_rgba(src, &red_left_blue_right(W, H));

    let mut engine = VideoFxEngine::new();
    let out = engine.apply_chain(&mut r, src, W, H, &[invert(0.0)], 0);

    let bytes = readback(&mut r, out, W, H);
    assert!(is_red(px(&bytes, W, 4, 8)), "amount=0 で left が red 保存されない: {:?}", px(&bytes, W, 4, 8));
    assert!(is_blue(px(&bytes, W, 12, 8)), "amount=0 で right が blue 保存されない: {:?}", px(&bytes, W, 12, 8));

    engine.clear(&mut r);
    r.destroy_texture(src);
}

/// 2 効果チェーン (invert → invert) は ping-pong を経て元に戻る。A→B の交互 target 切替を検証。
#[test]
fn two_pass_ping_pong_restores_original() {
    const W: u32 = 16;
    const H: u32 = 16;
    let Some(mut r) = try_renderer(W, H) else { return };

    let src = r.create_texture(W, H);
    r.upload_texture_rgba(src, &red_left_blue_right(W, H));

    let mut engine = VideoFxEngine::new();
    let out = engine.apply_chain(&mut r, src, W, H, &[invert(1.0), invert(1.0)], 0);

    let bytes = readback(&mut r, out, W, H);
    assert!(is_red(px(&bytes, W, 4, 8)), "invert×2 で left が red に戻らない (ping-pong 不正?): {:?}", px(&bytes, W, 4, 8));
    assert!(is_blue(px(&bytes, W, 12, 8)), "invert×2 で right が blue に戻らない: {:?}", px(&bytes, W, 12, 8));

    engine.clear(&mut r);
    r.destroy_texture(src);
}

/// 回帰 (review 検出 High): 同寸の効果レイヤーが **同一 frame 内で複数** あるとき、各レイヤーは
/// 自分の効果出力を保持しなければならない。遅延描画 (全 quad を 1 scene に貯めて末尾で 1 回 render)
/// 下で pool target を (w,h) 共有していると、後の apply_chain が前の出力を上書きし全レイヤーが
/// 最後の出力になる (= 動画クロスフェードで発症)。end_frame を挟まず 2 回 apply して 1 scene に
/// 両方 push し、左=cyan(invert red) / 右=blue(passthrough) を確認する。
#[test]
fn two_same_size_layers_do_not_collide() {
    const W: u32 = 16;
    const H: u32 = 16;
    let Some(mut r) = try_renderer(W, H) else { return };

    // 同寸の src 2 枚 (全面 red / 全面 blue)。
    let mut red = vec![0u8; (W * H * 4) as usize];
    let mut blue = vec![0u8; (W * H * 4) as usize];
    for i in 0..(W * H) as usize {
        red[i * 4] = 255;
        red[i * 4 + 3] = 255;
        blue[i * 4 + 2] = 255;
        blue[i * 4 + 3] = 255;
    }
    let src_red = r.create_texture(W, H);
    r.upload_texture_rgba(src_red, &red);
    let src_blue = r.create_texture(W, H);
    r.upload_texture_rgba(src_blue, &blue);

    let mut engine = VideoFxEngine::new();
    // 同一 frame 内 (end_frame を挟まない) で 2 回 apply。red→invert=cyan、blue→passthrough=blue。
    let out_red = engine.apply_chain(&mut r, src_red, W, H, &[invert(1.0)], 0);
    let out_blue = engine.apply_chain(&mut r, src_blue, W, H, &[invert(0.0)], 1);
    assert_ne!(out_red, out_blue, "同寸 2 レイヤーが同一 target を共有している (衝突)");

    // 両方を 1 scene に push (左半分=out_red、右半分=out_blue) → 1 回 render。
    let mut base = Scene::new();
    base.clear_color = Color::BLACK.to_wgpu();
    base.push_textured_quad(TexturedQuad::new(Rect::new(0.0, 0.0, 8.0, H as f32), out_red));
    base.push_textured_quad(TexturedQuad::new(Rect::new(8.0, 0.0, 8.0, H as f32), out_blue));
    let bytes = r.render_to_rgba(&base).expect("render base scene");

    assert!(
        is_cyan(px(&bytes, W, 4, 8)),
        "左 (out_red) が cyan でない (後の apply に上書きされた?): {:?}",
        px(&bytes, W, 4, 8)
    );
    assert!(
        is_blue(px(&bytes, W, 12, 8)),
        "右 (out_blue) が blue でない: {:?}",
        px(&bytes, W, 12, 8)
    );

    engine.end_frame(&mut r);
    engine.clear(&mut r);
    r.destroy_texture(src_red);
    r.destroy_texture(src_blue);
}

/// 空チェーンは src をそのまま返す (no-op)。
#[test]
fn empty_chain_returns_src() {
    const W: u32 = 8;
    const H: u32 = 8;
    let Some(mut r) = try_renderer(W, H) else { return };
    let src = r.create_texture(W, H);
    let mut engine = VideoFxEngine::new();
    let out = engine.apply_chain(&mut r, src, W, H, &[], 0);
    assert_eq!(out, src, "空チェーンは src をそのまま返すべき");
    r.destroy_texture(src);
}

fn gaussian_blur(radius: f32) -> ResolvedEffect {
    ResolvedEffect {
        def: def_by_id("builtin.video.gaussian_blur").expect("gaussian_blur def"),
        params: vec![radius],
    }
}

fn pixelate(cells: f32) -> ResolvedEffect {
    ResolvedEffect {
        def: def_by_id("builtin.video.pixelate").expect("pixelate def"),
        params: vec![cells],
    }
}

/// Wave4a 回帰: SeparableBlur (H/V 2 パス) が red|blue の鋭いエッジを混色へ軟化する。
/// 新しい分離ブラープリミティブの WGSL 生成 + 2 パス ping-pong + texel 配線を pixel 検証。
#[test]
fn gaussian_blur_softens_edge() {
    const W: u32 = 32;
    const H: u32 = 16;
    let Some(mut r) = try_renderer(W, H) else { return };

    let src = r.create_texture(W, H);
    r.upload_texture_rgba(src, &red_left_blue_right(W, H)); // edge at x=16

    let mut engine = VideoFxEngine::new();
    let out = engine.apply_chain(&mut r, src, W, H, &[gaussian_blur(6.0)], 0);

    let bytes = readback(&mut r, out, W, H);
    // エッジ (x=16) 付近は red と blue が混ざり両チャネルが立つ (= 実際に近傍を sample している)。
    let edge = px(&bytes, W, 16, 8);
    assert!(
        edge.0 > 30 && edge.2 > 30,
        "ブラー後のエッジが混色でない (近傍 sample してない?): {edge:?}"
    );
    // 遠方は元色を概ね保持 (sampler は ClampToEdge)。
    let far_left = px(&bytes, W, 1, 8);
    let far_right = px(&bytes, W, 30, 8);
    assert!(far_left.0 > far_left.2, "遠左が red 寄りでない: {far_left:?}");
    assert!(far_right.2 > far_right.0, "遠右が blue 寄りでない: {far_right:?}");

    engine.clear(&mut r);
    r.destroy_texture(src);
}

/// Wave4a 回帰: Pixelate (Simple 1 パス) の WGSL がコンパイル・実行され、ブロック中心を
/// sample して概ね元色を保つ。 red|blue を粗いセルで潰しても遠方の支配色が保存される。
#[test]
fn pixelate_runs_and_preserves_dominant_colors() {
    const W: u32 = 16;
    const H: u32 = 16;
    let Some(mut r) = try_renderer(W, H) else { return };

    let src = r.create_texture(W, H);
    r.upload_texture_rgba(src, &red_left_blue_right(W, H)); // edge at x=8

    let mut engine = VideoFxEngine::new();
    // cells=8 → 2px ブロック。各ブロック中心を sample (左端=red ブロック / 右端=blue ブロック)。
    let out = engine.apply_chain(&mut r, src, W, H, &[pixelate(8.0)], 0);

    let bytes = readback(&mut r, out, W, H);
    assert!(is_red(px(&bytes, W, 1, 8)), "左端ブロックが red でない: {:?}", px(&bytes, W, 1, 8));
    assert!(is_blue(px(&bytes, W, 14, 8)), "右端ブロックが blue でない: {:?}", px(&bytes, W, 14, 8));

    engine.clear(&mut r);
    r.destroy_texture(src);
}

/// Wave4b 回帰: カタログの**全効果**（passes を持つもの）が WGSL コンパイル + 実行できる
/// （pipeline 生成 = naga 検証）。1 つでも壊れた WGSL / param レイアウト不整合があれば
/// apply_chain の pipeline 作成で panic する。Transform（passes 空＝配置 device）は
/// apply_chain 非対象なので skip。各効果は manifest default param で 1 回適用する。
#[test]
fn all_catalog_effects_compile_and_run() {
    const W: u32 = 16;
    const H: u32 = 16;
    let Some(mut r) = try_renderer(W, H) else { return };
    let src = r.create_texture(W, H);
    r.upload_texture_rgba(src, &red_left_blue_right(W, H));

    let mut engine = VideoFxEngine::new();
    for def in common::video_fx::builtin_video_fx() {
        if def.passes.is_empty() {
            continue; // Transform 等の配置 device（GPU パス無し）。
        }
        // 全 param を manifest default（実レンジ）で。
        let params: Vec<f32> = def
            .params
            .iter()
            .map(|p| p.kind.norm_to_real(p.kind.default_norm()))
            .collect();
        let eff = ResolvedEffect { def, params };
        let out = engine.apply_chain(&mut r, src, W, H, &[eff], 0);
        let bytes = readback(&mut r, out, W, H);
        assert_eq!(bytes.len(), (W * H * 4) as usize, "{}: readback size mismatch", def.id);
        engine.end_frame(&mut r);
    }

    engine.clear(&mut r);
    r.destroy_texture(src);
}

fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let mut data = vec![0u8; (w * h * 4) as usize];
    for chunk in data.as_chunks_mut::<4>().0 {
        chunk.copy_from_slice(&rgba);
    }
    data
}

fn echo_trails(decay: f32, mix: f32) -> ResolvedEffect {
    ResolvedEffect {
        def: def_by_id("builtin.video.echo_trails").expect("echo_trails def"),
        params: vec![decay, mix],
    }
}

/// B9 feedback / r.md #8: Echo Trails が **前フレーム出力を実際に次フレームへ持ち越す**ことを
/// pixel で証明する。frame1 を red、frame2 を black で適用する。feedback が無ければ frame2 は
/// black のはず。実際には history (前フレーム=red) が decay して max 合成で残るので frame2 出力は
/// red になる (= 永続 target の維持 + chain 末 blit 退避 + binding 3 sample が機能している証拠)。
/// これにより feedback の核 (フレーム跨ぎの lifecycle) を視覚確認に頼らず自動検証する。
#[test]
fn echo_trails_carries_previous_frame_forward() {
    const W: u32 = 16;
    const H: u32 = 16;
    let Some(mut r) = try_renderer(W, H) else { return };
    let mut engine = VideoFxEngine::new();
    let key = 42_u64;

    // Frame 1: red 入力。history は空 (transparent) なので出力 = src = red。
    let src1 = r.create_texture(W, H);
    r.upload_texture_rgba(src1, &solid(W, H, [255, 0, 0, 255]));
    let out1 = engine.apply_chain(&mut r, src1, W, H, &[echo_trails(0.8, 1.0)], key);
    let b1 = readback(&mut r, out1, W, H);
    assert!(is_red(px(&b1, W, 8, 8)), "frame1 は src(red) のはず: {:?}", px(&b1, W, 8, 8));
    engine.end_frame(&mut r); // pool は解放するが history target は維持される。

    // Frame 2: black 入力。feedback が無ければ black。history(前フレーム=red)*decay が max
    // 合成されるので出力は red trail として残る。
    let src2 = r.create_texture(W, H);
    r.upload_texture_rgba(src2, &solid(W, H, [0, 0, 0, 255]));
    let out2 = engine.apply_chain(&mut r, src2, W, H, &[echo_trails(0.8, 1.0)], key);
    let b2 = readback(&mut r, out2, W, H);
    let p = px(&b2, W, 8, 8);
    assert!(
        p.0 > 100 && p.0 > p.1 + 60 && p.0 > p.2 + 60,
        "frame2 は black 入力でも前フレームの red trail を持ち越すはず (feedback 証明): {p:?}"
    );

    engine.clear(&mut r);
    r.destroy_texture(src1);
    r.destroy_texture(src2);
}
