//! Integration test: 楽器立て → 再生 → group 化 → group へ Bitcrush+Delay
//! 追加 → Bitcrush 削除 → ungroup の一連シーケンスを通しで検証する。
//!
//! 検証する不変量:
//! 1. group 化で楽器 track が group の子になり、 group は楽器 track の
//!    直前 (= 上) に挿入される (Live 互換)
//! 2. group のチェーンに Bitcrush+Delay を順に積めて、 `track_plugin_ids`
//!    に plugin_id が反映される
//! 3. Bitcrush 削除で、 残った Delay の plugin_id だけが
//!    `track_plugin_ids[group_id]` に残る
//! 4. ungroup で
//!    - audio 側に `ClosePluginShmem(<group の Delay の plugin_id>)` が
//!      送信される (use-after-free 防止: plugin destroy より先に audio
//!      engine の `plugin_refs` から外す)
//!    - plugin_host 側に `RemoveTrack(<group_id>)` が送信される
//!    - 楽器 track は `parent_group_id == None` で残る (= 音継続)
//!    - `track_plugin_ids[<楽器 track id>]` に楽器 plugin_id が残ったまま

use std::sync::Arc;

use common::model::{InstrumentSource, PluginInstance};
use common::plugin_db::{PluginDatabase, PluginEntry};
use common::plugin_format::PluginFormat;
use common::protocol::{MainToChild, PluginSlot};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, PickerTarget};
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
                features: vec![],
                path: "C:/fake/synth.clap".into(),
                descriptor_index: 0,
            },
            PluginEntry {
                id: "test.bitcrush".into(),
                format: PluginFormat::Clap,
                name: "Test Bitcrush".into(),
                vendor: "Test".into(),
                version: "1.0".into(),
                features: vec![],
                path: "C:/fake/bitcrush.clap".into(),
                descriptor_index: 0,
            },
            PluginEntry {
                id: "test.delay".into(),
                format: PluginFormat::Clap,
                name: "Test Delay".into(),
                vendor: "Test".into(),
                version: "1.0".into(),
                features: vec![],
                path: "C:/fake/delay.clap".into(),
                descriptor_index: 0,
            },
        ],
        scanned_at: None,
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
/// dispatch。 production で plugin_host が返す内容を test がそのまま
/// 模倣する。
fn fake_plugin_loaded(
    app: &mut AppData,
    track_id: u32,
    slot: PluginSlot,
    id: &str,
    plugin_id: u32,
) {
    app.handle_event(AppEvent::SlotPluginLoadedFromChild {
        track: track_id,
        slot,
        id: id.into(),
        name: id.into(),
        plugin_id,
    });
}

#[test]
fn group_lifecycle_keeps_instrument_loaded_after_ungroup() {
    let (mut app, mut audio_rx, mut plugin_rx, _proxy) = build_app();

    // 初期状態: AppData::new で Track 1 が 1 つ作られている。
    // この track の id はまだ採番前 (0) で、 `ensure_ids` 前の状態
    // (production でも load 前は同じ)。 plugin_host との chain key は
    // この id をそのまま使うので、 0 でも track_id ベースの routing は
    // 動く (すべて id == 0 で揃うため)。
    assert_eq!(app.song.tracks.len(), 1);
    let inst_track_id = app.song.tracks[0].id;

    // Step 1: track 0 を選択し、 instrument picker から synth を入れる。
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Instrument));
    app.handle_event(AppEvent::SelectPluginFromDb("test.synth".into()));

    // SetSlotPlugin が plugin_host 行きに sent されているはず。
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs
            .iter()
            .any(|m| matches!(m, MainToChild::SetSlotPlugin { track, slot: PluginSlot::Instrument, .. } if *track == inst_track_id)),
        "SetSlotPlugin(Instrument) should be sent to plugin_host: {:?}",
        plugin_msgs
    );

    // plugin_host からの SlotPluginLoaded を fake dispatch。
    fake_plugin_loaded(
        &mut app,
        inst_track_id,
        PluginSlot::Instrument,
        "test.synth",
        100,
    );
    assert_eq!(
        app.track_plugin_ids.get(&inst_track_id).map(|v| v.as_slice()),
        Some([100u32].as_slice()),
        "instrument plugin_id should register in track_plugin_ids"
    );

    // Step 2: Play。 audio_tx に Play のみが出るはず。
    // dbca77f 以降、 play() は LoadSong を再送しない (= 旧バグ: 大量 WAV の
    // とき audio engine の compile_audio_schedule = decode + schedule build
    // が同期で 2 秒以上かかり再生開始が遅延)。 LoadSong は
    // sync_song_to_plugin_host 経由で都度 audio engine に届いている前提。
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

    // Step 3: instrument track を group 化。 selected_track_ids が group 自身に
    // なるよう仕様 (Live 互換)。 group 自体は instrument の **直前 (= 上)** に
    // 挿入される。
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

    // group 化 → sync_song_to_plugin_host で audio へ LoadSong が送られた
    // はず。 plugin chain に変更は無いので plugin_rx は空のまま。
    let _ = drain(&mut audio_rx);
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.is_empty(),
        "grouping does not touch plugin_host: {:?}",
        plugin_msgs
    );

    // Step 4: group が selected な状態で Bitcrush を Fx 0 に追加。
    // selected_track_ids = [group_id] の末尾は group_id なので cursor は group。
    app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Fx));
    app.handle_event(AppEvent::SelectPluginFromDb("test.bitcrush".into()));
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            MainToChild::SetSlotPlugin {
                track,
                slot: PluginSlot::Fx(0),
                ..
            } if *track == group_id
        )),
        "Bitcrush should land at Fx(0) on the group track: {:?}",
        plugin_msgs
    );
    fake_plugin_loaded(&mut app, group_id, PluginSlot::Fx(0), "test.bitcrush", 200);

    // Step 5: 同じく group に Delay を Fx 1 に追加。
    app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Fx));
    app.handle_event(AppEvent::SelectPluginFromDb("test.delay".into()));
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            MainToChild::SetSlotPlugin {
                track,
                slot: PluginSlot::Fx(1),
                ..
            } if *track == group_id
        )),
        "Delay should land at Fx(1) on the group track: {:?}",
        plugin_msgs
    );
    fake_plugin_loaded(&mut app, group_id, PluginSlot::Fx(1), "test.delay", 201);

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
            .map(|t| t.fx_chain.len()),
        Some(2),
        "group fx_chain has 2 entries"
    );

    // Step 6: Bitcrush (Fx 0) を削除。
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);
    app.handle_event(AppEvent::RemoveSlot {
        slot_kind: 2, // Fx
        slot_index: 0,
    });
    // RemoveSlot は plugin の最新 state を取ってから Undo snapshot →
    // 削除 という deferred path を通る (PendingStateRequest)。 test では
    // plugin_host を mock していないので、 fake で AllStatesReceived を
    // 流して deferred edit を実行させる。
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            MainToChild::RemoveSlotPlugin {
                track,
                slot: PluginSlot::Fx(0),
            } if *track == group_id
        )),
        "RemoveSlotPlugin(Fx 0) should be sent to plugin_host: {:?}",
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
            .map(|t| t.fx_chain.len()),
        Some(1),
        "after Bitcrush remove: group fx_chain has 1 entry"
    );

    // Step 7: ungroup。 これが本テストの肝 (use-after-free 防止):
    //   - audio 側に `ClosePluginShmem(201)` を **先に** 送る
    //     → audio engine の `plugin_refs` / `slot_to_plugin_id` から
    //        Delay の entry が消える
    //   - そのあと plugin_host に `RemoveTrack(group_id)` を送る
    //     → plugin_host が Delay の Box<Plugin> を destroy しても
    //        audio worker は Delay にアクセスしないので AV しない
    //   - 楽器 track は parent_group_id == None で残り、 plugin_id 100
    //     も track_plugin_ids[inst_track_id] に保持される (= 音継続)。
    let _ = drain(&mut audio_rx);
    let _ = drain(&mut plugin_rx);
    app.handle_event(AppEvent::UngroupTracks {
        track_ids: vec![group_id],
    });
    // RemoveSlot と同じく、 group_track の fx_chain が削除されるため
    // ungroup も deferred path (state 取得 → Undo snapshot → 実 ungroup)
    // を通る。 fake で AllStatesReceived を流して inner を発火させる。
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
    // ungroup 直後の (2 回目) sync_song_to_plugin_host で LoadSong が再度
    // 送られるが、 これは ClosePluginShmem **より後** であって良い (audio
    // engine 側の処理順としては、 ClosePluginShmem を先に処理しさえすれば
    // race を防げる)。
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
    // 念のため song モデル側も instrument が残っているか。
    let inst_track = &app.song.tracks[0];
    assert!(
        matches!(
            inst_track.instrument,
            Some(PluginInstance { ref plugin_id, .. }) if plugin_id == "test.synth"
        ),
        "instrument PluginInstance still bound to test.synth: {:?}",
        inst_track.instrument
    );
    assert!(
        matches!(inst_track.source, InstrumentSource::None),
        "instrument source is the default; we only loaded the plugin"
    );
}
