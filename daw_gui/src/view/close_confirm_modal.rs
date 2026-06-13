//! 未保存変更がある状態でウィンドウを閉じようとしたときの確認モーダル。
//! ふつうの DAW (Ableton / Bitwig / Logic 等) と同じく「保存して終了 /
//! 保存せず終了 / キャンセル」 の 3 択をユーザーに選ばせる。
//!
//! `Ui::modal` + `button_at_clicked` パターン (recovery_modal と同形)。
//! Esc / 外クリック / ✕ ボタンはすべて `CloseConfirmCancel` を発火し、
//! 終了を取りやめる。

use daw_ui_core::{Edit, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent};

const COLOR_TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };

const PANEL_W: f32 = 460.0;
const PANEL_H: f32 = 176.0;
const PAD: f32 = 16.0;
const TITLE_H: f32 = 28.0;
const BTN_H: f32 = 28.0;
/// 「保存して終了」 は長いので幅広に。 他 2 つは同幅。
const BTN_SAVE_W: f32 = 136.0;
const BTN_W: f32 = 116.0;
const BTN_GAP: f32 = 8.0;

const MODAL_STYLE: ModalStyle = ModalStyle {
    overlay_color: Color { r: 0.0, g: 0.0, b: 0.0, a: 0.6 },
    panel_bg: Color { r: 0.18, g: 0.18, b: 0.22, a: 1.0 },
    panel_radius: 6.0,
    close_on_outside_click: true,
    close_on_escape: true,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    if !app.show_close_confirm {
        return;
    }
    if !ui.is_modal_open("close_confirm") {
        ui.open_modal("close_confirm");
    }

    // 表示用プロジェクト名 (未保存新規は "Untitled")。
    let project_name = app
        .file_path
        .as_ref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_string();

    ui.modal(
        "close_confirm",
        (PANEL_W, PANEL_H),
        &MODAL_STYLE,
        Some(Box::new(|| {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CloseConfirmCancel)
            })
        })),
        move |ui, panel| {
            // タイトル
            ui.label_at(
                "cc_title",
                "保存の確認",
                panel.x + PAD,
                panel.y + PAD,
                16.0,
                COLOR_TEXT,
            );

            // メッセージ (2 行)
            ui.label_at(
                "cc_msg1",
                &format!("プロジェクト「{project_name}」に保存していない変更があります。"),
                panel.x + PAD,
                panel.y + PAD + TITLE_H,
                13.0,
                COLOR_TEXT,
            );
            ui.label_at(
                "cc_msg2",
                "閉じる前に保存しますか？",
                panel.x + PAD,
                panel.y + PAD + TITLE_H + 22.0,
                13.0,
                COLOR_TEXT,
            );

            // ボタン行 (下部、 右寄せ)。 左から「保存して終了」「保存せず終了」
            // 「キャンセル」。 主要操作 (保存) を左端に置く。
            let btn_y = panel.y + panel.h - PAD - BTN_H;
            let cancel_x = panel.x + panel.w - PAD - BTN_W;
            let discard_x = cancel_x - BTN_GAP - BTN_W;
            let save_x = discard_x - BTN_GAP - BTN_SAVE_W;

            if ui.button_at_clicked(
                "cc_save",
                "保存して終了",
                Rect { x: save_x, y: btn_y, w: BTN_SAVE_W, h: BTN_H },
            ) {
                ui.close_modal("close_confirm");
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CloseConfirmSave)
                }));
            }

            if ui.button_at_clicked(
                "cc_discard",
                "保存せず終了",
                Rect { x: discard_x, y: btn_y, w: BTN_W, h: BTN_H },
            ) {
                ui.close_modal("close_confirm");
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CloseConfirmDiscard)
                }));
            }

            if ui.button_at_clicked(
                "cc_cancel",
                "キャンセル",
                Rect { x: cancel_x, y: btn_y, w: BTN_W, h: BTN_H },
            ) {
                ui.close_modal("close_confirm");
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CloseConfirmCancel)
                }));
            }
        },
    );
}
