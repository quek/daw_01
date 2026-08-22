// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

// M14 Phase 78 (daw_01 #049): separable gaussian blur (5-tap linear-sample 最適化)。
// horizontal pass / vertical pass で texel direction を変える 2 entry。
// caller (TextEffectCompositor) が同じ shader module を 2 pipeline で使い分ける。

struct BlurUniform {
    /// texel size in NDC (= 1.0 / texture_size_px)、 + sigma など (使わない slot は pad)
    texel_inv: vec4<f32>, // (1/w, 1/h, _pad, _pad)
    /// linear-sample 用の重み。 weights[0] = center、 weights[1..2] = paired neighbor。
    weights: vec4<f32>,
    /// linear-sample 用の offset (texel 単位)。 offsets[0] は center で 0、 [1..2] = paired neighbor。
    offsets: vec4<f32>,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;
@group(0) @binding(2) var<uniform> u_blur: BlurUniform;

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

// Horizontal separable gaussian (radius ~3-4 で 5 sample = center + 2 paired neighbors)
@fragment
fn fs_blur_h(in: VsOut) -> @location(0) vec4<f32> {
    let texel_x = u_blur.texel_inv.x;
    var acc = textureSample(src_tex, src_sampler, in.uv) * u_blur.weights[0];
    let o1 = vec2<f32>(u_blur.offsets[1] * texel_x, 0.0);
    let o2 = vec2<f32>(u_blur.offsets[2] * texel_x, 0.0);
    acc = acc + textureSample(src_tex, src_sampler, in.uv + o1) * u_blur.weights[1];
    acc = acc + textureSample(src_tex, src_sampler, in.uv - o1) * u_blur.weights[1];
    acc = acc + textureSample(src_tex, src_sampler, in.uv + o2) * u_blur.weights[2];
    acc = acc + textureSample(src_tex, src_sampler, in.uv - o2) * u_blur.weights[2];
    return acc;
}

// Vertical separable gaussian (= H と同じ kernel で direction を y に切替)
@fragment
fn fs_blur_v(in: VsOut) -> @location(0) vec4<f32> {
    let texel_y = u_blur.texel_inv.y;
    var acc = textureSample(src_tex, src_sampler, in.uv) * u_blur.weights[0];
    let o1 = vec2<f32>(0.0, u_blur.offsets[1] * texel_y);
    let o2 = vec2<f32>(0.0, u_blur.offsets[2] * texel_y);
    acc = acc + textureSample(src_tex, src_sampler, in.uv + o1) * u_blur.weights[1];
    acc = acc + textureSample(src_tex, src_sampler, in.uv - o1) * u_blur.weights[1];
    acc = acc + textureSample(src_tex, src_sampler, in.uv + o2) * u_blur.weights[2];
    acc = acc + textureSample(src_tex, src_sampler, in.uv - o2) * u_blur.weights[2];
    return acc;
}
