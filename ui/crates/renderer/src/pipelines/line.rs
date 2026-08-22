// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Line segment パイプライン。
//!
//! 1 segment = 1 instance、6 頂点を頂点シェーダで quad に展開する。
//! 入力 instance: a (start), b (end), color, line_width。
//!
//! 主用途: 波形 (PeakLines)、メータ、グリッド、オートメーション (CPU flatten 後)。
//! M2 目標: 10 万頂点を 1 draw call で 60fps。

use bytemuck::{Pod, Zeroable};
use daw_ui_platform::PhysicalSize;

use crate::scene::{LineBatch, Rect};

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct LineInstance {
    a: [f32; 2],
    b: [f32; 2],
    color: [f32; 4],
    line_width: f32,
    _pad: [f32; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct ScreenUniform {
    size: [f32; 4],
}

/// instance buffer の初期容量。 以後は [`LinePipeline::upload`] が必要量まで grow する
/// (daw_01 r.md #59)。
///
/// **なぜ固定上限をやめたか**: 以前は `MAX_INSTANCES = 256 * 1024` を起動時に無条件確保して
/// おり、 `LineInstance` 48 B × 256 K = 12.6 MiB を **1 pipeline あたり** 常時占有していた。
/// `Renderer` は base と popup の 2 本を持ち、 daw_01 は main 窓と preview 窓で `Renderer` を
/// 2 つ持つので、 線を 1 本も描かない preview 窓のぶんまで含めて計 50 MiB が寝ていた。
/// さらに固定上限は超過分の線を **黙って捨てる** (旧 `enqueue_run` の `break`) ので、
/// 巨大アレンジで描画が欠ける崖でもあった。 grow 方式は両方を同時に解消する。
const INITIAL_INSTANCE_CAPACITY: u64 = 1024;

/// `capacity` 個の [`LineInstance`] を収容する instance buffer を確保する。
fn create_instance_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("line instance buffer"),
        size: capacity * std::mem::size_of::<LineInstance>() as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// 1 batch を何 instance 描画するか + scissor。
#[derive(Debug, Clone, Copy)]
struct DrawSpan {
    instance_start: u32,
    instance_end: u32,
    clip: Option<Rect>,
}

/// 1 つの "run" の draw range (`spans` の `[span_start, span_end)`)。
#[derive(Debug, Clone, Copy)]
pub struct LineRun {
    span_start: u32,
    span_end: u32,
}

impl LineRun {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.span_start == self.span_end
    }
}

pub struct LinePipeline {
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// `enqueue_run` で frame 全体に append、`render_run` 時に sub-range を順に draw。
    spans: Vec<DrawSpan>,
    /// frame 全体の line instance scratch (`upload` で 1 度に GPU へ転送)。
    instances: Vec<LineInstance>,
    /// `instance_buffer` が現在収容できる instance 数 (`upload` で必要量まで grow、 shrink しない)。
    instance_capacity: u64,
}

impl LinePipeline {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("line shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("line.wgsl").into()),
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("line uniform"),
            size: std::mem::size_of::<ScreenUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("line bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("line bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("line pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let instance_buffer = create_instance_buffer(device, INITIAL_INSTANCE_CAPACITY);

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("line pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<LineInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,  // a
                        1 => Float32x2,  // b
                        2 => Float32x4,  // color
                        3 => Float32,    // line_width
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            instance_buffer,
            uniform_buffer,
            bind_group,
            spans: Vec::new(),
            instances: Vec::new(),
            instance_capacity: INITIAL_INSTANCE_CAPACITY,
        }
    }

    /// frame 開始時に呼ぶ。
    pub fn begin_frame(&mut self) {
        self.spans.clear();
        self.instances.clear();
    }

    /// 1 つの run として `batches` を enqueue する。
    pub fn enqueue_run(&mut self, batches: &[LineBatch]) -> LineRun {
        let span_start = self.spans.len() as u32;
        for batch in batches {
            let start = self.instances.len() as u32;
            for seg in batch.segments.iter() {
                self.instances.push(LineInstance {
                    a: seg.a,
                    b: seg.b,
                    color: [seg.color.r, seg.color.g, seg.color.b, seg.color.a],
                    line_width: batch.line_width_px,
                    _pad: [0.0; 3],
                });
            }
            let end = self.instances.len() as u32;
            if end > start {
                self.spans.push(DrawSpan {
                    instance_start: start,
                    instance_end: end,
                    clip: batch.clip_rect,
                });
            }
        }
        let span_end = self.spans.len() as u32;
        LineRun { span_start, span_end }
    }

    /// frame 全 run の enqueue 後に呼ぶ。 instance 数がバッファ容量を超えていたら
    /// 転送前に確保し直す (daw_01 r.md #59)。
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, screen: PhysicalSize) {
        let needed = self.instances.len() as u64;
        if needed > self.instance_capacity {
            self.instance_capacity = needed.next_power_of_two();
            self.instance_buffer = create_instance_buffer(device, self.instance_capacity);
        }
        let uniform = ScreenUniform {
            size: [screen.width as f32, screen.height as f32, 0.0, 0.0],
        };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));
        if !self.instances.is_empty() {
            queue.write_buffer(
                &self.instance_buffer,
                0,
                bytemuck::cast_slice(&self.instances),
            );
        }
    }

    /// 1 つの run を render pass に発行する。
    pub fn render_run(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        screen: PhysicalSize,
        run: LineRun,
    ) {
        if run.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));

        for span in &self.spans[run.span_start as usize..run.span_end as usize] {
            if let Some(clip) = span.clip {
                let Some((x, y, w, h)) = super::scissor::scissor_rect(clip, screen) else {
                    continue;
                };
                pass.set_scissor_rect(x, y, w, h);
            } else {
                pass.set_scissor_rect(0, 0, screen.width, screen.height);
            }
            pass.draw(0..6, span.instance_start..span.instance_end);
        }

        pass.set_scissor_rect(0, 0, screen.width, screen.height);
    }
}
