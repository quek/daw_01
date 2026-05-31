// Instanced textured-quad shader (M14 Phase 71 / daw_01 #043, Phase 76 で rotation 拡張,
// Phase 92 / daw_01 #064 で任意 pivot 拡張)。
// 入力 instance: pos(left,top,w,h) / uv(uv_min.x, uv_min.y, uv_max.x, uv_max.y)
//              / misc(alpha, rotation_radians, pivot_off_x, pivot_off_y)
// 出力: texture sample × vec4(rgb, alpha) (standard alpha blend OVER で composite)
//
// rotation_radians:
//   pivot (cx = left + misc.z, cy = top + misc.w) を旋回中心とした 2D 回転 (clockwise
//   positive in screen-down y)。 0.0 で sin=0/cos=1 の恒等変換 → 既存挙動と完全互換。
//   NaN / ±Infinity は CPU 側 (`enqueue_run`) で 0.0 に正規化済。
//   pivot_off_x / pivot_off_y: rect 左上相対の旋回中心 px offset。 CPU 側で
//   `rotation_pivot == None` のとき `(w/2, h/2)` を書くので Phase 76 の rect 中心 pivot と
//   byte 完全互換。
//   非 0 の場合は pixel 空間で `[cos -sin; sin cos]` 行列を適用 (= normalized 空間で
//   回転すると non-square rect w≠h で aspect 歪みが起こるため、 必ず pixel 空間で実施)。
//   UV mapping は rotation 適用前の corner で計算する (= texture content は rect 4 隅に
//   "stuck" し、 rect 自体が rigid に回転する After Effects / Premiere セマンティクス)。

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
    @location(2) misc: vec4<f32>,  // alpha, rotation_radians, pivot_off_x, pivot_off_y
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

    // 1) 未回転の pixel 座標を計算。
    let px0 = left + corner.x * w;
    let py0 = top + corner.y * h;

    // 2) pivot (rect 左上 + misc.zw offset) を基準に rotation 行列を適用 (pixel 空間 = aspect 維持)。
    //    theta = 0 のときは sin=0/cos=1 で恒等変換 → Phase 71 と byte 完全互換。
    //    misc.zw は CPU 側で None のとき (w/2, h/2) を書くので中心 pivot の既存挙動と互換。
    let theta = in.misc.y;
    let cx = left + in.misc.z;
    let cy = top + in.misc.w;
    let rel_x = px0 - cx;
    let rel_y = py0 - cy;
    let s = sin(theta);
    let co = cos(theta);
    let px = cx + rel_x * co - rel_y * s;
    let py = cy + rel_x * s + rel_y * co;

    // 3) 物理ピクセル -> NDC (左上原点 -> 左下原点)。
    let ndc_x = (px / screen.size.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (py / screen.size.y) * 2.0;

    // 4) UV は rotation 適用前の corner で計算 (texture content は rect 4 隅に "stuck")。
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
