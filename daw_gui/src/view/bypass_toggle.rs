//! `Q` (= 「カーソル直下のものを無効化 / 有効化」) の宛先解決と発行。
//!
//! 判定順: マスターパネル → Mixer (Mixer タブ + pointer で門番) → 変調ラック →
//! インスペクタのチェーン行 → オートメーションレーン → ノート → クリップ / 時間範囲。
//! `view/root.rs::dispatch_shortcuts` の Q 節を切り出したもの (サイズ budget、 不変条件 9)。

use daw_ui_core::{Edit, Ui};

use crate::app::{AppData, AppEvent};
use crate::event_device::DeviceEvent;
use crate::state::ModRackHover;

/// `daw.toggle_mute` を消費し、 文脈で決まる対象の mute / bypass を切り替える。
/// 内蔵チャンネルストリップ (docs/plan_channel_strip.md) が Q を先取りする:
/// カーソルが Comp / EQ の上にあればそのセクションのバイパスを切り替え、
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
    if ui.take_shortcut("daw.toggle_mute") && !toggle_hovered_strip_section(app, ui, mixer_active) {
        dispatch_toggle_mute(app, ui, is_pianoroll_active);
    }
}

/// r.md #105: `Q` が bypass 切替する device = **カーソル直下のチェーン行だけ** (S キーの
/// ソロと同じ「カーソルがある行」規則)。 device の選択集合は使わない — チェーン行の
/// 選択は画面上で見分けにくく、 選択優先にすると「別の行を指して押したのに前に click
/// した行が切り替わる」 (実機 2026-09-05)。 空 = device は対象外 (clip / note へ落とす)。
fn q_device_targets(app: &AppData) -> Vec<u64> {
    app.cur.peph.inspector_hovered_device.into_iter().collect()
}

/// Q の対象を文脈で決めて mute / bypass を切り替える (`dispatch_shortcuts` の Q 節、 内蔵
/// ストリップのセクションを先取りした後)。 優先順: 変調ラック (r.md #115) → インスペクタの
/// device → オートメーションレーン → ノート → クリップ / 時間範囲。
fn dispatch_toggle_mute(app: &AppData, ui: &mut Ui<'_, AppData>, is_pianoroll_active: bool) {
    let device_targets = q_device_targets(app);
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
    } else if !device_targets.is_empty() {
        let bypassed = !app.all_devices_bypassed(&device_targets);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Device(DeviceEvent::SetDevicesBypassed {
                device_ids: device_targets,
                bypassed,
            }));
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

/// Q を内蔵チャンネルストリップに割り当てる。カーソルが Comp / EQ のセクション本体か
/// 常設帯の上にあれば、そのセクションのバイパスを切り替えて `true`。対象が無ければ
/// `false` で、呼び出し側は従来どおり note / clip の mute へ進む。
///
/// 対象面の算出は `view::strip_sections` (`mixer_hovered_strip_section`) が SSoT。
fn toggle_hovered_strip_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    mixer_active: bool,
) -> bool {
    use crate::event::{MasterSection, StripEdit, StripSection};
    // マスターパネルは常時描かれるので hover が古くなることはない。ミキサーより
    // 先に見る (パネルは mixer / arrangement のどちらの上にも無く、排他)。
    if let Some(section) = app.cur.peph.master_hovered_section {
        let param = match section {
            MasterSection::Comp => common::model::MasterStripParam::CompOn,
            MasterSection::Eq => common::model::MasterStripParam::EqOn,
            MasterSection::Limiter => common::model::MasterStripParam::LimiterOn,
        };
        let on = app.cur.song_doc.song().master_strip.param(param) >= 0.5;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::MasterStripEdit {
                param,
                value: f32::from(u8::from(!on)),
            });
        }));
        return true;
    }
    // hover 値は strip を描いた frame にしか更新されないので、Mixer タブから
    // 離れた後も最後の値が残る。**タブと pointer 位置で毎回ゲートする**
    // (`mixer_hovered_track` を使う S キーと同じ作法) — 無いと Piano Roll に
    // 切り替えた後の Q がノート mute ではなくストリップ切替になる。
    if !mixer_active {
        return false;
    }
    let Some((track_id, section)) = app.cur.peph.mixer_hovered_strip_section else {
        return false;
    };
    let param = match section {
        StripSection::Comp => common::model::TrackBuiltinParam::StripCompOn,
        StripSection::Eq => common::model::TrackBuiltinParam::StripEqOn,
    };
    let on = app
        .cur.song_doc
        .song()
        .track_by_id(track_id)
        .and_then(|t| t.strip.target_value(&param))
        .unwrap_or(0.0)
        >= 0.5;
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::StripEdit {
            track: track_id,
            edit: StripEdit::Param { param, value: f32::from(u8::from(!on)) },
        });
    }));
    true
}
