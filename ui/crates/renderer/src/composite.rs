// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! M14 Phase 93 (daw_01 #063): `Scene` を GPU 常駐の sampleable texture に合成する共通経路。
//!
//! 立ち絵 group transform 等で「子 quad 群を 1 枚のオフスクリーンテクスチャに焼いてから
//! 親 affine を 1 回かける」 用途。 `Renderer<W>` (preview) / `OffscreenRenderer` (export)
//! 双方の `composite_scene_to_texture` がこのモジュールに委譲する (DRY、 `enqueue_runs` /
//! `render_runs` と同じ shared-helper の置き場)。
//!
//! ## なぜ既存 pipeline を再利用してよいか (LAST WRITE WINS trap を踏まない理由)
//!
//! [`composite_scene`] は呼び出しごとに **独自の encoder を `queue.submit` する**。
//! `queue.write_buffer` は deferred だが「直前の submit までに積まれた write を、 その submit の
//! command が走る **前** に flush」 する。 よって
//! `composite(A) submit → composite(B) submit → render() submit` は各 submit 時点で
//! それぞれの screen uniform / instance buffer が個別に flush され、 各 draw が正しい値を読む。
//! LAST WRITE WINS trap (CLAUDE.md wgpu 節 / M9 Phase 44a / M14 Phase 78) は **1 つの submit 内**で
//! buffer を複数回書いて複数 draw が読む場合のみ起きる。 別 submit なら起きないので、 専用
//! pipeline (= GlyphPipeline の FontSystem 二重ロード等) を増やさず `rect/line/glyph/texture` を
//! そのまま流用する。
//!
//! ## render target の format
//!
//! 合成先 texture の format は **呼び出し元 renderer の pipeline format に一致させる** 必要がある
//! (render pipeline は `ColorTargetState.format` と一致する attachment にしか描けない)。
//! preview = surface format (Windows では `Bgra8UnormSrgb` が多い)、 export = `Rgba8UnormSrgb`。
//! 返った handle は `TexturedQuad` として sample されるが、 sampling は format-transparent
//! (sRGB → linear → blend → sRGB) なので caller は channel 順を意識しなくてよい。
//!
//! ## clear
//!
//! 合成結果は親 scene へ alpha composite される前提なので、 `scene.clear_color` は無視して
//! 常に **透明 (`wgpu::Color::TRANSPARENT`)** で clear する (daw_01 #063 要望)。
//!
//! ## target の使い回し (CompositePool)
//!
//! preview は毎フレーム呼ばれるので target texture を毎回 alloc/destroy すると無駄。
//! [`CompositePool`] が size 別に target を保持して使い回す。 単純な「size → 1 handle」 cache だと
//! **同一サイズの composite を 1 フレーム内で複数回** 呼ぶと後者が前者の内容を上書きして base
//! scene の `TexturedQuad` が壊れるため、 in-use フラグ付き pool にして「同 cycle 内は別 target を
//! 払い出す」。 [`CompositePool::end_cycle`] (= `render()` / `render_to_rgba()` 末尾で呼ぶ) が
//! in-use を解除し、 一定 cycle 未使用の target を evict する。

use daw_ui_platform::PhysicalSize;

use crate::fonts::FontAssets;
use crate::pipelines::{
    enqueue_runs, glyph::GlyphPipeline, line::LinePipeline, prepare_text_effects,
    rect::RectPipeline, render_runs, text_effect::TextEffectCompositor, texture::TexturePipeline,
};
use crate::scene::{Scene, TextureHandle};
use crate::texture_store::TextureStore;

/// この cycle 数だけ連続で未使用だった composite target を evict する閾値。 size がフレーム毎に
/// 変わる usage でも pool が無限に膨らまないようにする (典型 usage では group size は有限集合なので
/// ほぼ evict されない)。
const MAX_IDLE_CYCLES: u32 = 60;

struct CompositeTarget {
    handle: TextureHandle,
    width: u32,
    height: u32,
    /// この cycle で既に払い出し済か (= 同 cycle 内の別 composite に再払い出ししない)。
    in_use: bool,
    /// 連続で未使用だった cycle 数 (`MAX_IDLE_CYCLES` 超過で evict)。
    idle_cycles: u32,
}

/// composite_scene_to_texture の render target を size 別に使い回す pool。
/// `Renderer<W>` / `OffscreenRenderer` が 1 つずつ所有する (= renderer がライフサイクルを
/// 所有する SSoT、 daw_01 側の二重 cache 不要)。
pub(crate) struct CompositePool {
    targets: Vec<CompositeTarget>,
}

impl CompositePool {
    pub(crate) fn new() -> Self {
        Self { targets: Vec::new() }
    }

    /// `(width, height)` の render target を 1 つ確保 (in-use にマーク) して `(handle, target_view)`
    /// を返す。 同 cycle 内で未使用かつ同 size の既存 target があれば再利用、 無ければ
    /// [`TextureStore::create_render_target`] で新規作成する。 `target_view` は color attachment 用に
    /// 都度生成 (sampling 用 view は store の bind_group 内に別途保持されている)。
    ///
    /// 引数は `create_render_target` の resource bundle (device/sampler/layout/format) + size の
    /// 不可分入力なので flat 受けで `#[allow(too_many_arguments)]` (`enqueue_runs` 等と同 idiom)。
    #[allow(clippy::too_many_arguments)]
    fn acquire(
        &mut self,
        store: &mut TextureStore,
        device: &wgpu::Device,
        sampler: &wgpu::Sampler,
        layout: &wgpu::BindGroupLayout,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> (TextureHandle, wgpu::TextureView) {
        let mut reused = None;
        for t in &mut self.targets {
            if !t.in_use && t.width == width && t.height == height {
                t.in_use = true;
                t.idle_cycles = 0;
                reused = Some(t.handle);
                break;
            }
        }
        let handle = if let Some(h) = reused {
            h
        } else {
            let (h, _new_view) =
                store.create_render_target(device, sampler, layout, format, width, height);
            self.targets.push(CompositeTarget {
                handle: h,
                width,
                height,
                in_use: true,
                idle_cycles: 0,
            });
            h
        };
        let view = store
            .raw_texture(handle)
            .expect("composite target just created/reused must be live")
            .create_view(&wgpu::TextureViewDescriptor::default());
        (handle, view)
    }

    /// 1 render cycle (= `render()` / `render_to_rgba()` 1 回) の末尾で呼ぶ。 全 target の in-use を
    /// 解除し、 この cycle で未使用だった target の `idle_cycles` を加算、 `MAX_IDLE_CYCLES` 超過分は
    /// store から destroy して pool から外す。
    pub(crate) fn end_cycle(&mut self, store: &mut TextureStore) {
        self.targets.retain_mut(|t| {
            let keep = if t.in_use {
                t.idle_cycles = 0;
                true
            } else {
                t.idle_cycles += 1;
                t.idle_cycles <= MAX_IDLE_CYCLES
            };
            if !keep {
                store.destroy(t.handle);
            }
            t.in_use = false;
            keep
        });
    }

    /// pool 内の全 target を destroy して空にする (project close 等で明示解放したい場合用)。
    pub(crate) fn clear(&mut self, store: &mut TextureStore) {
        for t in self.targets.drain(..) {
            store.destroy(t.handle);
        }
    }
}

/// `scene.primitives` を `(width, height)` の GPU 常駐 sampleable texture に合成し、 その
/// `TextureHandle` を返す。 独自 encoder を submit する (= readback / present なし、 GPU stay)。
///
/// `scene.popup_primitives` は対象外 (合成済の子 group に popup は乗らない、 #063 reply 参照)。
/// `scene.clear_color` は無視して透明 clear する。
///
/// pipeline 群 (`rect`/`line`/`glyph`/`texture`) は呼び出し元 renderer の **base pass 用と同じ
/// instance を流用** する (独自 submit ゆえ LAST WRITE WINS trap を踏まない、 module doc 参照)。
#[allow(clippy::too_many_arguments)]
pub(crate) fn composite_scene(
    scene: &Scene,
    width: u32,
    height: u32,
    target_format: wgpu::TextureFormat,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    fonts: &mut FontAssets,
    rect: &mut RectPipeline,
    line: &mut LinePipeline,
    glyph: &mut GlyphPipeline,
    texture: &mut TexturePipeline,
    text_effect: &mut TextEffectCompositor,
    texture_store: &mut TextureStore,
    pool: &mut CompositePool,
) -> TextureHandle {
    let size = PhysicalSize { width: width.max(1), height: height.max(1) };

    // 1. begin_frame: 各 pipeline の scratch を reset (size は **合成先 (w,h)** を渡す。 surface
    //    size を渡すと NDC / scissor が誤 scale する)。
    rect.begin_frame();
    line.begin_frame();
    glyph.begin_frame(queue, size);
    texture.begin_frame();
    text_effect.begin_frame();

    // 2. encoder を先に作り、 text effect pre-pass を同 encoder に積む (device.rs と同形)。
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("daw-ui composite encoder"),
    });

    // 3. effect 付き Glyph を offscreen で焼いて Primitive::Texture に substitute。
    let substituted = prepare_text_effects(
        &scene.primitives,
        text_effect,
        device,
        queue,
        &mut encoder,
        fonts,
        texture_store,
        texture.sampler(),
        texture.texture_bind_group_layout(),
    );

    // 4. substituted primitives を call order で enqueue (合成先 size で projection)。
    let runs = enqueue_runs(
        &substituted,
        rect,
        line,
        glyph,
        Some(texture),
        device,
        queue,
        fonts,
        size,
    );

    // 5. instance buffer を合成先 size で upload (glyph は enqueue 内で済)。
    rect.upload(device, queue, size);
    line.upload(device, queue, size);
    texture.upload(queue, size);

    // 6. render target を pool から確保 (size 別使い回し)。
    let (handle, target_view) = pool.acquire(
        texture_store,
        device,
        texture.sampler(),
        texture.texture_bind_group_layout(),
        target_format,
        size.width,
        size.height,
    );

    // 7. 透明 clear + 全 run を call order で render。
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("daw-ui composite pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
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
        render_runs(
            &runs,
            rect,
            line,
            glyph,
            Some((texture, texture_store)),
            &mut pass,
            size,
        );
    }

    // 8. cache eviction を進める + submit (present / readback なし)。
    glyph.end_frame();
    text_effect.end_frame(texture_store);
    queue.submit(std::iter::once(encoder.finish()));

    handle
}
