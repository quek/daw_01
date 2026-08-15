//! `goniometer` / `correlation_meter` widget — ステレオの広がりと位相 (daw_01 r.md #50)。
//!
//! ゴニオは Lissajous を -45° 回した標準の向き (`x = (R-L)/√2`, `y = (L+R)/√2`) で、
//! **縦線 = モノ / 横線 = 逆相**。座標変換はオーディオ側で済ませて、この widget は
//! 正規化座標の点列を受け取るだけ。
//!
//! 残光 (persistence) は widget 内の点リングで表現する。オフスクリーン面へ
//! 毎フレーム黒を重ねる方式 (x42) と等価な指数減衰 `persist^age` を、
//! `persist^N < 1/255` になる `N` 点で打ち切って描く — 打ち切り点の輝度は
//! 1/255 未満、つまり 8bit の表示上まったく差が出ない。

use std::collections::VecDeque;
use std::hash::Hash;

use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect, RectCommand};

use crate::id::WidgetId;
use crate::theme::Palette;
use crate::ui::Ui;

/// 残光リングが保持する点数の上限。
const MAX_TRAIL_POINTS: usize = 12_000;
/// 前の点との距離² がこれ未満なら描かない (x42 と同じ間引き)。
const MIN_SEGMENT_PX2: f32 = 2.0;
/// グラティキュール円の分割角 [度]。
const CIRCLE_STEP_DEG: f32 = 6.0;

const LABEL_FONT_PX: f32 = 9.0;

#[derive(Debug, Clone, Copy)]
pub struct GoniometerStyle {
    pub bg: Color,
    pub border: Color,
    pub grid: Color,
    /// 軌跡の色 (alpha は残光で変調する)。
    pub trace: Color,
    pub label: Color,
    /// 1 フレームあたりの残光減衰率 (0.5..0.99)。
    pub persistence: f32,
    /// 表示倍率。1.0 で「片チャンネルフルスケール = 半径いっぱい」。
    pub gain: f32,
}

impl GoniometerStyle {
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            bg: p.inset_bg,
            border: p.border,
            grid: p.grid_line,
            trace: p.meter_green,
            label: p.text_dim.with_alpha(0.9),
            persistence: 0.90,
            gain: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CorrelationStyle {
    pub bg: Color,
    pub border: Color,
    /// +1 側 (同相 = 安全)。
    pub positive: Color,
    /// 0 付近 (無相関)。
    pub neutral: Color,
    /// 負 (逆相 = モノ互換性なし)。
    pub negative: Color,
    /// 直近の最小 / 最大を示す細線。
    pub range: Color,
    /// -1 / ±0.5 / +1 の目盛り線。
    pub tick: Color,
    pub label: Color,
}

impl CorrelationStyle {
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            bg: p.inset_bg,
            border: p.border,
            positive: p.meter_green,
            neutral: p.meter_yellow,
            negative: p.meter_red,
            range: p.text_dim,
            tick: p.grid_line,
            label: p.text_dim.with_alpha(0.9),
        }
    }
}

#[derive(Debug, Default)]
struct GonioState {
    points: VecDeque<[f32; 2]>,
    /// 最後に取り込んだバッチの通し番号 (`u64::MAX` = 未取り込み)。
    last_seq: u64,
    seen: bool,
    /// 直近バッチの点数 (残光の age 換算に使う)。
    last_batch: usize,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// ゴニオメーター。`points` はこのバッチで新たに得た正規化座標
    /// (`x` 右が正、`y` 上が正)。過去の点は widget 内のリングが保持する。
    ///
    /// `seq` は **バッチの通し番号**。immediate-mode なので、同じスナップショットで
    /// 複数フレーム描かれることがある (ポインタ移動などテレメトリ以外の再描画)。
    /// `seq` が前回と同じフレームでは取り込みをスキップし、同じ点列を二重に
    /// 積んで残光の時間軸が縮むのを防ぐ。
    pub fn goniometer(
        &mut self,
        id: impl Hash,
        rect: Rect,
        points: &[[f32; 2]],
        seq: u64,
        style: &GoniometerStyle,
    ) {
        if rect.w < 8.0 || rect.h < 8.0 {
            return;
        }
        let wid = WidgetId::ROOT.child((b"goniometer", &id));
        // 残光が見えなくなる長さで打ち切る (persist^n < 1/255)。
        let persist = style.persistence.clamp(0.5, 0.995);
        let visible_frames = ((1.0 / 255.0_f32).ln() / persist.ln()).ceil().max(1.0) as usize;
        let cap = (points.len().max(1) * visible_frames).min(MAX_TRAIL_POINTS);
        let (trail, per_batch) = {
            let state: &mut GonioState = self.widget_state(wid);
            if !state.seen || state.last_seq != seq {
                state.seen = true;
                state.last_seq = seq;
                state.last_batch = points.len();
                state.points.extend(points.iter().copied());
                while state.points.len() > cap {
                    state.points.pop_front();
                }
            }
            (
                state.points.iter().copied().collect::<Vec<_>>(),
                state.last_batch,
            )
        };

        self.push_rect(RectCommand {
            rect,
            fill: style.bg,
            border: style.border,
            border_width: 1.0,
            radius: [2.0; 4],
            clip_rect: None,
        });
        let size = rect.w.min(rect.h) - 4.0;
        let cx = rect.x + rect.w * 0.5;
        let cy = rect.y + rect.h * 0.5;
        let radius = (size * 0.5).max(1.0);

        self.draw_gonio_graticule(rect, cx, cy, radius, style);

        if trail.len() < 2 {
            return;
        }
        // 新しい点ほど不透明。age は「末尾からの距離 ÷ 1 バッチの点数」。
        let per_frame = per_batch.max(1) as f32;
        let n = trail.len();
        let mut segs: Vec<LineSegment> = Vec::with_capacity(n);
        let scale = radius * style.gain;
        let mut prev: Option<[f32; 2]> = None;
        for (i, p) in trail.iter().enumerate() {
            let sx = cx + p[0] * scale;
            let sy = cy - p[1] * scale;
            let cur = [sx, sy];
            if let Some(q) = prev {
                let dx = cur[0] - q[0];
                let dy = cur[1] - q[1];
                if dx * dx + dy * dy >= MIN_SEGMENT_PX2 {
                    let age = (n - 1 - i) as f32 / per_frame;
                    let alpha = style.trace.a * persist.powf(age);
                    if alpha > 1.0 / 255.0 {
                        segs.push(LineSegment {
                            a: q,
                            b: cur,
                            color: style.trace.with_alpha(alpha),
                        });
                    }
                    prev = Some(cur);
                }
            } else {
                prev = Some(cur);
            }
        }
        if !segs.is_empty() {
            self.push_lines(LineBatch {
                segments: segs.into(),
                line_width_px: 1.0,
                clip_rect: Some(rect),
            });
        }
    }

    fn draw_gonio_graticule(
        &mut self,
        rect: Rect,
        cx: f32,
        cy: f32,
        radius: f32,
        style: &GoniometerStyle,
    ) {
        let mut segs: Vec<LineSegment> = Vec::new();
        // 円 (フルスケール) — 2° 刻みの polygon 近似 (knob の push_arc と同じ流儀)。
        let steps = (360.0 / CIRCLE_STEP_DEG) as usize;
        for i in 0..steps {
            let a0 = (i as f32) * CIRCLE_STEP_DEG.to_radians();
            let a1 = ((i + 1) as f32) * CIRCLE_STEP_DEG.to_radians();
            segs.push(LineSegment {
                a: [cx + radius * a0.cos(), cy + radius * a0.sin()],
                b: [cx + radius * a1.cos(), cy + radius * a1.sin()],
                color: style.grid,
            });
        }
        // 縦 (モノ) / 横 (逆相) / L・R 対角。
        segs.push(LineSegment {
            a: [cx, cy - radius],
            b: [cx, cy + radius],
            color: style.grid,
        });
        segs.push(LineSegment {
            a: [cx - radius, cy],
            b: [cx + radius, cy],
            color: style.grid,
        });
        let d = radius * std::f32::consts::FRAC_1_SQRT_2;
        segs.push(LineSegment {
            a: [cx - d, cy - d],
            b: [cx + d, cy + d],
            color: style.grid,
        });
        segs.push(LineSegment {
            a: [cx + d, cy - d],
            b: [cx - d, cy + d],
            color: style.grid,
        });
        self.push_lines(LineBatch {
            segments: segs.into(),
            line_width_px: 1.0,
            clip_rect: Some(rect),
        });
        // L / R の方向ラベル (左だけの信号は左上、右だけは右上へ伸びる)。
        for (text, x) in [("L", cx - d - LABEL_FONT_PX), ("R", cx + d + 2.0)] {
            self.push_text(GlyphArea {
                text: text.into(),
                left: x,
                top: cy - d - LABEL_FONT_PX - 1.0,
                font_size: LABEL_FONT_PX,
                line_height: LABEL_FONT_PX + 2.0,
                color: style.label,
                clip_rect: Some(rect),
                ..GlyphArea::default()
            });
        }
    }

    /// 位相相関メーター (-1 .. +1 の横バー)。`min` / `max` は直近の観測レンジ
    /// (WaveLab の赤 2 本線と同じ用途)。
    pub fn correlation_meter(
        &mut self,
        id: impl Hash,
        rect: Rect,
        value: f32,
        min: f32,
        max: f32,
        style: &CorrelationStyle,
    ) {
        let _ = id;
        if rect.w < 8.0 || rect.h < 4.0 {
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
        let center = inner.x + inner.w * 0.5;
        let to_x = |v: f32| center + v.clamp(-1.0, 1.0) * inner.w * 0.5;

        // 中心 (相関 0) から現在値まで塗る。色は Logic / WaveLab の慣習
        // (正 = 緑、0 付近 = 黄、負 = 赤)。
        let v = value.clamp(-1.0, 1.0);
        let color = if v < 0.0 {
            style.negative
        } else if v < 0.3 {
            style.neutral
        } else {
            style.positive
        };
        let x = to_x(v);
        let (bx, bw) = if x >= center { (center, x - center) } else { (x, center - x) };
        if bw > 0.5 {
            self.push_rect(RectCommand {
                rect: Rect { x: bx, y: inner.y, w: bw, h: inner.h },
                fill: color,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: Some(inner),
            });
        }
        // 目盛り (-1 / -0.5 / 0 / +0.5 / +1) → 観測レンジ の順に線をまとめる。
        //
        // 目盛りが無いと「バーが右端まで来ている」が **+1.00 (= 完全モノ) なのか
        // 壊れているのか** を読み手が区別できない。モノ素材では実際に +1.00 に
        // 達するので、端がどこかを描くことが必須。
        let mut segs = Vec::with_capacity(8);
        for v in [-1.0_f32, -0.5, 0.5, 1.0] {
            let x = to_x(v).clamp(inner.x, inner.x + inner.w - 1.0).round();
            segs.push(LineSegment {
                a: [x, inner.y],
                b: [x, inner.y + inner.h],
                color: style.tick,
            });
        }
        segs.push(LineSegment {
            a: [center.round(), inner.y],
            b: [center.round(), inner.y + inner.h],
            color: style.label,
        });
        if min <= max {
            for v in [min, max] {
                let x = to_x(v);
                segs.push(LineSegment {
                    a: [x, inner.y],
                    b: [x, inner.y + inner.h],
                    color: style.range,
                });
            }
        }
        self.push_lines(LineBatch {
            segments: segs.into(),
            line_width_px: 1.0,
            clip_rect: Some(inner),
        });

        // 幅に余裕があるときだけ端と中心のラベルを出す。
        if rect.w >= 120.0 {
            for (text, x, align_right) in [
                ("-1", inner.x + 2.0, false),
                ("0", center + 2.0, false),
                ("+1", inner.x + inner.w - 2.0, true),
            ] {
                let w = LABEL_FONT_PX * 0.62 * text.chars().count() as f32;
                self.push_text(GlyphArea {
                    text: text.into(),
                    left: if align_right { x - w } else { x },
                    top: inner.y + (inner.h - LABEL_FONT_PX) * 0.5 - 1.0,
                    font_size: LABEL_FONT_PX,
                    line_height: LABEL_FONT_PX + 2.0,
                    color: style.label,
                    clip_rect: Some(inner),
                    ..GlyphArea::default()
                });
            }
        }
    }
}
