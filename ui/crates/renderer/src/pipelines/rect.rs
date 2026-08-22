//! Instanced 角丸矩形パイプライン。
//!
//! 入力: `RectInstance` (位置・サイズ・色・ボーダー・半径) を SSBO/VertexBuffer で渡す。
//! 1 矩形 = 6 頂点 (2 三角形) を頂点シェーダ内で生成し、フラグメントで角丸 + ボーダー。
//!
//! M1 目標: 1万矩形を 60fps。

use bytemuck::{Pod, Zeroable};
use daw_ui_platform::PhysicalSize;

use crate::scene::{Rect, RectCommand};

/// シェーダに渡す instance 1 件分。
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct RectInstance {
    /// 物理ピクセル単位の left, top, width, height
    pos: [f32; 4],
    /// fill RGBA
    fill: [f32; 4],
    /// border RGBA
    border: [f32; 4],
    /// border_width, radius_tl, radius_tr, radius_br
    misc0: [f32; 4],
    /// radius_bl, _padding, _padding, _padding
    misc1: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct ScreenUniform {
    /// 物理サイズ (width, height) と _pad
    size: [f32; 4],
}

/// instance buffer の初期容量。 以後は [`RectPipeline::upload`] が必要量まで grow する。
/// 固定上限をやめた理由は `line.rs` の同名定数の doc を参照 (daw_01 r.md #59)。
const INITIAL_INSTANCE_CAPACITY: u64 = 1024;

/// `capacity` 個の [`RectInstance`] を収容する instance buffer を確保する。
fn create_instance_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("rect instance buffer"),
        size: capacity * std::mem::size_of::<RectInstance>() as u64,
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

/// 1 つの "run" = 連続した同一 type primitive 群の draw range。
/// `RectPipeline.spans` 内の `[span_start, span_end)` を 1 run として draw する。
#[derive(Debug, Clone, Copy)]
pub struct RectRun {
    span_start: u32,
    span_end: u32,
}

impl RectRun {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.span_start == self.span_end
    }
}

pub struct RectPipeline {
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// `enqueue_run` で構築 (frame 全体で linear)。`render_run` 時に span 順で `set_scissor_rect` + draw。
    spans: Vec<DrawSpan>,
    /// frame 全体の instance バッファ scratch (`upload` で 1 度に GPU へ転送)。
    instances: Vec<RectInstance>,
    /// `instance_buffer` が現在収容できる instance 数 (`upload` で grow、 shrink しない)。
    instance_capacity: u64,
}

impl RectPipeline {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rect shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("rect.wgsl").into()),
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rect uniform"),
            size: std::mem::size_of::<ScreenUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rect bind group layout"),
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
            label: Some("rect bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rect pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let instance_buffer = create_instance_buffer(device, INITIAL_INSTANCE_CAPACITY);

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rect pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<RectInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x4,  // pos
                        1 => Float32x4,  // fill
                        2 => Float32x4,  // border
                        3 => Float32x4,  // misc0 (border_w, r_tl, r_tr, r_br)
                        4 => Float32x4,  // misc1 (r_bl, pad, pad, pad)
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // rect の fs は fill と border を **内部で合成** する唯一の pipeline なので、
                    // 出力は premultiplied alpha でなければ辻褄が合わない (r.md #53)。
                    // 単色を返す line / texture は straight alpha のままでよい。
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
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

    /// frame 開始時に呼ぶ。`spans` と `instances` の scratch を空にする。
    pub fn begin_frame(&mut self) {
        self.spans.clear();
        self.instances.clear();
    }

    /// 1 つの run として `rects` を enqueue する。`spans` / `instances` は frame 全体で linear
    /// に成長するので、以前の run の data は保持されたままで append される。
    pub fn enqueue_run(&mut self, rects: &[RectCommand]) -> RectRun {
        let span_start = self.spans.len() as u32;
        let mut span_inst_start: u32 = self.instances.len() as u32;
        let mut current_clip: Option<Rect> = None;
        let mut span_open = false;
        for cmd in rects {
            let i = self.instances.len() as u32;
            self.instances.push(RectInstance {
                pos: [cmd.rect.x, cmd.rect.y, cmd.rect.w, cmd.rect.h],
                fill: [cmd.fill.r, cmd.fill.g, cmd.fill.b, cmd.fill.a],
                border: [cmd.border.r, cmd.border.g, cmd.border.b, cmd.border.a],
                misc0: [cmd.border_width, cmd.radius[0], cmd.radius[1], cmd.radius[2]],
                misc1: [cmd.radius[3], 0.0, 0.0, 0.0],
            });
            if !span_open {
                span_inst_start = i;
                current_clip = cmd.clip_rect;
                span_open = true;
            } else if cmd.clip_rect != current_clip {
                self.spans.push(DrawSpan {
                    instance_start: span_inst_start,
                    instance_end: i,
                    clip: current_clip,
                });
                span_inst_start = i;
                current_clip = cmd.clip_rect;
            }
        }
        if span_open && (span_inst_start as usize) < self.instances.len() {
            self.spans.push(DrawSpan {
                instance_start: span_inst_start,
                instance_end: self.instances.len() as u32,
                clip: current_clip,
            });
        }
        let span_end = self.spans.len() as u32;
        RectRun { span_start, span_end }
    }

    /// frame 全 run の enqueue が終わったあとに呼ぶ。`instances` 全体を GPU に upload + uniform 更新。
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

    /// 1 つの run を render pass に発行する (set_pipeline + scissor span ごとに draw)。
    pub fn render_run(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        screen: PhysicalSize,
        run: RectRun,
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
            // 1 矩形 = 6 頂点 (2 三角形)、インスタンス展開
            pass.draw(0..6, span.instance_start..span.instance_end);
        }
        // 後続パイプラインのために scissor を全画面に戻す。
        pass.set_scissor_rect(0, 0, screen.width, screen.height);
    }
}
