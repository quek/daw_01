//! `Renderer` — wgpu のデバイス・キュー・サーフェス・パイプラインを束ねる入口。
//!
//! ライフタイム方針:
//! - `Arc<W: WindowBackend + Send + Sync + 'static>` を保持 → `Surface<'static>` で安全に共存
//! - 描画は `begin_frame` → 各 pipeline へ encode → `end_frame` で submit & present
//!
//! M1 の制約:
//! - MSAA / depth / present-mode 切替は最低限
//! - Vsync (FifoRelaxed) で安定描画

use std::sync::Arc;

use daw_ui_platform::{PhysicalSize, WindowBackend};

use crate::pipelines::{glyph::GlyphPipeline, line::LinePipeline, rect::RectPipeline};
use crate::scene::Scene;

/// 描画器本体。アプリ層が1つ持ち、フレーム毎に `render(&Scene)` を呼ぶ。
pub struct Renderer<W: WindowBackend + Send + Sync + 'static> {
    /// surface を生かすために window を保持。drop 順序は struct 末尾の方が後なので
    /// surface (上のフィールド) の方が先に drop される。
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    rect: RectPipeline,
    line: LinePipeline,
    glyph: GlyphPipeline,
    /// 現在の物理ピクセルサイズ。
    size: PhysicalSize,
    /// Window の所有権 (drop 順序のため最後)
    _window: Arc<W>,
}

impl<W: WindowBackend + Send + Sync + 'static> Renderer<W> {
    /// 同期的に wgpu を初期化。
    ///
    /// # Errors
    /// アダプタ取得失敗・デバイス取得失敗・サーフェス作成失敗。
    pub fn new(window: Arc<W>) -> Result<Self, RendererInitError> {
        let size = window.inner_size();

        // wgpu 29: InstanceDescriptor は Default を持たないので
        // `new_without_display_handle()` を使い、必要なフィールドだけ書き換える。
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(instance_desc);

        // Arc<W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static> から
        // 'static な Surface を作る。
        let surface = instance
            .create_surface(window.clone())
            .map_err(RendererInitError::CreateSurface)?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|_| RendererInitError::NoAdapter)?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("daw-ui device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(RendererInitError::RequestDevice)?;

        let surface_caps = surface.get_capabilities(&adapter);
        let format = surface_caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or_else(|| surface_caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let rect = RectPipeline::new(&device, format);
        let line = LinePipeline::new(&device, format);
        let glyph = GlyphPipeline::new(&device, &queue, format);

        Ok(Self {
            surface,
            config,
            device,
            queue,
            rect,
            line,
            glyph,
            size,
            _window: window,
        })
    }

    /// 物理サイズが変わったとき呼ぶ。
    pub fn resize(&mut self, new_size: PhysicalSize) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.size = new_size;
        self.config.width = new_size.width;
        self.config.height = new_size.height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn size(&self) -> PhysicalSize {
        self.size
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// Scene を 1 フレームとして描画。
    ///
    /// 失敗時 (Lost / Outdated 等) は再 configure して 1 度だけリトライ。
    /// それでも復帰しなければ `RenderError` を返す。
    ///
    /// # Errors
    /// サーフェステクスチャ取得不可 (デバイス消失等)。
    pub fn render(&mut self, scene: &Scene) -> Result<(), RenderError> {
        // 1. サーフェステクスチャ取得 (wgpu 29 は CurrentSurfaceTexture enum を返す)
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(t)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
                    other => return Err(RenderError::SurfaceUnavailable(format!("{other:?}"))),
                }
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                // フレームスキップ。エラーではない
                return Ok(());
            }
            other => return Err(RenderError::SurfaceUnavailable(format!("{other:?}"))),
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        // 2. パイプライン側の頂点/インスタンスバッファを準備
        self.rect.prepare(&self.device, &self.queue, &scene.rects, self.size);
        self.line.prepare(&self.device, &self.queue, &scene.line_batches, self.size);
        self.glyph.prepare(
            &self.device,
            &self.queue,
            &scene.glyph_areas,
            self.size,
        );

        // 3. encode
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("daw-ui frame encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("daw-ui main pass"),
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

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
    }
}

#[derive(Debug)]
pub enum RendererInitError {
    CreateSurface(wgpu::CreateSurfaceError),
    NoAdapter,
    RequestDevice(wgpu::RequestDeviceError),
}

impl std::fmt::Display for RendererInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateSurface(e) => write!(f, "wgpu surface 作成失敗: {e}"),
            Self::NoAdapter => write!(f, "wgpu アダプタが見つからない"),
            Self::RequestDevice(e) => write!(f, "wgpu デバイス取得失敗: {e}"),
        }
    }
}

impl std::error::Error for RendererInitError {}

#[derive(Debug)]
pub enum RenderError {
    SurfaceUnavailable(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SurfaceUnavailable(s) => write!(f, "wgpu surface 利用不能: {s}"),
        }
    }
}

impl std::error::Error for RenderError {}
