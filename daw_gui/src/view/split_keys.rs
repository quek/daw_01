//! r.md #132: 分割 / 結合キー (`E` / `Alt+E` / `Shift+E` / `J`) の振り分け
//! (`docs/plan_rmd_132_grid_split.md`)。
//!
//! どのキーも「ピアノロール (オーディオエディタを開いていない下部パネル) の上ならノート、
//! それ以外ならクリップ (オーディオエディタの波形の上ならその event)」 に効く。 面の判定だけを
//! ここで行い、処理は `AppEvent::SplitJoin` 1 本で handler へ渡す (`handle_event` を通るので
//! 1 回のキー操作 = 1 undo step、履歴に操作名が付く)。 `view/root.rs::dispatch_shortcuts` の
//! E / J 節を切り出したもの (サイズ budget、不変条件 9)。

use daw_ui_core::{Edit, Ui};

use crate::app::{AppData, AppEvent};
use crate::event_split::{SplitAt, SplitJoinEvent, SplitSurface};

/// `daw.split_clip_at_cursor` (E) / `daw.split_clip_at_cursor_no_snap` (Alt+E) /
/// `daw.split_at_grid` (Shift+E) / `daw.glue_selected_clips` (J) を消費してイベントを積む。
///
/// `is_pianoroll_active` = Piano Roll タブが選ばれていて pointer が下部パネル内。
pub(super) fn dispatch(app: &AppData, ui: &mut Ui<'_, AppData>, is_pianoroll_active: bool) {
    let surface = if is_pianoroll_active && app.cur.peph.audio_editor_clip.is_none() {
        SplitSurface::Notes
    } else {
        SplitSurface::Clips
    };
    let split = [
        ("daw.split_clip_at_cursor", SplitAt::Cursor { snap: true }),
        ("daw.split_clip_at_cursor_no_snap", SplitAt::Cursor { snap: false }),
        ("daw.split_at_grid", SplitAt::Grid),
    ];
    for (name, at) in split {
        if ui.take_shortcut(name) {
            push(ui, SplitJoinEvent::Split { surface, at });
        }
    }
    // `j` はビューで意味が分かれる — アレンジャー = 範囲を 1 クリップへ焼き込む /
    // ピアノロール = **Join Notes** (同じ音のノートを 1 本に結合)。 Live も `Ctrl+J` を
    // Consolidate / Join Notes に振り分けている (`docs/plan_range_selection.md` §7.4)。
    if ui.take_shortcut("daw.glue_selected_clips") {
        push(ui, SplitJoinEvent::Join { surface });
    }
}

fn push(ui: &mut Ui<'_, AppData>, ev: SplitJoinEvent) {
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::SplitJoin(ev));
    }));
}
