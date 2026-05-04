//! `time_ruler` + `bar_beat_grid` widget — DAW で頻出する時間軸 UI (M7 Phase 27)。
//!
//! piano_roll / arrangement で個別実装していた grid + ruler 描画を library 化。
//! `TimeMapping` (拍子・tempo・sample rate) と `ViewportState1D` (表示範囲) に依存。

use std::hash::Hash;

use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect};

use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::time::TimeMapping;
use crate::ui::Ui;
use crate::viewport::ViewportState1D;

const RULER_FONT: f32 = 11.0;
const RULER_LABEL_PAD_X: f32 = 3.0;

/// `Ui::time_ruler` のスタイル設定。
#[derive(Debug, Clone, Copy)]
pub struct TimeRulerStyle {
    pub bg: Color,
    pub tick_color: Color,
    pub label_color: Color,
    pub bar_tick_height: f32,
    pub beat_tick_height: f32,
}

impl Default for TimeRulerStyle {
    fn default() -> Self {
        Self {
            bg: Color::rgb(0.13, 0.14, 0.17),
            tick_color: Color::rgb(0.55, 0.60, 0.68),
            label_color: Color::rgb(0.85, 0.88, 0.92),
            bar_tick_height: 12.0,
            beat_tick_height: 5.0,
        }
    }
}

/// `Ui::bar_beat_grid` のスタイル設定。
#[derive(Debug, Clone, Copy)]
pub struct BarBeatGridStyle {
    pub bar_color: Color,
    pub beat_color: Color,
    pub bar_line_width: f32,
    pub beat_line_width: f32,
}

impl Default for BarBeatGridStyle {
    fn default() -> Self {
        Self {
            bar_color: Color::rgba(1.0, 1.0, 1.0, 0.18),
            beat_color: Color::rgba(1.0, 1.0, 1.0, 0.07),
            bar_line_width: 1.0,
            beat_line_width: 1.0,
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// time_ruler widget。`rect` 内に拍 / 小節 / SMPTE label と tick を描画。
    /// X 軸は `viewport` (sample 単位) を参照。表示モードは `mapping.display`。
    pub fn time_ruler(
        &mut self,
        id: impl Hash,
        rect: Rect,
        mapping: TimeMapping,
        viewport: ViewportState1D,
        style: TimeRulerStyle,
    ) {
        let wid = WidgetId::ROOT.child((b"time_ruler", &id));
        let input_hash = hash_inputs((
            b"time_ruler",
            (rect.x.to_bits(), rect.y.to_bits(), rect.w.to_bits(), rect.h.to_bits()),
            (mapping.sample_rate.to_bits(), mapping.tempo_bpm.to_bits(), mapping.time_sig),
            (viewport.view_start.to_bits(), viewport.view_len.to_bits()),
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            // 背景
            ui.push_rect(daw_ui_renderer::RectCommand {
                rect,
                fill: style.bg,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });

            let bar_y = rect.y + rect.h;
            let beat_y_start = bar_y - style.beat_tick_height;
            let bar_y_start = bar_y - style.bar_tick_height;
            let mut tick_segs: Vec<LineSegment> = Vec::new();

            // 拍 / 小節を viewport 範囲で iterate
            let view_end = viewport.view_start + viewport.view_len;
            let spb = mapping.samples_per_beat();
            let sec_per_bar_beats =
                f64::from(mapping.time_sig.0) * 4.0 / f64::from(mapping.time_sig.1);
            let beat_index_start = (viewport.view_start / spb).floor() as i64;
            let beat_index_end = (view_end / spb).ceil() as i64;

            for bi in beat_index_start..=beat_index_end {
                let s = (bi as f64) * spb;
                if s < viewport.view_start || s > view_end {
                    continue;
                }
                let local_x = viewport.unit_to_px(s, rect.w);
                let x = rect.x + local_x;
                if x < rect.x || x > rect.x + rect.w {
                    continue;
                }
                let is_bar = ((bi as f64).rem_euclid(sec_per_bar_beats)).abs() < 1e-6;
                let y_top = if is_bar { bar_y_start } else { beat_y_start };
                tick_segs.push(LineSegment {
                    a: [x, y_top],
                    b: [x, bar_y],
                    color: style.tick_color,
                });
            }
            if !tick_segs.is_empty() {
                ui.push_lines(LineBatch {
                    segments: tick_segs.into(),
                    line_width_px: 1.0,
                    clip_rect: Some(rect),
                });
            }

            // bar label (小節番号 / SMPTE / 秒)
            let bar_index_start = (viewport.view_start / mapping.samples_per_bar()).floor() as i64;
            let bar_index_end = (view_end / mapping.samples_per_bar()).ceil() as i64;
            for bar in bar_index_start..=bar_index_end {
                let s = (bar as f64) * mapping.samples_per_bar();
                if s < viewport.view_start || s > view_end {
                    continue;
                }
                let local_x = viewport.unit_to_px(s, rect.w);
                let x = rect.x + local_x;
                let label = mapping.format(s);
                ui.push_text(GlyphArea {
                    text: label.into(),
                    left: x + RULER_LABEL_PAD_X,
                    top: rect.y + 2.0,
                    font_size: RULER_FONT,
                    line_height: RULER_FONT * 1.2,
                    color: style.label_color,
                    clip_rect: Some(rect),
                });
            }
        });
    }

    /// bar/beat grid widget。`rect` 内に縦線で拍/小節を描画 (piano_roll / arrangement の grid 置換)。
    pub fn bar_beat_grid(
        &mut self,
        id: impl Hash,
        rect: Rect,
        mapping: TimeMapping,
        viewport: ViewportState1D,
        style: BarBeatGridStyle,
    ) {
        let wid = WidgetId::ROOT.child((b"bar_beat_grid", &id));
        let input_hash = hash_inputs((
            b"bar_beat_grid",
            (rect.x.to_bits(), rect.y.to_bits(), rect.w.to_bits(), rect.h.to_bits()),
            (mapping.sample_rate.to_bits(), mapping.tempo_bpm.to_bits(), mapping.time_sig),
            (viewport.view_start.to_bits(), viewport.view_len.to_bits()),
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            let view_end = viewport.view_start + viewport.view_len;
            let spb = mapping.samples_per_beat();
            let beats_per_bar =
                f64::from(mapping.time_sig.0) * 4.0 / f64::from(mapping.time_sig.1);
            let beat_index_start = (viewport.view_start / spb).floor() as i64;
            let beat_index_end = (view_end / spb).ceil() as i64;

            let mut bar_segs: Vec<LineSegment> = Vec::new();
            let mut beat_segs: Vec<LineSegment> = Vec::new();
            for bi in beat_index_start..=beat_index_end {
                let s = (bi as f64) * spb;
                if s < viewport.view_start || s > view_end {
                    continue;
                }
                let local_x = viewport.unit_to_px(s, rect.w);
                let x = rect.x + local_x;
                if x < rect.x || x > rect.x + rect.w {
                    continue;
                }
                let is_bar = ((bi as f64).rem_euclid(beats_per_bar)).abs() < 1e-6;
                let seg = LineSegment {
                    a: [x, rect.y],
                    b: [x, rect.y + rect.h],
                    color: if is_bar { style.bar_color } else { style.beat_color },
                };
                if is_bar {
                    bar_segs.push(seg);
                } else {
                    beat_segs.push(seg);
                }
            }
            if !beat_segs.is_empty() {
                ui.push_lines(LineBatch {
                    segments: beat_segs.into(),
                    line_width_px: style.beat_line_width,
                    clip_rect: Some(rect),
                });
            }
            if !bar_segs.is_empty() {
                ui.push_lines(LineBatch {
                    segments: bar_segs.into(),
                    line_width_px: style.bar_line_width,
                    clip_rect: Some(rect),
                });
            }
        });
    }
}
