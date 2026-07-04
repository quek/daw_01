//! Integration test: 楽器立て → 再生 → group 化 → group へ Bitcrush+Delay
//! 追加 → Bitcrush 削除 → ungroup の一連シーケンスを通しで検証する。
//!
//! 単一デバイスチェーン (`docs/plan_linear_chain.md`): 役割別 3 chain を捨て、
//! `Track.devices` を flat な `index: u32` 空間で扱う。plugin を picker で選ぶと
//! 末尾に append され、reorder は棄却なしの純 permutation。
//!
//! v29 (`docs/plan_arch_refactor.md` §1/§3): IPC は `AudioCommand` /
//! `PluginCommand` に分割され、 device のアドレスは安定 `device_id`
//! (`PluginInstance::id`) 一本。 旧 `ReorderChain` は削除され、 並び替えは
//! Song 編集 + `LoadSong` 再送のみで完結する。
//!
//! 検証する不変量:
//! 1. group 化で楽器 track が group の子になり、 group は楽器 track の
//!    直前 (= 上) に挿入される (Live 互換)
//! 2. group のチェーンに Bitcrush+Delay を順に append できて、
//!    `track_plugin_ids` に device_id が反映される
//! 3. Bitcrush 削除で、 残った Delay の device_id だけが
//!    `track_plugin_ids[group_id]` に残る
//! 4. ungroup で audio 側に `ClosePluginShmem` / plugin_host 側に
//!    `RemoveTrack` が送られ、 楽器 track は `parent_group_id == None` で残る

use common::model::{AutomationLane, AutomationTarget, InstrumentSource};
use common::protocol::{AudioCommand, PluginCommand, PluginEvent};
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::app::{device_id_at, AppData, AppEvent};

use super::support::{build_app, drain, fake_plugin_loaded};

#[test]
fn group_lifecycle_keeps_instrument_loaded_after_ungroup() {
    let (mut app, mut audio_rx, mut plugin_rx, _proxy) = build_app();

    assert_eq!(app.song_doc.song().tracks.len(), 1);
    let inst_track_id = app.song_doc.song().tracks[0].id;

    // Step 1: track 0 を選択し、 picker から synth を入れる (= device 0 に append)。
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });

    // SetSlotPlugin が plugin_host 行きに sent されているはず (安定 device id)。
    let synth_dev = device_id_at(app.song_doc.song(), inst_track_id, 0)
        .expect("picker append should allocate a device id");
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs
            .iter()
            .any(|m| matches!(m, PluginCommand::SetSlotPlugin { device_id, track_id, .. }
                if *device_id == synth_dev && *track_id == inst_track_id)),
        "SetSlotPlugin(device_id) should be sent to plugin_host: {:?}",
        plugin_msgs
    );

    fake_plugin_loaded(&mut app, inst_track_id, 0, "test.synth");
    assert_eq!(
        app.ipc.track_plugin_ids.get(&inst_track_id).map(|v| v.as_slice()),
        Some([synth_dev].as_slice()),
        "instrument device_id should register in track_plugin_ids"
    );

    // 子プロセス sync は pull 型 (docs/plan_arch_refactor.md §7.5): 実機では runner が
    // frame 末に flush_song_sync を呼ぶ。 headless test には frame loop が無いので、
    // 編集後に手で 1 回呼んで epoch 差分の LoadSong を送る。
    app.flush_song_sync();
    // add path の audio 再 sync (LoadSong) を drain (Step 2 の assertion を汚さない)。
    let audio_after_add = drain(&mut audio_rx);
    assert!(
        audio_after_add
            .iter()
            .any(|m| matches!(m, AudioCommand::LoadSong(_))),
        "adding a plugin should re-sync daw_audio with a fresh LoadSong: {:?}",
        audio_after_add
    );

    // Step 2: Play。 audio_tx に Play のみが出るはず。
    app.handle_event(AppEvent::Play);
    let audio_msgs = drain(&mut audio_rx);
    assert!(
        audio_msgs.iter().any(|m| matches!(m, AudioCommand::Play)),
        "Play should send Play to audio: {:?}",
        audio_msgs
    );
    assert!(
        !audio_msgs.iter().any(|m| matches!(m, AudioCommand::LoadSong(_))),
        "Play does NOT re-send LoadSong (dbca77f): {:?}",
        audio_msgs
    );
    assert!(app.transport.is_playing, "is_playing should be true after Play");

    // Step 3: instrument track を group 化。
    app.handle_event(AppEvent::GroupSelectedTracks {
        track_ids: vec![inst_track_id],
    });
    assert_eq!(
        app.song_doc.song().tracks.len(),
        2,
        "after group: 2 tracks (group + instrument)"
    );
    let group_id = app.song_doc.song().tracks[0].id;
    assert_ne!(group_id, inst_track_id, "group has fresh id");
    assert_eq!(
        app.song_doc.song().tracks[1].id, inst_track_id,
        "instrument is now after the group"
    );
    assert_eq!(
        app.song_doc.song().tracks[1].parent_group_id,
        Some(group_id),
        "instrument's parent should point at the new group"
    );
    assert_eq!(
        app.selection.selected_track_ids,
        vec![group_id],
        "selection moves to the group track"
    );
    assert!(
        app.is_group_track(group_id),
        "newly created group has children → is_group_track == true"
    );

    let _ = drain(&mut audio_rx);
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.is_empty(),
        "grouping does not touch plugin_host: {:?}",
        plugin_msgs
    );

    // Step 4: group が selected な状態で Bitcrush を append (= device 0 on group)。
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.bitcrush".into(),
        keep_open: false,
        open_gui: true,
    });
    let bitcrush_dev = device_id_at(app.song_doc.song(), group_id, 0)
        .expect("bitcrush append should allocate a device id");
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            PluginCommand::SetSlotPlugin { device_id, track_id, .. }
                if *device_id == bitcrush_dev && *track_id == group_id
        )),
        "Bitcrush should land at device 0 on the group track: {:?}",
        plugin_msgs
    );
    fake_plugin_loaded(&mut app, group_id, 0, "test.bitcrush");

    // Step 5: 同じく group に Delay を append (= device 1)。
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.delay".into(),
        keep_open: false,
        open_gui: true,
    });
    let delay_dev = device_id_at(app.song_doc.song(), group_id, 1)
        .expect("delay append should allocate a device id");
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            PluginCommand::SetSlotPlugin { device_id, track_id, .. }
                if *device_id == delay_dev && *track_id == group_id
        )),
        "Delay should land at device 1 on the group track: {:?}",
        plugin_msgs
    );
    fake_plugin_loaded(&mut app, group_id, 1, "test.delay");

    // group_plugin_ids には Bitcrush, Delay の device_id が register されている。
    assert_eq!(
        app.ipc.track_plugin_ids.get(&group_id).map(|v| v.as_slice()),
        Some([bitcrush_dev, delay_dev].as_slice()),
        "group has Bitcrush + Delay device_ids"
    );
    assert_eq!(
        app.song_doc.song().tracks
            .iter()
            .find(|t| t.id == group_id)
            .map(|t| t.devices.len()),
        Some(2),
        "group devices has 2 entries"
    );

    // Step 6: Bitcrush (device 0) を削除。
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);
    app.handle_event(AppEvent::RemoveDevice { index: 0 });
    // RemoveDevice は plugin の最新 state を取ってから Undo snapshot → 削除 という
    // deferred path を通る。 test では plugin_host を mock していないので、 fake で
    // AllStatesReceived を流して deferred edit を実行させる。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { entries: Vec::new() }));
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            PluginCommand::RemoveSlotPlugin { device_id } if *device_id == bitcrush_dev
        )),
        "RemoveSlotPlugin(bitcrush) should be sent to plugin_host: {:?}",
        plugin_msgs
    );
    // plugin_host が destroy 完了して SlotPluginUnloaded を返したのを fake。
    app.handle_event(AppEvent::Plugin(PluginEvent::SlotPluginUnloaded { device_id: bitcrush_dev }));

    // Bitcrush が track_plugin_ids から消えて、 Delay のみ残る。
    assert_eq!(
        app.ipc.track_plugin_ids.get(&group_id).map(|v| v.as_slice()),
        Some([delay_dev].as_slice()),
        "after Bitcrush remove: only Delay's device_id remains in group"
    );
    assert_eq!(
        app.song_doc.song().tracks
            .iter()
            .find(|t| t.id == group_id)
            .map(|t| t.devices.len()),
        Some(1),
        "after Bitcrush remove: group devices has 1 entry"
    );

    // Step 7: ungroup。 use-after-free 防止: audio 側に `ClosePluginShmem(delay)` を
    // 先に送り、 そのあと plugin_host に `RemoveTrack(group_id)`。
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);
    app.handle_event(AppEvent::UngroupTracks {
        track_ids: vec![group_id],
    });
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { entries: Vec::new() }));
    // frame flush: ClosePluginShmem は ungroup handler が UAF 防止で直送済。 schedule
    // 再構築の LoadSong はここで送られ、 close をブラケットする (load_before/after)。
    app.flush_song_sync();

    let audio_msgs = drain(&mut audio_rx);
    let plugin_msgs = drain(&mut plugin_rx);

    // ----- ungroup IPC: audio 側 -----
    let close_idx = audio_msgs
        .iter()
        .position(|m| matches!(m, AudioCommand::ClosePluginShmem { device_id } if *device_id == delay_dev))
        .unwrap_or_else(|| {
            panic!(
                "ClosePluginShmem(delay) must be sent on audio_tx during ungroup: {audio_msgs:?}"
            )
        });
    let load_after = audio_msgs.iter().enumerate().any(|(i, m)| {
        i > close_idx && matches!(m, AudioCommand::LoadSong(_))
    });
    let load_before = audio_msgs.iter().enumerate().any(|(i, m)| {
        i < close_idx && matches!(m, AudioCommand::LoadSong(_))
    });
    assert!(
        load_before || load_after,
        "LoadSong should bracket the close: {:?}",
        audio_msgs
    );

    // ----- ungroup IPC: plugin_host 側 -----
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            PluginCommand::RemoveTrack { track_id } if *track_id == group_id
        )),
        "RemoveTrack(group_id) must be sent on plugin_tx: {:?}",
        plugin_msgs
    );

    // ----- ungroup 後の AppData 状態 -----
    assert_eq!(
        app.song_doc.song().tracks.len(),
        1,
        "after ungroup: only the instrument track remains"
    );
    assert_eq!(
        app.song_doc.song().tracks[0].id, inst_track_id,
        "instrument track still in place"
    );
    assert_eq!(
        app.song_doc.song().tracks[0].parent_group_id, None,
        "instrument's parent reverts to master (None)"
    );
    assert!(
        !app.is_group_track(inst_track_id),
        "instrument track has no children → not a group"
    );
    assert!(
        !app.ipc.track_plugin_ids.contains_key(&group_id),
        "group_id is removed from track_plugin_ids"
    );
    assert_eq!(
        app.ipc.track_plugin_ids.get(&inst_track_id).map(|v| v.as_slice()),
        Some([synth_dev].as_slice()),
        "instrument track keeps its device_id (audio continues)"
    );
    // 念のため song モデル側も instrument device が残っているか。
    let inst_track = &app.song_doc.song().tracks[0];
    assert_eq!(
        inst_track.devices.first().map(|p| p.plugin_id.as_str()),
        Some("test.synth"),
        "instrument device still bound to test.synth: {:?}",
        inst_track.devices
    );
    assert!(
        matches!(inst_track.source, InstrumentSource::None),
        "instrument source is the default; we only loaded the plugin"
    );
}

/// Build track 0 = [synth, bitcrush, delay], all reported loaded, and drain
/// both child channels. Returns (track id, [synth_dev, bitcrush_dev, delay_dev]).
fn setup_loaded_chain(
    app: &mut AppData,
    audio_rx: &mut UnboundedReceiver<AudioCommand>,
    plugin_rx: &mut UnboundedReceiver<PluginCommand>,
) -> (u32, [u64; 3]) {
    let track_id = app.song_doc.song().tracks[0].id;
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: false,
    });
    let synth_dev = fake_plugin_loaded(app, track_id, 0, "test.synth");
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.bitcrush".into(),
        keep_open: false,
        open_gui: false,
    });
    let bitcrush_dev = fake_plugin_loaded(app, track_id, 1, "test.bitcrush");
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.delay".into(),
        keep_open: false,
        open_gui: false,
    });
    let delay_dev = fake_plugin_loaded(app, track_id, 2, "test.delay");
    // Sanity: starting layout is [synth, bitcrush, delay].
    {
        let t = &app.song_doc.song().tracks[0];
        assert_eq!(
            t.devices.iter().map(|p| p.plugin_id.as_str()).collect::<Vec<_>>(),
            vec!["test.synth", "test.bitcrush", "test.delay"]
        );
    }
    let _ = drain(audio_rx);
    let _ = drain(plugin_rx);
    (track_id, [synth_dev, bitcrush_dev, delay_dev])
}

/// v29 の reorder は (a) song を permute、 (b) daw_gui の `(track, index)`
/// cache を再キー、 (c) `LoadSong` 再送のみ (旧 `ReorderChain` は削除 —
/// plugin_host は順序を持たず、 audio の処理順は LoadSong が compile する)。
#[test]
fn inspector_chain_reorder_rekeys_both_children() {
    let (mut app, mut audio_rx, mut plugin_rx, _proxy) = build_app();
    let (track_id, [synth_dev, bitcrush_dev, delay_dev]) =
        setup_loaded_chain(&mut app, &mut audio_rx, &mut plugin_rx);

    // Reorder. devices = [synth(0), bitcrush(1), delay(2)]. gui_01 契約は
    // new[i] = items[order[i]]; order [0,2,1] は synth を残して 2 つの FX を入れ替え
    // (delay が bitcrush より前へ)。
    app.handle_event(AppEvent::ReorderInspectorChain(vec![0, 2, 1]));

    // (a) song permutation: device 順が [synth, delay, bitcrush] に。
    {
        let t = &app.song_doc.song().tracks[0];
        assert_eq!(
            t.devices.iter().map(|p| p.plugin_id.as_str()).collect::<Vec<_>>(),
            vec!["test.synth", "test.delay", "test.bitcrush"],
            "devices order permuted in the song model"
        );
        // 安定 device id は device と一緒に動く。
        assert_eq!(
            t.devices.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![synth_dev, delay_dev, bitcrush_dev],
            "device ids move with the devices"
        );
    }

    // (b) daw_gui caches re-keyed so each device index resolves to its moved plugin.
    assert_eq!(
        app.ipc.loaded_slots.get(&(track_id, 0)).map(|i| i.device_id),
        Some(synth_dev),
        "index 0 still maps to synth's device_id"
    );
    assert_eq!(
        app.ipc.loaded_slots.get(&(track_id, 1)).map(|i| i.device_id),
        Some(delay_dev),
        "index 1 now maps to delay's device_id"
    );
    assert_eq!(
        app.ipc.loaded_slots.get(&(track_id, 2)).map(|i| i.device_id),
        Some(bitcrush_dev),
        "index 2 now maps to bitcrush's device_id"
    );

    // (c) v29: children には LoadSong だけが飛ぶ (audio schedule は Song から
    // 再 compile、 plugin_host は順序を持たない)。 pull 型 sync なので frame flush を
    // 明示的に回して reorder 編集の LoadSong を送る (headless には frame loop 無し)。
    app.flush_song_sync();
    let plugin_msgs = drain(&mut plugin_rx);
    let audio_msgs = drain(&mut audio_rx);
    assert!(
        audio_msgs.iter().any(|m| matches!(m, AudioCommand::LoadSong(_))),
        "a LoadSong must follow to rebuild the schedule order: {audio_msgs:?}"
    );
    // plugin_host には並び替え由来の per-device 命令が飛ばないこと
    // (SetSlotPlugin / RemoveSlotPlugin が出たら再キー儀式の復活 = regression)。
    assert!(
        !plugin_msgs.iter().any(|m| matches!(
            m,
            PluginCommand::SetSlotPlugin { .. } | PluginCommand::RemoveSlotPlugin { .. }
        )),
        "reorder must not reload/remove plugins on the host: {plugin_msgs:?}"
    );
}

/// v29: `AutomationTarget::PluginParam` は安定 `device_id` addressing なので、
/// reorder しても lane target は**無変更のまま正しい** (id は device と一緒に
/// 動く)。 旧 positional remap の削除が退行していないことを確認する。
#[test]
fn inspector_chain_reorder_keeps_automation_lane_device_ids() {
    let (mut app, mut audio_rx, mut plugin_rx, _proxy) = build_app();
    let (_track_id, [_synth_dev, bitcrush_dev, _delay_dev]) =
        setup_loaded_chain(&mut app, &mut audio_rx, &mut plugin_rx);

    // Automate a param on bitcrush (currently device index 1).
    app.edit_song(|song| {
        song.tracks[0].automation_lanes.push(AutomationLane {
            id: 1,
            target: AutomationTarget::PluginParam {
                device_id: bitcrush_dev,
                param_id: 42,
                legacy_device_index: None,
                legacy_slot: None,
            },
            default_value: 0.25,
            enabled: true,
            visible: true,
            height_px: 60,
            clips: Vec::new(),
            next_clip_id: 1,
        });
    });

    // Swap the two FX (delay before bitcrush): order [0,2,1].
    app.handle_event(AppEvent::ReorderInspectorChain(vec![0, 2, 1]));

    // bitcrush moved index 1 -> index 2; the lane still points at bitcrush by id.
    assert_eq!(
        app.song_doc.song().tracks[0].automation_lanes[0].target,
        AutomationTarget::PluginParam {
            device_id: bitcrush_dev,
            param_id: 42,
            legacy_device_index: None,
            legacy_slot: None,
        },
        "the automation lane keeps addressing the plugin by its stable id"
    );
    // …and that id resolves to the plugin's new position.
    assert_eq!(
        app.song_doc.song().tracks[0]
            .devices
            .iter()
            .position(|d| d.id == bitcrush_dev),
        Some(2),
        "bitcrush now sits at index 2"
    );
}

/// The reorder re-keys daw_gui's positional caches, and a failed/in-flight
/// plugin load can leave a phantom in the song that the caches do not have.
/// Applying the reorder then would skew the cache re-keying, so the reorder
/// must be a no-op (UI snaps back) unless the track's whole chain is
/// consistently loaded.
#[test]
fn inspector_chain_reorder_aborts_when_chain_not_fully_loaded() {
    let (mut app, mut audio_rx, mut plugin_rx, _proxy) = build_app();
    let (track_id, _devs) = setup_loaded_chain(&mut app, &mut audio_rx, &mut plugin_rx);

    // Inject a PHANTOM 4th device: present in the song (and the inspector chain)
    // but never reported loaded, mimicking a plugin whose load failed.
    app.edit_song(|song| {
        song.tracks
            .iter_mut()
            .find(|t| t.id == track_id)
            .unwrap()
            .devices
            .push(common::model::PluginInstance::new(
                "test.delay".into(),
                common::plugin_format::PluginFormat::Clap,
            ));
    });
    let before: Vec<String> = app.song_doc.song().tracks[0]
        .devices
        .iter()
        .map(|p| p.plugin_id.clone())
        .collect();
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);

    // Try to swap indices 1 and 2 over the 4-item chain [synth, bitcrush, delay, phantom].
    app.handle_event(AppEvent::ReorderInspectorChain(vec![0, 2, 1, 3]));

    // No song mutation and no child IPC.
    let after: Vec<String> = app.song_doc.song().tracks[0]
        .devices
        .iter()
        .map(|p| p.plugin_id.clone())
        .collect();
    assert_eq!(before, after, "reorder must be a no-op on an inconsistent chain");
    let plugin_msgs = drain(&mut plugin_rx);
    let audio_msgs = drain(&mut audio_rx);
    assert!(
        plugin_msgs.is_empty(),
        "no plugin_host IPC may result from an aborted reorder: {plugin_msgs:?}"
    );
    assert!(
        audio_msgs.is_empty(),
        "no daw_audio IPC may result from an aborted reorder: {audio_msgs:?}"
    );
}
