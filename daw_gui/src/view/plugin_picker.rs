//! Plugin picker (modal overlay)。`Ui::modal` + `Ui::list_view` で構築。
//!
//! root.rs から常時呼ばれる。app.is_plugin_picker_open == true のときに
//! modal を開き、ESC / outside click / Close ボタンで閉じる。

use daw_ui_core::{Edit, ListViewStyle, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{theme, Color, Rect};

use crate::app::{AppData, AppEvent, PluginCategory};

const COLOR_TEXT: Color = theme::TEXT;
// プラグイン種別タグ / format ラベルの色分け (楽器=緑 / FX=青 / MIDI=橙 / 映像=紫)。
// theme に「プラグイン分類タグ」専用 token が無いため、 一意の category accent として
// 固有色を残す (選択行では TEXT_ON_ACCENT に潰す)。
const COLOR_TEXT_FORMAT: Color = Color { r: 0.55, g: 0.78, b: 0.95, a: 1.0 };
const COLOR_TAG_INST: Color = Color { r: 0.58, g: 0.85, b: 0.55, a: 1.0 };
const COLOR_TAG_FX: Color = Color { r: 0.55, g: 0.78, b: 0.95, a: 1.0 };
const COLOR_TAG_MIDI: Color = Color { r: 0.95, g: 0.74, b: 0.45, a: 1.0 };
const COLOR_TAG_VIDEO: Color = Color { r: 0.80, g: 0.62, b: 0.95, a: 1.0 };

const PANEL_W: f32 = 520.0;
const PANEL_H: f32 = 460.0;
const TITLE_H: f32 = 36.0;
const SEARCH_H: f32 = 26.0;

const MODAL_STYLE: ModalStyle = ModalStyle {
    overlay_color: theme::BACKDROP,
    panel_bg: theme::PANEL,
    panel_radius: 6.0,
    close_on_outside_click: true,
    close_on_escape: true,
};

const LIST_STYLE: ListViewStyle = ListViewStyle {
    row_height: 26.0,
    row_gap: 2.0,
    row_bg: theme::PANEL_RAISED,
    row_bg_hover: theme::CONTROL_HOVER,
    row_bg_selected: theme::ACCENT,
    radius: 3.0,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    // is_plugin_picker_open を modal 可視性の SSoT にする。 選択時 (Ctrl 以外) は
    // select_plugin_from_db が flag=false にするので、 ここで modal を閉じる
    // (= 選択したら閉じる、 FIXME #26)。 Ctrl 選択は flag=true のままなので開いた
    // まま連続追加できる。
    if app.is_plugin_picker_open && !ui.is_modal_open("plugin_picker") {
        ui.open_modal("plugin_picker");
    } else if !app.is_plugin_picker_open && ui.is_modal_open("plugin_picker") {
        ui.close_modal("plugin_picker");
    }

    ui.modal(
        "plugin_picker",
        (PANEL_W, PANEL_H),
        &MODAL_STYLE,
        Some(Box::new(|| {
            Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ClosePluginPicker))
        })),
        |ui, panel| {
            let pad = 12.0;

            // タイトル + Rescan / Close
            ui.label_at(
                "pp_title",
                "Select Plugin",
                panel.x + pad,
                panel.y + pad,
                16.0,
                COLOR_TEXT,
            );
            let rescan_w = 90.0;
            let close_w = 32.0;
            let rescan_x = panel.x + panel.w - pad - rescan_w - 6.0 - close_w;
            let close_x = panel.x + panel.w - pad - close_w;
            let is_rescanning = app.is_rescanning;
            ui.button_at(
                "pp_rescan",
                if is_rescanning { "Rescanning" } else { "Rescan" },
                Rect { x: rescan_x, y: panel.y + pad - 2.0, w: rescan_w, h: 24.0 },
                || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::RescanPluginDb)),
            );
            // ✕ ボタンは button_at_clicked + close_modal で閉じる。
            // close_modal が popup state を直接 remove → modal の on_close (上で渡した)
            // が次フレームで AppEvent::ClosePluginPicker を 1 度発火するため、 button click
            // 経路では Edit を発行しない (二重発火回避)。 gui_01 conversation #015 参照。
            if ui.button_at_clicked(
                "pp_close",
                "x",
                Rect { x: close_x, y: panel.y + pad - 2.0, w: close_w, h: 24.0 },
            ) {
                ui.close_modal("plugin_picker");
            }

            // 検索ボックス (タイトルと一覧の間)。 modal を開くと
            // text_input_at_focused が初回表示で自動 focus を取るので、 開いて
            // すぐタイプして絞り込める。 1 文字毎に SetPluginPickerQuery を発行し、
            // refresh_picker_visible が name / vendor の subsequence マッチで
            // visible を再計算する (controlled mode)。
            let search_rect = Rect {
                x: panel.x + pad,
                y: panel.y + TITLE_H + 6.0,
                w: panel.w - pad * 2.0,
                h: SEARCH_H,
            };
            let search_resp = ui.text_input_at_focused(
                "pp_search",
                search_rect,
                &app.plugin_picker_query,
                |new| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetPluginPickerQuery(new.clone()))
                    })
                },
            );
            // gui_01 #057 (Phase 86): focus を保ったまま ↑↓ で候補リストのカーソル移動。
            // Left/Right は text_input の cursor 移動に使われるので返らない。
            if search_resp.nav_down {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::MovePluginPickerCursor(1))
                }));
            }
            if search_resp.nav_up {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::MovePluginPickerCursor(-1))
                }));
            }
            // text_input は placeholder 非対応なので、 空のとき薄色ヒントを重ねる。
            if app.plugin_picker_query.is_empty() {
                ui.label_at(
                    "pp_search_hint",
                    "Filter by name / vendor  (e.g. murv)",
                    search_rect.x + 8.0,
                    search_rect.y + 6.0,
                    13.0,
                    COLOR_TEXT,
                );
            }
            // Enter でカーソル位置の候補を確定 (= list クリックと同じ経路でロード)。
            // 0 件なら何もしない。 カーソルは ↑↓ で動かす (gui_01 #057)。
            if search_resp.committed
                && let Some(target) = app.plugin_picker_visible.get(app.plugin_picker_cursor)
            {
                let id = target.id.clone();
                // 修飾キー (FIXME #26): Ctrl=開いたまま連続追加 / Shift=GUI を開かない。
                let m = ui.pointer().modifiers;
                let keep_open = m.ctrl;
                let open_gui = !m.shift;
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SelectPluginFromDb { id, keep_open, open_gui })
                }));
            }

            // 一覧 (検索ボックスの下)
            let list_y = search_rect.y + SEARCH_H + 6.0;
            let list_rect = Rect {
                x: panel.x + pad,
                y: list_y,
                w: panel.w - pad * 2.0,
                h: panel.y + panel.h - pad - list_y,
            };
            let visible = &app.plugin_picker_visible;
            if visible.is_empty() {
                ui.label_at(
                    "pp_empty",
                    "(\u{8a72}\u{5f53}\u{30d7}\u{30e9}\u{30b0}\u{30a4}\u{30f3}\u{306a}\u{3057})",
                    list_rect.x,
                    list_rect.y + 8.0,
                    12.0,
                    COLOR_TEXT,
                );
                return;
            }

            // カーソル位置を ListView ハイライト用に渡す。 visible 非空保証下なので
            // saturating_sub(1) で安全に clamp (refresh で 0 リセットされる + Move で
            // clamp 済みなので通常 cursor < visible.len() だが防衛的に)。
            let cursor = app
                .plugin_picker_cursor
                .min(visible.len().saturating_sub(1));
            // row callback は label のみ描画 (背景は list_view が selected / hover /
            // 通常で塗り分けてくれる)。 旧来 `button_at` で row 全面を塗っていたため
            // 選択ハイライト (`row_bg_selected` = 青) が button bg に隠れて見えなかった。
            // クリック判定は下の `resp.clicked` を採用する。
            let resp = ui.list_view(
                "pp_list",
                list_rect,
                visible,
                Some(cursor),
                &LIST_STYLE,
                |ui, entry, i, row_rect, is_selected| {
                    // 選択行は全 label を白に統一 (背景青と高コントラスト、 Windows
                    // ListBox / macOS NSTableView の反転表示慣習)。 非選択時は format
                    // を青、 vendor を dim グレーで区別。 選択時は format の青と背景
                    // 青が同化して読めなくなるため白で潰す。
                    let (name_color, vendor_color, format_color) = if is_selected {
                        (
                            theme::TEXT_ON_ACCENT,
                            theme::TEXT_ON_ACCENT,
                            theme::TEXT_ON_ACCENT,
                        )
                    } else {
                        (COLOR_TEXT, COLOR_TEXT, COLOR_TEXT_FORMAT)
                    };
                    ui.label_at(
                        ("pp_row_name", i),
                        &entry.name,
                        row_rect.x + 10.0,
                        row_rect.y + 6.0,
                        12.0,
                        name_color,
                    );
                    ui.label_at(
                        ("pp_row_vendor", i),
                        &entry.vendor,
                        row_rect.x + row_rect.w - 200.0,
                        row_rect.y + 6.0,
                        10.0,
                        vendor_color,
                    );
                    ui.label_at(
                        ("pp_row_format", i),
                        &entry.format_label,
                        row_rect.x + row_rect.w - 50.0,
                        row_rect.y + 6.0,
                        10.0,
                        format_color,
                    );
                    // 種別タグ (楽器 / FX / MIDI): 混合リストで選ぶ前に行き先が分かる。
                    let tag_color = if is_selected {
                        theme::TEXT_ON_ACCENT
                    } else {
                        match entry.category {
                            PluginCategory::Instrument => COLOR_TAG_INST,
                            PluginCategory::Fx => COLOR_TAG_FX,
                            PluginCategory::MidiFx => COLOR_TAG_MIDI,
                            PluginCategory::Video => COLOR_TAG_VIDEO,
                        }
                    };
                    ui.label_at(
                        ("pp_row_tag", i),
                        entry.category.tag(),
                        row_rect.x + row_rect.w - 250.0,
                        row_rect.y + 6.0,
                        10.0,
                        tag_color,
                    );
                },
            );
            if let Some(idx) = resp.clicked
                && let Some(target) = visible.get(idx)
            {
                let id = target.id.clone();
                // 修飾キー (FIXME #26): Ctrl=開いたまま連続追加 / Shift=GUI を開かない。
                let m = ui.pointer().modifiers;
                let keep_open = m.ctrl;
                let open_gui = !m.shift;
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SelectPluginFromDb { id, keep_open, open_gui })
                }));
            }
        },
    );
}
