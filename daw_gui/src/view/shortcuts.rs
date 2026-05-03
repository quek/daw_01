//! daw_01 用の shortcut binding。
//!
//! `ShortcutMap::with_default_bindings()` がベース (undo / redo / cut / copy / paste /
//! select_all / save / save_as / open / new / delete / escape / tab_next / tab_prev /
//! focus_up / focus_down / focus_left / focus_right) を提供する。
//!
//! 本モジュールはこれに DAW 固有の binding を追加する:
//! - `daw.play_toggle` = Space
//! - `daw.toggle_loop` = P
//! - `daw.synthesize_vocal` = V
//! - `daw.export_wav` = Ctrl+E
//! - `daw.toggle_help` = F1
//!
//! `Ui::take_shortcut(name)` で root 末尾から拾って AppEvent に変換する。
//!
//! NOTE: `gui_01` の `Shortcut::parse` は `/` 等の punctuation を受理しない
//! (alphanumeric / 特殊キー / F1-F24 のみ)。旧 `Shift+/` バインドは F1 で代替。

use daw_ui_core::ShortcutMap;

#[must_use]
pub fn daw_shortcut_map() -> ShortcutMap {
    let mut m = ShortcutMap::with_default_bindings();
    m.bind("daw.play_toggle", "Space");
    m.bind("daw.toggle_loop", "P");
    m.bind("daw.synthesize_vocal", "V");
    m.bind("daw.export_wav", "Ctrl+E");
    m.bind("daw.toggle_help", "F1");
    // gui_01 piano_roll widget の `take_shortcut("add_note")` 用バインド。
    m.bind("add_note", "Insert");
    m
}
