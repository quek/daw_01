//! VOICEVOX wav 合成 / 口パク生成の進行状態可視化の回帰テスト。
//!
//! 検証する `AppData` 状態機械:
//! - `VoicevoxSynthStatus` IPC → `voicevox_synth_status` map の busy/failing 更新、
//!   idle かつ非failing の entry 掃除。
//! - `voicevox_engine_unreachable` の閾値判定 (= failing が `VOICEVOX_ENGINE_WARNING`
//!   以上継続したら engine 未接続として警告) と `voicevox_animating` の連動。
//! - `LipsyncGenerated` が generation 不一致 / clips 空でも必ず `lipsync_inflight` を外す。
//! - track → builtin VOICEVOX plugin_id の解決と busy 集計 (`track_wav_synthesizing` /
//!   `voicevox_synth_busy_count`)。

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::model::{Clip, PluginInstance, Track};
use common::plugin_format::PluginFormat;
use common::port_config::PortConfig;
use common::protocol::MainToChild;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, LoadedSlotInfo, VOICEVOX_ENGINE_WARNING};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn build_app() -> (AppData, UnboundedReceiver<MainToChild>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let event_dispatcher_dyn: Arc<dyn BackgroundDispatcher> = event_dispatcher.clone();
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher_dyn,
        job_dispatcher,
        None,
        None,
        48_000, // (A1 r.md #8) test sample rate
    );
    (app, plugin_rx)
}

#[test]
fn synth_status_busy_then_failing_then_unreachable_threshold() {
    let (mut app, _rx) = build_app();
    // busy + failing → entry が立ち failing_since 記録。
    app.handle_event(AppEvent::VoicevoxSynthStatus { plugin_id: 1, busy: true, failing: true });
    let st = app.voicevox_synth_status.get(&1).copied().expect("entry present");
    assert!(st.busy);
    let since = st.failing_since.expect("failing_since set on first failing");

    // 直後 / 閾値未満は「合成中」(警告しない)、animating は true。
    assert!(!app.voicevox_engine_unreachable(since));
    assert!(!app.voicevox_engine_unreachable(since + VOICEVOX_ENGINE_WARNING - Duration::from_millis(1)));
    assert!(app.voicevox_animating(since + Duration::from_secs(1)));

    // 閾値超過で engine 未接続警告へ。警告確定後は animating=false (= static 表示)。
    let warned = since + VOICEVOX_ENGINE_WARNING + Duration::from_millis(1);
    assert!(app.voicevox_engine_unreachable(warned));
    assert!(!app.voicevox_animating(warned));
}

#[test]
fn synth_status_failing_then_success_clears_entry() {
    let (mut app, _rx) = build_app();
    app.handle_event(AppEvent::VoicevoxSynthStatus { plugin_id: 9, busy: true, failing: true });
    assert!(app.voicevox_any_generating());
    // 成功 (busy=false, failing=false) で entry 掃除 → 生成中なし。
    app.handle_event(AppEvent::VoicevoxSynthStatus { plugin_id: 9, busy: false, failing: false });
    assert!(!app.voicevox_synth_status.contains_key(&9));
    assert!(!app.voicevox_any_generating());
    // entry が無ければ未接続警告も出ない。
    assert!(!app.voicevox_engine_unreachable(Instant::now() + Duration::from_secs(100)));
}

#[test]
fn synth_status_busy_without_failing_is_generating_but_never_warns() {
    let (mut app, _rx) = build_app();
    app.handle_event(AppEvent::VoicevoxSynthStatus { plugin_id: 2, busy: true, failing: false });
    let st = app.voicevox_synth_status.get(&2).copied().expect("entry present");
    assert!(st.busy);
    assert!(st.failing_since.is_none(), "failing なしでは failing_since を立てない");
    assert!(app.voicevox_any_generating());
    // failing_since が無いので、いくら時間が経っても未接続警告は出ない。
    assert!(!app.voicevox_engine_unreachable(Instant::now() + Duration::from_secs(3600)));
}

#[test]
fn lipsync_generated_always_clears_inflight_even_when_stale_or_empty() {
    let (mut app, _rx) = build_app();
    // in-flight を直接立てる (= regenerate_lipsync_for_track 相当)。
    app.lipsync_inflight.insert(42);
    assert!(app.voicevox_any_generating());

    // generation 不一致 + clips 空 (= 全 HTTP 失敗) でも、必ず in-flight を外す。
    app.handle_event(AppEvent::LipsyncGenerated {
        vocal_track_id: 7,
        target_track_id: 42,
        bpm: 120.0,
        clips: Vec::new(),
        generation: app.lipsync_gen.wrapping_add(999),
    });
    assert!(!app.lipsync_inflight.contains(&42), "stale/空でも in-flight 解除");
    assert!(!app.voicevox_any_generating());
}

// Track / Clip はモジュール外からリテラル構築不可 (private legacy field) なので
// `default()` + フィールド代入で組む (= field_reassign_with_default は意図的)。
#[allow(clippy::field_reassign_with_default)]
#[test]
fn track_wav_synthesizing_resolves_plugin_id_and_counts_busy() {
    let (mut app, _rx) = build_app();
    // builtin VOICEVOX device を 1 つ持つ vocal track を足す。
    let mut track = Track::default();
    track.id = 100;
    track.name = "Vocal".into();
    track.devices.push(PluginInstance::with_ports(
        common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
        PluginFormat::Builtin,
        PortConfig { has_note_input: true, has_audio_output: true, ..Default::default() },
    ));
    let mut clip = Clip::default();
    clip.id = 1;
    clip.length_beats = 4.0;
    track.clips.push(clip);
    app.song.tracks.push(track);
    // host が割り当てた plugin_id を device index 0 に紐付け (= SlotPluginLoaded 相当)。
    app.loaded_slots.insert(
        (100, 0),
        LoadedSlotInfo {
            plugin_id: 5,
            plugin_id_str: common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
        },
    );

    // status 未受信ならどちらも非生成。
    assert!(!app.track_wav_synthesizing(100));
    assert_eq!(app.voicevox_synth_busy_count(), 0);

    // plugin_id=5 が busy → そのトラックが合成中、件数 1。
    app.handle_event(AppEvent::VoicevoxSynthStatus { plugin_id: 5, busy: true, failing: false });
    assert!(app.track_wav_synthesizing(100));
    assert_eq!(app.voicevox_synth_busy_count(), 1);

    // idle に戻ると 0 件 (entry も掃除される)。
    app.handle_event(AppEvent::VoicevoxSynthStatus { plugin_id: 5, busy: false, failing: false });
    assert!(!app.track_wav_synthesizing(100));
    assert_eq!(app.voicevox_synth_busy_count(), 0);
}

/// 口パク生成中判定 (= 出力先 口 track が `lipsync_inflight` に居る)。per-clip の
/// `auto_lipsync` gate は view 側 (`draw_clip_synth_spinner`) なのでここでは集合のみ検証。
#[test]
fn lipsync_target_generating_tracks_inflight_set() {
    let (mut app, _rx) = build_app();
    assert!(!app.lipsync_target_generating(200));
    app.lipsync_inflight.insert(200);
    assert!(app.lipsync_target_generating(200));
    assert!(app.voicevox_any_generating());
    // 別 target は無関係。
    assert!(!app.lipsync_target_generating(201));
}
