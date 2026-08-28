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
    let synced: Option<(String, usize, &'static str)> = match surface {
        EditSurface::AudioEvents => app.copy_events_clip().map(|(j, c)| (j, c, "イベント")),
        EditSurface::Notes => app.copy_notes_clip().map(|(j, c)| (j, c, "ノート")),
        EditSurface::AutomationPoints => app
            .copy_points_clip()
            .map(|(j, c)| (j, c, "オートメーションポイント")),
        EditSurface::Clips => app.copy_clips_clip().map(|(j, c)| (j, c, "クリップ")),
        EditSurface::AutomationClips => app
            .copy_automation_clips_clip()
            .map(|(j, c)| (j, c, "オートメーションクリップ")),
        // r.md #71 (プラグインのコピー / 移動): device の copy は **最新 plugin state が
        // 要るので非同期** (トラック copy と同じ round-trip 待ち)。 ここでは `None` に
        // 落とし、実処理は下の `matches!` ブロックが `&mut AppData` 越しに呼ぶ。
        EditSurface::Tracks | EditSurface::Sections | EditSurface::Devices => None,
    };
    if let Some((json, count, label)) = synced {
        ui.set_clipboard_text(json);
        let label = label.to_string();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.status_message = format!("コピー: {count} {label}");
        }));
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

/// Ctrl+X: copy (clipboard へ) + 対象の削除を 1 undo step。clipboard 書込は undo 対象外、
/// 削除イベントが自前で undo snapshot を積む。トラックは `AppData::cut_tracks` で
/// 非同期に copy+削除。
pub(crate) fn cut_for_surface(app: &AppData, ui: &mut Ui<'_, AppData>, surface: Option<EditSurface>) {
    let Some(surface) = surface else {
        return;
    };
    let synced: Option<(String, usize, &'static str)> = match surface {
        EditSurface::AudioEvents => app.copy_events_clip().map(|(j, c)| (j, c, "イベント")),
        EditSurface::Notes => app.copy_notes_clip().map(|(j, c)| (j, c, "ノート")),
        EditSurface::AutomationPoints => app
            .copy_points_clip()
            .map(|(j, c)| (j, c, "オートメーションポイント")),
        EditSurface::Clips => app.copy_clips_clip().map(|(j, c)| (j, c, "クリップ")),
        EditSurface::AutomationClips => app
            .copy_automation_clips_clip()
            .map(|(j, c)| (j, c, "オートメーションクリップ")),
        // r.md #71 (プラグインのコピー / 移動): device の copy は **最新 plugin state が
        // 要るので非同期** (トラック copy と同じ round-trip 待ち)。 ここでは `None` に
        // 落とし、実処理は下の `matches!` ブロックが `&mut AppData` 越しに呼ぶ。
        EditSurface::Tracks | EditSurface::Sections | EditSurface::Devices => None,
    };
    if let Some((json, count, label)) = synced {
        ui.set_clipboard_text(json);
        let del = match surface {
            EditSurface::AudioEvents => AppEvent::DeleteAudioEditorSelection,
            EditSurface::Notes => AppEvent::DeleteSelectedNotes,
            EditSurface::AutomationPoints => AppEvent::DeleteAutomationPoints {
                points: app.selection.selected_automation_points.clone(),
            },
            EditSurface::Clips => AppEvent::DeleteSelectedClip,
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
        P::Tracks(tracks) => {
            if !is_pianoroll_active
                && let Some(above) = app.ui_ephemeral.arrange_hovered_track
            {
                let tracks = crate::clipboard::sanitize_tracks(tracks);
                ui.push_edit(paste_edit("トラック", move |app| {
                    app.paste_tracks_at(tracks, src_pid, above)
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
if dup_shared && !dup_devices {
    // 対象面は copy/cut/delete と同じ last-wins (`edit_surface`)。トラック面なら
    // トラックをリンク複製 (D = 共有、 クリップ複製と同規約)。 それ以外は従来通り。
    if matches!(app.edit_surface(is_pianoroll_active), Some(EditSurface::Tracks)) {
        let ids = app.selection.selected_track_ids.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::DuplicateTracksShared(ids));
        }));
    } else if is_pianoroll_active && !app.selection.selected_notes.is_empty() {
        // ピアノロール上 + ノート選択中なら D = ノート複製。
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::DuplicateSelectedNotes);
        }));
    } else {
        // それ以外 (アレンジ文脈 / ピアノロールでもノート未選択) は選択中の
        // MIDI/Audio/Vocal clip と automation clip の両方を同時に共有複製
        // (Ableton/REAPER 流)。
        let midi_sources: Vec<crate::app::ClipRef> = app.selected_clip_refs();
        let automation_sources: Vec<common::model::AutomationClipKey> =
            app.selection.selected_automation_clips.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if !midi_sources.is_empty() {
                app.handle_event(AppEvent::DuplicateClipsShared(midi_sources));
            }
            if !automation_sources.is_empty() {
                app.handle_event(AppEvent::DuplicateAutomationClipsShared(automation_sources));
            }
        }));
    }
}
if dup_unique && !dup_devices {
    // トラック面なら Alt+D = トラックを独立複製 (クリップ複製と同規約)。
    if matches!(app.edit_surface(is_pianoroll_active), Some(EditSurface::Tracks)) {
        let ids = app.selection.selected_track_ids.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::DuplicateTracksUnique(ids));
        }));
    } else {
        let midi_sources: Vec<crate::app::ClipRef> = app.selected_clip_refs();
        let automation_sources: Vec<common::model::AutomationClipKey> =
            app.selection.selected_automation_clips.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if !midi_sources.is_empty() {
                app.handle_event(AppEvent::DuplicateClipsUnique(midi_sources));
            }
            if !automation_sources.is_empty() {
                app.handle_event(AppEvent::DuplicateAutomationClipsUnique(automation_sources));
            }
        }));
    }
}
}
