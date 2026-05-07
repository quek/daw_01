//! Offline WAV render. Drives the same `AudioWorkerPool` and per-plugin
//! `ProcessData` shmem the live audio thread uses, but freewheels through
//! the song as fast as the plugin chain allows. The CPAL callback
//! cooperates by checking `EngineShared::export_running` and writing
//! silence while we hold the resources (see [`engine::LocalState::process_buffer`]).
//!
//! Threading model: caller spawns `run_export` on a dedicated
//! `std::thread`. The export thread:
//!
//! 1. Sets `EngineShared::export_running = true` to mute the CPAL callback.
//! 2. Allocates its own `scratch` / `master_l` / `master_r` (heap is fine
//!    here; this thread is RT-irrelevant once the realtime callback is
//!    parked).
//! 3. Walks the song from frame 0 to `length_beats * samples_per_beat`,
//!    calling `pool.dispatch_and_wait` (or the serial fallback) every
//!    buffer and writing the master bus into `hound::WavWriter`.
//! 4. Clears `export_running` so live playback can resume.
//!
//! `clap_plugin_render.set(CLAP_RENDER_OFFLINE)` is bookended by the GUI
//! around this call (it sends `MainToChild::SetRenderMode(Offline)` to
//! the plugin host before triggering the export, and `Realtime` after).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use common::model::Song;
use hound::{SampleFormat, WavSpec, WavWriter};

use crate::engine::{
    EngineShared, MAX_TRACKS, execute_schedule_post_dispatch, process_track_owned,
};
use crate::graph::compile_schedule;
use crate::mixer::TrackScratch;

/// Hard ceiling on the rendered tail (10 s past `length_beats`). Stops
/// runaway plugins (long reverbs / oscillators with no auto-stop) from
/// inflating the WAV indefinitely.
const TAIL_MAX_SECONDS: u64 = 10;

/// Run the offline WAV export to completion. Blocks the caller until the
/// file is finalised. RT-irrelevant — the CPAL callback writes silence
/// while `engine_shared.export_running` is set.
pub fn run_export(
    path: PathBuf,
    engine_shared: Arc<EngineShared>,
    song: Song,
    sample_rate: u32,
    max_frames: usize,
) -> Result<()> {
    if song.bpm <= 0.0 {
        anyhow::bail!("song.bpm must be positive (got {})", song.bpm);
    }
    let n_tracks = song.tracks.len().min(MAX_TRACKS);
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let song_length_samples = (song.length_beats * samples_per_beat).max(0.0) as u64;
    let tail_max_samples = u64::from(sample_rate) * TAIL_MAX_SECONDS;
    let total_samples = song_length_samples + tail_max_samples;

    tracing::info!(
        path = %path.display(),
        sample_rate,
        n_tracks,
        song_length_samples,
        "starting offline WAV export"
    );

    let spec = WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer = WavWriter::create(&path, spec)
        .with_context(|| format!("failed to create WAV {}", path.display()))?;

    let mut scratch: Vec<TrackScratch> = (0..MAX_TRACKS).map(|_| TrackScratch::new()).collect();
    let mut master_l: Vec<f32> = vec![0.0; max_frames];
    let mut master_r: Vec<f32> = vec![0.0; max_frames];

    // Tell the CPAL callback to silence itself.
    engine_shared.export_running.store(true, Ordering::Release);

    let render_result = render_loop(
        &engine_shared,
        &song,
        n_tracks,
        sample_rate,
        max_frames,
        total_samples,
        song_length_samples,
        &mut scratch,
        &mut master_l,
        &mut master_r,
        &mut writer,
    );

    // Always clear the flag, even on render error, so live playback
    // never gets wedged.
    engine_shared.export_running.store(false, Ordering::Release);

    render_result?;

    writer
        .finalize()
        .with_context(|| format!("failed to finalize WAV {}", path.display()))?;

    tracing::info!(path = %path.display(), "offline WAV export finished");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_loop(
    engine_shared: &EngineShared,
    song: &Song,
    n_tracks: usize,
    sample_rate: u32,
    max_frames: usize,
    total_samples: u64,
    song_length_samples: u64,
    scratch: &mut [TrackScratch],
    master_l: &mut [f32],
    master_r: &mut [f32],
    writer: &mut WavWriter<std::io::BufWriter<std::fs::File>>,
) -> Result<()> {
    // Tail-silence cutoff: stop early if the master bus stays under
    // -60 dB for half a second once we're past the song body.
    let silence_thresh: f32 = 0.001;
    let silence_cutoff_samples = u64::from(sample_rate) / 2;
    let mut silence_counter: u64 = 0;

    let any_solo = song.tracks.iter().any(|t| t.solo);

    // Compile the routing schedule once for the whole render — same
    // structure as the live audio thread's `cached_schedule`. PDC
    // compensation (`ApplyDelay`), group buses (`Mix → TrackScratch`)
    // and SidechainTap all live in here; without using it the export
    // would silently bypass PR3 PDC and mis-render group hierarchies.
    let mut schedule = compile_schedule(song)
        .map_err(|e| anyhow::anyhow!("export schedule compile failed: {e:?}"))?;

    let mut playhead: u64 = 0;
    while playhead < total_samples {
        let remaining = total_samples - playhead;
        let frames = (remaining as usize).min(max_frames);
        let frames_u32 = frames as u32;

        master_l[..frames].fill(0.0);
        master_r[..frames].fill(0.0);

        // Snapshot the same wait-free state the audio thread reads.
        let plugin_refs_g = engine_shared.plugin_refs.load();
        let slot_map_g = engine_shared.slot_to_plugin_id.load();
        let vocal_store_g = engine_shared.vocal_store.load();
        let worker_syncs_g = engine_shared.worker_syncs.load();
        let pool_g = engine_shared.worker_pool.load();
        let audio_renderer_g = engine_shared.audio_clip_renderer.load();
        let audio_renderer: &crate::audio_clip_renderer::AudioClipRenderer =
            &audio_renderer_g;

        if let Some(pool) = pool_g.as_deref() {
            pool.dispatch_and_wait(
                Some(song),
                &mut scratch[..n_tracks],
                &plugin_refs_g,
                &slot_map_g,
                &vocal_store_g,
                audio_renderer,
                &worker_syncs_g,
                &mut master_l[..frames],
                &mut master_r[..frames],
                sample_rate,
                playhead,
                frames_u32,
                true,
                any_solo,
                &schedule.input_delay_per_track,
            );
        } else {
            let worker_sync = worker_syncs_g.first();
            #[allow(clippy::needless_range_loop)]
            for track_idx in 0..n_tracks {
                let song_track = &song.tracks[track_idx];
                let track_id = song_track.id;
                let vocal = vocal_store_g.get(&track_id);
                let input_delay = schedule
                    .input_delay_per_track
                    .get(track_idx)
                    .copied()
                    .unwrap_or(0);
                process_track_owned(
                    track_idx as u32,
                    song_track,
                    &mut scratch[track_idx],
                    &plugin_refs_g,
                    &slot_map_g,
                    vocal,
                    Some(audio_renderer),
                    worker_sync,
                    sample_rate,
                    playhead,
                    frames_u32,
                    true,
                    Some(song),
                    any_solo,
                    input_delay,
                );
            }
        }

        // Apply the routing schedule: ProcessTrack ops are no-ops here
        // (already done by `dispatch_and_wait` / `process_track_owned`
        // above). The remaining ops (`Mix` → master, `ApplyDelay` PDC,
        // `ProcessGroupFx` group bus FX) are what differentiates this
        // from a flat sum.
        execute_schedule_post_dispatch(
            &mut schedule,
            &mut scratch[..MAX_TRACKS],
            &mut master_l[..frames],
            &mut master_r[..frames],
            frames,
            song,
            &plugin_refs_g,
            &slot_map_g,
            worker_syncs_g.first(),
            sample_rate,
            frames as u32,
            true,
            any_solo,
        );

        // Write interleaved stereo + track block peak for tail
        // detection.
        let mut block_peak: f32 = 0.0;
        for i in 0..frames {
            let l = master_l[i];
            let r = master_r[i];
            writer
                .write_sample(l)
                .context("failed to write WAV sample (left)")?;
            writer
                .write_sample(r)
                .context("failed to write WAV sample (right)")?;
            block_peak = block_peak.max(l.abs()).max(r.abs());
        }

        playhead += frames as u64;

        // Tail-silence detection only kicks in once we're past the
        // declared song body; the body itself may legitimately be
        // silent (intro, etc.) and we shouldn't truncate it.
        if playhead >= song_length_samples {
            if block_peak < silence_thresh {
                silence_counter += frames as u64;
                if silence_counter >= silence_cutoff_samples {
                    tracing::info!(
                        playhead,
                        song_length_samples,
                        "tail silence detected, stopping export early"
                    );
                    break;
                }
            } else {
                silence_counter = 0;
            }
        }
    }

    Ok(())
}
