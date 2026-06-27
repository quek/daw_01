//! 画面下端のステータスバー: ファイルパス / MIDI 入力 / status_message。

use daw_ui_core::Ui;
use daw_ui_renderer::{theme, Color, Rect};

use crate::app::AppData;

const COLOR_BG: Color = theme::HEADER;
// 旧 dim グレー (0.65/0.68/0.72) はコントラスト不足だったため primary
// (= 他 view と同じ body text) に統一。MIDI/file ラベルの可読性を上げる。
const COLOR_TEXT: Color = theme::TEXT;
// status_message は成功/通知系の緑 = semantic PLAY (status success)。
const COLOR_MSG: Color = theme::PLAY;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("status_bg", area, COLOR_BG, 0.0);

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
