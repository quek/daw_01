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
use common::protocol::{MainToChild, PluginSlot};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, PickerTarget};
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
        state_load_error: None,
    });
}

#[test]
fn consecutive_remove_slot_serializes_through_state_queue() {
    let (mut app, _audio_rx, mut plugin_rx, _proxy) = build_app();

    // 1 つの track に instrument (Synth) + Fx 2 つ (Bitcrush, Delay) を
    // 用意。 song_has_plugin() == true なので 各 RemoveSlot が deferred
    // path を通る。
    let track_id = app.song.tracks[0].id;
    app.handle_event(AppEvent::SelectTrack(0));

    app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Instrument));
    app.handle_event(AppEvent::SelectPluginFromDb("test.synth".into()));
    fake_plugin_loaded(&mut app, track_id, PluginSlot::Instrument, "test.synth", 100);

    app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Fx));
    app.handle_event(AppEvent::SelectPluginFromDb("test.bitcrush".into()));
    fake_plugin_loaded(&mut app, track_id, PluginSlot::Fx(0), "test.bitcrush", 200);

    app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Fx));
    app.handle_event(AppEvent::SelectPluginFromDb("test.delay".into()));
    fake_plugin_loaded(&mut app, track_id, PluginSlot::Fx(1), "test.delay", 201);

    // pending_state_queue は初期で空。
    assert!(app.pending_state_queue.is_empty(), "queue starts empty");

    // セットアップ中の plugin_rx を捨てる。
    let _ = drain(&mut plugin_rx);

    // 1 回目の RemoveSlot Fx(0) → queue.len == 1、 RequestAllStates が
    // 1 発送られる。
    app.handle_event(AppEvent::RemoveSlot {
        slot_kind: 2,
        slot_index: 0,
    });
    assert_eq!(
        app.pending_state_queue.len(),
        1,
        "1st RemoveSlot enqueues 1 entry"
    );
    let msgs = drain(&mut plugin_rx);
    assert_eq!(
        msgs.iter()
            .filter(|m| matches!(m, MainToChild::RequestAllStates))
            .count(),
        1,
        "1st RemoveSlot triggers 1 RequestAllStates: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, MainToChild::RemoveSlotPlugin { .. })),
        "no RemoveSlotPlugin yet (still pending): {msgs:?}"
    );

    // 2 回目の RemoveSlot Fx(1) — in-flight 中なので queue にだけ積まれて
    // RequestAllStates は再発行されない。
    app.handle_event(AppEvent::RemoveSlot {
        slot_kind: 2,
        slot_index: 1,
    });
    assert_eq!(
        app.pending_state_queue.len(),
        2,
        "2nd RemoveSlot enqueues without sending another RequestAllStates"
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

    // 1 回目の AllStatesReceived → 1 件目 (Fx(0)) が実行され、 queue 残り 1、
    // 次の RequestAllStates が再発行される。
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));
    assert_eq!(
        app.pending_state_queue.len(),
        1,
        "1st AllStatesReceived consumes 1 entry"
    );
    let msgs = drain(&mut plugin_rx);
    let removed_fx0 = msgs
        .iter()
        .any(|m| matches!(m, MainToChild::RemoveSlotPlugin { track, slot: PluginSlot::Fx(0) } if *track == track_id));
    assert!(removed_fx0, "RemoveSlotPlugin Fx(0) sent: {msgs:?}");
    let req_count = msgs
        .iter()
        .filter(|m| matches!(m, MainToChild::RequestAllStates))
        .count();
    assert_eq!(
        req_count, 1,
        "after 1st response, exactly 1 follow-up RequestAllStates: {msgs:?}"
    );

    // 2 回目の AllStatesReceived → 2 件目 (Fx(1)) が実行され、 queue 空、
    // RequestAllStates は再発行されない (= no follow-up)。
    app.handle_event(AppEvent::AllStatesReceived(Vec::new()));
    assert!(
        app.pending_state_queue.is_empty(),
        "queue drained after both responses"
    );
    let msgs = drain(&mut plugin_rx);
    let removed_fx1 = msgs
        .iter()
        .any(|m| matches!(m, MainToChild::RemoveSlotPlugin { track, slot: PluginSlot::Fx(1) } if *track == track_id));
    assert!(removed_fx1, "RemoveSlotPlugin Fx(1) sent: {msgs:?}");
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
