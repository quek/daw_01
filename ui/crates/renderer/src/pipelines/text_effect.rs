// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! M14 Phase 78 (daw_01 #049): text effect compositor。
//!
//! `GlyphArea` の outline / shadow / blur / rotation を組み合わせた最終 RGBA texture を
//! offscreen で焼き、 base scene には Phase 71/76 の [`TexturedQuad`] (rotation_radians 込み)
//! として push する。 effect 無し path (= [`GlyphArea::has_effects`] が false) は既存
//! `GlyphPipeline` の直接 glyphon path を維持して byte 完全互換。
//!
//! # Pipeline 構成 (5 pass / effect 付き area 1 個)
//!
//! - **A. glyph offscreen**: 自前 [`glyphon::TextRenderer`] + 自前 atlas で text を offscreen
//!   RGBA8UnormSrgb texture に焼く (= text mask)。 fill_color = `area.color`、 outline / shadow
//!   は **後段 pass** で重ねるためここでは fill のみ。
//! - **B. blur H** (`shadow_blur_px > 0` 時のみ): horizontal separable gaussian (5-tap
//!   linear-sample 最適化、 radius ≈ blur_px / 3)。 別 intermediate texture に書く。
//! - **C. blur V** (同上): vertical pass、 また別 intermediate に書く。
//! - **D. composite**: glyph mask (pass A) + blurred shadow (pass C、 or pass A if no blur) +
//!   outline 9-sample dilate を 1 fragment shader で合成 → final RGBA texture (= TextureStore
//!   に register、 base scene の TexturedQuad で sample される)。
//! - **E. main scene render** (= 既存 base pass): pass D の出力 texture を Phase 76 の vertex
//!   shader で `rotation_radians` 回転して描画。 ここは既存 `TexturePipeline` を流用。
//!
//! # Cache 戦略 (60+ fps 維持)
//!
//! [`EffectKey`] (text + style + effect params の量子化 hash) で final texture を cache。
//! `rotation_radians` は cache 外 (= base scene の vertex shader で適用するため)。 5 秒未使用で
//! eviction (= 既存 `GlyphPipeline::end_frame` と同 idiom)。
//!
//! # NaN / Infinity 正規化
//!
//! `rotation_radians` / `outline_width_px` / `shadow_blur_px` / `shadow_offset_px` の各 component
//! を [`normalize_finite`] で `is_finite()` ガード → 0.0 化。 caller 責務にしない (= Phase 76
//! の `normalize_rotation` と同 idiom、 GPU driver の sin/cos 非有限挙動を回避)。

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use wgpu::MultisampleState;

use crate::fonts::FontAssets;
use crate::scene::{Color, GlyphArea, TextureHandle};
use crate::texture_store::TextureStore;

const EVICT_AFTER_FRAMES: u64 = 300;

/// composite texture cache が保持してよい GPU バイト数の上限 (daw_01 r.md #59)。
///
/// # なぜフレーム数の TTL だけでは足りないか
///
/// [`EVICT_AFTER_FRAMES`] は「何フレーム使われなかったら捨てるか」しか決めておらず、
/// **1 entry が何バイトかを一切見ていない**。 `EffectKey` は text 内容と font size を含む
/// (= 鍵空間が無制限) 一方、 composite texture は「テキスト実寸 + outline + shadow 余白」で
/// 最大 4096×4096×4 B = 64 MiB まで振れる。 字幕の font size はプレビュー窓幅 / project 幅の
/// スケールで決まる (`group_compose.rs`) ので、 **窓をドラッグリサイズする / FontSize を
/// 変調する** だけで毎フレーム新しい鍵になり、 300 フレームぶんの巨大テクスチャが同時に
/// 生き残る。 実測 (このモジュールの回帰テスト): 400 フレームで 300 枚 = 176 MiB。
/// 1920 幅の字幕なら 1 枚 2 MB 級なので現実には数百 MiB に届く。
///
/// # なぜ 32 MiB か
///
/// このキャッシュが本当に必要なのは「今フレームに出ている effect 付きテキスト」だけで、
/// それ以外は投機的な再利用にすぎない。 1080p 全幅の字幕 1 枚が概ね 2 MB なので、
/// 32 MiB は同時表示しうる枚数より 1 桁多い余裕がある。 現フレームで使われた entry は
/// 予算超過でも決して捨てない (捨てるとその場で焼き直しになる) ので、 予算は
/// 「投機的に持ち越す量の上限」として働く。
const COMPOSITE_CACHE_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
/// offscreen composite texture format。 sRGB で blend が正しく走る。 既存
/// `OffscreenRenderer` の format (`Rgba8UnormSrgb`) と同じ。
pub const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// NaN / ±Infinity を 0.0 に正規化 (Phase 76 の `normalize_rotation` と同 idiom)。
#[inline]
#[must_use]
pub fn normalize_finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

// ============================================================
// EffectKey (cache key)
// ============================================================

/// effect 適用済 final texture の cache key。 `rotation_radians` は外す
/// (= base scene vertex shader で適用、 同じ最終 texture を異なる rotation で再利用可能)。
#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
struct EffectKey {
    text_hash: u64,
    /// M14 Phase 121 (daw_01 #096): 同 text+size+effect でも font 違いは別 composite texture。
    /// これを欠くと「同じ歌詞を別 font で重ねる」 ケースで先に焼いた composite に化ける。
    font_hash: u64,
    font_size_bits: u32,
    line_height_bits: u32,
    color_rgba8: [u8; 4],
    outline_color_rgba8: [u8; 4],
    outline_width_q: u16,
    shadow_color_rgba8: [u8; 4],
    shadow_offset_q: (i16, i16),
    shadow_blur_q: u16,
}

impl EffectKey {
    fn from_area(area: &GlyphArea) -> Self {
        let mut h = DefaultHasher::new();
        area.text.hash(&mut h);
        let text_hash = h.finish();
        let mut hf = DefaultHasher::new();
        area.resolved_font_family().hash(&mut hf);
        Self {
            text_hash,
            font_hash: hf.finish(),
            font_size_bits: area.font_size.to_bits(),
            line_height_bits: area.line_height.to_bits(),
            color_rgba8: rgba8(area.color),
            outline_color_rgba8: rgba8(area.outline_color),
            outline_width_q: quantize_q16(normalize_finite(area.outline_width_px).max(0.0)),
            shadow_color_rgba8: rgba8(area.shadow_color),
            shadow_offset_q: (
                quantize_q16_signed(normalize_finite(area.shadow_offset_px.0)),
                quantize_q16_signed(normalize_finite(area.shadow_offset_px.1)),
            ),
            shadow_blur_q: quantize_q16(normalize_finite(area.shadow_blur_px).max(0.0)),
        }
    }
}

#[inline]
fn rgba8(c: Color) -> [u8; 4] {
    [
        (c.r.clamp(0.0, 1.0) * 255.0) as u8,
        (c.g.clamp(0.0, 1.0) * 255.0) as u8,
        (c.b.clamp(0.0, 1.0) * 255.0) as u8,
        (c.a.clamp(0.0, 1.0) * 255.0) as u8,
    ]
}

/// `px * 16` を `u16` に丸める (= 0.0625 px 単位の量子化、 視覚差なし)。
#[inline]
fn quantize_q16(px: f32) -> u16 {
    let q = (px * 16.0).round();
    q.clamp(0.0, f32::from(u16::MAX)) as u16
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
fn quantize_q16_signed(px: f32) -> i16 {
    let q = (px * 16.0).round();
    q.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

// ============================================================
// CachedEffect (cache value)
// ============================================================

/// glyphon layout buffer cache 1 entry。 `GlyphPipeline.CachedBuffer` と同 idiom。
struct CachedBuffer {
    buffer: Buffer,
    last_seen_frame: u64,
}

struct CachedEffect {
    /// final composite texture (= TextureStore に登録済の TexturedQuad 用)。
    handle: TextureHandle,
    /// composite texture 内での text rect の left, top (= caller 描画 rect の起点を逆算)。
    /// padding 分のオフセット (= outline + shadow 余白) を含む。
    text_offset_x: f32,
    text_offset_y: f32,
    width: u32,
    height: u32,
    /// M14 Phase 122 (daw_01 #097): 実測テキスト寸法 (advance / block 高さ)。 cache HIT 経路でも
    /// `aligned_origin` を適用するために保持する。 align は baked texture を変えない (= EffectKey に
    /// align を入れず cache 共有) ので、 配置だけ毎フレーム実測値から再計算する。
    text_w: f32,
    text_h: f32,
    last_seen_frame: u64,
}

impl CachedEffect {
    /// この entry が GPU 上で占めるバイト数 ([`OFFSCREEN_FORMAT`] は 4 B/px)。
    fn gpu_bytes(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * 4
    }
}

// ============================================================
// Uniform structs
// ============================================================

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct BlurUniform {
    /// (1/w, 1/h, _pad, _pad)
    texel_inv: [f32; 4],
    /// (center, neighbor1, neighbor2, _pad)
    weights: [f32; 4],
    /// (0, offset1, offset2, _pad) — linear-sample offsets in texel
    offsets: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct CompositeUniform {
    tex_size: [f32; 4],
    outline: [f32; 4],
    outline_color: [f32; 4],
    shadow_offset: [f32; 4],
    shadow_color: [f32; 4],
}

// ============================================================
// TextEffectCompositor
// ============================================================

pub struct TextEffectCompositor {
    cache: HashMap<EffectKey, CachedEffect>,

    // glyphon resources (independent — format = OFFSCREEN_FORMAT)
    /// 全 pooled `TextRenderer` で共有する glyph atlas (= `GlyphPipeline` が 1 atlas を pool 全体で
    /// 共有するのと同 idiom、 glyphon 設計どおり安全)。 **frame 内では append-only に保つこと**: 同一
    /// submit に積んだ複数 offscreen pass が submit 時にこの atlas を読むため、 frame 途中で trim/shrink
    /// すると他 pass の glyph が消える。 縮小が必要なら全 render pass 完了後 (= `end_frame`) に限る。
    atlas: TextAtlas,
    /// glyphon `Cache` (内部 `Arc<Inner>` で cheap)。 frame 内で offscreen glyph pass 数まで
    /// `renderers` / `viewports` pool を grow する際の `Viewport::new` に必要なので保持する。
    glyphon_cache: Cache,
    /// M14 Phase 112 (daw_01 #084): offscreen glyph pass **1 つにつき 1 `TextRenderer`** (= 1 内部
    /// vertex_buffer)。 単一 instance を使い回すと、 同一 encoder/submit 内で複数 effect-ful area の
    /// `prepare` が同じ vertex_buffer を `queue.write_buffer` で上書きし、 `render` の `pass.draw` は
    /// **submit 時にその buffer を遅延読み**するため、 全 offscreen target が **最後に prepare された 1 枚**
    /// の文字列で焼ける (= 2 枚同時 active で両方同じ text、 #084 の症状)。 `GlyphPipeline.renderers` と
    /// 同 idiom: frame 内で必要数まで grow、 shrink しない (allocate は grow 1 度だけ)。
    renderers: Vec<TextRenderer>,
    /// M14 Phase 112 (daw_01 #084): `renderers` と lockstep の per-pass `Viewport`。 offscreen target は
    /// area ごとに composite size が異なり、 viewport の resolution uniform は **draw (submit) 時に shader が
    /// pixel→NDC 変換で読む** + `prepare` 時に bounds clamp に使われる (glyphon `text_render.rs`)。 単一
    /// viewport を per-area `update` すると、 renderer pool だけ直しても submit 時に LAST WRITE WINS で全
    /// draw が **最後の area の size** を読み、 size の違う overlay が mis-scale / off-target になる。 idx
    /// ごとに別 params_buffer を持たせて隔離する。
    viewports: Vec<Viewport>,
    /// M14 Phase 112 (daw_01 #084): 現フレームで払い出した offscreen pass 数。 `begin_frame` で 0 に
    /// reset、 `render_glyph_offscreen` で 1 つ払い出すごとに increment (= `GlyphPipeline.next_renderer_idx`
    /// と同 idiom)。 cache hit の area は `render_effect` 冒頭で early-return するので index を進めない。
    next_renderer_idx: usize,
    /// text layout buffer cache。 `CachedBuffer` で `last_seen_frame` を持って `end_frame` で
    /// `EVICT_AFTER_FRAMES` 経過 entry を retain で削除 (= 既存 `GlyphPipeline.cache` と同 idiom、
    /// 長期セッションで dynamic text 増加に対する memory growth を防ぐ)。
    buffer_cache: HashMap<u64, CachedBuffer>,

    // wgpu blur + composite pipelines
    blur_h_pipeline: wgpu::RenderPipeline,
    blur_v_pipeline: wgpu::RenderPipeline,
    composite_pipeline: wgpu::RenderPipeline,

    blur_bgl: wgpu::BindGroupLayout,
    composite_bgl: wgpu::BindGroupLayout,

    sampler: wgpu::Sampler,

    frame_counter: u64,
}

impl TextEffectCompositor {
    /// wgpu resource を 1 度に揃える init 関数。 3 pipeline + 2 BGL + 2 uniform buffer + sampler +
    /// glyphon resource (atlas / viewport / cache) を flat に組むため line 数大、 helper 分割すると
    /// 1 度しか呼ばない関数の散らばりが返って読みにくい (= device.rs / offscreen.rs の render() と
    /// 同 idiom)。
    #[allow(clippy::too_many_lines)]
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        // glyphon Cache は内部 Arc<Inner> なので新規 instance でも cheap。 frame 内で
        // renderers / viewports pool を grow する際の `Viewport::new` に使うので Self に保持する。
        let cache_handle = Cache::new(device);
        let atlas = TextAtlas::new(device, queue, &cache_handle, OFFSCREEN_FORMAT);
        // viewport / renderer は offscreen pass ごとに別 instance が要る (per-pass で size が異なる、
        // #084) ので、 ここでは作らず render_glyph_offscreen で必要数まで pool を grow する。

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("text_effect sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        // M14 Phase 78 BUG FIX: uniform buffer は per-call で作る (compositor instance 内で共有
        // すると同 encoder 内 LAST WRITE WINS で全 draw が最後の uniform 値を読む)。 ここでは
        // 構造的に保持せず、 実 buffer は run_blur_pass / run_composite_pass 内で device.create_buffer。

        // === blur BindGroupLayout (src texture + sampler + uniform) ===
        let blur_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("text_effect blur bgl"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        // === composite BindGroupLayout (glyph + sampler + shadow + sampler + uniform) ===
        let composite_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("text_effect composite bgl"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        // === blur shader + pipelines (H と V) ===
        let blur_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("text_effect blur shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("text_effect_blur.wgsl").into()),
        });
        let blur_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("text_effect blur pipeline layout"),
            bind_group_layouts: &[Some(&blur_bgl)],
            immediate_size: 0,
        });
        let make_blur_pipeline = |entry: &str, label: &str| -> wgpu::RenderPipeline {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&blur_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &blur_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &blur_shader,
                    entry_point: Some(entry),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: OFFSCREEN_FORMAT,
                        blend: None, // 出力は intermediate に直接書き (blend 不要)
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let blur_horiz = make_blur_pipeline("fs_blur_h", "text_effect blur H pipeline");
        let blur_vert = make_blur_pipeline("fs_blur_v", "text_effect blur V pipeline");

        // === composite shader + pipeline ===
        let composite_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("text_effect composite shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("text_effect_composite.wgsl").into()),
        });
        let composite_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("text_effect composite pipeline layout"),
                bind_group_layouts: &[Some(&composite_bgl)],
                immediate_size: 0,
            });
        let composite_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("text_effect composite pipeline"),
            layout: Some(&composite_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &composite_shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &composite_shader,
                entry_point: Some("fs_composite"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: OFFSCREEN_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            cache: HashMap::new(),
            atlas,
            glyphon_cache: cache_handle,
            renderers: Vec::new(),
            viewports: Vec::new(),
            next_renderer_idx: 0,
            buffer_cache: HashMap::new(),
            blur_h_pipeline: blur_horiz,
            blur_v_pipeline: blur_vert,
            composite_pipeline,
            blur_bgl,
            composite_bgl,
            sampler,
            frame_counter: 0,
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame_counter += 1;
        // M14 Phase 112 (daw_01 #084): offscreen glyph pass の renderer/viewport pool index を
        // frame 頭で reset (= `GlyphPipeline::begin_frame` と同 idiom)。 pool 自体 (Vec) は保持して
        // 再利用、 index だけ巻き戻すことで frame ごとに先頭 slot から払い出し直す。
        self.next_renderer_idx = 0;
    }

    /// effect 付き area を offscreen で render し、 base scene 用の (TextureHandle, rect) を返す。
    /// cache hit ならそのまま、 miss なら encoder に Pass A-D を発行。
    ///
    /// 戻り値: `(handle, dst_rect_x, dst_rect_y, dst_rect_w, dst_rect_h)`。 `dst_rect` は
    /// composite texture の論理サイズ (= padding 込み、 text 周囲に outline + shadow 用余白を
    /// 含む) で、 base scene の TexturedQuad に push する rect そのもの。 caller 側で
    /// `area.left - text_offset_x` で composite rect の left を計算する。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn render_effect(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fonts: &mut FontAssets,
        texture_store: &mut TextureStore,
        texture_sampler: &wgpu::Sampler,
        texture_bgl: &wgpu::BindGroupLayout,
        area: &GlyphArea,
    ) -> Option<(TextureHandle, f32, f32, f32, f32)> {
        let key = EffectKey::from_area(area);

        // cache hit
        if let Some(entry) = self.cache.get_mut(&key) {
            entry.last_seen_frame = self.frame_counter;
            // M14 Phase 122 (daw_01 #097): align は baked texture を変えない (EffectKey に align を
            // 入れず cache 共有) ので、 配置だけ cache 済の実測寸法から毎フレーム再計算する。 これが
            // 無いと miss 直後の frame は aligned だが以後の hit frame で left/top 原点に戻る (jump)。
            let (origin_x, origin_y) = area.aligned_origin(entry.text_w, entry.text_h);
            let x = origin_x - entry.text_offset_x;
            let y = origin_y - entry.text_offset_y;
            return Some((entry.handle, x, y, entry.width as f32, entry.height as f32));
        }

        // cache miss — measure text + render

        // (1) measure text via glyphon Buffer::layout_runs
        let (text_w, text_h) = self.measure_text(&mut fonts.font_system, area);
        if text_w <= 0.0 || text_h <= 0.0 {
            return None;
        }

        // (2) padding 計算: outline + |shadow_offset| + shadow_blur * 3 (= 3-σ rule)
        let outline_w = normalize_finite(area.outline_width_px).max(0.0);
        let shadow_off_x = normalize_finite(area.shadow_offset_px.0);
        let shadow_off_y = normalize_finite(area.shadow_offset_px.1);
        let blur = normalize_finite(area.shadow_blur_px).max(0.0);
        let blur_radius = (blur * 3.0).ceil(); // 3-σ rule
        let pad_left = outline_w + shadow_off_x.min(0.0).abs() + blur_radius;
        let pad_right = outline_w + shadow_off_x.max(0.0) + blur_radius;
        let pad_top = outline_w + shadow_off_y.min(0.0).abs() + blur_radius;
        let pad_bottom = outline_w + shadow_off_y.max(0.0) + blur_radius;

        let composite_w = (text_w + pad_left + pad_right).ceil() as u32;
        let composite_h = (text_h + pad_top + pad_bottom).ceil() as u32;
        let composite_w = composite_w.clamp(1, 4096);
        let composite_h = composite_h.clamp(1, 4096);

        // text を offscreen 内 (pad_left, pad_top) の位置に置く
        let text_offset_x = pad_left;
        let text_offset_y = pad_top;

        // (3) Pass A: glyph offscreen — text を glyph_tex に書く
        let (glyph_handle, glyph_view) = texture_store.create_render_target(
            device,
            texture_sampler,
            texture_bgl,
            OFFSCREEN_FORMAT,
            composite_w,
            composite_h,
        );
        self.render_glyph_offscreen(
            device,
            queue,
            encoder,
            fonts,
            area,
            text_offset_x,
            text_offset_y,
            composite_w,
            composite_h,
            &glyph_view,
        );

        // (4) Pass B/C: blur (shadow_blur_px > 0 のときのみ)
        let shadow_handle = if blur > 0.0 {
            let (h_handle, h_view) = texture_store.create_render_target(
                device,
                texture_sampler,
                texture_bgl,
                OFFSCREEN_FORMAT,
                composite_w,
                composite_h,
            );
            let (v_handle, v_view) = texture_store.create_render_target(
                device,
                texture_sampler,
                texture_bgl,
                OFFSCREEN_FORMAT,
                composite_w,
                composite_h,
            );
            // H pass: glyph_handle -> h_handle
            self.run_blur_pass(
                device,
                queue,
                encoder,
                texture_store,
                glyph_handle,
                &h_view,
                composite_w,
                composite_h,
                blur,
                true, // horizontal
            );
            // V pass: h_handle -> v_handle
            self.run_blur_pass(
                device,
                queue,
                encoder,
                texture_store,
                h_handle,
                &v_view,
                composite_w,
                composite_h,
                blur,
                false, // vertical
            );
            // h は不要、 後で destroy (= queue submit 後の cleanup を意識した destroy)
            // ただし wgpu の lifetime 管理は queue.submit までは Texture 生存を保証するので
            // ここで destroy しても submit 時に encoder が参照する texture は alive。
            texture_store.destroy(h_handle);
            Some(v_handle)
        } else {
            None
        };

        // (5) Pass D: composite — glyph + shadow (blur されてれば v_handle、 さもなければ glyph 直接) + outline 9-sample
        let (final_handle, final_view) = texture_store.create_render_target(
            device,
            texture_sampler,
            texture_bgl,
            OFFSCREEN_FORMAT,
            composite_w,
            composite_h,
        );
        let shadow_for_composite = shadow_handle.unwrap_or(glyph_handle);
        self.run_composite_pass(
            device,
            queue,
            encoder,
            texture_store,
            glyph_handle,
            shadow_for_composite,
            &final_view,
            composite_w,
            composite_h,
            area,
        );

        // intermediate texture を破棄 (= cache せず、 毎回 create)
        texture_store.destroy(glyph_handle);
        if let Some(sh) = shadow_handle {
            texture_store.destroy(sh);
        }

        // (6) cache insert
        let entry = CachedEffect {
            handle: final_handle,
            text_offset_x,
            text_offset_y,
            width: composite_w,
            height: composite_h,
            text_w,
            text_h,
            last_seen_frame: self.frame_counter,
        };
        self.cache.insert(key, entry);

        // M14 Phase 122 (daw_01 #097): box + align のとき、 offscreen 内 (pad_left, pad_top) に
        // 焼いた text が screen 上で **aligned origin** に着地するよう quad を逆オフセットする。
        // box 無し時は aligned_origin が (left, top) を返すので既存挙動と byte 完全互換。
        let (origin_x, origin_y) = area.aligned_origin(text_w, text_h);
        let x = origin_x - text_offset_x;
        let y = origin_y - text_offset_y;
        Some((final_handle, x, y, composite_w as f32, composite_h as f32))
    }

    pub fn end_frame(&mut self, texture_store: &mut TextureStore) {
        let frame = self.frame_counter;
        // composite texture cache eviction (= TextureHandle を destroy する必要があるので
        // retain でなく remove ループ経由)
        let mut to_remove = Vec::new();
        for (key, entry) in &self.cache {
            if frame.saturating_sub(entry.last_seen_frame) >= EVICT_AFTER_FRAMES {
                to_remove.push(*key);
            }
        }
        for key in to_remove {
            if let Some(entry) = self.cache.remove(&key) {
                texture_store.destroy(entry.handle);
            }
        }
        self.enforce_cache_budget(texture_store, frame);
        // M14 Phase 78 review: glyphon layout buffer cache eviction (= 既存 `GlyphPipeline::end_frame`
        // と同 idiom、 dynamic text で `buffer_cache` 無制限増加するのを防ぐ)。
        self.buffer_cache
            .retain(|_, e| frame.saturating_sub(e.last_seen_frame) < EVICT_AFTER_FRAMES);
        // daw_01 r.md #59: glyph atlas を trim (= `glyphs_in_use` を clear して LRU を回す)。
        // 呼ばないと atlas texture が `max_texture_dimension_2d` (= 8192) まで倍々に grow し、
        // 最後は `PrepareError::AtlasFull` で文字が出なくなる (理由と安全性の詳細は
        // `GlyphPipeline::end_frame` の doc comment)。 この位置は struct doc の
        // 「縮小は全 render pass 完了後 (= end_frame) に限る」 制約をそのまま満たす:
        // `end_frame` 以降このフレームでは `prepare` が走らないので、 caller の encoder に
        // 積んだ offscreen pass が読む atlas 領域が submit 前に書き換わることはない。
        self.atlas.trim();
    }

    /// composite texture cache を [`COMPOSITE_CACHE_BUDGET_BYTES`] 以下に収める
    /// (daw_01 r.md #59)。 古い (= `last_seen_frame` が小さい) entry から捨てる。
    ///
    /// **現フレームで使われた entry (`last_seen_frame == frame`) は捨てない**。 それらは
    /// caller の scene が `TexturedQuad` として今まさに参照しており、 捨てれば次フレームで
    /// 即座に焼き直しになるだけで何も得しない。 結果として、 1 フレームの working set 自体が
    /// 予算を超える場合は予算を超えたまま保持する (= 描画は絶対に壊さない)。
    fn enforce_cache_budget(&mut self, texture_store: &mut TextureStore, frame: u64) {
        let mut total: u64 = self.cache.values().map(CachedEffect::gpu_bytes).sum();
        if total <= COMPOSITE_CACHE_BUDGET_BYTES {
            return;
        }
        // 古い順に並べる (現フレーム使用ぶんは候補から除外)。
        let mut victims: Vec<(u64, EffectKey)> = self
            .cache
            .iter()
            .filter(|(_, e)| e.last_seen_frame != frame)
            .map(|(k, e)| (e.last_seen_frame, *k))
            .collect();
        victims.sort_unstable_by_key(|(seen, _)| *seen);
        for (_, key) in victims {
            if total <= COMPOSITE_CACHE_BUDGET_BYTES {
                break;
            }
            if let Some(entry) = self.cache.remove(&key) {
                total -= entry.gpu_bytes();
                texture_store.destroy(entry.handle);
            }
        }
    }

    // ============================================================
    // internal helpers
    // ============================================================

    /// glyphon Buffer::layout_runs で text bounding box を取得 (= measure)。
    fn measure_text(&mut self, font_system: &mut FontSystem, area: &GlyphArea) -> (f32, f32) {
        let key = buffer_cache_key(area);
        let frame = self.frame_counter;
        let cached = self.buffer_cache.entry(key).or_insert_with(|| {
            let metrics = Metrics::new(area.font_size.max(1.0), area.line_height.max(1.0));
            let mut buf = Buffer::new(font_system, metrics);
            buf.set_size(font_system, None, None); // no wrap
            let attrs = Attrs::new().family(Family::Name(area.resolved_font_family()));
            buf.set_text(font_system, &area.text, &attrs, Shaping::Advanced, None);
            buf.shape_until_scroll(font_system, false);
            CachedBuffer { buffer: buf, last_seen_frame: frame }
        });
        cached.last_seen_frame = frame;
        // M14 Phase 122 (daw_01 #097): measure を glyph path と同一の SSoT helper に委譲。
        super::glyph::measure_layout(&cached.buffer)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_glyph_offscreen(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        fonts: &mut FontAssets,
        area: &GlyphArea,
        text_offset_x: f32,
        text_offset_y: f32,
        composite_w: u32,
        composite_h: u32,
        target_view: &wgpu::TextureView,
    ) {
        // M14 Phase 112 (daw_01 #084): この offscreen glyph pass 専用の (renderer, viewport) slot を
        // pool から払い出す。 1 slot = 1 内部 vertex_buffer + 1 resolution uniform buffer。 同一
        // encoder/submit 内で複数 effect-ful area を焼くとき単一 instance を使い回すと、 submit 時に
        // 全 draw が **最後の prepare/update** を読む (= 文字列も resolution も最後の area に化ける)。
        // idx ごとに別 buffer を持たせて隔離する (= `GlyphPipeline` の renderer pool と同 idiom)。
        // renderers / viewports は lockstep で grow させ、 同じ idx で両者を参照する。
        while self.next_renderer_idx >= self.renderers.len() {
            self.renderers.push(TextRenderer::new(
                &mut self.atlas,
                device,
                MultisampleState::default(),
                None,
            ));
            self.viewports.push(Viewport::new(device, &self.glyphon_cache));
        }
        let idx = self.next_renderer_idx;
        self.next_renderer_idx += 1;

        // この slot の viewport を offscreen size に更新。 glyphon 0.11 では `prepare` が
        // `viewport.resolution()` を bounds clamp に読み (text_render.rs:146)、 `render` が
        // `viewport.bind_group` を bind して vertex shader が pixel→NDC 変換に読む (text_render.rs:350 +
        // shader.wgsl:65、 submit 時)。 どちらも resolution に依存するので prepare の前に更新する。
        // 単一 viewport を共有すると submit 時に最後の size が全 draw に効く (= サイズ違い overlay の
        // mis-scale)。 idx ごとに別 viewport (= 別 params_buffer) なので隔離される。
        self.viewports[idx].update(
            queue,
            Resolution {
                width: composite_w,
                height: composite_h,
            },
        );

        // buffer cache key 同じ (measure 時に確保済)
        let key = buffer_cache_key(area);
        // measure を呼ばずに直接 get (なければ panic — 直前に measure_text 呼ばれてる前提)
        let buffer = &self
            .buffer_cache
            .get(&key)
            .expect("buffer cache must be primed by measure_text")
            .buffer;

        let text_area = TextArea {
            buffer,
            left: text_offset_x,
            top: text_offset_y,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: i32::try_from(composite_w).unwrap_or(i32::MAX),
                bottom: i32::try_from(composite_h).unwrap_or(i32::MAX),
            },
            default_color: GlyphColor::rgba(
                (area.color.r * 255.0).clamp(0.0, 255.0) as u8,
                (area.color.g * 255.0).clamp(0.0, 255.0) as u8,
                (area.color.b * 255.0).clamp(0.0, 255.0) as u8,
                (area.color.a * 255.0).clamp(0.0, 255.0) as u8,
            ),
            custom_glyphs: &[],
        };

        let (font_system, swash_cache) = fonts.split();
        if let Err(e) = self.renderers[idx].prepare(
            device,
            queue,
            font_system,
            &mut self.atlas,
            &self.viewports[idx],
            std::iter::once(text_area),
            swash_cache,
        ) {
            eprintln!("text_effect glyph prepare error: {e:?}");
            return;
        }

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("text_effect glyph offscreen pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            multiview_mask: None,
            occlusion_query_set: None,
        });
        if let Err(e) = self.renderers[idx].render(&self.atlas, &self.viewports[idx], &mut pass) {
            eprintln!("text_effect glyph render error: {e:?}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_blur_pass(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        texture_store: &TextureStore,
        src_handle: TextureHandle,
        target_view: &wgpu::TextureView,
        composite_w: u32,
        composite_h: u32,
        blur_px: f32,
        horizontal: bool,
    ) {
        // 5-tap linear-sample weights for sigma = blur_px / 3 (3-σ rule で radius = blur_px)
        let sigma = (blur_px / 3.0).max(0.5);
        let (weights, offsets) = compute_blur_kernel(sigma);
        let uniform = BlurUniform {
            texel_inv: [
                1.0 / composite_w as f32,
                1.0 / composite_h as f32,
                0.0,
                0.0,
            ],
            weights: [weights[0], weights[1], weights[2], 0.0],
            offsets: [0.0, offsets[1], offsets[2], 0.0],
        };
        // M14 Phase 78 BUG FIX: per-call uniform buffer (= composite と同 issue)
        let per_call_blur_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("text_effect blur uniform (per-call)"),
            size: std::mem::size_of::<BlurUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &per_call_blur_uniform_buffer,
            0,
            bytemuck::bytes_of(&uniform),
        );

        let src_bg = texture_store
            .bind_group(src_handle)
            .expect("blur src handle must exist");

        // bind_group for blur (src texture + sampler + uniform). 既存 texture_store の bind_group は
        // texture pipeline 用の layout で作られているため、 blur 用に新規 bind_group を作る必要。
        // src texture と sampler を取り出して blur_bgl で再 bind。
        let src_view = self.create_src_view_from_handle(texture_store, src_handle);
        let blur_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("text_effect blur bg"),
            layout: &self.blur_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: per_call_blur_uniform_buffer.as_entire_binding(),
                },
            ],
        });
        let _ = src_bg; // bind_group そのものは layout 不一致で reuse 不可

        let pipeline = if horizontal {
            &self.blur_h_pipeline
        } else {
            &self.blur_v_pipeline
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("text_effect blur pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            multiview_mask: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &blur_bind_group, &[]);
        pass.draw(0..6, 0..1);
    }

    #[allow(clippy::too_many_arguments)]
    fn run_composite_pass(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        texture_store: &TextureStore,
        glyph_handle: TextureHandle,
        shadow_handle: TextureHandle,
        target_view: &wgpu::TextureView,
        composite_w: u32,
        composite_h: u32,
        area: &GlyphArea,
    ) {
        let uniform = CompositeUniform {
            tex_size: [composite_w as f32, composite_h as f32, 0.0, 0.0],
            outline: [
                normalize_finite(area.outline_width_px).max(0.0),
                0.0,
                0.0,
                0.0,
            ],
            outline_color: [
                area.outline_color.r,
                area.outline_color.g,
                area.outline_color.b,
                area.outline_color.a,
            ],
            shadow_offset: [
                normalize_finite(area.shadow_offset_px.0),
                normalize_finite(area.shadow_offset_px.1),
                0.0,
                0.0,
            ],
            shadow_color: [
                area.shadow_color.r,
                area.shadow_color.g,
                area.shadow_color.b,
                area.shadow_color.a,
            ],
        };
        // M14 Phase 78 BUG FIX: composite_uniform_buffer を共有すると、 同 encoder 内で複数の
        // effect-ful GlyphArea を render する際 `queue.write_buffer` の LAST WRITE WINS で
        // 全 draw が最後の uniform 値を読む (= 全 GlyphArea の effect が最後の area の値で
        // 描画される shadow bug)。 1 render_effect 呼出ごとに per-call の uniform buffer を新規
        // 作成して、 encoder が submit されるまで生存させる (= wgpu::Buffer は Arc 内包なので
        // 関数 return 時に drop されても encoder 参照が保つ)。
        let per_call_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("text_effect composite uniform (per-call)"),
            size: std::mem::size_of::<CompositeUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &per_call_uniform_buffer,
            0,
            bytemuck::bytes_of(&uniform),
        );

        let glyph_view = self.create_src_view_from_handle(texture_store, glyph_handle);
        let shadow_view = self.create_src_view_from_handle(texture_store, shadow_handle);
        let composite_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("text_effect composite bg"),
            layout: &self.composite_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&glyph_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&shadow_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: per_call_uniform_buffer.as_entire_binding(),
                },
            ],
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("text_effect composite pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            multiview_mask: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.composite_pipeline);
        pass.set_bind_group(0, &composite_bind_group, &[]);
        pass.draw(0..6, 0..1);
    }

    /// TextureHandle から sampling 用の TextureView を取り出す。 既存 store の bind_group は
    /// texture pipeline 用 layout (1 texture + 1 sampler) なので、 blur / composite shader の
    /// 別 layout (2 texture + 2 sampler + uniform) には不一致。 raw texture を取って新 view を
    /// 作る。 view は cheap (Arc 内包) なので毎 pass 都度 create で問題なし。
    ///
    /// `self` 引数は将来 view cache 等の lifetime を Self に持たせる余地のため `&self` で残す。
    #[allow(clippy::unused_self)]
    fn create_src_view_from_handle(
        &self,
        texture_store: &TextureStore,
        handle: TextureHandle,
    ) -> wgpu::TextureView {
        let tex = texture_store
            .raw_texture(handle)
            .expect("text_effect: src TextureHandle must reference live entry");
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    }
}

#[inline]
fn buffer_cache_key(area: &GlyphArea) -> u64 {
    let mut h = DefaultHasher::new();
    area.text.hash(&mut h);
    area.font_size.to_bits().hash(&mut h);
    area.line_height.to_bits().hash(&mut h);
    // M14 Phase 121 (daw_01 #096): font も layout buffer の cache identity (glyph.rs::buffer_key と同様)。
    area.resolved_font_family().hash(&mut h);
    h.finish()
}

/// 1D separable gaussian kernel (5-tap linear-sample 最適化: center + 2 paired neighbors)。
/// 戻り値: ([center_w, n1_w, n2_w], [_, n1_off, n2_off]) where n_off は texel 単位の linear-sample
/// 補間位置。
fn compute_blur_kernel(sigma: f32) -> ([f32; 3], [f32; 3]) {
    let sigma = sigma.max(0.5);
    let radius = (3.0 * sigma).ceil().max(1.0) as i32;
    let mut raw: Vec<f32> = (-radius..=radius)
        .map(|i| (-(i as f32).powi(2) / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = raw.iter().sum();
    for w in &mut raw {
        *w /= sum;
    }
    // linear-sample compaction (radius=1, 2 を paired にする = 5 sample / pass)
    let center_idx = radius as usize;
    let center = raw[center_idx];
    // pair (i=1, 2) and (i=3, 4) if radius >= 4
    // ただし shader 側は 5-tap 固定 (center + 2 pair) なので radius が小さい場合は weight=0
    let mut weights = [center, 0.0, 0.0];
    let mut offsets = [0.0, 0.0, 0.0];
    // pair 1: indices radius+1, radius+2
    if (radius as usize) + 2 <= raw.len() - 1 + 1 && radius >= 1 {
        let w1 = raw[center_idx + 1];
        let w2 = if (radius as usize) + 2 < raw.len() {
            raw[center_idx + 2]
        } else {
            0.0
        };
        let w_combined = w1 + w2;
        if w_combined > 0.0 {
            offsets[1] = 1.0 + w2 / w_combined;
        } else {
            offsets[1] = 1.0;
        }
        weights[1] = w_combined;
    }
    // pair 2: indices radius+3, radius+4
    if radius >= 3 {
        let w3 = raw.get(center_idx + 3).copied().unwrap_or(0.0);
        let w4 = raw.get(center_idx + 4).copied().unwrap_or(0.0);
        let w_combined = w3 + w4;
        if w_combined > 0.0 {
            offsets[2] = 3.0 + w4 / w_combined;
        } else {
            offsets[2] = 3.0;
        }
        weights[2] = w_combined;
    }
    // (review) shader は 5-tap (center + ±2 pair、 ±4 texel) 固定なので、 全範囲
    // (-radius..=radius) で正規化したままだと sigma > ~1.3 で有効重み和 < 1 になり
    // shadow が「ぼけずに薄くなる」 (blur_px=30 で 2 pass 後 ≈0.13 まで減光)。
    // 5-tap がカバーする範囲だけで再正規化して減光を消す (blur 半径が ~4-5px で
    // 頭打ちになる制限は残る — それ以上は tap 数可変 / downsample 多段が必要)。
    let covered = weights[0] + 2.0 * (weights[1] + weights[2]);
    if covered > 0.0 {
        weights[0] /= covered;
        weights[1] /= covered;
        weights[2] /= covered;
    }
    (weights, offsets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipelines::glyph::DEFAULT_FONT_FAMILY;

    #[test]
    fn normalize_finite_passes_through_finite() {
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(normalize_finite(0.0), 0.0);
            assert_eq!(normalize_finite(1.5), 1.5);
            assert_eq!(normalize_finite(-1.25), -1.25);
        }
    }

    #[test]
    fn normalize_finite_zeros_non_finite() {
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(normalize_finite(f32::NAN), 0.0);
            assert_eq!(normalize_finite(f32::INFINITY), 0.0);
            assert_eq!(normalize_finite(f32::NEG_INFINITY), 0.0);
        }
    }

    #[test]
    fn blur_kernel_center_weight_is_positive() {
        let (w, _) = compute_blur_kernel(1.0);
        assert!(w[0] > 0.0);
        // weights normalized to sum near 1.0 (after summing center + 2 * paired)
        let sum = w[0] + 2.0 * w[1] + 2.0 * w[2];
        assert!((sum - 1.0).abs() < 0.05, "weights sum ≈ 1.0, got {sum}");
    }

    /// (review) 大 sigma でも 5-tap の有効重み和が 1 のまま (= shadow が blur 量に
    /// 比例して減光しない)。 旧実装は全範囲正規化のため sigma=10 で和 ≈0.36 だった。
    #[test]
    fn blur_kernel_large_sigma_stays_normalized() {
        for sigma in [2.0_f32, 5.0, 10.0, 30.0] {
            let (w, _) = compute_blur_kernel(sigma);
            let sum = w[0] + 2.0 * w[1] + 2.0 * w[2];
            assert!(
                (sum - 1.0).abs() < 1e-3,
                "sigma={sigma}: weights sum ≈ 1.0, got {sum}"
            );
        }
    }

    /// outline 付き (= effect path) で font だけ違う 2 area を作る helper。
    fn effect_area(font: Option<&str>) -> GlyphArea {
        GlyphArea {
            text: "歌".into(),
            font_size: 48.0,
            line_height: 56.0,
            color: Color::rgb(1.0, 1.0, 1.0),
            font_family: font.map(std::convert::Into::into),
            outline_color: Color::rgb(0.0, 0.0, 0.0),
            outline_width_px: 2.0,
            ..GlyphArea::default()
        }
    }

    /// M14 Phase 121 (daw_01 #096): effect path の layout buffer cache key も font 違いを区別する。
    #[test]
    fn buffer_cache_key_font_family_diff() {
        let a = effect_area(Some("Arial"));
        let b = effect_area(Some("Times New Roman"));
        assert_ne!(buffer_cache_key(&a), buffer_cache_key(&b));
        // None == Some(DEFAULT) (resolved 経由で同 buffer)。
        assert_eq!(
            buffer_cache_key(&effect_area(None)),
            buffer_cache_key(&effect_area(Some(DEFAULT_FONT_FAMILY)))
        );
    }

    /// M14 Phase 121 (daw_01 #096): composite texture の EffectKey も font 違いを区別する
    /// (= 同じ歌詞・同じ effect を別 font で重ねても先に焼いた composite に化けない)。
    #[test]
    fn effect_key_font_family_diff() {
        let a = EffectKey::from_area(&effect_area(Some("Arial")));
        let b = EffectKey::from_area(&effect_area(Some("Times New Roman")));
        assert_ne!(a, b);
        // None == Some(DEFAULT)。
        assert_eq!(
            EffectKey::from_area(&effect_area(None)),
            EffectKey::from_area(&effect_area(Some(DEFAULT_FONT_FAMILY)))
        );
    }
}
