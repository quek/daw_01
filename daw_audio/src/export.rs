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
///
/// `range`:
/// - `None` — full song export (= `MainToChild::ExportWav`). Walks
///   `0..(song.length_beats × samples_per_beat) + tail_max` and writes
///   every frame to the WAV.
/// - `Some((start_frame, end_frame))` — clip-range bounce
///   (`MainToChild::BounceClipFxOnline`). Walks the song from frame 0
///   so plugin state at `start_frame` is fully accumulated (= reverb
///   tails / parameter ramps / sidechain history are correct), but
///   writes only frames in `[start_frame, end_frame)` plus tail
///   silence past `end_frame`. Returns the frame count written.
///
/// Returns the number of frames written to the WAV (= can be less than
/// the requested range if tail silence is detected and the render
/// stopped early).
pub fn run_export(
    path: PathBuf,
    engine_shared: Arc<EngineShared>,
    song: Song,
    sample_rate: u32,
    max_frames: usize,
    range: Option<(u64, u64)>,
) -> Result<u64> {
    if song.bpm <= 0.0 {
        anyhow::bail!("song.bpm must be positive (got {})", song.bpm);
    }
    let n_tracks = song.tracks.len().min(MAX_TRACKS);
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let song_length_samples = (song.length_beats * samples_per_beat).max(0.0) as u64;
    let tail_max_samples = u64::from(sample_rate) * TAIL_MAX_SECONDS;

    let (write_start, write_end) = range.unwrap_or((0, song_length_samples));
    if write_end < write_start {
        anyhow::bail!(
            "invalid bounce range: end_frame ({write_end}) < start_frame ({write_start})"
        );
    }
    // walk past write_end by tail_max so plugin release tails / verbs
    // can decay; walk start is always frame 0 to keep plugin state
    // consistent at the requested start_frame.
    let total_samples = write_end + tail_max_samples;

    tracing::info!(
        path = %path.display(),
        sample_rate,
        n_tracks,
        song_length_samples,
        write_start,
        write_end,
        is_clip_range = range.is_some(),
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
        write_start,
        write_end,
        &mut scratch,
        &mut master_l,
        &mut master_r,
        &mut writer,
    );

    // Always clear the flag, even on render error, so live playback
    // never gets wedged.
    engine_shared.export_running.store(false, Ordering::Release);

    let frames_written = render_result?;

    writer
        .finalize()
        .with_context(|| format!("failed to finalize WAV {}", path.display()))?;

    tracing::info!(
        path = %path.display(),
        frames_written,
        "offline WAV export finished"
    );
    Ok(frames_written)
}

#[allow(clippy::too_many_arguments)]
fn render_loop(
    engine_shared: &EngineShared,
    song: &Song,
    n_tracks: usize,
    sample_rate: u32,
    max_frames: usize,
    total_samples: u64,
    write_start: u64,
    write_end: u64,
    scratch: &mut [TrackScratch],
    master_l: &mut [f32],
    master_r: &mut [f32],
    writer: &mut WavWriter<std::io::BufWriter<std::fs::File>>,
) -> Result<u64> {
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

    // Frame counter for the WAV output. Walking the song always starts
    // at frame 0 so plugin state at `write_start` is properly built up,
    // but we don't write samples to the WAV before reaching `write_start`.
    let mut frames_written: u64 = 0;
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
        let generated_audio_g = engine_shared.generated_audio_store.load();
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
                &generated_audio_g,
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
                    &generated_audio_g,
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

        // Compute block peak across the full block (for tail-silence
        // detection past write_end).
        let block_start = playhead;
        let block_end = playhead + frames as u64;
        let mut block_peak: f32 = 0.0;
        for i in 0..frames {
            let l = master_l[i];
            let r = master_r[i];
            block_peak = block_peak.max(l.abs()).max(r.abs());
        }

        // Write only frames in [write_start, ∞). When the block
        // straddles write_start (e.g. write_start = 12000, block =
        // [10000, 11500) → none written; block = [10000, 13000) →
        // skip 0..2000, write 2000..3000), the suffix is written.
        // Before write_start, the entire block is rendered (= plugin
        // state advances) but skipped from the WAV output.
        if block_end > write_start {
            let local_start = (write_start.saturating_sub(block_start)) as usize;
            for i in local_start..frames {
                let l = master_l[i];
                let r = master_r[i];
                writer
                    .write_sample(l)
                    .context("failed to write WAV sample (left)")?;
                writer
                    .write_sample(r)
                    .context("failed to write WAV sample (right)")?;
                frames_written += 1;
            }
        }

        playhead += frames as u64;

        // Tail-silence detection only kicks in once we're past
        // `write_end` (= the declared song / clip body). The body
        // itself may legitimately be silent (intro, gap between
        // events) and we shouldn't truncate it.
        if playhead >= write_end {
            if block_peak < silence_thresh {
                silence_counter += frames as u64;
                if silence_counter >= silence_cutoff_samples {
                    tracing::info!(
                        playhead,
                        write_end,
                        frames_written,
                        "tail silence detected, stopping export early"
                    );
                    break;
                }
            } else {
                silence_counter = 0;
            }
        }
    }

    Ok(frames_written)
}
