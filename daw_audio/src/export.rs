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
use common::loudness_report::{LoudnessCollector, LoudnessReport};
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
///
/// 範囲は **拍** で受ける (r.md #54)。拍→サンプル換算は
/// [`common::automation::beats_to_samples`] = tempo automation を積分する
/// SSoT 一本に統一してあり、GUI 側は換算しない。定数 BPM で換算したフレームを
/// 送っていた旧形は、テンポカーブのある曲で「指定した小節と実際に走査する位置が
/// ずれる」ので撤去した。
#[derive(Debug, Clone, Copy)]
pub enum RenderSpan {
    /// Full-song export: write (and walk) `0..song_length`. (`ExportWav`
    /// with `range = None`.)
    Full,
    /// User export range: write `[start, end)` and walk **from
    /// `start`** (cold). Audio whose note began before `start` (e.g. a
    /// VOICEVOX phrase, a held note) is therefore *not* retriggered — the
    /// result matches pressing Play at `start`. Plugin tails start dry.
    RangeCold { start_beat: f64, end_beat: f64 },
    /// Clip-FX bounce: write `[start, end)` but walk **from frame 0** (warm)
    /// so plugin state at `start` is fully accumulated (reverb tails /
    /// parameter ramps / sidechain history). (`BounceClipFxOnline`.)
    RangeWarm { start_beat: f64, end_beat: f64 },
}

/// レンダされた 1 ブロックの受け取り先。
///
/// 不変条件 6 (live と export は同じ render 関数) の延長で、**offline 走査も
/// 1 つ** にするための唯一の分岐点。`render_master_buffer` → 窓の切り出し →
/// PDC シフト → tail 判定 → cancel を二重実装せず、「描いた音をどうするか」
/// だけを差し替える。
///
/// 渡るのは書き出し窓 `[write_start, ..)` に入ったフレームだけ。
pub trait RenderSink {
    fn accept(&mut self, l: &[f32], r: &[f32]) -> Result<()>;
}

/// WAV ファイルへ書く sink (従来の書き出し)。
struct WavSink<'a> {
    writer: &'a mut WavWriter<std::io::BufWriter<std::fs::File>>,
}

impl RenderSink for WavSink<'_> {
    fn accept(&mut self, l: &[f32], r: &[f32]) -> Result<()> {
        for (a, b) in l.iter().zip(r.iter()) {
            self.writer
                .write_sample(*a)
                .context("failed to write WAV sample (left)")?;
            self.writer
                .write_sample(*b)
                .context("failed to write WAV sample (right)")?;
        }
        Ok(())
    }
}

/// ラウドネスを積む sink (r.md #54)。進捗の送出もここが持つ — 収集器が
/// 「どこまで測ったか」と「今の値」の両方を知っている唯一の場所なので、
/// 走査ループ側の `on_progress` (フレーム数だけ) とは分けている。
struct LoudnessSink<F: FnMut(LoudnessReport)> {
    collector: LoudnessCollector,
    on_progress: F,
    last_at: std::time::Instant,
}

impl<F: FnMut(LoudnessReport)> RenderSink for LoudnessSink<F> {
    fn accept(&mut self, l: &[f32], r: &[f32]) -> Result<()> {
        self.collector.push(l, r);
        // 走査は実時間より遥かに速いので、進捗は wall-clock で間引く
        // (ExportWavProgress と同じ 250ms)。
        if self.last_at.elapsed() >= std::time::Duration::from_millis(250) {
            self.last_at = std::time::Instant::now();
            (self.on_progress)(self.collector.report(false));
        }
        Ok(())
    }
}

/// [`run_loudness_analysis`] の結果。
pub struct LoudnessOutcome {
    pub report: LoudnessReport,
    pub cancelled: bool,
}

/// Run the offline WAV export to completion. Blocks the caller until the
/// file is finalised. RT-irrelevant — the CPAL callback writes silence
/// while `engine_shared.export_running` is set.
///
/// `span` selects the written window and where the walk starts (see
/// [`RenderSpan`]): `Full` (whole song), `RangeCold` (user export range —
/// walk from the range start), or `RangeWarm` (clip bounce — walk from 0 to
/// warm plugin state). `write_video_sidecars` persists the sidecars the offline
/// video render needs next to the WAV — `.modenv` (modulation envelopes) と
/// `.launcher` (r.md #87: ランチャーの走行状態の遷移列)。動画書き出しだけが
/// 読むので、単体の WAV 書き出し / クリップ bounce は `false` を渡す。
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
    write_video_sidecars: bool,
    on_progress: impl FnMut(u64, u64),
) -> Result<ExportOutcome> {
    let win = RenderWindow::resolve(&song, sample_rate, span, true)?;
    let (write_start, write_end, walk_start, total_samples) =
        (win.write_start, win.write_end, win.walk_start, win.total_samples);
    // docs/plan_modulation.md §7: derive the modulation sidecar path up front,
    // before `path` is moved into the WAV writer below.
    let sidecar_path = common::mod_sidecar::ModEnvSidecar::sidecar_path(&path);
    let launcher_sidecar_path = common::launcher_sidecar::LauncherSidecar::sidecar_path(&path);
    let n_tracks = song.tracks.len().min(MAX_TRACKS);
    let song_length_samples = win.song_length_samples;

    tracing::info!(
        path = %path.display(),
        sample_rate,
        n_tracks,
        song_length_samples,
        write_start,
        write_end,
        walk_start,
        write_video_sidecars,
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

    wait_for_live_park(&engine_shared);

    // Plugins are reinitialised (deactivate→activate) by the GUI's
    // `begin_wav_export` → `ReinitAllPlugins` handshake *before* this
    // render runs, so a cold range / full export starts from a clean state (no
    // live reverb tail / VOICEVOX phrase / synth voice bleeding into the head).
    let render_result = {
        let mut sink = WavSink { writer: &mut writer };
        render_loop(
            &engine_shared,
            &song,
            sample_rate,
            max_frames,
            total_samples,
            write_start,
            write_end,
            walk_start,
            write_video_sidecars,
            &mut sink,
            on_progress,
        )
    };

    let RenderOutcome { frames_written, env_sidecar, launcher_sidecar, cancelled } =
        render_result?;

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

    // r.md #87 §3.6: ランチャーの走行状態も同じ best-effort で隣へ置く。
    // 読めなければ動画側は `Song.launcher` (= 撃った起点) へ倒れるので、
    // 書けなかったことで書き出し自体を失敗させない。
    if !launcher_sidecar.is_empty()
        && let Err(e) = launcher_sidecar.write(&launcher_sidecar_path)
    {
        tracing::warn!(
            error = %e,
            path = %launcher_sidecar_path.display(),
            "failed to write launcher sidecar"
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

/// 走査する窓 (サンプル)。`RenderSpan` (拍) を engine 側 SSoT で解決した結果。
struct RenderWindow {
    /// 曲本体の長さ [samples] (tempo automation を積分した値)。
    song_length_samples: u64,
    /// sink へ渡し始める / 終える位置。
    write_start: u64,
    write_end: u64,
    /// 走査を開始する位置 (cold = write_start、warm/full = 0)。
    walk_start: u64,
    /// 走査の終端 (= write_end + tail、tail 無しなら write_end)。
    total_samples: u64,
}

impl RenderWindow {
    /// 拍 → サンプル換算はここ 1 箇所 (`beats_to_samples` = tempo automation を
    /// 積分する SSoT)。`tail` が false のときは減衰を測らず窓ちょうどで止める
    /// (ラウドネス解析 = 「範囲のラウドネス」であって範囲外の残響ではない)。
    fn resolve(song: &Song, sample_rate: u32, span: RenderSpan, tail: bool) -> Result<Self> {
        if song.bpm <= 0.0 {
            anyhow::bail!("song.bpm must be positive (got {})", song.bpm);
        }
        // A2 (r.md #8): tempo automation を持つ曲も再生と同じ尺で焼くため、 song body の
        // sample 長は SongTempo カーブを積分して求める (constant-bpm なら従来の線形
        // `length_beats * 60*SR/bpm` と一致)。
        let song_length_samples =
            common::automation::beats_to_samples(song, sample_rate, song.length_beats);
        let to_frames = |beat: f64| {
            common::automation::beats_to_samples(song, sample_rate, beat.max(0.0))
        };
        // `write_*` = the window handed to the sink; `walk_start` = where the
        // render starts processing. Cold range starts the walk at `write_start`
        // (no pre-range retrigger); warm range / full start at 0.
        let (write_start, write_end, walk_start) = match span {
            RenderSpan::Full => (0, song_length_samples, 0),
            RenderSpan::RangeCold { start_beat, end_beat } => {
                let s = to_frames(start_beat);
                (s, to_frames(end_beat), s)
            }
            RenderSpan::RangeWarm { start_beat, end_beat } => {
                (to_frames(start_beat), to_frames(end_beat), 0)
            }
        };
        if write_end < write_start {
            anyhow::bail!(
                "invalid render range: end ({write_end}) < start ({write_start}) [samples]"
            );
        }
        // walk past write_end by tail_max so plugin release tails / verbs can
        // decay.
        let total_samples = if tail {
            write_end.saturating_add(u64::from(sample_rate) * TAIL_MAX_SECONDS)
        } else {
            write_end
        };
        Ok(Self { song_length_samples, write_start, write_end, walk_start, total_samples })
    }
}

/// `export_running` は既に recv loop が立てているので、live CPAL コールバックが
/// 実際に park するのを待ってから共有 plugin-host worker slot に触る。待たないと
/// フラグを立てた瞬間に `process_buffer` の途中だったバッファが、この走査と同じ
/// slot へ同時 dispatch する ("plugin processing collides")。`live_parked` が
/// true を返した時点で live 側はゲートを通っており、in-flight のバッファは
/// 完全に抜けている (CPAL は直列に呼ぶ)。
fn wait_for_live_park(engine_shared: &EngineShared) {
    let park_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !engine_shared.live_parked.load(Ordering::Acquire) {
        if std::time::Instant::now() >= park_deadline {
            tracing::warn!("live callback did not report parked within 2s; proceeding anyway");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// r.md #54: 範囲のラウドネスをオフラインで解析する。
///
/// [`run_export`] と**同じ走査** (`render_loop` → `render_master_buffer`) を通り、
/// 出力先だけ WAV 書き込みから [`LoudnessCollector`] へ差し替える。したがって
/// 「解析した値」と「同じ範囲を書き出した WAV の値」は構造的に一致する。
///
/// 減衰 tail は測らない (`RenderWindow::resolve(.., tail = false)`) ので、
/// 走査は範囲末尾ちょうどで止まる。中断は `EngineShared::export_cancel`
/// (= `AudioCommand::CancelExport`) で、書き出しと共通。
pub fn run_loudness_analysis(
    engine_shared: Arc<EngineShared>,
    song: Song,
    sample_rate: u32,
    max_frames: usize,
    span: RenderSpan,
    on_progress: impl FnMut(LoudnessReport),
) -> Result<LoudnessOutcome> {
    let win = RenderWindow::resolve(&song, sample_rate, span, false)?;
    let (range_start_beat, range_end_beat) = match span {
        RenderSpan::Full => (0.0, song.length_beats),
        RenderSpan::RangeCold { start_beat, end_beat }
        | RenderSpan::RangeWarm { start_beat, end_beat } => (start_beat, end_beat),
    };
    tracing::info!(
        sample_rate,
        write_start = win.write_start,
        write_end = win.write_end,
        range_start_beat,
        range_end_beat,
        "starting offline loudness analysis"
    );

    wait_for_live_park(&engine_shared);

    let mut sink = LoudnessSink {
        collector: LoudnessCollector::new(
            sample_rate,
            range_start_beat,
            range_end_beat,
            win.write_start,
            win.write_end.saturating_sub(win.write_start),
            max_frames,
        ),
        on_progress,
        last_at: std::time::Instant::now(),
    };
    let RenderOutcome { cancelled, .. } = render_loop(
        &engine_shared,
        &song,
        sample_rate,
        max_frames,
        win.total_samples,
        win.write_start,
        win.write_end,
        win.walk_start,
        false,
        &mut sink,
        |_, _| {},
    )?;
    let report = sink.collector.report(!cancelled);
    tracing::info!(
        integrated = report.integrated_lufs,
        lra = report.lra_lu,
        true_peak = report.true_peak_dbtp,
        measured_secs = report.measured_secs,
        cancelled,
        "offline loudness analysis finished"
    );
    Ok(LoudnessOutcome { report, cancelled })
}

/// r.md #39: 書き出し窓と走査終端を master 出力の PDC 遅延ぶん **後ろ** へずらす。
///
/// PDC は export でも有効なので (`compile_schedule` は live と共通)、`master_buffer[P]`
/// に載っているのは曲位置 `P - master_latency` の音。素通しで書くと wav 全体が
/// `master_latency` ぶん後ろへずれ、書き出した stem を同じ project の元位置へ貼り戻すと
/// 二重にずれて聞こえる (`daw_gui/tests/pdc_real_vst3.rs` が実 VST3 でこの症状を記述)。
///
/// 3 つ全部をずらすのが要点:
/// - `write_start`: 書き始めを遅らせる = 先頭の遅延ぶんを捨てる → `wav[0]` が曲位置 `write_start`
/// - `write_end`: tail-silence 検出の開始点も曲位置基準に保つ
/// - `total_samples`: 走査を同じだけ延長しないと末尾が `master_latency` ぶん欠ける
///
/// `master_latency == 0` (= PDC 無し) では恒等変換なので、既存挙動は変わらない。
fn shift_window_for_master_latency(
    master_latency: u32,
    write_start: u64,
    write_end: u64,
    total_samples: u64,
) -> (u64, u64, u64) {
    let l = u64::from(master_latency);
    (
        write_start.saturating_add(l),
        write_end.saturating_add(l),
        total_samples.saturating_add(l),
    )
}

/// [`render_loop`] の結果。sidecar が 2 種類あるのでタプルを畳んで名前を付ける。
struct RenderOutcome {
    frames_written: u64,
    env_sidecar: common::mod_sidecar::ModEnvSidecar,
    /// r.md #87: 動画書き出しが差す走行状態の遷移列 (`write_video_sidecars` が
    /// false のときは空)。
    launcher_sidecar: common::launcher_sidecar::LauncherSidecar,
    /// `true` = ユーザーが中断した (`AudioCommand::CancelExport`)。
    cancelled: bool,
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
    write_video_sidecars: bool,
    sink: &mut dyn RenderSink,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<RenderOutcome> {
    // heap 確保はここで一度だけ (この走査は off-RT)。
    let mut scratch: Vec<TrackScratch> = (0..MAX_TRACKS).map(|_| TrackScratch::new()).collect();
    let mut master_l: Vec<f32> = vec![0.0; max_frames];
    let mut master_r: Vec<f32> = vec![0.0; max_frames];
    let scratch = &mut scratch[..];
    let master_l = &mut master_l[..];
    let master_r = &mut master_r[..];
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
    // PDC の入力 (device 単位の報告 latency) は live publish と同じ表を読む
    // (`compile_schedule` は live / export 共通なので入力も共通)。
    let mut schedule = compile_schedule(
        song,
        &engine_shared.device_latencies.load(),
        sample_rate,
        max_frames as u32,
    )
    .map_err(|e| anyhow::anyhow!("export schedule compile failed: {e:?}"))?;

    // r.md #39: PDC 遅延ぶん書き出し窓を後ろへずらす (下の helper に理由を集約)。
    let (write_start, write_end, total_samples) = shift_window_for_master_latency(
        schedule.master_latency_samples,
        write_start,
        write_end,
        total_samples,
    );

    // docs/plan_modulation.md §7: bake each `ModSource`'s follower envelope per
    // render buffer (keyed by beat) so the offline video render reproduces the
    // live preview's modulation. Written to a sidecar next to the WAV — but
    // only when a video render will consume it (`write_video_sidecars`). A
    // standalone WAV export skips it (n_sources = 0 → no recording, no file):
    // the modulation is already baked into the rendered audio below regardless.
    // r.md #87 §3.6: 同じ理由でランチャーの走行状態も焼く — 動画書き出しは
    // フォローアクションがどこで次の列へ移ったかを知らないので、焼かないと
    // 「音は Scene2 へ移ったのに絵は Scene1 を延々ループ」になる。
    let mut launcher_sidecar = crate::launcher::sidecar::SidecarRecorder::new();
    // r.md #89: 変調の制御グリッド。**live engine と同じ 1 本** (`crate::mod_tick`、
    // アーキ不変条件 6) を通すので、刻みの割り方も transport の進め方も
    // buffer 長 (export 1024 固定 / live は device 実測長) に依存しない。
    let mut mod_tick = crate::mod_tick::ModTickRunner::new();
    {
        let plan = std::sync::Arc::new(common::mod_graph::build_plan(song, 1, |beat| {
            common::automation::beats_to_samples(song, sample_rate, beat) as f64
                / f64::from(sample_rate.max(1))
        }));
        let mut rt = common::mod_graph::ModRuntime::default();
        rt.install(&plan);
        let length_secs = common::automation::beats_to_samples(song, sample_rate, song.length_beats)
            as f64
            / f64::from(sample_rate.max(1));
        let table = std::sync::Arc::new(common::mod_graph::ModPhaseTable::build(
            &plan,
            song,
            sample_rate,
            length_secs,
        ));
        let _ = mod_tick.install(plan, rt);
        mod_tick.set_table(Some(table));
    }
    // r.md #89: 列のキーは `ModSource::id`。**値を出す面そのものから取る** —
    // `schedule.follower_keys` は `Song::mod_sources` の位置順、面は plan の
    // トポロジカル順なので、クロス変調で順序が変わると列と値が入れ替わる
    // (書き出した動画で別のソースの波形が絵を動かす)。
    let mut env_sidecar = common::mod_sidecar::ModEnvSidecar::new(if write_video_sidecars {
        mod_tick.publish_plane().ids().to_vec()
    } else {
        Vec::new()
    });
    // `Schedule` の follower slot → 係数表の列 (刻みごとにフォロワー係数を引く)。
    let mut follower_cols: Vec<u16> = Vec::new();
    mod_tick.build_follower_cols(&schedule.follower_keys, &mut follower_cols);
    // plan slot → `Schedule::follower_slots` の index (刻みごとの線形探索を避ける)。
    let mut follower_env_of_slot: Vec<u16> = Vec::new();
    mod_tick.build_follower_env_map(&schedule.follower_keys, &mut follower_env_of_slot);

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
    // r.md #89: 変調の位相と transport を走査開始位置で張る (範囲書き出しは曲頭
    // から始まらない)。live の seek と同じ `locate` を通すので、同じ位置から
    // 再生したときと位相が一致する。
    mod_tick.locate(song, walk_start, playhead_beats, sample_rate);

    // r.md #87 (Q9 / §2.5): 書き出しは **今のランチャーの状態を反映する**。
    // 走査の先頭で `Track.launcher` / `AutomationLane.launcher` を一斉に撃った
    // 状態から始め、以降はフォローアクションが決定論的に進む
    // (乱数は `f(seed, 発火拍)` の純ハッシュ) ので、同じプロジェクトなら
    // 何度書き出しても同じファイルになる。走行位置を `Song` に保存しないのが
    // その前提 (§1.4)。
    let mut launcher = crate::launcher::LauncherRuntime::new();
    launcher.arm_reseed();
    // グローバルローンチ量子化は engine と同じ値を使う (セルの `Global` の解決先)。
    // r.md #87: 生成と同じく `Song` が SSoT (engine.rs の同名変数と同じ理由)。
    // 書き出しは曲頭から freewheel で描き直すので、実行時の速報ではなく
    // 保存された値で決まる必要がある (= 同じプロジェクトなら同じファイル)。
    let global_launch_quantize = song.global_launch_quantize;
    // 走査が `write_end` を越えたか (= 曲 / 範囲の本体を描き終えたか)。
    //
    // ここから先は **減衰だけを録る区間**で、live で言えば「Stop を押した後」に
    // あたる。走査を続けるのはプラグインのリリースを取り込むためであって、
    // 音源を鳴らし続けるためではない。だから live の Stop と同じことをする —
    // 鳴っている note を全部 Off にし、以降の buffer は `playing = false` で描き、
    // ランチャーの走行状態を黙らせる。
    //
    // これをやらないと、`RowPhase::Cell { looping: true }` の行は曲末と無関係に
    // 鳴り続けるので **tail-silence 判定 (0.5 秒以上ピーク < -60dB) が永久に立たず、
    // WAV が必ず 10 秒伸びて末尾にループがそのまま入る**
    // (8 小節の範囲書き出しでも「8 小節 + 10 秒」)。アレンジ側も同じで、範囲
    // 書き出しの tail には範囲の**続きの小節**がそのまま入っていた。
    // sink 側で `write_end` を切ると減衰まで消えるので、「鳴らすのをやめる」側で解く。
    let mut tail_started = false;

    while playhead < total_samples {
        // User abort (`AudioCommand::CancelExport`). Checked before any
        // work this buffer so the render stops promptly; `run_export`
        // discards the partial WAV on the `cancelled = true` return.
        if engine_shared.export_cancel.load(Ordering::Acquire) {
            return Ok(RenderOutcome {
                frames_written,
                env_sidecar,
                launcher_sidecar: launcher_sidecar.finish(),
                cancelled: true,
            });
        }
        let remaining = total_samples - playhead;
        let frames = (remaining as usize).min(max_frames);

        // 本体を描き終えた最初の buffer で 1 度だけ transport を止める (上の doc)。
        if !tail_started && playhead >= write_end {
            tail_started = true;
            crate::mixer::queue_all_notes_off(scratch);
            launcher.silence_all();
        }
        let playing = !tail_started;

        // Snapshot the same wait-free state the notify thread sees (mirrors —
        // this thread is off-RT, so ArcSwap loads are fine here).
        let plugin_refs_g = engine_shared.plugin_refs.load();
        let worker_g = engine_shared.worker.load_full();
        let audio_renderer_g = engine_shared.audio_clip_renderer.load();
        let audio_renderer: &crate::audio_clip_renderer::AudioClipRenderer =
            &audio_renderer_g;

        // r.md #89: 刻みを回して、この buffer の変調値面と transport を解く
        // (live engine と同じ `ModTickRunner`)。テンポも `next_mark` が解くので、
        // ここで `evaluate_song_tempo` を別に呼ぶと live と規則が食い違う。
        let head_mark = {
            let sched = &schedule;
            let env_of = &follower_env_of_slot;
            let follower_env = |plan_slot: u16, tick: i64| {
                match env_of.get(usize::from(plan_slot)).copied() {
                    Some(i) if i != u16::MAX => sched
                        .follower_slots
                        .get(usize::from(i))
                        .map_or(0.0, |f| f.env_at_tick(tick)),
                    _ => 0.0,
                }
            };
            mod_tick.run_buffer(song, playhead, frames as u32, sample_rate, follower_env)
        };
        playhead_beats = head_mark.beat;
        let smoothed_current_bpm_freewheel = head_mark.bpm;

        // live と同一の単一 render 経路 (§5): dispatch → schedule → master fx
        // → master gain。 export (freewheel render) は loop しない。
        let master_gain = f32::from_bits(engine_shared.master_gain.load(Ordering::Relaxed));
        // 行ごとの時間軸を live と同じ解き方で更新する (不変条件 6 — 片方だけ
        // 別経路にすると「聴こえた通りに書き出す」が成立しない)。
        let span = crate::launcher::runtime::BufferSpan::new(
            playhead_beats,
            smoothed_current_bpm_freewheel as f32,
            sample_rate,
            frames as u32,
        );
        launcher.update(song, span, global_launch_quantize, playing);
        if write_video_sidecars {
            launcher_sidecar.record(launcher.rows(), span);
        }
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
            // `write_end` を越えたら false = live の Stop と同じ状態 (上の
            // `tail_started` の doc)。tail は減衰だけを録る。
            playing,
            // export (freewheel render) は loop しない。
            common::model::LoopRegion::default(),
            &empty_recording_lanes,
            // live 側 (engine.rs `current_bpm`) と同じ effective tempo を渡す。
            // base `song.bpm` を渡すと、 event 収集窓 (frames×bpm/(60·SR)) と
            // ループ末尾の beat 累算 (effective tempo) が乖離し、 tempo
            // automation 中の書き出しでノートが欠落 / 二重発音する。
            smoothed_current_bpm_freewheel as f32,
            playhead_beats,
            mod_tick.plane(),
            mod_tick.follower_drive(&follower_cols, playhead),
            launcher.rows(),
            master_gain,
        );

        // docs/plan_modulation.md §7: record this buffer's modulator values
        // (buffer 頭の刻みの値 — live engine が GUI へ publish するのと同じ点)
        // keyed by the block beat。動画は 30〜60fps でサンプルするので、刻み
        // (750Hz) ではなく buffer (≒50〜100Hz) の粒度で足りる。
        if env_sidecar.n_sources() > 0 {
            #[allow(clippy::cast_possible_truncation)]
            env_sidecar.push(playhead_beats as f32, mod_tick.publish_plane().values());
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

        // Hand only frames in [write_start, ∞) to the sink. When the block
        // straddles write_start (e.g. write_start = 12000, block =
        // [10000, 11500) → nothing; block = [10000, 13000) →
        // skip 0..2000, hand 2000..3000), the suffix is handed over.
        // Before write_start, the entire block is rendered (= plugin
        // state advances) but skipped from the output.
        if block_end > write_start {
            let local_start = ((write_start.saturating_sub(block_start)) as usize).min(frames);
            sink.accept(&master_l[local_start..frames], &master_r[local_start..frames])?;
            frames_written += (frames - local_start) as u64;
        }

        playhead += frames as u64;
        // r.md #89: 拍は **刻みが進める** — 次の iteration の頭で
        // `ModTickRunner::run_buffer` が `next_mark` の規則で解いた値を
        // `playhead_beats` に入れる。ここで buffer 単位に足し込むと、
        // live (`engine.rs`) と規則が食い違って書き出しの拍軸だけずれる
        // (アーキ不変条件 6 — 刻みの割り方も進め方も 1 本)。

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
    Ok(RenderOutcome {
        frames_written,
        env_sidecar,
        launcher_sidecar: launcher_sidecar.finish(),
        cancelled: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_master_latency_leaves_the_write_window_untouched() {
        // PDC 無しの曲では恒等変換 (既存挙動の回帰防止)。
        assert_eq!(
            shift_window_for_master_latency(0, 12_000, 96_000, 120_000),
            (12_000, 96_000, 120_000)
        );
    }

    #[test]
    fn master_latency_shifts_write_window_and_extends_the_walk() {
        // r.md #39: 書き始め・tail 判定・走査終端の 3 つを同じだけ後ろへ。
        // 走査を延ばさないと末尾が latency ぶん欠ける。
        let (ws, we, total) = shift_window_for_master_latency(4_096, 0, 96_000, 120_000);
        assert_eq!(ws, 4_096, "wav[0] は master[write_start + L] = 曲位置 write_start");
        assert_eq!(we, 100_096);
        assert_eq!(total, 124_096, "走査を L だけ延長して末尾を欠けさせない");
        // 書き出される長さは latency に依らず「要求された範囲」のまま。
        assert_eq!(total - ws, 120_000);
    }

    /// 120 BPM / 48kHz = 24,000 samples/beat の素の song。
    fn song(length_beats: f64) -> Song {
        Song { bpm: 120.0, length_beats, ..Song::default() }
    }

    #[test]
    fn 全曲は曲末まで書いて減衰ぶん走査を延ばす() {
        let w = RenderWindow::resolve(&song(16.0), 48_000, RenderSpan::Full, true).unwrap();
        assert_eq!((w.write_start, w.write_end, w.walk_start), (0, 384_000, 0));
        // 走査は write_end + TAIL_MAX_SECONDS。
        assert_eq!(w.total_samples, 384_000 + 48_000 * TAIL_MAX_SECONDS);
    }

    #[test]
    fn cold_範囲は先頭から走り_解析では減衰を測らない() {
        let s = song(64.0);
        let span = RenderSpan::RangeCold { start_beat: 8.0, end_beat: 24.0 };
        // 書き出し (tail あり)。
        let wav = RenderWindow::resolve(&s, 48_000, span, true).unwrap();
        assert_eq!((wav.write_start, wav.write_end), (192_000, 576_000));
        assert_eq!(wav.walk_start, 192_000, "cold は範囲の先頭から走る");
        assert_eq!(wav.total_samples, 576_000 + 48_000 * TAIL_MAX_SECONDS);
        // 解析 (tail なし) — r.md #54: 範囲ちょうどで止める。
        let an = RenderWindow::resolve(&s, 48_000, span, false).unwrap();
        assert_eq!(an.total_samples, an.write_end, "解析が範囲外の減衰まで測っている");
        assert_eq!(
            an.write_end - an.write_start,
            384_000,
            "測定長 = 16 拍 = 8 秒ぶんのフレーム"
        );
    }

    #[test]
    fn warm_範囲は曲頭から走ってプラグイン状態を温める() {
        let w = RenderWindow::resolve(
            &song(64.0),
            48_000,
            RenderSpan::RangeWarm { start_beat: 8.0, end_beat: 24.0 },
            true,
        )
        .unwrap();
        assert_eq!((w.write_start, w.write_end), (192_000, 576_000));
        assert_eq!(w.walk_start, 0, "warm は 0 から走らないとプラグインが温まらない");
    }

    #[test]
    fn 壊れた入力は走査せず失敗させる() {
        let s = song(64.0);
        // 逆順の範囲。
        assert!(
            RenderWindow::resolve(
                &s,
                48_000,
                RenderSpan::RangeCold { start_beat: 24.0, end_beat: 8.0 },
                false
            )
            .is_err()
        );
        // bpm が 0 以下 (拍→サンプル換算が定義できない)。
        let bad = Song { bpm: 0.0, ..song(16.0) };
        assert!(RenderWindow::resolve(&bad, 48_000, RenderSpan::Full, true).is_err());
    }

    #[test]
    fn 空範囲は_0_フレームとして通す() {
        // 「測るものが無い」は失敗ではない (GUI 側が事前に弾くが、engine も壊れない)。
        let w = RenderWindow::resolve(
            &song(16.0),
            48_000,
            RenderSpan::RangeCold { start_beat: 4.0, end_beat: 4.0 },
            false,
        )
        .unwrap();
        assert_eq!(w.write_start, w.write_end);
        assert_eq!(w.total_samples, w.walk_start, "空範囲で走査ループが 1 度も回らない");
    }

    #[test]
    fn shift_saturates_instead_of_overflowing() {
        let (ws, we, total) =
            shift_window_for_master_latency(u32::MAX, u64::MAX, u64::MAX, u64::MAX);
        assert_eq!((ws, we, total), (u64::MAX, u64::MAX, u64::MAX));
    }
}
