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
//! | `Delete` | `delete` → `LauncherEvent::DeleteCells` | セルを選んでいる (対象面の解決は `delete_current_surface`) |
//! | `Ctrl+D` | `daw.duplicate_audio_event` | セルを選んでいる (オーディオエディタの選択より優先) |
//! | `D` / `Alt+D` | `daw.duplicate_clip_*` | セルを選んでいる (`clipboard_ops::dispatch_duplicate`) |
//!
//! 「ランチャーが操作対象」の判定は [`launcher_is_target`] 1 本。解決順は
//! `AppData::edit_surface` と同じ **1. 直近確定面 → 2. ポインタ位置** で
//! ([[feedback_selection_action_last_wins]])、ポインタは面が 1 つも生きていない
//! ときのタイブレークにしか使わない。

use daw_ui_core::{Edit, Ui};

use crate::app::{AppData, AppEvent, EditSurface};
use crate::event_launcher::LauncherEvent;

/// 1 フレームに積める repeat 数の上限 (`dispatch_note_nudge` と同じ理由)。
const MAX_REPEATS_PER_FRAME: usize = 64;

/// **直近に確定した面がランチャーのセル面か。**
///
/// セル面は行の種類 (トラック行 / オートメーションレーン行) に関わらず
/// [`EditSurface::LauncherCells`] 1 面なので、タグ 1 つを見れば決まる。
#[must_use]
fn launcher_owns_surface(surface: Option<EditSurface>) -> bool {
    matches!(surface, Some(EditSurface::LauncherCells))
}

/// **ランチャーが今の操作対象か** (矢印 / `Enter` / `Ctrl+D` = *既にある選択に
/// 効く*操作)。
///
/// 解決順は `AppData::edit_surface` と同じ **1. 直近確定面 → 2. ポインタ位置**
/// ([[feedback_selection_action_last_wins]])。「セルを選んでいる」だけを条件に
/// したり、ポインタを先に見たりすると、アレンジで範囲を選び直した後も矢印が帯を
/// 動かし続ける (= 固定 tier、規範が禁じている形)。
///
/// **選択を作る操作 (`Ctrl+A`) はこれを使わない** — 順序が逆
/// ([`select_all_cells_if_launcher`])。
#[must_use]
pub(crate) fn launcher_is_target(app: &AppData, surface: Option<EditSurface>) -> bool {
    launcher_owns_surface(surface)
        || (surface.is_none() && app.launcher.hover.is_some())
}

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
    // ここから下は **ランチャーが今の操作対象** のときだけ ([`launcher_is_target`])。
    if !launcher_is_target(app, surface) {
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

/// `Ctrl+A` (既存 name = `select_all`) をランチャーが取るか。取ったら
/// **帯のセルを全部選択**して `true` を返す (呼び側は従来の文脈別全選択へ落とさない)。
///
/// これが無いと帯の上での `Ctrl+A` が `SelectAllClips` に落ち、曲全体 × 全トラックの
/// 時間範囲が張られて `last_edit_select` が範囲面へ移る — 画面上は何も変わらないのに
/// **次の `Delete` がアレンジの全クリップを消す**。
///
/// 対象は曲の **全行** (トラック行 + オートメーションレーン行) のセル全部。
/// 表示の折りたたみを見ないのはアレンジ側の全選択
/// (`select_all_clips` が `song.tracks` を全部見る) と同じ規約。
///
/// レーン行のセルを混ぜられるのは、セル面が
/// [`EditSurface::LauncherCells`] 1 面 1 集合になったから — 面が 2 つに割れて
/// いた頃は混ぜると `set_launcher_cell_selection` がタグを片方へ倒し、続く
/// `Delete` が選んだうちの半分しか消さなかった。
pub(crate) fn select_all_cells_if_launcher(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    surface: Option<EditSurface>,
) -> bool {
    // **ここだけポインタが先**。`Ctrl+A` は選択を*作る*操作なので、既存の文脈別
    // 全選択と同じく「いまどこを指しているか」が文脈 (選択前なので非空集合では
    // 振り分けられない、`root.rs` の `select_all` の doc)。 last-wins だけで判定
    // すると、帯にポインタを置いたままの `Ctrl+A` が `SelectAllClips` に落ちて
    // 曲全体 × 全トラックの範囲が張られ、面が黙って範囲へ移る (画面は変わらない
    // のに、続く `Delete` がアレンジの全クリップを消す)。
    if app.launcher.hover.is_none() && !launcher_owns_surface(surface) {
        return false;
    }
    let scene_ids = app.scene_ids();
    let cells: Vec<_> = app
        .all_launcher_rows()
        .into_iter()
        .flat_map(|row| {
            scene_ids
                .iter()
                .filter_map(move |s| app.cell_in_row_at_scene(row, *s))
                .collect::<Vec<_>>()
        })
        .collect();
    if cells.is_empty() {
        // 帯が対象なのにセルが 1 つも無いときは **何もしない** — ここで `false` を
        // 返すと、結局アレンジ全体の範囲選択に落ちて上記の事故がそのまま起きる。
        return true;
    }
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.set_launcher_cell_selection(&cells);
    }));
    true
}

/// `Ctrl+D` (既存 name = `daw.duplicate_audio_event`) をランチャーが取るか。
///
/// 取るのは **帯が今の操作対象** ([`launcher_is_target`]) でセルを選んでいるとき。
/// 取らなかった場合は呼び側が従来どおりオーディオイベントの複製へ流す。
/// **shortcut は呼び側が 1 度だけ take して bool を渡す**
/// (take は消費するので 2 度読むと 2 回目が必ず false になる)。
pub(crate) fn duplicate_cells_if_launcher(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    surface: Option<EditSurface>,
    pressed: bool,
) -> bool {
    if !pressed || !launcher_is_target(app, surface) {
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
