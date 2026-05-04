//! `Renderer` — wgpu のデバイス・キュー・サーフェス・パイプラインを束ねる入口。
//!
//! ライフタイム方針:
//! - `Arc<W: WindowBackend + Send + Sync + 'static>` を保持 → `Surface<'static>` で安全に共存
//! - 描画は `begin_frame` → 各 pipeline へ encode → `end_frame` で submit & present
//!
//! 外部 crate での使用 (DAW プラグイン UI 埋め込み):
//! - `W` は winit `WinitWindow` 以外でも、`HasWindowHandle + HasDisplayHandle + WindowBackend`
//!   を実装する自前の型で OK (例: VST3 / CLAP プラグインで親アプリから受け取った
//!   raw window handle を保持する型)。`examples/embedded_host` 参照。
//! - **drop 順序の責務**: 親アプリ側の window (DAW host) が drop すると `Surface` が
//!   dangling になる。`Renderer` を破棄してから親 window を破棄する流れを呼び出し側で
//!   守る (本構造体は `Arc<W>` で window を `'static` に持ち上げているが、raw handle
//!   自体の有効性は親プロセス管理)。
//!
//! M1 の制約:
//! - MSAA / depth / present-mode 切替は最低限
//! - Vsync (FifoRelaxed) で安定描画

use std::sync::Arc;

use daw_ui_platform::{PhysicalSize, WindowBackend};

use crate::pipelines::{
    enqueue_runs, glyph::GlyphPipeline, line::LinePipeline, rect::RectPipeline, render_runs,
};
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
    /// M9 Phase 44a: popup pass 用の独立 pipeline インスタンス群。
    /// rect / line / glyph いずれも `prepare` 内で `queue.write_buffer` を呼ぶため、
    /// 同じ pipeline を `prepare→render→prepare→render` すると submit 時の最終 write が
    /// 反映される結果、base pass の render が popup pass の data を読んでしまう。
    /// (具体例: popup pass で `self.rect.prepare(&scene.popup_rects, ...)` すると、
    ///  base pass の `self.rect.render` が popup_rects を render して text_input の枠 rect 等が
    ///  消える)。これを回避するため popup 用に独立した pipeline インスタンスを持ち、内部
    ///  vertex/instance buffer / Atlas / Buffer cache を分離する。GPU メモリは ~2x になるが、
    ///  popup の primitive 数は base より大幅に少ないので実害は小さい。
    popup_rect: RectPipeline,
    popup_line: LinePipeline,
    popup_glyph: GlyphPipeline,
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
        let popup_rect = RectPipeline::new(&device, format);
        let popup_line = LinePipeline::new(&device, format);
        let popup_glyph = GlyphPipeline::new(&device, &queue, format);

        Ok(Self {
            surface,
            config,
            device,
            queue,
            rect,
            line,
            glyph,
            popup_rect,
            popup_line,
            popup_glyph,
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
    #[allow(clippy::too_many_lines)]
    pub fn render(&mut self, scene: &Scene) -> Result<(), RenderError> {
        // 1. サーフェステクスチャ取得 (wgpu 29 は CurrentSurfaceTexture enum を返す)
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(t)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
                    other @ (wgpu::CurrentSurfaceTexture::Outdated
                    | wgpu::CurrentSurfaceTexture::Lost
                    | wgpu::CurrentSurfaceTexture::Timeout
                    | wgpu::CurrentSurfaceTexture::Occluded
                    | wgpu::CurrentSurfaceTexture::Validation) => {
                        return Err(RenderError::SurfaceUnavailable(format!("{other:?}")));
                    }
                }
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                // フレームスキップ。エラーではない
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(RenderError::SurfaceUnavailable("validation error".to_string()));
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        // 2. begin_frame: 各 pipeline の scratch / pool を reset
        self.rect.begin_frame();
        self.line.begin_frame();
        self.glyph.begin_frame(&self.queue, self.size);
        self.popup_rect.begin_frame();
        self.popup_line.begin_frame();
        self.popup_glyph.begin_frame(&self.queue, self.size);

        // 3. base pass: scene.primitives を call order で walk、同 type 連続を 1 run に enqueue
        let base_runs = enqueue_runs(
            &scene.primitives,
            &mut self.rect,
            &mut self.line,
            &mut self.glyph,
            &self.device,
            &self.queue,
            self.size,
        );

        // 4. popup pass: scene.popup_primitives を同様に enqueue
        let popup_runs = enqueue_runs(
            &scene.popup_primitives,
            &mut self.popup_rect,
            &mut self.popup_line,
            &mut self.popup_glyph,
            &self.device,
            &self.queue,
            self.size,
        );

        // 5. upload (rect/line の instance buffer を 1 度に GPU へ転送、glyph は enqueue 内で済)
        self.rect.upload(&self.queue, self.size);
        self.line.upload(&self.queue, self.size);
        self.popup_rect.upload(&self.queue, self.size);
        self.popup_line.upload(&self.queue, self.size);

        // 6. encode (base pass: clear + 全 base run を call order で render)
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("daw-ui frame encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("daw-ui base pass"),
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
            render_runs(
                &base_runs,
                &self.rect,
                &self.line,
                &self.glyph,
                &mut pass,
                self.size,
            );
        }

        // 7. popup pass: base pass の上に popup primitives を render。
        // M9 Phase 44a: popup_rect / popup_line / popup_glyph (独立 pipeline インスタンス) を使う。
        // base 用 pipeline の GPU buffer が popup data で上書きされて base render が壊れる
        // 干渉を避けるため、独立インスタンスを維持。
        if !popup_runs.is_empty() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("daw-ui popup pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            render_runs(
                &popup_runs,
                &self.popup_rect,
                &self.popup_line,
                &self.popup_glyph,
                &mut pass,
                self.size,
            );
        }

        // 8. end_frame: glyph cache eviction を進める
        self.glyph.end_frame();
        self.popup_glyph.end_frame();

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
