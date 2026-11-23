//! r.md #158: インスペクタの「帯」(`docs/plan_rmd_158_inspector.md` §2.2 / 仕様 1・4・7)。
//!
//! 最上位のまとまりごとに見出しの帯を置き、見出しのクリックで開閉する (Logic / Cubase / Studio One 型)。
//! 見出しには開閉マーク・名前・対象 (右寄せ) を出す。開閉で動くのは見出しより下だけで、見出し自身は
//! 動かない (末尾までスクロールした状態の床は `InspectorScrollFloor`)。帯の中の「詳細」は、めったに
//! 使わない欄を最初は畳んでおく行。どちらも開閉は見方の都合 (`InspectorEvent`、Song を書かない)。
//!
//! 既存の部品だけで描く (`panel` / `label_at_clipped` / `primary_click` / `disclosure_glyph`)。
//! 見出しのクリックは見出しの上に置く子 (モジュレーションの「+」) より **先に** 判定する —
//! press の所有者は後勝ちなので、子の press は子が取る (`ui/crates/ui/src/click.rs`)。

use daw_ui_core::{Edit, Ui, WidgetId};
use daw_ui_renderer::Rect;

use super::layout::{
    BAND_ACTIONS_GAP, BAND_BODY_BOTTOM, BAND_BODY_TOP, BAND_H, BAND_TARGET_GAP, DETAIL_GAP, DETAIL_ROW_H,
    DISCLOSURE_FONT, DISCLOSURE_W, HEAD_FONT, PAD, SUB_HEAD_FONT, SUB_HEAD_GAP, SUB_HEAD_H, TARGET_FONT,
};
use crate::app::{AppData, AppEvent};
use crate::event_inspector::InspectorEvent;
use crate::state::inspector_view::{InspectorBand, InspectorDetail};
use crate::view::disclosure::{RevealAxis, disclosure_glyph};

/// 帯の見出し。
pub(super) struct BandHead<'s> {
    /// 開閉状態の鍵 (種類ごとに共通)。
    pub key: InspectorBand,
    /// 名前 (「音声クリップ」「Rack」…)。
    pub title: &'s str,
    /// 対象 (「Drums」「選択中の 3 トラック」「Vocal のみ」…)。右寄せで、幅が足りなければ省略。空なら出さない
    /// (タイトルの 1 行目と同じ語は呼び出し側が空にする、`track_inspector::draw_bands`)。
    pub target: &'s str,
    /// 対象の左に警告色で添える注意 (Rack の「読み込み失敗 N」)。空なら出さない。畳んでいても見える。
    pub warn: &'s str,
    /// 見出しの右端に子の操作を置く幅 (モジュレーションの「+」)。対象はその左に寄せる。
    pub actions_w: f32,
}

impl<'s> BandHead<'s> {
    /// 対象だけの見出し (注意も子の操作も無い)。
    pub fn new(key: InspectorBand, title: &'s str, target: &'s str) -> Self {
        Self { key, title, target, warn: "", actions_w: 0.0 }
    }
}

/// 見出しを描いた結果。
pub(super) struct BandHeadOut {
    /// 開いているか (中身を描くか)。
    pub open: bool,
    /// 見出しの右端の、子の操作を置く矩形 (`actions_w` が 0 なら幅 0)。
    pub actions: Rect,
    /// 中身を描き始める y (開いているとき)。
    pub body_y: f32,
    /// 見出しだけで終わるときの次の y (畳んでいる / 中身が空)。
    pub head_end: f32,
}

/// 見出しを `y` に描き、見出しのクリックで開閉する。中身は呼び出し側が `body_y` から描き、
/// [`end`] で帯を閉じる (子の操作を見出しの上に置くために分けてある。普通は [`band`])。
pub(super) fn head(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect, y: f32, h: &BandHead<'_>) -> BandHeadOut {
    let p = &app.theme.core;
    let rect = Rect { x: area.x, y, w: area.w, h: BAND_H };
    let open = app.inspector_band_open(h.key);
    // popup (dropdown の一覧 / 右クリックメニュー) が開いている間は反応しない
    // (`[[feedback_popup_click_leaks_to_background]]`)。
    let inside = !ui.has_open_popups() && ui.pointer().pos.is_some_and(|(px, py)| rect.contains(px, py));
    let clicked = ui.primary_click(WidgetId::ROOT.child(("inspector_band_head", h.key)), inside).clicked;
    if clicked {
        let key = h.key;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Inspector(InspectorEvent::ToggleBand(key)));
        }));
    }
    // 見出しは押せる面なので button と同じ `control` / `control_hover` で塗る — 中の Rack の行
    // (`panel_raised`) より 1 段持ち上がり、親の見出しが子の行より目立つ (両テーマで地から離れる向き)。
    let fill = if inside && ui.hovers(rect) { p.control_hover } else { p.control };
    ui.panel(("inspector_band_bg", h.key), rect, fill, 0.0);
    ui.panel(("inspector_band_rule", h.key), Rect { y: y + BAND_H - 1.0, h: 1.0, ..rect }, p.border, 0.0);

    let text_y = |font: f32| y + (BAND_H - font * 1.2) * 0.5;
    let x = area.x + PAD;
    ui.label_at(
        ("inspector_band_glyph", h.key),
        disclosure_glyph(!open, RevealAxis::Block),
        x,
        text_y(DISCLOSURE_FONT),
        DISCLOSURE_FONT,
        p.text_dim,
    );
    let title_x = x + DISCLOSURE_W;
    let right = area.x + area.w - PAD;
    let actions = Rect { x: right - h.actions_w, y: y + 2.0, w: h.actions_w, h: BAND_H - 4.0 };
    let mut tail_right = if h.actions_w > 0.0 { actions.x - BAND_ACTIONS_GAP } else { right };
    let title_w = ui.measure_text(h.title, HEAD_FONT).min((tail_right - title_x).max(0.0));
    ui.label_at_clipped(
        ("inspector_band_title", h.key),
        h.title,
        Rect { x: title_x, y: text_y(HEAD_FONT), w: title_w + 1.0, h: HEAD_FONT * 1.2 },
        HEAD_FONT,
        p.text,
    );
    // 右から 対象 (text_dim) → 注意 (text_error) の順に詰める。名前に食い込む分は省略する。
    let left = title_x + title_w + BAND_TARGET_GAP;
    for (k, text, color) in [(0u8, h.target, p.text_dim), (1u8, h.warn, p.text_error)] {
        if text.is_empty() {
            continue;
        }
        let w = ui.measure_text(text, TARGET_FONT).min((tail_right - left).max(0.0));
        if w <= 0.0 {
            break;
        }
        let rect = Rect { x: tail_right - w, y: text_y(TARGET_FONT), w, h: TARGET_FONT * 1.2 };
        ui.label_at_clipped(("inspector_band_target", h.key, k), text, rect, TARGET_FONT, color);
        tail_right = rect.x - BAND_ACTIONS_GAP;
    }
    BandHeadOut { open, actions, body_y: y + BAND_H + BAND_BODY_TOP, head_end: y + BAND_H }
}

/// 中身を描き終えた y から帯の次の y を返す。中身が空 (`body_end == body_y`) なら見出しだけで閉じる。
pub(super) fn end(out: &BandHeadOut, body_end: f32) -> f32 {
    if out.open && body_end > out.body_y + 0.5 { body_end + BAND_BODY_BOTTOM } else { out.head_end }
}

/// 帯 1 本: 見出しを描き、開いていれば `body(ui, y) -> 次の y` を呼ぶ。返り値 = 帯の次の y。
pub(super) fn band<'u>(
    app: &AppData,
    ui: &mut Ui<'u, AppData>,
    area: Rect,
    y: f32,
    h: &BandHead<'_>,
    body: impl FnOnce(&mut Ui<'u, AppData>, f32) -> f32,
) -> f32 {
    let out = head(app, ui, area, y, h);
    let body_end = if out.open { body(ui, out.body_y) } else { out.body_y };
    end(&out, body_end)
}

/// 帯の中の「詳細」の行 (`▶ ラベル`)。クリックで開閉し、開いていれば `body(ui, y) -> 次の y` を呼ぶ。
/// 置くのは小まとまりの末尾 — 開いて動くのは詳細の行より下だけ。
#[allow(clippy::too_many_arguments)]
pub(super) fn details<'u>(
    app: &AppData,
    ui: &mut Ui<'u, AppData>,
    x: f32,
    w: f32,
    y: f32,
    key: InspectorDetail,
    label: &str,
    body: impl FnOnce(&mut Ui<'u, AppData>, f32) -> f32,
) -> f32 {
    let p = &app.theme.core;
    let row = Rect { x, y, w, h: DETAIL_ROW_H };
    let open = app.cur.view.inspector_folds.detail_open(key);
    let inside = !ui.has_open_popups() && ui.pointer().pos.is_some_and(|(px, py)| row.contains(px, py));
    if ui.primary_click(WidgetId::ROOT.child(("inspector_detail_row", key)), inside).clicked {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Inspector(InspectorEvent::ToggleDetail(key)));
        }));
    }
    let ink = if inside && ui.hovers(row) { p.text } else { p.text_dim };
    let text_y = |font: f32| y + (row.h - font * 1.2) * 0.5;
    ui.label_at(
        ("inspector_detail_glyph", key),
        disclosure_glyph(!open, RevealAxis::Block),
        x,
        text_y(DISCLOSURE_FONT),
        DISCLOSURE_FONT,
        ink,
    );
    ui.label_at_clipped(
        ("inspector_detail_label", key),
        label,
        Rect { x: x + DISCLOSURE_W, y: text_y(TARGET_FONT), w: (w - DISCLOSURE_W).max(0.0), h: TARGET_FONT * 1.2 },
        TARGET_FONT,
        ink,
    );
    let next = y + row.h + DETAIL_GAP;
    if open { body(ui, next) } else { next }
}

/// 小見出し (`ROOM ────`): 10px `text_dim` のラベルと、右端までの細い罫線。`right` は右端に右寄せで出す
/// 補助表示 (Delay の実効 ms など)。返り値 = 小見出しの下の y。内蔵 device の Par の小見出し
/// (`native_panel::cell::section_label`) もこれを通す (同じ描き方を 2 本持たない)。
#[allow(clippy::too_many_arguments)]
pub(super) fn sub_heading(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    id: impl std::hash::Hash + Copy,
    x: f32,
    w: f32,
    y: f32,
    label: &str,
    right: Option<&str>,
) -> f32 {
    let color = app.theme.core.text_dim;
    let right_edge = x + w;
    let label_w = ui.measure_text(label, SUB_HEAD_FONT);
    ui.label_at((id, 0u8), label, x, y, SUB_HEAD_FONT, color);
    let rule_end = match right {
        Some(text) => {
            let tw = ui.measure_text(text, SUB_HEAD_FONT);
            let tx = (right_edge - tw).max(x + label_w + SUB_HEAD_GAP * 2.0);
            ui.label_at((id, 1u8), text, tx, y, SUB_HEAD_FONT, color);
            tx - SUB_HEAD_GAP
        }
        None => right_edge,
    };
    let rule_x = x + label_w + SUB_HEAD_GAP;
    if rule_end > rule_x {
        let r = Rect { x: rule_x, y: y + SUB_HEAD_FONT * 0.5, w: rule_end - rule_x, h: 1.0 };
        ui.panel((id, 2u8), r, color, 0.0);
    }
    y + SUB_HEAD_H
}
