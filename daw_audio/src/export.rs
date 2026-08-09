//! Offline WAV render. Drives the same worker rig and per-plugin
//! `ProcessData` shmem the live audio thread uses, but freewheels through
//! the song as fast as the plugin chain allows. The CPAL callback
//! cooperates by checking `EngineShared::export_running` and writing
//! silence while we hold the resources (see [`crate::engine::LocalState::process_buffer`]).
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
//!    calling the live/export 共通の [`render_master_buffer`] every buffer
//!    and writing the master bus into `hound::WavWriter`. master fx chain と
//!    master gain もその中で適用される (§5 — 旧実装は export だけ両方を
//!    素通りしていて、master に挿した limiter / master volume が WAV に
//!    乗らなかった)。
//!
//! The spawn closure (not `run_export`) releases `export_running` after this
//! returns, on every path, so live playback can resume.
//!
//! `clap_plugin_render.set(CLAP_RENDER_OFFLINE)` is bookended by the GUI
//! around this call (it sends `PluginCommand::SetRenderMode(Offline)` to
//! the plugin host before triggering the export, and `Realtime` after).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use common::model::Song;
use hound::{SampleFormat, WavSpec, WavWriter};

use crate::engine::{EngineShared, MAX_TRACKS};
use crate::graph::{compile_schedule, render_master_buffer};
use crate::mixer::TrackScratch;

/// Hard ceiling on the rendered tail (10 s past `length_beats`). Stops
/// runaway plugins (long reverbs / oscillators with no auto-stop) from
/// inflating the WAV indefinitely.
const TAIL_MAX_SECONDS: u64 = 10;

/// Outcome of [`run_export`]. `cancelled` distinguishes a user abort
/// (`AudioCommand::CancelExport`) from success / error so the host can
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
/// that emits `AudioEvent::ExportWavProgress`; the clip-range bounce
/// passes a no-op (no progress overlay).
///
/// Cancellation: if `EngineShared::export_cancel` is raised mid-render
/// (via `AudioCommand::CancelExport`), the loop breaks, the partial WAV is
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
    // A2 (r.md #8): tempo automation を持つ曲も再生と同じ尺で焼くため、 song body の
    // sample 長は SongTempo カーブを積分して求める (constant-bpm なら従来の線形
    // `length_beats * 60*SR/bpm` と一致)。
    let song_length_samples =
        common::automation::beats_to_samples(&song, sample_rate, song.length_beats);
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

    // Ensure every audio source is decoded before the offline walk. The
    // background decode worker (r.md #7) may not have finished, and the
    // per-buffer `audio_clip_renderer.load()` below would otherwise render an
    // undecoded source as silence. Export / bounce is freewheel, so a
    // synchronous full compile here is appropriate; it reuses already-decoded
    // buffers and publishes the full renderer for the live load to pick up.
    {
        let prev = engine_shared.audio_clip_renderer.load();
        let prev_ref: &crate::audio_clip_renderer::AudioClipRenderer = &prev;
        let project_dir = engine_shared
            .project_dir
            .load()
            .as_ref()
            .map(|a| (**a).clone());
        if crate::audio_clip_renderer::has_undecoded_sources(
            song,
            prev_ref,
            project_dir.as_deref(),
        ) {
            // publish の口は `publish_audio_clip_schedule` 一本 (SSoT)。生 store
            // だと generation guard を迂回し、export 中に届いた新 song 用の
            // renderer を古いもので上書きしてしまう (かつ
            // `last_published_generation` も進まないので stale が live に残る)。
            let generation = engine_shared
                .schedule_generation
                .load(std::sync::atomic::Ordering::Acquire);
            let full = crate::audio_clip_renderer::compile_audio_schedule(
                song,
                Some(prev_ref),
                project_dir.as_deref(),
                sample_rate,
                true,
            );
            crate::publish_audio_clip_schedule(engine_shared, generation, full, sample_rate);
        }
    }

    // r.md #40: live 側は off-thread pool を RT へ配送するが、この walk 自体が
    // off-RT なので自前の `TrackScratch` に直接エンジンを積む (= live と同じ
    // `render_audio_events` を通す = 不変条件 #6)。 足りないと Stretch clip が
    // degrade 経路に落ちて書き出しだけ音が変わるので、ここで必ず揃える。
    {
        let renderer_g = engine_shared.audio_clip_renderer.load();
        for (track_idx, &needed) in renderer_g.engines_per_track.iter().enumerate() {
            let Some(ts) = scratch.get_mut(track_idx) else {
                break;
            };
            while ts.stretch_engines.len() < usize::from(needed)
                && ts.stretch_engines.len() < ts.stretch_engines.capacity()
            {
                let Some(engine) = crate::stretch_engine::StretchEngine::new(sample_rate) else {
                    tracing::error!(track_idx, "export: stretch engine の確保に失敗 (OOM?)");
                    break;
                };
                ts.stretch_engines.push(engine);
            }
        }
    }

    // Compile the routing schedule once for the whole render — same
    // structure as the live audio thread's cached schedule. PDC compensation
    // (`ApplyDelay`), group buses, SidechainTap and master fx all flow from
    // it via `render_master_buffer`. `buffer_frames` = このループの処理単位
    // `max_frames` (leaf 宛 sidechain tap の 1-buffer 補償量、 live と同規則)。
    let mut schedule = compile_schedule(song, sample_rate, max_frames as u32)
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

    // Phase 4 Step C-2: offline export 中は recording lane なし
    // (= GUI が active gesture を持たない、 transport が freewheel)。
    let empty_recording_lanes: std::collections::HashSet<
        (u32, common::model::AutomationTarget),
    > = std::collections::HashSet::new();

    // Frame counter for the WAV output. The walk starts at `walk_start`
    // (= 0 for full / warm bounce so plugin state at `write_start` is built
    // up; = `write_start` for a cold range so nothing before it is
    // retriggered). Samples before `write_start` are rendered but not written.
    let mut frames_written: u64 = 0;
    let mut playhead: u64 = walk_start;
    // A2 (r.md #8): beat 累算器。 live engine と同じく buffer 毎に current_bpm で
    // integrate して進める (= sample↔beat 対応が tempo automation に追従)。 曲中から
    // 始まる range 書き出しは walk_start に対応する beat で seed する。
    let mut playhead_beats = common::automation::samples_to_beats(song, sample_rate, walk_start);
    while playhead < total_samples {
        // User abort (`AudioCommand::CancelExport`). Checked before any
        // work this buffer so the render stops promptly; `run_export`
        // discards the partial WAV on the `cancelled = true` return.
        if engine_shared.export_cancel.load(Ordering::Acquire) {
            return Ok((frames_written, env_sidecar, true));
        }
        let remaining = total_samples - playhead;
        let frames = (remaining as usize).min(max_frames);

        // Snapshot the same wait-free state the notify thread sees (mirrors —
        // this thread is off-RT, so ArcSwap loads are fine here).
        let plugin_refs_g = engine_shared.plugin_refs.load();
        let worker_g = engine_shared.worker.load_full();
        let audio_renderer_g = engine_shared.audio_clip_renderer.load();
        let audio_renderer: &crate::audio_clip_renderer::AudioClipRenderer =
            &audio_renderer_g;

        // A2 (r.md #8): 当該 buffer の effective tempo を SongTempo カーブから取り、
        // live 再生と同じ sample↔beat 対応にする。 B11 (r.md #8): export も再生と
        // 同じく song-level tempo modulation を base tempo に焼く。
        // `mod_scalars_snapshot` は前 iteration 値 (engine と同じ 1-buffer lag)。
        let base_bpm_freewheel =
            f64::from(common::automation::evaluate_song_tempo(song, playhead_beats));
        let smoothed_current_bpm_freewheel = common::automation::apply_modulation_with_scalars(
            song,
            &common::model::AutomationTarget::SongTempo,
            base_bpm_freewheel,
            &song.song_mod_routings,
            &mod_scalars_snapshot,
        );

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

        // live と同一の単一 render 経路 (§5): dispatch → schedule → master fx
        // → master gain。 export (freewheel render) は loop しない。
        let master_gain = f32::from_bits(engine_shared.master_gain.load(Ordering::Relaxed));
        render_master_buffer(
            song,
            &mut schedule,
            scratch,
            &plugin_refs_g,
            worker_g.as_deref(),
            audio_renderer,
            &mut master_l[..frames],
            &mut master_r[..frames],
            sample_rate,
            frames as u32,
            true,
            false,
            &empty_recording_lanes,
            // live 側 (engine.rs `current_bpm`) と同じ effective tempo を渡す。
            // base `song.bpm` を渡すと、 event 収集窓 (frames×bpm/(60·SR)) と
            // ループ末尾の beat 累算 (effective tempo) が乖離し、 tempo
            // automation 中の書き出しでノートが欠落 / 二重発音する。
            smoothed_current_bpm_freewheel as f32,
            playhead_beats,
            &mod_scalars_snapshot,
            master_gain,
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
        // A2 (r.md #8): beat 累算器を当該 buffer の tempo で進める (live engine の
        // buffer 末 `playhead_beats += n*bpm/(60*SR)` と同一式)。
        if sample_rate > 0 {
            playhead_beats +=
                frames as f64 * smoothed_current_bpm_freewheel / (60.0 * f64::from(sample_rate));
        }

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
