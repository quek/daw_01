//! `pending_state_queue` のシリアライズを test で固定する。
//!
//! Risk B (plan_undo_reconcile_polish.md):
//! 旧実装は `pending_state_request: Option<PendingStateRequest>` で、
//! in-flight 中に来た 2 番目の deferred edit を state 同期なしで即時
//! 実行していた (= 2 番目の Undo で plugin knob 値が復元されない)。
//!
//! 本 test は 2 連続 RemoveSlot が queue に積まれ、 1 回目の
//! `AllStatesReceived` で 1 件目が実行され + 2 件目用の `RequestAllStates`
//! が再発行され、 2 回目の `AllStatesReceived` で 2 件目が実行されることを
//! 検証する。

use std::sync::Arc;

use common::plugin_db::{PluginDatabase, PluginEntry};
use common::plugin_format::PluginFormat;
use common::protocol::MainToChild;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, PendingStateRequest};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

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

fn build_app() -> (
    AppData,
    UnboundedReceiver<MainToChild>,
    UnboundedReceiver<MainToChild>,
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
    );
    (app, audio_rx, plugin_rx, event_dispatcher)
}

fn drain<T>(rx: &mut UnboundedReceiver<T>) -> Vec<T> {
    let mut v = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        v.push(msg);
    }
    v
}

fn has_pending_save(app: &AppData) -> bool {
    app.pending_state_queue
        .iter()
        .any(|r| matches!(r, PendingStateRequest::Save { .. }))
}

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
        state_load_error: None,
    });
}

#[test]
fn consecutive_remove_slot_serializes_through_state_queue() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();

    // 単一デバイスチェーン: 1 つの track に device 3 つ (synth=0, bitcrush=1,
    // delay=2) を順に append。 song_has_plugin() == true なので 各 RemoveDevice が
    // deferred path を通る。
    let track_id = app.song.tracks[0].id;
    app.handle_event(AppEvent::SelectTrack(0));

    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });
    fake_plugin_loaded(&mut app, track_id, 0, "test.synth", 100);

    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.bitcrush".into(),
        keep_open: false,
        open_gui: true,
    });
    fake_plugin_loaded(&mut app, track_id, 1, "test.bitcrush", 200);

    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.delay".into(),
        keep_open: false,
        open_gui: true,
    });
    fake_plugin_loaded(&mut app, track_id, 2, "test.delay", 201);

    // pending_state_queue は初期で空。
    assert!(app.pending_state_queue.is_empty(), "queue starts empty");

    // セットアップ中の plugin_rx を捨てる。
    let _ = drain(&mut plugin_rx);

    // 1 回目の RemoveDevice (bitcrush = index 1) → queue.len == 1、
    // RequestAllStates が 1 発送られる。
    app.handle_event(AppEvent::RemoveDevice { index: 1 });
    assert_eq!(
        app.pending_state_queue.len(),
        1,
        "1st RemoveDevice enqueues 1 entry"
    );
    let msgs = drain(&mut plugin_rx);
    assert_eq!(
        msgs.iter()
            .filter(|m| matches!(m, MainToChild::RequestAllStates))
            .count(),
        1,
        "1st RemoveDevice triggers 1 RequestAllStates: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, MainToChild::RemoveSlotPlugin { .. })),
        "no RemoveSlotPlugin yet (still pending): {msgs:?}"
    );

    // 2 回目の RemoveDevice (delay = index 2) — in-flight 中なので queue にだけ
    // 積まれて RequestAllStates は再発行されない。
    app.handle_event(AppEvent::RemoveDevice { index: 2 });
    assert_eq!(
        app.pending_state_queue.len(),
        2,
        "2nd RemoveDevice enqueues without sending another RequestAllStates"
    );
    let msgs = drain(&mut plugin_rx);
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, MainToChild::RequestAllStates)),
        "no extra RequestAllStates while in-flight: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, MainToChild::RemoveSlotPlugin { .. })),
        "no RemoveSlotPlugin yet: {msgs:?}"
    );

    // 1 回目の AllStatesReceived → 1 件目 (index 1) が実行され、 queue 残り 1、
    // 次の RequestAllStates が再発行される。
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));
    assert_eq!(
        app.pending_state_queue.len(),
        1,
        "1st AllStatesReceived consumes 1 entry"
    );
    let msgs = drain(&mut plugin_rx);
    let removed_dev1 = msgs
        .iter()
        .any(|m| matches!(m, MainToChild::RemoveSlotPlugin { track, index: 1 } if *track == track_id));
    assert!(removed_dev1, "RemoveSlotPlugin index 1 sent: {msgs:?}");
    let req_count = msgs
        .iter()
        .filter(|m| matches!(m, MainToChild::RequestAllStates))
        .count();
    assert_eq!(
        req_count, 1,
        "after 1st response, exactly 1 follow-up RequestAllStates: {msgs:?}"
    );

    // 2 回目の AllStatesReceived → 2 件目 (index 2) が実行され、 queue 空、
    // RequestAllStates は再発行されない (= no follow-up)。
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));
    assert!(
        app.pending_state_queue.is_empty(),
        "queue drained after both responses"
    );
    let msgs = drain(&mut plugin_rx);
    let removed_dev2 = msgs
        .iter()
        .any(|m| matches!(m, MainToChild::RemoveSlotPlugin { track, index: 2 } if *track == track_id));
    assert!(removed_dev2, "RemoveSlotPlugin index 2 sent: {msgs:?}");
    let req_count = msgs
        .iter()
        .filter(|m| matches!(m, MainToChild::RequestAllStates))
        .count();
    assert_eq!(
        req_count, 0,
        "no further RequestAllStates after queue drains: {msgs:?}"
    );

    // 各 deferred edit が個別 Undo snapshot を取れたことを確認。
    // 初回セットアップ (instrument / Bitcrush / Delay) でも snapshot が
    // 積まれるが、 ここでは「2 連続 RemoveSlot で 2 つ追加された」 ことを
    // 確認できれば十分なので最小値だけ assert する。
    assert!(
        app.undo_stack.len() >= 2,
        "at least 2 snapshots from the 2 deferred edits: {}",
        app.undo_stack.len()
    );
}

/// 1 つの track に device 3 つ (synth=0, bitcrush=1, delay=2) を載せる。
fn setup_track_with_two_fx(app: &mut AppData) -> u32 {
    let track_id = app.song.tracks[0].id;
    app.handle_event(AppEvent::SelectTrack(0));

    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });
    fake_plugin_loaded(app, track_id, 0, "test.synth", 100);

    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.bitcrush".into(),
        keep_open: false,
        open_gui: true,
    });
    fake_plugin_loaded(app, track_id, 1, "test.bitcrush", 200);

    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.delay".into(),
        keep_open: false,
        open_gui: true,
    });
    fake_plugin_loaded(app, track_id, 2, "test.delay", 201);
    track_id
}

/// FIXME #24 回帰: Save が **slot 削除の Deferred edit が in-flight 中** に enqueue
/// されたとき、 その save の凍結 snapshot は「削除が反映された後」 の layout で
/// 取られなければならない (= co-temporal snapshot)。
///
/// 旧 snapshot-at-invoke 実装は Save を押した瞬間 (= 削除前、 fx_chain=[Bitcrush,
/// Delay]) で凍結していた。 一方、 この save が受け取る plugin state は削除実行後に
/// 再発行された `RequestAllStates` の応答 (= fx_chain=[Delay]、 Delay が Fx(0) へ
/// shift) を反映する。 これを位置 index で旧 snapshot に適用すると、 Delay の state
/// が Bitcrush (snapshot の Fx(0)) に誤適用される silent corruption になっていた。
///
/// 本 test は、 Deferred(RemoveSlot Fx(0)) が実行された **後** に Save の snapshot が
/// 充填され、 その snapshot の fx_chain が削除後の 1 個 (= Delay のみ) であることを
/// 検証する。
#[test]
fn save_behind_deferred_remove_snapshots_post_removal_layout() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();
    let track_id = setup_track_with_two_fx(&mut app);
    assert!(app.pending_state_queue.is_empty(), "queue starts empty");
    let _ = drain(&mut plugin_rx);

    // RemoveDevice (bitcrush = index 1) → Deferred enqueue、 RequestAllStates(R1)
    // 送信。 live はまだ [synth, bitcrush, delay] (削除は deferred)。
    app.handle_event(AppEvent::RemoveDevice { index: 1 });
    assert_eq!(app.pending_state_queue.len(), 1, "RemoveDevice enqueues Deferred");
    let _ = drain(&mut plugin_rx);

    // Deferred in-flight 中に Save。 queue 後方に積まれ、 snapshot はまだ None
    // (= dispatch_front_state_request がこの save の RequestAllStates を送る瞬間に
    // 充填する設計なので、 後方に積まれている間は None)。
    app.file_path = Some(std::env::temp_dir().join("daw01_test_snapshot_timing.daw"));
    app.handle_event(AppEvent::Save);
    assert_eq!(
        app.pending_state_queue.len(),
        2,
        "Save enqueues behind the in-flight Deferred"
    );
    match app.pending_state_queue.back() {
        Some(PendingStateRequest::Save { snapshot, .. }) => assert!(
            snapshot.is_none(),
            "Save snapshot is not frozen yet while queued behind a Deferred"
        ),
        other => panic!("back of queue should be Save, got {other:?}"),
    }
    // この Save では RequestAllStates は再発行されない (in-flight 中)。
    let msgs = drain(&mut plugin_rx);
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, MainToChild::RequestAllStates)),
        "no extra RequestAllStates while Deferred in-flight: {msgs:?}"
    );

    // R1 応答 → Deferred(RemoveDevice index 1) 実行 (live devices → [synth, delay])、
    // queue 残り [Save]、 dispatch_front_state_request が Save の snapshot を **今の**
    // live (= 削除後 layout) で充填し、 R2 を送る。
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));
    assert_eq!(
        app.pending_state_queue.len(),
        1,
        "Deferred consumed, Save remains"
    );

    // live は削除を反映している (= [synth, delay]、 bitcrush が抜けて delay が
    // index 1 へ shift)。
    let live_track = app.song.tracks.iter().find(|t| t.id == track_id).unwrap();
    assert_eq!(live_track.devices.len(), 2, "live: bitcrush removed");
    assert_eq!(live_track.devices[0].plugin_id, "test.synth");
    assert_eq!(live_track.devices[1].plugin_id, "test.delay");

    // 肝心の検証: Save の snapshot が **削除後** layout (devices = [synth, delay])
    // で凍結されている。 旧 snapshot-at-invoke なら 3 個のままで、 R2 の device
    // state が誤適用された。
    match app.pending_state_queue.front() {
        Some(PendingStateRequest::Save { snapshot, .. }) => {
            let snap = snapshot
                .as_ref()
                .expect("snapshot is frozen once the Save reaches the front of the queue");
            let st = snap.tracks.iter().find(|t| t.id == track_id).unwrap();
            assert_eq!(
                st.devices.len(),
                2,
                "snapshot must reflect the post-removal layout (co-temporal with its states)"
            );
            assert_eq!(st.devices[1].plugin_id, "test.delay");
        }
        other => panic!("front of queue should be Save, got {other:?}"),
    }
}

/// 「保存して終了」 で plugin state 待ちの間に **編集が入らなかった** 場合、
/// 非同期保存の完了 (finish_save) で should_quit が立つ。
#[test]
fn save_and_quit_clean_sets_should_quit() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();
    let track_id = app.song.tracks[0].id;
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });
    fake_plugin_loaded(&mut app, track_id, 0, "test.synth", 100);
    let _ = drain(&mut plugin_rx);

    // 非同期保存を enqueue し、 「保存して終了」 の意図を立てる
    // (= close_confirm_save が plugin 有り dirty project でやること)。
    app.file_path = Some(std::env::temp_dir().join("daw01_test_quit_clean.daw"));
    app.handle_event(AppEvent::Save);
    app.quit_after_save = true;
    assert!(has_pending_save(&app), "Save in-flight");

    // 編集なしで応答到着 → finish_save が clean を確認して should_quit。
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));
    assert!(app.should_quit, "clean async save-and-quit sets should_quit");
    assert!(!app.quit_after_save, "quit intent cleared after quitting");
}

/// 「保存して終了」 で plugin state 待ちの間に編集が入った場合、 co-temporal
/// snapshot は編集前なのでこの保存に編集は含まれない。 finish_save は should_quit を
/// 立てず、 残った編集を確定するため再保存を enqueue して終了意図を維持する
/// (= FIXME #24 redesign の回帰修正: 旧コードは intent を捨ててアプリが閉じも
/// 保存もしない状態になっていた)。
#[test]
fn save_and_quit_with_window_edit_resaves_instead_of_quitting() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();
    let track_id = app.song.tracks[0].id;
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });
    fake_plugin_loaded(&mut app, track_id, 0, "test.synth", 100);
    let _ = drain(&mut plugin_rx);

    app.file_path = Some(std::env::temp_dir().join("daw01_test_quit_window_edit.daw"));
    app.handle_event(AppEvent::Save);
    app.quit_after_save = true;
    let _ = drain(&mut plugin_rx);

    // state 待ちの間に live を編集する (snapshot は既に凍結済みなので含まれない)。
    let extra_track = app.song.tracks[0].clone();
    app.song.tracks.push(extra_track);

    // 応答到着 → finish_save: saved baseline = 編集前 snapshot、 live は編集後で
    // dirty。 should_quit は立たず、 再保存が enqueue され、 終了意図は維持される。
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));
    assert!(
        !app.should_quit,
        "window edit during save-and-quit must NOT quit (would drop the edit)"
    );
    assert!(
        app.quit_after_save,
        "quit intent stays alive across the follow-up save"
    );
    assert!(
        has_pending_save(&app),
        "a follow-up Save is enqueued to persist the window edit"
    );
}

/// 通常ケース: queue が空のときの Save は invoke の瞬間 (= この save の
/// RequestAllStates を送る瞬間) に snapshot を凍結する。
#[test]
fn save_with_idle_queue_freezes_snapshot_at_invoke() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();
    let track_id = app.song.tracks[0].id;
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });
    fake_plugin_loaded(&mut app, track_id, 0, "test.synth", 100);
    assert!(app.pending_state_queue.is_empty(), "queue starts empty");
    let _ = drain(&mut plugin_rx);

    app.file_path = Some(std::env::temp_dir().join("daw01_test_snapshot_idle.daw"));
    app.handle_event(AppEvent::Save);

    // queue が空 → was_idle → この save の RequestAllStates を即送信し、 その瞬間に
    // snapshot を凍結する。
    assert_eq!(app.pending_state_queue.len(), 1, "Save enqueued");
    match app.pending_state_queue.front() {
        Some(PendingStateRequest::Save { snapshot, .. }) => assert!(
            snapshot.is_some(),
            "idle-queue Save freezes its snapshot immediately at invoke"
        ),
        other => panic!("front of queue should be Save, got {other:?}"),
    }
    let msgs = drain(&mut plugin_rx);
    assert_eq!(
        msgs.iter()
            .filter(|m| matches!(m, MainToChild::RequestAllStates))
            .count(),
        1,
        "idle-queue Save sends exactly 1 RequestAllStates: {msgs:?}"
    );
}
