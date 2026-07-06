//! r.md #11: フェーダーを 0dB (unity) より上げても 0 に戻らない — トラック音量 /
//! マスターゲインが +6dB (`MAX_TRACK_GAIN` = amp 2.0) まで保持され、 それ以上は
//! 上限 clamp されることの headless 回帰。 旧実装は `clamp(0.0, 1.0)` で unity 超を
//! 即 0dB に潰し、 フェーダーが上げた直後に戻って見えた。

use std::sync::Arc;

use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn build_app() -> (
    AppData,
    UnboundedReceiver<AudioCommand>,
    UnboundedReceiver<PluginCommand>,
) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    (app, audio_rx, plugin_rx)
}

fn track_volume(app: &AppData, track_id: u32) -> f32 {
    app.song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.id == track_id)
        .expect("track exists")
        .volume
}

#[test]
fn track_volume_keeps_boost_above_unity() {
    let (mut app, _a, _p) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;

    // +3dB 相当 (amp ≈ 1.41): 旧 unity clamp では 1.0 に潰れて 0dB に戻った。
    app.handle_event(AppEvent::SetTrackVolume { track: track_id, amp: 1.41 });
    assert!(
        (track_volume(&app, track_id) - 1.41).abs() < 1e-6,
        "unity 超の音量が保持される (got {})",
        track_volume(&app, track_id)
    );

    // 上端 +6dB (amp 2.0 = MAX_TRACK_GAIN) も保持。
    app.handle_event(AppEvent::SetTrackVolume { track: track_id, amp: 2.0 });
    assert!(
        (track_volume(&app, track_id) - 2.0).abs() < 1e-6,
        "+6dB (amp 2.0) が保持される (got {})",
        track_volume(&app, track_id)
    );

    // それ以上は上限 clamp (自動化描画等が範囲外を送っても安全)。
    app.handle_event(AppEvent::SetTrackVolume { track: track_id, amp: 5.0 });
    assert!(
        (track_volume(&app, track_id) - common::model::MAX_TRACK_GAIN).abs() < 1e-6,
        "上限は MAX_TRACK_GAIN (got {})",
        track_volume(&app, track_id)
    );

    // 下限 0 (無音) は据え置き。
    app.handle_event(AppEvent::SetTrackVolume { track: track_id, amp: -1.0 });
    assert_eq!(track_volume(&app, track_id), 0.0, "下限は 0");
}

#[test]
fn master_gain_keeps_boost_above_unity() {
    let (mut app, _a, _p) = build_app();

    app.handle_event(AppEvent::SetMasterGain(1.5));
    assert!(
        (app.transport.master_gain - 1.5).abs() < 1e-6,
        "master のブーストが保持される (got {})",
        app.transport.master_gain
    );

    app.handle_event(AppEvent::SetMasterGain(5.0));
    assert!(
        (app.transport.master_gain - common::model::MAX_TRACK_GAIN).abs() < 1e-6,
        "master 上限は MAX_TRACK_GAIN (got {})",
        app.transport.master_gain
    );
}
