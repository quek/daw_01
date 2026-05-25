// Instanced textured-quad shader (M14 Phase 71 / daw_01 #043)。
// 入力 instance: pos(left,top,w,h) / uv(uv_min.x, uv_min.y, uv_max.x, uv_max.y) / misc(alpha, _, _, _)
// 出力: texture sample × vec4(rgb, alpha) (standard alpha blend OVER で composite)

struct ScreenUniform {
    size: vec4<f32>,  // (width, height, _, _)
};

@group(0) @binding(0)
var<uniform> screen: ScreenUniform;

@group(1) @binding(0)
var src_tex: texture_2d<f32>;
@group(1) @binding(1)
var src_sampler: sampler;

struct VsIn {
    @location(0) pos:  vec4<f32>,  // left, top, w, h
    @location(1) uv:   vec4<f32>,  // uv_min.x, uv_min.y, uv_max.x, uv_max.y
    @location(2) misc: vec4<f32>,  // alpha, _, _, _
};

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) alpha: f32,
};

fn quad_corner(idx: u32) -> vec2<f32> {
    var t = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    return t[idx];
}

@vertex
fn vs_main(@builtin(vertex_index) vid: u32, in: VsIn) -> VsOut {
    let corner = quad_corner(vid);
    let left = in.pos.x;
    let top = in.pos.y;
    let w = in.pos.z;
    let h = in.pos.w;

    let px = left + corner.x * w;
    let py = top + corner.y * h;

    // 物理ピクセル -> NDC (左上原点 -> 左下原点)
    let ndc_x = (px / screen.size.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (py / screen.size.y) * 2.0;

    let uv_min = in.uv.xy;
    let uv_max = in.uv.zw;
    let uv = uv_min + corner * (uv_max - uv_min);

    var out: VsOut;
    out.clip_pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.uv = uv;
    out.alpha = in.misc.x;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let sample = textureSample(src_tex, src_sampler, in.uv);
    return vec4<f32>(sample.rgb, sample.a * in.alpha);
}
