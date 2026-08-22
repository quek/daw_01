// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! M14 Phase 133 (daw_01 #111): 映像効果フレームワーク用 texture interop primitive の GPU pixel verify。
//!
//! daw_01 が映像効果チェーンで使う 2 primitive を、 **実際に自前の effect pipeline を組んで** 合成し
//! pixel 単位で検証する (= 単なる passthrough コンパイル確認でなく、 daw_01 が通す経路そのものを通す):
//! - `raw_texture(src)` で合成済 / 動画フレーム texture を **自前の effect bind group** に bind する入力経路
//! - `create_render_target(w, h, fmt)` で得た `(handle, view)` に効果 pass を描画する出力経路
//! - 効果 pass を **自前 encoder で submit してから** `render_to_rgba` を呼ぶ submit 順序の契約
//! - 最終 handle を `push_textured_quad` で base scene に戻して sample (= texture pipeline 既存経路)
//!
//! GPU adapter が無い環境 (headless CI 等) では `OffscreenRenderer::new` が `Err` を返すので graceful skip。
//! memory: feedback_no_excuse_pixel_verify (「動く」 で済まさず pixel 単位で確認)。

use daw_ui_renderer::{wgpu, Color, OffscreenRenderer, Rect, Scene, TexturedQuad};

/// GPU が無ければ skip するための helper。
fn try_renderer(w: u32, h: u32) -> Option<OffscreenRenderer> {
    match OffscreenRenderer::new(w, h) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("skip interop GPU test: no adapter/device ({e})");
            None
        }
    }
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

/// daw_01 の effect pass の最小代理: src texture を fullscreen で sample して target にそのまま書き戻す
/// 「blit」 pipeline。 gui_01 は「効果とは何か」 を知らず、 daw_01 がこのように **自前の layout / shader /
/// sampler** を組んで `raw_texture` / `create_render_target` を使う、 という想定経路を実体化する。
struct BlitPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl BlitPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("interop blit shader"),
            source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("interop blit bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("interop blit pl"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("interop blit pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("interop blit sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self { pipeline, layout, sampler }
    }
}

const BLIT_WGSL: &str = r"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
    // fullscreen triangle (3 頂点で画面全域を覆う)。
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var u = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var o: VsOut;
    o.pos = vec4<f32>(p[vi], 0.0, 1.0);
    o.uv = u[vi];
    return o;
}

@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(t, s, in.uv);
}
";

/// 左半分 red / 右半分 blue で塗った `w*h*4` RGBA8 bytes (= 合成済トラック画 / 動画フレームの代理)。
/// 一色でなく 2 色にするのは、 effect pass が「定数を吐く」 のでなく **実際に src を sample している**
/// ことを左右の色保存で示すため。
fn red_left_blue_right(w: u32, h: u32) -> Vec<u8> {
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            if x < w / 2 {
                data[i] = 255; // R
            } else {
                data[i + 2] = 255; // B
            }
            data[i + 3] = 255; // A
        }
    }
    data
}

/// 効果フレームワークの全経路を 1 本通す:
/// `create_texture` で src を upload → `raw_texture(src)` を自前 bind group に bind →
/// `create_render_target` の view に効果 pass (blit) を描画 → 効果 encoder を submit →
/// 最終 handle を `push_textured_quad` → `render_to_rgba`。 左 red / 右 blue が保存されれば、
/// raw_texture の sample と create_render_target への描画が正しく合成され、 submit 順序も安全。
#[test]
fn effect_pass_round_trips_via_raw_texture_and_render_target() {
    const W: u32 = 16;
    const H: u32 = 16;
    let Some(mut r) = try_renderer(W, H) else { return };
    let fmt = r.target_format();

    // src: 効果 pass の入力 (= daw_01 では composite_scene_to_texture の戻り or 動画フレーム handle)。
    let src = r.create_texture(W, H);
    r.upload_texture_rgba(src, &red_left_blue_right(W, H));

    // rt: 効果 pass の出力先 (RENDER_ATTACHMENT | TEXTURE_BINDING)。
    let (rt, rt_view) = r.create_render_target(W, H, fmt);

    // daw_01 が組む自前 effect pipeline (gui_01 は中身を知らない)。
    let blit = BlitPipeline::new(r.device(), fmt);

    // raw_texture(src) を自前 layout に bind。 借用衝突回避のため texture を clone してから view を作る
    // (= ドキュメント記載の「raw_texture(h).cloned() で所有権を取る」 運用を踏襲)。
    let src_tex = r.raw_texture(src).expect("src must be live").clone();
    let src_view = src_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let bind = r.device().create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("interop blit src bg"),
        layout: &blit.layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&src_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&blit.sampler),
            },
        ],
    });

    // 効果 pass を **自前 encoder に積んで submit** (= 契約: render_to_rgba より前の別 submit)。
    let mut encoder = r.device().create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("interop effect encoder"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("interop effect pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &rt_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&blit.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
    }
    r.queue().submit(std::iter::once(encoder.finish()));

    // 最終 handle を base scene に push して render (= 別 submit、 契約どおり effect submit の後)。
    let mut base = Scene::new();
    base.clear_color = Color::BLACK.to_wgpu();
    base.push_textured_quad(TexturedQuad::new(
        Rect::new(0.0, 0.0, W as f32, H as f32),
        rt,
    ));
    let bytes = r.render_to_rgba(&base).expect("render base scene");

    assert!(
        is_red(px(&bytes, W, 4, 8)),
        "左半分が red でない (raw_texture sample または create_render_target 描画が失敗?): {:?}",
        px(&bytes, W, 4, 8)
    );
    assert!(
        is_blue(px(&bytes, W, 12, 8)),
        "右半分が blue でない (定数吐きでなく実 sample できているか): {:?}",
        px(&bytes, W, 12, 8)
    );

    // rt は caller 管理なので明示 destroy。
    r.destroy_texture(rt);
    r.destroy_texture(src);
}

/// 履歴 (feedback) target が render cycle を跨いで生存する: `create_render_target` の handle は
/// caller 管理なので、 `render_to_rgba` 冒頭の composite pool eviction (`end_cycle`) に **destroy されない**。
/// 前 frame で焼いた内容を今 frame で再描画せず sample できる (= フィードバック効果の前提)。
#[test]
fn render_target_survives_render_cycle_for_history_feedback() {
    const W: u32 = 16;
    const H: u32 = 16;
    let Some(mut r) = try_renderer(W, H) else { return };
    let fmt = r.target_format();

    // 履歴 target を 1 枚確保し red を焼く (前 frame の効果出力の代理)。
    let (hist, hist_view) = r.create_render_target(W, H, fmt);
    {
        let mut encoder = r.device().create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("history fill encoder"),
        });
        // clear だけの pass (内容を red にする)。 inner block の終端で pass が drop されて終了し、
        // その後で encoder.finish() できる。
        {
            let _clear_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("history fill pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &hist_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::RED),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        r.queue().submit(std::iter::once(encoder.finish()));
    }

    // cycle 1: 別 scene を render (end_cycle = composite pool eviction が走る)。
    let mut s1 = Scene::new();
    s1.clear_color = Color::BLACK.to_wgpu();
    let _ = r.render_to_rgba(&s1).expect("render cycle 1");

    // cycle 2: hist を **再描画せず** push して sample。 destroy されていれば描画 skip で黒くなる。
    let mut s2 = Scene::new();
    s2.clear_color = Color::BLACK.to_wgpu();
    s2.push_textured_quad(TexturedQuad::new(
        Rect::new(0.0, 0.0, W as f32, H as f32),
        hist,
    ));
    let bytes = r.render_to_rgba(&s2).expect("render cycle 2");

    assert!(
        is_red(px(&bytes, W, 8, 8)),
        "履歴 target が render cycle を跨いで生存していない (end_cycle が caller handle を destroy した?): {:?}",
        px(&bytes, W, 8, 8)
    );

    r.destroy_texture(hist);
    assert!(
        r.texture_size(hist).is_none(),
        "destroy 後も handle が live のまま"
    );
}

/// `raw_texture` / `create_render_target` の lifecycle と metadata: create 直後は live で size/format が
/// 正しく、 `destroy_texture` 後は `raw_texture` / `texture_size` ともに `None`。 size 0 は 1 に clamp。
#[test]
fn raw_texture_and_render_target_lifecycle() {
    const W: u32 = 8;
    const H: u32 = 4;
    let Some(mut r) = try_renderer(W, H) else { return };
    let fmt = r.target_format();

    let (rt, _view) = r.create_render_target(W, H, fmt);
    assert!(
        r.raw_texture(rt).is_some(),
        "create 直後の handle が raw_texture で None"
    );
    assert_eq!(r.texture_size(rt), Some((W, H)), "render target の size 不一致");
    assert_eq!(r.texture_format(rt), Some(fmt), "render target の format 不一致");

    r.destroy_texture(rt);
    assert!(
        r.raw_texture(rt).is_none(),
        "destroy 後も raw_texture が Some"
    );
    assert!(r.texture_size(rt).is_none(), "destroy 後も size が Some");

    // size 0 は 1 に clamp (wgpu の texture 作成 panic を避ける、 create_texture と同 policy)。
    let (zero, _v) = r.create_render_target(0, 0, fmt);
    assert_eq!(
        r.texture_size(zero),
        Some((1, 1)),
        "0 サイズが 1 に clamp されていない"
    );
    r.destroy_texture(zero);
}
