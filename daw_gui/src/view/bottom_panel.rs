//! Bottom panel: Mixer / Piano Roll を切り替えるタブ + 中身。
//!
//! gui_01 の `Ui::tab_view_with_state` を使う。selected の永続は `app.bottom_panel`
//! (u8) で持ち、widget には `&mut usize` で渡す。タブクリックで `*selected` が
//! 書き換わるので、変化があれば `AppEvent::SelectBottomPanel` で AppData に
//! 反映する (`gui_01_conversation_archive_001.md` #004 で gui_01 から共有された
//! パターン)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::view::{lyric_panel, mixer_strips, piano_roll_view};

const LYRIC_W: f32 = 240.0;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let prev_tab = app.bottom_panel as usize;
    let mut tab_idx = prev_tab;

    ui.tab_view_with_state("bottom_tabs", area, &mut tab_idx, |tabs| {
        tabs.tab("Mixer", |ui, pane| {
            mixer_strips::draw(app, ui, pane);
        });
        tabs.tab("Piano Roll", |ui, pane| {
            let pr = Rect {
                x: pane.x,
                y: pane.y,
                w: (pane.w - LYRIC_W).max(0.0),
                h: pane.h,
            };
            let lyr = Rect {
                x: pane.x + pane.w - LYRIC_W,
                y: pane.y,
                w: LYRIC_W,
                h: pane.h,
            };
            piano_roll_view::draw(app, ui, pr);
            lyric_panel::draw(app, ui, lyr);
        });
    });

    if tab_idx != prev_tab {
        let new_idx = tab_idx as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SelectBottomPanel(new_idx));
        }));
    }
}
