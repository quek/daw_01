//! Instanced textured-quad パイプライン (M14 Phase 71 / daw_01 #043)。
//!
//! 入力: `TexturedQuad` (位置・UV・alpha・texture handle) を VertexBuffer (per-instance) で渡す。
//! 1 quad = 6 頂点 (2 三角形) を頂点シェーダ内で生成、 fragment で `textureSample × alpha`。
//!
//! ## 設計判断 (#043 reply で確定)
//!
//! - **BindingArray は使わない**: 1 texture = 1 bind_group = 1 draw call。 driver 依存
//!   (`Features::TEXTURE_BINDING_ARRAY`) の回避と KISS。 multi-texture composite (video
//!   preview の N track 重ね) は draw call 数 = quad 数 だけ発生するが、 典型 1-4 枚で
//!   実害なし。 後で API 互換のまま binding array 化可能。
//! - **format 固定**: target は surface format に追従、 texture pool 側の format は
//!   `Rgba8UnormSrgb` 固定 ([`TextureStore`](crate::texture_store::TextureStore))。
//! - **filter 固定**: `FilterMode::Linear` (min/mag 双方)、 mipmap なし。 thumbnail
//!   縮小も preview 拡大も linear で破綻なし。
//! - **blend**: `BlendState::ALPHA_BLENDING` (= OVER)、 crossfade 用に alpha=0.3/0.7 で
//!   2 枚 push すると正しく混色。
//!
//! ## texture lookup
//!
//! `enqueue_run` は `TexturedQuad.texture` を `DrawCall.handle` に保存するだけ。 実際の
//! `wgpu::BindGroup` lookup は [`render_run`] で [`TextureStore`] 経由で行う (= destroy
//! 済 handle は `bind_group` が `None` で draw call が skip される、 panic しない)。

use bytemuck::{Pod, Zeroable};
use daw_ui_platform::PhysicalSize;

use crate::scene::{Rect, TextureHandle, TexturedQuad};
use crate::texture_store::TextureStore;

/// シェーダに渡す instance 1 件分。
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct TextureInstance {
    /// 物理ピクセル単位の left, top, width, height
    pos: [f32; 4],
    /// uv_min.x, uv_min.y, uv_max.x, uv_max.y (0.0..=1.0)
    uv: [f32; 4],
    /// alpha, rotation_radians, pivot_off_x, pivot_off_y
    /// rotation_radians: clockwise positive、 NaN/Inf は CPU 側で 0.0 に正規化済
    /// (= `enqueue_run` で `is_finite()` ガード)。
    /// pivot_off_x / pivot_off_y: 旋回中心の **rect 左上相対** ピクセル offset (M14 Phase 92 /
    /// daw_01 #064)。 `rotation_pivot == None` のとき `(w/2, h/2)` を書くので shader 側
    /// `cx = left + misc.z` は Phase 76 の中心 pivot と byte 完全互換。
    misc: [f32; 4],
}

/// rotation 値の正規化 (`NaN` / ±Infinity → `0.0`)。 GPU driver の sin/cos 非有限挙動が
/// vendor 毎に分かれる可能性を回避し、 ロジックを caller 側に押し付けない (M14 Phase 76)。
#[inline]
#[must_use]
fn normalize_rotation(r: f32) -> f32 {
    if r.is_finite() { r } else { 0.0 }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct ScreenUniform {
    size: [f32; 4],
}

const MAX_INSTANCES: u64 = 4 * 1024;

/// 1 draw call の情報。 instance buffer index + texture handle + scissor。
#[derive(Debug, Clone, Copy)]
struct DrawCall {
    instance_idx: u32,
    handle: TextureHandle,
    clip: Option<Rect>,
}

/// 1 つの "run" = 連続した Texture primitive 群の draw range。
/// `TexturePipeline.calls` 内の `[call_start, call_end)` を 1 run として draw。
#[derive(Debug, Clone, Copy)]
pub struct TextureRun {
    call_start: u32,
    call_end: u32,
}

impl TextureRun {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.call_start == self.call_end
    }
}

pub struct TexturePipeline {
    pipeline: wgpu::RenderPipeline,
    instance_buffer: wgpu::Buffer,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    /// texture pool ([`TextureStore`]) が `create_texture` 時にこの layout で bind_group を作る。
    texture_bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    calls: Vec<DrawCall>,
    instances: Vec<TextureInstance>,
}

impl TexturePipeline {
    /// wgpu pipeline + uniform buffer + bind_group layout + sampler + instance buffer + render
    /// pipeline をまとめて構築する初期化関数。 個別 helper に切ると 1 度限りの呼び出し関係が
    /// 分散して逆に読みにくいので `#[allow(clippy::too_many_lines)]` (device.rs / offscreen.rs
    /// の render() と同 idiom)。
    #[allow(clippy::too_many_lines)]
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("texture shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("texture.wgsl").into()),
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("texture uniform"),
            size: std::mem::size_of::<ScreenUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture uniform layout"),
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

        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("texture uniform bg"),
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("texture sampler+texture layout"),
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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("texture sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("texture pipeline layout"),
            bind_group_layouts: &[Some(&uniform_layout), Some(&texture_bind_group_layout)],
            immediate_size: 0,
        });

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("texture instance buffer"),
            size: MAX_INSTANCES * std::mem::size_of::<TextureInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("texture pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<TextureInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x4,  // pos
                        1 => Float32x4,  // uv
                        2 => Float32x4,  // misc (alpha, _, _, _)
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
            uniform_bind_group,
            texture_bind_group_layout,
            sampler,
            calls: Vec::new(),
            instances: Vec::new(),
        }
    }

    /// [`TextureStore::create`] が bind_group を作るときに使う layout。
    pub fn texture_bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.texture_bind_group_layout
    }

    /// [`TextureStore::create`] が bind_group を作るときに使う sampler。
    pub fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }

    /// frame 開始時に呼ぶ (rect / line / glyph と同 idiom)。
    pub fn begin_frame(&mut self) {
        self.calls.clear();
        self.instances.clear();
    }

    /// 1 つの run として `quads` を enqueue する。 各 quad は 1 draw call (= bind_group per texture)。
    pub fn enqueue_run(&mut self, quads: &[TexturedQuad]) -> TextureRun {
        let call_start = self.calls.len() as u32;
        let avail = MAX_INSTANCES.saturating_sub(self.instances.len() as u64) as usize;
        let count = quads.len().min(avail);
        for q in quads.iter().take(count) {
            let inst_idx = self.instances.len() as u32;
            let theta = normalize_rotation(q.rotation_radians);
            // M14 Phase 92 (daw_01 #064): pivot を rect 左上相対 offset で pack。 None / 非 finite
            // は rect 中心 (w/2, h/2) に fallback (= Phase 76 と byte 互換、 caller 責務にしない)。
            let (pivot_off_x, pivot_off_y) = match q.rotation_pivot {
                Some((px, py)) if px.is_finite() && py.is_finite() => (px, py),
                _ => (q.rect.w * 0.5, q.rect.h * 0.5),
            };
            self.instances.push(TextureInstance {
                pos: [q.rect.x, q.rect.y, q.rect.w, q.rect.h],
                uv: [q.uv_min.0, q.uv_min.1, q.uv_max.0, q.uv_max.1],
                misc: [q.alpha, theta, pivot_off_x, pivot_off_y],
            });
            self.calls.push(DrawCall {
                instance_idx: inst_idx,
                handle: q.texture,
                clip: q.clip_rect,
            });
        }
        let call_end = self.calls.len() as u32;
        TextureRun { call_start, call_end }
    }

    /// frame 全 run の enqueue が終わったあとに呼ぶ。
    pub fn upload(&self, queue: &wgpu::Queue, screen: PhysicalSize) {
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

    /// 1 run を render pass に発行する。 各 call ごとに `set_bind_group(1, texture_bg)` + draw。
    /// destroy 済 handle (`store.bind_group()` が `None`) は skip して panic しない。
    pub fn render_run(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        screen: PhysicalSize,
        run: TextureRun,
        store: &TextureStore,
    ) {
        if run.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.uniform_bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        for call in &self.calls[run.call_start as usize..run.call_end as usize] {
            let Some(bg) = store.bind_group(call.handle) else {
                continue;
            };
            if let Some(clip) = call.clip {
                let Some((x, y, w, h)) = super::scissor::scissor_rect(clip, screen) else {
                    continue;
                };
                pass.set_scissor_rect(x, y, w, h);
            } else {
                pass.set_scissor_rect(0, 0, screen.width, screen.height);
            }
            pass.set_bind_group(1, bg, &[]);
            pass.draw(0..6, call.instance_idx..call.instance_idx + 1);
        }
        pass.set_scissor_rect(0, 0, screen.width, screen.height);
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_rotation;

    #[test]
    fn normalize_rotation_passes_through_finite_values() {
        // bit-exact 比較で OK (passthrough なので)
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(normalize_rotation(0.0), 0.0);
            assert_eq!(normalize_rotation(1.5), 1.5);
            assert_eq!(normalize_rotation(-std::f32::consts::PI), -std::f32::consts::PI);
            // typical use ケースの ±π を 1 周期分超えても finite ならそのまま (sin/cos の周期性に任せる)
            assert_eq!(normalize_rotation(7.5), 7.5);
        }
    }

    #[test]
    fn normalize_rotation_maps_nan_and_infinity_to_zero() {
        // M14 Phase 76 (daw_01 #047): caller が NaN/Inf を渡しても callee で 0.0 に正規化
        // = GPU sin/cos の vendor 毎の非有限挙動差を回避し、 axis-aligned 描画にフォールバック
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(normalize_rotation(f32::NAN), 0.0);
            assert_eq!(normalize_rotation(f32::INFINITY), 0.0);
            assert_eq!(normalize_rotation(f32::NEG_INFINITY), 0.0);
        }
    }
}
