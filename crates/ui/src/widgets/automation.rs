//! オートメーションカーブ widget (M5.5)。
//!
//! cubic Bezier flatten + Catmull-Rom 自動 tangent で、ユーザは点列 `&[(f32, f32)]` を
//! 渡すだけで滑らかな curve が描画される。各点はノードとして drag 編集できる。
//!
//! 設計:
//! - 点座標は rect 内の比率 `[0.0, 1.0]` で指定 (x = 時間軸、y = 値、上 = 1.0)
//! - 隣接 4 点 (P0, P1, P2, P3) から Catmull-Rom スプライン → cubic Bezier 制御点
//!   (B1 = P1 + (P2-P0)/6, B2 = P2 - (P3-P1)/6) を生成、P1-P2 間を Bezier flatten
//! - 端点は仮想点 (P0=P1, P3=P2) で代用 (= 端点の出入り角は直角、KISS)
//! - flatten は de Casteljau の中点分割で、control points の chord 距離が
//!   `style.max_segment_px` 未満まで再帰 (デフォルト 2.0 px)
//! - `on_change(idx, (x, y))` で 1 点だけ更新する Edit を組み立てる (no-Clone 不変条件
//!   と整合、Vec 全体の copy 不要)

use std::hash::Hash;

use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::{Ui, hovered};

/// flatten の最大再帰深度 (NaN / 異常値で無限再帰しないようガード)。
const MAX_FLATTEN_DEPTH: u32 = 16;

#[derive(Debug, Clone, Copy)]
pub struct AutomationCurveStyle {
    pub line_color: Color,
    pub line_width_px: f32,
    pub node_color: Color,
    pub node_radius_px: f32,
    pub node_hover_color: Color,
    pub node_drag_color: Color,
    /// Bezier flatten で許容する control points の chord 最大距離 (px)。
    pub max_segment_px: f32,
}

impl Default for AutomationCurveStyle {
    fn default() -> Self {
        Self {
            line_color: Color::rgb(0.42, 0.85, 0.95),
            line_width_px: 2.0,
            node_color: Color::rgb(0.95, 0.97, 1.00),
            node_radius_px: 5.0,
            node_hover_color: Color::rgb(1.0, 1.0, 0.6),
            node_drag_color: Color::rgb(0.95, 0.45, 0.40),
            max_segment_px: 2.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AutomationCurveResponse {
    pub hovered: bool,
    pub dragging: bool,
    pub hovered_point_index: Option<usize>,
}

#[derive(Debug, Default)]
pub(crate) struct AutomationCurveState {
    /// drag 中の点 index と drag 開始時の値 (x_frac, y_frac)。
    drag: Option<(usize, (f32, f32))>,
}

// ============================================================
// Bezier flatten
// ============================================================

/// 点 `p` と直線 `a-b` の垂直距離。
#[inline]
fn perpendicular_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len = dx.hypot(dy);
    if len < 1e-6 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    // 直線の法線方向への射影距離 = |cross(p-a, b-a)| / |b-a|
    ((p.0 - a.0) * dy - (p.1 - a.1) * dx).abs() / len
}

#[inline]
fn midpoint(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5)
}

/// cubic Bezier `(p0, p1, p2, p3)` を `max_dist` 以下の chord で適応分割し、`p3` を
/// 終点として `out` に push する。`p0` は呼び出し側で 1 度だけ push 済の前提。
fn flatten_cubic(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
    max_dist: f32,
    depth: u32,
    out: &mut Vec<(f32, f32)>,
) {
    let d1 = perpendicular_dist(p1, p0, p3);
    let d2 = perpendicular_dist(p2, p0, p3);
    let dist = d1.max(d2);
    if dist <= max_dist || depth >= MAX_FLATTEN_DEPTH {
        out.push(p3);
        return;
    }
    // de Casteljau 中点分割
    let q0 = midpoint(p0, p1);
    let q1 = midpoint(p1, p2);
    let q2 = midpoint(p2, p3);
    let r0 = midpoint(q0, q1);
    let r1 = midpoint(q1, q2);
    let s0 = midpoint(r0, r1);
    flatten_cubic(p0, q0, r0, s0, max_dist, depth + 1, out);
    flatten_cubic(s0, r1, q2, p3, max_dist, depth + 1, out);
}

/// 全カーブを flatten した点列を返す。`points` は rect 内の比率 [0,1]。
fn flatten_curve(points: &[(f32, f32)], rect: Rect, max_dist: f32) -> Vec<(f32, f32)> {
    if points.len() < 2 {
        return Vec::new();
    }
    let to_px = |(x, y): (f32, f32)| -> (f32, f32) {
        (rect.x + x * rect.w, rect.y + (1.0 - y) * rect.h)
    };
    let mut flat: Vec<(f32, f32)> = Vec::new();
    flat.push(to_px(points[0]));
    for i in 0..points.len() - 1 {
        let p1 = to_px(points[i]);
        let p2 = to_px(points[i + 1]);
        let p0 = if i == 0 { p1 } else { to_px(points[i - 1]) };
        let p3 = if i + 2 >= points.len() { p2 } else { to_px(points[i + 2]) };
        // Catmull-Rom → Bezier 制御点 (B0=P1, B1=P1+(P2-P0)/6, B2=P2-(P3-P1)/6, B3=P2)
        let b1 = (
            p1.0 + (p2.0 - p0.0) / 6.0,
            p1.1 + (p2.1 - p0.1) / 6.0,
        );
        let b2 = (
            p2.0 - (p3.0 - p1.0) / 6.0,
            p2.1 - (p3.1 - p1.1) / 6.0,
        );
        flatten_cubic(p1, b1, b2, p2, max_dist, 0, &mut flat);
    }
    flat
}

// ============================================================
// Public widget API
// ============================================================

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// オートメーションカーブ widget (M5.5)。
    ///
    /// `points` は rect 内の比率 `[0.0, 1.0]` の `(x, y)` 列 (x = 時間軸、y = 値で上 = 1.0)。
    /// 隣接点間は Catmull-Rom 自動 tangent + cubic Bezier flatten で滑らかに繋がれる。
    ///
    /// 各点はノードとして drag 編集可能で、移動時に `on_change(idx, (x, y))` を Edit に積む。
    /// Edit は `move |m| m.points[idx] = pos` のように 1 点だけ書き換えれば良い (= Vec 全体の
    /// copy 不要、no-Clone 不変条件と整合)。
    #[allow(
        clippy::too_many_arguments,
        clippy::many_single_char_names,
        clippy::too_many_lines
    )]
    pub fn automation_curve<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        points: &[(f32, f32)],
        style: AutomationCurveStyle,
        on_change: F,
    ) -> AutomationCurveResponse
    where
        F: FnOnce(usize, (f32, f32)) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"automation_curve", &id));
        let pointer = self.pointer;
        let mut response = AutomationCurveResponse {
            hovered: hovered(rect, pointer),
            dragging: false,
            hovered_point_index: None,
        };

        // hover 判定: 各点との距離 (radius * 2 内なら hit、選びやすさのため少し甘く)
        let hovered_idx: Option<usize> = pointer.pos.and_then(|(px, py)| {
            let r2 = (style.node_radius_px * 2.0).powi(2);
            for (i, &(x, y)) in points.iter().enumerate() {
                let nx = rect.x + x * rect.w;
                let ny = rect.y + (1.0 - y) * rect.h;
                if (px - nx).powi(2) + (py - ny).powi(2) <= r2 {
                    return Some(i);
                }
            }
            None
        });
        response.hovered_point_index = hovered_idx;

        // drag state 更新 (scope を分けて borrow を early release)
        let drag = {
            let state: &mut AutomationCurveState = self.widget_state(wid);
            if pointer.primary_just_pressed
                && let Some(idx) = hovered_idx
                && idx < points.len()
            {
                state.drag = Some((idx, points[idx]));
            }
            if pointer.primary_just_released {
                state.drag = None;
            }
            state.drag
        };

        // drag 中なら Edit 発行 (1 フレームに 1 回呼ばれる on_change はその場で消費)
        if let Some((idx, _initial)) = drag
            && let Some((px, py)) = pointer.pos
        {
            response.dragging = true;
            let new_x = ((px - rect.x) / rect.w.max(1.0)).clamp(0.0, 1.0);
            let new_y = (1.0 - (py - rect.y) / rect.h.max(1.0)).clamp(0.0, 1.0);
            let edit = on_change(idx, (new_x, new_y));
            self.push_edit(edit);
        }

        // input_hash で per-widget cache (with_widget_node)
        // 全点を flatten した bits で hash (~10-20 点なら軽量)
        let mut point_bits: Vec<u32> = Vec::with_capacity(points.len() * 2);
        for &(x, y) in points {
            point_bits.push(x.to_bits());
            point_bits.push(y.to_bits());
        }
        let drag_idx_tag = drag.map_or(u32::MAX, |(idx, _)| idx as u32);
        let hover_idx_tag = hovered_idx.map_or(u32::MAX, |idx| idx as u32);
        let input_hash = hash_inputs((
            b"automation_curve",
            (rect.x.to_bits(), rect.y.to_bits(), rect.w.to_bits(), rect.h.to_bits()),
            point_bits,
            (
                style.line_width_px.to_bits(),
                style.node_radius_px.to_bits(),
                style.max_segment_px.to_bits(),
            ),
            (
                style.line_color.r.to_bits(),
                style.line_color.g.to_bits(),
                style.line_color.b.to_bits(),
                style.line_color.a.to_bits(),
            ),
            (
                style.node_color.r.to_bits(),
                style.node_color.g.to_bits(),
                style.node_color.b.to_bits(),
                style.node_color.a.to_bits(),
            ),
            (drag_idx_tag, hover_idx_tag),
        ));

        // 描画 (flatten 結果 + 各点 rect 角丸円)
        let points_owned: Vec<(f32, f32)> = points.to_vec();
        let max_seg = style.max_segment_px;
        self.with_widget_node(wid, input_hash, move |ui| {
            let flat = flatten_curve(&points_owned, rect, max_seg);
            if flat.len() >= 2 {
                let segments: Vec<LineSegment> = flat
                    .windows(2)
                    .map(|w| LineSegment {
                        a: [w[0].0, w[0].1],
                        b: [w[1].0, w[1].1],
                        color: style.line_color,
                    })
                    .collect();
                ui.push_lines(LineBatch {
                    segments: segments.into(),
                    line_width_px: style.line_width_px.max(1.0),
                    clip_rect: Some(rect),
                });
            }
            // 各点を rect 角丸円 (knob 同パターン) で描画
            for (i, &(x, y)) in points_owned.iter().enumerate() {
                let nx = rect.x + x * rect.w;
                let ny = rect.y + (1.0 - y) * rect.h;
                let r = style.node_radius_px;
                let fill = if Some(i) == drag.map(|(idx, _)| idx) {
                    style.node_drag_color
                } else if Some(i) == hovered_idx {
                    style.node_hover_color
                } else {
                    style.node_color
                };
                ui.push_rect(RectCommand {
                    rect: Rect { x: nx - r, y: ny - r, w: r * 2.0, h: r * 2.0 },
                    fill,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [r; 4],
                    clip_rect: None,
                });
            }
        });

        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// p0..p3 が直線上に並んだとき、flatness = 0 で 1 segment (2 点) に収束する。
    #[test]
    fn flatten_cubic_returns_endpoint_for_straight_line() {
        let mut out = vec![(0.0, 0.0)];
        flatten_cubic((0.0, 0.0), (10.0, 0.0), (20.0, 0.0), (30.0, 0.0), 2.0, 0, &mut out);
        assert_eq!(out.len(), 2, "直線は 1 segment に収束 (got {} points)", out.len());
        assert_eq!(out[0], (0.0, 0.0));
        assert_eq!(out[1], (30.0, 0.0));
    }

    /// 制御点が chord から離れた曲線では複数 segment に分割される。
    #[test]
    fn flatten_cubic_subdivides_for_curved() {
        let mut out = vec![(0.0, 0.0)];
        flatten_cubic((0.0, 0.0), (10.0, 100.0), (20.0, 100.0), (30.0, 0.0), 2.0, 0, &mut out);
        assert!(
            out.len() > 4,
            "曲線は 4 segment 以上に分割される (got {} points)",
            out.len()
        );
    }

    /// `flatten_curve` で `points.len() < 2` なら空 Vec。
    #[test]
    fn flatten_curve_empty_for_single_point() {
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        assert!(flatten_curve(&[], rect, 2.0).is_empty());
        assert!(flatten_curve(&[(0.5, 0.5)], rect, 2.0).is_empty());
    }
}
