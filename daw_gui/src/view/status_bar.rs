//! 画面下端のステータスバー: ファイルパス / MIDI 入力 / status_message。

use daw_ui_core::Ui;
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::AppData;

const COLOR_BG: Color = Color { r: 0.18, g: 0.18, b: 0.22, a: 1.0 };
const COLOR_TEXT: Color = Color { r: 0.65, g: 0.68, b: 0.72, a: 1.0 };
const COLOR_MSG: Color = Color { r: 0.55, g: 0.85, b: 0.55, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.heavy("status_bg", |hctx| {
        hctx.cached((area.w.to_bits(), area.h.to_bits()), |hctx| {
            hctx.push_rect(RectCommand {
                rect: area,
                fill: COLOR_BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });

    let pad = 12.0;
    let line_y = area.y + (area.h - 11.0) * 0.5;

    let left = format!(
        "MIDI: {} \u{2502} file: {}",
        if app.midi_input_label.is_empty() {
            "(none)"
        } else {
            app.midi_input_label.as_str()
        },
        app.file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unsaved)".to_string()),
    );
    ui.label_at("status_left", &left, area.x + pad, line_y, 11.0, COLOR_TEXT);

    if !app.status_message.is_empty() {
        // 画面右寄りに status_message。文字幅が分からないので右端から逆算は難しい。
        // 今は中央寄り左固定で。
        let mid_x = area.x + area.w * 0.55;
        ui.label_at(
            "status_message",
            &app.status_message,
            mid_x,
            line_y,
            11.0,
            COLOR_MSG,
        );
    }
}
