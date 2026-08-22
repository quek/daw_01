// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

// M14 Phase 78 (daw_01 #049): text effect composite shader。
// shadow (blurred separable gaussian 済) + outline (9-sample max α dilate) + fill (glyph α)
// を 1 fragment pass で合成。

struct CompositeUniform {
    /// (texture_size_px.x, texture_size_px.y, _pad, _pad)
    tex_size: vec4<f32>,
    /// (outline_width_px, _pad, _pad, _pad)
    outline: vec4<f32>,
    /// outline_color (rgba 0..1)
    outline_color: vec4<f32>,
    /// (shadow_offset_px.x, shadow_offset_px.y, _pad, _pad)
    shadow_offset: vec4<f32>,
    /// shadow_color (rgba 0..1)
    shadow_color: vec4<f32>,
};

@group(0) @binding(0) var glyph_tex: texture_2d<f32>;
@group(0) @binding(1) var glyph_sampler: sampler;
@group(0) @binding(2) var shadow_tex: texture_2d<f32>;
@group(0) @binding(3) var shadow_sampler: sampler;
@group(0) @binding(4) var<uniform> u_comp: CompositeUniform;

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// 共通: フルスクリーン quad (6 vertex triangle list)。
@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
    );
    var uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 0.0),
    );
    var out: VsOut;
    out.clip_pos = vec4<f32>(positions[vid], 0.0, 1.0);
    out.uv = uvs[vid];
    return out;
}

// standard alpha OVER blend (= src on top of dst)
fn over(src: vec4<f32>, dst: vec4<f32>) -> vec4<f32> {
    let a_out = src.a + dst.a * (1.0 - src.a);
    if (a_out <= 0.0) {
        return vec4<f32>(0.0);
    }
    let rgb_out = (src.rgb * src.a + dst.rgb * dst.a * (1.0 - src.a)) / a_out;
    return vec4<f32>(rgb_out, a_out);
}

@fragment
fn fs_composite(in: VsOut) -> @location(0) vec4<f32> {
    let glyph_center = textureSample(glyph_tex, glyph_sampler, in.uv);
    let outline_w = u_comp.outline.x;
    let tex_w = u_comp.tex_size.x;
    let tex_h = u_comp.tex_size.y;

    // === Layer 1: Shadow ===
    // shadow_tex は別 pass で blur 済の glyph mask。 shadow_offset_px の値分 uv を逆方向にずらして
    // sampling すると、 結果的に shadow texture が画面上で +offset 方向に shift される。
    let offset_uv = vec2<f32>(u_comp.shadow_offset.x / tex_w, u_comp.shadow_offset.y / tex_h);
    let shadow_uv = in.uv - offset_uv;
    var shadow_sample = vec4<f32>(0.0);
    if (shadow_uv.x >= 0.0 && shadow_uv.x <= 1.0 && shadow_uv.y >= 0.0 && shadow_uv.y <= 1.0) {
        let shadow_alpha = textureSample(shadow_tex, shadow_sampler, shadow_uv).a;
        shadow_sample = vec4<f32>(u_comp.shadow_color.rgb, u_comp.shadow_color.a * shadow_alpha);
    }

    // === Layer 2: Outline (= 8 方向 + 中心 sample で max α dilate、 中心領域は除外) ===
    var outline_alpha = 0.0;
    if (outline_w > 0.0) {
        let texel_uv = vec2<f32>(outline_w / tex_w, outline_w / tex_h);
        var max_a = glyph_center.a;
        // 8 方向 sample (cardinal + diagonal)
        max_a = max(max_a, textureSample(glyph_tex, glyph_sampler, in.uv + vec2<f32>( texel_uv.x, 0.0)).a);
        max_a = max(max_a, textureSample(glyph_tex, glyph_sampler, in.uv + vec2<f32>(-texel_uv.x, 0.0)).a);
        max_a = max(max_a, textureSample(glyph_tex, glyph_sampler, in.uv + vec2<f32>(0.0,  texel_uv.y)).a);
        max_a = max(max_a, textureSample(glyph_tex, glyph_sampler, in.uv + vec2<f32>(0.0, -texel_uv.y)).a);
        let d = 0.70710677;
        max_a = max(max_a, textureSample(glyph_tex, glyph_sampler, in.uv + vec2<f32>( texel_uv.x * d,  texel_uv.y * d)).a);
        max_a = max(max_a, textureSample(glyph_tex, glyph_sampler, in.uv + vec2<f32>(-texel_uv.x * d,  texel_uv.y * d)).a);
        max_a = max(max_a, textureSample(glyph_tex, glyph_sampler, in.uv + vec2<f32>( texel_uv.x * d, -texel_uv.y * d)).a);
        max_a = max(max_a, textureSample(glyph_tex, glyph_sampler, in.uv + vec2<f32>(-texel_uv.x * d, -texel_uv.y * d)).a);
        outline_alpha = max(0.0, max_a - glyph_center.a);
    }
    let outline_sample = vec4<f32>(u_comp.outline_color.rgb, u_comp.outline_color.a * outline_alpha);

    // === Layer 3: Fill (= glyph mask 中心) ===
    let fill_sample = glyph_center;

    // === z-order: shadow under outline under fill ===
    var result = shadow_sample;
    result = over(outline_sample, result);
    result = over(fill_sample, result);
    return result;
}
