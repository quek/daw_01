// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `color_picker` ウィジェット — overlay popup でパレットスウォッチ + HSV (SV 矩形 + Hue
//! バー) を選べる汎用カラーピッカー (daw_01 #058)。
//!
//! 設計:
//! - 既存 `open_popup` / `popup_layer` / `close_popup` の上の薄いラッパ (`modal` widget と同 idiom)。
//! - **真のモーダル (capture_input=true、 #065 / daw_01 #087)**: panel が開いている間、 panel 外の全
//!   widget への pointer / keyboard 入力を遮断する。 これにより SV 矩形 / Hue バーのドラッグ press を
//!   背景の arrangement 等が先取りして下の clip を動かす事故を防ぐ。 panel 内 (`drawing_in_popup`) は
//!   un-mask されて通常動作し、 ESC は body 内で `take_shortcut` する (background keyboard は masking
//!   されるため)。 panel 外 click の dismiss は生 pointer で判定されるので従来どおり効く。
//! - **uncontrolled HSV state**: `current` 引数は popup を開いた瞬間の初期値としてのみ使い、
//!   open 中は内部 `ColorPickerState { hue, sat, val }` を source-of-truth にする。
//!   RGB ↔ HSV の往復は gray / black で hue が不定になり、 毎フレーム `current` から HSV を
//!   導出すると SV ドラッグで gray に寄せた瞬間に Hue バーが飛ぶ。 内部 HSV を保持することで
//!   これを防ぐ (text_input #059 の uncontrolled 化と同じ思想)。
//! - **live-apply**: スウォッチ click / SV・Hue ドラッグで `picked = Some(色)` を逐次返す。
//!   `dismissed` は popup 外 click / Esc のみ true (OK/Cancel ボタンは持たない — caller は
//!   `picked` を model に live 反映し、 `dismissed` で picker state を閉じる)。
//! - renderer に gradient プリミティブが無いため、 SV 矩形は **2 層の strip 合成**
//!   (横方向 = 白→hue の彩度 strip + 縦方向 = 透明→黒の alpha strip)、 Hue バーは縦 strip で
//!   近似する。

use std::hash::Hash;

use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::id::WidgetId;
use crate::theme::Palette;
use crate::ui::Ui;

/// `Ui::color_picker` の見た目スタイル。
#[derive(Clone, Copy, Debug)]
pub struct ColorPickerStyle {
    /// popup panel 背景。
    pub background: Color,
    pub border: Color,
    pub border_width: f32,
    pub radius: f32,
    /// panel 内側の余白 (px)。
    pub padding: f32,
    /// パレットスウォッチ 1 個の 1 辺 (px)。
    pub swatch_size: f32,
    /// スウォッチ間の間隔 (px)。
    pub swatch_gap: f32,
    /// パレットの 1 行あたりスウォッチ数。
    pub swatches_per_row: usize,
    /// SV 矩形 (彩度 × 明度) の 1 辺 (px)。
    pub sv_size: f32,
    /// Hue バーの幅 (px、 SV 矩形の右に縦に並ぶ)。
    pub hue_bar_w: f32,
    /// セクション間の間隔 (px)。
    pub gap: f32,
    /// 現在色プレビュー帯の高さ (px)。
    pub preview_h: f32,
    /// SV / Hue のセレクタ (リング / ライン) の色。
    pub selector: Color,
}

impl ColorPickerStyle {
    /// パレットから既定のカラーピッカースタイルを組む。panel 面と枠はクロームのトークン
    /// (`panel` / `border`)。
    ///
    /// セレクタだけは **極性固定の明インク** (`ink_on_dark`) を使う。SV 矩形 / Hue バーは
    /// HSV から生成される **テーマ非依存の可変背景** で、ライトテーマでも見た目は変わらない。
    /// ここにクローム面用の `text` を使うとライトで暗インクに反転し、SV 矩形の下半分 (黒) や
    /// 濃い hue の上でセレクタが消える。ダーク値は両者同値なので既存の見た目は不変。
    ///
    /// `Default` は持たない (r.md #48): テーマ色を読む `Default::default()` は隠れた
    /// グローバル依存になり、ライトテーマに追従しないため。caller は
    /// `ColorPickerStyle::from_palette(ui.palette())` で組む。
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            background: p.panel,
            border: p.border,
            border_width: 1.0,
            radius: 6.0,
            padding: 8.0,
            swatch_size: 18.0,
            swatch_gap: 4.0,
            swatches_per_row: 8,
            sv_size: 140.0,
            hue_bar_w: 16.0,
            gap: 8.0,
            preview_h: 18.0,
            selector: p.ink_on_dark,
        }
    }
}

/// `Ui::color_picker` の 1 フレーム分の結果。
#[derive(Clone, Copy, Debug, Default)]
pub struct ColorPickerResponse {
    /// スウォッチ click / SV・Hue ドラッグで色が選ばれたら `Some(新色)`。 連続ドラッグ中も
    /// 毎フレーム返る (caller は live 反映してよい)。
    pub picked: Option<Color>,
    /// popup 外 click / Esc で閉じる要求。 caller は自前の picker state を閉じる
    /// (= 次フレーム以降 `color_picker` を呼ばない)。
    pub dismissed: bool,
}

/// SV 矩形 / Hue バーのどちらをドラッグ中か。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HsvDrag {
    Sv,
    Hue,
}

/// color_picker の永続状態 (open 中の HSV source-of-truth + ドラッグ対象)。
#[derive(Debug, Default)]
pub(crate) struct ColorPickerState {
    open: bool,
    hue: f32,
    sat: f32,
    val: f32,
    drag: Option<HsvDrag>,
}

/// popup content の各サブ矩形 (content 原点 = panel 左上 + padding)。
struct PickerLayout {
    panel: Rect,
    sv: Rect,
    hue: Rect,
    preview: Rect,
    /// パレット領域の左上 (スウォッチ grid の原点)。 `n_swatches == 0` なら未使用。
    swatch_origin: (f32, f32),
}

/// `[0.0, 1.0]` の (h, s, v) → RGB。 CSS と同じ sextant 法。
#[allow(clippy::many_single_char_names)]
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> Color {
    let h = h.rem_euclid(1.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let h6 = h * 6.0;
    let x = c * (1.0 - (h6.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = if h6 < 1.0 {
        (c, x, 0.0)
    } else if h6 < 2.0 {
        (x, c, 0.0)
    } else if h6 < 3.0 {
        (0.0, c, x)
    } else if h6 < 4.0 {
        (0.0, x, c)
    } else if h6 < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = v - c;
    Color::rgb(r1 + m, g1 + m, b1 + m)
}

/// RGB → `[0.0, 1.0]` の (h, s, v)。 hue が不定 (gray / black) なら `h = 0`。
#[allow(clippy::many_single_char_names)]
fn rgb_to_hsv(c: Color) -> (f32, f32, f32) {
    let (r, g, b) = (c.r.clamp(0.0, 1.0), c.g.clamp(0.0, 1.0), c.b.clamp(0.0, 1.0));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let v = max;
    let s = if max <= 0.0 { 0.0 } else { d / max };
    let h = if d <= 0.0 {
        0.0
    } else if (max - r).abs() < f32::EPSILON {
        ((g - b) / d).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    ((h / 6.0).rem_euclid(1.0), s, v)
}

/// popup panel のサイズを style + パレット件数から計算する。
fn picker_size(n_swatches: usize, style: &ColorPickerStyle) -> (f32, f32) {
    let per_row = style.swatches_per_row.max(1);
    let palette_w = per_row as f32 * style.swatch_size + (per_row as f32 - 1.0) * style.swatch_gap;
    let sv_block_w = style.sv_size + style.gap + style.hue_bar_w;
    let content_w = palette_w.max(sv_block_w);

    let rows = if n_swatches == 0 {
        0
    } else {
        n_swatches.div_ceil(per_row)
    };
    let palette_h = if rows == 0 {
        0.0
    } else {
        rows as f32 * style.swatch_size + (rows as f32 - 1.0) * style.swatch_gap + style.gap
    };
    let content_h = palette_h + style.sv_size + style.gap + style.preview_h;

    (
        content_w + style.padding * 2.0,
        content_h + style.padding * 2.0,
    )
}

/// panel rect から content の各サブ矩形を割り出す。
fn layout(panel: Rect, n_swatches: usize, style: &ColorPickerStyle) -> PickerLayout {
    let cx = panel.x + style.padding;
    let mut cy = panel.y + style.padding;

    let per_row = style.swatches_per_row.max(1);
    let swatch_origin = (cx, cy);
    if n_swatches > 0 {
        let rows = n_swatches.div_ceil(per_row);
        let palette_h = rows as f32 * style.swatch_size + (rows as f32 - 1.0) * style.swatch_gap;
        cy += palette_h + style.gap;
    }

    let sv = Rect { x: cx, y: cy, w: style.sv_size, h: style.sv_size };
    let hue = Rect {
        x: cx + style.sv_size + style.gap,
        y: cy,
        w: style.hue_bar_w,
        h: style.sv_size,
    };
    cy += style.sv_size + style.gap;

    let content_w = panel.w - style.padding * 2.0;
    let preview = Rect { x: cx, y: cy, w: content_w, h: style.preview_h };

    PickerLayout { panel, sv, hue, preview, swatch_origin }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 汎用カラーピッカーを overlay popup として描画する。
    ///
    /// `color_picker` を呼んだフレームで popup が開く (まだ開いていなければ `current` を初期
    /// HSV に取り込む)。 `dismissed == true` を受け取ったら caller は即座に呼び出しを止めること
    /// (= picker state を閉じる)。 同フレームで `dismissed` を無視して呼び続けると再オープンする。
    ///
    /// - `anchor`: popup を配置する基準矩形 (右クリックされた行 / clip の rect 等)。 画面端では
    ///   `popup_rect_below_or_above` で内側に flip / clamp する。
    /// - `current`: popup を開いた瞬間の初期色 (= SV / Hue / プレビューの初期表示)。 open 中は
    ///   内部 HSV state が source-of-truth なので、 open 後に `current` が変わっても無視される
    ///   (= ユーザのドラッグ中の選択が外部 model 更新で飛ばない)。
    /// - `palette`: 表示するスウォッチ群 (空でも可、 その場合 HSV エリアのみ)。
    #[allow(clippy::too_many_lines)]
    pub fn color_picker(
        &mut self,
        id: impl Hash + Copy,
        anchor: Rect,
        current: Color,
        palette: &[Color],
        style: &ColorPickerStyle,
    ) -> ColorPickerResponse {
        let pid = ("color_picker", id);
        let state_wid: WidgetId = WidgetId::ROOT.child((b"color_picker_state", id));

        // panel rect を毎フレーム計算 (anchor / screen 変化に追従)。
        let (popup_w, popup_h) = picker_size(palette.len(), style);
        let panel = crate::popup::popup_rect_below_or_above(anchor, popup_w, popup_h, self.screen());

        // 初回 (popup 未 open) は current を HSV に取り込んで open する。
        let just_opening = {
            let st: &mut ColorPickerState = self.widget_state(state_wid);
            if st.open {
                false
            } else {
                let (h, s, v) = rgb_to_hsv(current);
                st.hue = h;
                st.sat = s;
                st.val = v;
                st.drag = None;
                st.open = true;
                true
            }
        };
        if just_opening {
            // daw_01 #087: capture_input = true の「真のモーダル」 (#065) で開く。 開いている間
            // panel 外の pointer / keyboard が background widget (arrangement 等) に届かず、
            // SV 矩形 / Hue バーのドラッグが下の clip を一切動かさない。 panel 外
            // click は従来どおり popup_layer の outside-click 検出で dismiss する (capture でも
            // close 判定は生 pointer で行う #065 仕様)。
            self.open_popup_inner(pid, panel, true, true, true);
        }
        self.update_popup_anchor(pid, panel);

        let was_open = self.is_popup_open(pid);
        let n_swatches = palette.len();
        let style_copy = *style;
        let mut picked: Option<Color> = None;
        // Esc close は popup_layer body 内 (`drawing_in_popup`) で拾う。 capturing modal 中は background
        // フェーズの keyboard が masking される (#065) ため、 body 外で `take_shortcut("escape")` を
        // 呼んでも効かない (= #065 の `ui.modal` が ESC を body 内処理に移したのと同じ理由)。
        let mut esc_close = false;

        self.popup_layer(pid, |ui| {
            if ui.take_shortcut("escape") {
                esc_close = true;
            }
            let lay = layout(panel, n_swatches, &style_copy);

            // ---- panel 背景 ----
            ui.push_rect(RectCommand {
                rect: lay.panel,
                fill: style_copy.background,
                border: style_copy.border,
                border_width: style_copy.border_width,
                radius: [style_copy.radius; 4],
                clip_rect: None,
            });

            let pointer = ui.pointer;
            let ppos = pointer.pos;

            // ---- 入力処理 (state を mutate しつつ picked を決定) ----
            {
                let st: &mut ColorPickerState = ui.widget_state(state_wid);

                // press: ドラッグ対象 / スウォッチ click を判定。
                if pointer.primary_just_pressed
                    && let Some((px, py)) = ppos
                {
                    if lay.sv.contains(px, py) {
                        st.drag = Some(HsvDrag::Sv);
                    } else if lay.hue.contains(px, py) {
                        st.drag = Some(HsvDrag::Hue);
                    } else {
                        st.drag = None;
                        // スウォッチ hit-test (palette click)。
                        if let Some(c) =
                            swatch_at(px, py, palette, lay.swatch_origin, &style_copy)
                        {
                            let (h, s, v) = rgb_to_hsv(c);
                            st.hue = h;
                            st.sat = s;
                            st.val = v;
                            picked = Some(c);
                        }
                    }
                }
                if pointer.primary_just_released {
                    st.drag = None;
                }

                // drag 継続: SV / Hue を pointer 位置から更新 (rect 外でも clamp)。
                if pointer.primary_pressed
                    && let (Some(kind), Some((px, py))) = (st.drag, ppos)
                {
                    match kind {
                        HsvDrag::Sv => {
                            st.sat = ((px - lay.sv.x) / lay.sv.w).clamp(0.0, 1.0);
                            st.val = (1.0 - (py - lay.sv.y) / lay.sv.h).clamp(0.0, 1.0);
                        }
                        HsvDrag::Hue => {
                            st.hue = ((py - lay.hue.y) / lay.hue.h).clamp(0.0, 1.0);
                        }
                    }
                    picked = Some(hsv_to_rgb(st.hue, st.sat, st.val));
                }
            }

            // ---- 描画 (state を read-only コピー) ----
            let (hue, sat, val) = {
                let st: &mut ColorPickerState = ui.widget_state(state_wid);
                (st.hue, st.sat, st.val)
            };

            draw_swatches(ui, palette, lay.swatch_origin, &style_copy);
            draw_sv_square(ui, lay.sv, hue, sat, val, &style_copy);
            draw_hue_bar(ui, lay.hue, hue, &style_copy);

            // 現在色プレビュー。
            ui.push_rect(RectCommand {
                rect: lay.preview,
                fill: hsv_to_rgb(hue, sat, val),
                border: style_copy.border,
                border_width: 1.0,
                radius: [3.0; 4],
                clip_rect: None,
            });
        });

        // body 内で Esc を拾っていたら close + dismissed (capturing modal の ESC 経路)。
        if esc_close {
            self.close_popup(pid);
            self.widget_state::<ColorPickerState>(state_wid).open = false;
            return ColorPickerResponse { picked: None, dismissed: true };
        }

        // popup_layer 内で outside-click により閉じられたら dismissed。
        let now_open = self.is_popup_open(pid);
        if was_open && !now_open {
            self.widget_state::<ColorPickerState>(state_wid).open = false;
            return ColorPickerResponse { picked: None, dismissed: true };
        }

        ColorPickerResponse { picked, dismissed: false }
    }
}

/// (px, py) がどのスウォッチ上か判定して色を返す。
fn swatch_at(
    px: f32,
    py: f32,
    palette: &[Color],
    origin: (f32, f32),
    style: &ColorPickerStyle,
) -> Option<Color> {
    let per_row = style.swatches_per_row.max(1);
    let step = style.swatch_size + style.swatch_gap;
    for (i, &c) in palette.iter().enumerate() {
        let col = i % per_row;
        let row = i / per_row;
        let x = origin.0 + col as f32 * step;
        let y = origin.1 + row as f32 * step;
        let r = Rect { x, y, w: style.swatch_size, h: style.swatch_size };
        if r.contains(px, py) {
            return Some(c);
        }
    }
    None
}

fn draw_swatches<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    palette: &[Color],
    origin: (f32, f32),
    style: &ColorPickerStyle,
) {
    let per_row = style.swatches_per_row.max(1);
    let step = style.swatch_size + style.swatch_gap;
    for (i, &c) in palette.iter().enumerate() {
        let col = i % per_row;
        let row = i / per_row;
        let x = origin.0 + col as f32 * step;
        let y = origin.1 + row as f32 * step;
        ui.push_rect(RectCommand {
            rect: Rect { x, y, w: style.swatch_size, h: style.swatch_size },
            fill: c,
            border: style.border,
            border_width: 1.0,
            radius: [3.0; 4],
            clip_rect: None,
        });
    }
}

/// SV 矩形を 2 層 strip で近似描画 + セレクタリング。
fn draw_sv_square<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    sv: Rect,
    hue: f32,
    sat: f32,
    val: f32,
    style: &ColorPickerStyle,
) {
    const N: usize = 24;
    let col_w = sv.w / N as f32;
    let row_h = sv.h / N as f32;

    // 横方向: 彩度 (左=白, 右=full hue) を value=1 で。
    for i in 0..N {
        let s = (i as f32 + 0.5) / N as f32;
        ui.push_rect(RectCommand {
            rect: Rect { x: sv.x + i as f32 * col_w, y: sv.y, w: col_w + 0.5, h: sv.h },
            fill: hsv_to_rgb(hue, s, 1.0),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: Some(sv),
        });
    }
    // 縦方向: 明度 (上=透明, 下=黒) を alpha で重ねる。
    for j in 0..N {
        let a = (j as f32 + 0.5) / N as f32;
        ui.push_rect(RectCommand {
            rect: Rect { x: sv.x, y: sv.y + j as f32 * row_h, w: sv.w, h: row_h + 0.5 },
            fill: Color::rgba(0.0, 0.0, 0.0, a),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: Some(sv),
        });
    }
    // 枠線。
    ui.push_rect(RectCommand {
        rect: sv,
        fill: Color::TRANSPARENT,
        border: style.border,
        border_width: 1.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    // セレクタリング (彩度 = x, 明度 = 上から下に減少)。
    let ring = 10.0;
    let sx = sv.x + sat * sv.w - ring * 0.5;
    let sy = sv.y + (1.0 - val) * sv.h - ring * 0.5;
    ui.push_rect(RectCommand {
        rect: Rect { x: sx, y: sy, w: ring, h: ring },
        fill: Color::TRANSPARENT,
        border: style.selector,
        border_width: 2.0,
        radius: [ring * 0.5; 4],
        clip_rect: None,
    });
}

/// Hue バーを縦 strip で近似描画 + セレクタライン。
fn draw_hue_bar<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    bar: Rect,
    hue: f32,
    style: &ColorPickerStyle,
) {
    const N: usize = 32;
    let seg_h = bar.h / N as f32;
    for j in 0..N {
        let h = (j as f32 + 0.5) / N as f32;
        ui.push_rect(RectCommand {
            rect: Rect { x: bar.x, y: bar.y + j as f32 * seg_h, w: bar.w, h: seg_h + 0.5 },
            fill: hsv_to_rgb(h, 1.0, 1.0),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: Some(bar),
        });
    }
    ui.push_rect(RectCommand {
        rect: bar,
        fill: Color::TRANSPARENT,
        border: style.border,
        border_width: 1.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    // セレクタライン (現在 hue の y)。
    let ly = bar.y + hue * bar.h - 1.5;
    ui.push_rect(RectCommand {
        rect: Rect { x: bar.x - 2.0, y: ly, w: bar.w + 4.0, h: 3.0 },
        fill: Color::TRANSPARENT,
        border: style.selector,
        border_width: 2.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
}

#[cfg(test)]
mod tests {
    use super::{hsv_to_rgb, rgb_to_hsv};
    use daw_ui_renderer::Color;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn hsv_rgb_round_trip_saturated_colors() {
        // 代表的な彩度の高い色は HSV↔RGB 往復で復元される。
        for c in [
            Color::rgb(1.0, 0.0, 0.0),
            Color::rgb(0.0, 1.0, 0.0),
            Color::rgb(0.0, 0.0, 1.0),
            Color::rgb(0.5, 0.25, 0.75),
            Color::rgb(0.9, 0.55, 0.2),
        ] {
            let (h, s, v) = rgb_to_hsv(c);
            let back = hsv_to_rgb(h, s, v);
            assert!(
                approx(back.r, c.r) && approx(back.g, c.g) && approx(back.b, c.b),
                "round trip mismatch: in=({},{},{}) out=({},{},{})",
                c.r, c.g, c.b, back.r, back.g, back.b
            );
        }
    }

    #[test]
    fn hsv_primary_hues() {
        // hue=0 → 赤, 1/3 → 緑, 2/3 → 青 (full sat/val)。
        let red = hsv_to_rgb(0.0, 1.0, 1.0);
        assert!(approx(red.r, 1.0) && approx(red.g, 0.0) && approx(red.b, 0.0));
        let green = hsv_to_rgb(1.0 / 3.0, 1.0, 1.0);
        assert!(approx(green.r, 0.0) && approx(green.g, 1.0) && approx(green.b, 0.0));
        let blue = hsv_to_rgb(2.0 / 3.0, 1.0, 1.0);
        assert!(approx(blue.r, 0.0) && approx(blue.g, 0.0) && approx(blue.b, 1.0));
    }

    #[test]
    fn gray_has_zero_saturation() {
        let (_, s, v) = rgb_to_hsv(Color::rgb(0.5, 0.5, 0.5));
        assert!(approx(s, 0.0), "gray の彩度は 0");
        assert!(approx(v, 0.5), "gray の明度は値どおり");
    }

    // -------- UiHost 経由の統合テスト --------

    use std::cell::Cell;

    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey, PhysicalSize};
    use daw_ui_renderer::{Rect, Scene};

    use super::{ColorPickerResponse, ColorPickerStyle};
    use crate::input::{FrameInput, PointerFrame};
    use crate::theme::Palette;
    use crate::ui::UiHost;

    const ANCHOR: Rect = Rect { x: 100.0, y: 100.0, w: 50.0, h: 20.0 };
    const CURRENT: Color = Color::rgb(0.5, 0.5, 0.5);

    /// frame 1 で popup を開き、 callback の戻り `ColorPickerResponse` を `out` に格納する helper。
    /// `frames[i]` の `FrameInput` を順に流す。
    fn run_picker(frames: Vec<FrameInput>, palette: Vec<Color>) -> ColorPickerResponse {
        let style = ColorPickerStyle::from_palette(&Palette::dark());
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let out: Cell<ColorPickerResponse> = Cell::new(ColorPickerResponse::default());
        for input in frames {
            host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
                let r = ui.color_picker("p", ANCHOR, CURRENT, &palette, &style);
                out.set(r);
            });
        }
        out.get()
    }

    fn press_at(x: f32, y: f32) -> FrameInput {
        FrameInput {
            pointer: PointerFrame {
                pos: Some((x, y)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        }
    }

    #[test]
    fn swatch_click_returns_picked_color() {
        let palette = vec![Color::rgb(0.9, 0.1, 0.1), Color::rgb(0.1, 0.9, 0.1)];
        // panel = {x:100, y:120}, swatch_origin = (108, 128), swatch0 center = (117, 137)。
        let r = run_picker(
            vec![FrameInput::default(), press_at(117.0, 137.0)],
            palette.clone(),
        );
        assert_eq!(r.picked, Some(palette[0]), "swatch0 click でその色が返る");
        assert!(!r.dismissed);
    }

    #[test]
    fn sv_square_press_returns_picked() {
        let palette = vec![Color::rgb(0.9, 0.1, 0.1), Color::rgb(0.1, 0.9, 0.1)];
        // palette 1 行 (18px) + gap(8) → sv = {x:108, y:154, w:140, h:140}、 中央 = (178, 224)。
        let r = run_picker(
            vec![FrameInput::default(), press_at(178.0, 224.0)],
            palette,
        );
        assert!(r.picked.is_some(), "SV 矩形 press で色が返る");
        assert!(!r.dismissed);
    }

    #[test]
    fn escape_dismisses_and_closes() {
        let esc = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Escape, repeat: false
        };
        let r = run_picker(
            vec![
                FrameInput::default(),
                FrameInput { keyboard: vec![esc], ..FrameInput::default() },
            ],
            vec![Color::rgb(0.9, 0.1, 0.1)],
        );
        assert!(r.dismissed, "Esc で dismissed");
        assert!(r.picked.is_none());
    }

    #[test]
    fn outside_click_dismisses() {
        // panel 外 (5, 5) を click → popup_layer が outside-click で閉じる → dismissed。
        let r = run_picker(
            vec![FrameInput::default(), press_at(5.0, 5.0)],
            vec![Color::rgb(0.9, 0.1, 0.1)],
        );
        assert!(r.dismissed, "popup 外 click で dismissed");
        assert!(r.picked.is_none());
    }

    #[test]
    fn open_picker_masks_background_pointer() {
        // daw_01 #087: capturing modal (#065) — picker open 中の 2 フレーム目、 background 描画
        // フェーズで `ui.pointer().pos` が masking される (= 背景 arrangement が SV/Hue drag の
        // press を先取りして下の clip を動かす事故を防ぐ)。
        let style = ColorPickerStyle::from_palette(&Palette::dark());
        let palette = vec![Color::rgb(0.9, 0.1, 0.1)];
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let bg_pos: Cell<Option<(f32, f32)>> = Cell::new(Some((1.0, 1.0)));

        // frame 1: open (open は描画途中で起きるので、 この frame はまだ capturing でない)。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.color_picker("p", ANCHOR, CURRENT, &palette, &style);
        });
        // frame 2: pointer を panel 外 (5, 5) に置く (press はしない = dismiss させない)。 picker は
        // 前 frame で capture_input=true で開いているため modal_capturing が frame 頭から true。
        let input = FrameInput {
            pointer: PointerFrame { pos: Some((5.0, 5.0)), ..PointerFrame::default() },
            ..FrameInput::default()
        };
        host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
            // popup_layer の外 (background 描画フェーズ) で pointer を読む。
            bg_pos.set(ui.pointer().pos);
            ui.color_picker("p", ANCHOR, CURRENT, &palette, &style);
        });
        assert_eq!(
            bg_pos.get(),
            None,
            "capturing modal 中の background pointer は masking される (#087)"
        );
    }
}
