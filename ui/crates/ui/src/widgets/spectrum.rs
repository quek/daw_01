// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `spectrum_analyzer` widget — 対数周波数軸のスペクトラム表示 (daw_01 r.md #50)。
//!
//! **解析は一切しない**。呼び出し側が「対数等間隔のバンドごとの dB 値」を渡し、
//! この widget はそれをピクセル列へ畳んで描くだけ (FFT / 窓 / 傾き補正は
//! オーディオ側の責務)。バンド数はピクセル幅より多い前提で、列への集約は
//! **最大値** で行う (max は結合的なので、解析側で一度畳んでいても結果は同じ)。
//!
//! 描画は塗り (バンドごとの縦 rect) + ピーク保持線 (polyline) + グリッド。
//! `cached` は使わない — 中身が毎フレーム変わるのでキャッシュが必ず miss する。

use std::hash::Hash;

use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect, RectCommand};

use crate::theme::Palette;
use crate::ui::Ui;

/// グリッド線を引く周波数 [Hz] とラベル。
const GRID_HZ: &[(f32, &str)] = &[
    (50.0, "50"),
    (100.0, "100"),
    (200.0, "200"),
    (500.0, "500"),
    (1000.0, "1k"),
    (2000.0, "2k"),
    (5000.0, "5k"),
    (10000.0, "10k"),
];

/// グリッド線を引く dB 値 (0 dBFS からの深さ)。
const GRID_DB: &[f32] = &[-12.0, -24.0, -36.0, -48.0, -60.0, -72.0, -84.0];

const LABEL_FONT_PX: f32 = 9.0;

#[derive(Debug, Clone, Copy)]
pub struct SpectrumStyle {
    pub bg: Color,
    pub border: Color,
    /// 塗りの色 (下端に向かって薄くはしない、単色塗り)。
    pub fill: Color,
    /// 上端の輪郭線。
    pub outline: Color,
    /// ピーク保持線。
    pub hold: Color,
    pub grid: Color,
    pub label: Color,
    /// 表示レンジの下端 [dB] (上端は 0 dBFS 固定)。
    pub floor_db: f32,
    /// 周波数軸の範囲 [Hz]。
    pub f_min: f32,
    pub f_max: f32,
    /// 周波数 / dB のラベルを描くか。
    pub show_labels: bool,
}

impl SpectrumStyle {
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            bg: p.inset_bg,
            border: p.border,
            fill: p.accent.with_alpha(0.55),
            outline: p.accent,
            hold: p.text_dim.with_alpha(0.85),
            grid: p.grid_line,
            label: p.text_dim.with_alpha(0.9),
            floor_db: -100.0,
            f_min: 20.0,
            f_max: 20_000.0,
            show_labels: true,
        }
    }
}

/// 周波数 → rect 内の x 座標 (対数)。
fn freq_to_x(rect: Rect, f: f32, f_min: f32, f_max: f32) -> f32 {
    let t = (f / f_min).max(1e-6).ln() / (f_max / f_min).ln();
    rect.x + rect.w * t.clamp(0.0, 1.0)
}

/// dB → rect 内の y 座標 (0 dBFS が上端、`floor_db` が下端の線形)。
fn db_to_y(rect: Rect, db: f32, floor_db: f32) -> f32 {
    let t = ((db - floor_db) / -floor_db).clamp(0.0, 1.0);
    rect.y + rect.h * (1.0 - t)
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// スペクトラムを描く。`bands_db` / `hold_db` は**対数等間隔**のバンド値
    /// (index 0 = `style.f_min`、末尾 = `style.f_max`)。`hold_db` が空ならピーク線を描かない。
    pub fn spectrum_analyzer(
        &mut self,
        id: impl Hash,
        rect: Rect,
        bands_db: &[f32],
        hold_db: &[f32],
        style: &SpectrumStyle,
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
        self.draw_spectrum_grid(inner, style);
        if bands_db.is_empty() {
            return;
        }

        // バンド → ピクセル列 (最大値で畳む)。
        let columns = inner.w.floor().max(1.0) as usize;
        let mut col_db = vec![f32::NEG_INFINITY; columns];
        let n = bands_db.len();
        for (b, &db) in bands_db.iter().enumerate() {
            let c = (b * columns) / n;
            let c = c.min(columns - 1);
            if db > col_db[c] {
                col_db[c] = db;
            }
        }
        // バンドよりピクセルが多い場合に空列ができるので、直前の値で埋める。
        let mut last = f32::NEG_INFINITY;
        for v in &mut col_db {
            if v.is_finite() {
                last = *v;
            } else if last.is_finite() {
                *v = last;
            }
        }

        // 塗り (1px 幅の縦 rect 列) — rect を連続 push するので 1 run に畳まれる。
        for (c, &db) in col_db.iter().enumerate() {
            if !db.is_finite() || db <= style.floor_db {
                continue;
            }
            let y = db_to_y(inner, db, style.floor_db);
            let h = inner.y + inner.h - y;
            if h <= 0.0 {
                continue;
            }
            self.push_rect(RectCommand {
                rect: Rect { x: inner.x + c as f32, y, w: 1.0, h },
                fill: style.fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: Some(inner),
            });
        }

        // 上端の輪郭線 (塗りだけだと形が読みにくい)。
        let outline = polyline(&col_db, inner, style.floor_db, style.outline);
        if !outline.is_empty() {
            self.push_lines(LineBatch {
                segments: outline.into(),
                line_width_px: 1.0,
                clip_rect: Some(inner),
            });
        }

        // ピーク保持線。
        if !hold_db.is_empty() {
            let mut hold_col = vec![f32::NEG_INFINITY; columns];
            let hn = hold_db.len();
            for (b, &db) in hold_db.iter().enumerate() {
                let c = ((b * columns) / hn).min(columns - 1);
                if db > hold_col[c] {
                    hold_col[c] = db;
                }
            }
            let mut last = f32::NEG_INFINITY;
            for v in &mut hold_col {
                if v.is_finite() {
                    last = *v;
                } else if last.is_finite() {
                    *v = last;
                }
            }
            let segs = polyline(&hold_col, inner, style.floor_db, style.hold);
            if !segs.is_empty() {
                self.push_lines(LineBatch {
                    segments: segs.into(),
                    line_width_px: 1.0,
                    clip_rect: Some(inner),
                });
            }
        }
    }

    fn draw_spectrum_grid(&mut self, inner: Rect, style: &SpectrumStyle) {
        let mut segs: Vec<LineSegment> = Vec::new();
        for (f, _) in GRID_HZ {
            if *f < style.f_min || *f > style.f_max {
                continue;
            }
            let x = freq_to_x(inner, *f, style.f_min, style.f_max).round();
            segs.push(LineSegment {
                a: [x, inner.y],
                b: [x, inner.y + inner.h],
                color: style.grid,
            });
        }
        for db in GRID_DB {
            if *db < style.floor_db {
                continue;
            }
            let y = db_to_y(inner, *db, style.floor_db).round();
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
        if !style.show_labels {
            return;
        }
        for (f, label) in GRID_HZ {
            if *f < style.f_min || *f > style.f_max {
                continue;
            }
            let x = freq_to_x(inner, *f, style.f_min, style.f_max) + 2.0;
            if x + LABEL_FONT_PX * 2.0 > inner.x + inner.w {
                continue;
            }
            self.push_text(GlyphArea {
                text: (*label).into(),
                left: x,
                top: inner.y + inner.h - LABEL_FONT_PX - 3.0,
                font_size: LABEL_FONT_PX,
                line_height: LABEL_FONT_PX + 2.0,
                color: style.label,
                clip_rect: Some(inner),
                ..GlyphArea::default()
            });
        }
    }
}

/// 列ごとの dB を上端の折れ線に変換する。
fn polyline(col_db: &[f32], inner: Rect, floor_db: f32, color: Color) -> Vec<LineSegment> {
    let mut segs = Vec::new();
    let mut prev: Option<[f32; 2]> = None;
    for (c, &db) in col_db.iter().enumerate() {
        if !db.is_finite() {
            prev = None;
            continue;
        }
        let p = [inner.x + c as f32 + 0.5, db_to_y(inner, db, floor_db)];
        if let Some(q) = prev {
            segs.push(LineSegment { a: q, b: p, color });
        }
        prev = Some(p);
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 200.0, h: 100.0 }
    }

    /// 折れ線は「有限な値の連続区間」ごとに切れる (無音バンドを跨いで直線が
    /// 引かれると、存在しないピークが見えてしまう)。
    #[test]
    fn polyline_breaks_across_non_finite_bands() {
        let r = rect();
        let cols = [-10.0, -12.0, f32::NEG_INFINITY, -20.0, -22.0];
        let segs = polyline(&cols, r, -100.0, Color::WHITE);
        // 2 本 (0-1) + 1 本 (3-4) = 2 セグメント。
        assert_eq!(segs.len(), 2);
    }
}
