//! r.md #131: トラックの無効化 / 有効化 (`docs/plan_rmd_131_track_disable.md`)。
//!
//! 無効化は plugin を host から降ろすので、**降ろす前に最新の state を Song に書き戻す** (削除系と同じ
//! `RequestAllStates` の往復)。有効化 / undo / redo は host の load 状態を Song に追従させ、降ろした
//! device は Song の state 付きで載せ直す。ここが崩れると「無効 → 有効でツマミが初期値に戻る」か
//! 「無効なのに host に残って CPU を使う」になる。

use common::protocol::{AudioCommand, PluginCommand, PluginEvent, SlotState};

use daw_gui::app::{AppData, AppEvent};

use super::support::{build_app, drain, fake_plugin_loaded, load_instrument, select_track_single};

/// engine へ送った「読み込み中の device」(`SetLoadingDevices`) の位置と中身。
fn loading_sets(msgs: &[AudioCommand]) -> Vec<(usize, Vec<u64>)> {
    msgs.iter()
        .enumerate()
        .filter_map(|(i, m)| match m {
            AudioCommand::SetLoadingDevices { device_ids, .. } => Some((i, device_ids.clone())),
            _ => None,
        })
        .collect()
}

/// `track_id` が有効 / 無効で届いた `LoadSong` の位置。
fn load_song_at(msgs: &[AudioCommand], track_id: u32, enabled: bool) -> Option<usize> {
    msgs.iter().position(|m| {
        matches!(m, AudioCommand::LoadSong { song, .. } if song.track_by_id(track_id).is_some_and(|t| t.enabled == enabled))
    })
}

fn position(msgs: &[AudioCommand], pred: impl Fn(&AudioCommand) -> bool) -> Option<usize> {
    msgs.iter().position(pred)
}

/// 楽器を載せた track 0 を、再生中の状態で無効化まで済ませる (state の往復も)。`(track_id, device_id)`。
fn disabled_while_playing(app: &mut AppData) -> (u32, u64) {
    load_instrument(app);
    let track_id = app.cur.song_doc.song().tracks[0].id;
    let device_id = app.cur.song_doc.song().tracks[0].plugins().next().expect("synth").id;
    app.cur.transport.is_playing = true;
    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![track_id], enabled: false });
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));
    (track_id, device_id)
}

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

/// 再生中に有効へ戻す: 再生は止めず、engine には **トラックを実行に入れる構造 (LoadSong) より前に** その plugin が
/// 読み込み中だと届け (engine は読み込みが確定するまでグラフに入れない = FX の掛かっていない音を出さない)、
/// 登録 (`OpenPluginShmem`) の後に外す = そこから鳴る。
#[test]
fn 再生中に有効へ戻すと再生を止めず_読み込み中を構造より先に届け_登録の後に外す() {
    let (mut app, mut audio_rx, mut plugin_rx, _d) = build_app();
    let (track_id, device_id) = disabled_while_playing(&mut app);
    let msgs = drain(&mut audio_rx);
    let disabled = load_song_at(&msgs, track_id, false).expect("無効化の構造");
    let close = position(&msgs, |m| matches!(m, AudioCommand::ClosePluginShmem { device_id: d, .. } if *d == device_id));
    assert!(close.is_some_and(|c| disabled < c), "無効化は構造を届けてから plugin を降ろす (素通しで鳴らさない): {msgs:?}");
    drain(&mut plugin_rx);

    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![track_id], enabled: true });
    let msgs = drain(&mut audio_rx);
    let enabled = load_song_at(&msgs, track_id, true).expect("有効化の構造");
    let declared = loading_sets(&msgs).into_iter().find(|(_, ids)| ids.contains(&device_id));
    assert!(declared.is_some_and(|(i, _)| i < enabled), "読み込み中は構造より先に届く: {msgs:?}");
    assert!(position(&msgs, |m| matches!(m, AudioCommand::Stop { .. })).is_none(), "有効に戻しても再生は止めない: {msgs:?}");
    assert_eq!(app.cur.transport.pending_play, None);
    assert_eq!(set_slot_states(&drain(&mut plugin_rx), device_id).len(), 1);

    // 対照: 鳴っているトラックへ plugin を足すのは従来どおり止める (有効化の読み込みが応答待ちでも)。
    app.handle_event(AppEvent::AddInstrumentTrack);
    let other = app.cur.song_doc.song().tracks.len() - 1;
    select_track_single(&mut app, other);
    app.handle_event(AppEvent::SelectPluginFromDb { id: "test.fx".into(), keep_open: false, open_gui: false });
    assert!(drain(&mut audio_rx).iter().any(|m| matches!(m, AudioCommand::Stop { .. })), "plugin を足すと読み込みの間は止める");

    fake_plugin_loaded(&mut app, track_id, 0, "test.synth");
    let msgs = drain(&mut audio_rx);
    let open = position(&msgs, |m| matches!(m, AudioCommand::OpenPluginShmem { device_id: d, .. } if *d == device_id));
    let settled = loading_sets(&msgs).into_iter().find(|(_, ids)| !ids.contains(&device_id));
    assert!(open.zip(settled).is_some_and(|(o, (s, _))| o < s), "登録の後に読み込み中から外す: {msgs:?}");
}

/// 読み込みが失敗で確定しても読み込み中から外す (その device は素通しで鳴る = 失敗した device の規則)。
#[test]
fn 有効化の読み込みが失敗で確定すると読み込み中から外す() {
    let (mut app, mut audio_rx, _plugin_rx, _d) = build_app();
    let (track_id, device_id) = disabled_while_playing(&mut app);
    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![track_id], enabled: true });
    drain(&mut audio_rx);
    assert!(app.cur.pipc.last_sent_loading_devices.contains(&device_id), "前提: 読み込み中を届けてある");

    let generation = app.cur.pipc.pending_plugin_loads[&device_id];
    app.handle_event(AppEvent::Plugin(PluginEvent::SlotPluginLoadFailed {
        device: app.dev(device_id),
        plugin_id: "test.synth".into(),
        reason: "boom".into(),
        generation,
    }));
    let msgs = drain(&mut audio_rx);
    assert!(loading_sets(&msgs).iter().any(|(_, ids)| !ids.contains(&device_id)), "{msgs:?}");
    assert!(app.cur.pipc.failed_plugin_loads.contains_key(&device_id));
}

/// undo / redo で有効へ戻る場合も同じ規則 (止めない / 読み込み中が構造より先)。無効へ戻る redo は構造を届けてから降ろす。
#[test]
fn undo_redo_で有効へ戻るときも読み込み中を構造より先に届け_無効へ戻るときは構造の後に降ろす() {
    let (mut app, mut audio_rx, _plugin_rx, _d) = build_app();
    let (track_id, device_id) = disabled_while_playing(&mut app);
    drain(&mut audio_rx);

    app.handle_event(AppEvent::Undo);
    let msgs = drain(&mut audio_rx);
    assert!(app.cur.song_doc.song().track_effectively_enabled(track_id), "前提: undo で有効へ戻る");
    let enabled = load_song_at(&msgs, track_id, true).expect("構造");
    assert!(loading_sets(&msgs).iter().any(|&(i, ref ids)| i < enabled && ids.contains(&device_id)), "{msgs:?}");
    assert!(position(&msgs, |m| matches!(m, AudioCommand::Stop { .. })).is_none(), "{msgs:?}");

    app.handle_event(AppEvent::Redo);
    let msgs = drain(&mut audio_rx);
    let disabled = load_song_at(&msgs, track_id, false).expect("構造");
    let close = position(&msgs, |m| matches!(m, AudioCommand::ClosePluginShmem { device_id: d, .. } if *d == device_id));
    assert!(close.is_some_and(|c| disabled < c), "{msgs:?}");
    assert!(!app.cur.pipc.last_sent_loading_devices.contains(&device_id), "降ろした読み込みは外す");
}

/// プロジェクトを開いた直後の全読み込みも同じ規則: 開いた曲の plugin は、その曲の構造 (LoadSong) より先に読み込み中と
/// して届く。無効トラックの plugin は読み込まないので含まない。
#[test]
fn プロジェクトを開くと読み込む_plugin_を構造より先に読み込み中として届ける() {
    use common::model::{Device, PluginInstance, Track};
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("proj.daw");
    let (mut app, mut audio_rx, _plugin_rx, _d) = build_app();
    load_instrument(&mut app);
    let synth = app.cur.song_doc.song().tracks[0].plugins().next().expect("synth").id;
    let parked = app
        .edit_song(|song| {
            let id = song.alloc_device_id();
            let fx = PluginInstance { id, ..PluginInstance::new("test.fx".into(), common::plugin_format::PluginFormat::Clap) };
            let track = Track { id: song.alloc_track_id(), enabled: false, devices: vec![Device::Plugin(fx)], ..Track::default() };
            song.tracks.push(track);
            id
        })
        .expect("edit");
    common::project::save(&proj, app.cur.song_doc.song()).expect("write project file");
    app.cur.song_doc.mark_saved();
    app.flush_song_sync();
    drain(&mut audio_rx);

    app.handle_event(AppEvent::OpenRecent(proj));
    app.flush_song_sync();
    let msgs = drain(&mut audio_rx);
    let load = position(&msgs, |m| matches!(m, AudioCommand::LoadSong { .. })).expect("開いた曲の構造");
    let declared: Vec<u64> = loading_sets(&msgs).into_iter().filter(|&(i, _)| i < load).flat_map(|(_, ids)| ids).collect();
    assert!(declared.contains(&synth), "開いた曲の plugin は構造より先に読み込み中: {msgs:?}");
    assert!(!declared.contains(&parked), "無効トラックの plugin は読み込まない: {msgs:?}");
}

/// group を有効に戻すと、子の plugin も読み込み中として構造より先に届く (engine は group の子孫も待たせる)。
#[test]
fn group_を有効に戻すと子の読み込み中も構造より先に届く() {
    let (mut app, mut audio_rx, _plugin_rx, _d) = build_app();
    load_instrument(&mut app);
    let child = app.cur.song_doc.song().tracks[0].id;
    let device_id = app.cur.song_doc.song().tracks[0].plugins().next().expect("synth").id;
    app.handle_event(AppEvent::AddInstrumentTrack);
    let group = app.cur.song_doc.song().tracks.iter().map(|t| t.id).find(|&id| id != child).expect("group");
    app.handle_event(AppEvent::SetTrackParent { track_ids: vec![child], parent_id: Some(group), anchor_after: None });
    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![group], enabled: false });
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { project: app.pk(), entries: Vec::new() }));
    app.cur.transport.is_playing = true;
    drain(&mut audio_rx);

    app.handle_event(AppEvent::SetTracksEnabled { track_ids: vec![group], enabled: true });
    let msgs = drain(&mut audio_rx);
    let enabled = load_song_at(&msgs, group, true).expect("構造");
    assert!(loading_sets(&msgs).iter().any(|&(i, ref ids)| i < enabled && ids.contains(&device_id)), "{msgs:?}");
    assert!(position(&msgs, |m| matches!(m, AudioCommand::Stop { .. })).is_none(), "{msgs:?}");
}
