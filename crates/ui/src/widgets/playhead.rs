//! Playhead 縦線描画の共通 helper (M9 Phase 45e で `piano_roll` から切り出し)。
//!
//! `piano_roll` (M9 Phase 45c) と `arrangement` (M9 Phase 45e) の両方が、time で動く
//! 1 本の縦線を描画する。primitive は `LineBatch + LineSegment` 1 本 + clip_rect なし。
//! cached の **外** で毎フレーム呼ぶ前提 (playhead_beat の変化で全背景再描画は無駄)。

use daw_ui_renderer::{Color, LineBatch, LineSegment};

use crate::widgets::heavy::HeavyCtx;

/// `x` を中心に `y_top..y_bottom` の縦線 1 本。`y_bottom <= y_top` なら no-op。
pub(crate) fn draw_playhead_line<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    x: f32,
    y_top: f32,
    y_bottom: f32,
    color: Color,
    width_px: f32,
) {
    if y_bottom <= y_top {
        return;
    }
    hctx.push_lines(LineBatch {
        segments: vec![LineSegment { a: [x, y_top], b: [x, y_bottom], color }].into(),
        line_width_px: width_px,
        clip_rect: None,
    });
}
