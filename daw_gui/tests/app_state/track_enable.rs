//! r.md #131: トラックの無効化 / 有効化 (`docs/plan_rmd_131_track_disable.md`)。
//!
//! 無効化は plugin を host から降ろすので、**降ろす前に最新の state を Song に書き戻す** (削除系と同じ
//! `RequestAllStates` の往復)。有効化 / undo / redo は host の load 状態を Song に追従させ、降ろした
//! device は Song の state 付きで載せ直す。ここが崩れると「無効 → 有効でツマミが初期値に戻る」か
//! 「無効なのに host に残って CPU を使う」になる。

use common::protocol::{PluginCommand, PluginEvent, SlotState};

use daw_gui::app::AppEvent;

use super::support::{build_app, drain, load_instrument};

fn set_slot_states(msgs: &[PluginCommand], device_id: u64) -> Vec<Option<Vec<u8>>> {
    msgs.iter()
        .filter_map(|m| match m {
            PluginCommand::SetSlotPlugin { device, initial_state, .. } if device.device_id == device_id => {
                Some(initial_state.clone())
            }
            _ => None,
        })
        .collect()
}

fn removes(msgs: &[PluginCommand], device_id: u64) -> usize {
    msgs.iter()
        .filter(|m| matches!(m, PluginCommand::RemoveSlotPlugin { device } if device.device_id == device_id))
        .count()
}

#[test]
fn 無効化は_state_を書き戻してから降ろし_有効化と_undo_redo_で_state_付きで追従する() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    load_instrument(&mut app);
    let track_id = app.cur.song_doc.song().tracks[0].id;
    let device_id = app.cur.song_doc.song().tracks[0].plugins().next().expect("synth").id;
    assert!(app.cur.pipc.loaded_devices.contains_key(&device_id), "前提: host に載っている");
    drain(&mut plugin_rx);

    // 無効化: まず state の往復だけ (まだ降ろさない、Song もまだ有効)。
    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![track_id], enabled: false });
    let msgs = drain(&mut plugin_rx);
    assert!(msgs.iter().any(|m| matches!(m, PluginCommand::RequestAllStates { .. })), "{msgs:?}");
    assert_eq!(removes(&msgs, device_id), 0, "state を書き戻す前に降ろさない: {msgs:?}");
    assert!(app.cur.song_doc.song().track_effectively_enabled(track_id));

    // host の最新 state が届く → Song に書き戻してから無効化 + 降ろす。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates {
        project: app.pk(),
        entries: vec![SlotState { device_id, data: Some(vec![7, 7, 7]), ara_archive: None, error: None }],
    }));
    let msgs = drain(&mut plugin_rx);
    assert!(!app.cur.song_doc.song().track_effectively_enabled(track_id));
    assert_eq!(removes(&msgs, device_id), 1, "{msgs:?}");
    assert!(!app.cur.pipc.loaded_devices.contains_key(&device_id));
    let saved = app.cur.song_doc.song().plugin_by_id(device_id).and_then(|p| p.state.as_deref().map(<[u8]>::to_vec));
    assert_eq!(saved, Some(vec![7, 7, 7]), "降ろした device の state は Song に残る");

    // 有効化 (即時): Song の state 付きで載せ直す。
    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![track_id], enabled: true });
    let msgs = drain(&mut plugin_rx);
    assert_eq!(set_slot_states(&msgs, device_id), vec![Some(vec![7, 7, 7])], "{msgs:?}");

    // undo (= 無効へ戻る): 応答待ちの load も居るべきでないので降ろす。
    app.handle_event(AppEvent::Undo);
    let msgs = drain(&mut plugin_rx);
    assert!(!app.cur.song_doc.song().track_effectively_enabled(track_id));
    assert_eq!(removes(&msgs, device_id), 1, "{msgs:?}");
    assert!(!app.cur.pipc.pending_plugin_loads.contains_key(&device_id));

    // redo (= 有効へ): もう一度 state 付きで載せる。
    app.handle_event(AppEvent::Redo);
    let msgs = drain(&mut plugin_rx);
    assert!(app.cur.song_doc.song().track_effectively_enabled(track_id));
    assert_eq!(set_slot_states(&msgs, device_id), vec![Some(vec![7, 7, 7])], "{msgs:?}");
}

/// 無効な vocal トラックの歌唱メタデータは送らない — 送ると plugin host の builtin VOICEVOX が synth thread を
/// 自動起動して合成を始める。host の帳簿にまだ device が居る瞬間 (無効化の edit と unload の間) でも同じ。
#[test]
fn 無効な_vocal_トラックの歌唱メタデータは送らない() {
    use common::model::{Clip, ClipContent, Device, MidiContent, Note, PluginInstance, Track};
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    let (track_id, device_id) = (100_u32, 5_u64);
    app.edit_song(|song| {
        let cid = song.alloc_content_id();
        let note = Note { id: 1, start_beat: 0.0, duration_beats: 1.0, pitch: 60, velocity: 100, lyric: Some("ら".into()), muted: false };
        song.clip_contents.insert(cid, ClipContent::Midi(MidiContent { notes: vec![note], next_note_id: 2 }));
        let mut track = Track { id: track_id, ..Track::default() };
        track.devices.push(Device::Plugin(PluginInstance {
            id: device_id,
            ..PluginInstance::with_ports(
                common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
                common::plugin_format::PluginFormat::Builtin,
                common::port_config::PortConfig { has_note_input: true, has_audio_output: true, ..Default::default() },
            )
        }));
        track.clips.push(Clip { id: 1, length_beats: 4.0, content_id: cid, ..Clip::default() });
        track.enabled = false;
        song.tracks.push(track);
    });
    app.cur.pipc.loaded_devices.insert(
        device_id,
        daw_gui::app::LoadedDeviceInfo {
            plugin_id_str: common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
            token: common::protocol::InstanceToken(1),
        },
    );
    let metadata = |msgs: &[PluginCommand]| {
        msgs.iter().filter(|m| matches!(m, PluginCommand::SetBuiltinPluginNoteMetadata { .. })).count()
    };
    drain(&mut plugin_rx);
    app.flush_song_sync();
    assert_eq!(metadata(&drain(&mut plugin_rx)), 0, "無効な vocal には送らない");

    // 対照: 有効に戻すと送る (device は帳簿に居るので送れる状態)。
    let _ = app.edit_song(|song| song.track_by_id_mut(track_id).expect("vocal").enabled = true);
    app.flush_song_sync();
    assert_eq!(metadata(&drain(&mut plugin_rx)), 1);
}

/// 無効な group の中へ移したトラックの plugin も降りる (state を書き戻してから)。group を解けば載せ直す。
#[test]
fn 無効な_group_へ移すと降り_解くと戻る() {
    let (mut app, _audio_rx, mut plugin_rx, _d) = build_app();
    load_instrument(&mut app);
    let child = app.cur.song_doc.song().tracks[0].id;
    let device_id = app.cur.song_doc.song().tracks[0].plugins().next().expect("synth").id;
    app.handle_event(AppEvent::AddInstrumentTrack);
    let group = app.cur.song_doc.song().tracks.iter().map(|t| t.id).find(|&id| id != child).expect("group");
    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![group], enabled: false });
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));
    drain(&mut plugin_rx);
    assert!(!app.cur.song_doc.song().track_effectively_enabled(group), "前提: 無効な group");

    app.handle_event(AppEvent::SetTrackParent { track_ids: vec![child], parent_id: Some(group), anchor_after: None });
    assert_eq!(removes(&drain(&mut plugin_rx), device_id), 0, "state の往復が済むまで降ろさない");
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));
    assert!(!app.cur.song_doc.song().track_effectively_enabled(child), "無効な group の子は実効的に無効");
    assert!(app.cur.song_doc.song().track_by_id(child).is_some_and(|t| t.enabled), "子自身の値は有効のまま");
    assert_eq!(removes(&drain(&mut plugin_rx), device_id), 1);

    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![group], enabled: true });
    assert_eq!(set_slot_states(&drain(&mut plugin_rx), device_id).len(), 1, "group を有効に戻すと子の plugin を載せる");
}
