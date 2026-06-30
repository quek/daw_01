//! Integration test: 楽器立て → 再生 → group 化 → group へ Bitcrush+Delay
//! 追加 → Bitcrush 削除 → ungroup の一連シーケンスを通しで検証する。
//!
//! 単一デバイスチェーン (`docs/plan_linear_chain.md`): 役割別 3 chain を捨て、
//! `Track.devices` を flat な `index: u32` 空間で扱う。plugin を picker で選ぶと
//! 末尾に append され、reorder は棄却なしの純 permutation。
//!
//! 検証する不変量:
//! 1. group 化で楽器 track が group の子になり、 group は楽器 track の
//!    直前 (= 上) に挿入される (Live 互換)
//! 2. group のチェーンに Bitcrush+Delay を順に append できて、
//!    `track_plugin_ids` に plugin_id が反映される
//! 3. Bitcrush 削除で、 残った Delay の plugin_id だけが
//!    `track_plugin_ids[group_id]` に残る
//! 4. ungroup で audio 側に `ClosePluginShmem` / plugin_host 側に
//!    `RemoveTrack` が送られ、 楽器 track は `parent_group_id == None` で残る

use std::sync::Arc;

use common::model::{AutomationLane, AutomationTarget, InstrumentSource};
use common::plugin_db::{PluginDatabase, PluginEntry};
use common::plugin_format::PluginFormat;
use common::protocol::MainToChild;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

/// テスト用 plugin_db。 楽器 / Bitcrush / Delay の 3 件だけを含む。
/// `path` は実在不要 (production の plugin loader に通すわけではない)。
fn make_plugin_db() -> Arc<PluginDatabase> {
    Arc::new(PluginDatabase {
        entries: vec![
            PluginEntry {
                id: "test.synth".into(),
                format: PluginFormat::Clap,
                name: "Test Synth".into(),
                vendor: "Test".into(),
                version: "1.0".into(),
                features: vec!["instrument".into()],
                path: "C:/fake/synth.clap".into(),
                descriptor_index: 0,
                has_note_input: true,
                has_note_output: false,
                has_audio_output: true,
                // instrument: audio を生成するだけ → audio 入力なし。
                has_audio_input: false,
                has_video_input: false,
                has_video_output: false,
            },
            PluginEntry {
                id: "test.bitcrush".into(),
                format: PluginFormat::Clap,
                name: "Test Bitcrush".into(),
                vendor: "Test".into(),
                version: "1.0".into(),
                features: vec!["audio-effect".into()],
                path: "C:/fake/bitcrush.clap".into(),
                descriptor_index: 0,
                has_note_input: false,
                has_note_output: false,
                has_audio_output: true,
                // audio-effect: audio を加工する → audio 入力あり。
                has_audio_input: true,
                has_video_input: false,
                has_video_output: false,
            },
            PluginEntry {
                id: "test.delay".into(),
                format: PluginFormat::Clap,
                name: "Test Delay".into(),
                vendor: "Test".into(),
                version: "1.0".into(),
                features: vec!["audio-effect".into()],
                path: "C:/fake/delay.clap".into(),
                descriptor_index: 0,
                has_note_input: false,
                has_note_output: false,
                has_audio_output: true,
                // audio-effect: audio を加工する → audio 入力あり。
                has_audio_input: true,
                has_video_input: false,
                has_video_output: false,
            },
        ],
        scanned_at: None,
        port_probe_version: 0,
    })
}

/// AppData を test 用 dispatcher 込みで構築。 dispatcher は trait 抽象に
/// なっているので winit EventLoop は不要。
fn build_app() -> (
    AppData,
    UnboundedReceiver<MainToChild>, // audio_rx
    UnboundedReceiver<MainToChild>, // plugin_rx
    Arc<RecordingDispatcher>,
) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let event_dispatcher_dyn: Arc<dyn BackgroundDispatcher> = event_dispatcher.clone();
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        Some(make_plugin_db()),
        event_dispatcher_dyn,
        job_dispatcher,
        None,
        // app_dirs: None = 永続化なし。 実 %LOCALAPPDATA%/daw_01/recent*.json を汚染しない。
        None,
        48_000, // (A1 r.md #8) test sample rate
    );
    (app, audio_rx, plugin_rx, event_dispatcher)
}

/// `rx` から現在キューにある全メッセージを取り出す。 試験 assertion 用。
fn drain<T>(rx: &mut UnboundedReceiver<T>) -> Vec<T> {
    let mut v = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        v.push(msg);
    }
    v
}

/// ヘルパ: plugin_host の `SlotPluginLoaded` を AppEvent として fake
/// dispatch。 production で plugin_host が返す内容を test がそのまま模倣する。
/// `index` は flat な device index (= 末尾 append した位置)。
fn fake_plugin_loaded(
    app: &mut AppData,
    track_id: u32,
    index: u32,
    id: &str,
    plugin_id: u32,
) {
    app.handle_event(AppEvent::SlotPluginLoadedFromChild {
        track: track_id,
        index,
        id: id.into(),
        name: id.into(),
        plugin_id,
        shmem_id: String::new(),
        // テストは state 復元 path をシミュレートしない (= initial_state =
        // None でロードしたのと等価)。
        state_load_error: None,
        aux_output_count: 0,
    });
}

#[test]
fn group_lifecycle_keeps_instrument_loaded_after_ungroup() {
    let (mut app, mut audio_rx, mut plugin_rx, _proxy) = build_app();

    assert_eq!(app.song.tracks.len(), 1);
    let inst_track_id = app.song.tracks[0].id;

    // Step 1: track 0 を選択し、 picker から synth を入れる (= device 0 に append)。
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });

    // SetSlotPlugin が plugin_host 行きに sent されているはず (device 0)。
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs
            .iter()
            .any(|m| matches!(m, MainToChild::SetSlotPlugin { track, index: 0, .. } if *track == inst_track_id)),
        "SetSlotPlugin(index 0) should be sent to plugin_host: {:?}",
        plugin_msgs
    );

    fake_plugin_loaded(&mut app, inst_track_id, 0, "test.synth", 100);
    assert_eq!(
        app.track_plugin_ids.get(&inst_track_id).map(|v| v.as_slice()),
        Some([100u32].as_slice()),
        "instrument plugin_id should register in track_plugin_ids"
    );

    // add path の audio 再 sync (LoadSong) を drain (Step 2 の assertion を汚さない)。
    let audio_after_add = drain(&mut audio_rx);
    assert!(
        audio_after_add
            .iter()
            .any(|m| matches!(m, MainToChild::LoadSong(_))),
        "adding a plugin should re-sync daw_audio with a fresh LoadSong: {:?}",
        audio_after_add
    );

    // Step 2: Play。 audio_tx に Play のみが出るはず。
    app.handle_event(AppEvent::Play);
    let audio_msgs = drain(&mut audio_rx);
    assert!(
        audio_msgs.iter().any(|m| matches!(m, MainToChild::Play)),
        "Play should send Play to audio: {:?}",
        audio_msgs
    );
    assert!(
        !audio_msgs.iter().any(|m| matches!(m, MainToChild::LoadSong(_))),
        "Play does NOT re-send LoadSong (dbca77f): {:?}",
        audio_msgs
    );
    assert!(app.is_playing, "is_playing should be true after Play");

    // Step 3: instrument track を group 化。
    app.handle_event(AppEvent::GroupSelectedTracks {
        track_ids: vec![inst_track_id],
    });
    assert_eq!(
        app.song.tracks.len(),
        2,
        "after group: 2 tracks (group + instrument)"
    );
    let group_id = app.song.tracks[0].id;
    assert_ne!(group_id, inst_track_id, "group has fresh id");
    assert_eq!(
        app.song.tracks[1].id, inst_track_id,
        "instrument is now after the group"
    );
    assert_eq!(
        app.song.tracks[1].parent_group_id,
        Some(group_id),
        "instrument's parent should point at the new group"
    );
    assert_eq!(
        app.selected_track_ids,
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
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            MainToChild::SetSlotPlugin { track, index: 0, .. } if *track == group_id
        )),
        "Bitcrush should land at device 0 on the group track: {:?}",
        plugin_msgs
    );
    fake_plugin_loaded(&mut app, group_id, 0, "test.bitcrush", 200);

    // Step 5: 同じく group に Delay を append (= device 1)。
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.delay".into(),
        keep_open: false,
        open_gui: true,
    });
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            MainToChild::SetSlotPlugin { track, index: 1, .. } if *track == group_id
        )),
        "Delay should land at device 1 on the group track: {:?}",
        plugin_msgs
    );
    fake_plugin_loaded(&mut app, group_id, 1, "test.delay", 201);

    // group_plugin_ids には Bitcrush(200), Delay(201) が register されている。
    assert_eq!(
        app.track_plugin_ids.get(&group_id).map(|v| v.as_slice()),
        Some([200u32, 201u32].as_slice()),
        "group has Bitcrush + Delay plugin_ids"
    );
    assert_eq!(
        app.song.tracks
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
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            MainToChild::RemoveSlotPlugin { track, index: 0 } if *track == group_id
        )),
        "RemoveSlotPlugin(index 0) should be sent to plugin_host: {:?}",
        plugin_msgs
    );
    // plugin_host が destroy 完了して SlotPluginUnloaded を返したのを fake。
    app.handle_event(AppEvent::SlotPluginUnloadedFromChild { plugin_id: 200 });

    // Bitcrush(200) が track_plugin_ids から消えて、 Delay(201) のみ残る。
    assert_eq!(
        app.track_plugin_ids.get(&group_id).map(|v| v.as_slice()),
        Some([201u32].as_slice()),
        "after Bitcrush remove: only Delay's plugin_id remains in group"
    );
    assert_eq!(
        app.song.tracks
            .iter()
            .find(|t| t.id == group_id)
            .map(|t| t.devices.len()),
        Some(1),
        "after Bitcrush remove: group devices has 1 entry"
    );

    // Step 7: ungroup。 use-after-free 防止: audio 側に `ClosePluginShmem(201)` を
    // 先に送り、 そのあと plugin_host に `RemoveTrack(group_id)`。
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);
    app.handle_event(AppEvent::UngroupTracks {
        track_ids: vec![group_id],
    });
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));

    let audio_msgs = drain(&mut audio_rx);
    let plugin_msgs = drain(&mut plugin_rx);

    // ----- ungroup IPC: audio 側 -----
    let close_idx = audio_msgs
        .iter()
        .position(|m| matches!(m, MainToChild::ClosePluginShmem { plugin_id: 201 }))
        .unwrap_or_else(|| {
            panic!(
                "ClosePluginShmem(201) must be sent on audio_tx during ungroup: {audio_msgs:?}"
            )
        });
    let load_after = audio_msgs.iter().enumerate().any(|(i, m)| {
        i > close_idx && matches!(m, MainToChild::LoadSong(_))
    });
    let load_before = audio_msgs.iter().enumerate().any(|(i, m)| {
        i < close_idx && matches!(m, MainToChild::LoadSong(_))
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
            MainToChild::RemoveTrack { track } if *track == group_id
        )),
        "RemoveTrack(group_id) must be sent on plugin_tx: {:?}",
        plugin_msgs
    );

    // ----- ungroup 後の AppData 状態 -----
    assert_eq!(
        app.song.tracks.len(),
        1,
        "after ungroup: only the instrument track remains"
    );
    assert_eq!(
        app.song.tracks[0].id, inst_track_id,
        "instrument track still in place"
    );
    assert_eq!(
        app.song.tracks[0].parent_group_id, None,
        "instrument's parent reverts to master (None)"
    );
    assert!(
        !app.is_group_track(inst_track_id),
        "instrument track has no children → not a group"
    );
    assert!(
        !app.track_plugin_ids.contains_key(&group_id),
        "group_id is removed from track_plugin_ids"
    );
    assert_eq!(
        app.track_plugin_ids.get(&inst_track_id).map(|v| v.as_slice()),
        Some([100u32].as_slice()),
        "instrument track keeps its plugin_id (audio continues)"
    );
    // 念のため song モデル側も instrument device が残っているか。
    let inst_track = &app.song.tracks[0];
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

/// Build track 0 = [synth(0,100), bitcrush(1,101), delay(2,102)], all reported
/// loaded, and drain both child channels. Returns the track id.
fn setup_loaded_chain(
    app: &mut AppData,
    audio_rx: &mut UnboundedReceiver<MainToChild>,
    plugin_rx: &mut UnboundedReceiver<MainToChild>,
) -> u32 {
    let track_id = app.song.tracks[0].id;
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: false,
    });
    fake_plugin_loaded(app, track_id, 0, "test.synth", 100);
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.bitcrush".into(),
        keep_open: false,
        open_gui: false,
    });
    fake_plugin_loaded(app, track_id, 1, "test.bitcrush", 101);
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.delay".into(),
        keep_open: false,
        open_gui: false,
    });
    fake_plugin_loaded(app, track_id, 2, "test.delay", 102);
    // Sanity: starting layout is [synth, bitcrush, delay].
    {
        let t = &app.song.tracks[0];
        assert_eq!(
            t.devices.iter().map(|p| p.plugin_id.as_str()).collect::<Vec<_>>(),
            vec!["test.synth", "test.bitcrush", "test.delay"]
        );
    }
    let _ = drain(audio_rx);
    let _ = drain(plugin_rx);
    track_id
}

/// 単一デバイスチェーンの reorder は (a) song を permute、 (b) daw_gui の
/// `(track, index)` cache を再キー、 (c) `ReorderChain` を BOTH children へ送り、
/// その後 `LoadSong` で audio schedule を再構築する。
#[test]
fn inspector_chain_reorder_rekeys_both_children() {
    let (mut app, mut audio_rx, mut plugin_rx, _proxy) = build_app();
    let track_id = setup_loaded_chain(&mut app, &mut audio_rx, &mut plugin_rx);

    // Reorder. devices = [synth(0), bitcrush(1), delay(2)]. gui_01 契約は
    // new[i] = items[order[i]]; order [0,2,1] は synth を残して 2 つの FX を入れ替え
    // (delay が bitcrush より前へ)。
    app.handle_event(AppEvent::ReorderInspectorChain(vec![0, 2, 1]));

    // (a) song permutation: device 順が [synth, delay, bitcrush] に。
    {
        let t = &app.song.tracks[0];
        assert_eq!(
            t.devices.iter().map(|p| p.plugin_id.as_str()).collect::<Vec<_>>(),
            vec!["test.synth", "test.delay", "test.bitcrush"],
            "devices order permuted in the song model"
        );
    }

    // (b) daw_gui caches re-keyed so each device index resolves to its moved plugin.
    assert_eq!(
        app.loaded_slots.get(&(track_id, 0)).map(|i| i.plugin_id),
        Some(100),
        "index 0 still maps to synth's plugin_id"
    );
    assert_eq!(
        app.loaded_slots.get(&(track_id, 1)).map(|i| i.plugin_id),
        Some(102),
        "index 1 now maps to delay's plugin_id"
    );
    assert_eq!(
        app.loaded_slots.get(&(track_id, 2)).map(|i| i.plugin_id),
        Some(101),
        "index 2 now maps to bitcrush's plugin_id"
    );

    // (c) ReorderChain with the correct (old -> new) permutation to BOTH
    // children, plus the LoadSong that rebuilds the audio schedule.
    // moves[i] = (order[i], i): synth stays, delay 2->1, bitcrush 1->2.
    let expected_moves: Vec<(u32, u32)> = vec![(0, 0), (2, 1), (1, 2)];
    let plugin_msgs = drain(&mut plugin_rx);
    let audio_msgs = drain(&mut audio_rx);
    let find_reorder = |msgs: &[MainToChild]| -> Option<Vec<(u32, u32)>> {
        msgs.iter().find_map(|m| match m {
            MainToChild::ReorderChain { track, moves } if *track == track_id => {
                Some(moves.clone())
            }
            _ => None,
        })
    };
    assert_eq!(
        find_reorder(&plugin_msgs).as_deref(),
        Some(expected_moves.as_slice()),
        "ReorderChain must reach plugin_host with the live-move permutation: {plugin_msgs:?}"
    );
    assert_eq!(
        find_reorder(&audio_msgs).as_deref(),
        Some(expected_moves.as_slice()),
        "ReorderChain must reach daw_audio with the same permutation: {audio_msgs:?}"
    );
    assert!(
        audio_msgs.iter().any(|m| matches!(m, MainToChild::LoadSong(_))),
        "a LoadSong must follow to rebuild the schedule order: {audio_msgs:?}"
    );
}

/// `AutomationTarget::PluginParam { device_index }` lanes are addressed by device
/// index and persisted. A reorder must re-point each lane old→new, or the moved
/// plugin loses its automation and whatever took its old index inherits it
/// (audible wrong audio that also survives a reload).
#[test]
fn inspector_chain_reorder_remaps_automation_lane_slots() {
    let (mut app, mut audio_rx, mut plugin_rx, _proxy) = build_app();
    let _track_id = setup_loaded_chain(&mut app, &mut audio_rx, &mut plugin_rx);

    // Automate a param on bitcrush (currently device index 1).
    app.song.tracks[0].automation_lanes.push(AutomationLane {
        id: 1,
        target: AutomationTarget::PluginParam {
            device_index: 1,
            param_id: 42,
            legacy_slot: None,
        },
        default_value: 0.25,
        enabled: true,
        visible: true,
        height_px: 60,
        clips: Vec::new(),
        next_clip_id: 1,
    });

    // Swap the two FX (delay before bitcrush): order [0,2,1].
    app.handle_event(AppEvent::ReorderInspectorChain(vec![0, 2, 1]));

    // bitcrush moved index 1 -> index 2; its lane must follow so it still drives
    // bitcrush (not delay, which now sits at index 1).
    assert_eq!(
        app.song.tracks[0].automation_lanes[0].target,
        AutomationTarget::PluginParam {
            device_index: 2,
            param_id: 42,
            legacy_slot: None,
        },
        "the automation lane must track the plugin to its new device index"
    );
}

/// The reorder re-keys all three processes, but a failed/in-flight plugin load
/// can leave a phantom in the song that the plugin host's live chain does not
/// have. Applying the reorder then would make the host skip while the audio
/// engine + daw_gui apply, diverging permanently. So the reorder must be a no-op
/// (UI snaps back) unless the track's whole chain is consistently loaded.
#[test]
fn inspector_chain_reorder_aborts_when_chain_not_fully_loaded() {
    let (mut app, mut audio_rx, mut plugin_rx, _proxy) = build_app();
    let track_id = setup_loaded_chain(&mut app, &mut audio_rx, &mut plugin_rx);

    // Inject a PHANTOM 4th device: present in the song (and the inspector chain)
    // but never reported loaded, mimicking a plugin whose load failed.
    app.song
        .tracks
        .iter_mut()
        .find(|t| t.id == track_id)
        .unwrap()
        .devices
        .push(common::model::PluginInstance::new(
            "test.delay".into(),
            common::plugin_format::PluginFormat::Clap,
        ));
    let before: Vec<String> = app.song.tracks[0]
        .devices
        .iter()
        .map(|p| p.plugin_id.clone())
        .collect();
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);

    // Try to swap indices 1 and 2 over the 4-item chain [synth, bitcrush, delay, phantom].
    app.handle_event(AppEvent::ReorderInspectorChain(vec![0, 2, 1, 3]));

    // No song mutation and no ReorderChain to either child.
    let after: Vec<String> = app.song.tracks[0]
        .devices
        .iter()
        .map(|p| p.plugin_id.clone())
        .collect();
    assert_eq!(before, after, "reorder must be a no-op on an inconsistent chain");
    let plugin_msgs = drain(&mut plugin_rx);
    let audio_msgs = drain(&mut audio_rx);
    assert!(
        !plugin_msgs.iter().any(|m| matches!(m, MainToChild::ReorderChain { .. })),
        "no ReorderChain may reach plugin_host: {plugin_msgs:?}"
    );
    assert!(
        !audio_msgs.iter().any(|m| matches!(m, MainToChild::ReorderChain { .. })),
        "no ReorderChain may reach daw_audio: {audio_msgs:?}"
    );
}
