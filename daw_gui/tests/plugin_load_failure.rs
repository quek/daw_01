//! Integration test: plugin_host で `SetSlotPlugin` の load が失敗したとき、
//! daw_gui が `pending_plugin_loads` を解放し、 queue 中の Play を flush
//! する流れを検証する。 failure 通知 (= `SlotPluginLoadFailed`) が来ない
//! と pending stuck で再生不能になる A8 の core 動作。

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
                id: "test.fx".into(),
                format: PluginFormat::Clap,
                name: "Test FX".into(),
                vendor: "Test".into(),
                version: "1.0".into(),
                features: vec![],
                path: "C:/fake/fx.clap".into(),
                descriptor_index: 0,
            },
        ],
        scanned_at: None,
    })
}

fn build_app() -> (
    AppData,
    UnboundedReceiver<MainToChild>, // audio_rx
    UnboundedReceiver<MainToChild>, // plugin_rx
) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        Some(make_plugin_db()),
        event_dispatcher,
        job_dispatcher,
        None,
        // app_dirs: None = 永続化なし。 実 %LOCALAPPDATA%/daw_01/recent*.json を汚染しない。
        None,
    );
    (app, audio_rx, plugin_rx)
}

fn drain<T>(rx: &mut UnboundedReceiver<T>) -> Vec<T> {
    let mut v = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        v.push(msg);
    }
    v
}

/// 単一 pending → 失敗通知 1 回 → pending 解放 + queue Play flush。
#[test]
fn load_failure_releases_single_pending_and_flushes_play() {
    let (mut app, mut audio_rx, mut plugin_rx) = build_app();
    let track_id = app.song.tracks[0].id;

    // 1. instrument を picker からロード → SetSlotPlugin 送信、 pending に entry。
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Instrument));
    app.handle_event(AppEvent::SelectPluginFromDb("test.synth".into()));

    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            MainToChild::SetSlotPlugin { track, slot: PluginSlot::Instrument, .. }
            if *track == track_id
        )),
        "SetSlotPlugin should be sent: {plugin_msgs:?}"
    );
    assert!(
        app.pending_plugin_loads
            .contains(&(track_id, PluginSlot::Instrument)),
        "pending_plugin_loads should contain the slot after track_pending_load"
    );

    // 2. Play を押す → pending があるので queue 化 (pending_play=true,
    //    is_playing は false のまま、 audio に Play は送らない)。
    let _ = drain(&mut audio_rx);
    app.handle_event(AppEvent::Play);
    let audio_msgs_before_failure = drain(&mut audio_rx);
    assert!(
        !audio_msgs_before_failure
            .iter()
            .any(|m| matches!(m, MainToChild::Play)),
        "Play should be queued, not sent yet: {audio_msgs_before_failure:?}"
    );
    assert!(app.pending_play, "pending_play should be true while waiting");
    assert!(!app.is_playing, "is_playing should be false while queued");

    // 3. plugin_host から load failure 通知が届いた fake dispatch。
    app.handle_event(AppEvent::SlotPluginLoadFailedFromChild {
        track: track_id,
        slot: PluginSlot::Instrument,
        plugin_id: "test.synth".into(),
        reason: "fake load failed".into(),
    });

    // 4. pending 解放 + queue Play flush + status_message に失敗内容。
    assert!(
        !app.pending_plugin_loads
            .contains(&(track_id, PluginSlot::Instrument)),
        "pending_plugin_loads should be cleared after failure"
    );
    assert!(
        app.pending_plugin_loads.is_empty(),
        "pending should be empty: {:?}",
        app.pending_plugin_loads
    );
    assert!(!app.pending_play, "pending_play should be cleared");
    assert!(app.is_playing, "play() should have fired");
    let audio_msgs_after_failure = drain(&mut audio_rx);
    assert!(
        audio_msgs_after_failure
            .iter()
            .any(|m| matches!(m, MainToChild::Play)),
        "Play should be flushed to audio: {audio_msgs_after_failure:?}"
    );
    assert!(
        app.status_message.contains("失敗"),
        "status_message should announce failure: {:?}",
        app.status_message
    );
    assert!(
        app.status_message.contains("test.synth"),
        "status_message should include plugin id: {:?}",
        app.status_message
    );
}

/// 2 pending → 1 件失敗 → 残 1 件 pending のまま Play は flush されない。
#[test]
fn load_failure_keeps_other_pending_unaffected() {
    let (mut app, mut audio_rx, mut plugin_rx) = build_app();
    let track_id = app.song.tracks[0].id;

    // instrument + Fx の 2 つを順次ロード → pending 2 件。
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Instrument));
    app.handle_event(AppEvent::SelectPluginFromDb("test.synth".into()));
    app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Fx));
    app.handle_event(AppEvent::SelectPluginFromDb("test.fx".into()));
    let _ = drain(&mut plugin_rx);

    assert_eq!(
        app.pending_plugin_loads.len(),
        2,
        "expected 2 pending: {:?}",
        app.pending_plugin_loads
    );

    // Play を queue。
    let _ = drain(&mut audio_rx);
    app.handle_event(AppEvent::Play);
    assert!(app.pending_play);
    assert!(!app.is_playing);

    // Instrument だけ失敗。 Fx の pending はそのまま残る。
    app.handle_event(AppEvent::SlotPluginLoadFailedFromChild {
        track: track_id,
        slot: PluginSlot::Instrument,
        plugin_id: "test.synth".into(),
        reason: "fake load failed".into(),
    });

    assert!(
        !app.pending_plugin_loads
            .contains(&(track_id, PluginSlot::Instrument)),
        "Instrument should be cleared"
    );
    assert!(
        app.pending_plugin_loads
            .contains(&(track_id, PluginSlot::Fx(0))),
        "Fx pending should remain"
    );
    assert!(
        app.pending_play,
        "pending_play should still be true while Fx is loading"
    );
    assert!(!app.is_playing, "is_playing should still be false");
    let audio_msgs = drain(&mut audio_rx);
    assert!(
        !audio_msgs.iter().any(|m| matches!(m, MainToChild::Play)),
        "Play should NOT yet be flushed: {audio_msgs:?}"
    );
    // status_message は失敗内容 + 残数表示。
    assert!(
        app.status_message.contains("失敗"),
        "status_message should announce failure: {:?}",
        app.status_message
    );
    assert!(
        app.status_message.contains("残 1"),
        "status_message should include remaining count: {:?}",
        app.status_message
    );
}
