//! 選択中ノートの歌詞編集パネル。`text_input` で歌詞を編集 → SetSelectedNoteLyric。
//! ノート未選択時はプレースホルダ表示。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent};

const COLOR_BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const COLOR_TEXT: Color = Color { r: 0.85, g: 0.88, b: 0.92, a: 1.0 };
const COLOR_TEXT_DIM: Color = Color { r: 0.55, g: 0.58, b: 0.65, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("lyric_bg", area, COLOR_BG, 0.0);

    let pad = 8.0;
    ui.label_at(
        "lyric_title",
        "Lyric",
        area.x + pad,
        area.y + pad,
        13.0,
        COLOR_TEXT,
    );

    let input_y = area.y + pad + 22.0;
    if app.selected_notes.is_empty() {
        ui.label_at(
            "lyric_empty",
            "(\u{30ce}\u{30fc}\u{30c8}\u{672a}\u{9078}\u{629e})",
            area.x + pad,
            input_y,
            12.0,
            COLOR_TEXT_DIM,
        );
        return;
    }

    let lyric = app.selected_lyric();
    ui.text_input_at(
        "lyric_input",
        Rect {
            x: area.x + pad,
            y: input_y,
            w: area.w - pad * 2.0,
            h: 28.0,
        },
        &lyric,
        |new| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSelectedNoteLyric(new))
            })
        },
    );
}
