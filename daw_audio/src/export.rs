//! Offline WAV render. Drives the same `AudioWorkerPool` and per-plugin
//! `ProcessData` shmem the live audio thread uses, but freewheels through
//! the song as fast as the plugin chain allows. The CPAL callback
//! cooperates by checking `EngineShared::export_running` and writing
//! silence while we hold the resources (see [`engine::LocalState::process_buffer`]).
//!
//! Threading model: the daw_audio receive loop reserves
//! `EngineShared::export_running` (compare_exchange, to mute the CPAL
//! callback and serialize against a second export), resets `export_cancel`,
//! then spawns `run_export` on a dedicated `std::thread`. The export thread:
//!
//! 1. Allocates its own `scratch` / `master_l` / `master_r` (heap is fine
//!    here; this thread is RT-irrelevant once the realtime callback is
//!    parked).
//! 2. Walks the song from frame 0 to `length_beats * samples_per_beat`,
//!    calling `pool.dispatch_and_wait` (or the serial fallback) every
//!    buffer and writing the master bus into `hound::WavWriter`.
//!
//! The spawn closure (not `run_export`) releases `export_running` after this
//! returns, on every path, so live playback can resume.
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

/// Outcome of [`run_export`]. `cancelled` distinguishes a user abort
/// (`MainToChild::CancelExport`) from success / error so the host can
/// branch on a typed flag instead of matching an error string.
pub struct ExportOutcome {
    /// Frames written to the WAV (0 when cancelled — the partial file is
    /// deleted).
    pub frames: u64,
    /// `true` if the render was aborted via `EngineShared::export_cancel`.
    pub cancelled: bool,
}

/// How the offline render walks the song relative to the window it writes.
/// The render always *writes* only `[write_start, write_end)` (plus tail),
/// but where it starts *walking* differs by intent.
#[derive(Debug, Clone, Copy)]
pub enum RenderSpan {
    /// Full-song export: write (and walk) `0..song_length`. (`ExportWav`
    /// with `range = None`.)
    Full,
    /// User export range: write `[start, end)` and walk **from
    /// `start`** (cold). Audio whose note began before `start` (e.g. a
    /// VOICEVOX phrase, a held note) is therefore *not* retriggered — the
    /// result matches pressing Play at `start`. Plugin tails start dry.
    RangeCold { start: u64, end: u64 },
    /// Clip-FX bounce: write `[start, end)` but walk **from frame 0** (warm)
    /// so plugin state at `start` is fully accumulated (reverb tails /
    /// parameter ramps / sidechain history). (`BounceClipFxOnline`.)
    RangeWarm { start: u64, end: u64 },
}

/// Run the offline WAV export to completion. Blocks the caller until the
/// file is finalised. RT-irrelevant — the CPAL callback writes silence
/// while `engine_shared.export_running` is set.
///
/// `span` selects the written window and where the walk starts (see
/// [`RenderSpan`]): `Full` (whole song), `RangeCold` (user export range —
/// walk from the range start), or `RangeWarm` (clip bounce — walk from 0 to
/// warm plugin state). `write_mod_sidecar` persists the modulation-envelope
/// sidecar (`.modenv`) next to the WAV; only the offline video render reads
/// it, so standalone WAV exports / clip bounces pass `false`.
///
/// Returns the number of frames written to the WAV (= can be less than
/// the requested range if tail silence is detected and the render
/// stopped early).
///
/// `on_progress(done, total)` is called every render buffer with the
/// song-body samples rendered so far (`done`, capped at the body length)
/// and the body length in samples (`total`). The caller is expected to
/// throttle the actual IPC send. Standalone WAV export passes a sender
/// that emits `ChildToMain::ExportWavProgress`; the clip-range bounce
/// passes a no-op (no progress overlay).
///
/// Cancellation: if `EngineShared::export_cancel` is raised mid-render
/// (via `MainToChild::CancelExport`), the loop breaks, the partial WAV is
/// deleted, and the function returns `Ok(ExportOutcome { cancelled: true, .. })`
/// (a cancel is not an error).
#[allow(clippy::too_many_arguments)]
pub fn run_export(
    path: PathBuf,
    engine_shared: Arc<EngineShared>,
    song: Song,
    sample_rate: u32,
    max_frames: usize,
    span: RenderSpan,
    write_mod_sidecar: bool,
    on_progress: impl FnMut(u64, u64),
) -> Result<ExportOutcome> {
    if song.bpm <= 0.0 {
        anyhow::bail!("song.bpm must be positive (got {})", song.bpm);
    }
    // docs/plan_modulation.md §7: derive the modulation sidecar path up front,
    // before `path` is moved into the WAV writer below.
    let sidecar_path = common::mod_sidecar::ModEnvSidecar::sidecar_path(&path);
    let n_tracks = song.tracks.len().min(MAX_TRACKS);
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let song_length_samples = (song.length_beats * samples_per_beat).max(0.0) as u64;
    let tail_max_samples = u64::from(sample_rate) * TAIL_MAX_SECONDS;

    // `write_*` = the window written to the WAV; `walk_start` = where the
    // render starts processing. Cold range starts the walk at `write_start`
    // (no pre-range retrigger); warm range / full start at 0.
    let (write_start, write_end, walk_start) = match span {
        RenderSpan::Full => (0, song_length_samples, 0),
        RenderSpan::RangeCold { start, end } => (start, end, start),
        RenderSpan::RangeWarm { start, end } => (start, end, 0),
    };
    if write_end < write_start {
        anyhow::bail!(
            "invalid export range: end_frame ({write_end}) < start_frame ({write_start})"
        );
    }
    // walk past write_end by tail_max so plugin release tails / verbs can
    // decay.
    let total_samples = write_end.saturating_add(tail_max_samples);

    tracing::info!(
        path = %path.display(),
        sample_rate,
        n_tracks,
        song_length_samples,
        write_start,
        write_end,
        walk_start,
        write_mod_sidecar,
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

    // NB: `export_running` (the CPAL-silence flag) and `export_cancel` are
    // both owned by the daw_audio receive loop, not by this function. The recv
    // loop reserves `export_running` with a compare_exchange and resets
    // `export_cancel` *before* spawning this thread (so both are FIFO-ordered
    // against a later `CancelExport`), and the spawn closure releases
    // `export_running` after this returns (on every path, including an early
    // bail above). We only read `export_cancel` here.
    //
    // `export_running` is already set (by the recv loop), so wait for
    // the live CPAL callback to actually park before we touch the shared
    // plugin-host worker slots. Otherwise a CPAL buffer that was mid-
    // `process_buffer` when the flag flipped would dispatch to the same worker
    // slots concurrently with our render ("plugin processing collides"). Once
    // `live_parked` is observed `true`, the live callback has gone through the
    // gate and any in-flight buffer has fully drained (CPAL calls serially).
    let park_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !engine_shared.live_parked.load(Ordering::Acquire) {
        if std::time::Instant::now() >= park_deadline {
            tracing::warn!("live callback did not report parked within 2s; proceeding anyway");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    // Plugins are reinitialised (deactivate→activate) by the GUI's
    // `begin_wav_export` → `ReinitAllPlugins` handshake *before* this
    // render runs, so a cold range / full export starts from a clean state (no
    // live reverb tail / VOICEVOX phrase / synth voice bleeding into the head).
    let render_result = render_loop(
        &engine_shared,
        &song,
        n_tracks,
        sample_rate,
        max_frames,
        total_samples,
        write_start,
        write_end,
        walk_start,
        write_mod_sidecar,
        &mut scratch,
        &mut master_l,
        &mut master_r,
        &mut writer,
        on_progress,
    );

    let (frames_written, env_sidecar, cancelled) = render_result?;

    // User aborted mid-render: discard the partial WAV (don't finalize —
    // an un-finalized hound header would leave a corrupt file) and report
    // the cancel via the typed `cancelled` flag (not an error). The
    // modulation sidecar was never written on this path.
    if cancelled {
        drop(writer);
        let _ = std::fs::remove_file(&path);
        tracing::info!(path = %path.display(), frames_written, "offline WAV export cancelled");
        return Ok(ExportOutcome { frames: 0, cancelled: true });
    }

    // docs/plan_modulation.md §7: persist the modulation envelope sidecar next
    // to the WAV (skip when there were no sources). Best-effort — a sidecar
    // write failure must not fail the audio export; the video render falls back
    // to no modulation (curve/base only).
    if !env_sidecar.is_empty()
        && let Err(e) = env_sidecar.write(&sidecar_path)
    {
        tracing::warn!(
            error = %e,
            path = %sidecar_path.display(),
            "failed to write modulation env sidecar"
        );
    }

    writer
        .finalize()
        .with_context(|| format!("failed to finalize WAV {}", path.display()))?;

    tracing::info!(
        path = %path.display(),
        frames_written,
        "offline WAV export finished"
    );
    Ok(ExportOutcome { frames: frames_written, cancelled: false })
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
    walk_start: u64,
    write_mod_sidecar: bool,
    scratch: &mut [TrackScratch],
    master_l: &mut [f32],
    master_r: &mut [f32],
    writer: &mut WavWriter<std::io::BufWriter<std::fs::File>>,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<(u64, common::mod_sidecar::ModEnvSidecar, bool)> {
    // Tail-silence cutoff: stop early if the master bus stays under
    // -60 dB for half a second once we're past the song body.
    let silence_thresh: f32 = 0.001;
    let silence_cutoff_samples = u64::from(sample_rate) / 2;
    let mut silence_counter: u64 = 0;

    let any_solo = song.tracks.iter().any(|t| t.solo);

    // Ensure every audio source is decoded before the offline walk. The
    // background decode worker (r.md #7) may not have finished, and the
    // per-buffer `audio_clip_renderer.load()` below would otherwise render an
    // undecoded source as silence. Export / bounce is freewheel, so a
    // synchronous full compile here is appropriate; it reuses already-decoded
    // buffers and publishes the full renderer for the live load to pick up.
    {
        let prev = engine_shared.audio_clip_renderer.load();
        let prev_ref: &crate::audio_clip_renderer::AudioClipRenderer = &prev;
        if crate::audio_clip_renderer::has_undecoded_sources(song, prev_ref) {
            let project_dir = engine_shared
                .project_dir
                .load()
                .as_ref()
                .map(|a| (**a).clone());
            let full = crate::audio_clip_renderer::compile_audio_schedule(
                song,
                Some(prev_ref),
                project_dir.as_deref(),
                sample_rate,
                true,
            );
            engine_shared.audio_clip_renderer.store(Arc::new(full));
        }
    }

    // Compile the routing schedule once for the whole render — same
    // structure as the live audio thread's `cached_schedule`. PDC
    // compensation (`ApplyDelay`), group buses (`Mix → TrackScratch`)
    // and SidechainTap all live in here; without using it the export
    // would silently bypass PR3 PDC and mis-render group hierarchies.
    let mut schedule = compile_schedule(song)
        .map_err(|e| anyhow::anyhow!("export schedule compile failed: {e:?}"))?;

    // docs/plan_modulation.md §7: bake each `ModSource`'s follower envelope per
    // render buffer (keyed by beat) so the offline video render reproduces the
    // live preview's modulation. Written to a sidecar next to the WAV — but
    // only when a video render will consume it (`write_mod_sidecar`). A
    // standalone WAV export skips it (n_sources = 0 → no recording, no file):
    // the modulation is already baked into the rendered audio below regardless.
    let mut env_sidecar = common::mod_sidecar::ModEnvSidecar::new(if write_mod_sidecar {
        schedule.follower_slots.len()
    } else {
        0
    });
    // docs/plan_modulation.md §5: reusable per-buffer follower scalar snapshot
    // (prev buffer's env) for audio-param modulation, mirroring the live engine.
    let mut mod_scalars_snapshot: Vec<f32> = Vec::with_capacity(schedule.follower_slots.len());

    // Frame counter for the WAV output. The walk starts at `walk_start`
    // (= 0 for full / warm bounce so plugin state at `write_start` is built
    // up; = `write_start` for a cold range so nothing before it is
    // retriggered). Samples before `write_start` are rendered but not written.
    let mut frames_written: u64 = 0;
    let mut playhead: u64 = walk_start;
    while playhead < total_samples {
        // User abort (`MainToChild::CancelExport`). Checked before any
        // work this buffer so the render stops promptly; `run_export`
        // discards the partial WAV on the `cancelled = true` return.
        if engine_shared.export_cancel.load(Ordering::Acquire) {
            return Ok((frames_written, env_sidecar, true));
        }
        let remaining = total_samples - playhead;
        let frames = (remaining as usize).min(max_frames);
        let frames_u32 = frames as u32;

        master_l[..frames].fill(0.0);
        master_r[..frames].fill(0.0);

        // Snapshot the same wait-free state the audio thread reads.
        let plugin_refs_g = engine_shared.plugin_refs.load();
        let slot_map_g = engine_shared.slot_to_plugin_id.load();
        let worker_syncs_g = engine_shared.worker_syncs.load();
        let pool_g = engine_shared.worker_pool.load();
        let audio_renderer_g = engine_shared.audio_clip_renderer.load();
        let audio_renderer: &crate::audio_clip_renderer::AudioClipRenderer =
            &audio_renderer_g;

        // Phase 4 Step C-2: offline export 中は recording lane なし
        // (= GUI が active gesture を持たない、 transport が freewheel)。
        // empty set を渡して bypass disabled に統一。
        let empty_recording_lanes: std::collections::HashSet<
            (u32, common::model::AutomationTarget),
        > = std::collections::HashSet::new();

        // Phase 5 follow-up (MIDI tempo follow): offline export は constant
        // song.bpm で freewheel するので、 playhead_beats を sample-domain
        // から linear 換算で求める (= playhead * bpm / (60 * SR))。 SongTempo
        // lane を offline export で評価して time-stretch するのは別 phase
        // のスコープ。
        let playhead_beats = playhead as f64 * song.bpm as f64
            / (60.0 * sample_rate as f64);

        // Phase 5 follow-up (granular DSP click 抑制) / r.md #6: offline export は
        // constant song.bpm で freewheel するので smoothed_current_bpm = song.bpm
        // (= LP smoothing 不要、 過渡応答も click 源も無い)。 render 側 Stretch は
        // tempo_follow_ratio(stretch_ratio, song.bpm, nominal_bpm) で source 進度を
        // 出すので、 import 後に bpm を変えてから export すると (= nominal != song.bpm)
        // 追従して伸縮した結果が WAV に焼かれる (= 旧実装は 1.0 固定で追従しなかった)。
        let smoothed_current_bpm_freewheel = f64::from(song.bpm);

        // docs/plan_modulation.md §5: snapshot the prev buffer's follower envs
        // (slot order) so audio-param modulation renders into the WAV too.
        // follower は env、 generator は song 位置から直接算出 (live と同経路)。
        let export_song_secs = playhead as f64 / sample_rate as f64;
        mod_scalars_snapshot.clear();
        for (fs, kind) in schedule
            .follower_slots
            .iter()
            .zip(schedule.mod_kinds.iter())
        {
            let v = common::modulators::generator_scalar(kind, playhead_beats, export_song_secs)
                .unwrap_or(fs.env);
            mod_scalars_snapshot.push(v);
        }

        if let Some(pool) = pool_g.as_deref() {
            pool.dispatch_and_wait(
                Some(song),
                &mut scratch[..n_tracks],
                &plugin_refs_g,
                &slot_map_g,
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
                &empty_recording_lanes,
                song.bpm,
                playhead_beats,
                smoothed_current_bpm_freewheel,
                // export (freewheel render) は loop しない。
                false,
                &mod_scalars_snapshot,
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
                    Some(audio_renderer),
                    worker_sync,
                    sample_rate,
                    playhead,
                    frames_u32,
                    true,
                    Some(song),
                    any_solo,
                    input_delay,
                    &empty_recording_lanes,
                    song.bpm,
                    playhead_beats,
                    smoothed_current_bpm_freewheel,
                    // export (freewheel render) は loop しない。
                    false,
                    &mod_scalars_snapshot,
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
            playhead,
            &empty_recording_lanes,
            song.bpm,
            playhead_beats,
            // export (freewheel render) は loop しない。
            false,
        );

        // docs/plan_modulation.md §7: record this buffer's follower envelopes
        // (block-rate `env`, same value the live engine publishes to
        // `mod_scalars`) keyed by the block beat.
        if env_sidecar.n_sources > 0 {
            env_sidecar.beats.push(playhead_beats as f32);
            // follower は env、 generator は song 位置から算出して焼き込む
            // (render_video は sidecar を sample するだけで全種別を再現)。
            for (fs, kind) in schedule
                .follower_slots
                .iter()
                .zip(schedule.mod_kinds.iter())
            {
                let v = common::modulators::generator_scalar(kind, playhead_beats, export_song_secs)
                    .unwrap_or(fs.env);
                env_sidecar.scalars.push(v);
            }
        }

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

        // Report song-body render progress to the host (the caller's
        // sender throttles the actual IPC send). `done` caps at
        // `write_end` so the bar reaches 100 % at the song-body end; the
        // tail-silence walk past `write_end` holds it there until the
        // export finalises.
        on_progress(playhead.min(write_end), write_end);

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

    // docs/plan_modulation.md §7: hand the baked envelope sidecar back to
    // `run_export`, which owns the WAV path and persists it next to the WAV.
    // `false` = ran to completion (not cancelled).
    Ok((frames_written, env_sidecar, false))
}
