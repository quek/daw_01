// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `loudness_graph` / `loudness_histogram` widget — 時系列ラウドネスの表示
//! (daw_01 r.md #54)。
//!
//! **解析は一切しない**。呼び出し側が「区間を等分割したラウドネス列 [LUFS]」と
//! 「1 LU 刻みの分布」を渡し、この widget はそれを描くだけ。目標線を跨いだ部分の
//! 色分けと、クリック位置 (0..1) の通知だけがここの責務。
//!
//! `spectrum_analyzer` と同じ流儀: 値 → ピクセル列へ畳んで塗り + 折れ線、
//! `f32::NEG_INFINITY` は「値が無い」= 折れ線を切る。

use std::hash::Hash;

use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect, RectCommand};

use crate::theme::Palette;
use crate::DragKind;
use crate::ui::Ui;

const LABEL_FONT_PX: f32 = 9.0;

/// 縦軸に線を引く LUFS 値。表示レンジ外は自動で捨てる。
const GRID_LUFS: &[f32] = &[-6.0, -12.0, -18.0, -24.0, -30.0, -36.0, -42.0, -48.0];

#[derive(Debug, Clone, Copy)]
pub struct LoudnessGraphStyle {
    pub bg: Color,
    pub border: Color,
    pub grid: Color,
    pub label: Color,
    /// 主曲線 (Short-term) の塗りと線。
    pub fill: Color,
    pub line: Color,
    /// 副曲線 (Momentary) の線。塗らない。
    pub secondary_line: Color,
    /// 目標ラウドネスの水平線と、それを超えた部分の塗り。
    pub target_line: Color,
    pub over_fill: Color,
    /// 表示レンジ [LUFS] (下端, 上端)。
    pub range_lufs: (f32, f32),
    pub show_labels: bool,
}

impl LoudnessGraphStyle {
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            bg: p.inset_bg,
            border: p.border,
            grid: p.grid_line,
            label: p.text_dim.with_alpha(0.9),
            fill: p.accent.with_alpha(0.45),
            line: p.accent,
            secondary_line: p.text_dim.with_alpha(0.75),
            target_line: p.meter_yellow,
            over_fill: p.meter_red.with_alpha(0.55),
            range_lufs: (-50.0, -3.0),
            show_labels: true,
        }
    }
}

/// LUFS → rect 内の y (上端が `range.1`、下端が `range.0`)。
fn lufs_to_y(rect: Rect, lufs: f32, range: (f32, f32)) -> f32 {
    let span = (range.1 - range.0).max(1e-6);
    let t = ((lufs - range.0) / span).clamp(0.0, 1.0);
    rect.y + rect.h * (1.0 - t)
}

/// 値列をピクセル列へ畳む (最大値)。空列は直前の値で埋める。
fn to_columns(values: &[f32], columns: usize) -> Vec<f32> {
    let mut col = vec![f32::NEG_INFINITY; columns];
    if values.is_empty() || columns == 0 {
        return col;
    }
    for (i, &v) in values.iter().enumerate() {
        if !v.is_finite() {
            continue;
        }
        let c = ((i * columns) / values.len()).min(columns - 1);
        if v > col[c] {
            col[c] = v;
        }
    }
    // 値よりピクセルが多いと空列ができるので、直前の値で埋める
    // (曲線が櫛状に切れて「無音区間」に見えるのを防ぐ)。
    let mut last = f32::NEG_INFINITY;
    for v in &mut col {
        if v.is_finite() {
            last = *v;
        } else if last.is_finite() {
            *v = last;
        }
    }
    col
}

/// 列ごとの値を折れ線に変換する。値が無い列で線を切る。
fn polyline(cols: &[f32], inner: Rect, range: (f32, f32), color: Color) -> Vec<LineSegment> {
    let mut segs = Vec::new();
    let mut prev: Option<[f32; 2]> = None;
    for (c, &v) in cols.iter().enumerate() {
        if !v.is_finite() {
            prev = None;
            continue;
        }
        let p = [inner.x + c as f32 + 0.5, lufs_to_y(inner, v, range)];
        if let Some(q) = prev {
            segs.push(LineSegment { a: q, b: p, color });
        }
        prev = Some(p);
    }
    segs
}

impl<M: ?Sized + 'static> Ui<'_, M> {
    /// ラウドネスの時系列を描く。
    ///
    /// `primary` / `secondary` は測定区間を等分割した LUFS 値の列
    /// (`f32::NEG_INFINITY` = まだ値が無い)。`target` を渡すと水平線を引き、
    /// それを超えた部分の塗りを `over_fill` にする。
    ///
    /// 戻り値 = クリックされた位置 (0.0 = 区間先頭、1.0 = 区間末尾)。
    /// 呼び出し側はこれをプレイヘッド移動に使える。
    pub fn loudness_graph(
        &mut self,
        id: impl Hash,
        rect: Rect,
        primary: &[f32],
        secondary: &[f32],
        target: Option<f32>,
        style: &LoudnessGraphStyle,
    ) -> Option<f32> {
        if rect.w < 4.0 || rect.h < 4.0 {
            return None;
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
        self.draw_loudness_grid_lines(inner, style);

        let columns = inner.w.floor().max(1.0) as usize;
        let cols = to_columns(primary, columns);
        let target_y = target.map(|t| lufs_to_y(inner, t, style.range_lufs));

        self.draw_loudness_fill(inner, &cols, target_y, style);

        // 副曲線 (Momentary) → 主曲線の輪郭 → 目標線 の順に重ねる。
        if !secondary.is_empty() {
            let segs = polyline(
                &to_columns(secondary, columns),
                inner,
                style.range_lufs,
                style.secondary_line,
            );
            if !segs.is_empty() {
                self.push_lines(LineBatch {
                    segments: segs.into(),
                    line_width_px: 1.0,
                    clip_rect: Some(inner),
                });
            }
        }
        let segs = polyline(&cols, inner, style.range_lufs, style.line);
        if !segs.is_empty() {
            self.push_lines(LineBatch {
                segments: segs.into(),
                line_width_px: 1.0,
                clip_rect: Some(inner),
            });
        }
        if let Some(ty) = target_y {
            self.push_lines(LineBatch {
                segments: vec![LineSegment {
                    a: [inner.x, ty.round()],
                    b: [inner.x + inner.w, ty.round()],
                    color: style.target_line,
                }]
                .into(),
                line_width_px: 1.0,
                clip_rect: Some(inner),
            });
        }

        // **縦軸ラベルは塗りの後**。先に描くと、曲線の塗り (左端から立ち上がる)
        // に覆われてコントラストが落ち、長い範囲では常時読めなくなる。
        self.draw_loudness_grid_labels(inner, style);

        // クリックで位置を返す (press した瞬間に 1 度だけ)。
        let d = self.take_drag_in_rect(("loudness_graph", &id), rect)?;
        if !matches!(d.kind, DragKind::Started) {
            return None;
        }
        let (px, _) = d.current;
        Some(((px - inner.x) / inner.w).clamp(0.0, 1.0))
    }

    /// 曲線下の塗り (1px 幅の縦 rect 列)。目標線より上に出ている分だけ色を変える
    /// (= 「どこがどれだけ超えているか」が形で読める)。
    fn draw_loudness_fill(
        &mut self,
        inner: Rect,
        cols: &[f32],
        target_y: Option<f32>,
        style: &LoudnessGraphStyle,
    ) {
        let bottom = inner.y + inner.h;
        for (c, &v) in cols.iter().enumerate() {
            if !v.is_finite() || v <= style.range_lufs.0 {
                continue;
            }
            let y = lufs_to_y(inner, v, style.range_lufs);
            if bottom - y <= 0.0 {
                continue;
            }
            let x = inner.x + c as f32;
            let mut bar = |y: f32, h: f32, fill: Color| {
                self.push_rect(RectCommand {
                    rect: Rect { x, y, w: 1.0, h },
                    fill,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: Some(inner),
                });
            };
            match target_y {
                Some(ty) if y < ty => {
                    bar(y, ty - y, style.over_fill);
                    bar(ty, bottom - ty, style.fill);
                }
                _ => bar(y, bottom - y, style.fill),
            }
        }
    }

    /// グリッド線 (塗りの**下**)。
    fn draw_loudness_grid_lines(&mut self, inner: Rect, style: &LoudnessGraphStyle) {
        let mut segs: Vec<LineSegment> = Vec::new();
        for l in GRID_LUFS {
            if *l < style.range_lufs.0 || *l > style.range_lufs.1 {
                continue;
            }
            let y = lufs_to_y(inner, *l, style.range_lufs).round();
            segs.push(LineSegment {
                a: [inner.x, y],
                b: [inner.x + inner.w, y],
                color: style.grid,
            });
        }
        if !segs.is_empty() {
            self.push_lines(LineBatch {
                segments: segs.into(),
                line_width_px: 1.0,
                clip_rect: Some(inner),
            });
        }
    }

    /// 縦軸ラベル (塗りの**上**)。塗りの下に敷くと読めなくなる。
    fn draw_loudness_grid_labels(&mut self, inner: Rect, style: &LoudnessGraphStyle) {
        if !style.show_labels {
            return;
        }
        for l in GRID_LUFS {
            if *l < style.range_lufs.0 || *l > style.range_lufs.1 {
                continue;
            }
            let y = lufs_to_y(inner, *l, style.range_lufs);
            if y - LABEL_FONT_PX < inner.y {
                continue;
            }
            let text = format!("{l:.0}");
            let top = y - LABEL_FONT_PX - 1.0;
            // 曲線の塗りの上に乗るので、暗いバッキングチップでコントラストを
            // 保証する (可変背景の上の標識の作法。素の text_dim だと accent 塗りの
            // 上でコントラスト比 1.3 まで落ちて判読できない)。
            self.push_rect(RectCommand {
                rect: Rect {
                    x: inner.x + 1.0,
                    y: top - 1.0,
                    w: LABEL_FONT_PX * text.chars().count() as f32 * 0.62 + 5.0,
                    h: LABEL_FONT_PX + 3.0,
                },
                fill: style.bg.with_alpha(0.78),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [2.0; 4],
                clip_rect: Some(inner),
            });
            self.push_text(GlyphArea {
                text: text.into(),
                left: inner.x + 3.0,
                top,
                font_size: LABEL_FONT_PX,
                line_height: LABEL_FONT_PX + 2.0,
                color: style.label,
                clip_rect: Some(inner),
                ..GlyphArea::default()
            });
        }
    }

    /// ラウドネスの分布 (ヒストグラム) を横向きに描く。
    ///
    /// `bins[i]` は `min_lufs + i * step_lu` から `step_lu` 幅に入った回数。
    /// 縦軸がラウドネス (グラフと同じ向き)、横に伸びる棒が回数。
    pub fn loudness_histogram(
        &mut self,
        id: impl Hash,
        rect: Rect,
        bins: &[u32],
        min_lufs: f32,
        step_lu: f32,
        style: &LoudnessGraphStyle,
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
        let max = bins.iter().copied().max().unwrap_or(0);
        if max == 0 {
            return;
        }
        // 表示レンジ内の bin だけを、グラフと同じ縦写像で描く。
        for (i, &c) in bins.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let lo = min_lufs + i as f32 * step_lu;
            let hi = lo + step_lu;
            if hi < style.range_lufs.0 || lo > style.range_lufs.1 {
                continue;
            }
            let y_top = lufs_to_y(inner, hi, style.range_lufs);
            let y_bot = lufs_to_y(inner, lo, style.range_lufs);
            let h = (y_bot - y_top).max(1.0);
            let w = (inner.w * (c as f32 / max as f32)).max(1.0);
            self.push_rect(RectCommand {
                rect: Rect { x: inner.x, y: y_top, w, h },
                fill: style.fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: Some(inner),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 100.0, h: 50.0 }
    }

    #[test]
    fn 値の無い列で折れ線が切れる() {
        // 未到達区間 (窓が埋まる前) を跨いで線を引くと、存在しない推移が見えてしまう。
        let cols = [-20.0, -21.0, f32::NEG_INFINITY, -30.0, -31.0];
        let segs = polyline(&cols, rect(), (-50.0, -3.0), Color::WHITE);
        assert_eq!(segs.len(), 2);
    }

    #[test]
    fn 列への畳み込みは最大値で穴を埋める() {
        // 値 2 個 → 列 6 個。前半 3 列が値 0、後半 3 列が値 1 になり、穴は残らない。
        let cols = to_columns(&[-20.0, -10.0], 6);
        assert_eq!(cols, vec![-20.0, -20.0, -20.0, -10.0, -10.0, -10.0]);
    }

    #[test]
    fn 先頭が未到達でも後続の値で埋め戻さない() {
        // 「まだ測っていない」区間を勝手に埋めると、走査中のグラフが右から
        // 左へ伸びているように見えてしまう。
        let cols = to_columns(&[f32::NEG_INFINITY, -10.0], 4);
        assert!(cols[0].is_infinite());
        assert!(cols[1].is_infinite());
        assert_eq!(cols[2], -10.0);
    }

    #[test]
    fn 上端と下端が表示レンジに対応する() {
        let r = rect();
        let range = (-50.0, -10.0);
        assert!((lufs_to_y(r, -10.0, range) - r.y).abs() < 1e-3);
        assert!((lufs_to_y(r, -50.0, range) - (r.y + r.h)).abs() < 1e-3);
        // レンジ外はクランプ (枠外へはみ出さない)。
        assert!((lufs_to_y(r, 0.0, range) - r.y).abs() < 1e-3);
        assert!((lufs_to_y(r, -90.0, range) - (r.y + r.h)).abs() < 1e-3);
    }
}
