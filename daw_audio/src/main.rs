// release ではコンソール窓を出さない (windows-subsystem)。 debug は console の
// まま (standalone 起動時に stdout/tracing が見える)。 docs/plan_icon_and_console.md (#48)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};

// Debug-only: route every heap allocation through `AllocDisabler` so the
// `assert_no_alloc!(...)` blocks inside audio worker code panic the
// instant an RT path tries to allocate. Enabled by `--features rt-assert`.
#[cfg(feature = "rt-assert")]
#[global_allocator]
static GLOBAL: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
use common::audio_bridge::AudioBridgeHandle;
use common::meter::compute_block_peak;
use common::metrics_bridge::MetricsBridgeHandle;
use common::scope_bridge::ScopeBridgeHandle;
use common::protocol::{AudioCommand, AudioEvent};
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
mod launcher;
mod metronome;
mod mixer;
mod mod_plan_publish;
mod mod_tick;
mod sequencer;
mod song_values;
mod stretch_engine;

use engine::{
    EngineCommand, EngineShared, LocalState, PlaybackCommand, PluginEntry, RtBundle, SharedState,
    SyncSlot, WorkerRig,
};
use graph::{DelayLine, Schedule, compile_schedule};
use mod_plan_publish::{ModPhaseTableBuilder, ModPlanPublisher};

/// A1 (r.md #8): 出力ストリームを開く前にデフォルト出力デバイスの実サンプルレートを
/// 問い合わせる (stream は開かない)。 Hello で親へ報告し、 session.sample_rate の SSoT に
/// する → engine / plugin / GUI / VOICEVOX が全てハードウェアのレートで揃う。 デバイス
/// 無し / query 失敗時は `None` (親が `audio_bridge::DEFAULT_SAMPLE_RATE` へ fallback)。
/// 後で `start_output_stream` が同じ `default_output_config` で stream を開くので、
/// 報告値と実ストリームのレートは一致する。
fn query_default_output_sample_rate() -> Option<u32> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host().default_output_device()?;
    let config = device.default_output_config().ok()?;
    // cpal 0.17: `sample_rate()` は raw `u32` を返す (SampleRate newtype 廃止)。
    Some(config.sample_rate())
}

#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = common::logging::init_tracing_for("daw_audio");
    tracing::info!("daw_audio started");

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    // A1 (r.md #8): デバイス実レートを Hello で親へ報告 → session.sample_rate の SSoT。
    // Hello には PROTOCOL_FINGERPRINT が同梱され、親がビルド世代を検証する (§3)。
    let device_sample_rate = query_default_output_sample_rate();
    let mut pipe =
        common::client::perform_audio_handshake(&pipe_name, device_sample_rate).await?;
    tracing::info!(?device_sample_rate, "daw_audio handshake complete");

    let session = common::client::read_audio_session(&mut pipe).await?;
    tracing::info!(?session, "audio session ready");

    let bridge = Arc::new(
        AudioBridgeHandle::open(&session.shmem_id).context("failed to open audio shmem")?,
    );
    // resource monitor (r.md #3): DSP load / xrun / buffer 情報を publish する
    // 共有メモリ。 daw_gui が create したものを open する。
    let metrics = Arc::new(
        MetricsBridgeHandle::open(&session.metrics_shmem_id)
            .context("failed to open metrics shmem")?,
    );
    // r.md #50: マスター出力サンプルのリング。GUI が create したものを open し、
    // `render_master_buffer` の出力 (メトロノーム前) を毎バッファ書き込む。
    let scope = Arc::new(
        ScopeBridgeHandle::open(&session.scope_shmem_id)
            .context("failed to open scope shmem")?,
    );
    scope.set_sample_rate(session.sample_rate);

    let shared = Arc::new(SharedState::new());
    // Engine resources shared between the CPAL closure, the export thread and
    // the notify thread.
    // r.md #40: stretch engine pool の off-thread -> RT 配送 ring は EngineShared と
    // 一緒に作り、RT 側の片割れを CPAL closure (LocalState) へ渡す。
    let (engine_shared, stretch_pool_rx, stretch_pool_recycle_tx) =
        EngineShared::new_with_stretch_rings();
    let engine_shared = Arc::new(engine_shared);

    // Preview channel: the receive loop pushes keyboard-preview notes here;
    // the audio thread drains it at the top of every buffer. shmem / worker
    // pool の重い扱いは bundle ring 経由に移設済 (plan §4)。
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<EngineCommand>();

    // plan §4: wait-free SPSC pair for RT snapshot delivery. The receive loop
    // builds `RtBundle`s (schedule compile / plugin_refs rebuild / worker pool
    // spawn — all off-thread) and pushes them on the forward ring; the audio
    // thread pops the newest and ships the superseded bundle back on the
    // recycle ring, so `Drop` (free / shmem unmap / worker join) never runs on
    // the CPAL callback. 64 recycle slots is far above the edits-between-
    // drains a human can produce; the receive loop drains it on every message.
    let (bundle_tx, bundle_rx) = rtrb::RingBuffer::<RtBundle>::new(8);
    let (bundle_recycle_tx, bundle_recycle_rx) = rtrb::RingBuffer::<RtBundle>::new(64);

    let stream = start_output_stream(
        Arc::clone(&shared),
        Arc::clone(&engine_shared),
        Arc::clone(&bridge),
        Arc::clone(&metrics),
        Arc::clone(&scope),
        session.sample_rate,
        cmd_rx,
        bundle_rx,
        bundle_recycle_tx,
        stretch_pool_rx,
        stretch_pool_recycle_tx,
    )
    .context("failed to start audio stream")?;
    tracing::info!("audio stream running");
    // r.md #49: 以後 stream の pause / play は `ParkDriver` 経由でのみ行う
    // (park を要求する CPAL コールバックと、解除を要求する receive loop の
    // 2 系統を 1 本の Mutex に直列化する)。
    let park: Park = Arc::new(std::sync::Mutex::new(ParkDriver {
        stream,
        parked: false,
    }));

    // Split the pipe so the receive loop can keep reading while the
    // export thread (off-tokio) ships completion notifications back to
    // daw_gui. `out_rx` drains the queue on a single tokio task so the
    // pipe writer is single-owner.
    let (read_half, mut write_half) = tokio::io::split(pipe);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<AudioEvent>();
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Err(e) = write_msg(&mut write_half, &msg).await {
                tracing::error!(error = ?e, "failed to send AudioEvent from daw_audio");
                break;
            }
        }
    });

    // plan §4: RT からは pipe に書けない (I/O 禁止) ので、 quarantine / pool
    // stall / MMCSS 失敗のフラグを 100ms 周期で poll して GUI へ通知する
    // 専用スレッド。 dedup は per-entry / per-rig の AtomicBool swap。
    let notify = spawn_notify_thread(
        Arc::clone(&engine_shared),
        out_tx.clone(),
        Arc::clone(&shared),
        Arc::clone(&park),
    );

    // Background decode worker (r.md #7 decode 再設計 B): keeps large WAV
    // decodes off the tokio receive loop. The receive loop publishes a
    // reuse-only partial schedule instantly (zero decode), then hands the song
    // here for full decode of any newly-added source.
    let (decode_tx, decode_rx) = std::sync::mpsc::channel::<DecodeJob>();
    {
        let engine_shared = Arc::clone(&engine_shared);
        let sr = session.sample_rate;
        std::thread::Builder::new()
            .name("audio-decode".to_string())
            .spawn(move || decode_worker_loop(decode_rx, engine_shared, sr))
            .context("failed to spawn audio decode worker")?;
    }

    recv_loop(
        read_half,
        shared,
        Arc::clone(&engine_shared),
        session.sample_rate,
        cmd_tx,
        out_tx,
        decode_tx,
        bundle_tx,
        bundle_recycle_rx,
        Arc::clone(&park),
    )
    .await;

    // (r.md #61) ここから graceful teardown。順序が意味を持つ:
    //   1. notify thread を止めて join する — この thread が `park` の Arc clone を
    //      持っている間は `cpal::Stream::drop` が走らない (旧実装の Arc リーク)。
    //   2. stream を明示 pause してデバイスを解放する。
    //   3. `park` の最後の Arc を落として `cpal::Stream` を drop する。
    // 待ちはどれも有界 (join は poll 周期 100ms が上限)。
    if let Some(notify) = notify {
        notify.stop_and_join();
    }
    {
        let mut driver = park
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        driver.stop_for_shutdown();
    }
    // 不変条件: ここで `park` の Arc は 1 本だけ (notify thread と recv_loop の
    // clone は上で落ちている)。残っていると `cpal::Stream::drop` が走らず
    // デバイスが解放されない = r.md #61 で直した Arc リークの再発。
    // panic ではなく error ログにするのは、**この時点でプロセスは終わるだけ**で、
    // 落として exit code を汚し log flush を飛ばすより、リークの事実を記録して
    // 静かに終わる方が診断に役立つため (探しに行く先はまさにこのログ)。
    let remaining = Arc::strong_count(&park);
    if remaining != 1 {
        tracing::error!(
            remaining,
            "Park の Arc が他に残っている — cpal::Stream::drop が走らずデバイスが解放されない"
        );
    }
    // ここで `cpal::Stream::drop` が run thread を畳み、WASAPI デバイスが
    // 実際に解放される。**「解放できた」を表すのはこの行** (`stop_for_shutdown`
    // の pause は要求をキューに積んだだけ)。
    drop(park);
    tracing::info!("audio stream released");
    tracing::info!("daw_audio exiting");
    Ok(())
}

/// A request for the background decode worker: fully (re)compile the audio
/// schedule for `song`, decoding any source not already cached in the live
/// renderer. `generation` is the schedule version at dispatch — the worker
/// drops its result if a newer `LoadSong` has bumped it, so a slow decode can't
/// clobber a fresher schedule (r.md #7 decode 再設計 B)。
struct DecodeJob {
    song: Arc<common::model::Song>,
    project_dir: Option<std::path::PathBuf>,
    generation: u64,
}

/// Background decode worker loop. Owns a dedicated std::thread so large WAV
/// decodes never stall the tokio IPC receive loop. Coalesces queued jobs to the
/// newest (so a burst of imports doesn't decode intermediate states), reuses
/// already-decoded buffers from the live renderer, decodes only the missing
/// sources, and publishes the full renderer via `ArcSwap` — but only while its
/// generation is still current.
fn decode_worker_loop(
    rx: std::sync::mpsc::Receiver<DecodeJob>,
    engine_shared: Arc<EngineShared>,
    session_sample_rate: u32,
) {
    while let Ok(mut job) = rx.recv() {
        // Coalesce to the newest queued song so a flurry of imports/edits only
        // decodes the final state, not every intermediate one.
        while let Ok(newer) = rx.try_recv() {
            job = newer;
        }
        if job.generation != engine_shared.schedule_generation.load(Ordering::Acquire) {
            continue; // superseded before we started
        }
        let prev = engine_shared.audio_clip_renderer.load();
        let prev_ref: &audio_clip_renderer::AudioClipRenderer = &prev;
        let full = audio_clip_renderer::compile_audio_schedule(
            &job.song,
            Some(prev_ref),
            job.project_dir.as_deref(),
            session_sample_rate,
            true,
        );
        // Publish only if no newer schedule has landed while we decoded
        // (mutex-guarded so the generation check and the store are atomic).
        publish_audio_clip_schedule(&engine_shared, job.generation, full, session_sample_rate);
    }
}

/// Publish a freshly compiled renderer for `generation`, but only if no newer
/// schedule has already been published. Serializes the receive loop's reuse-only
/// partial and the decode worker's full renderer through a mutex so a slow
/// decode for generation N can't clobber a newer N+1 that landed during the
/// decode (the bare `schedule_generation` re-check has a TOCTOU window between
/// its load and the `store`). Off the audio thread — the CPAL callback only ever
/// `load()`s the `ArcSwap`, never this mutex (r.md #7 B)。
pub(crate) fn publish_audio_clip_schedule(
    engine_shared: &EngineShared,
    generation: u64,
    renderer: audio_clip_renderer::AudioClipRenderer,
    session_sample_rate: u32,
) {
    let mut last = engine_shared
        .last_published_generation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if generation >= *last {
        // r.md #40: schedule を store する **前**に、その schedule が要求する
        // stretch engine を確保して配送する。 RT は buffer の頭で pool を drain
        // してから renderer snapshot を load するので、この順序で「新 schedule の
        // `engine_slot` に対応するエンジンが必ず居る」 が成立する。
        deliver_stretch_engines(engine_shared, &renderer, session_sample_rate);
        engine_shared.audio_clip_renderer.store(Arc::new(renderer));
        *last = generation;
    }
}

/// 新 schedule が要求する stretch engine のうち **不足分だけ**を確保して RT へ
/// 配送する (`EngineShared::stretch_pool_tx`)。 pool は grow-only なので、
/// 既に走行中のエンジンには一切触らない (= 無関係な編集で発音中の clip が
/// prime し直しにならない)。 off-thread 専用 (1 個 ~1 MB の確保が走る)。
fn deliver_stretch_engines(
    engine_shared: &EngineShared,
    renderer: &audio_clip_renderer::AudioClipRenderer,
    session_sample_rate: u32,
) {
    // RT が空にして返した配送便をここで捨てる (RT では free しない)。
    if let Ok(mut recycle) = engine_shared.stretch_pool_recycle_rx.lock() {
        while recycle.pop().is_ok() {}
    }

    let mut delivered = engine_shared
        .delivered_engines_per_track
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Ok(mut tx) = engine_shared.stretch_pool_tx.lock() else {
        return;
    };
    // RT 側 (= consumer) が居ないなら作っても捨てるだけ。 1 個 ~1 MB の確保を
    // publish のたびに空振りさせない。
    if tx.is_abandoned() {
        return;
    }
    for (track_idx, &needed) in renderer.engines_per_track.iter().enumerate() {
        // `MAX_TRACKS` を超える track は render されない (`render_master_buffer` が
        // `min(MAX_TRACKS)` で切る) ので、エンジンを作っても無駄。
        if track_idx >= common::audio_bridge::MAX_TRACKS {
            break;
        }
        // `TrackScratch::stretch_engines` の予約容量が上限。 これを超えて配送すると
        // RT 側が取り込めず、「配送済み」 だけが進んで永久に足りない状態になる
        // (`assign_engine_slots` が同じ上限で彩色するので通常は届かない)。
        let needed = needed.min(
            u16::try_from(audio_clip_renderer::MAX_STRETCH_ENGINES_PER_TRACK).unwrap_or(u16::MAX),
        );
        if delivered.len() <= track_idx {
            delivered.resize(track_idx + 1, 0);
        }
        let have = delivered[track_idx];
        if needed <= have {
            continue;
        }
        let mut engines = Vec::with_capacity(usize::from(needed - have));
        for _ in have..needed {
            let Some(engine) = stretch_engine::StretchEngine::new(session_sample_rate) else {
                tracing::error!(track_idx, "stretch engine の確保に失敗 (OOM?)");
                break;
            };
            engines.push(engine);
        }
        if engines.is_empty() {
            continue;
        }
        let added = u16::try_from(engines.len()).unwrap_or(u16::MAX);
        if tx
            .push(engine::StretchPoolDelivery { track_idx, engines })
            .is_err()
        {
            // ring 満杯 = RT が drain していない (停止中 / 起動前)。 次の publish で
            // 再送されるよう配送済みカウントは進めない。
            tracing::warn!(track_idx, "stretch engine pool ring full; 次の publish で再送");
            continue;
        }
        delivered[track_idx] = have.saturating_add(added);
    }
}

/// r.md #49: CPAL stream の park / resume を直列化する唯一の口。
///
/// park を要求するのは CPAL コールバック (無音アイドルの検出者)、解除を要求するのは
/// receive loop (コマンドの受け手) と、所有者が 2 つに割れる。どちらも非 RT スレッド
/// なので Mutex で 1 本化し、「今 pause 済みか」をこの中だけが持つ状態にする
/// (2 箇所が別々に `Stream` を触ると、pause と play が入れ違って **無音のまま
/// 起きてこない** = 最悪の失敗モードになる)。
///
/// コールバック自身は stream を触れない (cpal のコマンドキュー経由なので
/// コールバック内から呼ぶとデッドロックしうる) ため、要求は atomic フラグで渡す。
struct ParkDriver {
    stream: cpal::Stream,
    parked: bool,
}

impl ParkDriver {
    /// stream を `want` の状態へ遷移させる (既にその状態なら何もしない)。
    /// **呼び出し側が Mutex を保持していること** — 「今どちらか」の判定と実際の
    /// pause / play が割り込まれると、pause と play が入れ違って無音のまま
    /// 起きてこなくなる。
    fn apply(&mut self, engine_shared: &EngineShared, want: bool) {
        if self.parked == want {
            return;
        }
        let result = if want {
            self.stream.pause().map_err(|e| e.to_string())
        } else {
            self.stream.play().map_err(|e| e.to_string())
        };
        match result {
            Ok(()) => {
                self.parked = want;
                if !want {
                    engine_shared.live_parked.store(false, Ordering::Release);
                }
                tracing::info!(parked = want, "audio stream park state changed");
            }
            // 失敗しても状態は変えないので、次の reconcile で再試行される。
            // デバイス消失等の恒常障害は CPAL の error callback が別途上げる。
            Err(e) => tracing::error!(
                error = %e,
                want_parked = want,
                "failed to change audio stream park state"
            ),
        }
    }

    /// (r.md #61) プロセス終了に向けて stream の停止を要求する。
    ///
    /// `apply` と違って「今の状態」を見ずに **必ず** 要求する。
    ///
    /// **これは「停止した」ではなく「停止を要求した」**。cpal 0.17 の wasapi
    /// backend では `Stream::pause` は `Command::PauseStream` をキューに積んで
    /// 即 `Ok(())` を返すだけで (`host/wasapi/stream.rs`)、実際の
    /// `IAudioClient::Stop` は run thread の `process_commands` が後から実行し、
    /// その失敗は `error_callback` へ行ってこの `Result` には現れない。
    /// デバイスが実際に解放されるのは `Stream::drop` が run thread を畳んだ
    /// 時点なので、確認したいログは後段の `audio stream released`。
    fn stop_for_shutdown(&mut self) {
        match self.stream.pause() {
            Ok(()) => tracing::info!("requested audio stream pause for shutdown"),
            Err(e) => tracing::warn!(error = %e, "failed to queue the audio stream pause"),
        }
        self.parked = true;
    }
}

// `cpal::Stream` は wasapi backend で `Send + Sync`
// (`cpal-0.17.1/src/host/wasapi/stream.rs:40,49`)。Mutex 越しに 2 スレッドから
// pause / play を出すのはこの保証に依存している。
type Park = Arc<std::sync::Mutex<ParkDriver>>;

/// (r.md #61) notify thread の停止ハンドル。
///
/// 旧実装は脱出条件の無い `loop` で、`park: Park` の `Arc` clone を**永久に
/// 保持**していた。そのため `main` が return しても strong count が 0 にならず、
/// `cpal::Stream::drop` (= WASAPI デバイスの解放) が **原理的に走らなかった**。
/// 「recv_loop を break する」だけでは直らない ので、停止フラグ + join を持つ。
struct NotifyThread {
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: std::thread::JoinHandle<()>,
}

impl NotifyThread {
    /// 停止を要求して join する。thread は poll 周期 (100ms) の頭でフラグを
    /// 見るので、待ちは高々その 1 周期 (= 有界)。
    fn stop_and_join(self) {
        self.stop.store(true, Ordering::Release);
        match self.join.join() {
            Ok(()) => tracing::info!("audio notify thread joined"),
            Err(_) => tracing::warn!("audio notify thread panicked"),
        }
    }
}

/// `park_requested` が指す状態へ stream を寄せる (reconciler)。notify thread が
/// 100ms ごとに呼ぶ。
///
/// **要求の読み取りを Mutex の中で行う**のが要点。外で読むと、`wake_stream` が
/// 要求を取り下げた**後**に古い `true` で pause してしまい、再生中に音が止まる。
///
/// 「要求 → 追従」の形にしておくと、コールバックが要求を取り下げただけの場合
/// (= IPC を伴わずにアイドルが崩れた) も次の周回で自然に復帰する。
fn reconcile_park(park: &Park, shared: &SharedState, engine_shared: &EngineShared) {
    let mut d = park
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let want = shared.park_requested.load(Ordering::Acquire);
    d.apply(engine_shared, want);
}

/// park 要求を取り下げて即座に起こす。receive loop がコマンド受信時に呼ぶ。
///
/// reconciler を待たずにここで起こすのは応答性のため (最大 100ms 遅れると
/// 「Play を押してから音が出るまで一拍おく」になる)。要求の取り下げを先に
/// 行うので、同時に走っている reconciler が pause 側へ倒すことはない。
fn wake_stream(park: &Park, shared: &SharedState, engine_shared: &EngineShared) {
    shared.park_requested.store(false, Ordering::Release);
    shared.idle_silent_samples.store(0, Ordering::Release);
    let mut d = park
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    d.apply(engine_shared, false);
}

/// plan §4: quarantine / pool stall / MMCSS 失敗フラグを 100ms 周期で poll
/// して GUI へ `AudioEvent` を送る通知スレッド (RT からは atomic store のみ)。
/// フラグの SSoT は `PluginEntry` / `WorkerRig` / `EngineShared` 上の
/// AtomicBool で、 dedup は `*_notified` の swap。
///
/// r.md #49: アイドル park の実行もここが担う (コールバックは atomic を立てるだけ)。
fn spawn_notify_thread(
    engine_shared: Arc<EngineShared>,
    out_tx: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
    shared: Arc<SharedState>,
    park: Park,
) -> Option<NotifyThread> {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("audio-notify".to_string())
        .spawn(move || {
            // (r.md #61) `park` の Arc clone をこの thread が持つので、
            // **抜ける条件が無いと `cpal::Stream::drop` が永久に走らない**。
            // `stop` を見て抜け、clone をここで落とす。
            while !stop_for_thread.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(100));
                // r.md #49: stream の状態をコールバックの park 要求へ追従させる。
                // pause 側 (アイドル検出) も play 側 (要求の取り下げ) もここが拾う。
                reconcile_park(&park, &shared, &engine_shared);
                // CPAL callback の MMCSS join 失敗の one-shot warn (RT では
                // tracing を出せないのでここで代行)。
                if engine_shared.mmcss_join_failed.load(Ordering::Acquire)
                    && !engine_shared.mmcss_warned.swap(true, Ordering::AcqRel)
                {
                    tracing::warn!("CPAL callback: MMCSS join (Pro Audio) failed");
                }
                // dispatch timeout → quarantine された device を 1 回だけ通知。
                let refs = engine_shared.plugin_refs.load();
                for (id, entry) in refs.iter() {
                    if entry.quarantined.load(Ordering::Acquire)
                        && !entry.unresponsive_notified.swap(true, Ordering::AcqRel)
                    {
                        tracing::warn!(
                            device_id = *id,
                            "plugin unresponsive (dispatch timeout); quarantined"
                        );
                        let _ = out_tx.send(AudioEvent::PluginUnresponsive { device_id: *id });
                    }
                }
                // worker pool 全体の完了待ち timeout → pool 停止を 1 回だけ通知
                // (GUI は plugin_host respawn → OpenWorkerPool 再送で復旧する)。
                if let Some(rig) = engine_shared.worker.load_full()
                    && rig.pool.as_ref().is_some_and(|p| p.is_stalled())
                    && !rig.stall_notified.swap(true, Ordering::AcqRel)
                {
                    tracing::error!(
                        "audio worker pool stalled; dispatch disabled until pool rebuild"
                    );
                    let _ = out_tx.send(AudioEvent::WorkerPoolStalled);
                }
            }
            tracing::info!("audio notify thread exiting");
        });
    match join {
        Ok(join) => Some(NotifyThread { stop, join }),
        Err(e) => {
            // spawn 失敗はアイドル park と quarantine 通知を失うだけで、
            // 音は鳴り続ける。終了時に join するものが無いだけなので None。
            tracing::error!(error = ?e, "failed to spawn audio notify thread");
            None
        }
    }
}

/// forward ring への bundle 送出 (drop-oldest 化)。 rtrb の producer は
/// consumer 側を追い出せないので、 ring full 時は新しい bundle を `parked` に
/// 退避し (旧 parked = superseded は **ここ (off-thread)** で drop)、 次の
/// 送出 / recv イテレーションで再 push する。 RT が正常に drain していれば
/// full は起きない — これは「RT が遅くても最新編集が最終的に必ず届く」保険
/// (plan §4: drop-newest でなく drop-oldest)。
struct BundlePublisher {
    tx: rtrb::Producer<RtBundle>,
    parked: Option<RtBundle>,
    /// r.md #89: クロス変調の評価計画の publish 状態 (内容が変わったときだけ載せる)。
    mod_plans: ModPlanPublisher,
    /// 直近 topology compile に使った `buffer_frames` (leaf 宛 sidechain tap
    /// の 1-buffer 補償量)。 実測値との drift を検知して再 compile する。
    last_compiled_frames: Option<u32>,
}

impl BundlePublisher {
    fn new(tx: rtrb::Producer<RtBundle>) -> Self {
        Self {
            tx,
            parked: None,
            mod_plans: ModPlanPublisher::default(),
            last_compiled_frames: None,
        }
    }

    /// parked bundle があれば ring へ再 push を試みる。
    fn flush(&mut self) {
        if let Some(bundle) = self.parked.take()
            && let Err(rtrb::PushError::Full(back)) = self.tx.push(bundle)
        {
            self.parked = Some(back);
        }
    }

    fn send(&mut self, bundle: RtBundle) {
        self.flush();
        if let Err(rtrb::PushError::Full(mut newest)) = self.tx.push(bundle) {
            // ring full。 旧 parked は superseded だが、 `schedule` は snapshot
            // ではなく delta なので、 捨てる前に `supersede` で newest へ
            // 畳み込む (RT 側 `refresh_bundle` の coalescing と同じ規約 —
            // 畳み込まないと topology 更新がここで失われる)。 畳み込み後の
            // 残骸だけを drop する — off-thread。
            if let Some(older) = self.parked.take() {
                drop(newest.supersede(older));
            }
            self.parked = Some(newest);
        }
    }
}

/// schedule compile に使う buffer frames (= leaf 宛 sidechain tap の 1-buffer
/// staging 補償量)。 CPAL callback が実測値を `last_buffer_frames` に publish
/// する。 未計測 (stream 稼働前の初回 publish のみ) は WASAPI 共有モード既定の
/// 10ms 周期を仮定し、 最初の callback 後に recv loop の drift check が実測値で
/// 再 compile する。
fn resolve_buffer_frames(engine_shared: &EngineShared, sample_rate: u32) -> u32 {
    let max = common::process_data::MAX_FRAMES as u32;
    match engine_shared.last_buffer_frames.load(Ordering::Acquire) {
        0 => (sample_rate / 100).clamp(64, max),
        measured => measured.min(max),
    }
}

/// `input_delay_per_track` が `TrackScratch` の prealloc (1s) を超える病的
/// ケース用の置換 DelayLine を off-thread で確保する (install 時に RT が
/// swap するだけで済むように)。 全 track が prealloc 内なら空 Vec。
fn build_input_delay_replacements(schedule: &Schedule) -> Vec<Option<DelayLine>> {
    let mut any = false;
    let repl: Vec<Option<DelayLine>> = schedule
        .input_delay_per_track
        .iter()
        .map(|&d| {
            let need = d as usize + 1;
            if d > 0 && need > mixer::INPUT_DELAY_PREALLOC_SAMPLES {
                any = true;
                Some(DelayLine::with_capacity(need))
            } else {
                None
            }
        })
        .collect();
    if any { repl } else { Vec::new() }
}

/// `publish_bundle` の topology 引数。 旧 `recompile: bool` を置き換え、
/// 呼び出し側が意図を名前で述べるようにしたもの。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Topology {
    /// 値のみの更新 (SetTrackVolume 等)。 schedule は載せない = RT は現行
    /// schedule (走行状態込み) を据え置く (§5 D: 値更新で `compile_schedule`
    /// を走らせない)。
    Unchanged,
    /// topology 変更 (LoadSong / buffer_frames drift)。 schedule を off-thread
    /// で再 compile して載せる。
    ///
    /// `reset_song_scoped_state = true` は「別プロジェクトが読み込まれた」
    /// (`Song::project_id` が変わった) の意で、RT に走行状態 (PDC ring /
    /// follower envelope / per-track input delay line) を **引き継がず捨てさせる**。
    /// これらの移送キーは Song スコープの id なので project を跨ぐと別物同士が
    /// 一致してしまう。
    Recompile { reset_song_scoped_state: bool },
}

/// 現在の mirrors (plugin_refs / worker) + `song` で `RtBundle` を組んで RT へ
/// 配送する。 `shared.song` mirror もここで更新する (off-thread 読者用)。
fn publish_bundle(
    publisher: &mut BundlePublisher,
    shared: &SharedState,
    engine_shared: &EngineShared,
    song: Option<Arc<common::model::Song>>,
    sample_rate: u32,
    topology: Topology,
    phase_tables: &ModPhaseTableBuilder,
) {
    shared.song.store(song.clone());
    let tempo_map = match song.as_deref() {
        Some(s) => common::tempo_map::TempoMap::from_song(s),
        None => common::tempo_map::TempoMap::from_song(&common::model::Song::default()),
    };
    let (schedule, input_delay_replacements) = if topology != Topology::Unchanged {
        let buffer_frames = resolve_buffer_frames(engine_shared, sample_rate);
        publisher.last_compiled_frames = Some(buffer_frames);
        let sched = match song.as_deref() {
            // compile 失敗は empty schedule (silent master) に fallback —
            // 壊れた graph は謎の音ではなく無音として聴こえる方が診断しやすい。
            Some(s) => match compile_schedule(
                s,
                &engine_shared.device_latencies.load(),
                sample_rate,
                buffer_frames,
            ) {
                Ok(sc) => sc,
                Err(e) => {
                    tracing::warn!(?e, "graph compile failed; master goes silent");
                    Schedule::empty()
                }
            },
            None => Schedule::empty(),
        };
        let repl = build_input_delay_replacements(&sched);
        (Some(sched), repl)
    } else {
        (None, Vec::new())
    };
    // r.md #89: クロス変調の評価計画。`Song::mod_sources` / `mod_routings` /
    // automation lane から決まるので **値のみ更新でも変わりうる** (schedule と
    // 違って topology 限定ではない)。作るのは安いが、内容が変わっていないのに
    // 載せると RT が毎 buffer 位相を捨てて張り直すので、前回と同じなら載せない。
    let mod_plan = song
        .as_deref()
        .and_then(|sg| publisher.mod_plans.build(sg, sample_rate));
    // 位相表は曲長ぶんの刻みループなので **必ず off-thread**。構築中は旧表 +
    // 閉形式シードで凌ぎ、完成したら housekeeping が次の便で載せる。
    if let (Some((plan, _)), Some(sg)) = (mod_plan.as_ref(), song.as_deref()) {
        phase_tables.request(Arc::clone(plan), sg, sample_rate);
    }
    publisher.send(RtBundle {
        song,
        tempo_map,
        schedule,
        mod_plan,
        mod_phase_table: None,
        reset_song_scoped_state: matches!(
            topology,
            Topology::Recompile {
                reset_song_scoped_state: true
            }
        ),
        input_delay_replacements,
        plugin_refs: engine_shared.plugin_refs.load_full(),
        worker: engine_shared.worker.load_full(),
    });
}

/// Apply `f` to a clone of the current song and publish the result as a
/// **値のみ** bundle (schedule 再 compile なし — §5 D)。 mixer-strip 変更は
/// user-driven (slider drag rate) なので clone は IPC スレッドで許容。
fn update_song_values<F>(
    publisher: &mut BundlePublisher,
    shared: &SharedState,
    engine_shared: &EngineShared,
    sample_rate: u32,
    phase_tables: &ModPhaseTableBuilder,
    f: F,
) where
    F: FnOnce(&mut common::model::Song),
{
    let snapshot = shared.song.load();
    let Some(song) = snapshot.as_deref() else {
        return;
    };
    let mut next = song.clone();
    f(&mut next);
    publish_bundle(
        publisher,
        shared,
        engine_shared,
        Some(Arc::new(next)),
        sample_rate,
        Topology::Unchanged,
        phase_tables,
    );
}

/// 鍵盤プレビューの note-on/off を送る対象 track の Vec index を、 audio engine
/// の現 song snapshot から track id で引く。 song 未ロード / id 不在 / `MAX_TRACKS`
/// 超過は `None` (= プレビュー drop)。 id ベースなので GUI 側の track 並べ替えと
/// race しない (= `SetTrackVolume` 等と同じ方針)。
fn preview_track_index(shared: &Arc<SharedState>, track_id: u32) -> Option<usize> {
    let snapshot = shared.song.load();
    let song = snapshot.as_deref()?;
    song.tracks
        .iter()
        .position(|t| t.id == track_id)
        .filter(|&i| i < engine::MAX_TRACKS)
}

/// recv loop の周期処理 (メッセージ処理とは独立に走る)。
///
/// plan §4: dispose bundles the audio thread superseded, so their `Drop`
/// (free / shmem unmap / worker pool join) runs here, off the audio callback.
/// parked bundle (ring full 時の drop-oldest 退避) も再送する。
///
/// **メッセージ到着に依存させない**のが load-bearing: 旧実装はこれを
/// `read_msg().await` の手前 1 箇所でしか走らせておらず、「次の AudioCommand が
/// 来なければ superseded bundle は永久に解放されない」= 編集を止めた瞬間に
/// shmem mapping / worker rig / Song snapshot が無期限に居座る状態だった
/// (解放時刻に上限が無い)。[`HOUSEKEEPING_INTERVAL`] のタイマ枝から同じ処理を
/// 呼ぶことで上限を与える。RT 側は無変更 (push のみ)。
fn recv_loop_housekeeping(
    publisher: &mut BundlePublisher,
    bundle_recycle_rx: &mut rtrb::Consumer<RtBundle>,
    shared: &Arc<SharedState>,
    engine_shared: &Arc<EngineShared>,
    session_sample_rate: u32,
    phase_tables: &ModPhaseTableBuilder,
) {
    while let Ok(old) = bundle_recycle_rx.pop() {
        drop(old);
    }
    publisher.flush();
    // r.md #89: off-thread で張り終えた位相表を RT へ載せる (構築中は旧表 +
    // 閉形式シードで凌いでいる)。plan と別便なのは、表の構築が曲長ぶんの
    // 刻みループで、plan の配送を待たせたくないから (設計正本 §2.4)。
    if let Some(table) = phase_tables.take_finished() {
        publisher.send(RtBundle {
            song: shared.song.load_full(),
            tempo_map: match shared.song.load().as_deref() {
                Some(s) => common::tempo_map::TempoMap::from_song(s),
                None => common::tempo_map::TempoMap::from_song(&common::model::Song::default()),
            },
            schedule: None,
            reset_song_scoped_state: false,
            input_delay_replacements: Vec::new(),
            plugin_refs: engine_shared.plugin_refs.load_full(),
            worker: engine_shared.worker.load_full(),
            mod_plan: None,
            mod_phase_table: Some(table),
        });
    }
    // leaf 宛 sidechain tap の 1-buffer 補償量 (= 実測 buffer frames) が
    // compile 時の仮定から変わっていたら topology を再 publish する
    // (初回 publish が stream 実測前に走った場合の是正)。
    if let Some(compiled) = publisher.last_compiled_frames
        && resolve_buffer_frames(engine_shared, session_sample_rate) != compiled
    {
        let song = shared.song.load_full();
        if song.is_some() {
            publish_bundle(
                publisher,
                shared,
                engine_shared,
                song,
                session_sample_rate,
                // 同 project の再 compile なので走行状態は引き継ぐ。
                Topology::Recompile {
                    reset_song_scoped_state: false,
                },
                phase_tables,
            );
        }
    }
}

/// [`recv_loop_housekeeping`] の周期。数 buffer 分 (~10-21ms/buffer) の
/// オーダーで、アイドル時の wake も無視できる粒度。
const HOUSEKEEPING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// shmem 名まわりの回帰を **決定論** に落とすためのフォールトインジェクション
/// (debug ビルド限定・既定 OFF)。
///
/// `DAW01_TEST_SHMEM_HOLD_MS=<ms>` を設定すると、`ClosePluginShmem` で map から
/// 外した entry (= `ProcessData` shmem の OS ハンドル) を指定時間だけ保持し続ける。
/// 「daw_audio が旧 mapping をまだ握っている」状態を任意に作れる。
///
/// なぜ**保持側**に入れるのか: 「同名 `ProcessData` shmem の再作成が
/// `already exists` で失敗する」の発火条件は *plugin_host が create する時点で
/// daw_audio がまだ解放していない* こと。create 側を遅らせると解放が先に間に合って
/// **失敗しにくくなる** ので、レースを常時再現させるには保持側を押さえるしかない。
/// これで「(incarnation 導入前は) 必ず落ちる / 導入後は必ず通る」実験が成立する。
/// 手順は `daw_gui/tests/scripts/reopen_same_project.js` の冒頭コメント。
#[cfg(debug_assertions)]
fn hold_released_entry_for_test(entry: Option<Arc<PluginEntry>>) {
    let Some(entry) = entry else { return };
    let Some(ms) = std::env::var("DAW01_TEST_SHMEM_HOLD_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
    else {
        return; // 既定: ここで drop = 通常の解放経路
    };
    tracing::warn!(hold_ms = ms, "DAW01_TEST_SHMEM_HOLD_MS: holding released plugin shmem (fault injection)");
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        drop(entry);
    });
}

#[cfg(not(debug_assertions))]
fn hold_released_entry_for_test(_entry: Option<Arc<PluginEntry>>) {}

#[allow(clippy::too_many_arguments)]
async fn recv_loop(
    mut pipe: ReadHalf<NamedPipeClient>,
    shared: Arc<SharedState>,
    engine_shared: Arc<EngineShared>,
    session_sample_rate: u32,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<EngineCommand>,
    out_tx: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
    decode_tx: std::sync::mpsc::Sender<DecodeJob>,
    bundle_tx: rtrb::Producer<RtBundle>,
    mut bundle_recycle_rx: rtrb::Consumer<RtBundle>,
    park: Park,
) {
    let mut publisher = BundlePublisher::new(bundle_tx);
    // r.md #89: 位相表を張る専用スレッド (最新の要求だけ残す郵便受け)。
    let phase_tables = ModPhaseTableBuilder::spawn();
    let mut housekeeping = tokio::time::interval(HOUSEKEEPING_INTERVAL);
    // 遅延して詰まった tick を burst で取り戻さない (drain は冪等なので
    // 取り戻す意味が無く、CPU を無駄に食うだけ)。
    housekeeping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        recv_loop_housekeeping(
            &mut publisher,
            &mut bundle_recycle_rx,
            &shared,
            &engine_shared,
            session_sample_rate,
            &phase_tables,
        );
        // `read_msg` (= `read_exact` 2 回) は **cancel-safe ではない**ので、
        // select の枝に直接置くと timer が先に発火したときに読みかけの
        // length prefix / body を捨ててストリームを破壊する。future を
        // ループの外で 1 度だけ作って `&mut` で待ち続け、tick 側だけを
        // 繰り返すことで、read の状態を保ったまま周期処理を挟む。
        let msg = {
            let mut read_fut = std::pin::pin!(read_msg::<_, AudioCommand>(&mut pipe));
            loop {
                tokio::select! {
                    // メッセージが読めたなら常にそちらを優先する。
                    biased;
                    r = &mut read_fut => break r,
                    _ = housekeeping.tick() => recv_loop_housekeeping(
                        &mut publisher,
                        &mut bundle_recycle_rx,
                        &shared,
                        &engine_shared,
                        session_sample_rate,
                        &phase_tables,
                    ),
                }
            }
        };
        // r.md #49: park 中に届いたコマンドは、それが何であれ「エンジンに仕事が
        // 生じた」合図なので stream を起こしてから処理する。`Play` のとき /
        // preview のとき / 書き出しのとき… と列挙すると、コマンドが増えるたびに
        // 起こし忘れる「補償コード」(アーキテクチャ不変条件 1 が禁じる形) になる。
        //
        // 例外は `SetAppActive(false)` だけ — これは park 要求そのもの。
        // 起こしたあと条件がまだ揃っていれば、コールバックが改めて数え直して
        // 再び park するので、余分に起きても害はない。
        if !matches!(msg, Ok(AudioCommand::SetAppActive(false))) {
            wake_stream(&park, &shared, &engine_shared);
        }
        match msg {
            // Handshake 済みの再送 Ack / Session は no-op (Session は起動時に
            // `read_audio_session` が消費済み — shmem 名と format はプロセス
            // 生存中不変)。
            Ok(AudioCommand::Ack) | Ok(AudioCommand::Session(_)) => {}
            // r.md #49: アプリの窓がアクティブかの報告。park してよいかの判断は
            // engine 側 (`buffer_is_idle`) が行うので、ここは事実の反映のみ。
            Ok(AudioCommand::SetAppActive(active)) => {
                shared.app_active.store(active, Ordering::Release);
                if active {
                    // 猶予カウンタを畳んでおく (非アクティブ→アクティブ→非アクティブ
                    // と往復したとき、前回の途中まで数えた分から再開しない)。
                    shared.idle_silent_samples.store(0, Ordering::Release);
                }
            }
            Ok(AudioCommand::Play) => {
                tracing::info!("received Play");
                shared
                    .playback
                    .store(PlaybackCommand::Play as u8, Ordering::Release);
            }
            Ok(AudioCommand::Stop) => {
                tracing::info!("received Stop");
                shared
                    .playback
                    .store(PlaybackCommand::Stop as u8, Ordering::Release);
            }
            Ok(AudioCommand::Panic) => {
                // arm the master declick. The CPAL callback consumes
                // this edge flag, fades the master out and holds at zero until
                // `PanicRelease`, so the imminent `ReinitAllPlugins` (which yanks
                // every plugin out of the mix) doesn't produce a step click.
                tracing::info!("received Panic (master declick)");
                shared.panic_declick.store(true, Ordering::Release);
            }
            Ok(AudioCommand::PanicRelease) => {
                // the plugin reinit finished — release the declick
                // hold so the master fades back in over a now-silent mix.
                tracing::info!("received PanicRelease (declick fade-in)");
                shared.panic_release.store(true, Ordering::Release);
            }
            Ok(AudioCommand::SetLoop(mut region)) => {
                // IPC は信頼境界。 NaN / 負値の拍位置は samples_per_beat 換算を
                // 壊すので store 前に正規化する (LoadSong の sanitize_ranges と同旨)。
                region.sanitize();
                shared.loop_region.store(std::sync::Arc::new(region));
            }
            Ok(AudioCommand::SeekTo { samples }) => {
                // playhead を IPC 受信スレッドから直接書かない。
                // audio thread も buffer 末で playhead を store するため、両者が
                // 同一 atomic を別スレッドから書く race になり、Stop 直後の開始
                // 位置への巻き戻しが in-flight buffer の advance に上書きされて
                // 停止位置から再生されるバグを生む。seek 要求は
                // pending_seek に積み、audio thread が process_buffer 冒頭で
                // swap 消費して playhead に反映する (playhead の writer を audio
                // thread 単独に保つ)。ruler click / Stop 復帰の双方ともこの経路。
                shared.pending_seek.store(samples, Ordering::Release);
                tracing::info!(samples, "received SeekTo");
            }
            Ok(AudioCommand::LoadSong(mut song)) => {
                // IPC は信頼境界なので、 受信した song の値域を store 前に
                // 正規化 (bpm/time_sig/length/loop/framerate を有限・正に)。
                // これで下流の divisor (samples_per_beat 等) が NaN / 0 /
                // 負値で壊れない。 idempotent。
                song.sanitize_ranges();
                // 別プロジェクトが読み込まれたか (`Song::project_id` は v24 で
                // 導入されたプロジェクト同一性の SSoT。New で採番し save/load で
                // 保持される)。Song 内の id (track / device / audio_source /
                // ModSource …) はどれも project ごとに 1 から再採番されるので、
                // project が変わった瞬間に **それらを key にした状態は全部無効**
                // になる。ここが daw_audio 側の唯一の検出点。
                let project_switched = engine_shared
                    .loaded_project_id
                    .swap(song.project_id, Ordering::AcqRel)
                    != song.project_id;
                if project_switched {
                    tracing::info!(
                        project_id = song.project_id,
                        "project switched; dropping song-scoped engine state"
                    );
                    // 旧 project の device の shmem 参照。 破棄は daw_gui の
                    // ClosePluginShmem に依存していたが、 その列挙元 (帳簿) に
                    // 取りこぼしがあると前 project の instance を掴んだまま
                    // 音を出してしまう。 device_id も project ごとに 1 から
                    // 再採番されるので、 ここで一括して捨てる (新 project の
                    // 分は SetSlotPlugin → SlotPluginLoaded → OpenPluginShmem
                    // で必ず後から届く)。
                    engine_shared
                        .plugin_refs
                        .store(Arc::new(std::collections::HashMap::new()));
                    // track_id keyed。 非空のまま持ち越すと新 project の同番号
                    // track の automation が bypass されたままになる。
                    shared
                        .recording_lanes
                        .store(Arc::new(std::collections::HashSet::new()));
                    // device_id keyed の PDC 入力も同じく project スコープ。
                    // 持ち越すと新 project の同番号 device が、 まだ何も報告して
                    // いないのに前 project の latency で補償される。
                    engine_shared
                        .device_latencies
                        .store(Arc::new(graph::DeviceLatencies::new()));
                }
                let project_dir_g = engine_shared.project_dir.load();
                let project_dir: Option<std::path::PathBuf> =
                    project_dir_g.as_ref().map(|arc| (**arc).clone());
                // Bump the schedule version so any in-flight decode for an older
                // song is discarded when it tries to publish (r.md #7 B)。
                let generation = engine_shared
                    .schedule_generation
                    .fetch_add(1, Ordering::AcqRel)
                    + 1;
                let song = Arc::new(song);
                // Phase 1: publish a reuse-only schedule synchronously. Sources
                // already decoded in the live renderer are Arc-cloned (no
                // decode), so BPM change / edit / scrub re-compile with zero
                // decode and never block the receive loop. Sources not yet
                // decoded are left out — their events stay silent until the
                // worker fills them in.
                let prev = engine_shared.audio_clip_renderer.load();
                let prev_ref: &audio_clip_renderer::AudioClipRenderer = &prev;
                let partial = audio_clip_renderer::compile_audio_schedule(
                    &song,
                    Some(prev_ref),
                    project_dir.as_deref(),
                    session_sample_rate,
                    false,
                );
                // main 側: 未 decode 判定は id 一致でなく origin (解決済み絶対
                // パス) 一致で行う。 r.md #40 側: publish が stretch engine pool の
                // 配送も担うので session SR が要る。
                let needs_decode = audio_clip_renderer::has_undecoded_sources(
                    &song,
                    &partial,
                    project_dir.as_deref(),
                );
                publish_audio_clip_schedule(
                    &engine_shared,
                    generation,
                    partial,
                    session_sample_rate,
                );
                // topology publish: routing schedule + tempo map を off-thread
                // で compile して RT へ wait-free 配送 (shared.song もここで更新)。
                publish_bundle(
                    &mut publisher,
                    &shared,
                    &engine_shared,
                    Some(Arc::clone(&song)),
                    session_sample_rate,
                    Topology::Recompile {
                        reset_song_scoped_state: project_switched,
                    },
                    &phase_tables,
                );
                // Phase 2: hand off to the background worker for full decode of
                // any missing source. Skipped when everything was reusable
                // (= BPM change / edit / scrub → decode ゼロ で即完結)。
                if needs_decode {
                    let _ = decode_tx.send(DecodeJob {
                        song,
                        project_dir,
                        generation,
                    });
                }
            }
            Ok(AudioCommand::SetMasterGain(g)) => {
                // render (`render_master_buffer`) が読む — live / export 共通。
                // +6 dB (amp 2.0) まで許可 (r.md #11、 GUI clamp と同 SSoT)。
                let clamped = g.clamp(0.0, common::model::MAX_TRACK_GAIN);
                engine_shared
                    .master_gain
                    .store(clamped.to_bits(), Ordering::Relaxed);
            }
            Ok(AudioCommand::SetDeviceLatency { device_id, samples }) => {
                // 報告値はプラグイン (= 信頼できない外部コード) が返した u32 で、
                // そのまま DelayLine の容量になる。 実在するプラグインの latency は
                // 高々数百 ms なので、 10 秒相当で頭打ちにして異常値で確保を
                // 暴走させない (FFI / IPC 境界の値域検証)。
                let max_samples = session_sample_rate.saturating_mul(10);
                let samples = if samples > max_samples {
                    tracing::warn!(
                        device_id,
                        samples,
                        max_samples,
                        "plugin reported an implausible latency; clamping"
                    );
                    max_samples
                } else {
                    samples
                };
                // PDC の入力更新。 表は off-RT でしか読まれない (compile 時のみ)
                // ので、 copy-on-write で差し替えて schedule を組み直す。
                // 値が変わらないなら再 compile しない (plugin host は load ごとに
                // 0 でも必ず報告してくるので、 無条件 recompile は起動時に
                // device 数ぶんの無駄な再 compile を生む)。
                let current = engine_shared.device_latencies.load();
                let unchanged = match (current.get(&device_id), samples) {
                    (None, 0) => true,
                    (Some(&prev), s) => prev == s,
                    _ => false,
                };
                if !unchanged {
                    let mut next = (**current).clone();
                    if samples == 0 {
                        next.remove(&device_id);
                    } else {
                        next.insert(device_id, samples);
                    }
                    tracing::info!(device_id, samples, "device latency updated (PDC 再 compile)");
                    engine_shared.device_latencies.store(Arc::new(next));
                    // song が届く前の報告もあり得る (plugin load の方が速い) —
                    // その場合は表だけ更新し、 次の LoadSong の compile が拾う。
                    let song = shared.song.load_full();
                    if song.is_some() {
                        publish_bundle(
                            &mut publisher,
                            &shared,
                            &engine_shared,
                            song,
                            session_sample_rate,
                            // 同 project の再 compile。 DelayLine / FollowerSlot の
                            // 走行状態は引き継ぐ (曲は変わっていない)。
                            Topology::Recompile {
                                reset_song_scoped_state: false,
                            },
                            &phase_tables,
                        );
                    }
                }
            }
            Ok(AudioCommand::OpenWorkerPool {
                n_workers,
                worker_bridge_shmem_id,
                wake_event_names,
                done_event_names,
            }) => {
                // worker rig (bridge shmem + handshake events + audio worker
                // threads) を **off-thread で** 構築し、 mirror + bundle で
                // 配送する。 旧 rig は RT の swap 後 recycle ring 経由で
                // ここに戻り、 off-thread で drop (= worker join) される。
                match build_worker_rig(
                    n_workers,
                    &worker_bridge_shmem_id,
                    &wake_event_names,
                    &done_event_names,
                ) {
                    Ok(rig) => {
                        tracing::info!(
                            n_sync_slots = rig.slots.len(),
                            has_pool = rig.pool.is_some(),
                            "audio engine bound to plugin-host worker pool"
                        );
                        engine_shared.worker.store(Some(Arc::new(rig)));
                        let song = shared.song.load_full();
                        publish_bundle(
                            &mut publisher,
                            &shared,
                            &engine_shared,
                            song,
                            session_sample_rate,
                            Topology::Unchanged,
                    &phase_tables,
                        );
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "failed to open audio-side worker pool");
                    }
                }
            }
            Ok(AudioCommand::CloseWorkerPool) => {
                engine_shared.worker.store(None);
                let song = shared.song.load_full();
                publish_bundle(
                    &mut publisher,
                    &shared,
                    &engine_shared,
                    song,
                    session_sample_rate,
                    Topology::Unchanged,
                    &phase_tables,
                );
            }
            Ok(AudioCommand::OpenPluginShmem { device_id, shmem_id }) => {
                // v29: 配置 (どの track のどの位置か) は Song 側の
                // `PluginInstance::id` が SSoT なので、 ここでは device_id →
                // shmem の対応を登録するだけ。 map rebuild は off-thread
                // (snapshot-copy-mutate-publish)、 handle は entry が持つ
                // (旧 Box::leak の解消)。
                match common::process_data::ProcessDataHandle::open(&shmem_id) {
                    Ok(handle) => {
                        let entry = Arc::new(PluginEntry::new(device_id, handle));
                        let mut map: engine::PluginRefs =
                            (**engine_shared.plugin_refs.load()).clone();
                        map.insert(device_id, entry);
                        engine_shared.plugin_refs.store(Arc::new(map));
                        let song = shared.song.load_full();
                        publish_bundle(
                            &mut publisher,
                            &shared,
                            &engine_shared,
                            song,
                            session_sample_rate,
                            Topology::Unchanged,
                    &phase_tables,
                        );
                        tracing::info!(device_id, "plugin shmem registered");
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, device_id, "failed to open plugin shmem");
                    }
                }
            }
            Ok(AudioCommand::ClosePluginShmem { device_id }) => {
                let mut map: engine::PluginRefs = (**engine_shared.plugin_refs.load()).clone();
                let removed = map.remove(&device_id);
                engine_shared.plugin_refs.store(Arc::new(map));
                let song = shared.song.load_full();
                publish_bundle(
                    &mut publisher,
                    &shared,
                    &engine_shared,
                    song,
                    session_sample_rate,
                    Topology::Unchanged,
                    &phase_tables,
                );
                // 旧 entry (shmem mapping) は RT が新 bundle を install して
                // recycle が drain された時点で off-thread unmap される
                // (drain の上限は `recv_loop_housekeeping` のタイマが保証)。
                hold_released_entry_for_test(removed);
                tracing::info!(device_id, "plugin shmem dropped");
            }
            // 値のみの Song 更新 (mixer strip / send / arm / bpm / 拍子)。
            // 宛先は安定 id、クランプと適用は `song_values::apply` が SSoT。
            // schedule は再 compile しない (§5 D) — RT は snapshot を live-read する。
            Ok(cmd @ (AudioCommand::SetTrackVolume { .. }
                | AudioCommand::SetTrackPan { .. }
                | AudioCommand::SetTrackMuted { .. }
                | AudioCommand::SetTrackSolo { .. }
                | AudioCommand::SetTrackArmed { .. }
                | AudioCommand::SetSendGain { .. }
                | AudioCommand::SetSendEnabled { .. }
                | AudioCommand::SetSongBpm { .. }
                | AudioCommand::SetSongTimeSigNumerator { .. })) => {
                update_song_values(&mut publisher, &shared, &engine_shared, session_sample_rate, &phase_tables, |s| {
                    song_values::apply(&cmd, s);
                });
            }
            // r.md #87: クリップランチャーの操作。発火の判断には Song が要るので
            // ここでは audio thread へ積むだけ (`launcher::ipc` が唯一の口)。
            Ok(cmd @ (AudioCommand::LaunchCell { .. }
                | AudioCommand::LaunchScene { .. }
                | AudioCommand::StopRow { .. }
                | AudioCommand::StopAllRows
                | AudioCommand::SwitchRowToArranger { .. }
                | AudioCommand::SwitchAllToArranger
                )) => {
                launcher::ipc::dispatch(cmd, &cmd_tx);
            }
            Ok(AudioCommand::StartRecording { preroll_samples }) => {
                // r.md #51: 録音セッションの開始。 `recording_requested` は
                // 曲末 auto-stop の抑止と `recording_live` の publish に使う。
                // preroll > 0 なら process_buffer が「dispatch / clip render skip +
                // metronome のみ render」 の count-in ループに入り、 0 到達で
                // 通常再生に復帰する (= その瞬間に recording_live が立つ)。
                engine_shared
                    .preroll_total_samples
                    .store(preroll_samples, Ordering::Release);
                engine_shared
                    .preroll_remaining_samples
                    .store(preroll_samples, Ordering::Release);
                engine_shared
                    .recording_requested
                    .store(true, Ordering::Release);
                tracing::info!(preroll_samples, "received StartRecording");
            }
            Ok(AudioCommand::StopRecording) => {
                // r.md #51: 録音セッションの終了 (パンチアウト / 停止 / count-in
                // 取り消し)。 transport はここでは止めない — パンチアウトは
                // 再生を続けるのが参照 DAW 共通の挙動で、停止は `Stop` の仕事。
                engine_shared
                    .recording_requested
                    .store(false, Ordering::Release);
                engine_shared
                    .preroll_remaining_samples
                    .store(0, Ordering::Release);
                engine_shared.preroll_total_samples.store(0, Ordering::Release);
                tracing::info!("received StopRecording");
            }
            Ok(AudioCommand::SetMetronomeEnabled(enabled)) => {
                // Phase 7 B3 (2026-05-13): GUI が transport bar の metronome
                // toggle を切り替え。 audio thread は次 buffer から
                // `render_metronome` の有効無効を切り替える。 lock-free / 0
                // allocation on audio thread。
                shared.metronome_enabled.store(enabled, Ordering::Release);
            }
            Ok(AudioCommand::PreviewNoteOn {
                track_id,
                pitch,
                velocity,
            }) => {
                // 鍵盤レーン click のプレビュー (gui_01 #055)。 GUI は track id を
                // 送る。 ここで audio engine の現 song snapshot から Vec index を
                // 引いて EngineCommand に載せ替える。 解決は IPC スレッド上 =
                // RT 外。 song 未ロード / id 不在なら drop (= 無音)。
                if let Some(track) = preview_track_index(&shared, track_id) {
                    let _ = cmd_tx.send(EngineCommand::PreviewNoteOn {
                        track,
                        pitch,
                        velocity: f64::from(velocity) / 127.0,
                    });
                }
            }
            Ok(AudioCommand::PreviewNoteOff { track_id, pitch }) => {
                if let Some(track) = preview_track_index(&shared, track_id) {
                    let _ = cmd_tx.send(EngineCommand::PreviewNoteOff { track, pitch });
                }
            }
            Ok(AudioCommand::SetRecordingLanes { lanes }) => {
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
            Ok(AudioCommand::SetProjectDir(dir)) => {
                // `compile_audio_schedule` が `AudioSourcePath::ProjectRelative`
                // を `<project_dir>/samples/<...>` に解決するのに使う。
                // `None` for unsaved projects.
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
            Ok(AudioCommand::ExportWav { path, range, write_mod_sidecar }) => {
                // Multi-process defense: atomically reserve the engine for this
                // render. compare_exchange on the recv loop serializes against a
                // second ExportWav — the old "load here, set inside the spawned
                // thread" pattern had a TOCTOU window where two ExportWav could
                // both pass the check before either set the flag and double-spawn,
                // corrupting the shared WAV writer / plugin chain. The spawn
                // closure (and the early-out paths below) release it.
                if engine_shared
                    .export_running
                    .compare_exchange(
                        false,
                        true,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    tracing::warn!("ExportWav received while a render is already running; ignoring");
                    let _ = out_tx.send(AudioEvent::ExportWavComplete {
                        error: Some("export already in progress".into()),
                        cancelled: false,
                    });
                    continue;
                }
                let song_snap = shared.song.load();
                let Some(song_arc) = song_snap.as_ref() else {
                    tracing::warn!("ExportWav received but no song loaded");
                    // Release the reservation we just took (no render will run).
                    engine_shared
                        .export_running
                        .store(false, Ordering::Release);
                    let _ = out_tx.send(AudioEvent::ExportWavComplete {
                        error: Some("no song loaded".into()),
                        cancelled: false,
                    });
                    continue;
                };
                let song = (**song_arc).clone();
                drop(song_snap);
                // Clear any stale cancel from a previous render, synchronously
                // on this receive loop so it's FIFO-ordered against a later
                // CancelExport (which then aborts THIS render, not a prior one).
                engine_shared
                    .export_cancel
                    .store(false, Ordering::Release);
                let engine_shared_clone = Arc::clone(&engine_shared);
                let engine_shared_release = Arc::clone(&engine_shared);
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
                                    done.saturating_sub(prev) >= step
                                        || (done >= total && prev < total)
                                }
                            };
                            let heartbeat = last_at.elapsed()
                                >= std::time::Duration::from_millis(250);
                            if crossed || heartbeat {
                                last_sent = Some(done);
                                last_at = std::time::Instant::now();
                                let _ = out_tx_progress
                                    .send(AudioEvent::ExportWavProgress { done, total });
                            }
                        };
                        // user export range walks cold from the range
                        // start (matches Play-from-here); full export walks 0..len.
                        let span = match range {
                            Some((start_beat, end_beat)) => {
                                export::RenderSpan::RangeCold { start_beat, end_beat }
                            }
                            None => export::RenderSpan::Full,
                        };
                        let result = export::run_export(
                            path,
                            engine_shared_clone,
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
                            error: error_msg,
                            cancelled,
                        });
                    })
                {
                    tracing::error!(error = ?e, "failed to spawn export thread");
                    // No thread will release the reservation we took above.
                    engine_shared
                        .export_running
                        .store(false, Ordering::Release);
                    let _ = out_tx.send(AudioEvent::ExportWavComplete {
                        error: Some(format!("failed to spawn export thread: {e}")),
                        cancelled: false,
                    });
                }
            }
            // r.md #54: 範囲ラウドネス解析。ExportWav と同じ engine 予約 /
            // live park ハンドシェイク / cancel を共有し、走査の出力先だけ
            // WAV writer から LoudnessCollector へ差し替える。
            Ok(AudioCommand::AnalyzeLoudness { range }) => {
                if engine_shared
                    .export_running
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    tracing::warn!(
                        "AnalyzeLoudness received while a render is already running; ignoring"
                    );
                    let _ = out_tx.send(AudioEvent::LoudnessAnalysisComplete {
                        report: None,
                        error: Some("export already in progress".into()),
                        cancelled: false,
                    });
                    continue;
                }
                let song_snap = shared.song.load();
                let Some(song_arc) = song_snap.as_ref() else {
                    tracing::warn!("AnalyzeLoudness received but no song loaded");
                    engine_shared.export_running.store(false, Ordering::Release);
                    let _ = out_tx.send(AudioEvent::LoudnessAnalysisComplete {
                        report: None,
                        error: Some("no song loaded".into()),
                        cancelled: false,
                    });
                    continue;
                };
                let song = (**song_arc).clone();
                drop(song_snap);
                // Clear any stale cancel, synchronously on this receive loop so
                // it is FIFO-ordered against a later CancelExport (same contract
                // as the ExportWav path above).
                engine_shared.export_cancel.store(false, Ordering::Release);
                let engine_shared_clone = Arc::clone(&engine_shared);
                let engine_shared_release = Arc::clone(&engine_shared);
                let out_tx_clone = out_tx.clone();
                let out_tx_progress = out_tx.clone();
                let sample_rate = session_sample_rate;
                if let Err(e) = std::thread::Builder::new()
                    .name("daw-audio-loudness".into())
                    .spawn(move || {
                        // スロットルは sink 側 (250ms) が持つ。ここは送るだけ。
                        let on_progress = move |report: common::loudness_report::LoudnessReport| {
                            let _ = out_tx_progress
                                .send(AudioEvent::LoudnessAnalysisProgress(Box::new(report)));
                        };
                        // 範囲は cold 走査 (= その範囲を書き出したのと同じ音)。
                        let span = match range {
                            Some((start_beat, end_beat)) => {
                                export::RenderSpan::RangeCold { start_beat, end_beat }
                            }
                            None => export::RenderSpan::Full,
                        };
                        let result = export::run_loudness_analysis(
                            engine_shared_clone,
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
                                report: Some(Box::new(outcome.report)),
                                error: None,
                                cancelled: outcome.cancelled,
                            },
                            Err(e) => {
                                tracing::error!(error = ?e, "offline loudness analysis failed");
                                AudioEvent::LoudnessAnalysisComplete {
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
                    let _ = out_tx.send(AudioEvent::LoudnessAnalysisComplete {
                        report: None,
                        error: Some(format!("failed to spawn loudness thread: {e}")),
                        cancelled: false,
                    });
                }
            }
            // CancelExport: raise the flag the freewheel loop polls. No-op
            // when no export is running (the next run clears it on entry).
            Ok(AudioCommand::CancelExport) => {
                engine_shared
                    .export_cancel
                    .store(true, Ordering::Release);
                tracing::info!("received CancelExport; offline render will abort");
            }
            Ok(AudioCommand::BounceClipFxOnline {
                path,
                source_track,
                source_clip,
                start_beat,
                end_beat,
                warm,
            }) => {
                // Reserve the engine (same atomic reservation as ExportWav —
                // bounce and WAV export share EngineShared / the CPAL-silence
                // flag and must not run concurrently). run_export no longer sets
                // export_running, so the bounce path must reserve it here too,
                // otherwise the CPAL callback wouldn't be silenced during the
                // bounce render. The closure / early-outs release it.
                if engine_shared
                    .export_running
                    .compare_exchange(
                        false,
                        true,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    tracing::warn!("BounceClipFxOnline received while a render is already running; ignoring");
                    let _ = out_tx.send(AudioEvent::BounceClipFxComplete {
                        path,
                        source_track,
                        source_clip,
                        error: Some("export already in progress".into()),
                        frames: 0,
                    });
                    continue;
                }
                let song_snap = shared.song.load();
                let Some(song_arc) = song_snap.as_ref() else {
                    tracing::warn!("BounceClipFxOnline received but no song loaded");
                    engine_shared
                        .export_running
                        .store(false, Ordering::Release);
                    let _ = out_tx.send(AudioEvent::BounceClipFxComplete {
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
                // Clear stale cancel (FIFO-ordered with CancelExport), same as
                // the ExportWav path. The bounce itself has no Cancel UI, but
                // this prevents a leftover cancel from a prior aborted export
                // from killing the bounce render on its first buffer.
                engine_shared
                    .export_cancel
                    .store(false, Ordering::Release);
                let engine_shared_clone = Arc::clone(&engine_shared);
                let engine_shared_release = Arc::clone(&engine_shared);
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
                                tracing::error!(
                                    error = ?e,
                                    "offline plugin-FX bounce failed"
                                );
                                (Some(format!("{e:#}")), 0)
                            }
                        };
                        let _ = out_tx_clone.send(AudioEvent::BounceClipFxComplete {
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
                    engine_shared
                        .export_running
                        .store(false, Ordering::Release);
                    let _ = out_tx.send(AudioEvent::BounceClipFxComplete {
                        path,
                        source_track,
                        source_clip,
                        error: Some(format!("failed to spawn bounce thread: {e}")),
                        frames: 0,
                    });
                }
            }
            // (r.md #61) daw_gui の終了シーケンスからの正常終了要求。
            // 親 crash の pipe EOF (下の `Err` 枝) と同じ出口へ合流し、
            // 呼び出し元 `main` が stream / notify thread を畳む。
            Ok(AudioCommand::Shutdown) => {
                tracing::info!("received Shutdown");
                break;
            }
            Err(e) => {
                tracing::info!(error = ?e, "receive loop ending");
                break;
            }
        }
    }
}

/// Open the WorkerBridge shmem + N (wake, done) named events for the audio
/// side, spawn the audio worker pool, and bundle everything into a
/// [`WorkerRig`]. Runs on the receive loop (off-thread) — thread spawn /
/// event creation never touches the CPAL callback (plan §4)。 event 名は
/// daw_gui が世代込みで mint した opaque な文字列 (`worker_wake_event_name`)
/// をそのまま使う — pool 再構築時に旧世代の stale signal が新 pool へ漏れない。
fn build_worker_rig(
    n_workers: u32,
    worker_bridge_shmem_id: &str,
    wake_event_names: &[String],
    done_event_names: &[String],
) -> Result<WorkerRig> {
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
    // Per-slot pointer into the bridge's worker_task array — the mapping's
    // address is stable for the bridge handle's lifetime, which the rig owns
    // (moving the handle struct does not move the mapped view).
    let mut slots = Vec::with_capacity(n_workers as usize);
    for i in 0..n_workers as usize {
        let wake = common::plugin_ref::create_named_event(&wake_event_names[i])
            .with_context(|| format!("failed to open wake event {i}"))?;
        let done = common::plugin_ref::create_named_event(&done_event_names[i])
            .with_context(|| format!("failed to open done event {i}"))?;
        slots.push(SyncSlot {
            sync: common::plugin_ref::WorkerSyncRef {
                worker_idx: i as u32,
                worker_task: &bridge.bridge().worker_task[i] as *const _,
                event_wake: wake,
                event_done: done,
            },
            poisoned: std::sync::atomic::AtomicBool::new(false),
        });
    }
    // Spawn the audio-engine worker pool sized to the sync slots (master owns
    // slot 0, worker i owns slot i+1). 失敗しても handshake 面は生かして
    // serial fallback (slot 0 のみ) で動かす。
    let pool = match audio_worker::AudioWorkerPool::new(n_workers) {
        Ok(pool) => Some(pool),
        Err(e) => {
            tracing::error!(error = ?e, "AudioWorkerPool::new failed; serial fallback");
            None
        }
    };
    Ok(WorkerRig {
        pool,
        slots,
        bridge,
        stall_notified: std::sync::atomic::AtomicBool::new(false),
    })
}

#[allow(clippy::too_many_arguments)]
fn start_output_stream(
    shared: Arc<SharedState>,
    engine_shared: Arc<EngineShared>,
    bridge: Arc<AudioBridgeHandle>,
    metrics: Arc<MetricsBridgeHandle>,
    scope: Arc<ScopeBridgeHandle>,
    session_sample_rate: u32,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<EngineCommand>,
    bundle_rx: rtrb::Consumer<RtBundle>,
    bundle_recycle_tx: rtrb::Producer<RtBundle>,
    stretch_pool_rx: rtrb::Consumer<engine::StretchPoolDelivery>,
    stretch_pool_recycle_tx: rtrb::Producer<engine::StretchPoolDelivery>,
) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default output device")?;
    // cpal 0.17: `name()` は deprecated — `description()` (name + 種別) を使う。
    let device_name = device
        .description()
        .map(|d| d.to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    let supported = device
        .default_output_config()
        .context("failed to query default output config")?;

    let sample_rate = supported.sample_rate();
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
        metrics,
        scope,
        session_sample_rate,
        cmd_rx,
        bundle_rx,
        bundle_recycle_tx,
        stretch_pool_rx,
        stretch_pool_recycle_tx,
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
    metrics: Arc<MetricsBridgeHandle>,
    scope: Arc<ScopeBridgeHandle>,
    session_sample_rate: u32,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<EngineCommand>,
    bundle_rx: rtrb::Consumer<RtBundle>,
    bundle_recycle_tx: rtrb::Producer<RtBundle>,
    stretch_pool_rx: rtrb::Consumer<engine::StretchPoolDelivery>,
    stretch_pool_recycle_tx: rtrb::Producer<engine::StretchPoolDelivery>,
) -> Result<cpal::Stream> {
    let channels_usize = channels as usize;
    let max_frames = common::process_data::MAX_FRAMES;
    // `LocalState` is the CPAL closure's exclusive heap. It holds
    // master_l/r and the per-track scratch — pre-allocated here, never
    // touched outside the audio thread.
    let mut local = LocalState::new(
        max_frames,
        cmd_rx,
        engine_shared,
        bundle_rx,
        bundle_recycle_tx,
        stretch_pool_rx,
        stretch_pool_recycle_tx,
    );

    // panic-button master declick. `AudioCommand::Panic` sets
    // `shared.panic_declick`; the callback consumes that edge and fades the
    // master out, then HOLDS it at zero until `AudioCommand::PanicRelease`
    // (`shared.panic_release`) arrives — which daw_gui sends only once the
    // plugin host has actually finished `ReinitAllPlugins` (reply
    // `PluginsReinitDone`). Holding until the real reinit completion (rather
    // than a fixed timer) means a stalled GUI main thread or a slow/large
    // reinit can never un-mute the master while plugins are still ringing in
    // the mix (the step-discontinuity click / re-exposed reverb tail this whole
    // mechanism exists to prevent). A `declick_max_hold` safety auto-releases if
    // the reply never comes (plugin-host hang) so the master can't get stuck.
    //
    // `declick_t` = samples since the envelope started (`None` = inactive).
    // `declick_released_at` = the `declick_t` at which the fade-in began
    // (`None` = still fading out / holding). Durations derive from the sample rate.
    let sr64 = u64::from(session_sample_rate);
    let declick_fade_out = (sr64 * 5 / 1000).max(1); // 5 ms
    let declick_fade_in = (sr64 * 20 / 1000).max(1); // 20 ms
    let declick_max_hold = sr64 * 2; // 2 s safety: auto-release if no reply
    let mut declick_t: Option<u64> = None;
    let mut declick_released_at: Option<u64> = None;
    // resource monitor (r.md #3): DSP load average の EMA 状態。 callback 間で保持。
    let mut dsp_load_ema: f32 = 0.0;
    // r.md #49: アイドル park に入るまでの連続無音サンプル数。
    let idle_park_samples = sr64 * engine::IDLE_PARK_DELAY_SECS;
    // E (plan §5): callback thread を MMCSS "Pro Audio" に自前 join する
    // one-shot フラグ (per-thread once — CPAL は単一 stream thread で callback
    // を直列に呼ぶ)。
    let mut mmcss_tried = false;

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                if !mmcss_tried {
                    mmcss_tried = true;
                    // E (plan §5): RT 優先度ポリシーの SSoT を自プロセスに持つ
                    // (cpal 0.17 の内部対応と独立に自前 join)。 join handle は
                    // callback thread の寿命 = stream の寿命なので forget で
                    // 保持 (revert は thread 終了時に OS 側で行われる)。 失敗は
                    // フラグに立て、 notify thread が 1 回だけ warn する
                    // (RT では tracing 禁止)。
                    match common::mmcss::join_pro_audio() {
                        Some(join) => std::mem::forget(join),
                        None => local
                            .shared
                            .mmcss_join_failed
                            .store(true, Ordering::Release),
                    }
                }
                // resource monitor (r.md #3): callback 全体の処理時間を測る。
                // plugin 処理は worker pool でブロッキング同期されるため、 この
                // 区間に plugin 負荷が含まれる。 `Instant::now()` は RT 許容。
                let cb_start = std::time::Instant::now();
                let frames = (data.len() / channels_usize).min(max_frames);

                local.process_buffer(&shared, &bridge, &scope, session_sample_rate, frames);

                // A2: publish the engine's playhead to shmem so the GUI
                // can draw the cursor. 停止中も現在の playhead をそのまま
                // publish する (= ruler click で動かした位置や、 Stop 直前
                // の位置を GUI に反映、 業界標準の挙動)。 `u64::MAX` は
                // bridge の未初期化値 (= audio thread が一度も書いてない
                // 状態) として残す。
                let published_ph = shared.playhead.load(Ordering::Acquire);
                bridge.set_playhead_samples(published_ph);

                // consume the panic edge to (re)start the master
                // declick envelope at sample 0 of this buffer.
                if shared.panic_declick.swap(false, Ordering::AcqRel) {
                    declick_t = Some(0);
                    declick_released_at = None;
                }
                // Once the fade-out is done and we're holding at zero, release
                // (begin the fade-in) when daw_gui signals the reinit finished,
                // or when the safety hold cap is hit (reply never arrived).
                if let Some(t) = declick_t
                    && declick_released_at.is_none()
                    && t >= declick_fade_out
                    && (shared.panic_release.swap(false, Ordering::AcqRel)
                        || t >= declick_fade_out + declick_max_hold)
                {
                    declick_released_at = Some(t);
                }

                // Interleave master_l/r into the device buffer, applying
                // the panic declick envelope when active (master gain は
                // render_master_buffer 内で適用済み — live/export 統一 §5)。
                // Lanes beyond stereo on the device are zeroed.
                unsafe {
                    let dst = data.as_mut_ptr();
                    for i in 0..frames {
                        let dg = match declick_t {
                            Some(t) => panic_declick_gain(
                                t + i as u64,
                                declick_fade_out,
                                declick_fade_in,
                                declick_released_at,
                            ),
                            None => 1.0,
                        };
                        let l = local.master_l[i] * dg;
                        let r = local.master_r[i] * dg;
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
                // Advance the envelope; clear it once the fade-in has finished
                // so the master returns to full gain.
                if let Some(t) = declick_t {
                    let next = t + frames as u64;
                    declick_t = match declick_released_at {
                        Some(r) if next >= r + declick_fade_in => None,
                        _ => Some(next),
                    };
                }
                let filled = frames * channels_usize;
                for s in &mut data[filled..] {
                    *s = 0.0;
                }

                // r.md #49 のアイドル判定用「実際にデバイスへ出た音の無音判定」。
                // r.md #50 でメーター表示の測定点は `render_master_buffer` 直後
                // (= メトロノーム前) へ移したが、park してよいかは**スピーカーへ
                // 出ている音**で決めなければならない (メトロノームが鳴っている
                // 間に park すると click が切れる)。目的が違うので共有しない。
                let (peak_l, peak_r) = block_peaks_stereo(data, channels_usize);

                // resource monitor (r.md #3): DSP load を publish。 load =
                // 処理時間 ÷ バッファ周期。 peak は直近窓の worst-case (GUI が
                // swap でリセット)、 avg は EMA。 load>1.0 は dropout として記録。
                let elapsed = cb_start.elapsed().as_secs_f32();
                let load =
                    common::metrics_bridge::dsp_load(elapsed, frames as u32, session_sample_rate);
                metrics.observe_dsp_load_peak(load);
                dsp_load_ema = common::metrics_bridge::ema(dsp_load_ema, load, 0.1);
                metrics.set_dsp_load_avg(dsp_load_ema);
                metrics.set_buffer_info(frames as u32, session_sample_rate);
                // xrun は再生中のみカウントする。 停止中 (無音) の callback 処理時間
                // スパイク (起動直後の cold start / OS scheduling jitter) は実際の
                // 音切れではないため除外する (`local.playing` = 今 buffer が rolling か)。
                if load > 1.0 && local.playing {
                    metrics.add_xrun();
                }

                // r.md #49: アイドル park の判定。atomic の読み書きだけなので RT 安全。
                //
                // 「無音」を条件に含めているので、リバーブの残響や自走プラグイン
                // (VCV Rack 等) が鳴っている間はカウンタが進まない = 音が途中で
                // ブツッと切れることは構造的に起きない。
                //
                // **コールバックの最後に置くこと** — publish を 0 に畳む処理が、
                // 上の meters / DSP load publish に上書きされてはならない。
                let idle = engine::buffer_is_idle(
                    shared.app_active.load(Ordering::Acquire),
                    local.playing,
                    local.shared.preroll_remaining_samples.load(Ordering::Acquire),
                    local.shared.export_running.load(Ordering::Acquire),
                    peak_l,
                    peak_r,
                );
                let idle_n = engine::advance_idle_counter(
                    &shared.idle_silent_samples,
                    idle,
                    frames as u64,
                );
                if idle_n >= idle_park_samples {
                    if !shared.park_requested.swap(true, Ordering::AcqRel) {
                        // park に入る前に「動くもの」を 0 で publish しておく。
                        // publish が止まった後も GUI は最後の値を読み続けるので、
                        // これが無いと止まったメーターが点灯したまま凍結する。
                        //
                        // mod scalars は**ゼロにしない** — メーターではなく
                        // パラメータ値なので、最後の値のまま凍結するのが正しい
                        // (ゼロにすると画像 / 映像効果の見た目が飛ぶ)。
                        //
                        // r.md #50 のマスターメーターはここで何もしない: 解析器は
                        // 「新しいフレームが来なかった経過時間ぶんの無音」を自分で
                        // 流し込んで落ちるので、書き手側の後始末が要らない。
                        for t in 0..common::audio_bridge::MAX_TRACKS {
                            bridge.set_track_peak(t, 0.0, 0.0);
                        }
                        dsp_load_ema = 0.0;
                        metrics.set_dsp_load_avg(0.0);
                    }
                    // park 中は dispatch していないのが事実なので `live_parked` を
                    // 立てる。これが無いと書き出しのたびに `export.rs` の
                    // 「live callback が park するまで最大 2 秒待つ」を踏む。
                    local.shared.live_parked.store(true, Ordering::Release);
                } else if shared.park_requested.load(Ordering::Acquire) {
                    // 条件が崩れた (= resume 済み or 音が鳴り始めた)。要求を取り下げ
                    // れば notify thread の reconciler が stream を起こす。
                    //
                    // `swap` でなく load してから store するのは、定常状態
                    // (park 要求が無い) で **書き込みを一切出さない**ため。毎バッファ
                    // RMW すると notify thread と共有するキャッシュラインを 10ms ごとに
                    // 汚す。
                    shared.park_requested.store(false, Ordering::Release);
                    local.shared.live_parked.store(false, Ordering::Release);
                }
            },
            |err| tracing::error!(?err, "audio stream error"),
            None,
        )
        .context("failed to build output stream")?;
    Ok(stream)
}

/// master gain multiplier for the panic declick envelope at sample
/// offset `t` (samples since the panic was armed):
/// `fade_out` (1 → 0) → hold (0, until `released_at` is set) → fade-in (0 → 1
/// over `fade_in`, starting at `released_at`) → done (1). The master is faded to
/// silence *before* `ReinitAllPlugins` yanks every plugin out of the mix (so the
/// step discontinuity is masked) and HELD there until `released_at` is set — by
/// the caller when daw_gui confirms the reinit actually completed — so the
/// un-mute can never happen while plugins are still ringing in the mix. RT-safe:
/// branch + one division, no allocation.
fn panic_declick_gain(t: u64, fade_out: u64, fade_in: u64, released_at: Option<u64>) -> f32 {
    if t < fade_out {
        return 1.0 - t as f32 / fade_out as f32; // fade-out
    }
    match released_at {
        // Holding at zero until release.
        None => 0.0,
        Some(r) if t < r => 0.0,
        // Fade back in from the release point.
        Some(r) => {
            let u = t - r;
            if u < fade_in {
                u as f32 / fade_in as f32
            } else {
                1.0
            }
        }
    }
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

    // panic declick envelope (fade_out=4, fade_in=4), hold-until-release.
    #[test]
    fn panic_declick_envelope_phases() {
        let (fo, fi) = (4u64, 4u64);
        // fade-out: 1.0 at t=0, linearly to 0 at t=fo.
        assert_eq!(panic_declick_gain(0, fo, fi, None), 1.0);
        assert!((panic_declick_gain(2, fo, fi, None) - 0.5).abs() < 1e-6);
        // hold (not released): zero forever, however long t grows.
        assert_eq!(panic_declick_gain(fo, fo, fi, None), 0.0);
        assert_eq!(panic_declick_gain(10_000, fo, fi, None), 0.0);
        // released at t=20: still zero before the release point, then fade-in.
        let r = Some(20);
        assert_eq!(panic_declick_gain(19, fo, fi, r), 0.0);
        assert_eq!(panic_declick_gain(20, fo, fi, r), 0.0);
        assert!((panic_declick_gain(22, fo, fi, r) - 0.5).abs() < 1e-6);
        // done: full gain once the fade-in completes.
        assert_eq!(panic_declick_gain(24, fo, fi, r), 1.0);
        assert_eq!(panic_declick_gain(10_000, fo, fi, r), 1.0);
    }

    /// BundlePublisher の drop-oldest: ring が full のとき新しい bundle が
    /// park され (最新優先、 superseded parked は off-thread drop)、 space が
    /// できたら flush で届く。
    #[test]
    fn bundle_publisher_parks_newest_on_full_ring() {
        let (tx, mut rx) = rtrb::RingBuffer::<RtBundle>::new(1);
        let mut publisher = BundlePublisher::new(tx);
        let bundle = || RtBundle {
            song: None,
            tempo_map: common::tempo_map::TempoMap::from_song(
                &common::model::Song::default(),
            ),
            schedule: None,
            reset_song_scoped_state: false,
            input_delay_replacements: Vec::new(),
            plugin_refs: Arc::new(std::collections::HashMap::new()),
            worker: None,
            mod_plan: None,
            mod_phase_table: None,
        };
        publisher.send(bundle()); // fills the 1-slot ring
        publisher.send(bundle()); // full → parked
        publisher.send(bundle()); // full → parked (previous parked dropped here)
        assert!(publisher.parked.is_some());
        // consumer drains one slot → flush delivers the parked newest.
        assert!(rx.pop().is_ok());
        publisher.flush();
        assert!(publisher.parked.is_none());
        assert!(rx.pop().is_ok());
    }
}
