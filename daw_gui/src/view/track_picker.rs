//! Send 宛先トラックピッカー (modal overlay)。`plugin_picker.rs` を踏襲した
//! `Ui::modal` + `Ui::list_view` 構成。
//!
//! root.rs から常時呼ばれる。`app.ui_ephemeral.send_picker == Some(..)` のとき modal を
//! 開き、ESC / outside click / Close ボタンで閉じる。宛先 track を選ぶと
//! `AppEvent::AddSend { src_track_id, dest_track_id }` を発行して閉じる。
//!
//! 候補は `AppData::send_destination_candidates` が生成する (= 自分自身と
//! ルーティング閉路を作る track を除外済み)。

use daw_ui_core::{Edit, ListViewStyle, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect};
use crate::theme;

use crate::app::{AppData, AppEvent};

const COLOR_TEXT: Color = theme::TEXT;

const PANEL_W: f32 = 420.0;
const PANEL_H: f32 = 420.0;
const TITLE_H: f32 = 36.0;

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
    let Some(state) = app.ui_ephemeral.send_picker else {
        return;
    };
    let src_track_id = state.src_track_id;

    if !ui.is_modal_open("send_picker") {
        ui.open_modal("send_picker");
    }

    // 候補は毎フレーム派生 (= AppData は plain struct、 Memo 不使用)。 候補数は
    // track 数オーダーなので per-frame 再計算で十分。
    let candidates = app.send_destination_candidates(src_track_id);

    ui.modal(
        "send_picker",
        (PANEL_W, PANEL_H),
        &MODAL_STYLE,
        Some(Box::new(|| {
            Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::CloseSendPicker))
        })),
        |ui, panel| {
            let pad = 12.0;

            ui.label_at(
                "sp_title",
                "Send to track",
                panel.x + pad,
                panel.y + pad,
                16.0,
                COLOR_TEXT,
            );
            let close_w = 32.0;
            let close_x = panel.x + panel.w - pad - close_w;
            // plugin_picker と同じく ✕ は close_modal を呼ぶ → modal on_close が
            // 次フレームで CloseSendPicker を 1 度発火する (二重発火回避)。
            if ui.button_at_clicked(
                "sp_close",
                "x",
                Rect { x: close_x, y: panel.y + pad - 2.0, w: close_w, h: 24.0 },
            ) {
                ui.close_modal("send_picker");
            }

            // 一覧 (タイトルの下)
            let list_y = panel.y + TITLE_H + 6.0;
            let list_rect = Rect {
                x: panel.x + pad,
                y: list_y,
                w: panel.w - pad * 2.0,
                h: panel.y + panel.h - pad - list_y,
            };

            if candidates.is_empty() {
                ui.label_at(
                    "sp_empty",
                    "(\u{9001}\u{308c}\u{308b}\u{30c8}\u{30e9}\u{30c3}\u{30af}\u{306a}\u{3057})",
                    list_rect.x,
                    list_rect.y + 8.0,
                    12.0,
                    COLOR_TEXT,
                );
                return;
            }

            let resp = ui.list_view(
                "sp_list",
                list_rect,
                &candidates,
                None,
                &LIST_STYLE,
                |ui, entry, i, row_rect, is_selected| {
                    let name_color = if is_selected { theme::TEXT_ON_ACCENT } else { COLOR_TEXT };
                    ui.label_at(
                        ("sp_row_name", i),
                        &entry.1,
                        row_rect.x + 10.0,
                        row_rect.y + 6.0,
                        12.0,
                        name_color,
                    );
                },
            );
            if let Some(idx) = resp.clicked
                && let Some(entry) = candidates.get(idx)
            {
                let dest_track_id = entry.0;
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::AddSend { src_track_id, dest_track_id });
                    app.handle_event(AppEvent::CloseSendPicker);
                }));
                ui.close_modal("send_picker");
            }
        },
    );
}
