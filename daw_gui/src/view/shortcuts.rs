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
    // Ableton Live 互換: Ctrl+G で選択トラック群をグループ化、
    // Alt+G で解除。 G 単独は元の grid snap toggle のまま。
    m.bind("daw.group_tracks", "Ctrl+G");
    m.bind("daw.ungroup_tracks", "Alt+G");
    // Grid snap (G キー) / auto-fit zoom (X キー)。focus 中の text_input 無効時のみ発火。
    m.bind("daw.toggle_snap", "G");
    m.bind("daw.fit_view", "X");
    // Ableton Live 互換 (modifier 無し版): 1=Narrow, 2=Widen, 3=Toggle Triplet。
    m.bind("daw.narrow_grid", "1");
    m.bind("daw.widen_grid", "2");
    m.bind("daw.toggle_triplet", "3");
    // gui_01 piano_roll widget の `take_shortcut("add_note")` 用バインド。
    m.bind("add_note", "Insert");
    // gui_01 #017 (M14 Phase 59): piano_roll で note 1 つ選択中に L で歌詞
    // 編集モード起動。 修飾なし shortcut だが widget 側で `is_typing_only`
    // 扱いされるので、 編集中の text_input 入力中は 'l' 文字として届く。
    m.bind("piano_roll.edit_lyric", "L");
    // gui_01 #019: 選択中 clip の末尾直後にコピー生成。
    // - D: 共有コピー (linked clip、 source content を共有)
    // - Alt+D: 独立コピー (notes を deep clone + 新 ContentId)
    // text_input フォーカス中は無効 (gui_01 が自動処理)。
    m.bind("daw.duplicate_clip_shared", "D");
    m.bind("daw.duplicate_clip_unique", "Alt+D");
    // Phase 1 PR7 (`docs/plan_audio_clip.md` §3.3 / §14): clip kind に
    // 関係なく MIDI / Audio / Vocal すべての clip に対して動く。
    // - E: cursor (= マウスホバー位置) で選択 clip を 2 つに split (snap 適用)
    // - Alt+E: 同上だが snap 一時無効
    // - J: 選択中の隣接 clip を 1 つに glue (Consolidate)
    m.bind("daw.split_clip_at_cursor", "E");
    m.bind("daw.split_clip_at_cursor_no_snap", "Alt+E");
    m.bind("daw.glue_selected_clips", "J");
    // Phase 2 PR-D 段階 1: Audio Editor で開いている clip 内 event を
    // Duplicate (spec §3.10.2 の `Ctrl+D`)。 root.rs::dispatch_shortcuts
    // で `audio_editor_clip is Some` のときだけ消費するよう gate。
    m.bind("daw.duplicate_audio_event", "Ctrl+D");
    // PR-D 段階 2: Audio Editor 内で multi-event clip の event 選択を
    // 移動する keyboard navigation (= Inspector / overlay highlight が
    // 当該 event に追従)。 Ctrl+] / Ctrl+[ で next / prev (Bitwig clip
    // navigation を参考)。 audio_editor_clip is Some 時のみ消費。
    m.bind("daw.next_audio_event", "Ctrl+]");
    m.bind("daw.prev_audio_event", "Ctrl+[");
    m
}
