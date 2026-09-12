//! オフライン描画 (WAV 書き出し / ラウドネス解析 / クリップ FX bounce) の起動側。
//!
//! 3 つとも **エンジン全体で同時に 1 本** (`EngineShared::export_running` の予約) で、
//! 走っている間は全 project の live 出力が無音になる (`docs/plan_project_tabs.md` §0)。
//! 描画する材料 (song / plugin_refs / renderer / latency / project_dir) は対象 project の
//! [`ProjectShared`] から、worker pool は [`EngineShared`] から取る。
//!
//! `main.rs::recv_loop` から分離したのは不変条件 9 (`main.rs` の budget)。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::protocol::{AudioEvent, ProjectKey};

use crate::engine::{EngineShared, ProjectShared};
use crate::export;

type Out = tokio::sync::mpsc::UnboundedSender<AudioEvent>;

/// エンジン全体のオフライン描画予約を取る。取れなければ `false`。
///
/// compare_exchange on the recv loop serializes against a second request — the
/// old "load here, set inside the spawned thread" pattern had a TOCTOU window
/// where two requests could both pass the check before either set the flag and
/// double-spawn, corrupting the shared WAV writer / plugin chain. The spawn
/// closure (and the early-out paths) release it.
fn reserve(engine_shared: &EngineShared) -> bool {
    engine_shared
        .export_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// 予約後の共通前処理: song snapshot を取り、stale cancel を畳む。song が無ければ
/// 予約を返して `None`。
fn take_song(engine_shared: &EngineShared, project: &ProjectShared) -> Option<common::model::Song> {
    let song_snap = project.song.load();
    let Some(song_arc) = song_snap.as_ref() else {
        engine_shared.export_running.store(false, Ordering::Release);
        return None;
    };
    let song = (**song_arc).clone();
    drop(song_snap);
    // Clear any stale cancel from a previous render, synchronously
    // on this receive loop so it's FIFO-ordered against a later
    // CancelExport (which then aborts THIS render, not a prior one).
    engine_shared.export_cancel.store(false, Ordering::Release);
    Some(song)
}

/// `ExportWav`: kick off the offline render on a dedicated thread so the IPC
/// receive loop stays responsive. The export thread silences the CPAL callback
/// via `EngineShared::export_running` while it holds the audio resources.
pub fn export_wav(
    engine_shared: &Arc<EngineShared>,
    project: &Arc<ProjectShared>,
    session_sample_rate: u32,
    out_tx: &Out,
    path: std::path::PathBuf,
    range: Option<(f64, f64)>,
    write_mod_sidecar: bool,
) {
    let key = project.key;
    let complete = |error: Option<String>, cancelled: bool| AudioEvent::ExportWavComplete {
        project: key,
        error,
        cancelled,
    };
    if !reserve(engine_shared) {
        tracing::warn!("ExportWav received while a render is already running; ignoring");
        let _ = out_tx.send(complete(Some("export already in progress".into()), false));
        return;
    }
    let Some(song) = take_song(engine_shared, project) else {
        tracing::warn!("ExportWav received but no song loaded");
        let _ = out_tx.send(complete(Some("no song loaded".into()), false));
        return;
    };
    let engine_shared_clone = Arc::clone(engine_shared);
    let engine_shared_release = Arc::clone(engine_shared);
    let project = Arc::clone(project);
    let out_tx_clone = out_tx.clone();
    let out_tx_progress = out_tx.clone();
    let sample_rate = session_sample_rate;
    // by the time this thread runs, the GUI has stopped
    // playback and reinitialised every plugin (deactivate→activate)
    // for a clean cold start. The export thread waits for the live
    // CPAL callback to park, then freewheels and reports progress.
    if let Err(e) = std::thread::Builder::new()
        .name("daw-audio-export".into())
        .spawn(move || {
            // Throttle progress to ~every 0.5% of the song body so
            // the determinate overlay updates smoothly without
            // flooding the IPC pipe, PLUS a 250 ms wall-clock
            // heartbeat. The heartbeat fires even when `done` is
            // unchanged — during the tail-silence walk `done` is
            // pinned at `total`, so without an unconditional
            // heartbeat the GUI would get no message for the whole
            // tail and its no-progress watchdog could false-fire on a
            // heavy/slow tail. 250 ms throttle bounds the plateau to
            // ~4 msgs/s (≈40 over the 10 s tail cap), not a flood.
            let mut last_sent: Option<u64> = None;
            let mut last_at = std::time::Instant::now();
            let on_progress = move |done: u64, total: u64| {
                let step = (total / 200).max(1);
                let crossed = match last_sent {
                    None => true,
                    Some(prev) => {
                        done.saturating_sub(prev) >= step || (done >= total && prev < total)
                    }
                };
                let heartbeat = last_at.elapsed() >= std::time::Duration::from_millis(250);
                if crossed || heartbeat {
                    last_sent = Some(done);
                    last_at = std::time::Instant::now();
                    let _ = out_tx_progress.send(AudioEvent::ExportWavProgress {
                        project: key,
                        done,
                        total,
                    });
                }
            };
            // user export range walks cold from the range
            // start (matches Play-from-here); full export walks 0..len.
            let span = match range {
                Some((start_beat, end_beat)) => export::RenderSpan::RangeCold { start_beat, end_beat },
                None => export::RenderSpan::Full,
            };
            let result = export::run_export(
                path,
                engine_shared_clone,
                project,
                song,
                sample_rate,
                common::process_data::MAX_FRAMES,
                span,
                write_mod_sidecar,
                on_progress,
            );
            // Release the engine reservation now the render is done,
            // on every path (including an early bail inside
            // run_export, which no longer touches export_running).
            engine_shared_release
                .export_running
                .store(false, Ordering::Release);
            let (error_msg, cancelled) = match result {
                Ok(outcome) => (None, outcome.cancelled),
                Err(e) => {
                    tracing::error!(error = ?e, "offline WAV export failed");
                    (Some(format!("{e:#}")), false)
                }
            };
            let _ = out_tx_clone.send(AudioEvent::ExportWavComplete {
                project: key,
                error: error_msg,
                cancelled,
            });
        })
    {
        tracing::error!(error = ?e, "failed to spawn export thread");
        // No thread will release the reservation we took above.
        engine_shared.export_running.store(false, Ordering::Release);
        let _ = out_tx.send(complete(Some(format!("failed to spawn export thread: {e}")), false));
    }
}

/// r.md #54: 範囲ラウドネス解析。ExportWav と同じ engine 予約 /
/// live park ハンドシェイク / cancel を共有し、走査の出力先だけ
/// WAV writer から LoudnessCollector へ差し替える。
pub fn analyze_loudness(
    engine_shared: &Arc<EngineShared>,
    project: &Arc<ProjectShared>,
    session_sample_rate: u32,
    out_tx: &Out,
    range: Option<(f64, f64)>,
) {
    let key = project.key;
    let failed = |error: String| AudioEvent::LoudnessAnalysisComplete {
        project: key,
        report: None,
        error: Some(error),
        cancelled: false,
    };
    if !reserve(engine_shared) {
        tracing::warn!("AnalyzeLoudness received while a render is already running; ignoring");
        let _ = out_tx.send(failed("export already in progress".into()));
        return;
    }
    let Some(song) = take_song(engine_shared, project) else {
        tracing::warn!("AnalyzeLoudness received but no song loaded");
        let _ = out_tx.send(failed("no song loaded".into()));
        return;
    };
    let engine_shared_clone = Arc::clone(engine_shared);
    let engine_shared_release = Arc::clone(engine_shared);
    let project = Arc::clone(project);
    let out_tx_clone = out_tx.clone();
    let out_tx_progress = out_tx.clone();
    let sample_rate = session_sample_rate;
    if let Err(e) = std::thread::Builder::new()
        .name("daw-audio-loudness".into())
        .spawn(move || {
            // スロットルは sink 側 (250ms) が持つ。ここは送るだけ。
            let on_progress = move |report: common::loudness_report::LoudnessReport| {
                let _ = out_tx_progress.send(AudioEvent::LoudnessAnalysisProgress {
                    project: key,
                    report: Box::new(report),
                });
            };
            // 範囲は cold 走査 (= その範囲を書き出したのと同じ音)。
            let span = match range {
                Some((start_beat, end_beat)) => export::RenderSpan::RangeCold { start_beat, end_beat },
                None => export::RenderSpan::Full,
            };
            let result = export::run_loudness_analysis(
                engine_shared_clone,
                project,
                song,
                sample_rate,
                common::process_data::MAX_FRAMES,
                span,
                on_progress,
            );
            engine_shared_release
                .export_running
                .store(false, Ordering::Release);
            let event = match result {
                Ok(outcome) => AudioEvent::LoudnessAnalysisComplete {
                    project: key,
                    report: Some(Box::new(outcome.report)),
                    error: None,
                    cancelled: outcome.cancelled,
                },
                Err(e) => {
                    tracing::error!(error = ?e, "offline loudness analysis failed");
                    AudioEvent::LoudnessAnalysisComplete {
                        project: key,
                        report: None,
                        error: Some(format!("{e:#}")),
                        cancelled: false,
                    }
                }
            };
            let _ = out_tx_clone.send(event);
        })
    {
        tracing::error!(error = ?e, "failed to spawn loudness analysis thread");
        engine_shared.export_running.store(false, Ordering::Release);
        let _ = out_tx.send(failed(format!("failed to spawn loudness thread: {e}")));
    }
}

/// `BounceClipFxOnline`: クリップ範囲のオフライン bounce。予約 / park / cancel は
/// ExportWav と共通 (run_export は export_running を立てないので、ここで予約しないと
/// bounce 中に CPAL callback が無音にならない)。
#[allow(clippy::too_many_arguments)]
pub fn bounce_clip_fx(
    engine_shared: &Arc<EngineShared>,
    project: &Arc<ProjectShared>,
    session_sample_rate: u32,
    out_tx: &Out,
    path: std::path::PathBuf,
    source_track: u32,
    source_clip: u32,
    start_beat: f64,
    end_beat: f64,
    warm: bool,
) {
    let key: ProjectKey = project.key;
    let failed = |path: std::path::PathBuf, error: String| AudioEvent::BounceClipFxComplete {
        project: key,
        path,
        source_track,
        source_clip,
        error: Some(error),
        frames: 0,
    };
    if !reserve(engine_shared) {
        tracing::warn!("BounceClipFxOnline received while a render is already running; ignoring");
        let _ = out_tx.send(failed(path, "export already in progress".into()));
        return;
    }
    let Some(song) = take_song(engine_shared, project) else {
        tracing::warn!("BounceClipFxOnline received but no song loaded");
        let _ = out_tx.send(failed(path, "no song loaded".into()));
        return;
    };
    let engine_shared_clone = Arc::clone(engine_shared);
    let engine_shared_release = Arc::clone(engine_shared);
    let project = Arc::clone(project);
    let out_tx_clone = out_tx.clone();
    let sample_rate = session_sample_rate;
    let path_for_thread = path.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("daw-audio-bounce-fx".into())
        .spawn(move || {
            let path_for_complete = path_for_thread.clone();
            // Clip-FX bounce: warm walk from frame 0 so plugin tails /
            // sidechain state at the clip start are correct. Glue の
            // 焼き込みは insert を外した素材だけなので cold (= 範囲頭から)。
            // No video consumer, so no modulation sidecar.
            let span = if warm {
                export::RenderSpan::RangeWarm { start_beat, end_beat }
            } else {
                export::RenderSpan::RangeCold { start_beat, end_beat }
            };
            let result = export::run_export(
                path_for_thread,
                engine_shared_clone,
                project,
                song,
                sample_rate,
                common::process_data::MAX_FRAMES,
                span,
                false,
                // Clip-range bounce has no progress overlay (it
                // completes quickly and replaces the clip in place).
                |_, _| {},
            );
            // Release the engine reservation on every path.
            engine_shared_release
                .export_running
                .store(false, Ordering::Release);
            let (error_msg, frames) = match result {
                // Bounce has no Cancel UI; `outcome.cancelled` is
                // ignored (it can only be set if a stale cancel
                // slipped through, which the recv-loop reset prevents).
                Ok(outcome) => (None, outcome.frames),
                Err(e) => {
                    tracing::error!(error = ?e, "offline plugin-FX bounce failed");
                    (Some(format!("{e:#}")), 0)
                }
            };
            let _ = out_tx_clone.send(AudioEvent::BounceClipFxComplete {
                project: key,
                path: path_for_complete,
                source_track,
                source_clip,
                error: error_msg,
                frames,
            });
        })
    {
        tracing::error!(error = ?e, "failed to spawn bounce thread");
        // No thread will release the reservation we took above.
        engine_shared.export_running.store(false, Ordering::Release);
        let _ = out_tx.send(failed(path, format!("failed to spawn bounce thread: {e}")));
    }
}
