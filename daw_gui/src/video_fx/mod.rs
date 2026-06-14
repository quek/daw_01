//! GPU 映像効果 実行基盤 (FIXME #54 / docs/plan_video_fx.md §1, §8)。
//!
//! [`common::video_fx`] の宣言的カタログ ([`VideoFxDef`]) を受け取り、トラックの
//! 合成画 (1 枚の RGBA `TextureHandle`) にチェーン順で WGSL fragment パスを
//! ping-pong 適用して、効果適用後の `TextureHandle` を返す。
//!
//! gui_01 #111 で公開された interop primitive (`raw_texture` / `create_render_target`
//! / `device` / `queue`) の上に、daw_01 が**自前の effect pipeline** を組む
//! (効果の定義は daw_01 ドメイン = SSoT、gui_01 は汎用レンダラのまま)。
//!
//! preview (`Renderer<DawGuiWindow>`) と export (`OffscreenRenderer`) の両方で
//! 同一適用するため、必要な操作を [`VideoFxRenderer`] trait に抽象化し両者へ実装する。
//!
//! ## submit 順序 (gui_01 #111 D2 契約)
//!
//! [`VideoFxEngine::apply_chain`] は効果 chain を **自前 encoder に積んで `queue.submit`
//! してから return** する。呼び出し側はその後で `composite_scene_to_texture` の結果や
//! 効果出力 handle を base scene に push し、`render()` / `render_to_rgba()` を呼ぶ
//! (= 別 submit)。GPU は submit 順に実行するので効果出力は後段 sample より前に完了する。
//!
//! ## パラメータ値ドメイン
//!
//! 映像 param は automation lane に 0..=1 正規化 plain で保存 ([`common::video_fx`]
//! モジュール doc)。[`resolve_track_effects`] が automation + 変調を合成して 0..=1 の
//! 実効値を出し、manifest の min/max で実レンジ値へ展開して uniform に流す。

use std::collections::HashMap;

use common::automation;
use common::model::{AutomationTarget, Song, Track};
use common::video_fx::{PassKind, VideoFxDef, def_by_id};
use daw_ui_renderer::{OffscreenRenderer, Renderer, TextureHandle};

use crate::view::window::DawGuiWindow;

/// preview / export の両レンダラを効果実行基盤から統一して触るための抽象。
/// gui_01 #111 で公開された interop primitive をそのまま薄く束ねる。
pub trait VideoFxRenderer {
    fn fx_device(&self) -> &wgpu::Device;
    fn fx_queue(&self) -> &wgpu::Queue;
    /// `handle` の wgpu テクスチャ (効果シェーダの sample 入力)。destroy 済は `None`。
    fn fx_raw_texture(&self, handle: TextureHandle) -> Option<&wgpu::Texture>;
    /// `RENDER_ATTACHMENT | TEXTURE_BINDING` な target を確保 (caller 管理 = この
    /// engine が pool で frame 跨ぎ使い回し、teardown で destroy)。`(handle, color view)`。
    fn fx_create_render_target(
        &mut self,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> (TextureHandle, wgpu::TextureView);
    fn fx_destroy_texture(&mut self, handle: TextureHandle);
    /// 効果 target / base pass の color format (preview = surface、export = Rgba8UnormSrgb)。
    fn fx_format(&self) -> wgpu::TextureFormat;
}

impl VideoFxRenderer for Renderer<DawGuiWindow> {
    fn fx_device(&self) -> &wgpu::Device {
        self.device()
    }
    fn fx_queue(&self) -> &wgpu::Queue {
        self.queue()
    }
    fn fx_raw_texture(&self, handle: TextureHandle) -> Option<&wgpu::Texture> {
        self.raw_texture(handle)
    }
    fn fx_create_render_target(
        &mut self,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> (TextureHandle, wgpu::TextureView) {
        self.create_render_target(width, height, format)
    }
    fn fx_destroy_texture(&mut self, handle: TextureHandle) {
        self.destroy_texture(handle);
    }
    fn fx_format(&self) -> wgpu::TextureFormat {
        self.surface_format()
    }
}

impl VideoFxRenderer for OffscreenRenderer {
    fn fx_device(&self) -> &wgpu::Device {
        self.device()
    }
    fn fx_queue(&self) -> &wgpu::Queue {
        self.queue()
    }
    fn fx_raw_texture(&self, handle: TextureHandle) -> Option<&wgpu::Texture> {
        self.raw_texture(handle)
    }
    fn fx_create_render_target(
        &mut self,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> (TextureHandle, wgpu::TextureView) {
        self.create_render_target(width, height, format)
    }
    fn fx_destroy_texture(&mut self, handle: TextureHandle) {
        self.destroy_texture(handle);
    }
    fn fx_format(&self) -> wgpu::TextureFormat {
        self.target_format()
    }
}

/// チェーン上の 1 効果の **実効パラメータ**。`def` はカタログ定義、`params` は
/// `def.params` と同順の **実レンジ値** (automation + 変調を 0..=1 で合成後に展開)。
#[derive(Clone, Debug)]
pub struct ResolvedEffect {
    pub def: &'static VideoFxDef,
    /// `def.params[i]` に対応する実レンジ値。長さ = `def.params.len()`。
    pub params: Vec<f32>,
}

/// トラックの video device チェーンを解決して [`ResolvedEffect`] 列を返す。
/// `track.devices` 中の映像 device (`builtin.video.*`) を順に拾い、各 param の
/// 実効値 (automation lane の default/curve ⊕ 変調、0..=1) を manifest の実レンジへ展開。
/// `device_index` は統合 `Track.devices` 上の位置 ([`AutomationTarget::PluginParam`])。
#[must_use]
pub fn resolve_track_effects(
    song: &Song,
    track: &Track,
    song_beat: f64,
    mod_scalars: &[f32],
) -> Vec<ResolvedEffect> {
    let mut out = Vec::new();
    for (di, inst) in track.devices.iter().enumerate() {
        if !inst.ports.is_video() {
            continue;
        }
        let Some(def) = def_by_id(&inst.plugin_id) else {
            continue;
        };
        let device_index = di as u32;
        let params = def
            .params
            .iter()
            .map(|p| {
                let target = AutomationTarget::PluginParam {
                    device_index,
                    param_id: p.id,
                    legacy_slot: None,
                };
                // base = lane の default/curve (0..=1)、無ければ manifest default (0..=1)。
                let base = track
                    .automation_lanes
                    .iter()
                    .find(|l| l.target == target)
                    .map_or_else(
                        || p.kind.default_norm(),
                        |l| automation::lane_value_at(l, &song.clip_contents, song_beat),
                    );
                // 変調を 0..=1 領域で合成 (PluginParam は plain==norm なので恒等)。
                let eff_norm = automation::apply_modulation_with_scalars(
                    song,
                    &target,
                    base,
                    &track.mod_routings,
                    mod_scalars,
                );
                p.kind.norm_to_real(eff_norm)
            })
            .collect();
        out.push(ResolvedEffect { def, params });
    }
    out
}

// ============================================================================
// GPU 実行基盤
// ============================================================================

/// 効果 pipeline のキャッシュキー: 効果 id + パス index + 出力 format。
#[derive(Clone, PartialEq, Eq, Hash)]
struct PipelineKey {
    id: &'static str,
    pass: usize,
    format: wgpu::TextureFormat,
}

/// 効果 target の 1 枚 (color attachment view 付き)。1 frame の間 `in_use` の target は
/// 他の `apply_chain` 呼び出しに払い出さない (= gui_01 `CompositePool` と同じ「同 cycle 内は
/// 別 target」方式)。これが無いと、遅延描画 (frame 末に 1 回 render) 環境で同寸の効果レイヤーが
/// 複数あると後の呼び出しが前の出力 target を上書きし、全レイヤーが最後の効果出力を表示してしまう
/// (動画クロスフェード等で発症)。`idle` は連続未使用 frame 数 (退避判定)。
struct PoolTarget {
    handle: TextureHandle,
    /// color attachment 用 view (描画先)。sample は `raw_texture(handle)` から別 view を作る。
    view: wgpu::TextureView,
    in_use: bool,
    idle: u32,
}

/// `(width, height)` ごとの target 群 (frame 跨ぎ使い回し、同 frame 内は in_use で分離)。
struct SizePool {
    format: wgpu::TextureFormat,
    targets: Vec<PoolTarget>,
}

/// 効果実行基盤。pipeline cache + sampler + bind group layout + target pool を所有。
/// preview / export がそれぞれ 1 つ持つ (renderer に紐づくライフサイクル)。caller は frame ごとに
/// [`VideoFxEngine::end_frame`] を 1 回呼んで in_use を解放する (= gui_01 の `end_cycle` 相当)。
#[derive(Default)]
pub struct VideoFxEngine {
    /// 遅延初期化 (最初の `apply_chain` で device から作る)。
    common: Option<CommonGpu>,
    pipelines: HashMap<PipelineKey, wgpu::RenderPipeline>,
    pool: HashMap<(u32, u32), SizePool>,
}

/// device に紐づく共有リソース (sampler + bind group layout)。
struct CommonGpu {
    sampler: wgpu::Sampler,
    /// binding 0=src texture, 1=sampler, 2=uniform。
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
}

impl VideoFxEngine {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// pool / pipeline を全破棄 (renderer teardown / format 変更時)。target handle を
    /// renderer から destroy する。
    pub fn clear<R: VideoFxRenderer>(&mut self, r: &mut R) {
        for (_, sp) in self.pool.drain() {
            for t in sp.targets {
                r.fx_destroy_texture(t.handle);
            }
        }
        self.pipelines.clear();
    }

    fn ensure_common(&mut self, device: &wgpu::Device) -> &CommonGpu {
        self.common.get_or_insert_with(|| {
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("video_fx sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                ..Default::default()
            });
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("video_fx bind group layout"),
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
            let pipeline_layout =
                device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("video_fx pipeline layout"),
                    bind_group_layouts: &[Some(&layout)],
                    immediate_size: 0,
                });
            CommonGpu { sampler, layout, pipeline_layout }
        })
    }

    fn pipeline_for(
        &mut self,
        device: &wgpu::Device,
        def: &'static VideoFxDef,
        pass: usize,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        let key = PipelineKey { id: def.id, pass, format };
        if let Some(p) = self.pipelines.get(&key) {
            return p.clone();
        }
        let pipeline_layout = self.ensure_common(device).pipeline_layout.clone();
        let wgsl = assemble_module(def, pass);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(def.id),
            source: wgpu::ShaderSource::Wgsl(wgsl.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(def.id),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        self.pipelines.insert(key.clone(), pipeline.clone());
        pipeline
    }

    /// `(w, h, format)` の未使用 target を 1 枚払い出す (なければ作成、format 不一致なら作り直し)。
    /// 払い出した target は frame 内 `in_use` になり、[`end_frame`](Self::end_frame) まで他の
    /// `apply_chain` 呼び出しへ再払い出しされない (= 遅延描画でも各レイヤーの効果出力が衝突しない)。
    fn acquire<R: VideoFxRenderer>(
        &mut self,
        r: &mut R,
        w: u32,
        h: u32,
        format: wgpu::TextureFormat,
    ) -> (TextureHandle, wgpu::TextureView) {
        let sp = self
            .pool
            .entry((w, h))
            .or_insert_with(|| SizePool { format, targets: Vec::new() });
        if sp.format != format {
            for t in sp.targets.drain(..) {
                r.fx_destroy_texture(t.handle);
            }
            sp.format = format;
        }
        if let Some(t) = sp.targets.iter_mut().find(|t| !t.in_use) {
            t.in_use = true;
            t.idle = 0;
            return (t.handle, t.view.clone());
        }
        let (handle, view) = r.fx_create_render_target(w, h, format);
        sp.targets.push(PoolTarget { handle, view: view.clone(), in_use: true, idle: 0 });
        (handle, view)
    }

    /// frame ごとに 1 回呼ぶ (preview は `render` 前の冒頭、export は build_frame_scene 冒頭)。
    /// 全 target の `in_use` を解放し、一定 frame 連続未使用の target を破棄する
    /// (gui_01 `CompositePool::end_cycle` 相当: VRAM の無限増加を防ぐ)。
    pub fn end_frame<R: VideoFxRenderer>(&mut self, r: &mut R) {
        const MAX_IDLE: u32 = 120;
        for sp in self.pool.values_mut() {
            let mut i = 0;
            while i < sp.targets.len() {
                let t = &mut sp.targets[i];
                if t.in_use {
                    t.in_use = false;
                    t.idle = 0;
                    i += 1;
                } else {
                    t.idle += 1;
                    if t.idle > MAX_IDLE {
                        let h = t.handle;
                        sp.targets.remove(i);
                        r.fx_destroy_texture(h);
                    } else {
                        i += 1;
                    }
                }
            }
        }
        self.pool.retain(|_, sp| !sp.targets.is_empty());
    }

    /// `src` (トラック合成画) に `chain` をチェーン順 (ping-pong) で適用し、結果の
    /// `TextureHandle` を返す。`chain` が空 / 実行可能パスが無いときは `src` をそのまま返す。
    ///
    /// 効果 chain は自前 encoder に積んで `queue.submit` してから return する
    /// (gui_01 #111 D2 契約)。返る handle は engine 所有の pool target (caller は
    /// destroy しない、次の `apply_chain` まで有効)。
    pub fn apply_chain<R: VideoFxRenderer>(
        &mut self,
        r: &mut R,
        src: TextureHandle,
        width: u32,
        height: u32,
        chain: &[ResolvedEffect],
    ) -> TextureHandle {
        // 実行可能 (= Simple) パスを平坦化。SeparableBlur / History は後続実装
        // (効果実行基盤の波状拡張、現カタログは全 Simple)。
        let mut passes: Vec<(&'static VideoFxDef, usize, &[f32])> = Vec::new();
        for eff in chain {
            for (pi, pass) in eff.def.passes.iter().enumerate() {
                if matches!(pass.kind, PassKind::Simple) {
                    passes.push((eff.def, pi, &eff.params));
                }
            }
        }
        if passes.is_empty() || width == 0 || height == 0 {
            return src;
        }

        let format = r.fx_format();
        // この呼び出し専用の出力 target を確保 (frame 内 in_use で他レイヤーと分離)。単一パスは
        // 1 枚、複数パスは ping-pong 用に 2 枚。返した handle は end_frame まで上書きされないので、
        // 遅延描画 (frame 末に 1 回 render) でも同寸の他レイヤーと衝突しない。
        let t0 = self.acquire(r, width, height, format);
        let t1 = if passes.len() > 1 {
            Some(self.acquire(r, width, height, format))
        } else {
            None
        };
        // borrow を分離: device / queue は Arc backed clone で取り出してから pipeline を使う。
        let device = r.fx_device().clone();
        let queue = r.fx_queue().clone();
        let sampler = self.ensure_common(&device).sampler.clone();

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("video_fx chain"),
        });

        let texel = [1.0 / width as f32, 1.0 / height as f32];
        let resolution = [width as f32, height as f32];

        let mut input = src;
        let mut use_t0 = true; // 次の出力先は t0?
        for (def, pass_idx, params) in passes {
            let pipeline = self.pipeline_for(&device, def, pass_idx, format);
            let (out_handle, out_view) = if use_t0 {
                (t0.0, &t0.1)
            } else {
                let t1 = t1.as_ref().expect("t1 は複数パス時に確保済み");
                (t1.0, &t1.1)
            };

            // 入力テクスチャ view (gui_01 #111 D3: handle→texture を clone して借用衝突回避)。
            let Some(in_tex) = r.fx_raw_texture(input).cloned() else {
                // 入力 texture が消えている (想定外)。silent に落とさず log してから元 src を返す
                // (feedback_verify_actual_content: 視覚回帰を観測可能にする)。
                tracing::warn!(
                    effect = def.id,
                    pass = pass_idx,
                    "video_fx: input texture missing mid-chain; effect dropped"
                );
                return src;
            };
            let in_view = in_tex.create_view(&wgpu::TextureViewDescriptor::default());

            let ubuf = make_uniform_buffer(&device, def, params, resolution, texel);
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(def.id),
                layout: &self.ensure_common(&device).layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&in_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: ubuf.as_entire_binding(),
                    },
                ],
            });

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some(def.id),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: out_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&pipeline);
                pass.set_bind_group(0, &bind, &[]);
                pass.draw(0..3, 0..1);
            }

            input = out_handle;
            use_t0 = !use_t0;
        }

        queue.submit(std::iter::once(encoder.finish()));
        input
    }
}

/// uniform buffer を `def` の param 表 + prelude (resolution/texel/time) から組む。
/// レイアウトは [`assemble_module`] が生成する `Params` 構造体と一致させる:
/// `[res.x, res.y, texel.x, texel.y, time, 0, 0, 0, p0, p1, ...]` を 4 float (16B) 境界へ pad。
fn make_uniform_buffer(
    device: &wgpu::Device,
    def: &VideoFxDef,
    params: &[f32],
    resolution: [f32; 2],
    texel: [f32; 2],
) -> wgpu::Buffer {
    let mut data: Vec<f32> = Vec::with_capacity(8 + def.params.len() + 3);
    data.extend_from_slice(&[resolution[0], resolution[1], texel[0], texel[1]]);
    data.push(0.0); // time (現カタログ未使用、将来 noise/feedback 用)
    data.extend_from_slice(&[0.0, 0.0, 0.0]); // pad0..2 (prelude を 8 float = 32B に)
    for (i, _p) in def.params.iter().enumerate() {
        data.push(params.get(i).copied().unwrap_or(0.0));
    }
    while !data.len().is_multiple_of(4) {
        data.push(0.0);
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(data.len() * 4);
    for f in &data {
        bytes.extend_from_slice(&f.to_ne_bytes());
    }
    device.create_buffer_init_compat(&bytes)
}

/// `def` の `pass` 番目 effect-body を標準ハーネス (頂点シェーダ + bind group +
/// `Params` uniform + `@fragment`) で包んで完全な WGSL モジュールにする。
/// uniform レイアウトは [`make_uniform_buffer`] と一致 (prelude 8 float → params)。
#[must_use]
pub fn assemble_module(def: &VideoFxDef, pass: usize) -> String {
    let body = def.passes[pass].wgsl;
    let mut param_fields = String::new();
    for p in def.params {
        param_fields.push_str(&format!("    {}: f32,\n", p.key));
    }
    // params を 4 float 境界へ pad (uniform struct size を 16B 倍数に)。
    let mut tail = (8 + def.params.len()) % 4;
    if tail != 0 {
        tail = 4 - tail;
    }
    let mut tail_fields = String::new();
    for i in 0..tail {
        tail_fields.push_str(&format!("    _tail{i}: f32,\n"));
    }
    format!(
        r#"struct Params {{
    resolution: vec2<f32>,
    texel: vec2<f32>,
    time: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
{param_fields}{tail_fields}}};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_samp: sampler;
@group(0) @binding(2) var<uniform> P: Params;

fn sample(uv: vec2<f32>) -> vec4<f32> {{
    return textureSampleLevel(src_tex, src_samp, uv, 0.0);
}}

struct VsOut {{
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {{
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let xy = corners[vi];
    var out: VsOut;
    out.pos = vec4<f32>(xy, 0.0, 1.0);
    out.uv = vec2<f32>((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return out;
}}

{body}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {{
    return effect(in.uv, sample(in.uv));
}}
"#
    )
}

/// `wgpu::util::DeviceExt::create_buffer_init` 相当 (util feature 非依存の薄い helper)。
trait DeviceCreateBufferInit {
    fn create_buffer_init_compat(&self, contents: &[u8]) -> wgpu::Buffer;
}
impl DeviceCreateBufferInit for wgpu::Device {
    fn create_buffer_init_compat(&self, contents: &[u8]) -> wgpu::Buffer {
        // uniform buffer (COPY_DST 無しでも mapped_at_creation で初期値を書ける)。
        let buf = self.create_buffer(&wgpu::BufferDescriptor {
            label: Some("video_fx uniform"),
            size: contents.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: true,
        });
        buf.slice(..).get_mapped_range_mut().copy_from_slice(contents);
        buf.unmap();
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::video_fx::builtin_video_fx;

    #[test]
    fn assemble_is_balanced_and_contains_entrypoints() {
        for def in builtin_video_fx() {
            for pass in 0..def.passes.len() {
                let src = assemble_module(def, pass);
                assert!(src.contains("fn vs_main"), "{} missing vs", def.id);
                assert!(src.contains("fn fs_main"), "{} missing fs", def.id);
                assert!(src.contains("fn effect"), "{} body missing effect()", def.id);
                assert_eq!(
                    src.matches('{').count(),
                    src.matches('}').count(),
                    "{} brace imbalance",
                    def.id
                );
                // param key が Params 構造体に出る。
                for p in def.params {
                    assert!(
                        src.contains(&format!("{}: f32", p.key)),
                        "{}: param {} not in Params",
                        def.id,
                        p.key
                    );
                }
            }
        }
    }

    #[test]
    fn uniform_struct_is_16byte_aligned() {
        // prelude 8 float + params + tail が 4 の倍数 (= 16B 倍数)。
        for def in builtin_video_fx() {
            let total = 8 + def.params.len();
            let mut tail = total % 4;
            if tail != 0 {
                tail = 4 - tail;
            }
            assert_eq!((total + tail) % 4, 0, "{}", def.id);
        }
    }
}
