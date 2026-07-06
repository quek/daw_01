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
    /// 現在の物理ピクセルサイズ。
    size: PhysicalSize,
    /// Window の所有権 (drop 順序のため最後)
    _window: Arc<W>,
}

impl<W: WindowBackend + Send + Sync + 'static> Renderer<W> {
    /// ui-core の text 計測 (`TextMetrics`) にこの renderer 所有の `FontSystem` を貸す。
    /// `UiHost::frame_with_fonts` に渡すと、ui-core が測定用に別 `FontSystem` を二重ロード
    /// する無駄を無くせる (measure と raster が同一 font DB / shape 設定を共有する SSoT)。
    pub fn font_system_mut(&mut self) -> &mut glyphon::FontSystem {
        self.glyph.font_system_and_swash().0
    }

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

    // ============================================================
    // M14 Phase 133 (daw_01 #111): 映像効果フレームワーク用 texture interop primitive
    // ============================================================

    /// handle が指す `wgpu::Texture` への参照を返す (destroy 済 / 未知の handle は `None`)。
    ///
    /// daw_01 の映像効果チェーンが、 合成済トラック画像 ([`Self::composite_scene_to_texture`] の戻り) や
    /// 動画フレーム handle を **自前の effect pipeline の sampler に bind** するための入口。 効果の定義
    /// (WGSL / パラメータ表 / ping-pong / 履歴) は daw_01 ドメインなので、 gui_01 はこの生 texture を渡す
    /// だけで「効果とは何か」 を知らない (SSoT)。 gui_01 自身の text effect compositor が `TextureStore` の
    /// 同 method で blur / composite pass を組んでいるのと同型の primitive。
    ///
    /// 戻りは `&wgpu::Texture` (`&self` 借用)。 同じ renderer の `&mut self` メソッド
    /// ([`Self::create_render_target`] / [`Self::create_texture`] 等) と借用が衝突する場合は、
    /// `wgpu::Texture` は Arc-backed で clone が安価なので `renderer.raw_texture(h).cloned()` で所有権を
    /// 取ってから `&mut` メソッドを呼ぶ (内部の async readback `offscreen.rs` も同パターンで handle を clone)。
    #[must_use]
    pub fn raw_texture(&self, handle: TextureHandle) -> Option<&wgpu::Texture> {
        self.texture_store.raw_texture(handle)
    }

    /// `RENDER_ATTACHMENT | TEXTURE_BINDING` な空 texture を確保し、 `(handle, color_attachment_view)` を
    /// 返す (映像効果の出力 / ping-pong 中間 / 履歴ターゲット用)。
    ///
    /// - `handle`: store 登録済なので、 効果適用後にそのまま [`Scene::push_textured_quad`] で base scene へ
    ///   戻して sample できる (texture pipeline の sampler/layout で bind 済)。 別の effect pass の sample
    ///   入力にしたい場合は [`Self::raw_texture`] で生 texture を取り、 自前 bind group を作る。
    /// - 戻りの `wgpu::TextureView` は `begin_render_pass` の `color_attachments[].view` 用 (= 効果 pass を
    ///   ここへ描く)。 view は使い終えたら drop してよい (sampling 用 view は store の bind_group 内に別途保持)。
    /// - `format` は base pass に揃える (preview = [`Self::surface_format`] / export = `OffscreenRenderer::target_format`)。
    ///
    /// # lifecycle (caller 管理)
    /// [`Self::create_texture`] と同じ texture pool 上の handle。 [`Self::composite_scene_to_texture`] の
    /// renderer-managed handle (次の `render()` まで有効、 caller は destroy しない) と違い、 こちらは renderer が
    /// **recycle しない** ので、 不要になったら [`Self::destroy_texture`] で解放する。 `(chain, size)` ごとに
    /// 2〜3 枚 + 履歴 1 枚を frame 跨ぎで使い回す想定。 `render()` 冒頭の composite pool eviction はこの handle を
    /// **触らない** (= caller が destroy するまで生存)。
    ///
    /// # submit 順序の契約
    /// 効果 pass を **自前 encoder に積んで `queue.submit` してから** [`Self::render`] を呼ぶこと。 GPU は submit
    /// 順に実行するので、 同一 frame 内の「create → 効果 pass 描画 (submit A) → 最終 handle を push して render
    /// (submit B)」 は安全 ([`Self::composite_scene_to_texture`] = #063 と同じ「別 submit なら安全」、 CLAUDE.md
    /// wgpu 罠「LAST WRITE WINS の対」)。 **履歴 (feedback) target** も「前 frame の write (submit) → 今 frame の
    /// sample (submit)」 の順なので安全。 ただし **同一 render pass で同じ texture を sample と render target の
    /// 両方にしない** (ping-pong で読みと書きを別 texture に分ける)。
    pub fn create_render_target(
        &mut self,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> (TextureHandle, wgpu::TextureView) {
        self.texture_store.create_render_target(
            &self.device,
            self.texture.sampler(),
            self.texture.texture_bind_group_layout(),
            format,
            width,
            height,
        )
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
        // M14 Phase 93 (daw_01 #063): 直前フレームに composite された target を解放 (in-use 解除 +
        // idle evict)。 **render の冒頭**で呼ぶことで、 surface 取得失敗 (Timeout / Occluded の
        // frame-skip / SurfaceUnavailable) で早期 return しても pool が in-use のまま膨らむ leak を
        // 防ぐ。 ここで in-use を解除しても、 この frame で sample される composite target は handle
        // 経由で texture_store から引かれる (= destroy されない限り valid)、 かつ end_cycle は
        // idle>閾値 の **未使用** target しか destroy しないので base pass の sampling は壊れない。
        self.composite_pool.end_cycle(&mut self.texture_store);

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

