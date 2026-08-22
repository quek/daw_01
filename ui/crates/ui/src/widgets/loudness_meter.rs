// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `loudness_meter` widget — EBU Tech 3341 の LU 目盛りバー (daw_01 r.md #50)。
//!
//! 測定 (K-weighting / ゲート / LRA) はオーディオ側の責務。この widget は
//! 「目標値を 0 LU とした相対軸」に Short-term (バー) と Momentary (細線)、
//! そして目標線を描くだけ。
//!
//! 目標を 0 LU に置くのは Tech 3341 §2.7 の EBU スケールそのままで、
//! 絶対 (LUFS) 表示に切り替えても**目標線の位置は動かない**という規格要求も
//! この構造で自然に満たされる (軸は常に相対、数字だけが絶対になる)。

use std::hash::Hash;

use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect, RectCommand};

use crate::theme::Palette;
use crate::ui::Ui;

const LABEL_FONT_PX: f32 = 9.0;
/// バーの上下に確保する余白 (px)。
///
/// 目盛りラベルは**目盛り位置を中心**に置くので、上端 / 下端の目盛りをそのまま
/// rect の縁に置くとラベルの上半分 / 下半分が rect の外へ出て clip される
/// (level_meter の `SCALE_VPAD` と同じ理由・同じ流儀)。
const VPAD: f32 = LABEL_FONT_PX * 0.5 + 2.0;
/// 目盛りを引く LU 値 (目標 = 0 LU からの相対)。
const TICKS_LU: &[f32] = &[9.0, 6.0, 3.0, 0.0, -3.0, -6.0, -9.0, -12.0, -18.0, -24.0, -36.0];

#[derive(Debug, Clone, Copy)]
pub struct LoudnessMeterStyle {
    pub bg: Color,
    pub border: Color,
    /// Short-term バーの色 (目標以下)。
    pub bar: Color,
    /// Short-term バーの色 (目標超過)。
    pub bar_over: Color,
    /// Momentary の細線。
    pub momentary: Color,
    /// 目標線。
    pub target: Color,
    pub tick: Color,
    pub label: Color,
    /// 表示範囲 (下端 LU, 上端 LU)。
    pub range_lu: (f32, f32),
    /// 目盛りの数字を描くか。
    pub show_labels: bool,
}

impl LoudnessMeterStyle {
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            bg: p.inset_bg,
            border: p.border,
            bar: p.meter_green,
            bar_over: p.meter_orange,
            momentary: p.text,
            target: p.accent,
            tick: p.text_dim.with_alpha(0.9),
            label: p.text_dim.with_alpha(0.9),
            range_lu: (-18.0, 9.0),
            show_labels: true,
        }
    }
}

/// LU → rect 内の y 座標 (上が大きい値)。
fn lu_to_y(rect: Rect, lu: f32, range: (f32, f32)) -> f32 {
    let (lo, hi) = range;
    let t = ((lu - lo) / (hi - lo)).clamp(0.0, 1.0);
    rect.y + rect.h * (1.0 - t)
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// ラウドネスバー。`short_term_lu` / `momentary_lu` は**目標値からの相対 LU**
    /// (無音は `f32::NEG_INFINITY`)。
    pub fn loudness_meter(
        &mut self,
        id: impl Hash,
        rect: Rect,
        short_term_lu: f32,
        momentary_lu: f32,
        style: &LoudnessMeterStyle,
    ) {
        let _ = id;
        if rect.w < 6.0 || rect.h < 8.0 {
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
        let label_w = if style.show_labels {
            (LABEL_FONT_PX * 2.4).min(rect.w * 0.55)
        } else {
            0.0
        };
        let bar = Rect {
            x: rect.x + 1.0,
            y: rect.y + VPAD,
            w: (rect.w - 2.0 - label_w).max(2.0),
            h: (rect.h - VPAD * 2.0).max(2.0),
        };

        // Short-term バー (下端から現在値まで)。
        if short_term_lu.is_finite() {
            let y = lu_to_y(bar, short_term_lu, style.range_lu);
            let h = bar.y + bar.h - y;
            if h > 0.0 {
                self.push_rect(RectCommand {
                    rect: Rect { x: bar.x, y, w: bar.w, h },
                    fill: if short_term_lu > 0.0 { style.bar_over } else { style.bar },
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: Some(bar),
                });
            }
        }

        // 目盛り + 目標線 + Momentary の順に線をまとめて 1 バッチで出す。
        let mut segs: Vec<LineSegment> = Vec::new();
        for lu in TICKS_LU {
            if *lu < style.range_lu.0 || *lu > style.range_lu.1 {
                continue;
            }
            let y = lu_to_y(bar, *lu, style.range_lu).round();
            let is_target = lu.abs() < 1e-6;
            segs.push(LineSegment {
                a: [bar.x, y],
                b: [bar.x + bar.w, y],
                color: if is_target { style.target } else { style.tick },
            });
        }
        if momentary_lu.is_finite() {
            let y = lu_to_y(bar, momentary_lu, style.range_lu).round();
            segs.push(LineSegment {
                a: [bar.x, y],
                b: [bar.x + bar.w, y],
                color: style.momentary,
            });
        }
        self.push_lines(LineBatch {
            segments: segs.into(),
            line_width_px: 1.0,
            clip_rect: Some(rect),
        });

        if !style.show_labels || label_w <= 0.0 {
            return;
        }
        for lu in TICKS_LU {
            if *lu < style.range_lu.0 || *lu > style.range_lu.1 {
                continue;
            }
            let y = lu_to_y(bar, *lu, style.range_lu);
            let text = if *lu > 0.0 {
                format!("+{lu:.0}")
            } else {
                format!("{lu:.0}")
            };
            self.push_text(GlyphArea {
                text: text.into(),
                left: bar.x + bar.w + 2.0,
                top: y - LABEL_FONT_PX * 0.5 - 1.0,
                font_size: LABEL_FONT_PX,
                line_height: LABEL_FONT_PX + 2.0,
                color: if lu.abs() < 1e-6 { style.target } else { style.label },
                clip_rect: Some(rect),
                ..GlyphArea::default()
            });
        }
    }
}
