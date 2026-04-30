// Instanced rounded-rect shader.
// 入力 instance: pos(left,top,w,h) / fill / border / (border_w,r_tl,r_tr,r_br) / (r_bl,pad,pad,pad)
// 出力: 角丸 + ボーダー描画

struct ScreenUniform {
    size: vec4<f32>,  // (width, height, _, _)
};

@group(0) @binding(0)
var<uniform> screen: ScreenUniform;

struct VsIn {
    @location(0) pos:    vec4<f32>,  // left, top, w, h
    @location(1) fill:   vec4<f32>,
    @location(2) border: vec4<f32>,
    @location(3) misc0:  vec4<f32>,  // border_w, r_tl, r_tr, r_br
    @location(4) misc1:  vec4<f32>,  // r_bl, _, _, _
};

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) local_uv: vec2<f32>,    // 矩形ローカル座標 [0,w] x [0,h]
    @location(1) size:     vec2<f32>,    // (w, h)
    @location(2) fill:     vec4<f32>,
    @location(3) border:   vec4<f32>,
    @location(4) border_w: f32,
    @location(5) r_tl:     f32,
    @location(6) r_tr:     f32,
    @location(7) r_br:     f32,
    @location(8) r_bl:     f32,
};

// 6 頂点で矩形を 2 三角形に展開。
fn quad_corner(idx: u32) -> vec2<f32> {
    // (0,0)(1,0)(0,1) / (1,0)(1,1)(0,1)
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

    var out: VsOut;
    out.clip_pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.local_uv = vec2<f32>(corner.x * w, corner.y * h);
    out.size = vec2<f32>(w, h);
    out.fill = in.fill;
    out.border = in.border;
    out.border_w = in.misc0.x;
    out.r_tl = in.misc0.y;
    out.r_tr = in.misc0.z;
    out.r_br = in.misc0.w;
    out.r_bl = in.misc1.x;
    return out;
}

// 矩形 (size) 内の点 p における「角丸表面までの符号付き距離」。
// 正なら矩形外、負なら矩形内、0 が境界。
fn rounded_box_sdf(p: vec2<f32>, size: vec2<f32>, r_tl: f32, r_tr: f32, r_br: f32, r_bl: f32) -> f32 {
    // p は矩形左上原点 [0,size]
    // 各角に応じた半径を選ぶ
    let cx = select(0.0, 1.0, p.x > size.x * 0.5);  // 0=left, 1=right
    let cy = select(0.0, 1.0, p.y > size.y * 0.5);  // 0=top,  1=bottom
    var r: f32 = 0.0;
    if (cy < 0.5 && cx < 0.5)      { r = r_tl; }
    else if (cy < 0.5 && cx >= 0.5) { r = r_tr; }
    else if (cy >= 0.5 && cx >= 0.5){ r = r_br; }
    else                             { r = r_bl; }

    // 中心原点に変換
    let center = size * 0.5;
    let q = abs(p - center) - (center - vec2<f32>(r, r));
    let outside = length(max(q, vec2<f32>(0.0))) ;
    let inside = min(max(q.x, q.y), 0.0);
    return outside + inside - r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let d = rounded_box_sdf(in.local_uv, in.size, in.r_tl, in.r_tr, in.r_br, in.r_bl);

    // アンチエイリアス幅 (1 ピクセル目安)
    let aa = 1.0;

    // fill alpha: SDF<=0 で 1, 境界付近で smoothstep
    let fill_alpha = 1.0 - smoothstep(-aa, 0.0, d);
    var color = in.fill * fill_alpha;

    // ボーダー: 境界 d=0 から内側 -border_w までを線で塗る
    if (in.border_w > 0.0 && in.border.a > 0.0) {
        let bw = in.border_w;
        // 帯の中心 d = -bw/2、幅 bw
        let band = abs(d + bw * 0.5);
        let border_alpha = 1.0 - smoothstep(bw * 0.5 - aa, bw * 0.5, band);
        // ボーダーは fill に上書き
        color = mix(color, in.border, border_alpha * in.border.a);
    }

    return color;
}
