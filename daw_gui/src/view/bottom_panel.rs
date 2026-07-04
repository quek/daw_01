//! Bottom panel: Mixer / Piano Roll を切り替えるタブ + 中身。
//!
//! gui_01 の `Ui::tab_view_with_state` を使う。selected の永続は `app.ui_prefs.bottom_panel`
//! (u8) で持ち、widget には `&mut usize` で渡す。タブクリックで `*selected` が
//! 書き換わるので、変化があれば `AppEvent::SelectBottomPanel` で AppData に
//! 反映する (`gui_01_conversation_archive_001.md` #004 で gui_01 から共有された
//! パターン)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::view::{audio_editor, mixer_strips, piano_roll_view};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let prev_tab = app.ui_prefs.bottom_panel as usize;
    let mut tab_idx = prev_tab;

    // Phase 2 PR6: audio clip ダブルクリックで `audio_editor_clip` が
    // セットされるので、 そのとき tab 1 のラベルと中身を Audio Editor
    // に切替 (`docs/plan_audio_clip.md` §3.10 「piano_roll の領域を流用」)。
    let audio_editor_open = app.ui_ephemeral.audio_editor_clip.is_some();
    let tab1_label = if audio_editor_open { "Audio Editor" } else { "Piano Roll" };

    ui.tab_view_with_state("bottom_tabs", area, &mut tab_idx, |tabs| {
        tabs.tab("Mixer", |ui, pane| {
            mixer_strips::draw(app, ui, pane);
        });
        tabs.tab(tab1_label, |ui, pane| {
            if audio_editor_open {
                audio_editor::draw(app, ui, pane);
            } else {
                // 旧 lyric panel は piano_roll widget の L キー編集 (gui_01 #017)
                // で代替されたので削除。 Piano Roll タブの全幅を使う。
                piano_roll_view::draw(app, ui, pane);
            }
        });
    });

    if tab_idx != prev_tab {
        let new_idx = tab_idx as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SelectBottomPanel(new_idx));
        }));
    }
}
