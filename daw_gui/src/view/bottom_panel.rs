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
use crate::view::{mixer_strips, piano_roll_view};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let prev_tab = app.bottom_panel as usize;
    let mut tab_idx = prev_tab;

    ui.tab_view_with_state("bottom_tabs", area, &mut tab_idx, |tabs| {
        tabs.tab("Mixer", |ui, pane| {
            mixer_strips::draw(app, ui, pane);
        });
        tabs.tab("Piano Roll", |ui, pane| {
            // 旧 lyric panel は piano_roll widget の L キー編集 (gui_01 #017)
            // で代替されたので削除。 Piano Roll タブの全幅を使う。
            piano_roll_view::draw(app, ui, pane);
        });
    });

    if tab_idx != prev_tab {
        let new_idx = tab_idx as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SelectBottomPanel(new_idx));
        }));
    }
}
