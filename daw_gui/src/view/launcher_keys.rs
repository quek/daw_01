//! r.md #87 クリップランチャーの **キーボード操作** と、
//! ランチャー widget (束 C) から受けたイベントの流し込み。
//!
//! `root.rs` から分けてあるのは不変条件 9 (サイズ budget) のため
//! — `dispatch_shortcuts` は実コード 300 行 budget の天井に張り付いている。
//!
//! ## キーの割り当てが「既存 binding への相乗り」になっている理由
//!
//! `ShortcutMap::matches` は同じキーに複数の name が bind されていても
//! **先勝ちで 1 つしか返さない**。 つまり `Left` に
//! `daw.launcher_left` を新規 bind しても、先に宣言されている
//! `daw.nudge_note_left` が必ず勝って永久に発火しない。
//! なので矢印 / `Delete` / `Ctrl+D` は **既存の name をここで文脈判定して
//! 振り分ける** (`daw.duplicate_clip_shared` が「ノート選択中はノート複製」
//! になっているのと同じ流儀)。
//!
//! | キー | 既存 name | ランチャーが取る条件 |
//! |---|---|---|
//! | 矢印 | `daw.nudge_note_*` | 対象面が `Notes` **でない** かつ ランチャーが操作対象 |
//! | `Enter` | `daw.launcher_fire` (新規) | ランチャーが操作対象 |
//! | `Delete` | `delete` → `DeleteSelectedClip` | セルを選んでいる (handler 側で入れ物を判別) |
//! | `Ctrl+D` | `daw.duplicate_audio_event` | セルを選んでいる (オーディオエディタの選択より優先) |
//! | `D` / `Alt+D` | `daw.duplicate_clip_*` | セルを選んでいる (`clipboard_ops::dispatch_duplicate`) |
//!
//! 「ランチャーが操作対象」 = フォーカスがある **かつ** (ポインタがランチャー帯に
//! 乗っている または セルを選んでいる)。フォーカスだけを条件にすると、
//! 一度セルを触った後アレンジで作業していても矢印がランチャーを動かしてしまう。

use daw_ui_core::{Edit, Ui};

use crate::app::{AppData, AppEvent, EditSurface};
use crate::event_launcher::LauncherEvent;

/// 1 フレームに積める repeat 数の上限 (`dispatch_note_nudge` と同じ理由)。
const MAX_REPEATS_PER_FRAME: usize = 64;

/// ランチャーのキーボード操作を消費する。`dispatch_shortcuts` の途中から
/// **`dispatch_note_nudge` より先に**呼ぶこと (矢印の取り合いをここで決める)。
pub(crate) fn dispatch_launcher_keys(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    surface: Option<EditSurface>,
) {
    // Tab は文脈に関係なく効く (帯の見せ方はいつでも切り替えたい)。
    if ui.take_shortcut("daw.cycle_launcher_layout") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Launcher(LauncherEvent::CycleLayout));
        }));
    }
    // ここから下は **ランチャーが今の操作対象** のときだけ。
    //
    // フォーカスはセルを 1 度クリックしたら残り続けるので、それだけを条件に
    // すると「セルを触った後、アレンジで作業していても矢印がランチャーを動かす」
    // という位置依存の取り違えになる。 対象面の判定は copy / delete と同じ
    // 流儀 (ポインタが乗っている面 → 選択が生きている面) に揃える。
    let active = app.launcher.focus.is_some()
        && (app.launcher.hover.is_some() || !app.selected_launcher_cells().is_empty());
    if !active {
        return;
    }
    if ui.take_shortcut("daw.launcher_fire") {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::Launcher(LauncherEvent::LaunchFocused));
        }));
    }
    // 矢印はノート編集が対象面のときは触らない (ピアノロールの nudge が勝つ)。
    if matches!(surface, Some(EditSurface::Notes)) {
        return;
    }
    let moves: [(&'static str, i32, i32); 4] = [
        ("daw.nudge_note_left", -1, 0),
        ("daw.nudge_note_right", 1, 0),
        ("daw.nudge_note_up", 0, -1),
        ("daw.nudge_note_down", 0, 1),
    ];
    for (name, dx, dy) in moves {
        let n = i32::try_from(ui.take_shortcut_count(name).min(MAX_REPEATS_PER_FRAME))
            .unwrap_or(0);
        if n == 0 {
            continue;
        }
        // 押しっぱなしの repeat 回数ぶんまとめて動かす (1 回しか消費しないと
        // 移動量がフレームレート次第で目減りする)。
        let (dx, dy) = (dx * n, dy * n);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Launcher(LauncherEvent::MoveFocus { dx, dy }));
        }));
    }
}

/// `Ctrl+D` (既存 name = `daw.duplicate_audio_event`) をランチャーが取るか。
///
/// セルを 1 つでも選んでいれば取る。取らなかった場合は呼び側が従来どおり
/// オーディオイベントの複製へ流す。 **shortcut は呼び側が 1 度だけ take して
/// bool を渡す** (take は消費するので 2 度読むと 2 回目が必ず false になる)。
pub(crate) fn duplicate_cells_if_launcher(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    pressed: bool,
) -> bool {
    if !pressed {
        return false;
    }
    let cells = app.selected_launcher_cells();
    if cells.is_empty() {
        return false;
    }
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::Launcher(LauncherEvent::DuplicateCells {
            cells: cells.clone(),
            unique: false,
        }));
    }));
    true
}

/// 束 C のランチャー widget が 1 フレームに集めたイベントを流し込む。
///
/// **接点はこの 1 本だけ**。`ArrangementResponse` に
/// `pub launcher: Vec<LauncherEvent>` を足して、`arrangement_view::draw` の
/// widget 呼び出し直後に `dispatch_launcher_events(ui, &resp.launcher)` を
/// 1 行書けば繋がる (widget 側は `Song` を触らず「何が押されたか」だけ返す)。
pub fn dispatch_launcher_events(ui: &mut Ui<'_, AppData>, events: &[LauncherEvent]) {
    for ev in events {
        let ev = ev.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Launcher(ev.clone()));
        }));
    }
}
