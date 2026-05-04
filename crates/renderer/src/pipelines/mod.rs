//! wgpu パイプライン群 + run dispatch 共通ヘルパ。
//!
//! M1: rect (instanced 角丸矩形), glyph (テキスト)。
//! M2: line (波形/メータ/グリッド)。
//! M9 Phase 45f: scene primitive を call order で interleave するために `RunHandle` /
//! `enqueue_runs` / `render_runs` を共通化 (device.rs / offscreen.rs 双方で使う)。
//! M5+ で quad (textured) を追加予定。

pub mod glyph;
pub mod line;
pub mod rect;

use daw_ui_platform::PhysicalSize;

use self::glyph::{GlyphPipeline, GlyphRun};
use self::line::{LinePipeline, LineRun};
use self::rect::{RectPipeline, RectRun};
use crate::scene::{GlyphArea, LineBatch, Primitive, RectCommand};

/// 1 つの primitive run handle (どの pipeline で render するか + 各 pipeline の run 情報)。
#[derive(Debug, Clone, Copy)]
pub enum RunHandle {
    Rect(RectRun),
    Line(LineRun),
    Glyph(GlyphRun),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrimKind {
    Rect,
    Line,
    Glyph,
}

fn primitive_kind(p: &Primitive) -> PrimKind {
    match p {
        Primitive::Rect(_) => PrimKind::Rect,
        Primitive::Line(_) => PrimKind::Line,
        Primitive::Glyph(_) => PrimKind::Glyph,
    }
}

/// `primitives` を call order で walk、同 type 連続を 1 つの run にまとめて各 pipeline に enqueue。
/// 戻り値の `Vec<RunHandle>` は **call order の z-order** を保つ run handle 列。
///
/// device.rs (window-backed render) と offscreen.rs (PNG snapshot) 双方で使う共通 helper。
pub fn enqueue_runs(
    primitives: &[Primitive],
    rect: &mut RectPipeline,
    line: &mut LinePipeline,
    glyph: &mut GlyphPipeline,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
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
                runs.push(RunHandle::Glyph(glyph.enqueue_run(device, queue, &buf, screen)));
            }
        }
    }
    runs
}

/// `runs` を順次 render pass に発行する (call order = z-order を保つ)。
pub fn render_runs(
    runs: &[RunHandle],
    rect: &RectPipeline,
    line: &LinePipeline,
    glyph: &GlyphPipeline,
    pass: &mut wgpu::RenderPass<'_>,
    screen: PhysicalSize,
) {
    for run in runs {
        match run {
            RunHandle::Rect(r) => rect.render_run(pass, screen, *r),
            RunHandle::Line(r) => line.render_run(pass, screen, *r),
            RunHandle::Glyph(r) => glyph.render_run(pass, *r),
        }
    }
}
