// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! テキスト描画が抱える GPU リソースが、長時間セッションで単調増加しないことの回帰テスト
//! (daw_01 r.md #59)。
//!
//! # 対象は 2 つの独立した資産
//!
//! **(1) glyph atlas** — glyphon 0.11 の `TextAtlas` は「毎フレーム `trim()` を呼ぶ」 前提で
//! 設計されている (upstream の `examples/hello-world.rs:255` / `examples/text-sizes.rs:327` /
//! `examples/custom-glyphs.rs:355` / `benches/prepare.rs:114` がいずれも `queue.submit` の
//! 直後に呼ぶ)。 呼ばないと `InnerAtlas::glyphs_in_use` (`text_atlas.rs:26`) が **一度描いた
//! 全 glyph cache key を永久に保持** し、 `try_allocate` (`text_atlas.rs:72-104`) は in-use な
//! glyph を LRU から追い出せず `None` を返す → `TextRenderer::prepare` が `atlas.grow()` を
//! 呼び、 atlas texture が 256 → 512 → … → `max_texture_dimension_2d` (この renderer は
//! `wgpu::Limits::default()` なので 8192) まで倍々に膨らむ。 mask atlas (R8) 1 枚で 64 MiB、
//! 到達後は `PrepareError::AtlasFull` で文字が出なくなる。
//!
//! **(2) effect 済 composite texture** — `TextEffectCompositor` は outline / shadow 付き text を
//! offscreen に焼いて `EffectKey` (text 内容 + font size + effect パラメータ) でキャッシュする。
//! フレーム数 TTL しか無いと、 鍵空間が無制限なのに 1 枚が最大 4096×4096×4 B まで振れるため、
//! 300 フレームぶんの巨大テクスチャが同時に生き残る。
//!
//! どちらも引き金は同じ: font size が連続値で決まる操作。 piano_roll の歌詞は `note_h * 0.75`
//! (縦ズーム・縦スクロールで毎フレーム変化)、 字幕は preview 窓幅 / project 幅のスケール
//! (窓リサイズや FontSize 変調で毎フレーム変化)。 cosmic-text の `CacheKey` は font_size を
//! 生ビットで持つので、 これらは全て別 glyph・別 `EffectKey` になる。
//!
//! # 何を測るか
//!
//! `wgpu::Device::generate_allocator_report()` (wgpu 29 の DX12 / Vulkan で実装済) が返す
//! `allocations[]` を **ラベルで絞って** 合計する。 `total_allocated_bytes` 全体だと
//! 「どちらの資産が増えたのか」 が判らず、 片方の修正を消しても閾値内に収まってしまう。

use daw_ui_renderer::{Color, GlyphArea, OffscreenRenderer, Scene};

/// glyphon が atlas texture に付ける wgpu ラベル。
const LABEL_GLYPH_ATLAS: &str = "glyphon atlas";
/// `TextureStore::create_render_target` が付ける wgpu ラベル
/// (= effect 済 composite texture の実体)。
const LABEL_RENDER_TARGET: &str = "texture pool entry (render target)";

/// zoom 操作を再現するために描くフレーム数 (毎フレーム別 font size)。
const FRAMES: usize = 400;

/// glyph atlas の許容増加量。 実測 (2026-08-16, D3D12): trim 無しでは 256² → 4096² まで
/// grow して +16 MiB、 trim 有りでは 1 フレームの working set が 256²〜512² に収まるので
/// ほぼ 0。 4 MiB は「1 段でも grow したら落ちる」 が「driver 差の端数では落ちない」 閾値。
const MAX_ATLAS_GROWTH: u64 = 4 * 1024 * 1024;

/// composite texture cache の許容量。 `COMPOSITE_CACHE_BUDGET_BYTES` (32 MiB) は texel 実バイト
/// (w×h×4) で数えるが、 allocator は row pitch / placement alignment のぶん上乗せして確保する
/// (実測で約 1.4 倍)。 64 MiB はその上乗せを飲み込みつつ、 予算が効いていない状態
/// (実測 176 MiB / 300 枚、 しかもテキストが長いほど増える) は確実に弾く。
const MAX_COMPOSITE_BYTES: u64 = 64 * 1024 * 1024;

/// eviction 前後で同一性を確認する基準テキスト。
const PROBE_SIZE: f32 = 24.0;
const PROBE_TEXT: &str = "あA五";

/// GPU adapter が無い環境 (headless CI 等) では skip する。
fn try_renderer(w: u32, h: u32) -> Option<OffscreenRenderer> {
    match OffscreenRenderer::new(w, h) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("skip glyph atlas GPU test: no adapter/device ({e})");
            None
        }
    }
}

/// 指定ラベルの GPU アロケーション合計バイト数。 report 非対応バックエンドでは `None`。
///
/// ラベルが変わって 0 になると test が無言で通ってしまうので、 呼び出し側は
/// 「baseline が 0 でない」 ことを必ず確認すること。
fn gpu_bytes_labeled(r: &OffscreenRenderer, label: &str) -> Option<u64> {
    r.device().generate_allocator_report().map(|rep| {
        rep.allocations.iter().filter(|a| a.name == label).map(|a| a.size).sum()
    })
}

/// effect 無しテキストを 1 フレーム描く (= base `GlyphPipeline` の atlas を使う)。
fn draw_text_frame(r: &mut OffscreenRenderer, font_size: f32, text: &str) {
    let mut scene = Scene::new();
    scene.clear_color = Color::BLACK.to_wgpu();
    scene.push_text(GlyphArea::new(
        text.into(),
        0.0,
        0.0,
        font_size,
        font_size * 1.2,
        Color::WHITE,
    ));
    r.render_to_rgba(&scene).expect("render");
}

/// アウトライン付きテキストを 1 フレーム描く (= `TextEffectCompositor` の atlas +
/// composite texture cache を使う)。
fn draw_outlined_text_frame(r: &mut OffscreenRenderer, font_size: f32, text: &str) {
    let mut scene = Scene::new();
    scene.clear_color = Color::BLACK.to_wgpu();
    let mut area = GlyphArea::new(text.into(), 0.0, 0.0, font_size, font_size * 1.2, Color::WHITE);
    area.outline_color = Color::rgb(1.0, 0.0, 0.0);
    area.outline_width_px = 2.0;
    scene.push_text(area);
    r.render_to_rgba(&scene).expect("render");
}

/// font size を連続的に変えながら [`FRAMES`] フレーム描いて atlas を埋め回す (= zoom 操作)。
fn churn_atlas(r: &mut OffscreenRenderer) {
    for i in 0..FRAMES {
        draw_text_frame(r, 8.0 + (i as f32) * 0.37, &format!("歌詞テキスト{i} Lyric{i}"));
    }
}

/// 非黒 pixel 数 (= 文字の «インク»)。
fn ink_pixels(bytes: &[u8]) -> usize {
    bytes.chunks_exact(4).filter(|p| p[0] > 32 || p[1] > 32 || p[2] > 32).count()
}

/// base `GlyphPipeline` の atlas: zoom 相当で font size を散らしても GPU 確保量が
/// 単調増加しない (= `GlyphPipeline::end_frame` の `atlas.trim()` が効いている)。
#[test]
fn glyph_atlas_stays_bounded_across_many_font_sizes() {
    let Some(mut r) = try_renderer(64, 64) else { return };

    // 1 フレーム描いて atlas の初期確保を済ませる。
    draw_text_frame(&mut r, 12.0, "あA");
    let Some(baseline) = gpu_bytes_labeled(&r, LABEL_GLYPH_ATLAS) else {
        eprintln!("skip: backend does not implement generate_allocator_report");
        return;
    };
    assert!(baseline > 0, "ラベル {LABEL_GLYPH_ATLAS} の確保が見つからない (glyphon 側でラベルが変わった?)");

    churn_atlas(&mut r);

    let after = gpu_bytes_labeled(&r, LABEL_GLYPH_ATLAS).expect("report available");
    let grown = after.saturating_sub(baseline);
    assert!(
        grown < MAX_ATLAS_GROWTH,
        "glyph atlas が {FRAMES} フレームで {} MiB 増えた ({} B → {} B)。 \
         GlyphPipeline::end_frame の atlas.trim() が効いていない",
        grown / (1024 * 1024),
        baseline,
        after,
    );
}

/// trim を入れると、 これまで一度も起きていなかった **atlas eviction** が実際に走るようになる。
/// evict された glyph が次に必要になったとき正しく再ラスタライズ・再アップロードされることを
/// pixel で確認する (= 「メモリは減ったが文字が化けた」 を防ぐ)。
#[test]
fn text_renders_identically_after_atlas_eviction() {
    let Some(mut r) = try_renderer(96, 48) else { return };

    let before = render_probe(&mut r);
    let ink_before = ink_pixels(&before);
    assert!(ink_before > 0, "基準テキストが 1 pixel も描かれていない (font 解決失敗?)");

    // atlas を埋め回して基準 glyph を LRU から追い出す。
    churn_atlas(&mut r);

    let after = render_probe(&mut r);
    assert_eq!(
        ink_pixels(&after),
        ink_before,
        "atlas eviction を挟むと同じテキストの描画結果が変わった (再アップロードが壊れている)"
    );
    assert_eq!(after, before, "atlas eviction 後の pixel が bit 単位で一致しない");
}

/// 基準テキストを描いて RGBA bytes を返す。
fn render_probe(r: &mut OffscreenRenderer) -> Vec<u8> {
    let mut scene = Scene::new();
    scene.clear_color = Color::BLACK.to_wgpu();
    scene.push_text(GlyphArea::new(
        PROBE_TEXT.into(),
        0.0,
        0.0,
        PROBE_SIZE,
        PROBE_SIZE * 1.2,
        Color::WHITE,
    ));
    r.render_to_rgba(&scene).expect("render")
}

/// `TextEffectCompositor` は base とは **別の `TextAtlas`** を持つので trim も独立に必要。
/// base 側の test は effect 経路を一切通らないため、 片方の trim を消しても落ちない。
#[test]
fn text_effect_atlas_stays_bounded_across_many_font_sizes() {
    let Some(mut r) = try_renderer(128, 64) else { return };

    draw_outlined_text_frame(&mut r, 12.0, "あA");
    let Some(baseline) = gpu_bytes_labeled(&r, LABEL_GLYPH_ATLAS) else {
        eprintln!("skip: backend does not implement generate_allocator_report");
        return;
    };
    assert!(baseline > 0, "ラベル {LABEL_GLYPH_ATLAS} の確保が見つからない");

    for i in 0..FRAMES {
        draw_outlined_text_frame(&mut r, 8.0 + (i as f32) * 0.37, &format!("字幕{i} Sub{i}"));
    }

    let after = gpu_bytes_labeled(&r, LABEL_GLYPH_ATLAS).expect("report available");
    let grown = after.saturating_sub(baseline);
    assert!(
        grown < MAX_ATLAS_GROWTH,
        "text_effect の atlas が {FRAMES} フレームで {} MiB 増えた ({} B → {} B)。 \
         TextEffectCompositor::end_frame の atlas.trim() が効いていない",
        grown / (1024 * 1024),
        baseline,
        after,
    );
}

/// effect 済 composite texture のキャッシュがバイト予算で頭打ちになる。
/// フレーム数 TTL (`EVICT_AFTER_FRAMES`) だけだと 300 枚ぶんが同時に生き残り、
/// テキストが長い / font が大きいほど青天井に増える (実測 176 MiB)。
#[test]
fn text_effect_composite_cache_respects_byte_budget() {
    let Some(mut r) = try_renderer(128, 64) else { return };

    draw_outlined_text_frame(&mut r, 12.0, "あA");
    let Some(baseline) = gpu_bytes_labeled(&r, LABEL_RENDER_TARGET) else {
        eprintln!("skip: backend does not implement generate_allocator_report");
        return;
    };
    assert!(
        baseline > 0,
        "ラベル {LABEL_RENDER_TARGET} の確保が見つからない (TextureStore 側でラベルが変わった?)"
    );

    for i in 0..FRAMES {
        draw_outlined_text_frame(&mut r, 8.0 + (i as f32) * 0.37, &format!("字幕{i} Sub{i}"));
    }
    // `TextureStore::destroy` は wgpu::Texture を drop するだけで、 実際の GPU 解放は次の
    // submit の maintain で起きる。 最終フレームぶんを反映させるため数フレーム空回しする
    // (同じ内容なので cache hit = 新規確保なし)。
    for _ in 0..3 {
        draw_outlined_text_frame(&mut r, 12.0, "あA");
    }

    let after = gpu_bytes_labeled(&r, LABEL_RENDER_TARGET).expect("report available");
    assert!(
        after < MAX_COMPOSITE_BYTES,
        "effect composite cache が {} MiB を保持している。 \
         TextEffectCompositor::enforce_cache_budget が効いていない",
        after / (1024 * 1024),
    );
}
