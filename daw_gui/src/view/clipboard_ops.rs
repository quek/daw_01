//! Ctrl+C / Ctrl+X / Ctrl+V / D (複製) — **編集面ごとの** クリップボード操作。
//!
//! `root.rs` から分けてあるのは不変条件 9 (サイズ budget) のため。 root.rs は
//! 「画面を割って sub view を呼ぶ」 + 「shortcut を AppEvent に変換する」 だけを持ち、
//! 「どの面の何を、 どこへ貼るのか」 の規則はここ 1 か所に集める。
//!
//! **対象面の解決は `AppData::edit_surface` (last-selection-wins) 1 本**
//! ([[feedback_selection_action_last_wins]])。 copy / cut / delete / paste / 複製が
//! 同じ面を見るので、 「アレンジでクリップを選び直した直後の Ctrl+C が
//! ノートをコピーする」 類のズレが構造的に起きない。
use daw_ui_core::{Edit, Ui};

use crate::app::{AppData, AppEvent, EditSurface};

/// Ctrl+C: 対象面の選択を clipboard envelope にして OS clipboard へ。トラックだけは
/// plugin state 収集が非同期なので `AppData::copy_tracks` 経由 (結果は
/// `pending_clipboard_write` から flush される)。
pub(crate) fn copy_for_surface(app: &AppData, ui: &mut Ui<'_, AppData>, surface: Option<EditSurface>) {
    let Some(surface) = surface else {
        return;
    };
    // r.md #87: 選択が **ランチャーのセル** (`EditSurface::LauncherCells`) なら
    // セルとしてコピーする。貼り先の座標系が (トラック, 拍) ではなく (行, 列) なので
    // payload がアレンジのクリップと別 (`copy_launcher_cells_clip` は面タグを見るので
    // 別の面を選んでいるときは `None` を返す)。
    if let Some((json, count)) = app.copy_launcher_cells_clip() {
        finish_copy(ui, json, count, "セル");
        return;
    }
    let synced: Option<(String, usize, &'static str)> = match surface {
        EditSurface::AudioEvents => app.copy_events_clip().map(|(j, c)| (j, c, "イベント")),
        EditSurface::Notes => app.copy_notes_clip().map(|(j, c)| (j, c, "ノート")),
        EditSurface::AutomationPoints => app
            .copy_points_clip()
            .map(|(j, c)| (j, c, "オートメーションポイント")),
        // 範囲: 形のまま (前後の空白込みで) コピーする。
        EditSurface::TimeRange => app.copy_time_selection_clip().map(|(j, c)| (j, c, "範囲")),
        // セル面はこの関数の頭で捌き済 (`copy_launcher_cells_clip`)。ここへ来るのは
        // 「面はセルだが実在するセルが 1 つも無い」ときだけなので何もしない —
        // アレンジのクリップの copy へ落とすと、帯を触っていたつもりの `Ctrl+C` が
        // 黙って別の面のものを載せる。
        EditSurface::LauncherCells => None,
        EditSurface::AutomationClips => app
            .copy_automation_clips_clip()
            .map(|(j, c)| (j, c, "オートメーションクリップ")),
        // r.md #71 (プラグインのコピー / 移動): device の copy は **最新 plugin state が
        // 要るので非同期** (トラック copy と同じ round-trip 待ち)。 ここでは `None` に
        // 落とし、実処理は下の `matches!` ブロックが `&mut AppData` 越しに呼ぶ。
        // 列 (シーン) は clipboard の payload を持たない (列そのものを他所へ
        // 貼る意味が無く、中身はセル面のコピーで運べる) ので `None`。
        EditSurface::Tracks
        | EditSurface::Sections
        | EditSurface::Scenes
        | EditSurface::Devices => None,
    };
    if let Some((json, count, label)) = synced {
        finish_copy(ui, json, count, label);
        return;
    }
    if matches!(surface, EditSurface::Tracks) {
        let ids = app.selection.selected_track_ids.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.copy_tracks(ids);
        }));
    }
    if matches!(surface, EditSurface::Devices) {
        let ids = app.live_device_ids();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.copy_devices(ids);
        }));
    }
}

/// copy 成立時の共通後始末 (clipboard へ書いて status に件数を出す)。
fn finish_copy(ui: &mut Ui<'_, AppData>, json: String, count: usize, label: &str) {
    ui.set_clipboard_text(json);
    let label = label.to_string();
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.ui_ephemeral.status_message = format!("コピー: {count} {label}");
    }));
}

/// Ctrl+X: copy (clipboard へ) + 対象の削除を 1 undo step。clipboard 書込は undo 対象外、
/// 削除イベントが自前で undo snapshot を積む。トラックは `AppData::cut_tracks` で
/// 非同期に copy+削除。
pub(crate) fn cut_for_surface(app: &AppData, ui: &mut Ui<'_, AppData>, surface: Option<EditSurface>) {
    let Some(surface) = surface else {
        return;
    };
    // r.md #87: セルの cut は「セルとしてコピー + セル削除」。 削除は
    // `DeleteCells` (アレンジのクリップは触らない) を使う。
    if let Some((json, count)) = app.copy_launcher_cells_clip() {
        ui.set_clipboard_text(json);
        let cells = app.selected_launcher_cells();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Launcher(
                crate::event_launcher::LauncherEvent::DeleteCells(cells.clone()),
            ));
            app.ui_ephemeral.status_message = format!("カット: {count} セル");
        }));
        return;
    }
    let synced: Option<(String, usize, &'static str)> = match surface {
        EditSurface::AudioEvents => app.copy_events_clip().map(|(j, c)| (j, c, "イベント")),
        EditSurface::Notes => app.copy_notes_clip().map(|(j, c)| (j, c, "ノート")),
        EditSurface::AutomationPoints => app
            .copy_points_clip()
            .map(|(j, c)| (j, c, "オートメーションポイント")),
        // 範囲: 形のまま (前後の空白込みで) コピーする。
        EditSurface::TimeRange => app.copy_time_selection_clip().map(|(j, c)| (j, c, "範囲")),
        // セル面はこの関数の頭で捌き済 (`copy_launcher_cells_clip`)。ここへ来るのは
        // 「面はセルだが実在するセルが 1 つも無い」ときだけなので何もしない —
        // アレンジのクリップの copy へ落とすと、帯を触っていたつもりの `Ctrl+C` が
        // 黙って別の面のものを載せる。
        EditSurface::LauncherCells => None,
        EditSurface::AutomationClips => app
            .copy_automation_clips_clip()
            .map(|(j, c)| (j, c, "オートメーションクリップ")),
        // r.md #71 (プラグインのコピー / 移動): device の copy は **最新 plugin state が
        // 要るので非同期** (トラック copy と同じ round-trip 待ち)。 ここでは `None` に
        // 落とし、実処理は下の `matches!` ブロックが `&mut AppData` 越しに呼ぶ。
        // 列 (シーン) は clipboard の payload を持たない (列そのものを他所へ
        // 貼る意味が無く、中身はセル面のコピーで運べる) ので `None`。
        EditSurface::Tracks
        | EditSurface::Sections
        | EditSurface::Scenes
        | EditSurface::Devices => None,
    };
    if let Some((json, count, label)) = synced {
        ui.set_clipboard_text(json);
        let del = match surface {
            EditSurface::AudioEvents => AppEvent::DeleteAudioEditorSelection,
            EditSurface::Notes => AppEvent::DeleteSelectedNotes,
            EditSurface::AutomationPoints => AppEvent::DeleteAutomationPoints {
                points: app.selection.selected_automation_points.clone(),
            },
            EditSurface::TimeRange => AppEvent::DeleteTimeSelection,
            EditSurface::AutomationClips => AppEvent::DeleteAutomationClips {
                keys: app.selection.selected_automation_clips.clone(),
            },
            _ => return,
        };
        let label = label.to_string();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(del);
            app.ui_ephemeral.status_message = format!("カット: {count} {label}");
        }));
        return;
    }
    if matches!(surface, EditSurface::Tracks) {
        let ids = app.selection.selected_track_ids.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.cut_tracks(ids);
        }));
    }
    // r.md #71: device の cut は clipboard 書き込みと削除を `cut_devices` が
    // 1 undo step にまとめる (上の `del` match には足さない)。
    if matches!(surface, EditSurface::Devices) {
        let ids = app.live_device_ids();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.cut_devices(ids);
        }));
    }
}

/// Ctrl+V: clipboard envelope を種別で分岐し、**ポインタが合う面の上**でのみマウス位置に
/// 貼る。合わなければ no-op + status (再生ヘッドへの fallback はしない)。
pub(crate) fn paste_from_clipboard(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    text: &str,
    is_pianoroll_active: bool,
) {
    let Some(env) = crate::clipboard::ClipboardEnvelope::from_json(text) else {
        return; // 他アプリ text / 旧 format → 黙って無視
    };
    let src_pid = env.source_project_id;
    use crate::clipboard::ClipboardPayload as P;
    match env.payload {
        P::Notes(notes) => {
            if is_pianoroll_active
                && app.ui_ephemeral.audio_editor_clip.is_none()
                && let Some(at) = app.ui_ephemeral.pianoroll_hover_beat
            {
                let notes = crate::clipboard::sanitize_notes(notes);
                ui.push_edit(paste_edit("ノート", move |app| app.paste_notes_at(notes, at)));
                return;
            }
            paste_noop(ui);
        }
        P::AudioEvents(events) => {
            if is_pianoroll_active
                && app.ui_ephemeral.audio_editor_clip.is_some()
                && let Some(at) = app.ui_ephemeral.audio_editor_hover_beat_in_clip
            {
                let events = crate::clipboard::sanitize_audio_events(events);
                ui.push_edit(paste_edit("イベント", move |app| app.paste_events_at(events, at)));
                return;
            }
            paste_noop(ui);
        }
        P::AutomationPoints(points) => {
            if let (Some(lane), Some(at)) = (
                app.ui_ephemeral.arrange_hovered_automation_lane,
                app.ui_ephemeral.arrangement_hover_beat,
            ) {
                let points = crate::clipboard::sanitize_points(points);
                ui.push_edit(paste_edit("オートメーションポイント", move |app| {
                    app.paste_points_at(points, lane, at)
                }));
                return;
            }
            paste_noop(ui);
        }
        P::Clips(clips) => {
            if !is_pianoroll_active
                && app.ui_ephemeral.arrange_hovered_automation_lane.is_none()
                && let (Some(track), Some(at)) =
                    (app.ui_ephemeral.arrange_hovered_track, app.ui_ephemeral.arrangement_hover_beat)
            {
                let clips = crate::clipboard::sanitize_clips(clips);
                ui.push_edit(paste_edit("クリップ", move |app| {
                    app.paste_clips_at(clips, src_pid, track, at)
                }));
                return;
            }
            paste_noop(ui);
        }
        P::AutomationClips(clips) => {
            // automation clip は「マウス下の automation lane」へ、 hover 拍を基準に貼る
            // (= automation point paste と同じく lane + beat が揃ったときのみ)。
            if let (Some(lane), Some(at)) = (
                app.ui_ephemeral.arrange_hovered_automation_lane,
                app.ui_ephemeral.arrangement_hover_beat,
            ) {
                let clips = crate::clipboard::sanitize_automation_clips(clips);
                ui.push_edit(paste_edit("オートメーションクリップ", move |app| {
                    app.paste_automation_clips_at(clips, lane, at)
                }));
                return;
            }
            paste_noop(ui);
        }
        P::LauncherCells(cells) => {
            // r.md #87: 貼り先は **ポインタが乗っているセル** (行 × 列)。
            // ランチャーの上にポインタが無ければ貼らない (アレンジの paste と
            // 同じ規約 — 再生ヘッドや先頭列への fallback はしない)。
            if let Some(dest) = app.launcher.hover {
                let cells = crate::clipboard::sanitize_launcher_cells(cells);
                ui.push_edit(paste_edit("セル", move |app| {
                    app.paste_launcher_cells(cells, src_pid, dest)
                }));
                return;
            }
            paste_noop(ui);
        }
        P::Tracks(payload) => {
            if !is_pianoroll_active
                && let Some(above) = app.ui_ephemeral.arrange_hovered_track
            {
                let payload = crate::clipboard::sanitize_tracks(payload);
                ui.push_edit(paste_edit("トラック", move |app| {
                    app.paste_tracks_at(payload, src_pid, above)
                }));
                return;
            }
            paste_noop(ui);
        }
        P::Devices(devices) => {
            // r.md #71 (プラグインのコピー / 移動): 貼り先は「いまインスペクタに
            // 出ているチェーン」。 挿入位置は **選んでいるプラグインの直前**、
            // 選択が無ければ末尾 (Ableton 流)。
            if let Some(dest_track) = app.cursor_track_id() {
                let devices = crate::clipboard::sanitize_devices(devices);
                ui.push_edit(paste_edit("プラグイン", move |app| {
                    app.paste_devices(devices, dest_track)
                }));
                return;
            }
            paste_noop(ui);
        }
    }
}

/// 貼り付け 1 件分の `Edit`。 **`paste` が 0 を返したら status を出さない** —
/// 「貼れなかった」 のに成功メッセージが出ると、 何が起きたのか分からなくなる。
fn paste_edit(
    noun: &'static str,
    paste: impl FnOnce(&mut AppData) -> usize + Send + 'static,
) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        let n = paste(app);
        if n > 0 {
            app.ui_ephemeral.status_message = format!("貼り付け: {n} {noun}");
        }
    })
}

/// r.md #71 (プラグインのコピー / 移動): 選択中の device を **各 device の直後**に
/// 複製する (D / Alt+D / 右クリックメニューの「複製」)。
///
/// リンク / 独立の区別は device には無い (プラグインインスタンスは共有できない) ので、
/// D と Alt+D はどちらもここへ来る。
pub(crate) fn duplicate_devices(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let ids = app.live_device_ids();
    if ids.is_empty() {
        return;
    }
    let Some(dest_track) = app.cursor_track_id() else {
        return;
    };
    // 挿入位置 = 選択の末尾 device の直後 (選択ブロック全体の後ろに並べる)。
    let Some(dest_index) = app
        .song_doc
        .song()
        .fx_chain_by_track_id(dest_track)
        .and_then(|c| c.iter().rposition(|d| ids.contains(&d.id)))
        .map(|i| i as u32 + 1)
    else {
        return;
    };
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::RelocateDevices(crate::app::RelocateDevices {
            device_ids: ids,
            dest_track,
            dest_index,
            copy: true,
        }));
    }));
}

fn paste_noop(ui: &mut Ui<'_, AppData>) {
    ui.push_edit(Edit::mutate(|app: &mut AppData| {
        app.ui_ephemeral.status_message =
            "ここには貼り付けできません (貼り先の上にカーソルを置いてください)".to_string();
    }));
}

/// D / Alt+D — **編集面ごとの複製**。 対象面の解決は copy / cut / paste と同じ
/// `edit_surface` (last-selection-wins)。
///
/// device 面だけは D と Alt+D が同じ動作になる (**リンク / 独立の区別が device に
/// 無い** — プラグインインスタンスは共有できない)。
pub(crate) fn dispatch_duplicate(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    is_pianoroll_active: bool,
) {
// ----- Clip duplicate (gui_01 #019) -----
// D / Alt+D で選択中 clip 群をまとめて共有/独立コピー。 選択ブロック全体の
// span だけ後ろにずらして相対位置を保ったまま複製する (REAPER / Ableton の
// Ctrl+D 流、 Ctrl+drag と同じセマンティクス)。 複製は全選択になり連打で
// 後方連鎖。 selected が空なら no-op。
// shortcut は **1 度だけ** take する (take は消費するので、同じ名前を 2 回読むと
// 2 回目が必ず false になる)。
let dup_shared = ui.take_shortcut("daw.duplicate_clip_shared");
let dup_unique = ui.take_shortcut("daw.duplicate_clip_unique");
// r.md #71 (プラグインのコピー / 移動): device 面は D / Alt+D で同じ動作
// (**リンク / 独立の区別は device に無い** — プラグインインスタンスは共有できない)。
// 面の判定を 2 か所に書き分けないよう、ここで 1 回だけ裁く。
let dup_devices = (dup_shared || dup_unique)
    && matches!(app.edit_surface(is_pianoroll_active), Some(EditSurface::Devices));
if dup_devices {
    duplicate_devices(app, ui);
}
// r.md #87: ランチャーのセルを選んでいるなら D / Alt+D はセルの複製
// (共有 / 独立の区別はアレンジのクリップと同じ)。 セルは `EditSurface::LauncherCells` を
// 共有するので、ここで先に裁かないと下の clip 複製が `selected_clip_refs()` の
// 空リストで無音の no-op になる。
let launcher_cells = if dup_devices { Vec::new() } else { app.selected_launcher_cells() };
if (dup_shared || dup_unique) && !launcher_cells.is_empty() {
    let unique = dup_unique;
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::Launcher(
            crate::event_launcher::LauncherEvent::DuplicateCells {
                cells: launcher_cells.clone(),
                unique,
            },
        ));
    }));
    return;
}
// r.md #108: ランチャーの列 (シーン) を選んでいるなら D / Alt+D は列ごと複製。
// 列面はセル面と `SelectionState` 上で排他なので、上のセル分岐とは重ならない。
if (dup_shared || dup_unique)
    && matches!(app.edit_surface(is_pianoroll_active), Some(EditSurface::Scenes))
{
    let scene_ids = app.selection.selected_scene_ids.clone();
    let unique = dup_unique;
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::Launcher(
            crate::event_launcher::LauncherEvent::DuplicateScenes { scene_ids, unique },
        ));
    }));
    return;
}
if dup_shared && !dup_devices {
    // 対象面は copy/cut/delete と同じ last-wins (`edit_surface`)。トラック面なら
    // トラックをリンク複製 (D = 共有、 クリップ複製と同規約)。 それ以外は従来通り。
    if matches!(app.edit_surface(is_pianoroll_active), Some(EditSurface::Tracks)) {
        let ids = app.selection.selected_track_ids.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::DuplicateTracksShared(ids));
        }));
    } else if is_pianoroll_active
        && matches!(app.time_selection_surface(), Some(EditSurface::Notes))
    {
        // ピアノロール上で範囲が鍵盤行に掛かっているなら D = **範囲の複製**
        // (`docs/plan_range_selection.md` §6)。 範囲に音が 1 つも無くても、
        // 「クリップ全体を複製」へ落ちてはいけない (面が違う)。
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::DuplicateSelectedNotes);
        }));
    } else {
        // それ以外 (アレンジ文脈) は **範囲を 1 つ後ろへ複製**する
        // (`docs/plan_range_selection.md` §6)。 複製する対象も、送る量も、複製後の
        // 選択も範囲 1 本で決まる。 クリップ集合から外接 span を出していた旧実装は、
        // 行き先に元から居たクリップを次の D で巻き込んで雪だるまになっていた。
        duplicate_time_range(app, ui, false);
    }
}
if dup_unique && !dup_devices {
    // トラック面なら Alt+D = トラックを独立複製 (クリップ複製と同規約)。
    if matches!(app.edit_surface(is_pianoroll_active), Some(EditSurface::Tracks)) {
        let ids = app.selection.selected_track_ids.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::DuplicateTracksUnique(ids));
        }));
    } else if is_pianoroll_active
        && matches!(app.time_selection_surface(), Some(EditSurface::Notes))
    {
        // ノートに「リンク / 独立」の区別は無いので Alt+D も D と同じ (device 面と同規約)。
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::DuplicateSelectedNotes);
        }));
    } else {
        duplicate_time_range(app, ui, true);
    }
}
}

/// アレンジャーの `D` / `Alt+D` = **範囲を 1 つ後ろへ複製**する
/// (`docs/plan_range_selection.md` §6)。
///
/// 送る量は範囲の長さ。 行き先は上書き規則で削られるので、複製後の範囲には
/// **複製したものしか居ない** — 次の `D` が元から居たクリップを巻き込まない。
/// 選択中の automation クリップ (範囲に畳まれていない唯一の面) は従来どおり別に複製する。
fn duplicate_time_range(app: &AppData, ui: &mut Ui<'_, AppData>, unique: bool) {
    let automation_sources: Vec<common::model::AutomationClipKey> =
        app.selection.selected_automation_clips.clone();
    let Some(sel) = app.time_selection() else {
        return;
    };
    let (a, b) = (sel.start_beat, sel.end_beat);
    let map: Vec<(u32, u32)> = sel.track_row_ids().map(|id| (id, id)).collect();
    let offset = b - a;
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        // クリップと automation で `edit_song` が 2 回走るので 1 undo step に畳む。
        app.song_doc.begin_gesture();
        app.copy_time_range(a, b, offset, &map, unique);
        if !automation_sources.is_empty() {
            let ev = if unique {
                AppEvent::DuplicateAutomationClipsUnique(automation_sources.clone())
            } else {
                AppEvent::DuplicateAutomationClipsShared(automation_sources.clone())
            };
            app.handle_event(ev);
        }
        app.song_doc.end_gesture();
    }));
}
