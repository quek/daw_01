//! `oscilloscope` widget — 列ごとの min/max で波形を描く (daw_01 r.md #50)。
//!
//! 呼び出し側が「列ごとの `[Lmin, Lmax, Rmin, Rmax]`」を渡す。トリガ検出も
//! サブサンプル補間もオーディオ側の責務で、この widget は縦線を並べるだけ。
//!
//! 1px = 1 時間区間なので **区間内の min/max を縦線で描く** のが正しい
//! (中央値だけを折れ線で結ぶと、区間内のピークが消えて波形が痩せる)。
//! 列が 1 本の縦線に潰れないよう、隣の列と連結して途切れを防ぐ。

use std::hash::Hash;

use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::theme::Palette;
use crate::ui::Ui;

/// 1 列ぶんの `[Lmin, Lmax, Rmin, Rmax]`。
pub type ScopeColumn = [f32; 4];

#[derive(Debug, Clone, Copy)]
pub struct OscilloscopeStyle {
    pub bg: Color,
    pub border: Color,
    pub grid: Color,
    /// 中心線 (0 レベル)。
    pub center: Color,
    pub left_trace: Color,
    pub right_trace: Color,
    /// 縦倍率。1.0 でフルスケール (±1.0) が rect の高さいっぱい。
    pub gain: f32,
}

impl OscilloscopeStyle {
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            bg: p.inset_bg,
            border: p.border,
            grid: p.grid_line,
            center: p.grid_line_strong,
            left_trace: p.meter_green,
            right_trace: p.accent,
            gain: 1.0,
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 波形を描く。`columns` は左端 → 右端の順。
    pub fn oscilloscope(
        &mut self,
        id: impl Hash,
        rect: Rect,
        columns: &[ScopeColumn],
        style: &OscilloscopeStyle,
    ) {
        let _ = id;
        if rect.w < 4.0 || rect.h < 4.0 {
            return;
        }
        self.push_rect(RectCommand {
            rect,
            fill: style.bg,
            border: style.border,
            border_width: 1.0,
            radius: [2.0; 4],
            clip_rect: None,
        });
        let inner = Rect {
            x: rect.x + 1.0,
            y: rect.y + 1.0,
            w: (rect.w - 2.0).max(1.0),
            h: (rect.h - 2.0).max(1.0),
        };
        let mid_y = inner.y + inner.h * 0.5;

        // グリッド (±0.5) と中心線。
        let half = inner.h * 0.25;
        self.push_lines(LineBatch {
            segments: vec![
                LineSegment {
                    a: [inner.x, mid_y - half],
                    b: [inner.x + inner.w, mid_y - half],
                    color: style.grid,
                },
                LineSegment {
                    a: [inner.x, mid_y + half],
                    b: [inner.x + inner.w, mid_y + half],
                    color: style.grid,
                },
                LineSegment {
                    a: [inner.x, mid_y],
                    b: [inner.x + inner.w, mid_y],
                    color: style.center,
                },
            ]
            .into(),
            line_width_px: 1.0,
            clip_rect: Some(inner),
        });

        if columns.is_empty() {
            return;
        }
        let pixels = inner.w.floor().max(1.0) as usize;
        let cols = fold_columns(columns, pixels);

        let scale = inner.h * 0.5 * style.gain;
        let to_y = |v: f32| (mid_y - v * scale).clamp(inner.y, inner.y + inner.h);
        let mut segs: Vec<LineSegment> = Vec::with_capacity(pixels * 2);
        // R を先に描いて L を上に重ねる (L が主 = 手前)。
        for (ch, color) in [(1_usize, style.right_trace), (0, style.left_trace)] {
            let (lo_i, hi_i) = if ch == 0 { (0, 1) } else { (2, 3) };
            let mut prev_hi: Option<f32> = None;
            let mut prev_lo: Option<f32> = None;
            for (p, c) in cols.iter().enumerate() {
                if c[lo_i] > c[hi_i] {
                    prev_hi = None;
                    prev_lo = None;
                    continue;
                }
                let x = inner.x + p as f32 + 0.5;
                let (mut lo, mut hi) = (c[lo_i], c[hi_i]);
                // 隣の列と連結して、急峻な波形でも縦線が飛ばないようにする。
                if let (Some(ph), Some(pl)) = (prev_hi, prev_lo) {
                    hi = hi.max(ph.min(pl));
                    lo = lo.min(pl.max(ph));
                }
                let (y0, y1) = (to_y(hi), to_y(lo));
                segs.push(LineSegment {
                    a: [x, y0],
                    // 完全に潰れると線分がラスタライズされないので最低 1px 立てる。
                    b: [x, if (y1 - y0).abs() < 1.0 { y0 + 1.0 } else { y1 }],
                    color,
                });
                prev_hi = Some(c[hi_i]);
                prev_lo = Some(c[lo_i]);
            }
        }
        if !segs.is_empty() {
            self.push_lines(LineBatch {
                segments: segs.into(),
                line_width_px: 1.0,
                clip_rect: Some(inner),
            });
        }
    }
}

/// 入力列をピクセル列へ min/max で畳む。ピクセルの方が多いときは直前の列を
/// 引き伸ばして途切れを作らない。空きは `[MAX, MIN, MAX, MIN]` のまま残す
/// (= 呼び出し側が「データ無し」と判定できる)。
fn fold_columns(columns: &[ScopeColumn], pixels: usize) -> Vec<ScopeColumn> {
    let mut out = vec![[f32::MAX, f32::MIN, f32::MAX, f32::MIN]; pixels];
    if columns.is_empty() || pixels == 0 {
        return out;
    }
    let mut used = vec![false; pixels];
    for (i, c) in columns.iter().enumerate() {
        let p = ((i * pixels) / columns.len()).min(pixels - 1);
        let d = &mut out[p];
        d[0] = d[0].min(c[0]);
        d[1] = d[1].max(c[1]);
        d[2] = d[2].min(c[2]);
        d[3] = d[3].max(c[3]);
        used[p] = true;
    }
    let mut last: Option<ScopeColumn> = None;
    for (p, u) in used.iter().enumerate() {
        if *u {
            last = Some(out[p]);
        } else if let Some(v) = last {
            out[p] = v;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 縮小時に区間内のピークを取りこぼさない (痩せた波形にならない)。
    #[test]
    fn folding_keeps_the_extremes_of_every_source_column() {
        let src = vec![
            [-0.1, 0.1, -0.2, 0.2],
            [-0.9, 0.3, -0.1, 0.8], // この列の -0.9 / 0.8 が生き残るべき
            [-0.2, 0.2, -0.3, 0.1],
            [-0.1, 0.4, -0.1, 0.1],
        ];
        let out = fold_columns(&src, 2);
        assert_eq!(out.len(), 2);
        assert!((out[0][0] - (-0.9)).abs() < 1e-6, "L min lost: {:?}", out[0]);
        assert!((out[0][3] - 0.8).abs() < 1e-6, "R max lost: {:?}", out[0]);
        assert!((out[1][1] - 0.4).abs() < 1e-6, "L max lost: {:?}", out[1]);
    }

    /// 拡大時は直前の列で埋めて、線が途切れないようにする。
    #[test]
    fn folding_fills_gaps_when_there_are_more_pixels_than_columns() {
        let src = vec![[-0.5, 0.5, -0.5, 0.5], [-0.25, 0.25, -0.25, 0.25]];
        let out = fold_columns(&src, 6);
        assert!(out.iter().all(|c| c[0] <= c[1]), "gap left unfilled: {out:?}");
    }

    #[test]
    fn empty_input_yields_columns_marked_as_having_no_data() {
        let out = fold_columns(&[], 4);
        assert!(out.iter().all(|c| c[0] > c[1]));
    }
}
