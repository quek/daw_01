//! r.md #158 仕様 8: インスペクタのタイトル (スクロールしない 2 行、高さは `TITLE_H` で一定)。
//!
//! 1 行目 = 選んでいるもの (クリップ名 / 列名 / トラック名 / 「N クリップ」…) と、その色見本。
//! 2 行目 = 種類とトラック名 (小さく) と、トラックの色見本。どの色見本もクリックで色を選ぶ
//! (トラック色を変える操作はいつもここに残る)。中身は `AppData::inspector_title`。

use daw_ui_core::{Edit, Ui, WidgetId};
use daw_ui_renderer::Rect;

use super::layout::{
    PAD, SWATCH_HIT_PAD, TITLE_FONT, TITLE_H, TITLE_LINE_GAP, TITLE_SUB_FONT, TITLE_SUB_SWATCH, TITLE_SUB_SWATCH_GAP,
    TITLE_SWATCH, TITLE_SWATCH_GAP, TITLE_TOP,
};
use crate::app::AppData;
use crate::handler::inspector_title::{InspectorTitle, TitleSwatch};
use crate::view::track_color;

/// タイトル `title` を `area` (高さ `TITLE_H`) に描く。
pub(super) fn draw_title(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect, title: &InspectorTitle) {
    let p = &app.theme.core;
    let right = area.x + area.w - PAD;

    // ---- 1 行目: [色見本] 選んでいるもの ----
    let line1_y = area.y + TITLE_TOP;
    let mut x = area.x + PAD;
    if let Some(s) = title.main_swatch {
        let r = Rect { x, y: line1_y + (TITLE_FONT * 1.2 - TITLE_SWATCH) * 0.5, w: TITLE_SWATCH, h: TITLE_SWATCH };
        swatch(app, ui, "inspector_title_swatch", r, s);
        x += TITLE_SWATCH + TITLE_SWATCH_GAP;
    }
    ui.label_at_clipped(
        "inspector_title",
        &title.main,
        Rect { x, y: line1_y, w: (right - x).max(0.0), h: TITLE_FONT * 1.2 },
        TITLE_FONT,
        p.text,
    );

    // ---- 2 行目: 種類 · トラック名 [色見本] ----
    let line2_y = line1_y + TITLE_FONT * 1.2 + TITLE_LINE_GAP;
    let x = area.x + PAD;
    let reserve = if title.sub_swatch.is_some() { TITLE_SUB_SWATCH + TITLE_SUB_SWATCH_GAP } else { 0.0 };
    let avail = (right - x - reserve).max(0.0);
    // 文字の幅が要るのは色見本を文字のすぐ右に置くときだけ (見本が無ければ幅いっぱいに切って描けば同じ見た目)。
    let text_w = if title.sub_swatch.is_some() { ui.measure_text(&title.sub, TITLE_SUB_FONT).min(avail) } else { avail };
    ui.label_at_clipped(
        "inspector_title_sub",
        &title.sub,
        Rect { x, y: line2_y, w: text_w + 1.0, h: TITLE_SUB_FONT * 1.2 },
        TITLE_SUB_FONT,
        p.text_dim,
    );
    if let Some(s) = title.sub_swatch {
        let r = Rect {
            x: x + text_w + TITLE_SUB_SWATCH_GAP,
            y: line2_y + (TITLE_SUB_FONT * 1.2 - TITLE_SUB_SWATCH) * 0.5,
            w: TITLE_SUB_SWATCH,
            h: TITLE_SUB_SWATCH,
        };
        swatch(app, ui, "inspector_title_sub_swatch", r, s);
    }

    // タイトルとスクロールする本文の境目。
    ui.panel("inspector_title_rule", Rect { x: area.x, y: area.y + TITLE_H - 1.0, w: area.w, h: 1.0 }, p.border, 0.0);
}

/// 色見本 1 つ。見本の周りに数 px の当たり代を取り (10px の見本を狙わせない)、当たりは `primary_click` だけで
/// 取る — button を下に敷くと、当たり代のぶん button の枠が見本の外にはみ出して見える。popup (色の選択窓 /
/// dropdown の一覧) が開いている間は反応しない (`[[feedback_popup_click_leaks_to_background]]`)。
fn swatch(app: &AppData, ui: &mut Ui<'_, AppData>, id: &'static str, r: Rect, s: TitleSwatch) {
    let hit = Rect {
        x: r.x - SWATCH_HIT_PAD,
        y: r.y - SWATCH_HIT_PAD,
        w: r.w + SWATCH_HIT_PAD * 2.0,
        h: r.h + SWATCH_HIT_PAD * 2.0,
    };
    let inside = !ui.has_open_popups() && ui.pointer().pos.is_some_and(|(px, py)| hit.contains(px, py));
    let clicked = ui.primary_click(WidgetId::ROOT.child(("inspector_title_swatch_hit", id)), inside).clicked;
    let border = if inside && ui.hovers(hit) { app.theme.core.border_hover } else { app.theme.core.border };
    ui.panel_with_border(("fill", id), r, track_color::to_renderer(s.rgb), border, 1.0, 3.0);
    if clicked {
        let target = s.target;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.open_color_picker(target, r);
        }));
    }
}
