// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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

// AA fade 領域を geometry の外側に確保する幅 (px)。 これが無いと矩形の境界を
// またぐピクセルは fragment がそもそも raster されず、 被覆率 (= 部分的な alpha)
// を出せない。 line.wgsl:51-54 が「bar grid の特定 bar だけ消える」 症状の
// root cause として同じ拡張を既に持っている。 rect だけ取り残されていたのが
// 「オートスクロールでクリップの左右がチラつく」 (r.md #53) の主因。
const AA_PAD: f32 = 1.0;

@vertex
fn vs_main(@builtin(vertex_index) vid: u32, in: VsIn) -> VsOut {
    let corner = quad_corner(vid);
    let left = in.pos.x;
    let top = in.pos.y;
    let w = in.pos.z;
    let h = in.pos.w;

    // quad は矩形より各辺 AA_PAD だけ大きい。 local_uv は矩形基準のまま
    // (= [-AA_PAD, size + AA_PAD]) に保つので SDF の意味は変わらない。
    let ex_w = w + 2.0 * AA_PAD;
    let ex_h = h + 2.0 * AA_PAD;
    let px = left - AA_PAD + corner.x * ex_w;
    let py = top - AA_PAD + corner.y * ex_h;

    // 物理ピクセル -> NDC (左上原点 -> 左下原点)
    let ndc_x = (px / screen.size.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (py / screen.size.y) * 2.0;

    var out: VsOut;
    out.clip_pos = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.local_uv = vec2<f32>(corner.x * ex_w - AA_PAD, corner.y * ex_h - AA_PAD);
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

// 符号付き距離 d (px、 内側が負) の位置にある 1 ピクセルの被覆率。
// `clamp(0.5 - d, 0, 1)` は直線エッジに対する 1px box filter の厳密解で、
// 隣り合う 2 ピクセルの被覆の和が常に 1 になる (= インク保存)。 旧実装の
// `1 - smoothstep(-1, 0, d)` は「図形の内側だけ」 の片側ランプで、 (a) 半ピクセル
// 内側にずれる、 (b) 幅 1px の帯では位相によって総インクが 0 〜 0.5 に振動する、
// の 2 つの欠陥があった。
fn coverage(d: f32) -> f32 {
    return clamp(0.5 - d, 0.0, 1.0);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let d = rounded_box_sdf(in.local_uv, in.size, in.r_tl, in.r_tr, in.r_br, in.r_bl);

    // 図形全体の被覆と、 ボーダーの内側 (= fill だけの領域) の被覆。 差がボーダー帯の
    // 被覆になる。 この差分方式なら任意の位相・任意の border_w で帯の総インクが
    // border_w に厳密一致する (border_w = 0 なら自動的に 0 なので分岐も要らない)。
    let a_shape = coverage(d);
    let a_inner = coverage(d + max(in.border_w, 0.0));
    let a_band = max(a_shape - a_inner, 0.0);

    // 帯の中では border が fill の上に乗る (旧実装の mix と同じ意味)。
    let ba = in.border.a;
    let fa = in.fill.a;
    let band_a = ba + fa * (1.0 - ba);
    let band_rgb = in.border.rgb * ba + in.fill.rgb * fa * (1.0 - ba);

    // 帯と内側は幾何的に排他なので、 被覆で重み付けして足せばよい。 出力は
    // premultiplied alpha (pipeline 側 blend も PREMULTIPLIED_ALPHA_BLENDING)。
    // 旧実装は premultiplied な色を straight alpha の blend に流していたため、
    // エッジ 1 列が被覆率の 2 乗で暗くなっていた。
    let out_rgb = band_rgb * a_band + in.fill.rgb * fa * a_inner;
    let out_a = band_a * a_band + fa * a_inner;
    return vec4<f32>(out_rgb, out_a);
}
