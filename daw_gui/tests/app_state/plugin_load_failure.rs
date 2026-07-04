//! Integration test: plugin_host で `SetSlotPlugin` の load が失敗したとき、
//! daw_gui が `pending_plugin_loads` を解放し、 queue 中の Play を flush
//! する流れを検証する。 failure 通知 (= `SlotPluginLoadFailed`) が来ない
//! と pending stuck で再生不能になる A8 の core 動作。
//!
//! v29: pending は `device_id → 要求 generation` の map。 失敗通知は
//! 最新 generation の echo だけ受理される (stale 応答 guard)。

use common::protocol::{AudioCommand, PluginCommand, PluginEvent};
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::app::{device_id_at, AppData, AppEvent};

use super::support::{self, drain};

/// 旧独立バイナリ時代のシグネチャを保つ thin adapter (dispatcher はここで drop)。
fn build_app() -> (
    AppData,
    UnboundedReceiver<AudioCommand>,  // audio_rx
    UnboundedReceiver<PluginCommand>, // plugin_rx
) {
    let (app, audio_rx, plugin_rx, _dispatcher) = support::build_app();
    (app, audio_rx, plugin_rx)
}

/// pending に登録された要求 generation を引く (fake failure の echo 用)。
fn pending_generation(app: &AppData, device_id: u64) -> u64 {
    app.ipc.pending_plugin_loads
        .get(&device_id)
        .copied()
        .expect("device should be pending")
}

/// 単一 pending → 失敗通知 1 回 → pending 解放 + queue Play flush。
#[test]
fn load_failure_releases_single_pending_and_flushes_play() {
    let (mut app, mut audio_rx, mut plugin_rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;

    // 1. instrument を picker からロード → SetSlotPlugin 送信、 pending に entry。
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });

    // 単一デバイスチェーン: picker は末尾 append、 空チェーンなので index 0。
    let synth_dev = device_id_at(app.song_doc.song(), track_id, 0).expect("device id allocated");
    let plugin_msgs = drain(&mut plugin_rx);
    assert!(
        plugin_msgs.iter().any(|m| matches!(
            m,
            PluginCommand::SetSlotPlugin { device_id, track_id: t, .. }
            if *device_id == synth_dev && *t == track_id
        )),
        "SetSlotPlugin should be sent: {plugin_msgs:?}"
    );
    assert!(
        app.ipc.pending_plugin_loads.contains_key(&synth_dev),
        "pending_plugin_loads should contain the device after track_pending_load"
    );

    // 2. Play を押す → pending があるので queue 化 (pending_play=true,
    //    is_playing は false のまま、 audio に Play は送らない)。
    let _ = drain(&mut audio_rx);
    app.handle_event(AppEvent::Play);
    let audio_msgs_before_failure = drain(&mut audio_rx);
    assert!(
        !audio_msgs_before_failure
            .iter()
            .any(|m| matches!(m, AudioCommand::Play)),
        "Play should be queued, not sent yet: {audio_msgs_before_failure:?}"
    );
    assert!(app.transport.pending_play, "pending_play should be true while waiting");
    assert!(!app.transport.is_playing, "is_playing should be false while queued");

    // 3. plugin_host から load failure 通知が届いた fake dispatch。
    let generation = pending_generation(&app, synth_dev);
    app.handle_event(AppEvent::Plugin(PluginEvent::SlotPluginLoadFailed {
        device_id: synth_dev,
        plugin_id: "test.synth".into(),
        reason: "fake load failed".into(),
        generation,
    }));

    // 4. pending 解放 + queue Play flush + status_message に失敗内容。
    assert!(
        !app.ipc.pending_plugin_loads.contains_key(&synth_dev),
        "pending_plugin_loads should be cleared after failure"
    );
    assert!(
        app.ipc.pending_plugin_loads.is_empty(),
        "pending should be empty: {:?}",
        app.ipc.pending_plugin_loads
    );
    assert!(!app.transport.pending_play, "pending_play should be cleared");
    assert!(app.transport.is_playing, "play() should have fired");
    let audio_msgs_after_failure = drain(&mut audio_rx);
    assert!(
        audio_msgs_after_failure
            .iter()
            .any(|m| matches!(m, AudioCommand::Play)),
        "Play should be flushed to audio: {audio_msgs_after_failure:?}"
    );
    assert!(
        app.ui_ephemeral.status_message.contains("失敗"),
        "status_message should announce failure: {:?}",
        app.ui_ephemeral.status_message
    );
    assert!(
        app.ui_ephemeral.status_message.contains("test.synth"),
        "status_message should include plugin id: {:?}",
        app.ui_ephemeral.status_message
    );
}

/// 2 pending → 1 件失敗 → 残 1 件 pending のまま Play は flush されない。
#[test]
fn load_failure_keeps_other_pending_unaffected() {
    let (mut app, mut audio_rx, mut plugin_rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;

    // instrument + Fx の 2 つを順次ロード → pending 2 件。
    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.fx".into(),
        keep_open: false,
        open_gui: true,
    });
    let _ = drain(&mut plugin_rx);
    let synth_dev = device_id_at(app.song_doc.song(), track_id, 0).expect("synth device id");
    let fx_dev = device_id_at(app.song_doc.song(), track_id, 1).expect("fx device id");

    assert_eq!(
        app.ipc.pending_plugin_loads.len(),
        2,
        "expected 2 pending: {:?}",
        app.ipc.pending_plugin_loads
    );

    // Play を queue。
    let _ = drain(&mut audio_rx);
    app.handle_event(AppEvent::Play);
    assert!(app.transport.pending_play);
    assert!(!app.transport.is_playing);

    // device 0 (test.synth) だけ失敗。 device 1 (test.fx) の pending は残る。
    let generation = pending_generation(&app, synth_dev);
    app.handle_event(AppEvent::Plugin(PluginEvent::SlotPluginLoadFailed {
        device_id: synth_dev,
        plugin_id: "test.synth".into(),
        reason: "fake load failed".into(),
        generation,
    }));

    assert!(
        !app.ipc.pending_plugin_loads.contains_key(&synth_dev),
        "device 0 should be cleared"
    );
    assert!(
        app.ipc.pending_plugin_loads.contains_key(&fx_dev),
        "device 1 (fx) pending should remain"
    );
    assert!(
        app.transport.pending_play,
        "pending_play should still be true while Fx is loading"
    );
    assert!(!app.transport.is_playing, "is_playing should still be false");
    let audio_msgs = drain(&mut audio_rx);
    assert!(
        !audio_msgs.iter().any(|m| matches!(m, AudioCommand::Play)),
        "Play should NOT yet be flushed: {audio_msgs:?}"
    );
    // status_message は失敗内容 + 残数表示。
    assert!(
        app.ui_ephemeral.status_message.contains("失敗"),
        "status_message should announce failure: {:?}",
        app.ui_ephemeral.status_message
    );
    assert!(
        app.ui_ephemeral.status_message.contains("残 1"),
        "status_message should include remaining count: {:?}",
        app.ui_ephemeral.status_message
    );
}

/// v29 世代 guard: 古い generation の失敗応答は無視される (A→B 連続差し替えの
/// stale 応答 race 対策)。
#[test]
fn stale_generation_failure_is_ignored() {
    let (mut app, _audio_rx, mut plugin_rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;

    app.handle_event(AppEvent::SelectTrack(0));
    app.handle_event(AppEvent::OpenPluginPicker);
    app.handle_event(AppEvent::SelectPluginFromDb {
        id: "test.synth".into(),
        keep_open: false,
        open_gui: true,
    });
    let _ = drain(&mut plugin_rx);
    let synth_dev = device_id_at(app.song_doc.song(), track_id, 0).expect("device id");
    let generation = pending_generation(&app, synth_dev);

    // 古い世代 (generation - 1 相当 = 別の値) の失敗が遅れて届いた fake。
    app.handle_event(AppEvent::Plugin(PluginEvent::SlotPluginLoadFailed {
        device_id: synth_dev,
        plugin_id: "test.synth".into(),
        reason: "stale failure".into(),
        generation: generation.wrapping_add(1000),
    }));

    // pending は解放されない (最新世代の応答待ちを維持)。
    assert!(
        app.ipc.pending_plugin_loads.contains_key(&synth_dev),
        "stale-generation failure must not clear the pending entry"
    );
}
