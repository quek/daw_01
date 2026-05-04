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
use common::protocol::{ChildKind, MainToChild};
use common::wire::read_msg;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::net::windows::named_pipe::NamedPipeClient;

mod audio_worker;
mod engine;
mod mixer;
mod sequencer;
mod tracks;
mod vocal;

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

    recv_loop(pipe, shared, master_gain, cmd_tx).await;
    tracing::info!("daw_audio exiting");
    Ok(())
}

async fn recv_loop(
    mut pipe: NamedPipeClient,
    shared: Arc<SharedState>,
    master_gain: Arc<AtomicU32>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<engine::AudioCommand>,
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
            Ok(MainToChild::LoadSong(song)) => {
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
                slot,
            }) => {
                if let Err(e) =
                    handle_open_plugin_shmem(plugin_id, &shmem_id, track, slot, &cmd_tx)
                {
                    tracing::error!(error = ?e, plugin_id, "failed to open plugin shmem");
                }
            }
            Ok(MainToChild::ClosePluginShmem { plugin_id }) => {
                let _ = cmd_tx.send(engine::AudioCommand::ClosePluginShmem { plugin_id });
            }
            Ok(MainToChild::SetTrackVolume { track, volume }) => {
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.get_mut(track as usize) {
                        t.volume = volume.clamp(0.0, 1.0);
                    }
                });
            }
            Ok(MainToChild::SetTrackPan { track, pan }) => {
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.get_mut(track as usize) {
                        t.pan = pan.clamp(-1.0, 1.0);
                    }
                });
            }
            Ok(MainToChild::SetTrackMuted { track, muted }) => {
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.get_mut(track as usize) {
                        t.muted = muted;
                    }
                });
            }
            Ok(MainToChild::SetTrackSolo { track, solo }) => {
                update_song_track(&shared, |s| {
                    if let Some(t) = s.tracks.get_mut(track as usize) {
                        t.solo = solo;
                    }
                });
            }
            Ok(MainToChild::SetVocalAudio {
                track,
                clip: _,
                clip_start_samples,
                sample_rate: _,
                samples,
            }) => {
                let _ = cmd_tx.send(engine::AudioCommand::SetVocalAudio {
                    track,
                    clip_start_samples,
                    samples,
                });
            }
            // Plugin lifecycle, GUI, state save/restore, export, vocal
            // synthesis, per-track mixer params, slot reorder, and the
            // plugin-host CloseWorkerPool tear-down all stay on the
            // plugin_host side (or move to daw_audio in PR6d+).
            Ok(MainToChild::Ack)
            | Ok(MainToChild::Session(_))
            | Ok(MainToChild::SetSlotPlugin { .. })
            | Ok(MainToChild::RemoveSlotPlugin { .. })
            | Ok(MainToChild::MoveSlot { .. })
            | Ok(MainToChild::RemoveTrack { .. })
            | Ok(MainToChild::SwapTracks { .. })
            | Ok(MainToChild::ReorderTracks(_))
            | Ok(MainToChild::RequestSlotState { .. })
            | Ok(MainToChild::RequestAllStates)
            | Ok(MainToChild::OpenSlotGuiEmbedded { .. })
            | Ok(MainToChild::CloseSlotGui { .. })
            | Ok(MainToChild::ResizeSlotGui { .. })
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
    slot: common::protocol::PluginSlot,
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
            track,
            slot,
        })
        .map_err(|_| anyhow::anyhow!("audio command channel closed"))?;
    // The handle owns the shmem mapping. We can't move it across to the
    // audio thread inside the command — instead, the engine's PluginRef
    // is just a raw pointer, so this side has to keep the mapping alive
    // until ClosePluginShmem. Stash it in a leaky static for now; PR8
    // will hang it off a proper `PluginShmemRegistry`.
    let leaked = Box::leak(Box::new(handle));
    let _ = leaked;
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

                local.process_buffer(&shared, session_sample_rate, frames);

                // A2: publish the engine's playhead to shmem so the GUI
                // can draw the cursor. `u64::MAX` is the "not playing"
                // sentinel the GUI already understands.
                let published_ph = if local.playing {
                    shared.playhead.load(Ordering::Acquire)
                } else {
                    u64::MAX
                };
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
