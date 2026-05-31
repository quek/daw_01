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

use crate::composite::{composite_scene, CompositePool};
use crate::pipelines::{
    enqueue_runs, glyph::GlyphPipeline, line::LinePipeline, prepare_text_effects,
    rect::RectPipeline, render_runs, text_effect::TextEffectCompositor,
    texture::TexturePipeline,
};
use crate::scene::{Scene, TextureHandle};
use crate::texture_store::TextureStore;

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
    /// M14 Phase 71 (daw_01 #043): video frame / thumbnail 用 textured-quad pipeline。
    /// base pass のみ (popup pass からは push されない、 #043 reply 参照)。
    texture: TexturePipeline,
    /// M14 Phase 71: texture handle → wgpu::Texture + bind_group の lookup table。
    /// caller (daw_01 daw_gui) が `create_texture` / `upload_texture_rgba` / `destroy_texture`
    /// で lifecycle 管理する (GUI 側 LRU 等は持たない、 #043 設計判断)。
    texture_store: TextureStore,
    /// M14 Phase 78 (daw_01 #049): GlyphArea outline / shadow / blur / rotation 効果 compositor。
    /// effect 付き area を offscreen RGBA texture に焼いて、 base scene の TexturedQuad
    /// (Phase 71/76 rotation 込み) として push する。 effect 無し path は既存 GlyphPipeline 直接。
    text_effect: TextEffectCompositor,
    /// M14 Phase 93 (daw_01 #063): `composite_scene_to_texture` の render target を size 別に
    /// 使い回す pool。 handle は `texture_store` 内の texture を指す (pool 自体は GPU resource を
    /// 直接持たない)。
    composite_pool: CompositePool,
    /// M14 Phase 74 (daw_01 #045 §B) / Phase 75 (#046): adapter の backend を保存。
    /// `create_texture_from_d3d11_shared_handle` が DX12 + Vulkan dispatch するため、
    /// 呼び出し時に `Backend` で path を選ぶ。 Metal / GL では `WrongBackend` で fail-soft。
    backend: wgpu::Backend,
    /// M14 Phase 75 (daw_01 #046): Vulkan backend で `VULKAN_EXTERNAL_MEMORY_WIN32` feature を
    /// adapter がサポート + `request_device` で要求した結果 enable された場合のみ `true`。
    /// `false` のときは `create_texture_from_d3d11_shared_handle` (Vulkan path) で
    /// [`RendererError::FeatureUnsupported`] で fail-soft する。 AMD / Intel 一部 driver で
    /// `D3D11_TEXTURE` handle type を report しない既知の罠への対応 (= renderer 初期化は成功、
    /// zero-copy 経路が使えない時だけ caller に伝える)。
    vulkan_external_memory_supported: bool,
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
        // M14 Phase 74 (daw_01 #045 §B): backend を保存 (DX12 / Vulkan dispatch で使用)。
        let backend = adapter.get_info().backend;

        // M14 Phase 75 (daw_01 #046): Backend::Vulkan + adapter サポート時のみ
        // VULKAN_EXTERNAL_MEMORY_WIN32 を required_features に conditional 追加。
        // adapter 非対応の Vulkan 環境 (AMD / Intel 一部 driver) では feature を要求せずに
        // device 取得 = renderer 初期化は成功、 `create_texture_from_d3d11_shared_handle`
        // 呼び出し時のみ `FeatureUnsupported` で fail-soft (= caller protect、 SSoT を守る)。
        let vulkan_external_memory_supported = backend == wgpu::Backend::Vulkan
            && adapter.features().contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32);
        let mut required_features = wgpu::Features::empty();
        if vulkan_external_memory_supported {
            required_features |= wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32;
        }

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("daw-ui device"),
            required_features,
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
        let texture = TexturePipeline::new(&device, format);
        let texture_store = TextureStore::new();
        let text_effect = TextEffectCompositor::new(&device, &queue);
        let composite_pool = CompositePool::new();

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
            texture,
            texture_store,
            text_effect,
            composite_pool,
            backend,
            vulkan_external_memory_supported,
            size,
            _window: window,
        })
    }

    // ============================================================
    // M14 Phase 71 (daw_01 #043): texture pool public API
    // ============================================================

    /// 指定サイズの空 RGBA8UnormSrgb texture を確保し、 `TextureHandle` を返す。
    /// sRGB encoded RGBA8 入力前提 (= PNG decode 結果 / FFmpeg sws_scale RGBA 出力)。
    /// `width` / `height` = 0 は 1 に clamp。
    pub fn create_texture(&mut self, width: u32, height: u32) -> TextureHandle {
        self.texture_store.create(
            &self.device,
            self.texture.sampler(),
            self.texture.texture_bind_group_layout(),
            wgpu::TextureFormat::Rgba8UnormSrgb,
            width,
            height,
        )
    }

    /// M14 Phase 73 (daw_01 #045): 指定サイズの空 BGRA8UnormSrgb texture を確保。
    /// WMF / DXVA 系 decoder が直接吐く `MFVideoFormat_ARGB32` (= little-endian で BGRA8) を
    /// **CPU swap 不要で直接 upload** できるようにする (= daw_01 P2、 1080p60 で ~3ms/frame の
    /// release-build coast を除去)。 sampling は format-transparent (= 既存
    /// `Scene::push_textured_quad` で OK、 GPU 内 sampling shader が format を見て channel を
    /// 正しく取り出す)。
    pub fn create_texture_bgra(&mut self, width: u32, height: u32) -> TextureHandle {
        self.texture_store.create(
            &self.device,
            self.texture.sampler(),
            self.texture.texture_bind_group_layout(),
            wgpu::TextureFormat::Bgra8UnormSrgb,
            width,
            height,
        )
    }

    /// RGBA8 (`width * height * 4` bytes) で texture content を上書き。
    /// destroy 済 handle / size 不一致は no-op (debug build では panic)。
    /// handle が BGRA texture で作成済の場合も no-op + debug panic (cross-format protect)。
    pub fn upload_texture_rgba(&mut self, handle: TextureHandle, data: &[u8]) {
        self.texture_store.upload_with_format(
            &self.queue,
            handle,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            data,
        );
    }

    /// M14 Phase 73 (daw_01 #045): BGRA8 (`width * height * 4` bytes、 B G R A 順) で texture
    /// content を上書き。 RGBA upload と同 byte layout だが channel 順だけ違う (= caller の
    /// `bgra` slice を sRGB blue → green → red → alpha で読み取る)。
    /// destroy 済 / size 不一致 / format 不一致は no-op + debug_assert (RGBA 版と同 policy)。
    pub fn upload_texture_bgra(&mut self, handle: TextureHandle, bgra: &[u8]) {
        self.texture_store.upload_with_format(
            &self.queue,
            handle,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            bgra,
        );
    }

    /// texture を解放。 既に解放された handle に対する操作は no-op。 以後 `texture_size` は `None`、
    /// `push_textured_quad` で render しても描画 skip (panic しない)。
    pub fn destroy_texture(&mut self, handle: TextureHandle) {
        self.texture_store.destroy(handle);
    }

    /// texture の native (width, height) を返す。 destroy 済は `None`。
    /// arrangement clip thumbnail の aspect-fit 計算 (daw_01 #044) で widget 内部から参照される。
    #[must_use]
    pub fn texture_size(&self, handle: TextureHandle) -> Option<(u32, u32)> {
        self.texture_store.size(handle)
    }

    /// M14 Phase 73 (daw_01 #045): texture の format を返す (debug / test 用)。
    /// destroy 済は `None`。 通常 caller は handle 発行時の format を覚えていれば良いので、
    /// production path で参照する必要はない (= sampling は bind_group 経由で format-agnostic)。
    #[must_use]
    pub fn texture_format(&self, handle: TextureHandle) -> Option<wgpu::TextureFormat> {
        self.texture_store.format(handle)
    }

    /// M14 Phase 74 (daw_01 #045 §B): D3D11 shared NT handle 経由の zero-copy texture import。
    /// **DX12 backend 限定** — non-DX12 では [`RendererError::WrongBackend`] を返し fail-soft。
    ///
    /// # 動作
    ///
    /// 1. `wgpu::Device::as_hal::<dx12::Api>` で `ID3D12Device` を取得
    /// 2. `ID3D12Device::OpenSharedHandle(handle)` で `ID3D12Resource` を取得
    /// 3. `wgpu::hal::dx12::Device::texture_from_raw` で `hal::Texture` を構築
    /// 4. `wgpu::Device::create_texture_from_hal` で `wgpu::Texture` に昇格
    /// 5. [`TextureStore::import_texture`] で bind_group を作って store に登録
    ///
    /// # Arguments
    ///
    /// - `shared_handle`: D3D11 で生成した shared NT handle (`windows = "0.62"` の `HANDLE`、
    ///   = `*mut c_void` の newtype)。 daw_01 workspace も `windows = "0.62"` に揃える設計で、
    ///   newtype のまま渡し、 raw 値展開を caller に強要しない (= 型システムが境界を守る、
    ///   #045 reply の Q (B) で確定)。
    /// - `format`: imported texture の wgpu format (e.g. `Bgra8UnormSrgb` for WMF ARGB32)
    /// - `width` / `height`: native pixel size
    ///
    /// # Caller responsibilities
    ///
    /// - `shared_handle` は **valid な NT handle** (= `D3D11_RESOURCE_MISC_SHARED_NTHANDLE +
    ///   KEYED_MUTEX` 付きで生成済) でなければならない
    /// - 返ってきた `TextureHandle` を [`Self::destroy_texture`] で release するまで、
    ///   underlying shared handle / D3D11 resource は **valid なまま保持** する責務 (gui_01 は
    ///   shared handle を Close しない、 透過導管)
    /// - WMF が source resource に書き込む前後で **`KEYED_MUTEX` の acquire/release** は caller 管理
    /// - `format` は実体 (D3D11 resource の format) と一致している前提 (= 不一致なら描画が壊れる、
    ///   現状 wgpu::hal::dx12::texture_from_raw は format check しないため caller protect)
    ///
    /// # Errors
    ///
    /// - [`RendererError::WrongBackend`]: DX12 / Vulkan 以外の backend (Metal / GL)
    /// - [`RendererError::OpenSharedHandle`]: DX12 path で `ID3D12Device::OpenSharedHandle` が
    ///   HRESULT 失敗 (invalid handle / ACL / resource 既 release 等)
    /// - [`RendererError::VulkanImportFailed`]: Vulkan path で `texture_from_d3d11_shared_handle`
    ///   が失敗 (= driver が `D3D11_TEXTURE` handle type を report しない、 invalid handle 等)
    /// - [`RendererError::FeatureUnsupported`]: Vulkan adapter が `VULKAN_EXTERNAL_MEMORY_WIN32`
    ///   を report しない (= AMD / Intel 一部 driver の既知の罠)
    ///
    /// # Safety
    ///
    /// 本 method は `pub` だが **caller responsibilities** を満たす前提で動く。 violated な
    /// `shared_handle` を渡すと OpenSharedHandle / Vulkan import が HRESULT / VkResult で
    /// fail-soft する想定だが、 driver / OS によっては process crash の可能性もある
    /// (= raw NT handle の宿命)。 caller は WMF / D3D11 経路で確実に正しい handle を生成すること。
    #[cfg(windows)]
    pub fn create_texture_from_d3d11_shared_handle(
        &mut self,
        shared_handle: windows::Win32::Foundation::HANDLE,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Result<TextureHandle, RendererError> {
        // M14 Phase 75 (daw_01 #046): backend dispatch (DX12 / Vulkan 透過、 caller boilerplate ゼロ)。
        match self.backend {
            wgpu::Backend::Dx12 => self.import_d3d11_shared_handle_dx12(
                shared_handle,
                format,
                width,
                height,
            ),
            wgpu::Backend::Vulkan => {
                if !self.vulkan_external_memory_supported {
                    return Err(RendererError::FeatureUnsupported(
                        "VULKAN_EXTERNAL_MEMORY_WIN32",
                    ));
                }
                self.import_d3d11_shared_handle_vulkan(shared_handle, format, width, height)
            }
            other => Err(RendererError::WrongBackend(other)),
        }
    }

    /// M14 Phase 74 (DX12 path、 daw_01 #045 §B): 3-step pattern。
    /// 1) `as_hal::<dx12::Api>` → `raw_device()` で `&ID3D12Device`
    /// 2) `ID3D12Device::OpenSharedHandle::<ID3D12Resource>` で NT handle → COM resource
    /// 3) `wgpu::hal::dx12::Device::texture_from_raw` で hal::Texture (static 風 fn、 D3D12 API 不発)
    /// 4) `create_texture_from_hal` で wgpu::Texture 化 → `TextureStore::import_texture`
    ///
    /// SAFETY: 全 unsafe block は親 method の caller responsibility 文書に依拠。
    /// shared_handle が valid な NT handle で、 format/size が実体一致している前提。
    #[cfg(windows)]
    fn import_d3d11_shared_handle_dx12(
        &mut self,
        shared_handle: windows::Win32::Foundation::HANDLE,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Result<TextureHandle, RendererError> {
        use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

        let hal_texture: wgpu::hal::dx12::Texture = unsafe {
            let hal_device_guard = self.device.as_hal::<wgpu::hal::dx12::Api>();
            let Some(hal_device) = hal_device_guard else {
                return Err(RendererError::WrongBackend(self.backend));
            };
            let d3d12_device = hal_device.raw_device();
            // windows 0.62.x の OpenSharedHandle は out-param 形式 (= `*mut Option<T>`)。
            // `&raw mut` で明示的に raw pointer 化 (clippy::borrow_as_ptr、 rust 1.95+ の lint)。
            let mut resource_out: Option<ID3D12Resource> = None;
            d3d12_device
                .OpenSharedHandle::<ID3D12Resource>(shared_handle, &raw mut resource_out)
                .map_err(|e| RendererError::OpenSharedHandle(format!("{e}")))?;
            let resource = resource_out.ok_or_else(|| {
                RendererError::OpenSharedHandle(
                    "OpenSharedHandle returned Ok but resource was None".into(),
                )
            })?;

            wgpu::hal::dx12::Device::texture_from_raw(
                resource,
                format,
                wgpu::TextureDimension::D2,
                wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                1, // mip_level_count
                1, // sample_count
            )
        };

        // SAFETY: hal_texture は texture_from_raw で構築済、 同 device の HAL なので bound 一致。
        let wgpu_texture = unsafe {
            self.device.create_texture_from_hal::<wgpu::hal::dx12::Api>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("d3d11 shared texture (dx12)"),
                    size: wgpu::Extent3d {
                        width: width.max(1),
                        height: height.max(1),
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
        };

        Ok(self.texture_store.import_texture(
            &self.device,
            self.texture.sampler(),
            self.texture.texture_bind_group_layout(),
            wgpu_texture,
            format,
            width,
            height,
        ))
    }

    /// M14 Phase 75 (Vulkan path、 daw_01 #046): 2-step pattern (DX12 より 1 段少ない、
    /// wgpu_hal が `OpenSharedHandle` 同等の `VkImportMemoryWin32HandleInfoKHR` を内製)。
    /// 1) `as_hal::<vulkan::Api>` で `&wgpu::hal::vulkan::Device` 取得
    /// 2) `wgpu::hal::vulkan::Device::texture_from_d3d11_shared_handle(handle, desc)` で
    ///    hal::Texture (`VK_KHR_external_memory_win32` 経由で内部 import)
    /// 3) `create_texture_from_hal::<vulkan::Api>` で wgpu::Texture 化 → `TextureStore::import_texture`
    ///
    /// SAFETY: 全 unsafe block は親 method の caller responsibility 文書に依拠。
    /// `vulkan_external_memory_supported` は事前に Renderer::new で確認済 (= `request_device` で
    /// `VULKAN_EXTERNAL_MEMORY_WIN32` feature が enable された)、 device は対応している前提。
    #[cfg(windows)]
    fn import_d3d11_shared_handle_vulkan(
        &mut self,
        shared_handle: windows::Win32::Foundation::HANDLE,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Result<TextureHandle, RendererError> {
        let desc = wgpu::hal::TextureDescriptor {
            label: Some("d3d11 shared texture (vulkan)"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            // wgpu 29.0.1: `wgpu::hal::TextureDescriptor.usage` の型は `wgpu_types::TextureUses`
            // (= `wgpu::wgt::TextureUses` で参照、 wgpu 内で `pub extern crate wgpu_types as wgt`)。
            usage: wgpu::wgt::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };

        let hal_texture: wgpu::hal::vulkan::Texture = unsafe {
            let hal_device_guard = self.device.as_hal::<wgpu::hal::vulkan::Api>();
            let Some(hal_device) = hal_device_guard else {
                // backend check は親で済んでいるので通常到達しないが、 wgpu 内部状態が変わった
                // (= driver 切替 hot-swap 等) に備えた defensive。
                return Err(RendererError::WrongBackend(self.backend));
            };
            hal_device
                .texture_from_d3d11_shared_handle(shared_handle, &desc)
                .map_err(|e| RendererError::VulkanImportFailed(format!("{e:?}")))?
        };

        // SAFETY: hal_texture は texture_from_d3d11_shared_handle で構築済、 同 device の HAL なので
        // bound 一致。
        let wgpu_texture = unsafe {
            self.device.create_texture_from_hal::<wgpu::hal::vulkan::Api>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("d3d11 shared texture (vulkan)"),
                    size: wgpu::Extent3d {
                        width: width.max(1),
                        height: height.max(1),
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
        };

        Ok(self.texture_store.import_texture(
            &self.device,
            self.texture.sampler(),
            self.texture.texture_bind_group_layout(),
            wgpu_texture,
            format,
            width,
            height,
        ))
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

    /// M14 Phase 93 (daw_01 #063): `scene.primitives` を `width × height` の GPU 常駐 sampleable
    /// texture に合成し、 その [`TextureHandle`] を返す。 立ち絵 group transform 等で「子 quad 群を
    /// 1 枚に焼いてから親 affine (#064 の `rotation_pivot` 込み) を 1 回かける」 用途。
    ///
    /// - **GPU 常駐 / readback なし**: preview で毎フレーム呼べる。 内部で独自 encoder を submit する。
    /// - **透明 clear**: `scene.clear_color` は無視し常に透明で clear (合成結果は親 scene へ alpha
    ///   composite される前提)。 `scene.popup_primitives` は対象外。
    /// - **target の使い回し**: size 別に内部 pool で再利用 (renderer がライフサイクル所有 = SSoT、
    ///   caller は返却 handle を `destroy_texture` しない)。 返った handle は **次の `render()` まで**
    ///   有効 (= その frame の base scene に `push_textured_quad` して `render()` するまでに使う)。
    /// - **format**: 返る texture の format は本 renderer の surface format に一致する (preview pipeline は
    ///   surface format で描くため)。 `TexturedQuad` sampling は format-transparent なので caller は
    ///   channel 順を意識しなくてよい。
    ///
    /// # Errors
    /// `width` / `height` が `max_texture_dimension_2d` を超える場合
    /// [`RenderError::CompositeTargetTooLarge`] (= wgpu の texture 作成 panic を caller protect)。
    pub fn composite_scene_to_texture(
        &mut self,
        scene: &Scene,
        width: u32,
        height: u32,
    ) -> Result<TextureHandle, RenderError> {
        let max = self.device.limits().max_texture_dimension_2d;
        if width > max || height > max {
            return Err(RenderError::CompositeTargetTooLarge { width, height, max });
        }
        Ok(composite_scene(
            scene,
            width,
            height,
            self.config.format,
            &self.device,
            &self.queue,
            &mut self.rect,
            &mut self.line,
            &mut self.glyph,
            &mut self.texture,
            &mut self.text_effect,
            &mut self.texture_store,
            &mut self.composite_pool,
        ))
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
        self.texture.begin_frame();
        self.text_effect.begin_frame();

        // 3. encoder を **先に** 作る (M14 Phase 78): text effect の pre-pass (offscreen
        //    glyph + blur H/V + composite) を base pass より前に同 encoder に積むため。
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("daw-ui frame encoder"),
        });

        // 4. M14 Phase 78 (daw_01 #049): effect 付き Glyph primitive を offscreen で render → texture
        //    に焼いて Primitive::Texture に substitute。 effect 無し / 他 type は pass-through。
        let (font_system, swash_cache) = self.glyph.font_system_and_swash();
        let base_primitives_substituted = prepare_text_effects(
            &scene.primitives,
            &mut self.text_effect,
            &self.device,
            &self.queue,
            &mut encoder,
            font_system,
            swash_cache,
            &mut self.texture_store,
            self.texture.sampler(),
            self.texture.texture_bind_group_layout(),
        );

        // 5. base pass: substituted primitives を call order で walk、同 type 連続を 1 run に enqueue
        let base_runs = enqueue_runs(
            &base_primitives_substituted,
            &mut self.rect,
            &mut self.line,
            &mut self.glyph,
            Some(&mut self.texture),
            &self.device,
            &self.queue,
            self.size,
        );

        // 6. popup pass: scene.popup_primitives を同様に enqueue (texture は base のみ、 #043、
        //    popup には text effect 適用なし — popup 用途では outline / shadow / blur 不要)
        let popup_runs = enqueue_runs(
            &scene.popup_primitives,
            &mut self.popup_rect,
            &mut self.popup_line,
            &mut self.popup_glyph,
            None,
            &self.device,
            &self.queue,
            self.size,
        );

        // 7. upload (rect/line/texture の instance buffer を 1 度に GPU へ転送、glyph は enqueue 内で済)
        self.rect.upload(&self.queue, self.size);
        self.line.upload(&self.queue, self.size);
        self.texture.upload(&self.queue, self.size);
        self.popup_rect.upload(&self.queue, self.size);
        self.popup_line.upload(&self.queue, self.size);

        // 8. encode (base pass: clear + 全 base run を call order で render)
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
                Some((&self.texture, &self.texture_store)),
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
                None,
                &mut pass,
                self.size,
            );
        }

        // end_frame: glyph cache eviction を進める + text_effect も同様に eviction (= 5sec 未使用で
        // composite texture を destroy、 既存 GlyphPipeline と同 idiom)
        self.glyph.end_frame();
        self.popup_glyph.end_frame();
        self.text_effect.end_frame(&mut self.texture_store);

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();

        // M14 Phase 93 (daw_01 #063): この frame の composite target を解放 (in-use 解除 + idle evict)。
        // base pass は既に submit 済 (= composite texture を sample 済) なので安全。
        self.composite_pool.end_cycle(&mut self.texture_store);
        Ok(())
    }

    /// M14 Phase 93 (daw_01 #063): composite target pool を即座に空にする (全 target を destroy)。
    /// 通常は `MAX_IDLE_CYCLES` 未使用で自動 evict されるが、 project / scene を閉じて VRAM を
    /// すぐ返したい場合に明示的に呼ぶ。
    pub fn clear_composite_cache(&mut self) {
        self.composite_pool.clear(&mut self.texture_store);
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
    /// M14 Phase 93 (daw_01 #063): `composite_scene_to_texture` の要求サイズが
    /// `max_texture_dimension_2d` を超過 (= wgpu の texture 作成 panic を caller protect)。
    CompositeTargetTooLarge { width: u32, height: u32, max: u32 },
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SurfaceUnavailable(s) => write!(f, "wgpu surface 利用不能: {s}"),
            Self::CompositeTargetTooLarge { width, height, max } => write!(
                f,
                "composite target size {width}x{height} exceeds max_texture_dimension_2d {max}"
            ),
        }
    }
}

impl std::error::Error for RenderError {}

/// M14 Phase 74 (daw_01 #045 §B): D3D11 shared NT handle texture import の失敗種別。
///
/// - `WrongBackend`: `create_texture_from_d3d11_shared_handle` を non-DX12 backend で呼んだ
///   (= Vulkan / GL 強制環境)。 fail-soft で caller が fallback 経路 (BGRA CPU upload) に
///   切替えるべき。
/// - `OpenSharedHandle`: `ID3D12Device::OpenSharedHandle` が失敗 (= invalid handle / ACL /
///   resource 既 release 等)。 string は Windows HRESULT を含む。
/// - `FormatMismatch`: caller が `format` 引数で要求した wgpu format と imported texture の
///   実 format が齟齬 (= 現状 wgpu::hal::dx12::texture_from_raw は format check しないので
///   この variant は将来用予約)。
#[derive(Debug)]
pub enum RendererError {
    WrongBackend(wgpu::Backend),
    OpenSharedHandle(String),
    /// M14 Phase 75 (daw_01 #046): Vulkan path での `wgpu_hal::vulkan::Device::
    /// texture_from_d3d11_shared_handle` 失敗。 driver が `D3D11_TEXTURE` handle type を
    /// report しない / invalid handle 等。 string は wgpu HAL error を含む。
    VulkanImportFailed(String),
    /// M14 Phase 75 (daw_01 #046): Vulkan adapter が `VULKAN_EXTERNAL_MEMORY_WIN32` feature を
    /// サポートしていない (= AMD / Intel 一部 driver の既知の罠)。 caller は fallback 経路に
    /// 切替えるべき。
    FeatureUnsupported(&'static str),
    FormatMismatch { requested: wgpu::TextureFormat },
}

impl std::fmt::Display for RendererError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongBackend(b) => {
                write!(f, "D3D11 shared handle import requires DX12 or Vulkan backend, current = {b:?}")
            }
            Self::OpenSharedHandle(s) => write!(f, "DX12 OpenSharedHandle failed: {s}"),
            Self::VulkanImportFailed(s) => {
                write!(f, "Vulkan texture_from_d3d11_shared_handle failed: {s}")
            }
            Self::FeatureUnsupported(name) => {
                write!(f, "wgpu feature unsupported on this adapter: {name}")
            }
            Self::FormatMismatch { requested } => {
                write!(f, "imported texture format mismatch: requested {requested:?}")
            }
        }
    }
}

impl std::error::Error for RendererError {}
