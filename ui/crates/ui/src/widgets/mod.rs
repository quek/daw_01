//! ウィジェット実装。
//! M1: label / button。
//! M2: waveform (波形表示、line strip パイプラインを使う)。
//! M3: fader / knob / checkbox / text_input。

pub mod automation;
pub mod button;
pub mod channel_fader_meter;
pub mod checkbox;
pub mod color_picker;
pub mod drag_in_rect;
pub mod drag_rect;
pub mod edge_scroll;
pub mod dropdown;
pub mod fader;
pub mod goniometer;
pub mod heavy;
pub mod knob;
pub mod label;
pub mod level_meter;
pub mod list_view;
pub mod loudness_graph;
pub mod loudness_meter;
pub mod menu;
pub mod needle_meter;
pub mod modal;
pub mod modulator_editor;
pub mod oscilloscope;
pub mod panel;
pub mod playhead;
pub mod reorderable_list;
pub mod scroll_area;
pub mod scrubable_number;
pub mod spectrum;
pub mod split_view;
pub mod tab_view;
pub mod text_input;
pub mod toggle_button;
pub mod waveform;

use std::any::Any;

use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect};

use crate::widgets::heavy::HeavyCtx;

/// muted な clip / note の塗り色。fill の alpha を落として lane 背景を
/// 透過させ、暗く沈める (REAPER / Ableton 流)。clip rect / note rect 共通で使う。
#[must_use]
pub fn muted_dim_fill(c: Color) -> Color {
    Color { a: c.a * 0.42, ..c }
}

/// muted な矩形に 45°(`╱`) の斜線ハッチを重ねる。線は rect の上下端を
/// 結ぶ平行線群 (`x + y = const`) を `spacing_px` 間隔で生成し、`scissor` で rect 内だけに
/// clip する (x が rect 外に伸びても scissor が切る)。clip / note 共通。
pub fn push_muted_hatch<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    rect: Rect,
    scissor: Rect,
    color: Color,
    spacing_px: f32,
    width_px: f32,
) {
    if rect.w <= 1.0 || rect.h <= 1.0 || spacing_px <= 0.0 {
        return;
    }
    let bottom = rect.y + rect.h;
    let lo = rect.x + rect.y;
    let hi = (rect.x + rect.w) + bottom;
    let mut segments: Vec<LineSegment> = Vec::new();
    let mut k = lo;
    while k <= hi {
        segments.push(LineSegment {
            a: [k - rect.y, rect.y],
            b: [k - bottom, bottom],
            color,
        });
        k += spacing_px;
    }
    if segments.is_empty() {
        return;
    }
    hctx.push_lines(LineBatch {
        segments: segments.into(),
        line_width_px: width_px,
        clip_rect: Some(scissor),
    });
}

/// ウィジェット永続状態の共通インタフェース (`Box<dyn WidgetState>` で保持するため)。
pub trait WidgetState: Any + Send + Sync {
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any + Send + Sync> WidgetState for T {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// button / label はそれぞれ `Ui` への inherent impl を提供する形なので、ここでは re-export 不要。
