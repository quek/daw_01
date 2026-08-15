//! wgpu パイプライン群 + run dispatch 共通ヘルパ。
//!
//! M1: rect (instanced 角丸矩形), glyph (テキスト)。
//! M2: line (波形/メータ/グリッド)。
//! M9 Phase 45f: scene primitive を call order で interleave するために `RunHandle` /
//! `enqueue_runs` / `render_runs` を共通化 (device.rs / offscreen.rs 双方で使う)。
//! M14 Phase 71 (daw_01 #043): texture (video frame / thumbnail) を追加。 popup pass では
//! texture pipeline を持たない (`Option<&mut TexturePipeline>` / `Option<(&TexturePipeline,
//! &TextureStore)>` で None を渡すと Texture primitive は skip される)。

pub mod glyph;
pub mod line;
pub mod rect;
pub mod scissor;
pub mod text_effect;
pub mod texture;

use daw_ui_platform::PhysicalSize;

use self::glyph::{GlyphPipeline, GlyphRun};
use self::line::{LinePipeline, LineRun};
use self::rect::{RectPipeline, RectRun};
use self::text_effect::TextEffectCompositor;
use self::texture::{TexturePipeline, TextureRun};
use crate::fonts::FontAssets;
use crate::scene::{GlyphArea, LineBatch, Primitive, Rect, RectCommand, TexturedQuad};
use crate::texture_store::TextureStore;

/// M14 Phase 78 (daw_01 #049): scene primitive を walk して effect 付き `Primitive::Glyph` を
/// `TextEffectCompositor` で render → `Primitive::Texture` に substitute する。 effect 無し
/// Glyph と他 type は pass-through。 戻り値は substituted primitive list で、 直後の
/// `enqueue_runs` にそのまま渡せる (= z-order = call order を保つ)。
///
/// 引数数は多いが、 全て不可分の resource bundle (= 1 つでも欠けると compositor が動作しない)
/// なので `Config` struct 化しても caller boilerplate が増えるだけのため flat 受けで `#[allow]`。
///
/// `texture_sampler` / `texture_bgl` は `TexturePipeline` の bind_group layout (= 最終 composite
/// texture を `TexturedQuad` として sampling する際に使う)。
#[allow(clippy::too_many_arguments)]
pub fn prepare_text_effects(
    primitives: &[Primitive],
    compositor: &mut TextEffectCompositor,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    fonts: &mut FontAssets,
    texture_store: &mut TextureStore,
    texture_sampler: &wgpu::Sampler,
    texture_bgl: &wgpu::BindGroupLayout,
) -> Vec<Primitive> {
    let mut out = Vec::with_capacity(primitives.len());
    for p in primitives {
        match p {
            Primitive::Glyph(area) if area.has_effects() => {
                if let Some((handle, x, y, w, h)) = compositor.render_effect(
                    device,
                    queue,
                    encoder,
                    fonts,
                    texture_store,
                    texture_sampler,
                    texture_bgl,
                    area,
                ) {
                    // M14 Phase 122 (daw_01 #097): box 指定時は **box 中心** を旋回中心にする
                    // (daw_01 の TextEvent.rotation は box 中心を旋回中心とする仕様)。 pivot は
                    // quad 左上 (x, y) を原点とした相対座標。 box 未指定の軸は rect 中心 (= 従来の
                    // `None` pivot と同値) に落とす。 `box_width` / `box_height` が **両方 None** の
                    // ときだけ pivot 自体を `None` にするので、 box 無しは byte 完全互換。
                    let rotation_pivot = if area.box_width.is_some() || area.box_height.is_some() {
                        let px = match area.box_width.filter(|v| v.is_finite()) {
                            Some(bw) => area.left + bw * 0.5 - x,
                            None => w * 0.5,
                        };
                        let py = match area.box_height.filter(|v| v.is_finite()) {
                            Some(bh) => area.top + bh * 0.5 - y,
                            None => h * 0.5,
                        };
                        Some((px, py))
                    } else {
                        None
                    };
                    out.push(Primitive::Texture(TexturedQuad {
                        rect: Rect::new(x, y, w, h),
                        texture: handle,
                        alpha: 1.0,
                        uv_min: (0.0, 0.0),
                        uv_max: (1.0, 1.0),
                        clip_rect: area.clip_rect,
                        rotation_radians: text_effect::normalize_finite(area.rotation_radians),
                        rotation_pivot,
                    }));
                } else {
                    // compositor が None を返したら (= text 0 サイズ等) plain Glyph に fallback。
                    out.push(p.clone());
                }
            }
            other => out.push(other.clone()),
        }
    }
    out
}

/// 1 つの primitive run handle (どの pipeline で render するか + 各 pipeline の run 情報)。
#[derive(Debug, Clone, Copy)]
pub enum RunHandle {
    Rect(RectRun),
    Line(LineRun),
    Glyph(GlyphRun),
    /// M14 Phase 71: texture pipeline (popup pass では `enqueue_runs` で skip されるため
    /// この variant は base pass からしか生成されない)。
    Texture(TextureRun),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrimKind {
    Rect,
    Line,
    Glyph,
    Texture,
}

fn primitive_kind(p: &Primitive) -> PrimKind {
    match p {
        Primitive::Rect(_) => PrimKind::Rect,
        Primitive::Line(_) => PrimKind::Line,
        Primitive::Glyph(_) => PrimKind::Glyph,
        Primitive::Texture(_) => PrimKind::Texture,
    }
}

/// `primitives` を call order で walk、同 type 連続を 1 つの run にまとめて各 pipeline に enqueue。
/// 戻り値の `Vec<RunHandle>` は **call order の z-order** を保つ run handle 列。
///
/// device.rs (window-backed render) と offscreen.rs (PNG snapshot) 双方で使う共通 helper。
///
/// `texture` は base pass で `Some(&mut TexturePipeline)`、 popup pass で `None`。 None に
/// Texture primitive が混ざっていれば silently skip (Ui レイヤで base 側に振り分けるのが
/// 通常パスなので、 popup に来る Texture は誤用)。
///
/// 引数数は 9 (= clippy default 7 超過)。 4 pipeline + texture option + device/queue + fonts +
/// screen と本質的に多入力なので `Config` 構造体に集約しても意味不明な bag になるため `#[allow]`。
#[allow(clippy::too_many_arguments)]
pub fn enqueue_runs(
    primitives: &[Primitive],
    rect: &mut RectPipeline,
    line: &mut LinePipeline,
    glyph: &mut GlyphPipeline,
    mut texture: Option<&mut TexturePipeline>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    fonts: &mut FontAssets,
    screen: PhysicalSize,
) -> Vec<RunHandle> {
    let mut runs: Vec<RunHandle> = Vec::new();
    let mut i = 0;
    while i < primitives.len() {
        let start = i;
        let kind = primitive_kind(&primitives[i]);
        i += 1;
        while i < primitives.len() && primitive_kind(&primitives[i]) == kind {
            i += 1;
        }
        let group = &primitives[start..i];
        match kind {
            PrimKind::Rect => {
                let buf: Vec<RectCommand> = group
                    .iter()
                    .filter_map(|p| if let Primitive::Rect(c) = p { Some(*c) } else { None })
                    .collect();
                runs.push(RunHandle::Rect(rect.enqueue_run(&buf)));
            }
            PrimKind::Line => {
                let buf: Vec<LineBatch> = group
                    .iter()
                    .filter_map(|p| if let Primitive::Line(l) = p { Some(l.clone()) } else { None })
                    .collect();
                runs.push(RunHandle::Line(line.enqueue_run(&buf)));
            }
            PrimKind::Glyph => {
                let buf: Vec<GlyphArea> = group
                    .iter()
                    .filter_map(|p| if let Primitive::Glyph(g) = p { Some(g.clone()) } else { None })
                    .collect();
                runs.push(RunHandle::Glyph(
                    glyph.enqueue_run(device, queue, fonts, &buf, screen),
                ));
            }
            PrimKind::Texture => {
                let Some(tex_pipeline) = texture.as_deref_mut() else {
                    continue;
                };
                let buf: Vec<TexturedQuad> = group
                    .iter()
                    .filter_map(|p| if let Primitive::Texture(q) = p { Some(*q) } else { None })
                    .collect();
                runs.push(RunHandle::Texture(tex_pipeline.enqueue_run(&buf)));
            }
        }
    }
    runs
}

/// `runs` を順次 render pass に発行する (call order = z-order を保つ)。
///
/// `texture` は base pass で `Some((&TexturePipeline, &TextureStore))`、 popup pass で `None`。
/// None で `RunHandle::Texture` が来た場合は skip (enqueue_runs で `texture=None` なら Texture
/// run は生成されないので通常 unreachable)。
pub fn render_runs(
    runs: &[RunHandle],
    rect: &RectPipeline,
    line: &LinePipeline,
    glyph: &GlyphPipeline,
    texture: Option<(&TexturePipeline, &TextureStore)>,
    pass: &mut wgpu::RenderPass<'_>,
    screen: PhysicalSize,
) {
    for run in runs {
        match run {
            RunHandle::Rect(r) => rect.render_run(pass, screen, *r),
            RunHandle::Line(r) => line.render_run(pass, screen, *r),
            RunHandle::Glyph(r) => glyph.render_run(pass, *r),
            RunHandle::Texture(r) => {
                if let Some((tp, store)) = texture {
                    tp.render_run(pass, screen, *r, store);
                }
            }
        }
    }
}
