//! Instanced 角丸矩形パイプライン。
//!
//! 入力: `RectInstance` (位置・サイズ・色・ボーダー・半径) を SSBO/VertexBuffer で渡す。
//! 1 矩形 = 6 頂点 (2 三角形) を頂点シェーダ内で生成し、フラグメントで角丸 + ボーダー。
//!
//! M1 目標: 1万矩形を 60fps。

use bytemuck::{Pod, Zeroable};
use daw_ui_platform::PhysicalSize;

use crate::scene::RectCommand;

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

const MAX_INSTANCES: u64 = 64 * 1024;

pub struct RectPipeline {
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instance_count: u32,
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

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rect instance buffer"),
            size: MAX_INSTANCES * std::mem::size_of::<RectInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rect pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
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
            instance_count: 0,
        }
    }

    pub fn prepare(
        &mut self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        rects: &[RectCommand],
        screen: PhysicalSize,
    ) {
        // ScreenUniform を更新
        let uniform = ScreenUniform {
            size: [screen.width as f32, screen.height as f32, 0.0, 0.0],
        };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));

        // インスタンスを敷き詰め
        let count = rects.len().min(MAX_INSTANCES as usize);
        let mut instances: Vec<RectInstance> = Vec::with_capacity(count);
        for cmd in rects.iter().take(count) {
            instances.push(RectInstance {
                pos: [cmd.rect.x, cmd.rect.y, cmd.rect.w, cmd.rect.h],
                fill: [cmd.fill.r, cmd.fill.g, cmd.fill.b, cmd.fill.a],
                border: [cmd.border.r, cmd.border.g, cmd.border.b, cmd.border.a],
                misc0: [cmd.border_width, cmd.radius[0], cmd.radius[1], cmd.radius[2]],
                misc1: [cmd.radius[3], 0.0, 0.0, 0.0],
            });
        }
        if !instances.is_empty() {
            queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&instances));
        }
        self.instance_count = count as u32;
    }

    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.instance_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        // 1 矩形 = 6 頂点 (2 三角形)、インスタンス展開
        pass.draw(0..6, 0..self.instance_count);
    }
}
