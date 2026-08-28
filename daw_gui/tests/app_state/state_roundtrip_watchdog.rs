//! plugin host が crash でなく **hang** した (プロセス・パイプは生存の
//! まま `state_save` 等で停止) とき、 `RequestAllStates` の応答 (`AllStatesReceived`)
//! が永久に来ず、 `pending_state_queue` が drain しないため保存 / New / Open /
//! Open Recent / 終了(✕) が恒久ロックする。 `ChildDisconnected` も発火しないので
//! #63 の disconnect 救済 (`plugin_host_disconnect_unblocks_dirty_guard`) では救えない。
//!
//! `on_tick` の hang watchdog (`poll_state_roundtrip_watchdog`) が、 round-trip の
//! 送信時刻から一定時間応答が無ければ queue を破棄し、 保留中のガード操作を捨てて
//! 脱出口を作ることを検証する。 経過時間は `now` 引数で注入する (実時間に依存しない)。

use std::time::{Duration, Instant};

use common::protocol::{AudioCommand, AudioEvent, PluginCommand, PluginEvent, VocalSynthProgress};
use tokio::sync::mpsc::UnboundedReceiver;

use daw_gui::shutdown::QuitRequest;
use daw_gui::app::{AppData, AppEvent, DirtyGuardAction, ExportStage};

use super::support::{self, drain, load_instrument};

/// 旧独立バイナリ時代のシグネチャを保つ thin adapter (audio_rx はここで drop)。
fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (app, plugin_rx, _audio_rx) = build_app_with_audio();
    (app, plugin_rx)
}

/// `build_app` と同じだが audio 側 receiver も返す (ExportWav の発射検証用)。
/// 戻り順が support::build_app と違う (plugin が先) のは旧シグネチャの保存。
fn build_app_with_audio() -> (
    AppData,
    UnboundedReceiver<PluginCommand>,
    UnboundedReceiver<AudioCommand>,
) {
    let (app, audio_rx, plugin_rx, _dispatcher) = support::build_app();
    (app, plugin_rx, audio_rx)
}

/// 過去に始まった round-trip として watchdog を発火させる「未来」時刻。
/// `Instant + Duration` は underflow しないので安全。
fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(120)
}

/// 「保存して終了」 で plugin state 待ちに入った直後に host が hang した場合、
/// watchdog が round-trip を破棄し、 ガードが再び機能する。
///
/// (r.md #61) 終了意図は **捨てずに聞き直す**。旧実装は warn ログ 1 行だけ残して
/// 黙って捨てており、ユーザーからは「✕ が effective でなかった」ようにしか見えな
/// かった。かといって保存が成立していないまま終了させるのも誤り (未保存変更を失う)
/// ので、queue が空になった最新状態でガードをやり直す — 正常系
/// (`on_all_states_from_child` 末尾) とまったく同じ扱い。
#[test]
fn hang_during_save_and_quit_reasks_the_guard() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    load_instrument(&mut app);
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});
    app.request_close();

    // 「保存して終了」: plugin 有りなので state 取得待ちの非同期保存 (round-trip in flight)。
    app.handle_event(AppEvent::DirtyGuardSave);
    assert!(!app.ipc.pending_state_queue.is_empty(), "save round-trip in flight");
    assert_eq!(
        app.ui_ephemeral.guard_after_save,
        Some(DirtyGuardAction::Quit(QuitRequest::USER)),
        "quit-after-save intent pending"
    );

    // 閾値前は何もしない (slow render / busy host を誤って中止しない)。
    app.poll_state_roundtrip_watchdog(Instant::now() + Duration::from_secs(5));
    assert!(
        !app.ipc.pending_state_queue.is_empty(),
        "watchdog must not fire before the timeout"
    );
    assert_eq!(
        app.ui_ephemeral.guard_after_save,
        Some(DirtyGuardAction::Quit(QuitRequest::USER)),
        "intent intact before timeout"
    );

    // 応答が来ないまま閾値超過 → watchdog 発火で脱出。
    app.poll_state_roundtrip_watchdog(far_future());
    assert!(
        app.ipc.pending_state_queue.is_empty(),
        "stale state-request queue cleared by watchdog"
    );
    assert!(
        app.ui_ephemeral.guard_after_save.is_none(),
        "stuck quit-after-save action is no longer pending on the round-trip"
    );
    assert!(!app.shutdown.is_shutting_down(), "watchdog does not silently quit");
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::Quit(QuitRequest::USER)),
        "quit intent survives: the guard is re-asked with the current (unsaved) state"
    );
    assert!(!path.exists(), "nothing saved (host never returned state)");

    // 「保存せず終了」 を選べば、 hang した host を待たずに抜けられる。
    app.handle_event(AppEvent::DirtyGuardDiscard);
    assert!(app.shutdown.is_draining(), "discard quits without waiting for the hung host");
    assert!(app.ui_ephemeral.dirty_guard.is_none(), "modal closed");
}

/// Deferred edit (track 削除) の round-trip 中に host が hang し、 さらにその間に
/// Open Recent が保留 (`guard_pending_action`) されていた場合、 watchdog は queue と
/// 保留操作を破棄する。 削除は **適用されない** (= 完了ハンドラが走らない) ので
/// project は無傷で残る。
#[test]
fn hang_during_deferred_edit_aborts_without_applying_edit() {
    let (mut app, _rx) = build_app();
    load_instrument(&mut app); // plugin あり → DeleteTracks は deferred round-trip。
    // id は必ず採番し直す (clone のまま push すると同 id が 2 本並び、 安定 id での
    // 削除が意図しない方を指す)。
    let extra = app.song_doc.song().tracks[0].clone();
    let target_id = app
        .edit_song(|song| {
            let id = song.alloc_track_id();
            let mut t = extra;
            t.id = id;
            song.tracks.push(t);
            id
        })
        .expect("edit_song");
    let track_count = app.song_doc.song().tracks.len();

    app.handle_event(AppEvent::DeleteTracks(vec![target_id]));
    assert!(
        !app.ipc.pending_state_queue.is_empty(),
        "deferred delete round-trip in flight"
    );

    // round-trip 中に Open Recent → 完了まで保留。
    let target = std::path::PathBuf::from("C:/some/other.daw");
    app.handle_event(AppEvent::OpenRecent(target.clone()));
    assert_eq!(
        app.ui_ephemeral.guard_pending_action,
        Some(DirtyGuardAction::OpenPath(target)),
        "Open deferred until the queue drains"
    );

    // host hang → watchdog 発火。
    app.poll_state_roundtrip_watchdog(far_future());
    assert!(app.ipc.pending_state_queue.is_empty(), "queue cleared");
    assert!(
        app.ui_ephemeral.guard_pending_action.is_none(),
        "stuck queue-drain action dropped"
    );
    // 削除は完了ハンドラ (on_all_states_from_child) でしか実行されない。 watchdog は
    // それを呼ばないので track は残る (= データ破壊しない)。
    assert_eq!(
        app.song_doc.song().tracks.len(),
        track_count,
        "deferred delete was NOT applied (project intact)"
    );
}

/// round-trip が無いときの watchdog は完全に no-op (誤って状態を壊さない)。
#[test]
fn watchdog_is_noop_when_no_roundtrip_in_flight() {
    let (mut app, _rx) = build_app();
    assert!(app.ipc.pending_state_queue.is_empty());

    app.poll_state_roundtrip_watchdog(far_future());

    assert!(app.ipc.pending_state_queue.is_empty());
    assert!(app.ui_ephemeral.guard_after_save.is_none());
    assert!(app.ui_ephemeral.guard_pending_action.is_none());
    assert!(app.ui_ephemeral.status_message.is_empty(), "no spurious notification");
}

/// review finding #1: export 進行中は handle_event の gate が `AllStatesReceived` を
/// drop するので、 その間に watchdog が発火すると「応答が gate に食われただけ」の save を
/// hang と誤判定して中止してしまう。 export 中は watchdog を抑制し、 export 後に再評価する。
#[test]
fn watchdog_suppressed_during_export_then_fires_after() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    load_instrument(&mut app);
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::Save);
    assert!(!app.ipc.pending_state_queue.is_empty(), "save round-trip in flight");

    // export 進行中を模す。 閾値を遥かに超えても抑制される。
    app.transport.export_stage = Some(ExportStage::AudioRender { done: 0, total: 0 });
    app.poll_state_roundtrip_watchdog(far_future());
    assert!(
        !app.ipc.pending_state_queue.is_empty(),
        "watchdog must not fire while an export gates the response"
    );

    // video export の音声前段 (export_stage 未設定でも pending_video_export で gate) も抑制。
    app.transport.export_stage = None;
    app.transport.pending_video_export = Some(std::path::PathBuf::from("C:/out.mp4"));
    app.poll_state_roundtrip_watchdog(far_future());
    assert!(
        !app.ipc.pending_state_queue.is_empty(),
        "watchdog also suppressed while a video export is pending"
    );

    // gate 解除後は、 真に応答が来ない round-trip を改めて閾値超過で reap する。
    app.transport.pending_video_export = None;
    app.poll_state_roundtrip_watchdog(far_future());
    assert!(
        app.ipc.pending_state_queue.is_empty(),
        "watchdog fires once the export gate is lifted"
    );
}

/// review finding #2: plugin host が居ない (crash 後 respawn 断念で plugin_tx=None) のに
/// song に plugin が在る degraded 状態で Save すると、 旧実装は届かない RequestAllStates に
/// 30s 武装してから中止していた。 host 不在は dispatch 時点で分かるので、 30s 待たず
/// **即** round-trip を破棄して脱出する。
#[test]
fn roundtrip_with_no_plugin_host_aborts_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    load_instrument(&mut app); // song_has_plugin() == true。
    // crash 後 respawn 断念で host が居ない状況を模す (plugin_tx=None、 song は plugin 保持)。
    app.ipc.plugin_tx = None;
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});

    app.handle_event(AppEvent::Save);

    // dispatch が host 不在を検知して即 abort。 watchdog (30s) を一切回さずに queue は空。
    assert!(
        app.ipc.pending_state_queue.is_empty(),
        "no doomed round-trip is armed when there is no host to answer"
    );
    assert!(!app.ui_ephemeral.status_message.is_empty(), "user is notified immediately");
    assert!(!path.exists(), "save did not complete (no plugin states available)");

    // 以後もガードは生きている (恒久ロックしない)。
    app.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::New);
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::New),
        "dirty guard still works in the degraded no-host state"
    );
}

/// round-trip が応答 (`AllStatesReceived`) で正常完了したら、 その後に watchdog が
/// 過去 deadline で呼ばれても発火しない (= deadline が解除されている)。
#[test]
fn watchdog_does_not_fire_after_roundtrip_completes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proj.daw");

    let (mut app, _rx) = build_app();
    load_instrument(&mut app);
    app.song_doc.file_path = Some(path.clone());
    app.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::Save);
    assert!(!app.ipc.pending_state_queue.is_empty(), "save round-trip in flight");

    // 正常応答で完了。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates { entries: Vec::new() }));
    assert!(app.ipc.pending_state_queue.is_empty(), "queue drained on response");
    assert!(path.exists(), "project saved");

    // 完了後は watchdog が発火しない (deadline 解除済み)。
    app.poll_state_roundtrip_watchdog(far_future());
    assert!(
        app.ui_ephemeral.status_message.is_empty() || !app.ui_ephemeral.status_message.contains("応答しない"),
        "no hang notification after a clean completion"
    );
}

/// review finding (export gate): `PluginsReinitDone` は export 自身のハンドシェイク
/// 返信 (ReinitAllPlugins 応答) なのに、 export gate
/// (handle_event 冒頭) の whitelist から漏れていて drop されていた。 begin_wav_export は
/// export_stage を立てた *後* に ReinitAllPlugins を送るので、 この応答は必ず
/// export 中に到着する。 drop されると stashed `ExportWav` が永遠に発射されず、 GUI 実機
/// の WAV / video export が reinit でハングしていた (headless script 経路は gate を
/// 通らないので露見しなかった)。 gate を通過して handler が `ExportWav` を audio へ
/// 撃つことを検証する。
#[test]
fn plugins_reinit_done_passes_export_gate_and_fires_export_wav() {
    let (mut app, _plugin_rx, mut audio_rx) = build_app_with_audio();
    // 音声 freewheel 前段を模す: export_stage 立て + reinit 完了待ちの stashed export。
    app.transport.export_stage = Some(ExportStage::AudioRender { done: 0, total: 0 });
    app.transport.pending_export = Some((std::path::PathBuf::from("C:/out.wav"), None, false));
    let _ = drain(&mut audio_rx);

    // host の reinit 完了通知。 gate を通過 → handler が pending_export を撃つ。
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginsReinitDone));

    let msgs = drain(&mut audio_rx);
    assert!(
        msgs.iter()
            .any(|m| matches!(m, AudioCommand::ExportWav { .. })),
        "PluginsReinitDone must pass the export gate and fire ExportWav: {msgs:?}"
    );
    assert!(app.transport.pending_export.is_none(), "stashed export consumed");
}

/// gate を開け過ぎていないことの確認: export 中、 song を変える user 操作
/// (`SetTrackVolume`) は従来どおり drop される (render desync 防止)。
#[test]
fn export_gate_still_blocks_song_mutations() {
    let (mut app, _plugin_rx, _audio_rx) = build_app_with_audio();
    let track_id = app.song_doc.song().tracks[0].id;
    app.transport.export_stage = Some(ExportStage::AudioRender { done: 0, total: 0 });

    app.handle_event(AppEvent::SetTrackVolume {
        track: track_id,
        amp: 0.137,
    });

    let vol = app
        .song_doc.song()
        .tracks
        .iter()
        .find(|t| t.id == track_id)
        .map(|t| t.volume);
    assert_ne!(
        vol,
        Some(0.137),
        "song-mutating user events stay gated during export"
    );
}

/// gate 反転 (allow-list → block-list) の回帰。 export 中でも block-list 外の
/// protocol event は default で流れる (= 「新 variant を allow に入れ忘れ →
/// GUI 永久ロック」 class が構造的に消えた)。 一方、 走行中 render を壊す host
/// 再構成 round-trip は引き続き drop される。
#[test]
fn export_gate_flows_default_events_but_blocks_host_reconfig() {
    let (mut app, mut plugin_rx, _audio_rx) = build_app_with_audio();
    app.transport.export_stage = Some(ExportStage::AudioRender { done: 0, total: 0 });

    // (1) 正例: 旧 allow-list に無かった VoicevoxSynthStatus は今 export 中も流れ、
    //     handler が status entry を作る。 追加した variant を何もせず default で
    //     通す = 「allow 忘れ → deadlock」 が不能。
    app.handle_event(AppEvent::Plugin(PluginEvent::VoicevoxSynthStatus {
        device_id: 7,
        progress: VocalSynthProgress { busy: true, ..Default::default() },
    }));
    assert!(
        app.voicevox.voicevox_synth_status.contains_key(&7),
        "block-list 外の event は export 中も流れて処理される (positive-default)"
    );

    // (2) 反例: host を再構成する round-trip は block-list で drop される。
    //     BounceClipFxComplete の handler は pending 無しでも防御的に
    //     SetRenderMode(Realtime) を plugin へ送るので、 block されれば handler に
    //     到達せず、 その送信も起きない。
    let _ = drain(&mut plugin_rx); // setup 由来の command を掃除
    app.handle_event(AppEvent::Audio(AudioEvent::BounceClipFxComplete {
        path: std::path::PathBuf::from("x.wav"),
        source_track: 0,
        source_clip: 0,
        error: None,
        frames: 0,
    }));
    assert!(
        drain(&mut plugin_rx).is_empty(),
        "host 再構成 round-trip (BounceClipFxComplete) は export 中 block-list で drop される"
    );
}
