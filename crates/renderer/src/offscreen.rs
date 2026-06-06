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

use std::sync::mpsc::{sync_channel, Receiver};

use daw_ui_platform::PhysicalSize;

use crate::composite::{composite_scene, CompositePool};
use crate::device::{RenderError, RendererInitError};
use crate::pipelines::{
    enqueue_runs, glyph::GlyphPipeline, line::LinePipeline, prepare_text_effects,
    rect::RectPipeline, render_runs, text_effect::TextEffectCompositor,
    texture::TexturePipeline,
};
use crate::scene::{Scene, TextureHandle};
use crate::texture_store::TextureStore;

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
    /// M14 Phase 71 (daw_01 #043): textured-quad pipeline (offscreen でも base pass で texture 描画可能)。
    texture: TexturePipeline,
    /// M14 Phase 71: texture handle → wgpu::Texture + bind_group の lookup table。
    texture_store: TextureStore,
    /// M14 Phase 78 (daw_01 #049): text effect compositor (Renderer<W> と同 idiom、 PNG
    /// snapshot test でも outline / shadow / blur / rotation 効果を捉えるために必要)。
    text_effect: TextEffectCompositor,
    /// M14 Phase 93 (daw_01 #063): `composite_scene_to_texture` の render target を size 別に
    /// 使い回す pool (Renderer<W> と同 idiom)。
    composite_pool: CompositePool,
    /// M14 Phase 106 (daw_01 #077): `submit_readback` / `finish_readback` の async 経路で
    /// target texture + staging buffer を使い回す ring (export pipeline の composite ∥ readback ∥
    /// encode overlap 用)。 同期 `render_to_rgba` は使わない (= single-shot は毎回新規)。
    readback: ReadbackRing,
    size: PhysicalSize,
}

/// M14 Phase 106 (daw_01 #077): [`OffscreenRenderer::submit_readback`] が返す in-flight readback の
/// ハンドル。 [`OffscreenRenderer::finish_readback`] に **値渡しで** 回収する (= 二重 finish を
/// move 検査で構造的に防ぐ)。
///
/// 1 つの `OffscreenRenderer` から払い出された token のみ、 その renderer に返せる。 別 renderer に
/// 渡したり、 [`OffscreenRenderer::clear_readback_cache`] 後の stale token を渡すと
/// [`finish_readback`](OffscreenRenderer::finish_readback) が `Err` を返す。
#[must_use = "PendingReadback を finish_readback で回収しないと ring slot が in-flight のまま leak する"]
pub struct PendingReadback {
    /// `ReadbackRing::slots` への index。
    slot: usize,
    /// stale token 検出用 generation。 slot 払い出し時の世代と一致しなければ無効。
    generation: u64,
}

/// 1 in-flight readback 分の GPU リソース束。 `target` に描画 → `staging` へ copy → `map_async` する。
struct ReadbackSlot {
    /// COPY_SRC | RENDER_ATTACHMENT な描画先 (= `render_to_rgba` の per-call target と同 descriptor)。
    target: wgpu::Texture,
    /// `target` の color attachment view。
    view: wgpu::TextureView,
    /// COPY_DST | MAP_READ な readback staging (size = `padded * height`)。
    staging: wgpu::Buffer,
    width: u32,
    height: u32,
    /// 256-align 済の行 stride (`bytes_per_row`)。
    padded: u32,
    /// 払い出し中か (= submit 済で finish 未回収)。 `false` の slot のみ再利用する。
    in_flight: bool,
    /// この slot を払い出した世代 ([`PendingReadback`] の stale 検出用、 払い出すたび +1)。
    generation: u64,
    /// `map_async` 完了通知の受信端 (submit_readback で設定、 finish_readback で take)。
    rx: Option<Receiver<Result<(), wgpu::BufferAsyncError>>>,
    /// この readback を submit した時の index (= その submission だけを `finish_readback` で待つ)。
    submission: Option<wgpu::SubmissionIndex>,
}

/// async readback の target+staging を size 別に使い回す ring。 `OffscreenRenderer` が 1 つ所有する。
///
/// `CompositePool` と同じ「in-use フラグ付きで未使用 slot を再払い出し」 方式。 ただし readback slot は
/// `finish_readback` で明示的に解放されるまで in-flight (= staging が map 待ち) なので、 同時 in-flight
/// 数だけ slot が増える。 daw_01 の double-buffer は 2、 triple でも 3 で頭打ち。
struct ReadbackRing {
    slots: Vec<ReadbackSlot>,
    /// 払い出し世代の単調増加カウンタ。
    next_generation: u64,
}

impl ReadbackRing {
    fn new() -> Self {
        Self { slots: Vec::new(), next_generation: 1 }
    }

    /// `(width, height)` の空き slot を 1 つ確保 (in-flight にマーク) して `(index, generation)` を返す。
    /// 同 size の未使用 slot があれば再利用、 無ければ新規生成する。
    fn acquire(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> (usize, u64) {
        let generation = self.next_generation;
        self.next_generation += 1;

        if let Some(idx) = self
            .slots
            .iter()
            .position(|s| !s.in_flight && s.width == width && s.height == height)
        {
            let s = &mut self.slots[idx];
            s.in_flight = true;
            s.generation = generation;
            s.rx = None;
            s.submission = None;
            return (idx, generation);
        }

        let (target, view, staging, padded) = make_readback_target(device, format, width, height);
        self.slots.push(ReadbackSlot {
            target,
            view,
            staging,
            width,
            height,
            padded,
            in_flight: true,
            generation,
            rx: None,
            submission: None,
        });
        (self.slots.len() - 1, generation)
    }

    /// 全 slot を破棄して空にする (GPU リソース即解放)。 in-flight slot も落とすので、 残っている
    /// `PendingReadback` token は以後 stale 扱いになる。
    fn clear(&mut self) {
        self.slots.clear();
    }
}

/// readback 1 セット分の (target texture, view, staging buffer, padded 行 stride) を作る。
/// `render_to_rgba` (毎回新規) と [`ReadbackRing::acquire`] (slot 新規生成時) が共有する。
fn make_readback_target(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView, wgpu::Buffer, u32) {
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("daw-ui offscreen target"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let unpadded = width * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("daw-ui offscreen readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    (target, view, staging, padded)
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
        let texture = TexturePipeline::new(&device, target_format);
        let texture_store = TextureStore::new();
        let text_effect = TextEffectCompositor::new(&device, &queue);
        let composite_pool = CompositePool::new();
        let readback = ReadbackRing::new();

        Ok(Self {
            device,
            queue,
            target_format,
            rect,
            line,
            glyph,
            texture,
            texture_store,
            text_effect,
            composite_pool,
            readback,
            size: PhysicalSize { width: width.max(1), height: height.max(1) },
        })
    }

    pub fn size(&self) -> PhysicalSize {
        self.size
    }

    pub fn target_format(&self) -> wgpu::TextureFormat {
        self.target_format
    }

    // ============================================================
    // M14 Phase 71 (daw_01 #043): texture pool public API (Renderer<W> と同 idiom)
    // ============================================================

    /// 指定サイズの空 RGBA8UnormSrgb texture を確保。
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

    /// M14 Phase 73 (daw_01 #045): 指定サイズの空 BGRA8UnormSrgb texture を確保 (snapshot test 用)。
    /// 詳細は `Renderer::create_texture_bgra` 参照。
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

    /// RGBA8 で texture content を上書き。 destroy 済 / size 不一致 / format 不一致は no-op。
    pub fn upload_texture_rgba(&mut self, handle: TextureHandle, data: &[u8]) {
        self.texture_store.upload_with_format(
            &self.queue,
            handle,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            data,
        );
    }

    /// M14 Phase 73 (daw_01 #045): BGRA8 で texture content を上書き。 詳細は
    /// `Renderer::upload_texture_bgra` 参照。
    pub fn upload_texture_bgra(&mut self, handle: TextureHandle, bgra: &[u8]) {
        self.texture_store.upload_with_format(
            &self.queue,
            handle,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            bgra,
        );
    }

    /// texture を解放。 既に解放済 / 未知の handle は no-op。
    pub fn destroy_texture(&mut self, handle: TextureHandle) {
        self.texture_store.destroy(handle);
    }

    /// texture の native (width, height)。 destroy 済は `None`。
    #[must_use]
    pub fn texture_size(&self, handle: TextureHandle) -> Option<(u32, u32)> {
        self.texture_store.size(handle)
    }

    /// M14 Phase 73 (daw_01 #045): texture の format。 destroy 済は `None`。
    #[must_use]
    pub fn texture_format(&self, handle: TextureHandle) -> Option<wgpu::TextureFormat> {
        self.texture_store.format(handle)
    }

    /// M14 Phase 93 (daw_01 #063): `scene.primitives` を `width × height` の GPU 常駐 sampleable
    /// texture に合成し、 その [`TextureHandle`] を返す (`Renderer<W>` と同 API、 export 経路用)。
    ///
    /// 透明 clear / popup 対象外 / size 別 pool 使い回しは `Renderer::composite_scene_to_texture` と
    /// 同じ。 返る texture の format は `Rgba8UnormSrgb` (= offscreen pipeline の target format)。
    ///
    /// # Errors
    /// `width` / `height` が `max_texture_dimension_2d` を超える場合
    /// [`RenderError::CompositeTargetTooLarge`]。
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
            self.target_format,
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

    /// M14 Phase 93 (daw_01 #063): composite target pool を即座に空にする (全 target を destroy)。
    pub fn clear_composite_cache(&mut self) {
        self.composite_pool.clear(&mut self.texture_store);
    }

    /// 内部: `scene` を `view` (= `target` の color attachment) に描画し、 `target` → `staging` へ
    /// `copy_texture_to_buffer` する command encoder を組んで返す (`queue.submit` は **しない**)。
    ///
    /// `render_to_rgba` (同期 single-shot) と `submit_readback` (async pipeline) の両方が
    /// **この 1 経路だけ**を通るので、 同一 scene なら描画結果は bit 単位で一致する
    /// (= daw_01 #077 が要求する export/preview byte parity の SSoT)。
    ///
    /// `w`/`h` は `self.size`、 `padded` は呼び出し側が `staging` を作った時の 256-align 行 stride。
    #[allow(clippy::too_many_lines)]
    fn encode_scene_into(
        &mut self,
        scene: &Scene,
        view: &wgpu::TextureView,
        target: &wgpu::Texture,
        staging: &wgpu::Buffer,
        padded: u32,
    ) -> wgpu::CommandEncoder {
        let w = self.size.width;
        let h = self.size.height;

        // begin_frame + enqueue (call-order interleave、device.rs と同形)
        self.rect.begin_frame();
        self.line.begin_frame();
        self.glyph.begin_frame(&self.queue, self.size);
        self.texture.begin_frame();
        self.text_effect.begin_frame();

        // M14 Phase 78: encoder を **先に** 作り、 text effect pre-pass を同 encoder に積む
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("daw-ui offscreen encoder"),
        });

        // M14 Phase 78 (daw_01 #049): effect 付き Glyph を offscreen で焼いて Primitive::Texture に substitute
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
        // popup pass: OffscreenRenderer は pipeline instance を base / popup で共有するが、
        // upload の最終値が popup data になる干渉を避けるため、 device.rs と異なり同一 pipeline を
        // 流用 (offscreen は popup primitives 利用が少ないので許容)。 texture は base のみ (#043)。
        let popup_runs = enqueue_runs(
            &scene.popup_primitives,
            &mut self.rect,
            &mut self.line,
            &mut self.glyph,
            None,
            &self.device,
            &self.queue,
            self.size,
        );

        self.rect.upload(&self.queue, self.size);
        self.line.upload(&self.queue, self.size);
        self.texture.upload(&self.queue, self.size);

        // encode: base pass + (popup pass) + copy_texture_to_buffer
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("daw-ui offscreen base pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
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
        if !popup_runs.is_empty() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("daw-ui offscreen popup pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
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
                &self.rect,
                &self.line,
                &self.glyph,
                None,
                &mut pass,
                self.size,
            );
        }
        self.glyph.end_frame();
        // M14 Phase 78: text effect cache eviction (= 5sec 未使用 composite texture を destroy)
        self.text_effect.end_frame(&mut self.texture_store);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );

        encoder
    }

    /// `staging` の 256-align padded bytes を unpadded (`width * 4` 行 stride) に詰め直す。
    fn pack_unpadded(staging: &wgpu::Buffer, width: u32, height: u32, padded: u32) -> Vec<u8> {
        let unpadded = width * 4;
        let slice = staging.slice(..);
        let padded_view = slice.get_mapped_range();
        let mut out = Vec::with_capacity((unpadded * height) as usize);
        for row in 0..height {
            let s = (row * padded) as usize;
            out.extend_from_slice(&padded_view[s..s + unpadded as usize]);
        }
        drop(padded_view);
        staging.unmap();
        out
    }

    /// Scene を 1 フレーム分 render し、RGBA bytes (sRGB encoded) を返す (**同期 single-shot**)。
    ///
    /// 戻り値の bytes は行 stride `width * 4` (= unpadded、内部 staging buffer の
    /// 256-align padding は row 単位コピーで畳む)。PNG `ColorType::Rgba` にそのまま渡せる。
    ///
    /// 1 frame ごとに GPU を全 flush して CPU を待たせるので、 多数フレームを連続 readback する
    /// export では [`submit_readback`](Self::submit_readback) / [`finish_readback`](Self::finish_readback)
    /// で composite ∥ readback ∥ encode を overlap する方が速い。 本 method は snapshot / PNG 回帰用に
    /// 据え置く (出力 bytes は async 版と完全一致)。
    ///
    /// # Errors
    /// staging buffer の `map_async` / `Device::poll` が失敗した場合。
    pub fn render_to_rgba(&mut self, scene: &Scene) -> Result<Vec<u8>, RenderError> {
        // M14 Phase 93 (daw_01 #063): 直前の composite target を解放 (Renderer::render と同様、
        // readback error の早期 return でも pool が in-use のまま残らないよう **冒頭**で呼ぶ)。
        self.composite_pool.end_cycle(&mut self.texture_store);

        let w = self.size.width;
        let h = self.size.height;
        // single-shot なので毎回新規 (ring は使わない)。
        let (target, view, staging, padded) =
            make_readback_target(&self.device, self.target_format, w, h);

        let encoder = self.encode_scene_into(scene, &view, &target, &staging, padded);
        self.queue.submit(std::iter::once(encoder.finish()));

        let (tx, rx) = sync_channel::<Result<(), wgpu::BufferAsyncError>>(1);
        staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RenderError::SurfaceUnavailable(format!("offscreen poll: {e:?}")))?;
        rx.recv()
            .map_err(|e| RenderError::SurfaceUnavailable(format!("offscreen recv: {e:?}")))?
            .map_err(|e| RenderError::SurfaceUnavailable(format!("offscreen map_async: {e:?}")))?;

        Ok(Self::pack_unpadded(&staging, w, h, padded))
    }

    /// M14 Phase 106 (daw_01 #077): Scene を render + readback 予約し、 **`poll` せず即 return** する
    /// (= async readback、 export pipeline 用)。 返った [`PendingReadback`] を
    /// [`finish_readback`](Self::finish_readback) に渡して RGBA8 を回収する。
    ///
    /// 複数の `submit_readback` を `finish_readback` 前に重ねられる (in-flight は target+staging の
    /// ring で持ち回り)。 これにより daw_01 export は
    /// `submit(A) → submit(B) → finish(A) → encode(A) → finish(B)` で composite ∥ GPU readback ∥
    /// CPU encode を overlap できる。 同時 in-flight は double-buffer なら 2、 triple でも 3 で足りる。
    ///
    /// 出力 bytes は [`render_to_rgba`](Self::render_to_rgba) と **bit 単位で同一** (同じ
    /// [`encode_scene_into`](Self::encode_scene_into) 経路 + 同じ 256-align 詰め直し)。
    ///
    /// # 呼び出し順の契約 (composite を併用する場合)
    ///
    /// `composite_scene_to_texture` で焼いた texture を scene が参照する場合、 **frame ごとに**
    /// 「frame N の composite 群 → `submit_readback(N)` → (その後で) frame N+1 の composite 群」 の順を
    /// 守ること。 `submit_readback` 冒頭の `end_cycle` が frame N の composite target を再利用可能に
    /// するのは frame N の render が **submit された後**で、 GPU は submit 順に実行するため、 frame N+1 の
    /// composite が同 target を上書きするのは frame N の readback copy より後になる (= 安全)。
    /// daw_01 export の「`build_frame_scene(N)` → `submit_readback(N)`」 を毎 frame 回す自然なループは
    /// この順序を満たす。 1 frame 内で同 size の composite を複数回呼ぶ (立ち絵 group 複数) のも安全
    /// (`CompositePool` が同 cycle 内は別 target を払い出す)。
    ///
    /// # 契約 (token 回収)
    /// 払い出した `PendingReadback` は必ず `finish_readback` で回収すること。 回収しないと
    /// その slot は in-flight のまま残り再利用されない (staging buffer が leak する)。 in-flight 数は
    /// [`in_flight_readbacks`](Self::in_flight_readbacks) で観測でき、 double-buffer なら 2、 triple でも
    /// 3 で頭打ちになるはず。 [`clear_readback_cache`](Self::clear_readback_cache) で全 slot を破棄できる。
    ///
    /// # Errors
    /// daw_01 の想定 call site が `?` で受ける (spec `submit_readback(&scene)?`) ため `Result` を返すが、
    /// [`composite_scene_to_texture`](Self::composite_scene_to_texture) と同様、 描画自体は失敗しないので
    /// 現状は常に `Ok` (error 系 API の一貫性 + 将来の拡張余地)。
    pub fn submit_readback(&mut self, scene: &Scene) -> Result<PendingReadback, RenderError> {
        // render_to_rgba と同様、 直前 frame の composite target を解放 (冒頭で呼ぶ)。
        // submit_readback は render を即 submit するので、 この frame の composite target は
        // 解放マークされても submit までは valid (texture_store が handle を保持、 end_cycle は
        // destroy せず in-use 解除のみ)。 GPU は submit 順に実行するので、 後続フレームの composite が
        // 同 target を上書きするのは、 この frame の render+copy が submit された **後**になる。
        self.composite_pool.end_cycle(&mut self.texture_store);

        let w = self.size.width;
        let h = self.size.height;
        let (slot_idx, generation) =
            self.readback.acquire(&self.device, self.target_format, w, h);

        // borrow 分離: ring slot から wgpu handle (Arc backed = clone 安価) を取り出してから
        // pipelines (&mut self) を使う encode を回す。
        let (view, target, staging, padded) = {
            let s = &self.readback.slots[slot_idx];
            (s.view.clone(), s.target.clone(), s.staging.clone(), s.padded)
        };

        let encoder = self.encode_scene_into(scene, &view, &target, &staging, padded);
        let submission = self.queue.submit(std::iter::once(encoder.finish()));

        // map_async を **登録だけ** する (poll は finish_readback 側で)。 CLAUDE.md wgpu 罠:
        // 「map_async 登録 → poll」 の順を守る (逆だとコールバックが永遠に呼ばれない)。
        let (tx, rx) = sync_channel::<Result<(), wgpu::BufferAsyncError>>(1);
        staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });

        let s = &mut self.readback.slots[slot_idx];
        s.rx = Some(rx);
        s.submission = Some(submission);

        Ok(PendingReadback { slot: slot_idx, generation })
    }

    /// M14 Phase 106 (daw_01 #077): [`submit_readback`](Self::submit_readback) で予約した readback の
    /// 完了を待ち、 RGBA8 bytes (行 stride `width * 4`) を回収する。 slot は解放されて再利用可能になる。
    ///
    /// 待機は **その readback の submission のみ** を対象にする (`PollType::Wait { submission_index }`)
    /// ので、 後続の in-flight readback の進行を妨げない。
    ///
    /// # Errors
    /// - stale / 二重回収など無効な `PendingReadback` (`clear_readback_cache` 後の token 等)。
    /// - staging buffer の `map_async` / `Device::poll` 失敗。
    pub fn finish_readback(&mut self, pending: PendingReadback) -> Result<Vec<u8>, RenderError> {
        let slot = pending.slot;
        let valid = self
            .readback
            .slots
            .get(slot)
            .is_some_and(|s| s.in_flight && s.generation == pending.generation);
        if !valid {
            return Err(RenderError::SurfaceUnavailable(
                "finish_readback: stale or already-finished PendingReadback".to_string(),
            ));
        }

        let (rx, submission, w, h, padded) = {
            let s = &mut self.readback.slots[slot];
            (s.rx.take(), s.submission.take(), s.width, s.height, s.padded)
        };

        let poll = match submission {
            Some(idx) => wgpu::PollType::Wait { submission_index: Some(idx), timeout: None },
            None => wgpu::PollType::wait_indefinitely(),
        };
        self.device
            .poll(poll)
            .map_err(|e| RenderError::SurfaceUnavailable(format!("offscreen poll: {e:?}")))?;
        // wgpu 29 `PollType::Wait` は「指定 submission の完了 **と** その callback の呼び出し」 まで
        // block する保証があるので、 poll が Ok を返した時点でこの slot の map_async callback は既に
        // 発火済 = `recv()` は即座に返る (block しない)。 この poll → recv の順序依存は崩さないこと。
        rx.ok_or_else(|| {
            RenderError::SurfaceUnavailable("finish_readback: missing map channel".to_string())
        })?
        .recv()
        .map_err(|e| RenderError::SurfaceUnavailable(format!("offscreen recv: {e:?}")))?
        .map_err(|e| RenderError::SurfaceUnavailable(format!("offscreen map_async: {e:?}")))?;

        // `pack_unpadded` が `staging.unmap()` するまで in_flight=true を保つ (= 下の解放を unmap
        // より前に並べ替えると、 map 中の buffer を acquire が再払い出ししてしまう)。
        let out = {
            let staging = &self.readback.slots[slot].staging;
            Self::pack_unpadded(staging, w, h, padded)
        };
        self.readback.slots[slot].in_flight = false;
        Ok(out)
    }

    /// M14 Phase 106 (daw_01 #077): 現在 in-flight (submit 済 / finish 未回収) な readback の数。
    /// double-buffer なら 2、 triple でも 3 で頭打ちのはず。 単調増加するなら `PendingReadback` の
    /// 回収漏れ (leak) を示すので、 daw_01 側で assert / backpressure に使える。
    #[must_use]
    pub fn in_flight_readbacks(&self) -> usize {
        self.readback.slots.iter().filter(|s| s.in_flight).count()
    }

    /// M14 Phase 106 (daw_01 #077): readback ring の全 slot を破棄して GPU リソースを即解放する
    /// (export 終了 / project close 時)。 未回収の `PendingReadback` は以後 stale 扱いになる。
    ///
    /// map_async 予約中 (未 poll) の staging buffer を drop することになるが、 wgpu 29 は deferred
    /// destruction (submission 完了まで生存) で安全に処理し、 callback は drop 済 receiver へ送って
    /// 無視される (`stale_pending_after_cache_clear_errors` test で panic しないことを固定)。
    pub fn clear_readback_cache(&mut self) {
        self.readback.clear();
    }
}
