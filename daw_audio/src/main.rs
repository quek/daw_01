// release ではコンソール窓を出さない (windows-subsystem)。 debug は console の
// まま (standalone 起動時に stdout/tracing が見える)。 docs/plan_icon_and_console.md (#48)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
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
use common::protocol::{AudioCommand, AudioEvent, DeviceAddr, ProjectKey};
use common::scope_bridge::ScopeBridgeHandle;
use common::wire::{read_msg, write_msg};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::io::ReadHalf;
use tokio::net::windows::named_pipe::NamedPipeClient;

mod audio_clip_renderer;
mod audio_worker;
mod automation;
mod engine;
mod engine_shared;
mod export;
mod graph;
mod launcher;
mod metronome;
mod mixer;
mod mod_plan_publish;
mod mod_tick;
mod offline_jobs;
mod project_ctl;
mod sampler;
mod sequencer;
mod song_values;
mod stretch_engine;

use engine::{
    DeviceBundle, DeviceRt, EngineCommand, EngineShared, PluginEntry, ProjectDelivery, ProjectRt,
    SharedState, SyncSlot, WorkerRig,
};
use mod_plan_publish::ModPhaseTableBuilder;
use project_ctl::{DecodeJob, ProjectCtl};

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

/// project slot の生成 / 撤去便の ring 深さ (タブの開閉は人の速度)。
const PROJECT_RING_CAP: usize = engine::MAX_PROJECTS;
/// デバイス全体 snapshot (worker / sampler) の ring 深さ。
const DEVICE_RING_CAP: usize = 8;

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
    let engine_shared = Arc::new(EngineShared::new());

    // Preview / launcher channel: the receive loop pushes light commands here;
    // the audio thread drains it at the top of every buffer. shmem / worker
    // pool の重い扱いは bundle ring 経由 (plan §4)。
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<EngineCommand>();

    // plan §4 / `docs/plan_project_tabs.md` §3.2: wait-free SPSC pairs for RT
    // delivery. project slot は `ProjectDelivery` で開閉し、撤去した `ProjectRt` は
    // recycle ring で戻って off-thread で drop される。デバイス全体 snapshot
    // (worker rig / sampler ring) は `DeviceBundle` で届き、旧値も off-thread drop。
    // project ごとの `RtBundle` ring は `project_ctl::open_project` が作る。
    let (project_tx, project_rx) = rtrb::RingBuffer::<ProjectDelivery>::new(PROJECT_RING_CAP);
    let (project_recycle_tx, project_recycle_rx) =
        rtrb::RingBuffer::<Box<ProjectRt>>::new(PROJECT_RING_CAP);
    let (device_tx, device_rx) = rtrb::RingBuffer::<DeviceBundle>::new(DEVICE_RING_CAP);
    let (device_recycle_tx, device_recycle_rx) =
        rtrb::RingBuffer::<DeviceBundle>::new(DEVICE_RING_CAP);

    let stream = start_output_stream(
        Arc::clone(&shared),
        Arc::clone(&engine_shared),
        Arc::clone(&bridge),
        Arc::clone(&metrics),
        Arc::clone(&scope),
        session.sample_rate,
        cmd_rx,
        project_rx,
        project_recycle_tx,
        device_rx,
        device_recycle_tx,
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
        let sr = session.sample_rate;
        std::thread::Builder::new()
            .name("audio-decode".to_string())
            .spawn(move || project_ctl::decode_worker_loop(decode_rx, sr))
            .context("failed to spawn audio decode worker")?;
    }

    recv_loop(read_half, RecvLoop {
        shared,
        engine_shared: Arc::clone(&engine_shared),
        bridge: Arc::clone(&bridge),
        session_sample_rate: session.sample_rate,
        cmd_tx,
        out_tx,
        decode_tx,
        project_tx,
        project_recycle_rx,
        device_publisher: DevicePublisher { tx: device_tx, parked: None },
        device_recycle_rx,
        park: Arc::clone(&park),
    })
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
                notify_quarantined_devices(&engine_shared, &out_tx);
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

/// dispatch timeout → quarantine された device を 1 回だけ通知する (全 project)。
fn notify_quarantined_devices(
    engine_shared: &EngineShared,
    out_tx: &tokio::sync::mpsc::UnboundedSender<AudioEvent>,
) {
    let projects = engine_shared.projects.load();
    for (key, project) in projects.iter() {
        let refs = project.plugin_refs.load();
        for (id, entry) in refs.iter() {
            if !entry.quarantined.load(Ordering::Acquire)
                || entry.unresponsive_notified.swap(true, Ordering::AcqRel)
            {
                continue;
            }
            tracing::warn!(
                project = key.0,
                device_id = *id,
                "plugin unresponsive (dispatch timeout); quarantined"
            );
            let _ = out_tx.send(AudioEvent::PluginUnresponsive {
                device: DeviceAddr::new(*key, *id),
            });
        }
    }
}

/// デバイス全体 snapshot (`DeviceBundle`) の送出。worker / sampler は snapshot field
/// なので畳み込みは不要 — ring full なら最新を park して次の周回で再送する。
struct DevicePublisher {
    tx: rtrb::Producer<DeviceBundle>,
    parked: Option<DeviceBundle>,
}

impl DevicePublisher {
    fn flush(&mut self) {
        if let Some(b) = self.parked.take()
            && let Err(rtrb::PushError::Full(back)) = self.tx.push(b)
        {
            self.parked = Some(back);
        }
    }

    /// 現在のミラー (worker / sampler) を RT へ送る。
    fn publish(&mut self, engine_shared: &EngineShared) {
        self.flush();
        let bundle = DeviceBundle {
            worker: engine_shared.worker.load_full(),
            sampler: engine_shared.sampler.load_full(),
        };
        if let Err(rtrb::PushError::Full(newest)) = self.tx.push(bundle) {
            // 旧 parked は superseded (snapshot) なのでここ (off-thread) で drop。
            self.parked = Some(newest);
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
pub(crate) fn hold_released_entry_for_test(entry: Option<Arc<PluginEntry>>) {
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
pub(crate) fn hold_released_entry_for_test(_entry: Option<Arc<PluginEntry>>) {}

/// recv loop の所有物 (引数が多いので束ねる)。pipe の read half は別に持つ —
/// `read_msg` の future が借りている間も housekeeping がこちらを `&mut` で使えるように。
struct RecvLoop {
    shared: Arc<SharedState>,
    engine_shared: Arc<EngineShared>,
    bridge: Arc<AudioBridgeHandle>,
    session_sample_rate: u32,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<EngineCommand>,
    out_tx: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
    decode_tx: std::sync::mpsc::Sender<DecodeJob>,
    project_tx: rtrb::Producer<ProjectDelivery>,
    project_recycle_rx: rtrb::Consumer<Box<ProjectRt>>,
    device_publisher: DevicePublisher,
    device_recycle_rx: rtrb::Consumer<DeviceBundle>,
    park: Park,
}

/// recv loop の周期処理 (メッセージ処理とは独立に走る)。
///
/// plan §4: dispose bundles the audio thread superseded, so their `Drop`
/// (free / shmem unmap / worker pool join / `ProjectRt` の解体) runs here, off the
/// audio callback. parked bundle (ring full 時の drop-oldest 退避) も再送する。
///
/// **メッセージ到着に依存させない**のが load-bearing: 旧実装はこれを
/// `read_msg().await` の手前 1 箇所でしか走らせておらず、「次の AudioCommand が
/// 来なければ superseded bundle は永久に解放されない」= 編集を止めた瞬間に
/// shmem mapping / worker rig / Song snapshot が無期限に居座る状態だった
/// (解放時刻に上限が無い)。[`HOUSEKEEPING_INTERVAL`] のタイマ枝から同じ処理を
/// 呼ぶことで上限を与える。RT 側は無変更 (push のみ)。
fn recv_loop_housekeeping(
    rl: &mut RecvLoop,
    projects: &mut HashMap<ProjectKey, ProjectCtl>,
    phase_tables: &ModPhaseTableBuilder,
) {
    project_ctl::reap_closed_projects(&mut rl.project_recycle_rx, &rl.bridge);
    while let Ok(old) = rl.device_recycle_rx.pop() {
        drop(old);
    }
    rl.device_publisher.flush();
    let mut finished = phase_tables.take_finished();
    for ctl in projects.values_mut() {
        let key = ctl.key();
        let table = finished
            .iter()
            .position(|(k, _)| *k == key)
            .map(|i| finished.swap_remove(i).1);
        ctl.housekeeping(&rl.engine_shared, rl.session_sample_rate, phase_tables, table);
    }
}

async fn recv_loop(mut pipe: ReadHalf<NamedPipeClient>, mut rl: RecvLoop) {
    let mut projects: HashMap<ProjectKey, ProjectCtl> = HashMap::new();
    // r.md #89: 位相表を張る専用スレッド (project ごとに最新の要求だけ残す郵便受け)。
    let phase_tables = ModPhaseTableBuilder::spawn();
    let mut housekeeping = tokio::time::interval(HOUSEKEEPING_INTERVAL);
    // 遅延して詰まった tick を burst で取り戻さない (drain は冪等なので
    // 取り戻す意味が無く、CPU を無駄に食うだけ)。
    housekeeping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        recv_loop_housekeeping(&mut rl, &mut projects, &phase_tables);
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
                    _ = housekeeping.tick() => {
                        recv_loop_housekeeping(&mut rl, &mut projects, &phase_tables);
                    }
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
            wake_stream(&rl.park, &rl.shared, &rl.engine_shared);
        }
        let cmd = match msg {
            Ok(cmd) => cmd,
            Err(e) => {
                tracing::info!(error = ?e, "receive loop ending");
                break;
            }
        };
        match cmd {
            // Handshake 済みの再送 Ack / Session は no-op (Session は起動時に
            // `read_audio_session` が消費済み — shmem 名と format はプロセス
            // 生存中不変)。
            AudioCommand::Ack | AudioCommand::Session(_) => {}
            // r.md #49: アプリの窓がアクティブかの報告。park してよいかの判断は
            // engine 側 (`buffer_is_idle`) が行うので、ここは事実の反映のみ。
            AudioCommand::SetAppActive(active) => {
                rl.shared.app_active.store(active, Ordering::Release);
                if active {
                    // 猶予カウンタを畳んでおく (非アクティブ→アクティブ→非アクティブ
                    // と往復したとき、前回の途中まで数えた分から再開しない)。
                    rl.shared.idle_silent_samples.store(0, Ordering::Release);
                }
            }
            AudioCommand::Panic => {
                // arm the master declick. The CPAL callback consumes
                // this edge flag, fades the master out and holds at zero until
                // `PanicRelease`, so the imminent `ReinitAllPlugins` (which yanks
                // every plugin out of the mix) doesn't produce a step click.
                tracing::info!("received Panic (master declick)");
                rl.shared.panic_declick.store(true, Ordering::Release);
            }
            AudioCommand::PanicRelease => {
                // the plugin reinit finished — release the declick
                // hold so the master fades back in over a now-silent mix.
                tracing::info!("received PanicRelease (declick fade-in)");
                rl.shared.panic_release.store(true, Ordering::Release);
            }
            // `docs/plan_project_tabs.md` §3.3: project slot の開閉。
            AudioCommand::OpenProject { project } => {
                project_ctl::open_project(
                    project,
                    &mut projects,
                    &rl.engine_shared,
                    &rl.bridge,
                    &mut rl.project_tx,
                );
            }
            AudioCommand::CloseProject { project } => {
                project_ctl::close_project(project, &mut projects, &rl.engine_shared, &mut rl.project_tx);
            }
            AudioCommand::SetScopeProject { project } => {
                rl.engine_shared.scope_project.store(project.0, Ordering::Release);
                tracing::info!(project = project.0, "scope project (active tab) updated");
            }
            AudioCommand::OpenWorkerPool {
                n_workers,
                worker_bridge_shmem_id,
                wake_event_names,
                done_event_names,
            } => {
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
                        rl.engine_shared.worker.store(Some(Arc::new(rig)));
                        rl.device_publisher.publish(&rl.engine_shared);
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "failed to open audio-side worker pool");
                    }
                }
            }
            AudioCommand::CloseWorkerPool => {
                rl.engine_shared.worker.store(None);
                rl.device_publisher.publish(&rl.engine_shared);
            }
            // Global Sampler (`docs/plan_global_sampler.md` §3.2): リングの open と試聴。
            cmd @ (AudioCommand::OpenSamplerRing { .. }
            | AudioCommand::SamplerPreview { .. }
            | AudioCommand::SamplerPreviewStop) => {
                if sampler::handle_device_command(cmd, &rl.engine_shared, &rl.cmd_tx) {
                    rl.device_publisher.publish(&rl.engine_shared);
                }
            }
            // CancelExport: raise the flag the freewheel loop polls. No-op
            // when no export is running (the next run clears it on entry).
            AudioCommand::CancelExport => {
                rl.engine_shared.export_cancel.store(true, Ordering::Release);
                tracing::info!("received CancelExport; offline render will abort");
            }
            // (r.md #61) daw_gui の終了シーケンスからの正常終了要求。
            // 親 crash の pipe EOF (上の `Err` 枝) と同じ出口へ合流し、
            // 呼び出し元 `main` が stream / notify thread を畳む。
            AudioCommand::Shutdown => {
                tracing::info!("received Shutdown");
                break;
            }
            // オフライン描画 (エンジン全体で 1 本)。材料は対象 project から。
            AudioCommand::ExportWav { project, path, range, write_mod_sidecar } => {
                let Some(ctl) = projects.get(&project) else {
                    tracing::warn!(project = project.0, "ExportWav for an unknown project; ignored");
                    continue;
                };
                offline_jobs::export_wav(
                    &rl.engine_shared,
                    &ctl.shared,
                    rl.session_sample_rate,
                    &rl.out_tx,
                    path,
                    range,
                    write_mod_sidecar,
                );
            }
            AudioCommand::AnalyzeLoudness { project, range } => {
                let Some(ctl) = projects.get(&project) else {
                    tracing::warn!(project = project.0, "AnalyzeLoudness for an unknown project; ignored");
                    continue;
                };
                offline_jobs::analyze_loudness(
                    &rl.engine_shared,
                    &ctl.shared,
                    rl.session_sample_rate,
                    &rl.out_tx,
                    range,
                );
            }
            AudioCommand::BounceClipFxOnline {
                project,
                path,
                source_track,
                source_clip,
                start_beat,
                end_beat,
                warm,
            } => {
                let Some(ctl) = projects.get(&project) else {
                    tracing::warn!(project = project.0, "BounceClipFxOnline for an unknown project; ignored");
                    continue;
                };
                offline_jobs::bounce_clip_fx(
                    &rl.engine_shared,
                    &ctl.shared,
                    rl.session_sample_rate,
                    &rl.out_tx,
                    path,
                    source_track,
                    source_clip,
                    start_beat,
                    end_beat,
                    warm,
                );
            }
            // それ以外は全部 project 宛 (`AudioCommand::project` が SSoT)。閉じた
            // タブへの遅延 command は捨てる。
            cmd => {
                let Some(key) = cmd.project() else {
                    tracing::warn!(?cmd, "device-level command without a handler");
                    continue;
                };
                let Some(ctl) = projects.get_mut(&key) else {
                    tracing::debug!(project = key.0, ?cmd, "command for a closed project; dropped");
                    continue;
                };
                project_ctl::handle_project_command(
                    ctl,
                    cmd,
                    &rl.engine_shared,
                    rl.session_sample_rate,
                    &rl.cmd_tx,
                    &rl.decode_tx,
                    &phase_tables,
                );
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
    project_rx: rtrb::Consumer<ProjectDelivery>,
    project_recycle_tx: rtrb::Producer<Box<ProjectRt>>,
    device_rx: rtrb::Consumer<DeviceBundle>,
    device_recycle_tx: rtrb::Producer<DeviceBundle>,
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
    let local = DeviceRt::new(
        common::process_data::MAX_FRAMES,
        cmd_rx,
        engine_shared,
        project_rx,
        project_recycle_tx,
        device_rx,
        device_recycle_tx,
    );
    let stream = build_stream(
        &device,
        &config,
        channels,
        shared,
        bridge,
        metrics,
        scope,
        session_sample_rate,
        local,
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
    bridge: Arc<AudioBridgeHandle>,
    metrics: Arc<MetricsBridgeHandle>,
    scope: Arc<ScopeBridgeHandle>,
    session_sample_rate: u32,
    // `DeviceRt` is the CPAL closure's exclusive heap. It holds
    // master_l/r and the per-project scratch — pre-allocated, never
    // touched outside the audio thread.
    mut local: DeviceRt,
) -> Result<cpal::Stream> {
    let channels_usize = channels as usize;
    let max_frames = common::process_data::MAX_FRAMES;

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
                // 音切れではないため除外する (どれか 1 project でも rolling なら再生中)。
                let any_playing = local.any_playing();
                if load > 1.0 && any_playing {
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
                // 複数 project では「どれか 1 つでも走っている / count-in 中」で判定する。
                let idle = engine::buffer_is_idle(
                    shared.app_active.load(Ordering::Acquire),
                    local.any_rolling(),
                    0,
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
                        for p in &local.projects {
                            bridge.project(p.telemetry_slot).clear_track_meters();
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
}
