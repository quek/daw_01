// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `time_ruler` + `bar_beat_grid` widget — DAW で頻出する時間軸 UI (M7 Phase 27)。
//!
//! piano_roll / arrangement で個別実装していた grid + ruler 描画を library 化。
//! `TimeMapping` (拍子・tempo・sample rate) と `ViewportState1D` (表示範囲) に依存。
//!
//! M14 Phase 63m (daw_01 #027): zoom 連動の自動間引きを追加。 1 bar の表示 px 幅が
//! `min_label_spacing_px` 未満になると label / bar tick の step を 2 のべき乗で skip
//! (`1, 2, 4, 8, 16, ...` bar) するため、 ズームアウト時も bar 番号が重ならない。
//! 1 beat 表示幅が `min_beat_tick_px` (ruler) / `min_beat_line_px` (grid) 未満なら
//! beat tick / beat 線を描画しない (= bar 線のみ)。 これは Reaper / Live / Cubase 等の
//! 業界標準動作。

use std::hash::Hash;

use common::time::{TimeDisplay, TimeMapping};
use daw_ui_core::id::WidgetId;
use daw_ui_core::scenegraph::hash_inputs;
use daw_ui_core::ui::Ui;
use daw_ui_core::viewport::ViewportState1D;
use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect};
use crate::theme::Palette;

const RULER_FONT: f32 = 11.0;
const RULER_LABEL_PAD_X: f32 = 3.0;

/// label_step が病的に大きくならないための上限 (= 2^20 bars ≒ 100 万 bar)。
const MAX_LABEL_STEP: i64 = 1 << 20;

/// `Ui::time_ruler` のスタイル設定。
#[derive(Debug, Clone, Copy)]
pub struct TimeRulerStyle {
    pub bg: Color,
    pub tick_color: Color,
    pub label_color: Color,
    pub bar_tick_height: f32,
    pub beat_tick_height: f32,
    /// label が重ならないための最小間隔 (px)。 1 bar の表示 px 幅が
    /// この値未満なら、 描画 step を 2 bar / 4 bar / 8 bar ... と 2 のべき乗で
    /// skip する。 0.0 以下なら間引き無し (常に全 bar に label)。
    /// default 60.0 (= 4 桁 bar 番号 + 余白程度)。
    pub min_label_spacing_px: f32,
    /// beat tick (label を持たない短い tick) を描画する最小 1 beat 表示幅 (px)。
    /// これ未満では beat tick を描かず bar tick のみ。 0.0 以下なら常に描画。
    /// default 4.0。
    pub min_beat_tick_px: f32,
}

impl TimeRulerStyle {
    /// パレットから既定スタイルを組み立てる (r.md #48)。
    ///
    /// `Default` を廃したのは、テーマ色を読む `default()` が「いまどのパレットで描いて
    /// いるか」 を知らない隠れたグローバル依存になり、ライトテーマに追従しない (= ruler
    /// の帯だけダークのまま残る) ため。 caller は `ui.palette()` / `app.theme.core` を渡す。
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            bg: p.header,
            tick_color: p.text_dim,
            label_color: p.text_dim,
            bar_tick_height: 12.0,
            beat_tick_height: 5.0,
            min_label_spacing_px: 60.0,
            min_beat_tick_px: 4.0,
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
    /// beat 縦線を描画する最小 1 beat 表示幅 (px)。 これ未満では beat 線を
    /// 描かず bar 線のみ。 0.0 以下なら常に描画。 default 4.0。
    pub min_beat_line_px: f32,
    /// (M14 Phase 124 / daw_01 #100) subdivision (3 段目) 線を描画する最小
    /// `px_per_interval` (= `px_per_beat * interval_beats`)。 これ未満の frame では
    /// subdivision を描かず bar + beat の 2 段に退避する。 0.0 以下なら常に描画。 default 6.0。
    pub min_sub_line_px: f32,
}

impl BarBeatGridStyle {
    /// パレットから既定スタイルを組み立てる (r.md #48)。理由は
    /// [`TimeRulerStyle::from_palette`] と同じ (テーマ非追従の隠れたグローバル依存を作らない)。
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            bar_color: p.grid_line_strong,
            beat_color: p.grid_line,
            bar_line_width: 1.0,
            beat_line_width: 1.0,
            min_beat_line_px: 4.0,
            min_sub_line_px: 6.0,
        }
    }
}

/// (M14 Phase 124 / daw_01 #100) `bar_beat_grid` の **3 段目** (subdivision) グリッド指定。
///
/// `interval_beats` は「1 拍の分割数」ではなく **線間隔そのもの (拍単位)**。 これにより直線
/// (`0.25` = 1/16)・三連 (`2.0/3.0` = 1/4T、 非整数 per-beat)・付点をすべて literal に表現できる。
/// subdivision 線は **小節原点からの倍数** `m * interval_beats` (m=1,2,…、 小節内) に打たれ、
/// 拍線・小節線 (整数拍) と一致する位置は skip される。 描画は bar / beat より背面・最も淡く
/// (push 順 subdivision → beat → bar)。 ズーム退避は [`BarBeatGridStyle::min_sub_line_px`]。
#[derive(Debug, Clone, Copy)]
pub struct SubGridSpec {
    /// subdivision 線の間隔 (拍単位、 `> 0.0`)。 例: `0.25` (1/16)、 `2.0/3.0` (1/4T)。
    pub interval_beats: f64,
    /// subdivision 線の色。 beat_color より淡くするのが通例。
    pub color: Color,
    /// subdivision 線の幅 (px)。
    pub line_width: f32,
}

/// `min_label_spacing_px` から 2 のべき乗 step (1, 2, 4, 8, ...) を求める。
/// `px_per_bar * step >= min_spacing_px` を満たす最小値を返す。
/// `px_per_bar <= 0.0` または `min_spacing_px <= 0.0` なら 1 を返す (= 間引き無し)。
/// NaN 入力は while loop の比較が false になるため自動的に step=1 を返す。
fn compute_label_step(px_per_bar: f32, min_spacing_px: f32) -> i64 {
    if px_per_bar <= 0.0 || min_spacing_px <= 0.0 {
        return 1;
    }
    let mut step: i64 = 1;
    while (px_per_bar * step as f32) < min_spacing_px {
        step = step.saturating_mul(2);
        if step >= MAX_LABEL_STEP {
            return MAX_LABEL_STEP;
        }
    }
    step
}

/// `Ui` に DAW の時間軸 widget (`time_ruler` / `bar_beat_grid`) を生やす拡張トレイト。
///
/// daw-ui core は DAW ドメインを持たない (arch 不変条件 #8) ため、これら音楽ドメインの
/// widget は core の `Ui` 本体ではなくアプリ側 (daw_gui) の拡張トレイトとして定義する。
/// heavy ブロック内からは `hctx.ui_mut().time_ruler(...)` の形で呼ぶ。
pub trait TimeGridExt {
    /// time_ruler widget。`rect` 内に拍 / 小節 / SMPTE label と tick を描画する。
    fn time_ruler(
        &mut self,
        id: impl Hash,
        rect: Rect,
        mapping: TimeMapping,
        viewport: ViewportState1D,
        style: TimeRulerStyle,
    );
    /// bar/beat grid widget。`rect` 内に縦線で拍 / 小節 (+ 任意の subdivision) を描画する。
    fn bar_beat_grid(
        &mut self,
        id: impl Hash,
        rect: Rect,
        mapping: TimeMapping,
        viewport: ViewportState1D,
        style: BarBeatGridStyle,
        sub: Option<SubGridSpec>,
    );
}

impl<'a, M: ?Sized + 'static> TimeGridExt for Ui<'a, M> {
    /// time_ruler widget。`rect` 内に拍 / 小節 / SMPTE label と tick を描画。
    /// X 軸は `viewport` (sample 単位) を参照。表示モードは `mapping.display`。
    ///
    /// M14 Phase 63m (daw_01 #027): zoom 連動間引き。 1 bar 表示幅 < `min_label_spacing_px`
    /// で label / bar tick を 2 のべき乗 step で skip、 1 beat 表示幅 < `min_beat_tick_px` で
    /// beat tick を非表示。
    fn time_ruler(
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
            // M14 Phase 63m: 間引き threshold が変わると描画される label/tick 数も変わるので hash に含める。
            (style.min_label_spacing_px.to_bits(), style.min_beat_tick_px.to_bits()),
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
            let beats_per_bar =
                f64::from(mapping.time_sig.0) * 4.0 / f64::from(mapping.time_sig.1);
            let beat_index_start = (viewport.view_start / spb).floor() as i64;
            let beat_index_end = (view_end / spb).ceil() as i64;

            // M14 Phase 63m: 1 bar / 1 beat の表示 px 幅から label step と beat tick on/off を決定。
            let view_len_safe = viewport.view_len.max(1e-9);
            let px_per_bar = ((mapping.samples_per_bar() / view_len_safe) as f32) * rect.w;
            let px_per_beat = ((spb / view_len_safe) as f32) * rect.w;
            let label_step = compute_label_step(px_per_bar, style.min_label_spacing_px);
            let draw_beat_ticks =
                style.min_beat_tick_px <= 0.0 || px_per_beat >= style.min_beat_tick_px;

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
                if is_bar {
                    // bar tick は label_step の倍数の bar だけに描画 (label の根元には必ず tick)。
                    let bar_idx = ((bi as f64) / beats_per_bar).round() as i64;
                    if label_step > 1 && bar_idx.rem_euclid(label_step) != 0 {
                        continue;
                    }
                    tick_segs.push(LineSegment {
                        a: [x, bar_y_start],
                        b: [x, bar_y],
                        color: style.tick_color,
                    });
                } else if draw_beat_ticks {
                    tick_segs.push(LineSegment {
                        a: [x, beat_y_start],
                        b: [x, bar_y],
                        color: style.tick_color,
                    });
                }
            }
            if !tick_segs.is_empty() {
                ui.push_lines(LineBatch {
                    segments: tick_segs.into(),
                    line_width_px: 1.0,
                    clip_rect: Some(rect),
                });
            }

            // bar label (BarBeat は小節番号のみ "1", "2", ...; その他は mapping.format で
            // SMPTE / 秒文字列をそのまま使う)。 label_step の倍数 bar のみ描画。
            let bar_index_start = (viewport.view_start / mapping.samples_per_bar()).floor() as i64;
            let bar_index_end = (view_end / mapping.samples_per_bar()).ceil() as i64;
            for bar in bar_index_start..=bar_index_end {
                if label_step > 1 && bar.rem_euclid(label_step) != 0 {
                    continue;
                }
                let s = (bar as f64) * mapping.samples_per_bar();
                if s < viewport.view_start || s > view_end {
                    continue;
                }
                let local_x = viewport.unit_to_px(s, rect.w);
                let x = rect.x + local_x;
                let label = match mapping.display {
                    TimeDisplay::BarBeat => {
                        let (bar_num, _beat) = mapping.samples_to_bar_beat(s);
                        format!("{bar_num}")
                    }
                    _ => mapping.format(s),
                };
                ui.push_text(GlyphArea {
                    text: label.into(),
                    left: x + RULER_LABEL_PAD_X,
                    top: rect.y + 2.0,
                    font_size: RULER_FONT,
                    line_height: RULER_FONT * 1.2,
                    color: style.label_color,
                    clip_rect: Some(rect),
                    ..GlyphArea::default()
                });
            }
        });
    }

    /// bar/beat grid widget。`rect` 内に縦線で拍/小節を描画 (piano_roll / arrangement の grid 置換)。
    ///
    /// M14 Phase 63m (daw_01 #027): 1 beat 表示幅 < `min_beat_line_px` で beat 線を非表示
    /// (= bar 線のみ残る)。 zoom 小での beat 線密集を防ぎ描画コストも削減。
    ///
    /// M14 Phase 124 (daw_01 #100): `sub: Option<SubGridSpec>` で **3 段目** (subdivision) 線を
    /// 追加できる。 `Some` のとき小節原点からの `interval_beats` 倍数 (拍線・小節線と一致しない位置)
    /// に淡い縦線を打つ。 push 順は subdivision → beat → bar (subdivision が最背面・最も淡い)。
    /// `px_per_interval < style.min_sub_line_px` の frame では subdivision を描かず 2 段に退避する。
    /// 既存 caller は `None` を渡す (非破壊)。
    fn bar_beat_grid(
        &mut self,
        id: impl Hash,
        rect: Rect,
        mapping: TimeMapping,
        viewport: ViewportState1D,
        style: BarBeatGridStyle,
        sub: Option<SubGridSpec>,
    ) {
        let wid = WidgetId::ROOT.child((b"bar_beat_grid", &id));
        // M14 Phase 124 (#100): sub が変わると描画 line 数が変わるので hash に含める
        // (= スナップ変更で subdivision 間隔が変わったとき確実に再描画される)。
        let sub_key = sub.map_or((0_u8, 0_u64, 0_u32, 0_u32, 0_u32, 0_u32, 0_u32), |s| {
            (
                1,
                s.interval_beats.to_bits(),
                s.color.r.to_bits(),
                s.color.g.to_bits(),
                s.color.b.to_bits(),
                s.color.a.to_bits(),
                s.line_width.to_bits(),
            )
        });
        let input_hash = hash_inputs((
            b"bar_beat_grid",
            (rect.x.to_bits(), rect.y.to_bits(), rect.w.to_bits(), rect.h.to_bits()),
            (mapping.sample_rate.to_bits(), mapping.tempo_bpm.to_bits(), mapping.time_sig),
            (viewport.view_start.to_bits(), viewport.view_len.to_bits()),
            // M14 Phase 63m: beat 線 on/off threshold が変わると描画 line 数も変わるので hash に含める。
            style.min_beat_line_px.to_bits(),
            (style.min_sub_line_px.to_bits(), sub_key),
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            let view_end = viewport.view_start + viewport.view_len;
            let spb = mapping.samples_per_beat();
            let beats_per_bar =
                f64::from(mapping.time_sig.0) * 4.0 / f64::from(mapping.time_sig.1);
            let beat_index_start = (viewport.view_start / spb).floor() as i64;
            let beat_index_end = (view_end / spb).ceil() as i64;

            // M14 Phase 63m: 1 beat 表示幅 < min_beat_line_px なら beat 線を skip。
            let view_len_safe = viewport.view_len.max(1e-9);
            let px_per_beat = ((spb / view_len_safe) as f32) * rect.w;
            let draw_beat_lines =
                style.min_beat_line_px <= 0.0 || px_per_beat >= style.min_beat_line_px;

            // M14 Phase 124 (#100): subdivision (3 段目)。 ズーム退避 + px 変換は helper に委譲。
            // beat / bar より背面 = 先に push する。
            let sub_segs = sub.map_or_else(Vec::new, |s| {
                build_sub_segments(
                    rect,
                    viewport,
                    spb,
                    beats_per_bar,
                    beat_index_start,
                    beat_index_end,
                    px_per_beat,
                    style.min_sub_line_px,
                    s,
                )
            });

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
                if is_bar {
                    bar_segs.push(LineSegment {
                        a: [x, rect.y],
                        b: [x, rect.y + rect.h],
                        color: style.bar_color,
                    });
                } else if draw_beat_lines {
                    beat_segs.push(LineSegment {
                        a: [x, rect.y],
                        b: [x, rect.y + rect.h],
                        color: style.beat_color,
                    });
                }
            }
            // push 順 = subdivision (最背面) → beat → bar (最前面)。
            if !sub_segs.is_empty() {
                let line_width_px = sub.map_or(1.0, |s| s.line_width);
                ui.push_lines(LineBatch {
                    segments: sub_segs.into(),
                    line_width_px,
                    clip_rect: Some(rect),
                });
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

/// (M14 Phase 124 / daw_01 #100) subdivision 線分を `rect` 内に構築する。 ズーム退避
/// (`px_per_interval < min_sub_line_px` or `interval <= 0`) なら空 Vec を返す (= 2 段に退避)。
/// view 範囲外 / rect 範囲外の線はクリップする。 位置算出は [`subdivision_beats`] に委譲。
#[allow(clippy::too_many_arguments)]
fn build_sub_segments(
    rect: Rect,
    viewport: ViewportState1D,
    spb: f64,
    beats_per_bar: f64,
    beat_index_start: i64,
    beat_index_end: i64,
    px_per_beat: f32,
    min_sub_line_px: f32,
    sub: SubGridSpec,
) -> Vec<LineSegment> {
    let mut sub_segs = Vec::new();
    let px_per_interval = px_per_beat * (sub.interval_beats as f32);
    let interval_ok = sub.interval_beats > 0.0
        && px_per_interval > 0.0
        && (min_sub_line_px <= 0.0 || px_per_interval >= min_sub_line_px);
    if !interval_ok {
        return sub_segs;
    }
    let view_end = viewport.view_start + viewport.view_len;
    let bar_idx_start = (beat_index_start as f64 / beats_per_bar).floor() as i64;
    let bar_idx_end = (beat_index_end as f64 / beats_per_bar).ceil() as i64;
    for b in subdivision_beats(beats_per_bar, sub.interval_beats, bar_idx_start, bar_idx_end) {
        let s = b * spb;
        if s < viewport.view_start || s > view_end {
            continue;
        }
        let x = rect.x + viewport.unit_to_px(s, rect.w);
        if x < rect.x || x > rect.x + rect.w {
            continue;
        }
        sub_segs.push(LineSegment { a: [x, rect.y], b: [x, rect.y + rect.h], color: sub.color });
    }
    sub_segs
}

/// (M14 Phase 124 / daw_01 #100) subdivision 線の **beat 位置** を計算する純粋ヘルパー。
///
/// 可視小節範囲 `[bar_idx_start, bar_idx_end]` の各小節原点 `bar_i * beats_per_bar` から
/// `m * interval_beats` (m=1,2,…、 `< beats_per_bar`) の位置に線を打つ。 ただし **整数拍**
/// (= 拍線・小節線と一致) は skip する。 `interval_beats` は線間隔そのもの (拍単位) なので、
/// 直線 (0.25)・三連 (2/3、 非整数 per-beat)・付点をすべて literal に表現できる。
/// ズーム退避 (`px_per_interval` 判定) は呼び出し側で行う。
fn subdivision_beats(
    beats_per_bar: f64,
    interval_beats: f64,
    bar_idx_start: i64,
    bar_idx_end: i64,
) -> Vec<f64> {
    let mut out = Vec::new();
    if interval_beats <= 0.0 || beats_per_bar <= 0.0 {
        return out;
    }
    for bar_i in bar_idx_start..=bar_idx_end {
        let bar_beat0 = bar_i as f64 * beats_per_bar;
        let mut m = 1.0_f64;
        loop {
            let off = m * interval_beats;
            if off >= beats_per_bar - 1e-6 {
                break;
            }
            let b = bar_beat0 + off;
            // 整数拍 (= 拍線/小節線) と一致は skip。
            if (b - b.round()).abs() >= 1e-6 {
                out.push(b);
            }
            m += 1.0;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // compute_label_step (pure helper) の単体検証
    // ============================================================

    #[test]
    fn compute_label_step_no_thinning_when_threshold_disabled() {
        // min_spacing_px <= 0 → 常に 1 (間引き無し)
        assert_eq!(compute_label_step(2.0, 0.0), 1);
        assert_eq!(compute_label_step(2.0, -1.0), 1);
    }

    #[test]
    fn compute_label_step_no_thinning_when_px_per_bar_invalid() {
        assert_eq!(compute_label_step(0.0, 60.0), 1);
        assert_eq!(compute_label_step(-5.0, 60.0), 1);
        assert_eq!(compute_label_step(f32::NAN, 60.0), 1);
    }

    #[test]
    fn compute_label_step_returns_1_when_bar_already_wide() {
        // 1 bar = 100px、 threshold 60px → step 1 で十分。
        assert_eq!(compute_label_step(100.0, 60.0), 1);
    }

    #[test]
    fn compute_label_step_doubles_until_threshold_met() {
        // 1 bar = 30px、 threshold 60px → step 2 (= 60px)。
        assert_eq!(compute_label_step(30.0, 60.0), 2);
        // 1 bar = 10px、 threshold 60px → step 8 (= 80px、 step 4 = 40px は不足)。
        assert_eq!(compute_label_step(10.0, 60.0), 8);
        // 1 bar = 1px、 threshold 60px → step 64 (= 64px)。
        assert_eq!(compute_label_step(1.0, 60.0), 64);
        // 1 bar = 0.5px、 threshold 60px → step 128 (= 64px)。
        assert_eq!(compute_label_step(0.5, 60.0), 128);
    }

    #[test]
    fn compute_label_step_caps_at_max() {
        // 1 bar = 1e-9px、 threshold 60px → MAX_LABEL_STEP で clamp。
        assert_eq!(compute_label_step(1e-9, 60.0), MAX_LABEL_STEP);
    }

    // ============================================================
    // time_ruler / bar_beat_grid 統合 (frame レベル) — UiHost を組み立てて
    // Scene の glyph / line を直接観察。
    // ============================================================

    use daw_ui_core::input::FrameInput;
    use daw_ui_core::ui::UiHost;
    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::Scene;

    struct TestModel;

    fn run_frame<F>(host: &mut UiHost<TestModel>, model: &mut TestModel, f: F) -> Scene
    where
        F: for<'a> FnOnce(&'a TestModel, &mut Ui<'a, TestModel>),
    {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        host.frame(model, &mut scene, screen, FrameInput::default(), f);
        scene
    }

    /// `TimeMapping` (4/4, 120 BPM, 48k) を使い、 `view_len_beats` 拍を `rect.w = 800px` に
    /// 表示したときの ruler を render して glyph 数 (= label 数) を返す。
    fn ruler_label_count(view_len_beats: f64, style: TimeRulerStyle) -> usize {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel;
        let mapping = TimeMapping {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            time_sig: (4, 4),
            display: TimeDisplay::BarBeat,
        };
        let spb = mapping.samples_per_beat();
        let viewport = ViewportState1D::new(0.0, view_len_beats * spb);
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 24.0 };
        let scene = run_frame(&mut host, &mut model, |_, ui| {
            ui.time_ruler("ruler", rect, mapping, viewport, style);
        });
        scene.iter_glyphs().count()
    }

    /// `bar_beat_grid` の bar / beat / subdivision 線を色で分けて数える。
    fn grid_segment_counts(
        view_len_beats: f64,
        style: BarBeatGridStyle,
        sub: Option<SubGridSpec>,
    ) -> (usize, usize, usize) {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel;
        let mapping = TimeMapping {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            time_sig: (4, 4),
            display: TimeDisplay::BarBeat,
        };
        let spb = mapping.samples_per_beat();
        let viewport = ViewportState1D::new(0.0, view_len_beats * spb);
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 200.0 };
        let scene = run_frame(&mut host, &mut model, |_, ui| {
            ui.bar_beat_grid("grid", rect, mapping, viewport, style, sub);
        });
        let mut bars = 0usize;
        let mut beats = 0usize;
        let mut subs = 0usize;
        for batch in scene.iter_lines() {
            for seg in batch.segments.iter() {
                if seg.color == style.bar_color {
                    bars += 1;
                } else if seg.color == style.beat_color {
                    beats += 1;
                } else if sub.is_some_and(|s| seg.color == s.color) {
                    subs += 1;
                }
            }
        }
        (bars, beats, subs)
    }

    #[test]
    fn ruler_no_thinning_when_bars_wide_enough() {
        // 4 bar (16 beats) を 800px → 1 bar = 200px、 threshold 60px → step 1 (間引き無し)。
        // bar 0..=4 = 5 個の bar boundary、 すべて label。
        let style = TimeRulerStyle::from_palette(&Palette::dark());
        assert_eq!(ruler_label_count(16.0, style), 5);
    }

    #[test]
    fn ruler_thins_labels_on_zoom_out() {
        let style = TimeRulerStyle::from_palette(&Palette::dark());
        // 800 bar (3200 beats) を 800px → 1 bar = 1px、 threshold 60px → step 64。
        // bar idx 0,64,128,...,768 が出る → ceil(800/64)+1 = 14 個。
        // ただし view_end の ceil で bar_index_end = 800、 0..=800 を step 64 で iter:
        //   0, 64, 128, 192, 256, 320, 384, 448, 512, 576, 640, 704, 768 = 13 個
        // (bar 800 は s = 800*samples_per_bar = view_end ちょうど、 通常通る)
        let labels_zoomed_out = ruler_label_count(3200.0, style);
        assert!(
            (12..=14).contains(&labels_zoomed_out),
            "ズームアウト時は step 64 で間引かれて ~13 個: got {labels_zoomed_out}",
        );

        // 8000 bar (= さらに 10 倍 zoom out) → 1 bar = 0.1px → step 1024。
        // ceil(8000 / 1024) ≈ 8 個前後。
        let labels_more_out = ruler_label_count(32000.0, style);
        assert!(
            labels_more_out < labels_zoomed_out,
            "さらに zoom out すると label がさらに減る: {labels_more_out} < {labels_zoomed_out}",
        );
        assert!(labels_more_out > 0, "ただし完全に消えはしない");
    }

    #[test]
    fn ruler_label_count_bounded_by_threshold() {
        // 任意の zoom で label 数 ≦ ceil(rect.w / min_label_spacing_px) + 1 を満たす。
        // 800px / 60px = 13.3 → 上限 ≈ 14 個 (bar_index_end の +1 含む)。
        let style = TimeRulerStyle::from_palette(&Palette::dark());
        for &beats in &[8.0_f64, 100.0, 1000.0, 10_000.0, 100_000.0] {
            let count = ruler_label_count(beats, style);
            let bound = (800.0_f32 / style.min_label_spacing_px).ceil() as usize + 2;
            assert!(
                count <= bound,
                "len={beats} beat の label 数は threshold で bounded: got {count}, bound {bound}",
            );
            assert!(count >= 1, "len={beats} で最低 1 つは label が出る");
        }
    }

    #[test]
    fn ruler_thinning_disabled_with_zero_threshold() {
        let style = TimeRulerStyle {
            min_label_spacing_px: 0.0,
            ..TimeRulerStyle::from_palette(&Palette::dark())
        };
        // 800 bar → 間引き無効なら全 bar に label。 0..=800 = 801 個。
        let count = ruler_label_count(3200.0, style);
        assert_eq!(count, 801, "min_label_spacing_px=0 で全 bar に label");
    }

    #[test]
    fn ruler_min_beat_tick_disables_beat_ticks() {
        // 検証方針: tick の長さ (= b.y - a.y) が `bar_tick_height` か `beat_tick_height` の
        // どちらかを観察 → beat tick (短い tick) が含まれるかを直接確認。
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel;
        let mapping = TimeMapping {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            time_sig: (4, 4),
            display: TimeDisplay::BarBeat,
        };
        let spb = mapping.samples_per_beat();
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 24.0 };
        let style = TimeRulerStyle::from_palette(&Palette::dark());

        let count_short_ticks = |scene: &Scene| -> usize {
            scene
                .iter_lines()
                .flat_map(|b| b.segments.iter())
                .filter(|seg| {
                    let len = (seg.b[1] - seg.a[1]).abs();
                    // bar tick = 12px、 beat tick = 5px。 中間値で分離。
                    (len - style.beat_tick_height).abs() < 0.1
                })
                .count()
        };

        // close zoom: 1 bar (4 beats) を 800px → 1 beat = 200px > 4px → beat tick 描画。
        let viewport_close = ViewportState1D::new(0.0, 4.0 * spb);
        let scene_close = run_frame(&mut host, &mut model, |_, ui| {
            ui.time_ruler("r", rect, mapping, viewport_close, style);
        });
        let beat_ticks_close = count_short_ticks(&scene_close);
        assert!(beat_ticks_close >= 3, "近 zoom では beat tick 描画: got {beat_ticks_close}");

        // far zoom: 200 bar を 800px → 1 beat = 1px < 4px → beat tick OFF。
        let mut host2: UiHost<TestModel> = UiHost::no_redraw();
        let mut model2 = TestModel;
        let viewport_far = ViewportState1D::new(0.0, 200.0 * 4.0 * spb);
        let scene_far = run_frame(&mut host2, &mut model2, |_, ui| {
            ui.time_ruler("r2", rect, mapping, viewport_far, style);
        });
        let beat_ticks_far = count_short_ticks(&scene_far);
        assert_eq!(beat_ticks_far, 0, "遠 zoom では beat tick 0: got {beat_ticks_far}");
    }

    #[test]
    fn grid_no_beat_lines_on_extreme_zoom_out() {
        let style = BarBeatGridStyle::from_palette(&Palette::dark());
        // 1 bar = 800px → 1 beat = 200px → beat 線 描画。
        let (bars_close, beats_close, _) = grid_segment_counts(4.0, style, None);
        assert!(bars_close >= 1);
        assert!(beats_close >= 3, "1 bar ズーム時は beat 線も描画: got {beats_close}");

        // 200 bar = 800px → 1 beat = 1px (< 4px threshold) → beat 線 OFF。
        let (bars_far, beats_far, _) = grid_segment_counts(800.0, style, None);
        assert_eq!(beats_far, 0, "zoom out で beat 線消える: got {beats_far}");
        assert!(bars_far >= 1, "ただし bar 線は残る: got {bars_far}");
    }

    #[test]
    fn grid_threshold_zero_keeps_beat_lines_always() {
        let style = BarBeatGridStyle {
            min_beat_line_px: 0.0,
            ..BarBeatGridStyle::from_palette(&Palette::dark())
        };
        // 200 bar = 800 beats、 1 beat = 1px。 threshold 0 で常に描画 → beat 線 600 本程度。
        let (_bars, beats, _) = grid_segment_counts(800.0, style, None);
        assert!(beats > 100, "min_beat_line_px=0 で zoom out でも beat 線描画: got {beats}");
    }

    // ============================================================
    // (M14 Phase 124 / daw_01 #100) subdivision (3 段目) grid
    // ============================================================

    /// subdivision_beats (pure helper): 小節原点からの倍数、 整数拍 skip。
    #[test]
    fn subdivision_beats_straight_sixteenth() {
        // 4/4、 interval 0.25 (1/16)。 1 bar (4 拍) 内に 0.25 刻みで打つが整数拍は skip。
        // 0.25,0.5,0.75,(1.0 skip),1.25,1.5,1.75,(2.0 skip),... = 拍ごとに 3 本 × 4 拍 = 12 本。
        let subs = subdivision_beats(4.0, 0.25, 0, 0);
        assert_eq!(subs.len(), 12, "1 bar に 12 本: got {subs:?}");
        // どれも整数拍ではない (拍線・小節線と非重複)。
        for b in &subs {
            assert!((b - b.round()).abs() >= 1e-6, "{b} は整数拍 (拍線と重複)");
        }
    }

    /// 1/4T (= 2/3 拍間隔、 非整数 per-beat) が正しく描画される回帰。
    #[test]
    fn subdivision_beats_quarter_triplet_non_integer() {
        // interval 2/3。 1 bar 内: 2/3,4/3,(2.0 skip),8/3,10/3,(4.0 = 次小節 → < 範囲外) 。
        // = 4 本 (2.0 は拍線と一致して skip)。
        let subs = subdivision_beats(4.0, 2.0 / 3.0, 0, 0);
        assert_eq!(subs.len(), 4, "1/4T は 1 bar に 4 本 (2.0 拍は skip): got {subs:?}");
        // 2.0 拍 (整数拍) が含まれない。
        assert!(!subs.iter().any(|b| (b - 2.0).abs() < 1e-6), "2.0 拍は拍線と一致 → skip");
    }

    /// frame レベル: subdivision 線が beat 線と別色で描かれ、 拍線位置と重複しない。
    #[test]
    fn grid_draws_subdivision_lines() {
        let style = BarBeatGridStyle::from_palette(&Palette::dark());
        let sub = SubGridSpec {
            interval_beats: 0.25,
            color: Color::rgba(0.5, 0.5, 0.5, 0.5), // bar/beat と別色
            line_width: 1.0,
        };
        // 1 bar (4 拍) を 800px → 1 beat = 200px、 px_per_interval = 50px >= 6px → 描画。
        let (bars, beats, subs) = grid_segment_counts(4.0, style, Some(sub));
        assert!(bars >= 1, "bar 線あり");
        assert!(beats >= 3, "beat 線あり");
        assert_eq!(subs, 12, "subdivision 12 本 (拍内 3 本 × 4 拍): got {subs}");
    }

    /// ズーム退避: px_per_interval < min_sub_line_px で subdivision 0 本、 bar/beat は残る。
    #[test]
    fn grid_subdivision_retreats_on_zoom_out() {
        let style = BarBeatGridStyle::from_palette(&Palette::dark()); // min_sub_line_px = 6.0
        let sub = SubGridSpec {
            interval_beats: 0.25,
            color: Color::rgba(0.5, 0.5, 0.5, 0.5),
            line_width: 1.0,
        };
        // 100 bar (400 拍) を 800px → 1 beat = 2px、 px_per_interval = 0.5px < 6px → subdivision OFF。
        let (bars, _beats, subs) = grid_segment_counts(400.0, style, Some(sub));
        assert_eq!(subs, 0, "ズーム退避で subdivision 0 本: got {subs}");
        assert!(bars >= 1, "bar 線は残る (2 段に退避): got {bars}");
    }

    #[test]
    fn ruler_label_step_is_power_of_two() {
        // 任意の zoom で label_step が常に 2 のべき乗 (or 1) になることを compute helper で確認。
        for &(px_per_bar, threshold) in &[
            (3.0_f32, 60.0_f32),
            (7.0, 60.0),
            (12.5, 60.0),
            (29.0, 60.0),
            (61.0, 60.0),
        ] {
            let step = compute_label_step(px_per_bar, threshold);
            assert!(
                step == 1 || (step > 0 && (step & (step - 1)) == 0),
                "step は 2 のべき乗 (or 1): px_per_bar={px_per_bar}, threshold={threshold}, step={step}",
            );
        }
    }
}
