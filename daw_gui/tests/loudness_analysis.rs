//! 範囲ラウドネス解析 (r.md #54) のイベント層の回帰テスト。
//!
//! 検証する挙動:
//! - 解析メニュー → 範囲ピッカー (既定 = ループ範囲) → 確定で
//!   **書き出しと同じハンドシェイク** (停止 → Offline → 全プラグイン再初期化) が走り、
//!   `PluginsReinitDone` を受けて初めて `AudioCommand::AnalyzeLoudness` が **拍で** 飛ぶ。
//! - 走査中は再生も曲の編集も止まる (freewheel と競合するため)。
//! - 完了で Realtime に戻り、レポートが確定する。曲を編集すると「古い」印が付く。
//! - 中止は `CancelExport` (書き出しと共通の中断経路)。
//!
//! 測定そのもの (BS.1770 / Tech 3341・3342) は `common::loudness` /
//! `common::loudness_report` のテストが持つ。ここは **配線** だけを見る。

use std::sync::Arc;

use common::loudness_report::LoudnessReport;
use common::protocol::{AudioCommand, AudioEvent, PluginCommand, PluginEvent, RenderMode};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::state::LoudnessPhase;

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
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

fn drain<T>(rx: &mut UnboundedReceiver<T>) -> Vec<T> {
    let mut out = Vec::new();
    while let Ok(m) = rx.try_recv() {
        out.push(m);
    }
    out
}

/// 解析完了レポートのダミー (数値だけ埋める)。
fn report(range: (f64, f64), integrated: f32) -> Box<LoudnessReport> {
    Box::new(LoudnessReport {
        range_start_beat: range.0,
        range_end_beat: range.1,
        sample_rate: 48_000,
        done_frames: 480_000,
        total_frames: 480_000,
        complete: true,
        integrated_lufs: integrated,
        true_peak_dbtp: -0.8,
        ..LoudnessReport::default()
    })
}

/// ループ範囲を既定にしたピッカーが開き、確定でハンドシェイクが走り、
/// reinit 完了で **拍** の解析コマンドが飛ぶ。
#[test]
fn 解析はループ範囲を既定にして再初期化のあとに拍で飛ぶ() {
    let (mut app, mut audio_rx, mut plugin_rx) = build_app();
    app.handle_event(AppEvent::SetLoopRange { start: 8.0, end: 24.0 });
    drain(&mut audio_rx);
    drain(&mut plugin_rx);

    app.handle_event(AppEvent::AnalyzeLoudness);
    let picker = app
        .ui_ephemeral
        .export_range_picker
        .expect("範囲ピッカーが開いていない");
    assert_eq!((picker.start_beat, picker.end_beat), (8.0, 24.0));

    app.handle_event(AppEvent::ConfirmExportRange);
    assert_eq!(
        app.loudness.phase,
        LoudnessPhase::AwaitingReinit { range: Some((8.0, 24.0)) }
    );
    assert!(app.ui_prefs.loudness_report_open, "レポート窓が先に開いていない");

    // 再初期化ハンドシェイク: Offline へ切り替えて全プラグインを作り直す。
    let plugin_cmds = drain(&mut plugin_rx);
    assert!(plugin_cmds.contains(&PluginCommand::SetRenderMode(RenderMode::Offline)));
    assert!(plugin_cmds.contains(&PluginCommand::ReinitAllPlugins));
    // まだ解析コマンドは出ていない (状態が汚れたまま測らない)。
    assert!(
        !drain(&mut audio_rx)
            .iter()
            .any(|c| matches!(c, AudioCommand::AnalyzeLoudness { .. })),
        "reinit 完了前に解析が始まっている"
    );

    app.handle_event(AppEvent::Plugin(PluginEvent::PluginsReinitDone));
    assert_eq!(app.loudness.phase, LoudnessPhase::Running);
    assert!(
        drain(&mut audio_rx).contains(&AudioCommand::AnalyzeLoudness {
            range: Some((8.0, 24.0))
        }),
        "解析コマンドが拍で飛んでいない"
    );
}

/// 走査中は再生も曲の編集も通らない (freewheel と競合するため)。
#[test]
fn 走査中は再生と編集を受け付けない() {
    let (mut app, mut audio_rx, _plugin_rx) = build_app();
    app.handle_event(AppEvent::SetLoopRange { start: 0.0, end: 16.0 });
    app.handle_event(AppEvent::AnalyzeLoudness);
    app.handle_event(AppEvent::ConfirmExportRange);
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginsReinitDone));
    drain(&mut audio_rx);

    app.handle_event(AppEvent::Play);
    assert!(!app.transport.is_playing, "解析中に再生が始まっている");
    assert!(
        !drain(&mut audio_rx).contains(&AudioCommand::Play),
        "解析中に Play が engine へ飛んでいる"
    );

    let before = app.song_doc.song().bpm;
    app.handle_event(AppEvent::SetSongBpmFromScrub(180.0));
    assert_eq!(
        app.song_doc.song().bpm,
        before,
        "解析中に曲が編集されている (測っている前提が変わる)"
    );
}

/// 完了で Realtime へ戻り、レポートが確定する。その後の編集で「古い」印が付く。
#[test]
fn 完了でレポートが確定し編集すると古くなる() {
    let (mut app, mut audio_rx, mut plugin_rx) = build_app();
    app.handle_event(AppEvent::SetLoopRange { start: 4.0, end: 20.0 });
    app.handle_event(AppEvent::AnalyzeLoudness);
    app.handle_event(AppEvent::ConfirmExportRange);
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginsReinitDone));
    drain(&mut audio_rx);
    drain(&mut plugin_rx);

    // 途中経過 → 確定。
    app.handle_event(AppEvent::Audio(AudioEvent::LoudnessAnalysisProgress(
        Box::new(LoudnessReport {
            done_frames: 240_000,
            total_frames: 480_000,
            integrated_lufs: -15.0,
            ..LoudnessReport::default()
        }),
    )));
    assert_eq!(app.loudness.phase, LoudnessPhase::Running);
    assert_eq!(app.loudness.report.as_ref().unwrap().integrated_lufs, -15.0);

    app.handle_event(AppEvent::Audio(AudioEvent::LoudnessAnalysisComplete {
        report: Some(report((4.0, 20.0), -13.2)),
        error: None,
        cancelled: false,
    }));
    assert_eq!(app.loudness.phase, LoudnessPhase::Idle);
    assert!(!app.loudness_report_stale());
    let r = app.loudness.report.as_ref().expect("レポートが無い");
    assert_eq!(r.integrated_lufs, -13.2);
    // 目標との差 = 「あと何 dB」。
    let gain = r.normalization_gain_db(-14.0).expect("目標との差");
    assert!((gain - (-0.8)).abs() < 1e-4, "gain = {gain}");
    assert!(
        drain(&mut plugin_rx).contains(&PluginCommand::SetRenderMode(RenderMode::Realtime)),
        "解析後に Realtime へ戻していない"
    );

    // 編集すると値はもう古い。
    app.handle_event(AppEvent::SetSongBpmFromScrub(150.0));
    assert!(app.loudness_report_stale(), "曲を編集したのに古い印が付いていない");
    // undo でも古いまま (epoch はどちらへ動いても進む)。「編集の口ごとに印を
    // 立てる」設計だと undo / redo / プロジェクト差し替えで穴が開く。
    app.handle_event(AppEvent::Undo);
    assert!(app.loudness_report_stale(), "undo 後に古い印が消えている");
}

/// 中止は書き出しと共通の `CancelExport`、確定値は残さない。
#[test]
fn 中止すると途中の値を確定値として残さない() {
    let (mut app, mut audio_rx, _plugin_rx) = build_app();
    app.handle_event(AppEvent::SetLoopRange { start: 0.0, end: 8.0 });
    app.handle_event(AppEvent::AnalyzeLoudness);
    app.handle_event(AppEvent::ConfirmExportRange);
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginsReinitDone));
    drain(&mut audio_rx);

    app.handle_event(AppEvent::CancelLoudnessAnalysis);
    assert_eq!(app.loudness.phase, LoudnessPhase::Cancelling);
    assert!(drain(&mut audio_rx).contains(&AudioCommand::CancelExport));

    app.handle_event(AppEvent::Audio(AudioEvent::LoudnessAnalysisComplete {
        report: Some(report((0.0, 8.0), -20.0)),
        error: None,
        cancelled: true,
    }));
    assert_eq!(app.loudness.phase, LoudnessPhase::Idle);
    assert!(
        app.loudness.report.is_none(),
        "中止した途中値を確定レポートとして残している"
    );
}

/// 子プロセスが落ちたら engine ごと畳む。
///
/// `CancelExport` を送らないと daw_audio の `export_running` が立ちっぱなしになり、
/// CPAL が無音を書き続けて「再生しても音が出ない」+ 以後の書き出し / 解析が
/// 全部 "export already in progress" で弾かれる (書き出し側と同じ契約)。
#[test]
fn 子プロセス切断で解析を畳み_engine_へ中止を送る() {
    let (mut app, mut audio_rx, _plugin_rx) = build_app();
    app.handle_event(AppEvent::SetLoopRange { start: 0.0, end: 8.0 });
    app.handle_event(AppEvent::AnalyzeLoudness);
    app.handle_event(AppEvent::ConfirmExportRange);
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginsReinitDone));
    assert_eq!(app.loudness.phase, LoudnessPhase::Running);
    drain(&mut audio_rx);

    app.handle_event(AppEvent::Plugin(PluginEvent::ChildDisconnected));
    assert_eq!(app.loudness.phase, LoudnessPhase::Idle, "暗転したまま残っている");
    assert!(
        drain(&mut audio_rx).contains(&AudioCommand::CancelExport),
        "engine に中止を送っていない (export_running が立ちっぱなしになる)"
    );
}

/// 中止したあとに前セッションの完了が後着しても、それを結果として受理しない。
///
/// 受理すると「新しい解析の途中に古い結果が確定値として出る」だけでなく、
/// phase が Idle に落ちて次の `PluginsReinitDone` で新セッションが発火しなくなる。
#[test]
fn 前セッションの後着完了を新しい解析の結果にしない() {
    let (mut app, mut audio_rx, _plugin_rx) = build_app();
    app.handle_event(AppEvent::SetLoopRange { start: 0.0, end: 8.0 });
    app.handle_event(AppEvent::AnalyzeLoudness);
    app.handle_event(AppEvent::ConfirmExportRange);
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginsReinitDone));
    // watchdog 相当の強制終了 → 世代が進む。
    app.handle_event(AppEvent::Plugin(PluginEvent::ChildDisconnected));

    // 2 回目を開始 (まだ reinit 待ち)。
    app.handle_event(AppEvent::AnalyzeLoudness);
    app.handle_event(AppEvent::ConfirmExportRange);
    assert!(matches!(app.loudness.phase, LoudnessPhase::AwaitingReinit { .. }));
    drain(&mut audio_rx);

    // ここで 1 回目の完了が後着する。
    app.handle_event(AppEvent::Audio(AudioEvent::LoudnessAnalysisComplete {
        report: Some(report((0.0, 8.0), -18.0)),
        error: None,
        cancelled: false,
    }));
    assert!(
        matches!(app.loudness.phase, LoudnessPhase::AwaitingReinit { .. }),
        "後着完了で 2 回目のセッションが畳まれている"
    );
    assert!(app.loudness.report.is_none(), "前セッションの値を確定値にしている");

    // 2 回目は正常に発火できる。
    app.handle_event(AppEvent::Plugin(PluginEvent::PluginsReinitDone));
    assert_eq!(app.loudness.phase, LoudnessPhase::Running);
    assert!(
        drain(&mut audio_rx)
            .iter()
            .any(|c| matches!(c, AudioCommand::AnalyzeLoudness { .. })),
        "2 回目の解析が発火していない"
    );
}

/// 範囲プリセットはループ / 曲全体を拍で書き換える。対象が無いものは値を変えない。
#[test]
fn 範囲プリセットが拍範囲を差し替える() {
    use daw_gui::app_types::ExportRangeSource;

    let (mut app, mut audio_rx, _plugin_rx) = build_app();
    app.handle_event(AppEvent::SetLoopRange { start: 12.0, end: 28.0 });
    drain(&mut audio_rx);
    app.handle_event(AppEvent::AnalyzeLoudness);

    app.handle_event(AppEvent::SetExportRangeSource(ExportRangeSource::Whole));
    let len = app.song_doc.song().length_beats;
    let p = app.ui_ephemeral.export_range_picker.unwrap();
    assert_eq!((p.start_beat, p.end_beat), (0.0, len));

    app.handle_event(AppEvent::SetExportRangeSource(ExportRangeSource::Loop));
    let p = app.ui_ephemeral.export_range_picker.unwrap();
    assert_eq!((p.start_beat, p.end_beat), (12.0, 28.0));

    // セクションも選択も無い状態では範囲を変えずに理由を出す。
    app.handle_event(AppEvent::SetExportRangeSource(ExportRangeSource::Section));
    let p = app.ui_ephemeral.export_range_picker.unwrap();
    assert_eq!((p.start_beat, p.end_beat), (12.0, 28.0));
    assert!(app.ui_ephemeral.status_message.contains("セクション"));
}
