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

const MAX_INSTANCES: u64 = 256 * 1024;

/// 1 batch を何 instance 描画するか + scissor。
#[derive(Debug, Clone, Copy)]
struct DrawSpan {
    instance_start: u32,
    instance_end: u32,
    clip: Option<Rect>,
}

pub struct LinePipeline {
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// `prepare` で構築。`render` 時はこの spans を順に draw。
    spans: Vec<DrawSpan>,
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

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("line instance buffer"),
            size: MAX_INSTANCES * std::mem::size_of::<LineInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("line pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
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
                compilation_options: Default::default(),
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
        }
    }

    pub fn prepare(
        &mut self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        batches: &[LineBatch],
        screen: PhysicalSize,
    ) {
        // ScreenUniform 更新
        let uniform = ScreenUniform {
            size: [screen.width as f32, screen.height as f32, 0.0, 0.0],
        };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));

        // 全 batch の segment を 1 本のインスタンスバッファに詰めて、span だけ覚える
        self.spans.clear();
        let mut instances: Vec<LineInstance> = Vec::new();
        for batch in batches {
            let start = instances.len() as u32;
            for seg in &batch.segments {
                if instances.len() as u64 >= MAX_INSTANCES {
                    break;
                }
                instances.push(LineInstance {
                    a: seg.a,
                    b: seg.b,
                    color: [seg.color.r, seg.color.g, seg.color.b, seg.color.a],
                    line_width: batch.line_width_px,
                    _pad: [0.0; 3],
                });
            }
            let end = instances.len() as u32;
            if end > start {
                self.spans.push(DrawSpan {
                    instance_start: start,
                    instance_end: end,
                    clip: batch.clip_rect,
                });
            }
            if instances.len() as u64 >= MAX_INSTANCES {
                break;
            }
        }
        if !instances.is_empty() {
            queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&instances));
        }
    }

    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>, screen: PhysicalSize) {
        if self.spans.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));

        for span in &self.spans {
            if let Some(clip) = span.clip {
                // 画面内にクランプ。範囲外なら描画しない。
                let l = clip.x.max(0.0);
                let t = clip.y.max(0.0);
                let r = (clip.x + clip.w).min(screen.width as f32);
                let b = (clip.y + clip.h).min(screen.height as f32);
                if r <= l || b <= t {
                    continue;
                }
                pass.set_scissor_rect(l as u32, t as u32, (r - l) as u32, (b - t) as u32);
            } else {
                pass.set_scissor_rect(0, 0, screen.width, screen.height);
            }
            pass.draw(0..6, span.instance_start..span.instance_end);
        }

        // 後続パイプラインのために scissor を全画面に戻す。
        pass.set_scissor_rect(0, 0, screen.width, screen.height);
    }
}
