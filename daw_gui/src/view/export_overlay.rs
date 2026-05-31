//! Video export 中の進捗オーバーレイ。
//!
//! export は background thread で走り（`AppData::action_export_mp4`）、進捗を
//! `AppEvent::ExportProgress` で送ってくる。長尺 project は数十秒〜数分かかる
//! ため、 UI を固めず進捗と Cancel を見せる。`export_progress` が `Some` の間
//! だけ dismissable でない modal として表示する（Esc / 外クリックでは閉じず、
//! Cancel ボタンのみ中断）。

use daw_ui_core::{Edit, ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent};

const COLOR_TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const COLOR_TEXT_DIM: Color = Color { r: 0.65, g: 0.68, b: 0.72, a: 1.0 };
const BAR_BG: Color = Color { r: 0.12, g: 0.13, b: 0.16, a: 1.0 };
const BAR_FILL: Color = Color { r: 0.40, g: 0.70, b: 0.95, a: 1.0 };

const PANEL_W: f32 = 420.0;
const PANEL_H: f32 = 150.0;
const PAD: f32 = 16.0;
const BTN_H: f32 = 28.0;
const BTN_W: f32 = 110.0;

const MODAL_STYLE: ModalStyle = ModalStyle {
    overlay_color: Color { r: 0.0, g: 0.0, b: 0.0, a: 0.6 },
    panel_bg: Color { r: 0.18, g: 0.18, b: 0.22, a: 1.0 },
    panel_radius: 6.0,
    close_on_outside_click: false,
    close_on_escape: false,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    let Some((done, total)) = app.export_progress else {
        if ui.is_modal_open("export_progress") {
            ui.close_modal("export_progress");
        }
        return;
    };
    if !ui.is_modal_open("export_progress") {
        ui.open_modal("export_progress");
    }
    let pct = if total > 0 {
        (done as f32 / total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ui.modal(
        "export_progress",
        (PANEL_W, PANEL_H),
        &MODAL_STYLE,
        None,
        move |ui, panel| {
            ui.label_at(
                "exp_title",
                "Video export 中...",
                panel.x + PAD,
                panel.y + PAD,
                16.0,
                COLOR_TEXT,
            );
            ui.label_at(
                "exp_count",
                &format!("{done} / {total} フレーム ({:.0}%)", pct * 100.0),
                panel.x + PAD,
                panel.y + PAD + 28.0,
                13.0,
                COLOR_TEXT_DIM,
            );
            // 進捗バー（bg + fill）。
            let bar_y = panel.y + PAD + 54.0;
            let bar_w = panel.w - PAD * 2.0;
            let bar_h = 12.0;
            ui.panel(
                "exp_bar_bg",
                Rect { x: panel.x + PAD, y: bar_y, w: bar_w, h: bar_h },
                BAR_BG,
                3.0,
            );
            ui.panel(
                "exp_bar_fill",
                Rect { x: panel.x + PAD, y: bar_y, w: bar_w * pct, h: bar_h },
                BAR_FILL,
                3.0,
            );
            // Cancel ボタン（右下）。
            let btn_y = panel.y + panel.h - PAD - BTN_H;
            let btn_x = panel.x + panel.w - PAD - BTN_W;
            if ui.button_at_clicked(
                "exp_cancel",
                "キャンセル",
                Rect { x: btn_x, y: btn_y, w: BTN_W, h: BTN_H },
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CancelExport)
                }));
            }
        },
    );
}
