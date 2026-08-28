//! VOICEVOX wav 合成 / 口パク生成の進行状態可視化の回帰テスト。
//!
//! 検証する `AppData` 状態機械:
//! - `VoicevoxSynthStatus` IPC → `voicevox_synth_status` map の busy/failing/進捗更新、
//!   idle かつ非failing の entry 掃除。
//! - `voicevox_engine_unreachable` の閾値判定 (= failing が `VOICEVOX_ENGINE_WARNING`
//!   以上継続したら engine 未接続として警告) と `voicevox_animating` の連動。
//! - `LipsyncGenerated` が generation 不一致 / clips 空でも必ず `lipsync_inflight` を外す。
//! - track → builtin VOICEVOX device_id の解決と、r.md #75 の**フレーズ単位**の進捗集計
//!   (`voicevox_pending_phrase_count`) / **クリップ単位**のスピナー判定
//!   (`clip_wav_synthesizing`)。歌唱クリップも読み上げ (Text) クリップも
//!   `pending_clips` 経由で点く。

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::model::{Clip, PluginInstance, Track};
use common::plugin_format::PluginFormat;
use common::port_config::PortConfig;
use common::protocol::{
    PluginCommand, PluginEvent, VocalSynthFailure, VocalSynthProgress,
};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, LoadedSlotInfo, VOICEVOX_ENGINE_WARNING};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
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

/// `(busy, failure)` だけの状態遷移 (進捗を伴わない報告) を組む helper。
fn status(busy: bool, failure: VocalSynthFailure) -> VocalSynthProgress {
    VocalSynthProgress {
        busy,
        failure,
        ..Default::default()
    }
}

fn synth_status(device_id: u64, progress: VocalSynthProgress) -> AppEvent {
    AppEvent::Plugin(PluginEvent::VoicevoxSynthStatus {
        device_id,
        progress,
    })
}

#[test]
fn synth_status_busy_then_failing_then_unreachable_threshold() {
    let (mut app, _rx) = build_app();
    // busy + Unreachable → entry が立ち failing_since 記録。
    app.handle_event(synth_status(1, status(true, VocalSynthFailure::Unreachable)));
    let st = app.voicevox.voicevox_synth_status.get(&1).cloned().expect("entry present");
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
    app.handle_event(synth_status(9, status(true, VocalSynthFailure::Unreachable)));
    assert!(app.voicevox_any_generating());
    // 成功 (busy=false, None) で entry 掃除 → 生成中なし。
    app.handle_event(synth_status(9, status(false, VocalSynthFailure::None)));
    assert!(!app.voicevox.voicevox_synth_status.contains_key(&9));
    assert!(!app.voicevox_any_generating());
    // entry が無ければ未接続警告も出ない。
    assert!(!app.voicevox_engine_unreachable(Instant::now() + Duration::from_secs(100)));
}

#[test]
fn synth_status_busy_without_failing_is_generating_but_never_warns() {
    let (mut app, _rx) = build_app();
    app.handle_event(synth_status(2, status(true, VocalSynthFailure::None)));
    let st = app.voicevox.voicevox_synth_status.get(&2).cloned().expect("entry present");
    assert!(st.busy);
    assert!(st.failing_since.is_none(), "failing なしでは failing_since を立てない");
    assert!(app.voicevox_any_generating());
    // failing_since が無いので、いくら時間が経っても未接続警告は出ない。
    assert!(!app.voicevox_engine_unreachable(Instant::now() + Duration::from_secs(3600)));
}

#[test]
fn synth_status_rejected_shows_content_error_not_engine_warning() {
    let (mut app, _rx) = build_app();
    // engine 到達済だが歌詞拒否 (400)。busy=false でも entry は残り、内容エラーを持つ。
    app.handle_event(synth_status(
        3,
        status(
            false,
            VocalSynthFailure::Rejected { detail: "lyricが不正です: ー".into() },
        ),
    ));
    let st = app.voicevox.voicevox_synth_status.get(&3).cloned().expect("entry present");
    assert!(!st.busy);
    // Rejected は failing_since を立てない → 「engine 未接続」警告は永遠に出ない。
    assert!(st.failing_since.is_none());
    assert!(!app.voicevox_engine_unreachable(Instant::now() + Duration::from_secs(3600)));
    // 代わりに内容エラーの理由を提示する。
    assert_eq!(app.voicevox_rejected_detail(), Some("lyricが不正です: ー"));
    // busy でないので「生成中」でもない (スピナーは回さない)。
    assert!(!app.voicevox_any_generating());

    // 歌詞を直して合成成功 (None) → entry 掃除、内容エラーも消える。
    app.handle_event(synth_status(3, status(false, VocalSynthFailure::None)));
    assert!(!app.voicevox.voicevox_synth_status.contains_key(&3));
    assert_eq!(app.voicevox_rejected_detail(), None);
}

#[test]
fn lipsync_generated_always_clears_inflight_even_when_stale_or_empty() {
    let (mut app, _rx) = build_app();
    // in-flight を直接立てる (= regenerate_lipsync_for_track 相当)。
    app.voicevox.lipsync_inflight.insert(42);
    assert!(app.voicevox_any_generating());

    // generation 不一致 + clips 空 (= 全 HTTP 失敗) でも、必ず in-flight を外す。
    app.handle_event(AppEvent::LipsyncGenerated {
        vocal_track_id: 7,
        target_track_id: 42,
        bpm: 120.0,
        clips: Vec::new(),
        generation: app.voicevox.lipsync_gen.wrapping_add(999),
    });
    assert!(!app.voicevox.lipsync_inflight.contains(&42), "stale/空でも in-flight 解除");
    assert!(!app.voicevox_any_generating());
}

/// builtin VOICEVOX device を 1 つ持つ vocal track を組む (track_id=100, device_id=5)。
/// `clip_ids` の clip を持たせる。
// Track / Clip はモジュール外からリテラル構築不可 (private legacy field) なので
// `default()` + フィールド代入で組む (= field_reassign_with_default は意図的)。
#[allow(clippy::field_reassign_with_default)]
fn app_with_vocal_track(clip_ids: &[u32]) -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (mut app, rx) = build_app();
    let mut track = Track::default();
    track.id = 100;
    track.name = "Vocal".into();
    // v29: device には安定 id を持たせる (host addressing = この id)。
    track.devices.push(PluginInstance {
        id: 5,
        ..PluginInstance::with_ports(
            common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
            PluginFormat::Builtin,
            PortConfig { has_note_input: true, has_audio_output: true, ..Default::default() },
        )
    });
    for &cid in clip_ids {
        let mut clip = Clip::default();
        clip.id = cid;
        clip.start_beat = f64::from(cid) * 8.0;
        clip.length_beats = 4.0;
        track.clips.push(clip);
    }
    app.edit_song(|song| song.tracks.push(track));
    // 安定 device_id を device index 0 に紐付け (= SlotPluginLoaded 相当)。
    app.ipc.loaded_slots.insert(
        (100, 0),
        LoadedSlotInfo {
            device_id: 5,
            plugin_id_str: common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
        },
    );
    (app, rx)
}

#[test]
fn track_wav_synthesizing_resolves_plugin_id_and_counts_pending_phrases() {
    let (mut app, _rx) = app_with_vocal_track(&[1]);

    // status 未受信ならどちらも非生成。
    assert!(!app.track_wav_synthesizing(100));
    assert_eq!(app.voicevox_pending_phrase_count(), 0);

    // device_id=5 が busy → そのトラックが合成中。残件はフレーズ数で数える
    // (r.md #75: 旧「busy な track 数」ではない)。
    app.handle_event(synth_status(
        5,
        VocalSynthProgress {
            busy: true,
            failure: VocalSynthFailure::None,
            pending: 7,
            total: 11,
            pending_clips: vec![1],
        },
    ));
    assert!(app.track_wav_synthesizing(100));
    assert_eq!(app.voicevox_pending_phrase_count(), 7);

    // idle に戻ると 0 件 (entry も掃除される)。
    app.handle_event(synth_status(5, status(false, VocalSynthFailure::None)));
    assert!(!app.track_wav_synthesizing(100));
    assert_eq!(app.voicevox_pending_phrase_count(), 0);
}

/// r.md #75: クリップ上スピナーは **そのクリップに未完了フレーズがあるときだけ**点く。
/// 1 ノート直しただけで同トラックの全クリップが回らないことの回帰。
#[test]
fn clip_spinner_follows_pending_clips_not_track_busy() {
    let (mut app, _rx) = app_with_vocal_track(&[1, 2, 3]);

    // clip 2 だけに未完了フレーズが掛かっている。
    app.handle_event(synth_status(
        5,
        VocalSynthProgress {
            busy: true,
            failure: VocalSynthFailure::None,
            pending: 1,
            total: 40,
            pending_clips: vec![2],
        },
    ));
    // トラックとしては busy だが、
    assert!(app.track_wav_synthesizing(100));
    // スピナーは clip 2 にだけ点く。
    assert!(!app.clip_wav_synthesizing(100, 1));
    assert!(app.clip_wav_synthesizing(100, 2));
    assert!(!app.clip_wav_synthesizing(100, 3));
    // 存在しない track / clip では false (探索が失敗しても panic しない)。
    assert!(!app.clip_wav_synthesizing(999, 2));
}

/// r.md #75: 読み上げ (Text) クリップにもスピナーが点く。builtin は talk 発話も
/// `TalkMetadata::clip_id` から `pending_clips` に入れるので、歌唱と同じ経路で届く。
#[test]
fn talk_clip_spinner_lights_via_pending_clips() {
    let (mut app, _rx) = app_with_vocal_track(&[1, 9]);

    // clip 9 = Text クリップ相当 (talk 発話が未完了)。
    app.handle_event(synth_status(
        5,
        VocalSynthProgress {
            busy: true,
            failure: VocalSynthFailure::None,
            pending: 1,
            total: 1,
            pending_clips: vec![9],
        },
    ));
    assert!(app.clip_wav_synthesizing(100, 9), "Text クリップにもスピナーが点く");
    assert!(!app.clip_wav_synthesizing(100, 1));
    assert_eq!(app.voicevox_pending_phrase_count(), 1);

    // 完了で消える (idle 報告で entry ごと掃除)。
    app.handle_event(synth_status(5, status(false, VocalSynthFailure::None)));
    assert!(!app.clip_wav_synthesizing(100, 9));
}

/// 複数 device の残件は合算される (= 全体オーバーレイの「残り N フレーズ」)。
#[test]
fn pending_phrase_count_sums_across_devices() {
    let (mut app, _rx) = build_app();
    app.handle_event(synth_status(
        1,
        VocalSynthProgress { busy: true, pending: 3, total: 10, ..Default::default() },
    ));
    app.handle_event(synth_status(
        2,
        VocalSynthProgress { busy: true, pending: 4, total: 20, ..Default::default() },
    ));
    assert_eq!(app.voicevox_pending_phrase_count(), 7);
}

/// 口パク生成中判定 (= 出力先 口 track が `lipsync_inflight` に居る)。per-clip の
/// `auto_lipsync` gate は view 側 (`draw_clip_synth_spinner`) なのでここでは集合のみ検証。
#[test]
fn lipsync_target_generating_tracks_inflight_set() {
    let (mut app, _rx) = build_app();
    assert!(!app.lipsync_target_generating(200));
    app.voicevox.lipsync_inflight.insert(200);
    assert!(app.lipsync_target_generating(200));
    assert!(app.voicevox_any_generating());
    // 別 target は無関係。
    assert!(!app.lipsync_target_generating(201));
}
