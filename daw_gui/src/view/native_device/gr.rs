//! GR (ゲインリダクション) メーター (§10.4)。Comp / Bus Comp の行ミニ・Par・Mixer 帯と、
//! master Limiter のセグメント表示が共有する。
//!
//! 値はすべて **正の減衰量 dB** (0 = 掛かっていない。`TransportState::native_gr.get(id)` /
//! `master_limiter_gr` がこの向きで持つ)。`active == false` (bypass) は形を変えずに薄くする (Q9) —
//! 横 / セグメントは溝と塗りの両方 (GR 0 の停止中も ON の行と見分けられる)、縦は呼び出し側が面を渡す
//! (EQ カーブの上に重ねるので面は不透明のまま) ので塗りだけ。

use std::hash::Hash;

use common::model::GR_METER_RANGE_DB;
use daw_ui_core::Ui;
use daw_ui_renderer::{Color, Rect, RectCommand};

use super::INACTIVE_ALPHA;
use crate::app::AppData;

/// master Limiter の GR セグメント数 (1 セグメント = 1 dB、Mixbus と同じ粒度)。
pub const LIMITER_GR_SEGMENTS: usize = 12;

/// 縦の GR メーター (上から下へ減衰量ぶん伸びる、レンジ `GR_METER_RANGE_DB`)。
/// EQ カーブの上に重ねる用途があるので、面 (`bg`) は必ず自分で塗る。
pub fn draw_gr_vertical(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    id: impl Hash,
    rect: Rect,
    gr_db: f32,
    active: bool,
    bg: Color,
) {
    ui.panel((b"native_gr_v", &id), rect, bg, 2.0);
    let frac = gr_frac(gr_db, GR_METER_RANGE_DB);
    if frac > 0.0 {
        fill(ui, Rect { h: rect.h * frac, ..rect }, gr_color(app, active), [2.0, 2.0, 0.0, 0.0]);
    }
}

/// 横の GR メーター (左から右へ伸びる、レンジ `range_db`)。
///
/// `value_font` が `Some` なら右端に [`gr_text`] を出し、その幅 (font × 2) だけバーを縮める。
/// バーは `rect` の高さいっぱいに描き、数値は `rect` の縦の中央に揃える (`rect` より背が高い
/// 文字でもはみ出して描く) — バーの太さは呼び出し側が `rect` で決める。
#[allow(clippy::too_many_arguments)]
pub fn draw_gr_horizontal(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    id: impl Hash,
    rect: Rect,
    gr_db: f32,
    active: bool,
    range_db: f32,
    value_font: Option<f32>,
) {
    let p = &app.theme.core;
    let value_w = value_font.map_or(0.0, |font| font * 2.0 + 2.0);
    let bar = Rect { w: (rect.w - value_w).max(1.0), ..rect };
    ui.panel((b"native_gr_h", &id), bar, slot_color(app, active), 2.0);
    let frac = gr_frac(gr_db, range_db);
    if frac > 0.0 {
        fill(ui, Rect { w: bar.w * frac, ..bar }, gr_color(app, active), [2.0; 4]);
    }
    if let Some(font) = value_font {
        let color = if active { p.text_dim } else { p.text_faint };
        ui.label_at(
            (b"native_gr_h_value", &id),
            &gr_text(gr_db),
            rect.x + rect.w - (value_w - 2.0),
            rect.y + (rect.h - font * 1.2) * 0.5,
            font,
            color,
        );
    }
}

/// セグメント式の GR メーター (1 セグメント = 1 dB、`segments` 個で頭打ち)。
pub fn draw_gr_segments(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    id: impl Hash,
    rect: Rect,
    gr_db: f32,
    active: bool,
    segments: usize,
) {
    ui.panel((b"native_gr_seg", &id), rect, slot_color(app, active), 2.0);
    let segments = segments.max(1);
    let lit = (gr_db.max(0.0) as usize).min(segments);
    let seg_w = (rect.w - 2.0) / segments as f32;
    let color = gr_color(app, active);
    for i in 0..lit {
        let seg = Rect { x: rect.x + 1.0 + seg_w * i as f32, y: rect.y + 1.0, w: seg_w - 1.0, h: rect.h - 2.0 };
        fill(ui, seg, color, [1.0; 4]);
    }
}

/// 減衰量の数値表記。常に負方向なので符号は書かず、数 dB の掛かり具合が読めるよう小数第 1 位まで。
#[must_use]
pub fn gr_text(gr_db: f32) -> String {
    format!("{:.1}", gr_db.max(0.0))
}

/// 減衰量 → レンジに対する割合 (0..=1)。非有限値は 0。
fn gr_frac(gr_db: f32, range_db: f32) -> f32 {
    if range_db > 0.0 { (gr_db.max(0.0) / range_db).min(1.0) } else { 0.0 }
}

fn gr_color(app: &AppData, active: bool) -> Color {
    inactive_dim(app.theme.daw.strip_gr, active)
}

/// 横 / セグメントの溝 (メーターの窪み)。OFF は下の面へ溶かして薄くする。
fn slot_color(app: &AppData, active: bool) -> Color {
    inactive_dim(app.theme.core.window_bg, active)
}

fn inactive_dim(c: Color, active: bool) -> Color {
    if active { c } else { c.with_alpha(c.a * INACTIVE_ALPHA) }
}

fn fill(ui: &mut Ui<'_, AppData>, rect: Rect, color: Color, radius: [f32; 4]) {
    ui.push_rect(RectCommand {
        rect,
        fill: color,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius,
        clip_rect: None,
    });
}
