// Line segment パイプライン。
// 1 segment = 1 instance、6 頂点を頂点シェーダで quad に展開する。
// 入力 instance: a (start), b (end), color, line_width。
// 主用途: 波形 (PeakLines)、メータ、グリッド、オートメーション (CPU flatten 後)。

struct ScreenUniform {
    size: vec4<f32>,  // (width, height, _, _)
};

@group(0) @binding(0)
var<uniform> screen: ScreenUniform;

struct VsIn {
    @location(0) a: vec2<f32>,           // start (px)
    @location(1) b: vec2<f32>,           // end (px)
    @location(2) color: vec4<f32>,
    @location(3) line_width: f32,
};

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    /// 線中心からの perpendicular 距離 (px、符号付き)
    @location(1) edge_dist: f32,
    /// 線幅の半分 (px)
    @location(2) half_width: f32,
};

// 6 頂点で quad に展開:
//   vid 0: a side=-1 along=0   (start, "left" of line direction)
//   vid 1: b side=-1 along=1   (end,   "left")
//   vid 2: a side=+1 along=0   (start, "right")
//   vid 3: b side=-1 along=1   (end,   "left")
//   vid 4: b side=+1 along=1   (end,   "right")
//   vid 5: a side=+1 along=0   (start, "right")

@vertex
fn vs_main(@builtin(vertex_index) vid: u32, in: VsIn) -> VsOut {
    var along_table = array<f32, 6>(0.0, 1.0, 0.0, 1.0, 1.0, 0.0);
    var side_table  = array<f32, 6>(-1.0, -1.0, 1.0, -1.0, 1.0, 1.0);
    let along = along_table[vid];
    let side  = side_table[vid];

    let diff = in.b - in.a;
    let len  = max(length(diff), 1e-4);
    let dir  = diff / len;
    // 2D perpendicular (右手系で +90deg 回転)
    let perp = vec2<f32>(-dir.y, dir.x);

    let half_w = max(in.line_width * 0.5, 0.5);
    let center = mix(in.a, in.b, along);
    let pos_px = center + perp * (side * half_w);

    // 物理ピクセル -> NDC (左上原点 -> NDC は左下原点)
    let ndc_x = (pos_px.x / screen.size.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (pos_px.y / screen.size.y) * 2.0;

    var out: VsOut;
    out.clip_pos   = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.color      = in.color;
    out.edge_dist  = side * half_w;
    out.half_width = half_w;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // 線中心からの距離で 1px のアンチエイリアス。
    let aa = 1.0;
    let abs_dist = abs(in.edge_dist);
    let alpha = 1.0 - smoothstep(in.half_width - aa, in.half_width, abs_dist);
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
