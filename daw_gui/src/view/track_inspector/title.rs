//! r.md #158 仕様 8: インスペクタのタイトル (スクロールしない 2 行、高さは `TITLE_H` で一定)。
//!
//! 1 行目 = 選んでいるもの (クリップ名 / 列名 / トラック名 / 「N クリップ」…) と、その色見本。
//! 2 行目 = 種類とトラック名 (小さく) と、トラックの色見本。どの色見本もクリックで色を選ぶ
//! (トラック色を変える操作はいつもここに残る)。中身は `AppData::inspector_title`。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use super::layout::{PAD, TITLE_FONT, TITLE_H, TITLE_SUB_FONT, TITLE_SUB_SWATCH, TITLE_SWATCH};
use crate::app::AppData;
use crate::handler::inspector_title::TitleSwatch;
use crate::view::track_color;

/// タイトルを `area` (高さ `TITLE_H`) に描く。
pub(super) fn draw_title(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let p = &app.theme.core;
    let title = app.inspector_title();
    let right = area.x + area.w - PAD;

    // ---- 1 行目: [色見本] 選んでいるもの ----
    let line1_y = area.y + 10.0;
    let mut x = area.x + PAD;
    if let Some(s) = title.main_swatch {
        let r = Rect { x, y: line1_y + (TITLE_FONT * 1.2 - TITLE_SWATCH) * 0.5, w: TITLE_SWATCH, h: TITLE_SWATCH };
        swatch(app, ui, "inspector_title_swatch", r, s);
        x += TITLE_SWATCH + 8.0;
    }
    ui.label_at_clipped(
        "inspector_title",
        &title.main,
        Rect { x, y: line1_y, w: (right - x).max(0.0), h: TITLE_FONT * 1.2 },
        TITLE_FONT,
        p.text,
    );

    // ---- 2 行目: 種類 · トラック名 [色見本] ----
    let line2_y = line1_y + TITLE_FONT * 1.2 + 4.0;
    let x = area.x + PAD;
    let reserve = if title.sub_swatch.is_some() { TITLE_SUB_SWATCH + 6.0 } else { 0.0 };
    let text_w = ui.measure_text(&title.sub, TITLE_SUB_FONT).min((right - x - reserve).max(0.0));
    ui.label_at_clipped(
        "inspector_title_sub",
        &title.sub,
        Rect { x, y: line2_y, w: text_w + 1.0, h: TITLE_SUB_FONT * 1.2 },
        TITLE_SUB_FONT,
        p.text_dim,
    );
    if let Some(s) = title.sub_swatch {
        let r = Rect {
            x: x + text_w + 6.0,
            y: line2_y + (TITLE_SUB_FONT * 1.2 - TITLE_SUB_SWATCH) * 0.5,
            w: TITLE_SUB_SWATCH,
            h: TITLE_SUB_SWATCH,
        };
        swatch(app, ui, "inspector_title_sub_swatch", r, s);
    }

    // タイトルとスクロールする本文の境目。
    ui.panel("inspector_title_rule", Rect { x: area.x, y: area.y + TITLE_H - 1.0, w: area.w, h: 1.0 }, p.border, 0.0);
}

/// 色見本 1 つ。click 判定を先に取り、その上に色を塗る (button の既定描画は隠れる)。
fn swatch(app: &AppData, ui: &mut Ui<'_, AppData>, id: &'static str, r: Rect, s: TitleSwatch) {
    // 見本の周りに数 px の当たり代を取る (10px の見本を狙わせない)。
    let hit = Rect { x: r.x - 3.0, y: r.y - 3.0, w: r.w + 6.0, h: r.h + 6.0 };
    let clicked = ui.button_at_clicked(id, "", hit);
    ui.panel(("fill_bg", id), hit, app.theme.core.panel, 0.0);
    ui.panel_with_border(("fill", id), r, track_color::to_renderer(s.rgb), app.theme.core.border, 1.0, 3.0);
    if clicked {
        let target = s.target;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.open_color_picker(target, r);
        }));
    }
}
