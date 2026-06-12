use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};

// Debug-only: route every heap allocation through `AllocDisabler` so the
// `assert_no_alloc!(...)` blocks inside audio worker code panic the
// instant an RT path tries to allocate. Enabled by `--features rt-assert`.
#[cfg(feature = "rt-assert")]
#[global_allocator]
static GLOBAL: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
use common::audio_bridge::AudioBridgeHandle;
use common::meter::compute_block_peak;
use common::protocol::{ChildKind, ChildToMain, MainToChild};
use common::wire::{read_msg, write_msg};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::io::ReadHalf;
use tokio::net::windows::named_pipe::NamedPipeClient;

mod audio_clip_renderer;
mod audio_worker;
mod automation;
mod engine;
mod export;
mod graph;
mod mixer;
mod sequencer;

use engine::{EngineShared, LocalState, PlaybackCommand, SharedState};

#[tokio::main]
async fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_audio started");

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    let mut pipe = common::client::perform_handshake(&pipe_name, ChildKind::Audio).await?;
    tracing::info!("daw_audio handshake complete");

    let session = common::client::read_session(&mut pipe).await?;
    tracing::info!(?session, "audio session ready");

    let bridge = Arc::new(
        AudioBridgeHandle::open(&session.shmem_id).context("failed to open audio shmem")?,
    );

    let shared = Arc::new(SharedState::new());
    // Engine resources shared between the CPAL closure and (in A3) the
    // export thread. Held by `LocalState` for the audio path; export
    // will hold its own clone.
    let engine_shared = Arc::new(EngineShared::new());
    // Master gain stays a separate atomic from `SharedState` because the
    // CPAL closure applies it on the device-final samples (post-engine).
    let master_gain = Arc::new(AtomicU32::new(1.0_f32.to_bits()));

    // AudioCommand channel: the receive loop pushes handle-bearing
    // commands (OpenWorkerPool / OpenPluginShmem / ClosePluginShmem)
    // into this; the audio thread drains it at the top of every buffer.
    let (cmd_tx, cmd_rx) =
        tokio::sync::mpsc::unbounded_channel::<engine::AudioCommand>();

    let _stream = start_output_stream(
        Arc::clone(&shared),
        Arc::clone(&engine_shared),
        Arc::clone(&bridge),
        Arc::clone(&master_gain),
        session.sample_rate,
        cmd_rx,
    )
    .context("failed to start audio stream")?;
    tracing::info!("audio stream running");

    // Split the pipe so the receive loop can keep reading while the
    // export thread (off-tokio) ships completion notifications back to
    // daw_gui. `out_rx` drains the queue on a single tokio task so the
    // pipe writer is single-owner.
    let (read_half, mut write_half) = tokio::io::split(pipe);
    let (out_tx, mut out_rx) =
        tokio::sync::mpsc::unbounded_channel::<ChildToMain>();
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Err(e) = write_msg(&mut write_half, &msg).await {
                tracing::error!(error = ?e, "failed to send ChildToMain from daw_audio");
                break;
            }
        }
    });

    recv_loop(
        read_half,
        shared,
        Arc::clone(&engine_shared),
        master_gain,
        session.sample_rate,
        cmd_tx,
        out_tx,
    )
    .await;
    tracing::info!("daw_audio exiting");
    Ok(())
}

async fn recv_loop(
    mut pipe: ReadHalf<NamedPipeClient>,
    shared: Arc<SharedState>,
    engine_shared: Arc<EngineShared>,
    master_gain: Arc<AtomicU32>,
    session_sample_rate: u32,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<engine::AudioCommand>,
    out_tx: tokio::sync::mpsc::UnboundedSender<ChildToMain>,
) {
    loop {
        match read_msg::<_, MainToChild>(&mut pipe).await {
            Ok(MainToChild::Play) => {
                tracing::info!("received Play");
                shared
                    .playback
                    .store(PlaybackCommand::Play as u8, Ordering::Release);
            }
            Ok(MainToChild::Stop) => {
                tracing::info!("received Stop");
                shared
                    .playback
                    .store(PlaybackCommand::Stop as u8, Ordering::Release);
            }
            Ok(MainToChild::SetLoop(b)) => {
                shared.looping.store(b, Ordering::Release);
            }
            Ok(MainToChild::SeekTo { samples }) => {
                // FIXME #41: playhead を IPC 受信スレッドから直接書かない。
                // audio thread も buffer 末で playhead を store するため、両者が
                // 同一 atomic を別スレッドから書く race になり、Stop 直後の開始
                // 位置への巻き戻しが in-flight buffer の advance に上書きされて
                // 停止位置から再生されるバグ (= FIXME #41) を生む。seek 要求は
                // pending_seek に積み、audio thread が process_buffer 冒頭で
                // swap 消費して playhead に反映する (playhead の writer を audio
                // thread 単独に保つ)。ruler click / Stop 復帰の双方ともこの経路。
                shared.pending_seek.store(samples, Ordering::Release);
                tracing::info!(samples, "received SeekTo");
            }
            Ok(MainToChild::LoadSong(mut song)) => {
                // IPC は信頼境界なので、 受信した song の値域を store 前に
                // 正規化 (bpm/time_sig/length/loop/framerate を有限・正に)。
                // これで下流の divisor (samples_per_beat 等) が NaN / 0 /
                // 負値で壊れない。 idempotent。
                song.sanitize_ranges();
                // PR6: AudioClipRenderer を再 build (WAV decode + event
                // schedule flatten)。 LoadSong は IPC 受信スレッドから
                // 呼ばれるので decode はここで synchronous (Phase 2 で
                // background 化、 docs/plan_audio_clip.md §11)。
                let project_dir_g = engine_shared.project_dir.load();
                let project_dir: Option<std::path::PathBuf> =
                    project_dir_g.as_ref().map(|arc| (**arc).clone());
                let renderer = audio_clip_renderer::compile_audio_schedule(
                    &song,
                    project_dir.as_deref(),
                    session_sample_rate,
                );
                engine_shared
                    .audio_clip_renderer
                    .store(Arc::new(renderer));
                shared.song.store(Some(Arc::new(song)));
            }
            Ok(MainToChild::SetMasterGain(g)) => {
                let clamped = g.clamp(0.0, 1.0);
                master_gain.store(clamped.to_bits(), Ordering::Relaxed);
            }
            Ok(MainToChild::OpenWorkerPool {
                n_workers,
                worker_bridge_shmem_id,
                wake_event_names,
                done_event_names,
            }) => {
                if let Err(e) = handle_open_worker_pool(
                    n_workers,
                    &worker_bridge_shmem_id,
                    &wake_event_names,
                    &done_event_names,
                    &cmd_tx,
                ) {
                    tracing::error!(error = ?e, "failed to open audio-side worker pool");
                }
            }
            Ok(MainToChild::OpenPluginShmem {
                plugin_id,
                shmem_id,
                track,
                index,
            }) => {
                if let Err(e) =
                    handle_open_plugin_shmem(plugin_id, &shmem_id, track, index, &cmd_tx)
                {
                    tracing::error!(error = ?e, plugin_id, "failed to open plugin shmem");
                }
            }
            Ok(MainToChild::ClosePluginShmem { plugin_id }) => {
                let _ = cmd_tx.send(engine::AudioCommand::ClosePluginShmem { plugin_id });
            }
            // FIXME #32: a chain reorder re-keys `slot_to_plugin_id` so each
            // slot resolves to its moved plugin. Sent to the audio engine in
            // addition to the plugin host (and a `LoadSong` that rebuilds the
            // processing order). Atomic re-key on the audio thread; see
            // `AudioCommand::ReorderChain`.
            Ok(MainToChild::ReorderChain { track, moves }) => {
                let _ = cmd_tx.send(engine::AudioCommand::ReorderChain { track, moves });
            }
            // Phase 6 review (SSOT fix): `track` field を Track::id (stable)
            // に統一。 旧コードは `s.tracks.get_mut(track as usize)` で Vec
            // index 解釈していて、 GUI 側との順序ずれで違う track を操作する
            // race リスクがあった。 id lookup で stable 化。
            Ok(MainToChild::SetTrackVolume { track, volume }) => {
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.iter_mut().find(|t| t.id == track) {
                        t.volume = volume.clamp(0.0, 1.0);
                    }
                });
            }
            Ok(MainToChild::SetTrackPan { track, pan }) => {
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.iter_mut().find(|t| t.id == track) {
                        t.pan = pan.clamp(-1.0, 1.0);
                    }
                });
            }
            Ok(MainToChild::SetTrackMuted { track, muted }) => {
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.iter_mut().find(|t| t.id == track) {
                        t.muted = muted;
                    }
                });
            }
            Ok(MainToChild::SetTrackSolo { track, solo }) => {
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.iter_mut().find(|t| t.id == track) {
                        t.solo = solo;
                    }
                });
            }
            Ok(MainToChild::SetSendGain {
                track,
                send_idx,
                gain,
            }) => {
                // Realtime aux-send level. Same lightweight clone-mutate-
                // store path as SetTrackVolume — the MixSend op re-reads
                // this live (ramped) without recompiling the schedule.
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.iter_mut().find(|t| t.id == track)
                        && let Some(send) = t.sends.get_mut(send_idx as usize)
                    {
                        send.gain = gain.clamp(0.0, 2.0);
                    }
                });
            }
            Ok(MainToChild::SetSendEnabled {
                track,
                send_idx,
                enabled,
            }) => {
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.iter_mut().find(|t| t.id == track)
                        && let Some(send) = t.sends.get_mut(send_idx as usize)
                    {
                        send.enabled = enabled;
                    }
                });
            }
            Ok(MainToChild::SetTrackArmed { track, armed }) => {
                // Phase 7 B4 (2026-05-13): track.armed を Song に反映するのみ。
                // 録音書き込み自体は GUI process で行うため audio thread 側
                // は schema 一貫性のために値を持つだけ。 将来の audio input
                // 録音で audio thread 側書き込みに使う想定。
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.iter_mut().find(|t| t.id == track) {
                        t.armed = armed;
                    }
                });
            }
            Ok(MainToChild::SetSongBpm { bpm }) => {
                // Phase 5 Step 5.1 follow-up: BPM 軽量更新。 LoadSong を回避
                // して shared.song の inner bpm のみ swap。 ArcSwap で clone →
                // mutate → store の atomic publish。 BPM scrub drag 中の毎
                // frame 入力で audio engine が即時追随する。
                let clamped = bpm.clamp(1.0, 400.0);
                update_song_track(&shared, |s| {
                    s.bpm = clamped;
                });
            }
            Ok(MainToChild::SetSongTimeSigNumerator { num }) => {
                let clamped = num.clamp(1, 32);
                update_song_track(&shared, |s| {
                    s.time_sig.0 = clamped;
                });
            }
            Ok(MainToChild::StartCountIn { samples }) => {
                // Phase 7 B4 Step C (2026-05-13): GUI が Record toggle ON +
                // count_in_bars > 0 で発火。 EngineShared に preroll を立てて、
                // process_buffer 頭で「dispatch / clip render skip + metronome
                // のみ render」 ループに入る。 0 到達で通常再生復帰、 GUI 側は
                // audio_bridge mirror 経由で midi_recording_pending 解除。
                // samples = 0 で count-in 即時 cancel (= stop_recording 中の
                // preroll 中断)。
                use std::sync::atomic::Ordering;
                engine_shared
                    .preroll_total_samples
                    .store(samples, Ordering::Release);
                engine_shared
                    .preroll_remaining_samples
                    .store(samples, Ordering::Release);
                tracing::info!(samples, "received StartCountIn");
            }
            Ok(MainToChild::SetMetronomeEnabled(enabled)) => {
                // Phase 7 B3 (2026-05-13): GUI が transport bar の metronome
                // toggle を切り替え。 SharedState 上の AtomicBool を replace し、
                // audio thread は次 buffer から `render_metronome` の有効無効を
                // 切り替える (= 無効時は mix step を skip)。 lock-free / 0
                // allocation on audio thread。
                shared.metronome_enabled.store(
                    enabled,
                    std::sync::atomic::Ordering::Release,
                );
            }
            Ok(MainToChild::PreviewNoteOn {
                track_id,
                pitch,
                velocity,
            }) => {
                // 鍵盤レーン click のプレビュー (gui_01 #055)。 GUI は track id を
                // 送る。 ここで audio engine の現 song snapshot から Vec index を
                // 引いて AudioCommand に載せ替える (index は scratch / per-track
                // dispatch の addressing)。 解決は IPC スレッド上 = RT 外。 song
                // 未ロード / id 不在なら drop (= 無音、 plan §4 の no-op)。
                if let Some(track) = preview_track_index(&shared, track_id) {
                    let _ = cmd_tx.send(engine::AudioCommand::PreviewNoteOn {
                        track,
                        pitch,
                        velocity: f64::from(velocity) / 127.0,
                    });
                }
            }
            Ok(MainToChild::PreviewNoteOff { track_id, pitch }) => {
                if let Some(track) = preview_track_index(&shared, track_id) {
                    let _ =
                        cmd_tx.send(engine::AudioCommand::PreviewNoteOff { track, pitch });
                }
            }
            Ok(MainToChild::SetRecordingLanes { lanes }) => {
                // Phase 4 Step C-2: GUI が「現在 recording 中の lane」 セットを
                // 送ってきた。 ArcSwap で snapshot を replace し、 audio thread
                // は次 buffer から `fill_track_param_ramps` で該当 lane の
                // curve eval を skip する (= track.volume / track.pan の live
                // value がそのまま出力される、 user の knob 操作がそのまま
                // 聞こえる)。 lock-free / 0 allocation on audio thread。
                let set: std::collections::HashSet<(u32, common::model::AutomationTarget)> =
                    lanes.into_iter().collect();
                shared.recording_lanes.store(std::sync::Arc::new(set));
            }
            // SetGeneratedAudio (Phase 1 PR8): in-memory audio buffer
            // delivered by the GUI. Used both for
            // PR-V4: `MainToChild::SetGeneratedAudio` 削除済 (= VOICEVOX
            // 経路は builtin instrument plugin が plugin host 内で完結)。
            // 互換性のため variant をしばらく残す場合は ignore arm を
            // 入れるが、 完全削除したのでここは何もしない。
            // SetProjectDir (Phase 1 PR2): record the current project
            // directory so PR6's `compile_audio_schedule` can resolve
            // `AudioSourcePath::ProjectRelative` against
            // `<project_dir>/samples/<...>`. `None` for unsaved
            // projects.
            Ok(MainToChild::SetProjectDir(dir)) => {
                engine_shared
                    .project_dir
                    .store(dir.as_ref().map(|p| Arc::new(p.clone())));
                tracing::info!(?dir, "project_dir updated");
            }
            // ExportWav: kick off the offline render on a dedicated
            // thread so the IPC receive loop stays responsive. The
            // export thread silences the CPAL callback via
            // `EngineShared::export_running` while it holds the audio
            // resources.
            Ok(MainToChild::ExportWav { path }) => {
                let song_snap = shared.song.load();
                let Some(song_arc) = song_snap.as_ref() else {
                    tracing::warn!("ExportWav received but no song loaded");
                    let _ = out_tx.send(ChildToMain::ExportWavComplete {
                        error: Some("no song loaded".into()),
                    });
                    continue;
                };
                let song = (**song_arc).clone();
                drop(song_snap);
                let engine_shared_clone = Arc::clone(&engine_shared);
                let out_tx_clone = out_tx.clone();
                let sample_rate = session_sample_rate;
                if let Err(e) = std::thread::Builder::new()
                    .name("daw-audio-export".into())
                    .spawn(move || {
                        let result = export::run_export(
                            path,
                            engine_shared_clone,
                            song,
                            sample_rate,
                            common::process_data::MAX_FRAMES,
                            None,
                        );
                        let error_msg = match result {
                            Ok(_frames) => None,
                            Err(e) => {
                                tracing::error!(error = ?e, "offline WAV export failed");
                                Some(format!("{e:#}"))
                            }
                        };
                        let _ = out_tx_clone
                            .send(ChildToMain::ExportWavComplete { error: error_msg });
                    })
                {
                    tracing::error!(error = ?e, "failed to spawn export thread");
                    let _ = out_tx.send(ChildToMain::ExportWavComplete {
                        error: Some(format!("failed to spawn export thread: {e}")),
                    });
                }
            }
            Ok(MainToChild::BounceClipFxOnline {
                path,
                source_track,
                source_clip,
                start_frame,
                end_frame,
            }) => {
                let song_snap = shared.song.load();
                let Some(song_arc) = song_snap.as_ref() else {
                    tracing::warn!("BounceClipFxOnline received but no song loaded");
                    let _ = out_tx.send(ChildToMain::BounceClipFxComplete {
                        path,
                        source_track,
                        source_clip,
                        error: Some("no song loaded".into()),
                        frames: 0,
                    });
                    continue;
                };
                let song = (**song_arc).clone();
                drop(song_snap);
                let engine_shared_clone = Arc::clone(&engine_shared);
                let out_tx_clone = out_tx.clone();
                let sample_rate = session_sample_rate;
                let path_for_thread = path.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("daw-audio-bounce-fx".into())
                    .spawn(move || {
                        let path_for_complete = path_for_thread.clone();
                        let result = export::run_export(
                            path_for_thread,
                            engine_shared_clone,
                            song,
                            sample_rate,
                            common::process_data::MAX_FRAMES,
                            Some((start_frame, end_frame)),
                        );
                        let (error_msg, frames) = match result {
                            Ok(frames) => (None, frames),
                            Err(e) => {
                                tracing::error!(
                                    error = ?e,
                                    "offline plugin-FX bounce failed"
                                );
                                (Some(format!("{e:#}")), 0)
                            }
                        };
                        let _ = out_tx_clone.send(ChildToMain::BounceClipFxComplete {
                            path: path_for_complete,
                            source_track,
                            source_clip,
                            error: error_msg,
                            frames,
                        });
                    })
                {
                    tracing::error!(error = ?e, "failed to spawn bounce thread");
                    let _ = out_tx.send(ChildToMain::BounceClipFxComplete {
                        path,
                        source_track,
                        source_clip,
                        error: Some(format!("failed to spawn bounce thread: {e}")),
                        frames: 0,
                    });
                }
            }
            // Plugin lifecycle, GUI, state save/restore, per-track
            // mixer params, slot reorder, render-mode bookend, and the
            // plugin-host worker-pool tear-down stay on the
            // plugin_host side.
            Ok(MainToChild::Ack)
            | Ok(MainToChild::Session(_))
            | Ok(MainToChild::SetSlotPlugin { .. })
            | Ok(MainToChild::RemoveSlotPlugin { .. })
            | Ok(MainToChild::RemoveTrack { .. })
            | Ok(MainToChild::RequestSlotState { .. })
            | Ok(MainToChild::RequestAllStates)
            | Ok(MainToChild::OpenSlotGuiEmbedded { .. })
            | Ok(MainToChild::CloseSlotGui { .. })
            | Ok(MainToChild::SetRenderMode(_))
            | Ok(MainToChild::SetBuiltinPluginNoteMetadata { .. })
            // FIXME #42: 歌唱合成は plugin host が担うので audio engine は無視。
            | Ok(MainToChild::PrepareVocalSynth { .. })
            | Ok(MainToChild::CloseWorkerPool) => {}
            Err(e) => {
                tracing::info!(error = ?e, "receive loop ending");
                break;
            }
        }
    }
}

/// Open the WorkerBridge shmem + N (wake, done) events for the audio
/// side, build N `WorkerSyncRef`s pointing at the bridge slots, and
/// hand the bundle to the audio thread via the command channel.
fn handle_open_worker_pool(
    n_workers: u32,
    worker_bridge_shmem_id: &str,
    wake_event_names: &[String],
    done_event_names: &[String],
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<engine::AudioCommand>,
) -> Result<()> {
    anyhow::ensure!(
        wake_event_names.len() == n_workers as usize,
        "wake_event_names len {} != n_workers {}",
        wake_event_names.len(),
        n_workers
    );
    anyhow::ensure!(
        done_event_names.len() == n_workers as usize,
        "done_event_names len {} != n_workers {}",
        done_event_names.len(),
        n_workers
    );
    // IPC 由来の n_workers で worker_task[i] を indexing する前に上限検証
    // (out-of-bounds panic を防ぐ)。
    anyhow::ensure!(
        (n_workers as usize) <= common::worker_bridge::MAX_WORKERS,
        "n_workers {} exceeds MAX_WORKERS",
        n_workers
    );
    let bridge = common::worker_bridge::WorkerBridgeHandle::open(worker_bridge_shmem_id)
        .context("failed to open worker_bridge shmem")?;
    // Per-slot pointer into the bridge's worker_task array — stable for
    // the bridge's lifetime, which the audio thread holds (see
    // LocalState::worker_bridge).
    let bridge_ref = bridge.bridge();
    let mut worker_syncs = Vec::with_capacity(n_workers as usize);
    for i in 0..n_workers as usize {
        let wake = common::plugin_ref::create_named_event(&wake_event_names[i])
            .with_context(|| format!("failed to open wake event {i}"))?;
        let done = common::plugin_ref::create_named_event(&done_event_names[i])
            .with_context(|| format!("failed to open done event {i}"))?;
        worker_syncs.push(common::plugin_ref::WorkerSyncRef {
            worker_idx: i as u32,
            worker_task: &bridge_ref.worker_task[i] as *const _,
            event_wake: wake,
            event_done: done,
        });
    }
    cmd_tx
        .send(engine::AudioCommand::OpenWorkerPool {
            bridge,
            worker_syncs,
        })
        .map_err(|_| anyhow::anyhow!("audio command channel closed"))?;
    Ok(())
}

/// Apply `f` to a clone of the current song and publish the result.
/// `ArcSwap` keeps the swap wait-free for the audio thread; the clone
/// happens on the IPC thread, which is acceptable because mixer-strip
/// changes are user-driven (slider drag rate, not per-buffer).
/// 鍵盤プレビューの note-on/off を送る対象 track の Vec index を、 audio engine
/// の現 song snapshot から track id で引く。 song 未ロード / id 不在 / `MAX_TRACKS`
/// 超過は `None` (= プレビュー drop)。 id ベースなので GUI 側の track 並べ替えと
/// race しない (= `SetTrackVolume` 等と同じ方針)。
fn preview_track_index(shared: &Arc<engine::SharedState>, track_id: u32) -> Option<usize> {
    let snapshot = shared.song.load();
    let song = snapshot.as_deref()?;
    song.tracks
        .iter()
        .position(|t| t.id == track_id)
        .filter(|&i| i < engine::MAX_TRACKS)
}

fn update_song_track<F>(shared: &Arc<engine::SharedState>, f: F)
where
    F: FnOnce(&mut common::model::Song),
{
    let snapshot = shared.song.load();
    let Some(song) = snapshot.as_deref() else {
        return;
    };
    let mut next = song.clone();
    f(&mut next);
    shared.song.store(Some(Arc::new(next)));
}

/// Open the per-plugin `ProcessData` shmem and ship a `PluginRef` to the
/// audio thread along with the (track, slot) it's assigned to.
fn handle_open_plugin_shmem(
    plugin_id: u32,
    shmem_id: &str,
    track: u32,
    index: u32,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<engine::AudioCommand>,
) -> Result<()> {
    let handle = common::process_data::ProcessDataHandle::open(shmem_id)
        .context("failed to open ProcessData shmem")?;
    let plugin_ref = common::plugin_ref::PluginRef {
        plugin_id,
        process_data: handle.ptr(),
    };
    cmd_tx
        .send(engine::AudioCommand::OpenPluginShmem {
            plugin_id,
            plugin_ref,
            handle,
            track,
            index,
        })
        .map_err(|_| anyhow::anyhow!("audio command channel closed"))?;
    Ok(())
}

fn start_output_stream(
    shared: Arc<SharedState>,
    engine_shared: Arc<EngineShared>,
    bridge: Arc<AudioBridgeHandle>,
    master_gain: Arc<AtomicU32>,
    session_sample_rate: u32,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<engine::AudioCommand>,
) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default output device")?;
    let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
    let supported = device
        .default_output_config()
        .context("failed to query default output config")?;

    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels();
    let sample_format = supported.sample_format();

    tracing::info!(
        device = %device_name,
        sample_rate,
        channels,
        ?sample_format,
        "opening output stream"
    );

    if sample_format != cpal::SampleFormat::F32 {
        anyhow::bail!("unsupported sample format: {sample_format:?}, expected F32");
    }

    let config: cpal::StreamConfig = supported.into();
    let stream = build_stream(
        &device,
        &config,
        channels,
        shared,
        engine_shared,
        bridge,
        master_gain,
        session_sample_rate,
        cmd_rx,
    )?;
    stream.play().context("failed to start stream")?;
    Ok(stream)
}

#[allow(clippy::too_many_arguments)]
fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: u16,
    shared: Arc<SharedState>,
    engine_shared: Arc<EngineShared>,
    bridge: Arc<AudioBridgeHandle>,
    master_gain: Arc<AtomicU32>,
    session_sample_rate: u32,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<engine::AudioCommand>,
) -> Result<cpal::Stream> {
    let channels_usize = channels as usize;
    let max_frames = common::process_data::MAX_FRAMES;
    // `LocalState` is the CPAL closure's exclusive heap. It holds
    // master_l/r and the per-track scratch — pre-allocated here, never
    // touched outside the audio thread.
    let mut local = LocalState::new(max_frames, cmd_rx, engine_shared);

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                let frames = (data.len() / channels_usize).min(max_frames);

                local.process_buffer(&shared, &bridge, session_sample_rate, frames);

                // A2: publish the engine's playhead to shmem so the GUI
                // can draw the cursor. 停止中も現在の playhead をそのまま
                // publish する (= ruler click で動かした位置や、 Stop 直前
                // の位置を GUI に反映、 業界標準の挙動)。 `u64::MAX` は
                // bridge の未初期化値 (= audio thread が一度も書いてない
                // 状態) として残す。
                let published_ph = shared.playhead.load(Ordering::Acquire);
                bridge.set_playhead_samples(published_ph);

                let gain = f32::from_bits(master_gain.load(Ordering::Relaxed));

                // Interleave master_l/r into the device buffer, applying
                // master_gain. Lanes beyond stereo on the device are
                // zeroed.
                unsafe {
                    let dst = data.as_mut_ptr();
                    for i in 0..frames {
                        let l = local.master_l[i] * gain;
                        let r = local.master_r[i] * gain;
                        let out = dst.add(i * channels_usize);
                        *out = l;
                        if channels_usize > 1 {
                            *out.add(1) = r;
                        }
                        for c in 2..channels_usize {
                            *out.add(c) = 0.0;
                        }
                    }
                }
                let filled = frames * channels_usize;
                for s in &mut data[filled..] {
                    *s = 0.0;
                }

                let (peak_l, peak_r) = block_peaks_stereo(data, channels_usize);
                bridge.set_peaks(peak_l, peak_r);
            },
            |err| tracing::error!(?err, "audio stream error"),
            None,
        )
        .context("failed to build output stream")?;
    Ok(stream)
}

/// Scan interleaved `data` (stride = `channels`) for the per-channel peak of
/// the first two channels. RT-safe: a single pass, no allocation.
fn block_peaks_stereo(data: &[f32], channels: usize) -> (f32, f32) {
    if channels == 0 || data.is_empty() {
        return (0.0, 0.0);
    }
    if channels == 1 {
        let m = compute_block_peak(data);
        return (m, m);
    }
    let mut pl = 0.0_f32;
    let mut pr = 0.0_f32;
    for frame in data.chunks_exact(channels) {
        let l = frame[0].abs();
        let r = frame[1].abs();
        if l > pl {
            pl = l;
        }
        if r > pr {
            pr = r;
        }
    }
    (pl, pr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_peaks_stereo_empty_is_zero() {
        assert_eq!(block_peaks_stereo(&[], 2), (0.0, 0.0));
    }

    #[test]
    fn block_peaks_stereo_mono_duplicates() {
        let data = [0.1, -0.5, 0.3];
        assert_eq!(block_peaks_stereo(&data, 1), (0.5, 0.5));
    }

    #[test]
    fn block_peaks_stereo_interleaved_picks_per_channel_max() {
        let data = [0.1, -0.4, -0.2, 0.3, 0.05, -0.5];
        assert_eq!(block_peaks_stereo(&data, 2), (0.2, 0.5));
    }
}
