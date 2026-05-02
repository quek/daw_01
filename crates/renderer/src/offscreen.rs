//! `OffscreenRenderer` — wgpu surface を使わない render-to-texture + readback パス。
//!
//! 用途:
//! - DAW プラグイン UI 埋め込みでの試験 / snapshot 取得
//! - window なしで `daw-ui` の動作を PNG 化して回帰確認
//!
//! 既存 [`Renderer<W>`](crate::Renderer) (window 必須) とは独立した struct。pipelines は
//! 同じ `RectPipeline` / `LinePipeline` / `GlyphPipeline` を target_format 引数で初期化。
//!
//! # wgpu 29 系の罠
//! - `bytes_per_row` は `COPY_BYTES_PER_ROW_ALIGNMENT` (= 256) の倍数必須。本実装は
//!   `unpadded.div_ceil(256) * 256` で staging buffer に padding して、readback 後に
//!   row 単位で詰め直す。
//! - `Maintain::Wait` は 28 以前の API。29 では `PollType::wait_indefinitely()` を使う。
//! - readback bytes は sRGB 8-bit (`Rgba8UnormSrgb`)、PNG `ColorType::Rgba` にそのまま渡せる。

use std::sync::mpsc::sync_channel;

use daw_ui_platform::PhysicalSize;

use crate::device::{RenderError, RendererInitError};
use crate::pipelines::{glyph::GlyphPipeline, line::LinePipeline, rect::RectPipeline};
use crate::scene::Scene;

/// surface を使わない wgpu レンダラ。
///
/// `render_to_rgba(scene)` で内部 texture に 1 フレーム描画し、RGBA bytes を返す。
/// 親 window が無いプラグイン UI 埋め込み環境や snapshot 取得 (PNG) に使う。
pub struct OffscreenRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    target_format: wgpu::TextureFormat,
    rect: RectPipeline,
    line: LinePipeline,
    glyph: GlyphPipeline,
    size: PhysicalSize,
}

impl OffscreenRenderer {
    /// 同期的に wgpu を初期化 (surface なし)。
    ///
    /// target は `Rgba8UnormSrgb` 固定 (既存 pipeline の sRGB 前提に整合)。
    /// readback bytes は sRGB 8-bit で、PNG `ColorType::Rgba` にそのまま渡せる。
    ///
    /// # Errors
    /// アダプタ取得失敗・デバイス取得失敗。
    pub fn new(width: u32, height: u32) -> Result<Self, RendererInitError> {
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(instance_desc);

        // surface 不要なので compatible_surface=None で adapter を取る (native は OK、
        // WebGL2 のみ surface 必須なので wasm では別経路が必要)。
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .map_err(|_| RendererInitError::NoAdapter)?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("daw-ui offscreen device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(RendererInitError::RequestDevice)?;

        let target_format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let rect = RectPipeline::new(&device, target_format);
        let line = LinePipeline::new(&device, target_format);
        let glyph = GlyphPipeline::new(&device, &queue, target_format);

        Ok(Self {
            device,
            queue,
            target_format,
            rect,
            line,
            glyph,
            size: PhysicalSize { width: width.max(1), height: height.max(1) },
        })
    }

    pub fn size(&self) -> PhysicalSize {
        self.size
    }

    pub fn target_format(&self) -> wgpu::TextureFormat {
        self.target_format
    }

    /// Scene を 1 フレーム分 render し、RGBA bytes (sRGB encoded) を返す。
    ///
    /// 戻り値の bytes は行 stride `width * 4` (= unpadded、内部 staging buffer の
    /// 256-align padding は row 単位コピーで畳む)。PNG `ColorType::Rgba` にそのまま渡せる。
    ///
    /// # Errors
    /// staging buffer の `map_async` / `Device::poll` が失敗した場合。
    pub fn render_to_rgba(&mut self, scene: &Scene) -> Result<Vec<u8>, RenderError> {
        let w = self.size.width;
        let h = self.size.height;

        // 1. RENDER_ATTACHMENT | COPY_SRC な target texture
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("daw-ui offscreen target"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // 2. bytes_per_row を COPY_BYTES_PER_ROW_ALIGNMENT (= 256) に揃える
        let unpadded = w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded = unpadded.div_ceil(align) * align;
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("daw-ui offscreen readback"),
            size: u64::from(padded) * u64::from(h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        // 3. pipeline buffers の prepare (Renderer::render と同形)
        self.rect.prepare(&self.device, &self.queue, &scene.rects, self.size);
        self.line.prepare(&self.device, &self.queue, &scene.line_batches, self.size);
        self.glyph.prepare(&self.device, &self.queue, &scene.glyph_areas, self.size);

        // 4. encode: render pass + copy_texture_to_buffer
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("daw-ui offscreen encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("daw-ui offscreen pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(scene.clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.rect.render(&mut pass);
            self.line.render(&mut pass, self.size);
            self.glyph.render(&mut pass);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );

        // 5. submit + map_async + poll(Wait)
        self.queue.submit(std::iter::once(encoder.finish()));

        let slice = staging.slice(..);
        let (tx, rx) = sync_channel::<Result<(), wgpu::BufferAsyncError>>(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RenderError::SurfaceUnavailable(format!("offscreen poll: {e:?}")))?;
        rx.recv()
            .map_err(|e| RenderError::SurfaceUnavailable(format!("offscreen recv: {e:?}")))?
            .map_err(|e| RenderError::SurfaceUnavailable(format!("offscreen map_async: {e:?}")))?;

        // 6. padded → unpadded で row 単位コピー
        let padded_view = slice.get_mapped_range();
        let mut out = Vec::with_capacity((unpadded * h) as usize);
        for row in 0..h {
            let s = (row * padded) as usize;
            out.extend_from_slice(&padded_view[s..s + unpadded as usize]);
        }
        drop(padded_view);
        staging.unmap();

        Ok(out)
    }
}
