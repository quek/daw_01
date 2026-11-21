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
use tokio::io::ReadHalf;
use tokio::net::windows::named_pipe::NamedPipeClient;

// r.md #156: ASIO ドライバーを SDK 無しで直接呼ぶ層 (64 bit Windows 専用、`asio` の module doc)。
#[cfg(all(windows, target_pointer_width = "64"))]
mod asio;
mod audio_clip_renderer;
mod audio_rec;
mod audio_worker;
mod automation;
mod device;
#[cfg(test)]
// r.md #129: 内蔵 device DSP の golden (`native_dsp/golden_v38.txt`) の刺激・統計・形式。
// 比較は `native_dsp::tests`。
mod dsp_golden;
mod engine;
mod engine_shared;
mod export;
mod graph;
mod launcher;
mod metronome;
mod mixer;
mod mod_plan_publish;
mod mod_tick;
mod native_dsp;
mod note_ledger;
mod offline_jobs;
mod output_render;
mod project_ctl;
mod sampler;
mod sequencer;
mod song_values;
mod stretch_engine;

use engine::{
    DeviceBundle, DeviceRt, EngineCommandSender, EngineShared, PluginEntry,
    ProjectDelivery, ProjectRt, SharedState, SyncSlot, WorkerRig,
};
use mod_plan_publish::ModPhaseTableBuilder;
use project_ctl::{DecodeJob, ProjectCtl};

/// project slot の生成 / 撤去便の ring 深さ (タブの開閉は人の速度)。
const PROJECT_RING_CAP: usize = engine::MAX_PROJECTS;
/// デバイス全体 snapshot (worker / sampler) の ring 深さ。
const DEVICE_RING_CAP: usize = 8;

#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = common::logging::init_tracing_for("daw_audio");
    tracing::info!("daw_audio started");

    // `--asio-probe [名前]`: ASIO 層の実測だけして終わる (IPC を張らない、`asio::probe`)。
    #[cfg(all(windows, target_pointer_width = "64"))]
    if let Some(code) = asio::probe::run_from_args() {
        std::process::exit(code);
    }

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    // Hello には PROTOCOL_FINGERPRINT が同梱され、親がビルド世代を検証する (§3)。
    let mut pipe = common::client::perform_audio_handshake(&pipe_name).await?;
    // r.md #156: 親が「開くデバイス」を渡してくる。開いたデバイスを報告し、その実レートが
    // session.sample_rate の SSoT になる (A1 r.md #8: engine はハードウェアのレートで動く)。
    // デバイスに触るのは制御スレッドだけ (`device::control`)。ここは結果を待つ (有界)。まだ鳴らさない。
    let device_config = common::client::read_open_device(&mut pipe).await?;
    let device = device::control::DeviceControl::spawn().context("failed to spawn the audio device control thread")?;
    let device_info = tokio::task::block_in_place(|| device.open(device_config))?;
    common::client::send_device_info(&mut pipe, &device_info).await?;
    tracing::info!(?device_info, "daw_audio handshake complete");

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
    // r.md #129 §11.2: device 単位のサンプルリング (EQ Par のスペクトラム)。GUI が create したものを open し、
    // scope project の対象 device の出力を毎バッファ書き込む。
    let device_scope = Arc::new(
        common::device_scope_bridge::DeviceScopeBridgeHandle::open(&session.device_scope_shmem_id)
            .context("failed to open device scope shmem")?,
    );
    device_scope.set_sample_rate(session.sample_rate);

    let shared = Arc::new(SharedState::new());
    // Engine resources shared between the CPAL closure, the export thread and
    // the notify thread.
    let engine_shared = Arc::new(EngineShared::new());

    // Transport / launcher / preview channel: the receive loop pushes light commands here;
    // the audio thread drains it at the top of every buffer (rtrb、RT で確保も解放もしない)。
    // shmem / worker pool の重い扱いは bundle ring 経由 (plan §4)。
    let (cmd_tx, cmd_rx) = EngineCommandSender::channel();

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
    // r.md #156: オーディオ入力の録音。recv loop の口と、デバイスのコールバックへ差し込む tap。
    let (rec_hub, rec_tap) = audio_rec::channel();

    // 描き手 (engine の RT の状態一式)。持ち主は常に 1 本のスレッド (コールバックか先読みの描画スレッド。
    // 切り替えは `OutputStage` が停止中に行う — `output_render` の module doc)。
    let local = DeviceRt::new(
        common::process_data::MAX_FRAMES,
        cmd_rx,
        Arc::clone(&engine_shared),
        project_rx,
        project_recycle_tx,
        device_rx,
        device_recycle_tx,
    );
    // r.md #156: デバイスの入力をコールバックから描き手へ渡す ring (運ぶのは開いている入力 ch)。
    let (monitor_feed, monitor_input) = device::monitor::monitor_pair(
        device_info.active_inputs,
        (output_render::RENDER_AHEAD_PERIODS + 2) * common::process_data::MAX_FRAMES,
    );
    let renderer = output_render::OutputRenderer::new(
        local,
        Arc::clone(&shared),
        Arc::clone(&bridge),
        Arc::clone(&metrics),
        Arc::clone(&scope),
        Arc::clone(&device_scope),
        session.sample_rate,
        device::callback::OUT_CHANNELS,
        monitor_input,
    );
    let stage = output_render::OutputStage::spawn(renderer).context("failed to spawn audio render thread")?;
    // r.md #156: 録音の取り出し口 (デバイスのコールバックのスレッドで、描画の後に毎回呼ばれる)。
    let input_tap: Box<dyn device::input::InputTap> = Box::new(rec_tap);
    let callback = device::callback::EngineCallback::new(
        stage,
        input_tap,
        Arc::clone(&engine_shared),
        session.mute_output,
        monitor_feed,
    );

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

    // engine をデバイスへつないで鳴らし始める。以後 stream を止める / 起こす (r.md #49 のアイドル park) のは
    // 制御スレッドだけ: 描き手が `park_requested` を立て、制御スレッドが追従する。
    device.install(device::control::Install {
        callback,
        shared: Arc::clone(&shared),
        metrics: Arc::clone(&metrics),
        events: out_tx.clone(),
    });
    tracing::info!("audio stream running");

    // plan §4: RT からは pipe に書けない (I/O 禁止) ので、 quarantine / pool
    // stall / MMCSS 失敗のフラグを 100ms 周期で poll して GUI へ通知する
    // 専用スレッド。 dedup は per-entry / per-rig の AtomicBool swap。
    let notify = spawn_notify_thread(Arc::clone(&engine_shared), out_tx.clone());

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

    recv_loop(
        read_half,
        RecvLoop {
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
            rec: rec_hub,
        },
        &device,
    )
    .await;

    // (r.md #61) ここから graceful teardown。待ちはどれも有界:
    //   1. notify thread を止めて join する (poll 周期 100ms が上限)。
    //   2. 制御スレッドに stream を止めてデバイスを閉じさせ、描き手 (描画スレッド) を片付けさせる
    //      (`DeviceControl::shutdown`。「解放できた」を表すログは制御スレッドの `audio stream released`)。
    if let Some(notify) = notify {
        notify.stop_and_join();
    }
    device.shutdown();
    tracing::info!("daw_audio exiting");
    Ok(())
}

/// (r.md #61) notify thread の停止ハンドル (停止フラグ + join。脱出条件の無い loop にしない)。
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

/// park 要求を取り下げて即座に起こす。receive loop がコマンド受信時に呼ぶ。
///
/// 制御スレッドの周期 (100ms) を待たずに起こすのは応答性のため (待つと「Play を押してから音が出るまで
/// 一拍おく」になる)。要求の取り下げを先に行い、stream を触るのは制御スレッドだけなので、止める側と
/// 入れ違うことはない (`device::control`)。
fn wake_stream(device: &device::control::DeviceControl, shared: &SharedState) {
    shared.park_requested.store(false, Ordering::Release);
    shared.idle_silent_samples.store(0, Ordering::Release);
    device.wake();
}

/// plan §4: quarantine / pool stall / MMCSS 失敗フラグを 100ms 周期で poll
/// して GUI へ `AudioEvent` を送る通知スレッド (RT からは atomic store のみ)。
/// フラグの SSoT は `PluginEntry` / `WorkerRig` / `EngineShared` 上の
/// AtomicBool で、 dedup は `*_notified` の swap。
fn spawn_notify_thread(
    engine_shared: Arc<EngineShared>,
    out_tx: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
) -> Option<NotifyThread> {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("audio-notify".to_string())
        .spawn(move || {
            // rig ごとに「pair が詰まったまま借り替えられない」状態が始まった時刻。
            let mut degraded_since: Option<(usize, std::time::Instant)> = None;
            while !stop_for_thread.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(100));
                // デバイスのコールバックの MMCSS join 失敗の one-shot warn (RT では
                // tracing を出せないのでここで代行)。
                if engine_shared.mmcss_join_failed.load(Ordering::Acquire)
                    && !engine_shared.mmcss_warned.swap(true, Ordering::AcqRel)
                {
                    tracing::warn!("audio callback: MMCSS join (Pro Audio) failed");
                }
                notify_quarantined_devices(&engine_shared, &out_tx);
                // worker pool 全体の完了待ち timeout、または pair が詰まったまま予備も尽きた状態の持続 → pool 停止を
                // 1 回だけ通知 (GUI は plugin_host respawn → OpenWorkerPool 再送で復旧する)。
                if let Some(rig) = engine_shared.worker.load_full() { // arch-lint: allow-arcswap-load (off-RT: notify thread)
                    let key = Arc::as_ptr(&rig) as usize;
                    degraded_since = rig
                        .is_degraded()
                        .then(|| degraded_since.filter(|(k, _)| *k == key).unwrap_or((key, std::time::Instant::now())));
                    let escalate = std::time::Duration::from_millis(u64::from(common::plugin_ref::DISPATCH_TIMEOUT_MS) * 4);
                    let exhausted = degraded_since.is_some_and(|(_, since)| since.elapsed() >= escalate);
                    let stalled = rig.pool.as_ref().is_some_and(|p| p.is_stalled());
                    if (stalled || exhausted) && !rig.stall_notified.swap(true, Ordering::AcqRel) {
                        tracing::error!(stalled, exhausted, "audio worker pool unusable; rebuilding plugin host");
                        let _ = out_tx.send(AudioEvent::WorkerPoolStalled);
                    }
                }
            }
            tracing::info!("audio notify thread exiting");
        });
    match join {
        Ok(join) => Some(NotifyThread { stop, join }),
        Err(e) => {
            // spawn 失敗は quarantine 通知を失うだけで、
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
    let projects = engine_shared.projects.load(); // arch-lint: allow-arcswap-load (off-RT: notify thread)
    for (key, project) in projects.iter() {
        let refs = project.plugin_refs.load(); // arch-lint: allow-arcswap-load (off-RT: notify thread)
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
            worker: engine_shared.worker.load_full(), // arch-lint: allow-arcswap-load (off-RT: RT へ送る便を組む)
            sampler: engine_shared.sampler.load_full(), // arch-lint: allow-arcswap-load (off-RT: RT へ送る便を組む)
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
    cmd_tx: EngineCommandSender,
    out_tx: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
    decode_tx: std::sync::mpsc::Sender<DecodeJob>,
    project_tx: rtrb::Producer<ProjectDelivery>,
    project_recycle_rx: rtrb::Consumer<Box<ProjectRt>>,
    device_publisher: DevicePublisher,
    device_recycle_rx: rtrb::Consumer<DeviceBundle>,
    /// r.md #156: オーディオ入力の録音を作って tap と書き手へ渡す口。
    rec: audio_rec::RecHub,
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
    // ring が満杯の間に溜めた EngineCommand を流す (次のコマンドが来なくても届く)。
    rl.cmd_tx.flush();
    // r.md #156: tap が手放した録音の取り込み口を解放する。
    rl.rec.housekeeping();
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

async fn recv_loop(
    mut pipe: ReadHalf<NamedPipeClient>,
    mut rl: RecvLoop,
    device: &device::control::DeviceControl,
) {
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
            wake_stream(device, &rl.shared);
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
            // r.md #156: 開くデバイスは起動時の handshake で受け取り済み (動いている daw_audio のデバイスは
            // 替えない — 替えるときは daw_gui が daw_audio を起こし直す)。
            AudioCommand::OpenDevice { .. } => {
                tracing::warn!("OpenDevice after the handshake; ignored");
            }
            AudioCommand::ListAudioDevices => {
                // 列挙は数十 ms かかりうる (全デバイスの問い合わせ) ので、受信ループの外で。
                let out_tx = rl.out_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = out_tx.send(AudioEvent::AudioDevices(Box::new(device::enumerate::list())));
                });
            }
            // ドライバーの設定画面は制御スレッドが開く (ASIO のドライバーは作ったスレッドに縛られる)。
            AudioCommand::OpenDeviceControlPanel => device.open_control_panel(),
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
            // 描き手の切り替えは描き手を持っているスレッドが停止中に行う (`output_render`)。ここは望みの反映だけ。
            AudioCommand::SetRenderAhead(on) => {
                tracing::info!(on, "render-ahead preference");
                rl.shared.render_ahead_wanted.store(on, Ordering::Release);
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
            AudioCommand::OpenWorkerPool(spec) => {
                // worker rig (bridge shmem + handshake events + audio worker
                // threads) を **off-thread で** 構築し、 mirror + bundle で
                // 配送する。 旧 rig は RT の swap 後 recycle ring 経由で
                // ここに戻り、 off-thread で drop (= worker join) される。
                match build_worker_rig(&spec) {
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
                if sampler::handle_device_command(cmd, &rl.engine_shared, &mut rl.cmd_tx) {
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
                scope,
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
                    scope,
                );
            }
            // r.md #156: オーディオ入力の録音の準備。WAV の書き手を起こし、取り込み口を tap へ、判定の共有面を
            // 便で描画側へ届ける (この後に来る `StartRecording` で録音が始まる)。
            AudioCommand::StartAudioTakes { project, request } => {
                let Some(ctl) = projects.get_mut(&project) else {
                    tracing::warn!(project = project.0, "StartAudioTakes for an unknown project; ignored");
                    continue;
                };
                tracing::info!(project = project.0, take = request.take_id, tracks = request.takes.len(), "received StartAudioTakes");
                let take = rl.rec.start(project, request, rl.session_sample_rate, rl.out_tx.clone());
                ctl.set_audio_take(take, &rl.engine_shared, rl.session_sample_rate, &phase_tables);
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
                    &mut rl.cmd_tx,
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
/// `generation` は依頼番号に焼き込む (`common::worker_bridge` の module doc)。
fn build_worker_rig(spec: &common::protocol::WorkerPoolSpec) -> Result<WorkerRig> {
    // IPC 由来の数で channels[i] / event 名を indexing する前に検証 (out-of-bounds panic を防ぐ)。
    spec.validate()?;
    let bridge = common::worker_bridge::WorkerBridgeHandle::open(&spec.worker_bridge_shmem_id)
        .context("failed to open worker_bridge shmem")?;
    // Per-slot pointer into the bridge's channel array — the mapping's
    // address is stable for the bridge handle's lifetime, which the rig owns
    // (moving the handle struct does not move the mapped view).
    let mut slots = Vec::with_capacity(spec.n_pairs as usize);
    for (i, (wake_name, done_name)) in spec.wake_event_names.iter().zip(&spec.done_event_names).enumerate() {
        let wake = common::plugin_ref::create_named_event(wake_name)
            .with_context(|| format!("failed to open wake event {i}"))?;
        let done = common::plugin_ref::create_named_event(done_name)
            .with_context(|| format!("failed to open done event {i}"))?;
        slots.push(SyncSlot {
            sync: common::plugin_ref::WorkerSyncRef {
                worker_idx: i as u32,
                channel: &bridge.bridge().channels[i] as *const _,
                generation: spec.generation,
                event_wake: wake,
                event_done: done,
            },
            poisoned: std::sync::atomic::AtomicBool::new(false),
        });
    }
    // Spawn the audio-engine worker pool sized to the runners (master is runner 0,
    // worker i is runner i+1; each leases one pair, the rest are spares). 失敗しても handshake 面は生かして
    // serial fallback (runner 0 のみ) で動かす。
    let pool = match audio_worker::AudioWorkerPool::new(spec.n_runners) {
        Ok(pool) => Some(pool),
        Err(e) => {
            tracing::error!(error = ?e, "AudioWorkerPool::new failed; serial fallback");
            None
        }
    };
    Ok(WorkerRig::new(pool, slots, spec.n_runners as usize, bridge))
}

/// グラフの内訳 (自分の仕事 / plugin 待ち / 実効並列度) を metrics 面へ publish する。
///
/// callback の末尾で 1 回だけ呼ぶ。ふつうこのとき runner は全員グラフを抜けている
/// (`AudioWorkerPool::run` が `inside` 0 を見て戻る)。stall の窓の生き残りが重なっても
/// counter は atomic で、害は内訳のずれだけ (`crate::graph::profile` の module doc)。
/// 1 buffer も流れていない窓 (song 未ロード / 停止直後) は何も足さない。
/// RT-safe: atomic の読み書きだけ。
#[inline]
fn publish_graph_profile(
    worker: Option<&crate::engine_shared::WorkerRig>,
    metrics: &common::metrics_bridge::MetricsBridgeHandle,
) {
    let Some(rig) = worker else { return };
    let g = rig.profile.take();
    if g.buffers != 0 {
        metrics.add_graph(&g);
    }
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
