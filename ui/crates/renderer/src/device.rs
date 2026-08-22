// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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
//!
//! # GPU デバイスは「失われうる資源」 (daw_01 r.md #42)
//!
//! Windows のスリープで GPU が電源断されると Vulkan は `VK_ERROR_DEVICE_LOST` を返し、
//! wgpu-core は `Device::lose()` で **その `wgpu::Device` を恒久的に無効化**する
//! (`wgpu-core/src/device/resource.rs::lose` が `valid` を false に固定)。
//! wgpu のエラーシンクは `ErrorType::DeviceLost` を握り潰す (`wgpu/src/backend/wgpu_core.rs:297`
//! `ErrorType::DeviceLost => return, // will be surfaced via callback`) ので、 アプリからは
//! **panic も Err も観測できず**、 以後 `get_current_texture` が黙って `Validation` を
//! 返し続けるだけになる (= 落ちないのに永久に描けない)。 唯一の検出口は
//! [`wgpu::Device::set_device_lost_callback`]、 唯一の復旧は「device と全 GPU 資源の作り直し」
//! (wgpu doc `api/surface_texture.rs` `CurrentSurfaceTexture::Lost`)。
//!
//! そこで GPU 依存の資産をすべて [`GpuState`] に束ね、 `Renderer` は
//! `Option<GpuState>` として持つ。 消失時は丸ごと捨てて [`Renderer::recreate`] で作り直す。
//! font DB ([`FontAssets`]) と texture id 空間 (`next_texture_id`) は GPU に依存しないので
//! **世代をまたいで持ち越す**。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use daw_ui_platform::{PhysicalSize, WindowBackend};

use crate::composite::{composite_scene, CompositePool};
use crate::fonts::FontAssets;
use crate::pipelines::{
    enqueue_runs, glyph::GlyphPipeline, line::LinePipeline, prepare_text_effects,
    rect::RectPipeline, render_runs, text_effect::TextEffectCompositor,
    texture::TexturePipeline,
};
use crate::scene::{Scene, TextureHandle};
use crate::texture_store::TextureStore;

/// `Timeout` が何回続いたら surface を再 configure するか。
const TIMEOUT_RECONFIGURE_AT: u32 = 8;
/// 同じく、 何回続いたら「一時障害」 としてエラーに昇格するか (60 ≒ 1 秒 @60fps)。
const TIMEOUT_ESCALATE_AT: u32 = 60;

/// `Timeout` が `n` 回連続したときに取るべき措置。
///
/// 「無言でフレームを捨て続ける」 (旧実装は `Timeout | Occluded => return Ok(())` で
/// 何回続いても警告も昇格もしなかった) を構造的に禁止するための純ロジック。
/// GPU 無しでテストできるようここに切り出す。
///
/// **`Occluded` はここに含めない**。 最小化 / 他ウィンドウの背後は wgpu の doc どおり
/// **正常な状態**であって障害ではないので、 カウントも警告もせず黙ってスキップする
/// (再生中に最小化しただけで毎秒エラーが出る、 という誤検出を構造的に避ける)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeoutAction {
    /// 1 回目だけ警告を出す (以後は黙ってスキップ)。
    WarnOnce,
    /// スキップを続ける。
    Continue,
    /// surface を再 configure してからスキップを続ける。
    Reconfigure,
    /// 一時障害としてエラーに昇格する (カウンタは 0 に戻し、 以後この周期を繰り返す)。
    Escalate,
}

pub(crate) fn timeout_action(consecutive: u32) -> TimeoutAction {
    if consecutive >= TIMEOUT_ESCALATE_AT {
        TimeoutAction::Escalate
    } else if consecutive == TIMEOUT_RECONFIGURE_AT {
        TimeoutAction::Reconfigure
    } else if consecutive == 1 {
        TimeoutAction::WarnOnce
    } else {
        TimeoutAction::Continue
    }
}

/// surface 由来の失敗 (`Validation` / reconfigure しても取れない `Outdated`/`Lost`) を
/// 「device lost 相当」 へ昇格させる閾値。
///
/// # なぜ必要か (r.md #42 レビュー指摘)
///
/// 復旧の起動条件を `set_device_lost_callback` **一点**に依存させると、
/// **device は valid のまま surface だけが恒久的に壊れる** 経路で #42 と同じ
/// 「落ちないのに永久に描けない」 状態に戻る。 wgpu 29 に実在する経路:
///
/// `Surface::configure` は hal を叩く **前** に `presentation` を `take()` して `None` に
/// する (wgpu-core `device/resource.rs:5161`)。 hal 側が device 由来でないエラー
/// (Vulkan `create_swapchain` の `ERROR_SURFACE_LOST_KHR` 等) で失敗すると
/// `ConfigureSurfaceError::InvalidSurface` になり **`lose()` を通らない**ため、
/// `presentation` は `None` のまま残る。 以後 `get_current_texture` は
/// `SurfaceError::NotConfigured` → `ErrorType::Validation`
/// (wgpu-core `present.rs:162` / `present.rs:56-65`) → `SurfaceStatus::Validation` を
/// 返し続け、 device は valid なので `lost` フラグは永久に立たない。
///
/// wgpu の doc (`api/surface_texture.rs` の `CurrentSurfaceTexture::Lost`) は
/// 「device 全体が lost でなければ `Instance::create_surface()` で surface を作り直せ」
/// と言っており、 [`Renderer::recreate`] は Instance/Surface/Device ごと作り直すので
/// **この昇格はそのまま正しい remedy** になる。
///
/// 閾値は「連続失敗回数」 と「経過時間」 の **両方**。 回数だけだと 1 フレームの
/// blip で誤発火し、 時間だけだと 1 回きりの失敗で発火する。 モニタ切替 / 解像度変更の
/// ような正当な過渡状態 (~1-2 秒) を外すため 2 秒を採る。
const SURFACE_FAILURE_ESCALATE_COUNT: u32 = 4;
const SURFACE_FAILURE_ESCALATE_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

/// 連続 `count` 回・最初の失敗から `elapsed` 経過した surface 失敗を、
/// device lost 相当として復旧起動すべきか。 GPU 無しでテストできるよう純関数に切り出す。
pub(crate) fn surface_failure_escalates(count: u32, elapsed: std::time::Duration) -> bool {
    count >= SURFACE_FAILURE_ESCALATE_COUNT && elapsed >= SURFACE_FAILURE_ESCALATE_AFTER
}

/// 昇格 (= 自動 recreate) を 1 つの不調エピソード内で何回まで試みるか。
///
/// recreate が成功するのに描画が直らない (= surface が本当に死んでいる) 場合、
/// 上限が無いと 2 秒周期で「recreate → 派生キャッシュ全再構築 (画像 / サムネイルの
/// 再 decode)」 を回し続けて CPU を焼く。 wgpu 公認の remedy を数回試して駄目なら諦め、
/// エラーを返し続ける (ログはレート制限済み)。 成功フレームで 0 に戻る。
const MAX_SURFACE_ESCALATIONS: u32 = 3;

/// surface 失敗の理由文字列 (ログ / `RenderError` 用)。
const RECONFIGURE_FAILED: &str = "reconfigure 後も surface を取得できない";
const TIMEOUT_STREAK: &str = "surface acquire timeout が 60 フレーム連続";
const SURFACE_VALIDATION: &str = "Surface::get_current_texture が validation error を返す";

/// GPU に依存する全資産。device lost で丸ごと捨てて作り直す単位。
struct GpuState {
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
    /// **この `GpuState` はもう使えない** (= 作り直しが必要) を表すフラグ。
    ///
    /// 立てるのは 2 経路: (1) wgpu の device lost callback (**任意スレッド**から
    /// 呼ばれうる `Send + 'static` 要求なので `Arc<AtomicBool>`)、
    /// (2) surface 由来の連続失敗の昇格 ([`surface_failure_escalates`])。
    /// どちらも意味は同じ「この世代を捨てて `recreate` しろ」 なので 1 つの状態に集約する
    /// (復旧トリガを callback 一点に依存させない = 単一障害点を作らない)。
    lost: Arc<AtomicBool>,
    /// acquire `Timeout` の連続回数 ([`timeout_action`] の入力)。 成功フレーム /
    /// `Occluded` (= 正常な最小化) で 0 に戻す。
    consecutive_timeouts: u32,
    /// surface 由来の連続失敗 `(最初の失敗時刻, 連続回数)`。 成功フレームで `None`。
    surface_failures: Option<(std::time::Instant, u32)>,
}

impl GpuState {
    /// wgpu を初期化して GPU 資産一式を作る。 `next_texture_id` は前世代から引き継ぐ
    /// (0 なら新規)。
    fn new<W: WindowBackend + Send + Sync + 'static>(
        window: &Arc<W>,
        size: PhysicalSize,
        next_texture_id: u32,
    ) -> Result<Self, RendererInitError> {
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

        // device lost の **唯一の**検出口 (module doc 参照)。 エラー経路では観測できない。
        let lost = Arc::new(AtomicBool::new(false));
        device.set_device_lost_callback({
            let lost = Arc::clone(&lost);
            move |reason, msg| {
                // `Destroyed` は `Device::destroy` を呼んだ側の意図的な teardown であって
                // 障害ではない。 これで復旧を起動すると自前の破棄でループしうるので分ける。
                if matches!(reason, wgpu::DeviceLostReason::Destroyed) {
                    tracing::debug!(message = %msg, "wgpu device destroyed");
                    return;
                }
                lost.store(true, Ordering::Release);
                // 任意スレッドから呼ばれる。 ここでは記録のみ (復旧は GUI スレッドが
                // 次フレームの `render()` → `RenderError::DeviceLost` を見て駆動する)。
                tracing::warn!(?reason, message = %msg, "wgpu device lost");
            }
        });

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
        let texture_store = TextureStore::new_starting_at(next_texture_id);
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
            lost,
            consecutive_timeouts: 0,
            surface_failures: None,
        })
    }


    fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// surface 由来の失敗を 1 件記録し、 **device lost 相当へ昇格すべきなら `true`**。
    ///
    /// 昇格した場合は `lost` を自分で立てる (= 以後 `is_live()` は false、
    /// `render()` は即 `DeviceLost`、 caller の復旧経路が `recreate` する)。
    /// これで「復旧トリガは device lost callback だけ」 という単一障害点が外れる。
    fn note_surface_failure(&mut self, what: &str) -> bool {
        let now = std::time::Instant::now();
        let (first_at, count) = match self.surface_failures {
            Some((first_at, count)) => (first_at, count.saturating_add(1)),
            None => (now, 1),
        };
        self.surface_failures = Some((first_at, count));
        if !surface_failure_escalates(count, now.duration_since(first_at)) {
            return false;
        }
        self.surface_failures = None;
        tracing::warn!(
            count,
            reason = what,
            "surface が継続的に失敗しているため device lost 相当として作り直す"
        );
        self.lost.store(true, Ordering::Release);
        true
    }
}

/// 描画器本体。アプリ層が1つ持ち、フレーム毎に `render(&Scene)` を呼ぶ。
pub struct Renderer<W: WindowBackend + Send + Sync + 'static> {
    /// GPU 資産。`None` = device 消失中 (= [`Self::recreate`] 待ち)。
    gpu: Option<GpuState>,
    /// CPU 側フォント資産。**device をまたいで保持**する (font DB 再走査を起こさない)。
    fonts: FontAssets,
    /// `TextureHandle` id の単調増加カウンタ (世代をまたいで引き継ぐ)。
    /// 詳細は [`TextureStore::new_starting_at`] の doc を参照。
    next_texture_id: u32,
    /// 直近に確定した surface format。 GPU 消失中でも caller が pipeline format を
    /// 問い合わせられるようここに写す (再生成でも同じ adapter/surface なら同値)。
    surface_format: wgpu::TextureFormat,
    /// surface 失敗を device lost 相当へ昇格させた回数 ([`MAX_SURFACE_ESCALATIONS`] 上限)。
    /// **`recreate` をまたいで保持**し、 成功フレームで 0 に戻す
    /// (= 作り直しても直らない状況で 2 秒周期の再生成ループに陥らないため)。
    surface_escalations: u32,
    /// 現在の物理ピクセルサイズ。
    size: PhysicalSize,
    /// Window の所有権 (Surface 再生成に必要なので保持)。
    window: Arc<W>,
}

impl<W: WindowBackend + Send + Sync + 'static> Renderer<W> {
    /// ui-core の text 計測 (`TextMetrics`) にこの renderer 所有の `FontSystem` を貸す。
    /// `UiHost::frame_with_fonts` に渡すと、ui-core が測定用に別 `FontSystem` を二重ロード
    /// する無駄を無くせる (measure と raster が同一 font DB / shape 設定を共有する SSoT)。
    ///
    /// CPU 資産なので **GPU 消失中でも使える** (= 消失中も `ui.frame` は完走し、
    /// キーボードショートカット・保存・終了が効き続ける)。
    pub fn font_system_mut(&mut self) -> &mut glyphon::FontSystem {
        &mut self.fonts.font_system
    }

    /// 同期的に wgpu を初期化。
    ///
    /// # Errors
    /// アダプタ取得失敗・デバイス取得失敗・サーフェス作成失敗。
    pub fn new(window: Arc<W>) -> Result<Self, RendererInitError> {
        let size = window.inner_size();
        let gpu = GpuState::new(&window, size, 0)?;
        let surface_format = gpu.config.format;
        Ok(Self {
            gpu: Some(gpu),
            fonts: FontAssets::new(),
            next_texture_id: 0,
            surface_format,
            surface_escalations: 0,
            size,
            window,
        })
    }

    /// GPU 資産を全部捨てて作り直す (device lost からの復旧、 daw_01 r.md #42)。
    ///
    /// `fonts` と texture id 空間は引き継ぐ。 **既存の `TextureHandle` はすべて無効**に
    /// なるので、 caller は自前の handle キャッシュを破棄して再アップロードすること
    /// (id は連続するので、 破棄し忘れた handle が新テクスチャに化けることはない = 描画 skip)。
    ///
    /// # Errors
    /// アダプタ取得失敗・デバイス取得失敗・サーフェス作成失敗 (= まだ GPU が戻っていない)。
    /// 呼び出し側は backoff して再試行する。
    pub fn recreate(&mut self) -> Result<(), RendererInitError> {
        // **旧 GpuState を先に Drop させる**。 `self.gpu = Some(GpuState::new(..))` と 1 行で
        // 書くと RHS 評価時にまだ旧 Surface / swapchain が生きており、 同一 HWND に対する
        // 2 個目の swapchain 作成が Vulkan で `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`
        // (wgpu-hal vulkan/swapchain/native.rs:230) になる。 この失敗は「復帰しない」 という
        // 形でしか出ず、 実機スリープテストでしか見つからない。
        if let Some(old) = self.gpu.take() {
            // 世代をまたぐ id 空間の継続 (別名衝突による無言の誤描画を構造的に防ぐ)。
            //
            // `max` を取るのが要点: 消失中に `create_texture` が払い出した
            // backing 無し handle ([`Self::dead_texture_handle`]) は `self.next_texture_id`
            // だけを進めており、 旧 store の `next_id` には反映されていない。
            // 旧 store の値をそのまま代入すると id が **巻き戻り**、 消失中に配った
            // handle が新テクスチャを指してしまう。
            self.next_texture_id = self.next_texture_id.max(old.texture_store.next_id());
            drop(old);
        }
        let gpu = GpuState::new(&self.window, self.size, self.next_texture_id)?;
        self.surface_format = gpu.config.format;
        self.gpu = Some(gpu);
        tracing::info!(
            next_texture_id = self.next_texture_id,
            "gpu recreated"
        );
        Ok(())
    }

    /// GPU 資産が生きているか (= `render` / texture API が実効を持つか)。
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.gpu.as_ref().is_some_and(|g| !g.is_lost())
    }

    /// 生きている GPU 資産への `&mut` (消失中は `None`)。
    fn live_gpu(&mut self) -> Option<&mut GpuState> {
        match self.gpu.as_mut() {
            Some(g) if !g.is_lost() => Some(g),
            _ => None,
        }
    }

    /// GPU 消失中に払い出す **backing の無い** handle。
    ///
    /// `TextureHandle` に「無効」 を表す値は無いので、 id 空間だけ 1 進めて未登録の id を返す。
    /// destroy 済 handle と同じ扱い (upload は no-op、 描画は skip) になり、 かつ次世代 store は
    /// この id より後から採番するので新テクスチャと衝突しない。
    fn dead_texture_handle(&mut self) -> TextureHandle {
        self.next_texture_id = self.next_texture_id.saturating_add(1);
        let id = std::num::NonZeroU32::new(self.next_texture_id)
            .expect("next_texture_id は saturating_add で 1 以上");
        TextureHandle::from_raw(id)
    }

    // ============================================================
    // M14 Phase 71 (daw_01 #043): texture pool public API
    // ============================================================

    /// 指定サイズの空 RGBA8UnormSrgb texture を確保し、 `TextureHandle` を返す。
    /// sRGB encoded RGBA8 入力前提 (= PNG decode 結果 / FFmpeg sws_scale RGBA 出力)。
    /// `width` / `height` = 0 は 1 に clamp。
    ///
    /// GPU 消失中は backing の無い handle を返す (= destroy 済 handle と同じ扱い。
    /// panic しない。 [`Self::dead_texture_handle`] の doc 参照)。
    pub fn create_texture(&mut self, width: u32, height: u32) -> TextureHandle {
        let Some(gpu) = self.live_gpu() else {
            return self.dead_texture_handle();
        };
        gpu.texture_store.create(
            &gpu.device,
            gpu.texture.sampler(),
            gpu.texture.texture_bind_group_layout(),
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
        let Some(gpu) = self.live_gpu() else {
            return self.dead_texture_handle();
        };
        gpu.texture_store.create(
            &gpu.device,
            gpu.texture.sampler(),
            gpu.texture.texture_bind_group_layout(),
            wgpu::TextureFormat::Bgra8UnormSrgb,
            width,
            height,
        )
    }

    /// RGBA8 (`width * height * 4` bytes) で texture content を上書き。
    /// destroy 済 handle / size 不一致は no-op (debug build では panic)。
    /// handle が BGRA texture で作成済の場合も no-op + debug panic (cross-format protect)。
    pub fn upload_texture_rgba(&mut self, handle: TextureHandle, data: &[u8]) {
        let Some(gpu) = self.live_gpu() else { return };
        gpu.texture_store.upload_with_format(
            &gpu.queue,
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
        let Some(gpu) = self.live_gpu() else { return };
        gpu.texture_store.upload_with_format(
            &gpu.queue,
            handle,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            bgra,
        );
    }

    /// texture を解放。 既に解放された handle に対する操作は no-op。 以後 `texture_size` は `None`、
    /// `push_textured_quad` で render しても描画 skip (panic しない)。
    pub fn destroy_texture(&mut self, handle: TextureHandle) {
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.texture_store.destroy(handle);
        }
    }

    /// texture の native (width, height) を返す。 destroy 済は `None`。
    /// arrangement clip thumbnail の aspect-fit 計算 (daw_01 #044) で widget 内部から参照される。
    #[must_use]
    pub fn texture_size(&self, handle: TextureHandle) -> Option<(u32, u32)> {
        self.gpu.as_ref()?.texture_store.size(handle)
    }

    /// M14 Phase 73 (daw_01 #045): texture の format を返す (debug / test 用)。
    /// destroy 済は `None`。 通常 caller は handle 発行時の format を覚えていれば良いので、
    /// production path で参照する必要はない (= sampling は bind_group 経由で format-agnostic)。
    #[must_use]
    pub fn texture_format(&self, handle: TextureHandle) -> Option<wgpu::TextureFormat> {
        self.gpu.as_ref()?.texture_store.format(handle)
    }

    /// 物理サイズが変わったとき呼ぶ。
    ///
    /// GPU 消失中でもサイズは覚えておく (次の [`Self::recreate`] がこのサイズで surface を
    /// 作るので、 消失中にリサイズされても復帰後に正しい寸法になる)。
    pub fn resize(&mut self, new_size: PhysicalSize) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.size = new_size;
        if let Some(gpu) = self.live_gpu() {
            gpu.config.width = new_size.width;
            gpu.config.height = new_size.height;
            gpu.surface.configure(&gpu.device, &gpu.config);
        }
    }

    pub fn size(&self) -> PhysicalSize {
        self.size
    }

    /// 内部 `wgpu::Device` (GPU 消失中は `None`)。
    pub fn device(&self) -> Option<&wgpu::Device> {
        self.gpu.as_ref().map(|g| &g.device)
    }

    /// 内部 `wgpu::Queue` (GPU 消失中は `None`)。
    pub fn queue(&self) -> Option<&wgpu::Queue> {
        self.gpu.as_ref().map(|g| &g.queue)
    }

    /// base pass の color format。 GPU 消失中も **直近に確定した値**を返す
    /// (caller の pipeline format 決定を Option 汚染しないため)。
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.surface_format
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
        self.gpu.as_ref()?.texture_store.raw_texture(handle)
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
    ///
    /// GPU 消失中は `None` (= `TextureView` は device 無しでは作れないので handle だけ返す訳にいかない)。
    pub fn create_render_target(
        &mut self,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> Option<(TextureHandle, wgpu::TextureView)> {
        let gpu = self.live_gpu()?;
        Some(gpu.texture_store.create_render_target(
            &gpu.device,
            gpu.texture.sampler(),
            gpu.texture.texture_bind_group_layout(),
            format,
            width,
            height,
        ))
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
    /// - GPU 消失中は [`RenderError::DeviceLost`]。
    /// - `width` / `height` が `max_texture_dimension_2d` を超える場合
    ///   [`RenderError::CompositeTargetTooLarge`] (= wgpu の texture 作成 panic を caller protect)。
    pub fn composite_scene_to_texture(
        &mut self,
        scene: &Scene,
        width: u32,
        height: u32,
    ) -> Result<TextureHandle, RenderError> {
        let fonts = &mut self.fonts;
        let Some(gpu) = (match self.gpu.as_mut() {
            Some(g) if !g.is_lost() => Some(g),
            _ => None,
        }) else {
            return Err(RenderError::DeviceLost);
        };
        let max = gpu.device.limits().max_texture_dimension_2d;
        if width > max || height > max {
            return Err(RenderError::CompositeTargetTooLarge { width, height, max });
        }
        Ok(composite_scene(
            scene,
            width,
            height,
            gpu.config.format,
            &gpu.device,
            &gpu.queue,
            fonts,
            &mut gpu.rect,
            &mut gpu.line,
            &mut gpu.glyph,
            &mut gpu.texture,
            &mut gpu.text_effect,
            &mut gpu.texture_store,
            &mut gpu.composite_pool,
        ))
    }

    /// surface 由来の失敗を記録し、 返すべきエラーを決める。
    ///
    /// 連続失敗が [`surface_failure_escalates`] の閾値を超えたら **device lost 相当**に
    /// 昇格させて `DeviceLost` を返す (caller の復旧経路が `recreate` する)。
    /// ただし 1 エピソードあたり [`MAX_SURFACE_ESCALATIONS`] 回まで。
    fn surface_failure(&mut self, what: &'static str, transient: RenderError) -> RenderError {
        if self.surface_escalations >= MAX_SURFACE_ESCALATIONS {
            return transient;
        }
        let Some(gpu) = self.gpu.as_mut() else {
            return RenderError::DeviceLost;
        };
        if !gpu.note_surface_failure(what) {
            return transient;
        }
        self.surface_escalations = self.surface_escalations.saturating_add(1);
        if self.surface_escalations >= MAX_SURFACE_ESCALATIONS {
            tracing::error!(
                attempts = self.surface_escalations,
                "surface を作り直しても描画が回復しない (以後は自動再生成を止める)"
            );
        }
        RenderError::DeviceLost
    }

    /// 1 フレーム分の surface texture を取得する。
    ///
    /// - `Ok(Some((frame, reconfigure_after_present)))`: 描ける。
    /// - `Ok(None)`: このフレームはスキップ (最小化 / 一時的な acquire timeout)。
    /// - `Err(..)`: 描けない。 `DeviceLost` なら caller は [`Self::recreate`] へ。
    fn acquire_frame(&mut self) -> Result<Option<(wgpu::SurfaceTexture, bool)>, RenderError> {
        let Some(gpu) = self.gpu.as_mut() else {
            return Err(RenderError::DeviceLost);
        };
        // device lost は wgpu のエラーシンクで握り潰されて `Validation` として現れる
        // (module doc 参照) ので、 **`lost` フラグを見て振り分ける** のが第一の判別。
        // フラグが立たないまま surface だけ壊れる経路もあるので、 その場合は
        // 連続失敗を数えて `surface_failure` が device lost 相当へ昇格させる。
        // `Suboptimal` は描いたあとに再 configure する (wgpu doc の推奨)。
        match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => {
                gpu.consecutive_timeouts = 0;
                gpu.surface_failures = None;
                self.surface_escalations = 0;
                Ok(Some((t, false)))
            }
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                gpu.consecutive_timeouts = 0;
                gpu.surface_failures = None;
                self.surface_escalations = 0;
                Ok(Some((t, true)))
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                match gpu.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(t) => {
                        gpu.consecutive_timeouts = 0;
                        gpu.surface_failures = None;
                        self.surface_escalations = 0;
                        Ok(Some((t, false)))
                    }
                    wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                        gpu.consecutive_timeouts = 0;
                        gpu.surface_failures = None;
                        self.surface_escalations = 0;
                        Ok(Some((t, true)))
                    }
                    _ if gpu.is_lost() => Err(RenderError::DeviceLost),
                    _ => Err(self.surface_failure(
                        RECONFIGURE_FAILED,
                        RenderError::SurfaceTransient(RECONFIGURE_FAILED),
                    )),
                }
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                // 最小化 / 他ウィンドウの背後。 wgpu の doc どおり **正常な状態** なので
                // 障害として扱わない (カウントもしない = 復帰後に誤って昇格させない)。
                gpu.consecutive_timeouts = 0;
                gpu.surface_failures = None;
                Ok(None)
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                // acquire がタイムアウトした = 異常。 **無言でスキップし続けない**よう
                // 連続回数で reconfigure → エラー昇格と段階を上げる。
                gpu.consecutive_timeouts = gpu.consecutive_timeouts.saturating_add(1);
                match timeout_action(gpu.consecutive_timeouts) {
                    TimeoutAction::WarnOnce => {
                        tracing::warn!("surface acquire timeout (フレームスキップ開始)");
                    }
                    TimeoutAction::Continue => {}
                    TimeoutAction::Reconfigure => {
                        gpu.surface.configure(&gpu.device, &gpu.config);
                    }
                    TimeoutAction::Escalate => {
                        // 次の周期でまた reconfigure を試せるようカウンタを畳む
                        // (= 昇格したまま latch して再試行しなくなるのを防ぐ)。
                        gpu.consecutive_timeouts = 0;
                        if gpu.is_lost() {
                            return Err(RenderError::DeviceLost);
                        }
                        return Err(self.surface_failure(
                            TIMEOUT_STREAK,
                            RenderError::SurfaceTransient(TIMEOUT_STREAK),
                        ));
                    }
                }
                Ok(None)
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                if gpu.is_lost() {
                    return Err(RenderError::DeviceLost);
                }
                // **ここが r.md #42 の再発点**。 `configure` が device 由来でない理由で
                // 失敗すると presentation が None のまま残り、 以後 `NotConfigured` が
                // 永久に `Validation` として返り続ける (device は valid なので `lost` は
                // 立たない)。 連続失敗を数えて device lost 相当へ昇格させ、 Instance ごと
                // 作り直す ([`surface_failure_escalates`] の doc 参照)。
                Err(self.surface_failure(
                    SURFACE_VALIDATION,
                    RenderError::Validation(SURFACE_VALIDATION.to_string()),
                ))
            }
        }
    }

    /// Scene を 1 フレームとして描画。
    ///
    /// # Errors
    /// - [`RenderError::DeviceLost`]: GPU が失われた (または surface が継続的に壊れていて
    ///   作り直しが必要)。 caller は [`Self::recreate`] を呼び、 自前の `TextureHandle`
    ///   キャッシュを破棄して再アップロードすること。
    /// - [`RenderError::SurfaceTransient`]: surface だけの一時障害 (次フレームで再試行)。
    /// - [`RenderError::Validation`]: 本物の validation error (= バグ)。
    #[allow(clippy::too_many_lines)]
    pub fn render(&mut self, scene: &Scene) -> Result<(), RenderError> {
        let size = self.size;
        if !self.is_live() {
            return Err(RenderError::DeviceLost);
        }

        // M14 Phase 93 (daw_01 #063): 直前フレームに composite された target を解放 (in-use 解除 +
        // idle evict)。 **render の冒頭**で呼ぶことで、 surface 取得失敗 (Timeout / Occluded の
        // frame-skip / device lost) で早期 return しても pool が in-use のまま膨らむ leak を
        // 防ぐ。 ここで in-use を解除しても、 この frame で sample される composite target は handle
        // 経由で texture_store から引かれる (= destroy されない限り valid)、 かつ end_cycle は
        // idle>閾値 の **未使用** target しか destroy しないので base pass の sampling は壊れない。
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.composite_pool.end_cycle(&mut gpu.texture_store);
        }

        // 1. サーフェステクスチャ取得 (wgpu 29 は CurrentSurfaceTexture enum を返す)。
        let Some((frame, reconfigure_after_present)) = self.acquire_frame()? else {
            return Ok(()); // フレームスキップ (最小化 / 一時的な timeout)
        };

        let fonts = &mut self.fonts;
        let Some(gpu) = self.gpu.as_mut() else {
            return Err(RenderError::DeviceLost);
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        // 2. begin_frame: 各 pipeline の scratch / pool を reset
        gpu.rect.begin_frame();
        gpu.line.begin_frame();
        gpu.glyph.begin_frame(&gpu.queue, size);
        gpu.popup_rect.begin_frame();
        gpu.popup_line.begin_frame();
        gpu.popup_glyph.begin_frame(&gpu.queue, size);
        gpu.texture.begin_frame();
        gpu.text_effect.begin_frame();

        // 3. encoder を **先に** 作る (M14 Phase 78): text effect の pre-pass (offscreen
        //    glyph + blur H/V + composite) を base pass より前に同 encoder に積むため。
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("daw-ui frame encoder"),
        });

        // 4. M14 Phase 78 (daw_01 #049): effect 付き Glyph primitive を offscreen で render → texture
        //    に焼いて Primitive::Texture に substitute。 effect 無し / 他 type は pass-through。
        let base_primitives_substituted = prepare_text_effects(
            &scene.primitives,
            &mut gpu.text_effect,
            &gpu.device,
            &gpu.queue,
            &mut encoder,
            fonts,
            &mut gpu.texture_store,
            gpu.texture.sampler(),
            gpu.texture.texture_bind_group_layout(),
        );

        // 5. base pass: substituted primitives を call order で walk、同 type 連続を 1 run に enqueue
        let base_runs = enqueue_runs(
            &base_primitives_substituted,
            &mut gpu.rect,
            &mut gpu.line,
            &mut gpu.glyph,
            Some(&mut gpu.texture),
            &gpu.device,
            &gpu.queue,
            fonts,
            size,
        );

        // 6. popup pass: scene.popup_primitives を同様に enqueue (texture は base のみ、 #043、
        //    popup には text effect 適用なし — popup 用途では outline / shadow / blur 不要)
        let popup_runs = enqueue_runs(
            &scene.popup_primitives,
            &mut gpu.popup_rect,
            &mut gpu.popup_line,
            &mut gpu.popup_glyph,
            None,
            &gpu.device,
            &gpu.queue,
            fonts,
            size,
        );

        // 7. upload (rect/line/texture の instance buffer を 1 度に GPU へ転送、glyph は enqueue 内で済)
        gpu.rect.upload(&gpu.device, &gpu.queue, size);
        gpu.line.upload(&gpu.device, &gpu.queue, size);
        gpu.texture.upload(&gpu.queue, size);
        gpu.popup_rect.upload(&gpu.device, &gpu.queue, size);
        gpu.popup_line.upload(&gpu.device, &gpu.queue, size);

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
                &gpu.rect,
                &gpu.line,
                &gpu.glyph,
                Some((&gpu.texture, &gpu.texture_store)),
                &mut pass,
                size,
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
                &gpu.popup_rect,
                &gpu.popup_line,
                &gpu.popup_glyph,
                None,
                &mut pass,
                size,
            );
        }

        // end_frame: glyph cache eviction を進める + text_effect も同様に eviction (= 5sec 未使用で
        // composite texture を destroy、 既存 GlyphPipeline と同 idiom)
        gpu.glyph.end_frame();
        gpu.popup_glyph.end_frame();
        gpu.text_effect.end_frame(&mut gpu.texture_store);

        gpu.queue.submit(std::iter::once(encoder.finish()));
        frame.present();

        // `Suboptimal` だったフレームは present 後に再 configure する (wgpu doc の推奨。
        // 描く前に configure すると取得済み texture が無効化されるので present の後)。
        if reconfigure_after_present {
            gpu.surface.configure(&gpu.device, &gpu.config);
        }
        Ok(())
    }

    /// M14 Phase 93 (daw_01 #063): composite target pool を即座に空にする (全 target を destroy)。
    /// 通常は `MAX_IDLE_CYCLES` 未使用で自動 evict されるが、 project / scene を閉じて VRAM を
    /// すぐ返したい場合に明示的に呼ぶ。
    pub fn clear_composite_cache(&mut self) {
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.composite_pool.clear(&mut gpu.texture_store);
        }
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

/// 描画失敗の分類 (daw_01 r.md #42 で細分化)。
///
/// 旧実装は全部 `SurfaceUnavailable(String)` に潰していたため、 caller は「次フレーム
/// 再試行すれば直るのか / device を作り直すべきなのか / 自分のバグなのか」 を区別できず、
/// 結果として **ログを吐くだけ**の 1 分岐しか書けなかった (= スリープ復帰で 54,043 行の
/// 同一エラーを出しながら永久に描けない状態)。 復旧の可否を型で表す。
#[derive(Debug)]
pub enum RenderError {
    /// GPU device が失われた。 caller は [`Renderer::recreate`] を呼び、 自前の
    /// `TextureHandle` キャッシュを破棄して再構築すること。 同じ device は二度と復活しない
    /// (wgpu-core `Device::lose` が `valid` を恒久的に false にする)。
    DeviceLost,
    /// surface だけの一時障害。 次フレームで再試行すればよい (device は生きている)。
    SurfaceTransient(&'static str),
    /// 本物の validation error (= 呼び出し側のバグ)。
    Validation(String),
    /// offscreen readback (`poll` / `map_async`) の失敗。
    Readback(String),
    /// M14 Phase 93 (daw_01 #063): `composite_scene_to_texture` の要求サイズが
    /// `max_texture_dimension_2d` を超過 (= wgpu の texture 作成 panic を caller protect)。
    CompositeTargetTooLarge { width: u32, height: u32, max: u32 },
}

impl RenderError {
    /// device 再生成が必要か (= caller は `recreate` してから再構築する)。
    #[must_use]
    pub fn is_device_lost(&self) -> bool {
        matches!(self, Self::DeviceLost)
    }
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeviceLost => write!(f, "wgpu device が失われた (要再生成)"),
            Self::SurfaceTransient(s) => write!(f, "wgpu surface 一時障害: {s}"),
            Self::Validation(s) => write!(f, "wgpu validation error: {s}"),
            Self::Readback(s) => write!(f, "wgpu readback 失敗: {s}"),
            Self::CompositeTargetTooLarge { width, height, max } => write!(
                f,
                "composite target size {width}x{height} exceeds max_texture_dimension_2d {max}"
            ),
        }
    }
}

impl std::error::Error for RenderError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// acquire timeout を **無言で捨て続ける** のを構造的に禁止する昇格ロジック。
    /// 一瞬の timeout で復旧を走らせず、 かつ永久に黙らないこと。
    #[test]
    fn timeout_action_escalates_only_after_a_second_of_timeouts() {
        // 1 回目だけ警告 (= 「起きていること」 がログに残る)。
        assert_eq!(timeout_action(1), TimeoutAction::WarnOnce);
        // 以降はログを埋めない。
        assert_eq!(timeout_action(2), TimeoutAction::Continue);
        assert_eq!(timeout_action(7), TimeoutAction::Continue);
        // 8 連続でまず surface 再 configure を試す。
        assert_eq!(timeout_action(TIMEOUT_RECONFIGURE_AT), TimeoutAction::Reconfigure);
        assert_eq!(timeout_action(TIMEOUT_RECONFIGURE_AT + 1), TimeoutAction::Continue);
        // 60 連続 (≒1 秒) 未満では昇格しない。
        assert_eq!(timeout_action(TIMEOUT_ESCALATE_AT - 1), TimeoutAction::Continue);
        assert_eq!(timeout_action(TIMEOUT_ESCALATE_AT), TimeoutAction::Escalate);
        assert_eq!(timeout_action(u32::MAX), TimeoutAction::Escalate);
    }

    /// surface 由来の連続失敗を device lost 相当へ昇格させる条件 (r.md #42 レビュー指摘)。
    ///
    /// device lost callback が発火しない経路 (`configure` 失敗で presentation が None の
    /// まま残り、以後 `NotConfigured` が `Validation` として永久に返る) でも復旧が
    /// 起動するための安全網。回数と経過時間の **両方** を要求する:
    /// - 回数だけ → 1 フレームの blip で誤発火する
    /// - 時間だけ → 単発の失敗から 2 秒後に誤発火する
    #[test]
    fn surface_failure_escalates_needs_both_count_and_elapsed() {
        use std::time::Duration;
        let enough = SURFACE_FAILURE_ESCALATE_AFTER;
        let count = SURFACE_FAILURE_ESCALATE_COUNT;

        // 回数・時間とも足りて初めて昇格する。
        assert!(surface_failure_escalates(count, enough));
        assert!(surface_failure_escalates(count + 10, enough + Duration::from_secs(5)));

        // 回数が足りない (= 一瞬の blip) では昇格しない。
        assert!(!surface_failure_escalates(count - 1, enough));
        assert!(!surface_failure_escalates(1, Duration::from_secs(60)));

        // 時間が足りない (= 高フレームレートで一気に数が伸びただけ) では昇格しない。
        assert!(!surface_failure_escalates(count, enough - Duration::from_millis(1)));
        assert!(!surface_failure_escalates(1000, Duration::ZERO));

        // モニタ切替 / 解像度変更のような正当な過渡状態 (~1 秒) を巻き込まない。
        assert!(!surface_failure_escalates(60, Duration::from_secs(1)));
    }
}

