//! Font picker (modal overlay)。Text クリップのフォントを「プラグインピッカーと
//! 同じように」検索付きモーダルで選ぶ (`docs/plan_font_picker.md`)。
//!
//! plugin_picker.rs を下敷きにしつつ、 **各行をそのフォント自身で描画**するため
//! row callback で `ui.push_text(GlyphArea { font_family: Some(name), .. })` を呼ぶ
//! (`label_at` は default フォント固定なので使わない)。↑↓ / ホバーで編集対象の
//! テキストクリップがキャンバス上で即ライブプレビューされ、 確定で固定・Esc /
//! 外クリックで元に戻る (復元は `AppData::close_font_picker`)。

use daw_ui_core::{Edit, ListViewStyle, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{GlyphArea, Rect};

use crate::app::{AppData, AppEvent};

const PANEL_W: f32 = 460.0;
const PANEL_H: f32 = 480.0;
const TITLE_H: f32 = 36.0;
const SEARCH_H: f32 = 26.0;
const ROW_FONT: f32 = 15.0;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    // is_font_picker_open を modal 可視性の SSoT にする (plugin_picker と同 idiom)。
    // commit / cancel が flag=false にしたフレームで modal を閉じる。
    if app.ui_ephemeral.is_font_picker_open && !ui.is_modal_open("font_picker") {
        ui.open_modal("font_picker");
    } else if !app.ui_ephemeral.is_font_picker_open && ui.is_modal_open("font_picker") {
        ui.close_modal("font_picker");
    }

    // スタイルは const にできない (runtime テーマを読めない、 r.md #48)。 パレット既定を
    // ベースに、 このピッカー固有の寸法 (行はフォント見本のぶん高い) を差分で上書きする。
    let p = &app.theme.core;
    let modal_style = ModalStyle::from_palette(p);
    let list_style =
        ListViewStyle { row_height: 28.0, radius: 3.0, ..ListViewStyle::from_palette(p) };

    ui.modal(
        "font_picker",
        (PANEL_W, PANEL_H),
        &modal_style,
        // Esc / 外クリックで閉じたら cancel = 元フォントへ復元。
        Some(Box::new(|| {
            Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::CloseFontPicker))
        })),
        |ui, panel| {
            let pad = 12.0;

            ui.label_at(
                "fp_title",
                "Select Font",
                panel.x + pad,
                panel.y + pad,
                16.0,
                p.text,
            );
            let close_w = 32.0;
            let close_x = panel.x + panel.w - pad - close_w;
            // ✕ は close_modal → modal on_close が CloseFontPicker を 1 度発火する
            // (二重発火回避のためここでは Edit を出さない、 plugin_picker #015 と同じ)。
            if ui.button_at_clicked(
                "fp_close",
                "x",
                Rect { x: close_x, y: panel.y + pad - 2.0, w: close_w, h: 24.0 },
            ) {
                ui.close_modal("font_picker");
            }

            // 検索ボックス。
            let search_rect = Rect {
                x: panel.x + pad,
                y: panel.y + TITLE_H + 6.0,
                w: panel.w - pad * 2.0,
                h: SEARCH_H,
            };
            let search_resp = ui.text_input_at_focused(
                "fp_search",
                search_rect,
                &app.ui_ephemeral.font_picker_query,
                &ui.text_input_style(),
                |new| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetFontPickerQuery(new.clone()))
                    })
                },
            );
            if search_resp.nav_down {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::MoveFontPickerCursor(1))
                }));
            }
            if search_resp.nav_up {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::MoveFontPickerCursor(-1))
                }));
            }
            if app.ui_ephemeral.font_picker_query.is_empty() {
                ui.label_at(
                    "fp_search_hint",
                    "Filter fonts  (e.g. gothic)",
                    search_rect.x + 8.0,
                    search_rect.y + 6.0,
                    13.0,
                    p.text,
                );
            }
            // Enter でカーソル位置を確定。
            if search_resp.committed
                && let Some(family) = app.ui_ephemeral.font_picker_visible.get(app.ui_ephemeral.font_picker_cursor)
            {
                let family = family.clone();
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::CommitFontFromPicker(family))
                }));
            }

            let list_y = search_rect.y + SEARCH_H + 6.0;
            let list_rect = Rect {
                x: panel.x + pad,
                y: list_y,
                w: panel.w - pad * 2.0,
                h: panel.y + panel.h - pad - list_y,
            };
            let visible = &app.ui_ephemeral.font_picker_visible;
            if visible.is_empty() {
                let msg = if app.ui_ephemeral.font_picker_loading {
                    "\u{30d5}\u{30a9}\u{30f3}\u{30c8}\u{8aad}\u{8fbc}\u{4e2d}\u{2026}"
                } else {
                    "(\u{8a72}\u{5f53}\u{30d5}\u{30a9}\u{30f3}\u{30c8}\u{306a}\u{3057})"
                };
                ui.label_at(
                    "fp_empty",
                    msg,
                    list_rect.x,
                    list_rect.y + 8.0,
                    12.0,
                    p.text,
                );
                return;
            }

            let cursor = app
                .ui_ephemeral.font_picker_cursor
                .min(visible.len().saturating_sub(1));
            let resp = ui.list_view(
                "fp_list",
                list_rect,
                visible,
                Some(cursor),
                &list_style,
                |ui, family, i, row_rect, is_selected| {
                    // 行背景は list_view が塗るクローム面なので、 見本は `p.text` /
                    // 選択行は accent 塗りの上なので auto-contrast で取る。
                    let color = if is_selected { p.ink_on_accent() } else { p.text };
                    // 各行はその行のフォント自身で描画 (本物のプレビュー)。
                    // `""` = renderer default → "デフォルト" を default フォントで。
                    let (label, font): (&str, Option<std::sync::Arc<str>>) = if family.is_empty() {
                        ("\u{30c7}\u{30d5}\u{30a9}\u{30eb}\u{30c8}", None)
                    } else {
                        (family.as_str(), Some(family.as_str().into()))
                    };
                    ui.push_text(GlyphArea {
                        text: label.into(),
                        left: row_rect.x + 10.0,
                        top: row_rect.y + 6.0,
                        font_size: ROW_FONT,
                        line_height: ROW_FONT * 1.2,
                        color,
                        font_family: font,
                        clip_rect: Some(row_rect),
                        ..GlyphArea::default()
                    });
                    let _ = i;
                },
            );
            // ホバー行をライブプレビュー (cursor を合わせる)。handler 側で
            // 「既に cursor がそこなら no-op」 なので毎フレーム連発しても安全。
            if let Some(idx) = resp.hovered
                && idx != cursor
            {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::HoverFontInPicker(idx))
                }));
            }
            if let Some(idx) = resp.clicked
                && let Some(family) = visible.get(idx)
            {
                let family = family.clone();
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::CommitFontFromPicker(family))
                }));
            }
        },
    );
}
