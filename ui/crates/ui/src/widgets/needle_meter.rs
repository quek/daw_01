//! `needle_meter` widget — アナログ針式のメーター。
//!
//! 円弧の目盛りの上を針が振れる、VU / GR メーターの見た目。値の大小を **形**で
//! 読ませたいところに使う (数値やバーより「どれくらいの速さで動いているか」が分かる)。
//!
//! ドメイン知識は持たない: 値のレンジと目盛りラベルは caller が渡す。
//! 弾道 (針の慣性) も caller が値側で作るのが基本で、この widget は与えられた値を
//! そのまま指す — 「表示のなめらかさ」を widget が勝手に足すと、測定値と画面が
//! ずれるため (メーターは測定器であって演出ではない)。

use std::hash::Hash;
use std::sync::Arc;

use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect, RectCommand};

use crate::theme::Palette;
use crate::ui::Ui;

/// 目盛り円弧の左端 / 右端の角度 (12 時から時計回り、rad)。
/// 実機の VU メーターと同じく上を向いた扇形で、振れ幅は約 ±60° (計 120°)。
/// 狭いと針の動きが小さく読めないので、横長の枠を使い切る角度にしてある。
const ARC_START: f32 = -1.05;
/// [`ARC_START`] の対。
const ARC_END: f32 = 1.05;
/// 円弧を折れ線で近似するときの角度刻み (rad)。
const ARC_STEP: f32 = 0.03;
/// 目盛りラベルの font size (px)。
const LABEL_FONT_PX: f32 = 9.0;
/// 単位ラベル (中央下) の font size (px)。
const UNIT_FONT_PX: f32 = 8.0;

#[derive(Debug, Clone, Copy)]
pub struct NeedleMeterStyle {
    /// 文字盤の地の色。
    pub bg: Color,
    pub border: Color,
    /// 目盛りの弧と目盛り線。
    pub scale: Color,
    /// 目盛りの数字と単位ラベル。
    pub label: Color,
    /// 針。
    pub needle: Color,
    /// 針の軸 (中心の小円)。
    pub pivot: Color,
}

impl NeedleMeterStyle {
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            bg: p.inset_bg,
            border: p.border,
            scale: p.grid_line,
            label: p.text_dim,
            needle: p.text,
            pivot: p.border,
        }
    }
}

/// 文字盤の目盛り (値のレンジ / 目盛りとその数字 / 単位ラベル)。
///
/// メーターの「何を測っているか」を 1 つに束ねたもの。caller が持つので widget は
/// ドメインを知らない。
#[derive(Debug, Clone, Copy)]
pub struct NeedleScale<'s> {
    /// 目盛りの下端・上端。針は範囲外で端に止まる。
    pub range: (f32, f32),
    /// 目盛りを打つ値と、その数字ラベル。空なら目盛り線なし。
    pub ticks: &'s [(f32, &'s str)],
    /// 文字盤中央下の単位ラベル (例 `"dB"`)。空文字で省略。
    pub unit: &'s str,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 針式メーターを描く。`value` が指す値、`scale` が文字盤。
    ///
    /// 入力は受け取らない (メーターは読むだけのもの)。
    pub fn needle_meter(
        &mut self,
        id: impl Hash,
        rect: Rect,
        value: f32,
        scale: NeedleScale<'_>,
        style: &NeedleMeterStyle,
    ) {
        let NeedleScale { range, ticks, unit } = scale;
        if rect.w < 12.0 || rect.h < 10.0 {
            return;
        }
        // 入力も内部状態も持たないので widget id は要らない (描画だけ)。
        let _ = &id;

        self.push_rect(RectCommand {
            rect,
            fill: style.bg,
            border: style.border,
            border_width: 1.0,
            radius: [2.0; 4],
            clip_rect: None,
        });

        // 針の軸は文字盤の下端ぎりぎり。半径は「弧の両端が横に収まる」 と
        // 「弧の頂点が上端に収まる」 の小さい方 = 枠を使い切る最大 (余白は 2〜3px)。
        let cx = rect.x + rect.w * 0.5;
        let cy = rect.y + rect.h - 3.0;
        let r_fit_w = (rect.w * 0.5 - 2.0) / ARC_END.sin();
        let r_fit_h = rect.h - 6.0;
        let r = r_fit_w.min(r_fit_h).max(4.0);
        let arc_r = r * 0.94;

        let frac = |v: f32| {
            let (lo, hi) = range;
            if (hi - lo).abs() < f32::EPSILON {
                0.0
            } else {
                ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
            }
        };
        let angle = |v: f32| ARC_START + (ARC_END - ARC_START) * frac(v);
        let point = |a: f32, radius: f32| [cx + a.sin() * radius, cy - a.cos() * radius];

        // ---- 目盛りの弧 ----
        let mut segs: Vec<LineSegment> = Vec::new();
        let mut a = ARC_START;
        let mut prev = point(a, arc_r);
        while a < ARC_END {
            a = (a + ARC_STEP).min(ARC_END);
            let p = point(a, arc_r);
            segs.push(LineSegment { a: prev, b: p, color: style.scale });
            prev = p;
        }
        // ---- 目盛り線 ----
        for (v, _) in ticks {
            let a = angle(*v);
            segs.push(LineSegment {
                a: point(a, arc_r),
                b: point(a, arc_r * 0.86),
                color: style.scale,
            });
        }
        self.push_lines(LineBatch {
            segments: Arc::from(segs),
            line_width_px: 1.0,
            clip_rect: Some(rect),
        });

        // ---- 目盛りの数字 ----
        for (v, text) in ticks {
            if text.is_empty() {
                continue;
            }
            let a = angle(*v);
            let p = point(a, arc_r * 0.70);
            let w = self.measure_text(text, LABEL_FONT_PX);
            self.push_text(GlyphArea {
                text: Arc::from(*text),
                left: p[0] - w * 0.5,
                top: p[1] - LABEL_FONT_PX * 0.6,
                font_size: LABEL_FONT_PX,
                line_height: LABEL_FONT_PX * 1.2,
                color: style.label,
                clip_rect: Some(rect),
                ..GlyphArea::default()
            });
        }

        // ---- 単位ラベル ----
        if !unit.is_empty() {
            let w = self.measure_text(unit, UNIT_FONT_PX);
            self.push_text(GlyphArea {
                text: Arc::from(unit),
                left: cx - w * 0.5,
                top: cy - r * 0.34,
                font_size: UNIT_FONT_PX,
                line_height: UNIT_FONT_PX * 1.2,
                color: style.label,
                clip_rect: Some(rect),
                ..GlyphArea::default()
            });
        }

        // ---- 針 + 軸 ----
        let a = angle(value);
        self.push_lines(LineBatch {
            segments: Arc::from(vec![LineSegment {
                a: [cx, cy],
                b: point(a, arc_r * 0.97),
                color: style.needle,
            }]),
            line_width_px: 1.5,
            clip_rect: Some(rect),
        });
        let pivot_r = (r * 0.10).max(1.5);
        self.push_rect(RectCommand {
            rect: Rect {
                x: cx - pivot_r,
                y: cy - pivot_r,
                w: pivot_r * 2.0,
                h: pivot_r * 2.0,
            },
            fill: style.pivot,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [pivot_r; 4],
            clip_rect: None,
        });
    }
}
