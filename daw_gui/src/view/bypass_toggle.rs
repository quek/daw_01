//! `Q` (= 「カーソル直下のものを無効化 / 有効化」) の宛先解決と発行。
//!
//! 判定順: マスターパネル → Mixer (Mixer タブ + pointer で門番) → 変調ラック →
//! インスペクタのチェーン行 → オートメーションレーン → ノート → クリップ / 時間範囲。
//! `view/root.rs::dispatch_shortcuts` の Q 節を切り出したもの (サイズ budget、 不変条件 9)。

use daw_ui_core::{Edit, Ui};

use crate::app::{AppData, AppEvent};
use crate::state::ModRackHover;

/// `daw.toggle_mute` を消費し、 文脈で決まる対象の mute / bypass を切り替える。
/// マスターパネル / Mixer 帯の内蔵 device (r.md #129) が Q を先取りする:
/// カーソルがその上にあればその device (または master Limiter) の ON/OFF を切り替え、
/// 下の clip / note の mute へは落とさない。
///
/// `mixer_active` = Mixer タブが選ばれていて pointer が下部パネル内、
/// `is_pianoroll_active` = Piano Roll タブが選ばれていて pointer が下部パネル内。
pub(super) fn dispatch(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    mixer_active: bool,
    is_pianoroll_active: bool,
) {
    if !ui.take_shortcut("daw.toggle_mute") {
        return;
    }
    match app.hovered_bypass_target(mixer_active) {
        Some(target) => {
            let event = app.bypass_toggle_event(target);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(event);
            }));
        }
        None => dispatch_toggle_mute(app, ui, is_pianoroll_active),
    }
}

/// Q の対象を文脈で決めて mute / bypass を切り替える (`dispatch_shortcuts` の Q 節、 マスター
/// パネル / Mixer 帯を先取りした後)。 優先順: 変調ラック (r.md #115) → インスペクタの
/// 行 → オートメーションレーン → ノート → クリップ / 時間範囲。
fn dispatch_toggle_mute(app: &AppData, ui: &mut Ui<'_, AppData>, is_pianoroll_active: bool) {
    // r.md #105: `Q` が bypass 切替する device = **カーソル直下のチェーン行だけ** (S キーの
    // ソロと同じ「カーソルがある行」規則)。 device の選択集合は使わない — チェーン行の
    // 選択は画面上で見分けにくく、 選択優先にすると「別の行を指して押したのに前に click
    // した行が切り替わる」 (実機 2026-09-05)。
    if let Some(hover) = app.cur.peph.inspector_hovered_mod {
        // r.md #115: ポインタ下のモジュレーター (ヘッダ / 本体) または routing 行を
        // バイパス切替。 ラックにボタンは無く、 これが唯一の到達手段 (レーンと同じ)。
        let song = app.cur.song_doc.song();
        let event = match hover {
            ModRackHover::Source(id) => {
                let enabled = song.mod_sources.iter().find(|m| m.id == id).is_some_and(|m| m.enabled);
                AppEvent::SetModSourceEnabled { id, enabled: !enabled }
            }
            ModRackHover::Routing(routing_id) => {
                let enabled = song.mod_routing_by_id(routing_id).is_some_and(|r| r.enabled);
                AppEvent::SetModRoutingEnabled { routing_id, enabled: !enabled }
            }
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(event);
        }));
    } else if let Some(row) = app.cur.peph.inspector_hovered_row {
        // Rack の行 (Par パネル込みの高さ) → その device / master Limiter の ON/OFF。
        let event = app.bypass_toggle_event(row);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(event);
        }));
    } else if let Some(lane) = app.cur.peph.arrange_hovered_automation_lane {
        // ポインタ下のオートメーションレーン (本体 / ヘッダ) をバイパス切替。
        // ヘッダにボタンは無く、これが唯一の到達手段。
        let enabled = app
            .cur.song_doc
            .song()
            .automation_lane_by_key(lane.track, lane.lane)
            .is_some_and(|l| l.enabled);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetLaneEnabled {
                track_id: lane.track,
                lane_id: lane.lane,
                enabled: !enabled,
            });
        }));
    } else if is_pianoroll_active && app.cur.peph.audio_editor_clip.is_none() {
        // note 群は packed note id (`selected_notes` / `pianoroll_hover_note` は
        // 表示中全クリップに跨る packed id)。所属クリップは handler が decode するので、
        // ここで単一 anchor clip に縛らない (複数クリップ同時 mute を保つ)。
        let notes: Vec<u32> = if !app.selected_note_ids().is_empty() {
            app.selected_note_ids()
        } else {
            app.cur.peph.pianoroll_hover_note.into_iter().collect()
        };
        if !notes.is_empty() {
            let new_muted = !app.all_notes_muted(&notes);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetNotesMuted {
                    notes,
                    muted: new_muted,
                });
            }));
        }
    } else if !is_pianoroll_active && app.cur.selection.time.is_some() {
        // 範囲が立っていれば **範囲操作** — 境界で分割して範囲部分だけをミュートする
        // (Live §6.9 "deactivates a selection of material"、
        // `docs/plan_range_selection.md` §8)。
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.apply_mute_time_selection();
        }));
    } else {
        let targets: Vec<crate::app::ClipKey> = if is_pianoroll_active {
            // audio waveform editor を開いている: その clip を mute。
            app.cur.peph.audio_editor_clip.into_iter().collect()
        } else {
            app.cur.peph.arrangement_hover_clip.into_iter().collect()
        };
        if !targets.is_empty() {
            let new_muted = !app.all_clips_muted(&targets);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetClipsMuted {
                    targets,
                    muted: new_muted,
                });
            }));
        }
    }
}
