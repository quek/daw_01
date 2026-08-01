//! Subprocess + IPC + shmem bootstrap、 GUI mode と script mode で共通化。
//!
//! `bootstrap_subprocess()` は daw_audio / daw_plugin_host を spawn し、
//! 名前付きパイプで Hello / Ack handshake を行い、 audio shmem +
//! worker pool の各 OS リソースを作成して `Bootstrap` 構造体に
//! 詰めて返す。 GUI mode (`main.rs::run_gui`) と script mode
//! (`script::run_scripted`) は同じ `Bootstrap` を受け取り、 異なる driver
//! (winit event loop / boa script runtime) を被せる。
//!
//! v29 (`docs/plan_arch_refactor.md` §3): pipe は宛先ごとに型が分かれる。
//! gui→audio は [`AudioCommand`]、 audio→gui は [`AudioEvent`]、
//! gui→plugin_host は [`PluginCommand`]、 plugin_host→gui は
//! [`PluginEvent`]。 incoming は [`ChildEvent`] (2 pipe の多重化 enum) で
//! 1 本の channel に集約する。 handshake では子の Hello に載る
//! `protocol_fingerprint` を [`PROTOCOL_FINGERPRINT`] と照合し、 ビルド
//! 世代の混在 (silent misdecode) を接続時に検出する。
//!
//! `Bootstrap` を Drop すると、 keep-alive で握っている子プロセス /
//! Handle / shmem 群が一斉に解放される (Job Object 経由で子も kill される)。

use std::sync::Arc;

use anyhow::{Context as _, Result};
use common::audio_bridge::{AudioBridgeHandle, CHANNELS, DEFAULT_SAMPLE_RATE, MAX_FRAMES, shmem_id};
use common::metrics_bridge::{MetricsBridgeHandle, metrics_shmem_id};
use common::pipe::pipe_path;
use common::plugin_db::PluginDatabase;
use common::protocol::{
    AudioCommand, AudioEvent, AudioSession, ChildKind, PROTOCOL_FINGERPRINT, PluginCommand,
    PluginEvent,
};
use common::wire::{read_msg, write_msg};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::process::Child;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::job::JobHandle;

/// 2 本の pipe (daw_audio / daw_plugin_host) からの incoming event を
/// 1 本の channel に多重化する enum。 pipe 自体は [`AudioEvent`] /
/// [`PluginEvent`] で型分割されたまま、 GUI mode の `spawn_incoming_bridge` /
/// script mode の `pump_until` が単一 receiver で drain できるようにする。
#[derive(Debug, Clone, PartialEq)]
pub enum ChildEvent {
    Audio(AudioEvent),
    Plugin(PluginEvent),
}

/// 子の Hello が載せた `protocol_fingerprint` が親の
/// [`PROTOCOL_FINGERPRINT`] と一致しない (= 子 exe のビルドが古い)。
/// respawn 側はこれを downcast して「respawn loop に入らず、 make build を
/// 促す status message を出す」 分岐に使う (`docs/plan_arch_refactor.md` §3)。
#[derive(Debug, Clone, Copy)]
pub struct FingerprintMismatch {
    pub kind: ChildKind,
    pub child_fingerprint: u64,
}

impl std::fmt::Display for FingerprintMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "protocol fingerprint mismatch for {}: child={:#018x} parent={:#018x} \
             (daw_audio.exe / daw_plugin_host.exe のビルドが古い — make build を実行してください)",
            self.kind.as_str(),
            self.child_fingerprint,
            PROTOCOL_FINGERPRINT,
        )
    }
}

impl std::error::Error for FingerprintMismatch {}

/// 子プロセス起動 + IPC + shmem 全部を保持するセッション。 GUI mode と
/// script mode が共通で受け取る。 drop すると JobObject 経由で子プロセス
/// も auto-kill される (`JobHandle::new` で `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
/// を立てているため)。
pub struct Bootstrap {
    /// daw_audio への IPC 送信 channel。
    pub audio_tx: UnboundedSender<AudioCommand>,
    /// daw_plugin_host への IPC 送信 channel。
    pub plugin_tx: UnboundedSender<PluginCommand>,
    /// 子プロセスからの incoming events (2 pipe 多重化)。 GUI mode では
    /// `spawn_incoming_bridge` がここから drain して `EventLoopProxy` に
    /// flow させる、 script mode では同期 `pump_until` が直接 drain する。
    /// 一度しか consume できないので `Option` で持って `take_incoming_rx`
    /// で取り出す。 script mode は struct 全体を move するので
    /// `incoming_rx` フィールドを直接見る。
    pub incoming_rx: Option<UnboundedReceiver<ChildEvent>>,
    /// Audio shmem ハンドル。 audio_bridge::AudioBridgeHandle は playhead /
    /// peak meter 等の state を保持する shared memory region を参照する。
    pub bridge: Arc<AudioBridgeHandle>,
    /// (A1 r.md #8) 解決済みオーディオセッションのサンプルレート (= daw_audio が Hello で
    /// 報告したデバイス実レート、 query 失敗時は `DEFAULT_SAMPLE_RATE`)。 GUI の拍↔sample
    /// 変換 (seek / export range / clip 尺) はこの値を使い、 engine と一致させる。
    /// respawn で変わりうるので AppData 側の複製は respawn 結果で更新する。
    pub sample_rate: u32,
    /// resource monitor (r.md #3) の MetricsBridge ハンドル。 DSP load / xrun /
    /// per-plugin CPU を daw_audio / daw_plugin_host が書き、 poller と AppData
    /// (per-plugin 直接読み) が読む。
    pub metrics: Arc<MetricsBridgeHandle>,
    /// VOICEVOX engine 等の子プロセスを kill-on-close するための Job Object。
    pub job: Arc<JobHandle>,
    /// 起動時に読み込んだ plugin database (CLAP / VST3 enumeration)。
    pub plugin_db: Option<Arc<PluginDatabase>>,
    /// 子プロセス kill のために生かす tokio runtime + 子プロセス Handle。
    /// drop 順序が大事 (children → runtime の順)。
    pub rt: Runtime,
    /// 子プロセス自動再起動用 supervisor。 pipe loop が break して
    /// `AudioEvent::ChildDisconnected` / `PluginEvent::ChildDisconnected` を
    /// 発火したら、 AppData が `supervisor.respawn_audio()` /
    /// `respawn_plugin()` で新 child を spawn + handshake + Session/
    /// OpenWorkerPool 再送し、 新 tx を受け取って AppData の `audio_tx` /
    /// `plugin_tx` を差し替える。 worker pool の event 名 / HANDLE
    /// keep-alive も supervisor が世代ごと管理する。
    pub supervisor: Arc<ChildSupervisor>,
    _worker_bridge: common::worker_bridge::WorkerBridgeHandle,
}

/// 1 世代分の worker pool spec (= `OpenWorkerPool` の payload)。 respawn 後、
/// 生存している側の子プロセスにも同じ世代を re-open させるために
/// [`RespawnedAudio`] / [`RespawnedPlugin`] へ複製して返す。
#[derive(Debug, Clone)]
pub struct WorkerPoolSpec {
    pub n_workers: u32,
    pub worker_bridge_shmem_id: String,
    pub wake_event_names: Vec<String>,
    pub done_event_names: Vec<String>,
}

impl WorkerPoolSpec {
    pub fn to_audio_cmd(&self) -> AudioCommand {
        AudioCommand::OpenWorkerPool {
            n_workers: self.n_workers,
            worker_bridge_shmem_id: self.worker_bridge_shmem_id.clone(),
            wake_event_names: self.wake_event_names.clone(),
            done_event_names: self.done_event_names.clone(),
        }
    }

    pub fn to_plugin_cmd(&self) -> PluginCommand {
        PluginCommand::OpenWorkerPool {
            n_workers: self.n_workers,
            worker_bridge_shmem_id: self.worker_bridge_shmem_id.clone(),
            wake_event_names: self.wake_event_names.clone(),
            done_event_names: self.done_event_names.clone(),
        }
    }
}

/// worker pool の現世代 state。 event 名は
/// `worker_wake_event_name(pid, generation, idx)` で mint され、 pool
/// 再構築 (= `OpenWorkerPool` 再送) のたびに generation を bump する —
/// poisoned pair (dispatch timeout 後) が残した stale auto-reset signal を
/// 旧世代の名前空間へ隔離するため (`common::plugin_ref` の contract 参照)。
struct WorkerPoolState {
    generation: u32,
    spec: WorkerPoolSpec,
    /// 親プロセスが create した named event の keep-alive。 子はどちらも
    /// create-or-open するが、 親が先に握ることで「両子の open 順序 race」
    /// を消す。 世代交代で古い handle は drop (= CloseHandle) される。
    _handles: PoolEventHandles,
}

/// named event HANDLE の keep-alive + Drop で CloseHandle する wrapper。
/// kernel handle は thread affinity を持たないので Send/Sync は安全。
struct PoolEventHandles {
    #[cfg(windows)]
    handles: Vec<windows::Win32::Foundation::HANDLE>,
}

#[cfg(windows)]
unsafe impl Send for PoolEventHandles {}
#[cfg(windows)]
unsafe impl Sync for PoolEventHandles {}

#[cfg(windows)]
impl Drop for PoolEventHandles {
    fn drop(&mut self) {
        for h in self.handles.drain(..) {
            // named event: 全 handle が閉じた時点で kernel object が消える。
            // 子プロセス側はそれぞれ自分の handle を持つので、 ここで閉じても
            // 現役世代が壊れることはない (呼ばれるのは世代交代 / 終了時のみ)。
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(h);
            }
        }
    }
}

/// `respawn_audio` の結果。 呼び出し側 (AppData) は `tx` を差し替え、
/// `sample_rate` の複製を更新し、 生存側 plugin_host へ
/// `CloseWorkerPool` + `pool.to_plugin_cmd()` を送って新世代 pool に
/// 載せ替える。
pub struct RespawnedAudio {
    pub tx: UnboundedSender<AudioCommand>,
    /// 新 Hello の `device_sample_rate` を採用した session の実レート
    /// (報告なしなら旧値を維持)。
    pub sample_rate: u32,
    pub pool: WorkerPoolSpec,
}

/// `respawn_plugin` の結果。 呼び出し側 (AppData) は `tx` を差し替え、
/// 生存側 daw_audio へ `CloseWorkerPool` + `pool.to_audio_cmd()` を送る。
pub struct RespawnedPlugin {
    pub tx: UnboundedSender<PluginCommand>,
    pub pool: WorkerPoolSpec,
}

/// 子プロセス re-spawn 用の context。 daw_audio / daw_plugin_host が
/// pipe break で死亡したとき、 `respawn_audio()` / `respawn_plugin()` で
/// 新 child を spawn + handshake + Session/OpenWorkerPool 再送し、 新
/// sender を返す。
///
/// 子プロセスは `Mutex<Option<Child>>` で保持 (= Drop で自動 kill、
/// 再 spawn 時は古い child を `start_kill` してから置換)。 OS 上の
/// shared memory は `Bootstrap` 側で保持し続けるので、 新 child は
/// OpenShared で join できる。 worker pool の named event は世代ごとに
/// 新規作成する (上記 [`WorkerPoolState`] 参照)。
pub struct ChildSupervisor {
    pid: u32,
    rt_handle: tokio::runtime::Handle,
    job: Arc<JobHandle>,
    /// 現在の session。 audio respawn で新 Hello の `device_sample_rate` を
    /// 採用して更新される (= 以後の plugin respawn も新レートで Session を
    /// 受ける)。
    session: std::sync::Mutex<AudioSession>,
    n_workers: u32,
    worker_bridge_shmem_id: String,
    /// worker pool の現世代 (event 名 + HANDLE keep-alive)。
    pool: std::sync::Mutex<WorkerPoolState>,
    incoming_tx: UnboundedSender<ChildEvent>,
    audio_child: std::sync::Mutex<Option<Child>>,
    plugin_child: std::sync::Mutex<Option<Child>>,
}

impl ChildSupervisor {
    /// 現在の session snapshot (sample_rate は respawn で更新されうる)。
    pub fn current_session(&self) -> AudioSession {
        self.session
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// worker pool の世代を bump し、 新しい event 名 + HANDLE を mint して
    /// 現世代として保持する。 戻り値は新世代の `OpenWorkerPool` payload。
    fn rotate_worker_pool(&self) -> Result<WorkerPoolSpec> {
        let mut guard = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let generation = guard.generation.wrapping_add(1).max(1);
        let state = build_worker_pool_state(
            self.pid,
            generation,
            self.n_workers,
            &self.worker_bridge_shmem_id,
        )?;
        let spec = state.spec.clone();
        // 旧世代 handle はここで drop (= CloseHandle)。 stale auto-reset
        // signal は旧世代の名前空間ごと消える。
        *guard = state;
        Ok(spec)
    }

    /// daw_audio を再起動する。 新 Hello の `device_sample_rate` を採用して
    /// session を再構築し (`docs/plan_arch_refactor.md` §3)、 新世代の
    /// worker pool を open した上で送信 channel を返す。
    pub fn respawn_audio(&self) -> Result<RespawnedAudio> {
        let (server, child, device_sample_rate) =
            self.spawn_and_handshake_one(ChildKind::Audio)?;

        // 新 Hello のデバイス実レートを session に採用 (報告なしなら維持)。
        let session = {
            let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(sr) = device_sample_rate.filter(|&s| s > 0) {
                guard.sample_rate = sr;
            }
            guard.clone()
        };
        let pool = self.rotate_worker_pool()?;

        let mut server = server;
        let session_msg = AudioCommand::Session(session.clone());
        let open_pool = pool.to_audio_cmd();
        self.rt_handle.block_on(async {
            write_msg(&mut server, &session_msg).await?;
            write_msg(&mut server, &open_pool).await?;
            anyhow::Ok(())
        })?;

        let (tx, rx) = unbounded_channel::<AudioCommand>();
        let incoming_tx = self.incoming_tx.clone();
        self.rt_handle.spawn(audio_pipe_loop(server, rx, incoming_tx));

        if let Ok(mut guard) = self.audio_child.lock() {
            *guard = Some(child);
        }
        tracing::info!("audio child respawned");
        Ok(RespawnedAudio {
            tx,
            sample_rate: session.sample_rate,
            pool,
        })
    }

    /// daw_plugin_host を再起動する。 現 session + 新世代 worker pool を
    /// 送った上で送信 channel を返す。
    pub fn respawn_plugin(&self) -> Result<RespawnedPlugin> {
        let (server, child, _hello) = self.spawn_and_handshake_one(ChildKind::PluginHost)?;
        let session = self.current_session();
        let pool = self.rotate_worker_pool()?;

        let mut server = server;
        let session_msg = PluginCommand::Session(session);
        let open_pool = pool.to_plugin_cmd();
        // r.md #36: 「エディタ窓で拾ってよいキー」 も Session / worker pool と同じ
        // **子起動ごとに必ず送る初期状態**。 respawn で送り忘れると plugin-host の
        // forwarded_keys が空のままになり、 プラグインエディタ上の Space が黙って
        // 効かなくなる (キーは素通しされるので症状が「何も起きない」= 気づけない)。
        let forwarded_keys = forwarded_editor_keys_cmd();
        self.rt_handle.block_on(async {
            write_msg(&mut server, &session_msg).await?;
            write_msg(&mut server, &open_pool).await?;
            write_msg(&mut server, &forwarded_keys).await?;
            anyhow::Ok(())
        })?;

        let (tx, rx) = unbounded_channel::<PluginCommand>();
        let incoming_tx = self.incoming_tx.clone();
        self.rt_handle.spawn(plugin_pipe_loop(server, rx, incoming_tx));

        if let Ok(mut guard) = self.plugin_child.lock() {
            *guard = Some(child);
        }
        tracing::info!("plugin_host child respawned");
        Ok(RespawnedPlugin { tx, pool })
    }

    /// respawn の共通前段: 旧 child kill → pipe server 再作成 → spawn →
    /// Hello handshake (fingerprint 検証込み)。 audio pipe は
    /// `AudioEvent::Hello`、 plugin pipe は `PluginEvent::Hello` を受けるが、
    /// 呼び出し側が使うのは audio の `device_sample_rate` だけなので、
    /// それだけ取り出して返す (plugin は常に `None`)。
    fn spawn_and_handshake_one(
        &self,
        kind: ChildKind,
    ) -> Result<(NamedPipeServer, Child, Option<u32>)> {
        // 1) 既存 child を kill / drop。 OS pipe の解放を待つ前提で、
        // 同 pipe_path を再利用する (= new ServerOptions.create を回す)。
        let child_slot = match kind {
            ChildKind::Audio => &self.audio_child,
            ChildKind::PluginHost => &self.plugin_child,
        };
        if let Ok(mut guard) = child_slot.lock()
            && let Some(mut c) = guard.take()
        {
            // start_kill は async ではないので即返る。 status は最後に
            // wait されないが、 Job Object 経由でも回収される。
            let _ = c.start_kill();
        }

        let pipe = pipe_path(self.pid, kind);
        let binary = match kind {
            ChildKind::Audio => "daw_audio",
            ChildKind::PluginHost => "daw_plugin_host",
        };

        // 2) 新 server を作成 → 子 spawn → handshake、 を tokio runtime 内で
        // 実行。 handshake に timeout を付ける: respawn は GUI main thread の
        // block_on で呼ばれるので、 新 child が Hello を送る前にハング/
        // crash すると main thread が永久に固まる。 5s で諦めて Err を返す。
        let pipe_for_spawn = pipe.clone();
        let job = self.job.clone();
        self.rt_handle.block_on(async move {
            let server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&pipe_for_spawn)
                .with_context(|| format!("failed to create pipe {pipe_for_spawn}"))?;
            let mut child = crate::subprocess::spawn_sibling(binary, [&pipe_for_spawn])?;
            job.assign(&child)?;
            let handshake_result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                match kind {
                    ChildKind::Audio => {
                        let (hello, server) = handshake_audio(server).await?;
                        let AudioEvent::Hello {
                            device_sample_rate, ..
                        } = hello
                        else {
                            unreachable!("handshake_audio returns only Hello");
                        };
                        Ok((device_sample_rate, server))
                    }
                    ChildKind::PluginHost => {
                        let (_hello, server) = handshake_plugin(server).await?;
                        Ok((None, server))
                    }
                }
            })
            .await
            .context("respawn handshake timed out (new child died before handshake?)");
            let (device_sample_rate, server) = match handshake_result {
                Ok(Ok(v)) => v,
                Ok(Err(e)) | Err(e) => {
                    // fingerprint 不一致 / handshake 失敗した子は生かして
                    // おかない (pipe server はここで drop されるので子も
                    // 追随して exit するが、 明示 kill で即回収する)。
                    let _ = child.start_kill();
                    return Err(e);
                }
            };
            anyhow::Ok((server, child, device_sample_rate))
        })
    }
}

/// 子プロセスを spawn → handshake → Session / OpenWorkerPool 配信 →
/// pipe loop spawn まで一連の起動処理。 GUI mode と script mode の前段は
/// 完全に同一なのでこの 1 関数でカバー。
pub fn bootstrap_subprocess() -> Result<Bootstrap> {
    let job = Arc::new(JobHandle::new()?);
    let rt = Runtime::new().context("failed to create tokio runtime")?;

    let pid = std::process::id();
    // A1 (r.md #8): sample_rate は暫定 (DEFAULT)。 daw_audio が Hello で報告する
    // デバイス実レートで spawn_and_handshake 後に上書きする (= エンジンはハード
    // ウェアのレートで動く)。 shmem_id 等は sample_rate 非依存なのでここで確定。
    let mut session = AudioSession {
        shmem_id: shmem_id(pid),
        metrics_shmem_id: metrics_shmem_id(pid),
        sample_rate: DEFAULT_SAMPLE_RATE,
        max_frames: MAX_FRAMES,
        channels: CHANNELS as u16,
    };
    let bridge = Arc::new(
        AudioBridgeHandle::create(&session.shmem_id).context("failed to create audio shmem")?,
    );
    // resource monitor (r.md #3): DSP load / xrun / per-plugin CPU を集約する
    // 共有メモリ。 daw_gui (親) が create し、 daw_audio / daw_plugin_host が
    // session.metrics_shmem_id 経由で open する。
    let metrics = Arc::new(
        MetricsBridgeHandle::create(&session.metrics_shmem_id)
            .context("failed to create metrics shmem")?,
    );
    tracing::info!(?session, "created audio session handles");

    let n_workers = pick_worker_count();
    let worker_bridge_shmem_id = common::plugin_ref::worker_bridge_shmem_id(pid);
    let worker_bridge = common::worker_bridge::WorkerBridgeHandle::create(&worker_bridge_shmem_id)
        .context("failed to create worker_bridge shmem")?;
    // 初回 pool は generation 1 (respawn ごとに rotate_worker_pool が bump)。
    let pool_state = build_worker_pool_state(pid, 1, n_workers, &worker_bridge_shmem_id)?;
    tracing::info!(n_workers, "created plugin worker pool handles (generation 1)");

    let (audio_child, plugin_child, mut audio_server, mut plugin_server, audio_device_sr) =
        rt.block_on(spawn_and_handshake(&job))?;
    // A1 (r.md #8): daw_audio が報告したデバイス実レートを session の SSoT にする。
    // 報告無し (query 失敗) なら DEFAULT_SAMPLE_RATE のまま。
    if let Some(sr) = audio_device_sr.filter(|&s| s > 0) {
        session.sample_rate = sr;
    }
    tracing::info!(sample_rate = session.sample_rate, "resolved audio session sample rate");

    let open_pool_audio = pool_state.spec.to_audio_cmd();
    let open_pool_plugin = pool_state.spec.to_plugin_cmd();
    let forwarded_keys = forwarded_editor_keys_cmd();
    rt.block_on(async {
        write_msg(&mut audio_server, &AudioCommand::Session(session.clone())).await?;
        write_msg(&mut plugin_server, &PluginCommand::Session(session.clone())).await?;
        write_msg(&mut audio_server, &open_pool_audio).await?;
        write_msg(&mut plugin_server, &open_pool_plugin).await?;
        // r.md #36: 転送対象キー (SHORTCUTS 由来) も起動時の初期状態として送る。
        write_msg(&mut plugin_server, &forwarded_keys).await?;
        anyhow::Ok(())
    })
    .context("failed to send audio session / worker pool")?;

    let (audio_tx, audio_rx) = unbounded_channel::<AudioCommand>();
    let (plugin_tx, plugin_rx) = unbounded_channel::<PluginCommand>();
    let (incoming_tx, incoming_rx) = unbounded_channel::<ChildEvent>();
    rt.spawn(audio_pipe_loop(audio_server, audio_rx, incoming_tx.clone()));
    rt.spawn(plugin_pipe_loop(plugin_server, plugin_rx, incoming_tx.clone()));

    let plugin_db = load_or_build_plugin_db();

    let supervisor = Arc::new(ChildSupervisor {
        pid,
        rt_handle: rt.handle().clone(),
        job: job.clone(),
        session: std::sync::Mutex::new(session.clone()),
        n_workers,
        worker_bridge_shmem_id: worker_bridge_shmem_id.clone(),
        pool: std::sync::Mutex::new(pool_state),
        incoming_tx,
        audio_child: std::sync::Mutex::new(Some(audio_child)),
        plugin_child: std::sync::Mutex::new(Some(plugin_child)),
    });

    Ok(Bootstrap {
        audio_tx,
        plugin_tx,
        incoming_rx: Some(incoming_rx),
        bridge,
        sample_rate: session.sample_rate,
        metrics,
        job,
        plugin_db,
        rt,
        supervisor,
        _worker_bridge: worker_bridge,
    })
}

impl Bootstrap {
    /// `incoming_rx` を 1 度だけ取り出す。 GUI mode は `spawn_incoming_bridge`
    /// に渡すために take、 script mode は host が `&mut self.incoming_rx` で
    /// 直接 poll する (= take せず option として残す)。
    pub fn take_incoming_rx(&mut self) -> Option<UnboundedReceiver<ChildEvent>> {
        self.incoming_rx.take()
    }
}

const MAX_WORKER_COUNT_DEFAULT: u32 = 64;

fn pick_worker_count() -> u32 {
    let n = std::env::var("DAW_AUDIO_WORKERS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(1).max(1) as u32)
                .unwrap_or(2)
        });
    n.min(common::worker_bridge::MAX_WORKERS as u32)
        .clamp(1, MAX_WORKER_COUNT_DEFAULT)
}

/// 指定世代の worker pool state (event 名 + 親 keep-alive HANDLE) を作る。
/// 名前は `worker_wake_event_name(pid, generation, idx)` — 世代を名前に
/// 含めることで、 旧世代 pair の stale auto-reset signal が新 pool に
/// 漏れない (`common::plugin_ref` の poisoning contract)。
fn build_worker_pool_state(
    pid: u32,
    generation: u32,
    n_workers: u32,
    worker_bridge_shmem_id: &str,
) -> Result<WorkerPoolState> {
    let mut wake_names = Vec::with_capacity(n_workers as usize);
    let mut done_names = Vec::with_capacity(n_workers as usize);
    #[cfg(windows)]
    let mut handles: Vec<windows::Win32::Foundation::HANDLE> =
        Vec::with_capacity(2 * n_workers as usize);
    for i in 0..n_workers {
        let wn = common::plugin_ref::worker_wake_event_name(pid, generation, i);
        let dn = common::plugin_ref::worker_done_event_name(pid, generation, i);
        #[cfg(windows)]
        {
            let wh = common::plugin_ref::create_named_event(&wn)
                .with_context(|| format!("failed to create wake event {i} (gen {generation})"))?;
            let dh = common::plugin_ref::create_named_event(&dn)
                .with_context(|| format!("failed to create done event {i} (gen {generation})"))?;
            handles.push(wh);
            handles.push(dh);
        }
        wake_names.push(wn);
        done_names.push(dn);
    }
    Ok(WorkerPoolState {
        generation,
        spec: WorkerPoolSpec {
            n_workers,
            worker_bridge_shmem_id: worker_bridge_shmem_id.to_string(),
            wake_event_names: wake_names,
            done_event_names: done_names,
        },
        _handles: PoolEventHandles {
            #[cfg(windows)]
            handles,
        },
    })
}

async fn spawn_and_handshake(
    job: &JobHandle,
) -> Result<(Child, Child, NamedPipeServer, NamedPipeServer, Option<u32>)> {
    let pid = std::process::id();
    let audio_pipe = pipe_path(pid, ChildKind::Audio);
    let plugin_pipe = pipe_path(pid, ChildKind::PluginHost);

    let audio_server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&audio_pipe)
        .with_context(|| format!("failed to create pipe {audio_pipe}"))?;
    let plugin_server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&plugin_pipe)
        .with_context(|| format!("failed to create pipe {plugin_pipe}"))?;

    let audio_child = crate::subprocess::spawn_sibling("daw_audio", [&audio_pipe])?;
    job.assign(&audio_child)?;
    let plugin_child = crate::subprocess::spawn_sibling("daw_plugin_host", [&plugin_pipe])?;
    job.assign(&plugin_child)?;

    // respawn 側 (5s timeout) と同様、 handshake 前に子が crash / hang すると
    // ここが無限ブロックし GUI が起動しないまま固まる。 初回起動は AV スキャン
    // 等で respawn より遅くなり得るので余裕を持った 15s にする。
    // fingerprint 不一致 (= 古い exe) もここで明示 fail する — decode が偶然
    // 通って silent misdecode のまま走り出すことを構造的に防ぐ。
    let (audio_result, plugin_result) = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        async {
            tokio::try_join!(
                handshake_audio(audio_server),
                handshake_plugin(plugin_server),
            )
        },
    )
    .await
    .context("initial handshake timed out (child process died before handshake?)")??;
    let (audio_hello, audio_server) = audio_result;
    let (plugin_hello, plugin_server) = plugin_result;
    tracing::info!(?audio_hello, "audio handshake complete");
    tracing::info!(?plugin_hello, "plugin_host handshake complete");
    // A1 (r.md #8): daw_audio が Hello で報告したデバイス実レートを取り出す
    // (= session.sample_rate の SSoT)。
    let audio_device_sr = match &audio_hello {
        AudioEvent::Hello { device_sample_rate, .. } => *device_sample_rate,
        _ => None,
    };

    Ok((audio_child, plugin_child, audio_server, plugin_server, audio_device_sr))
}

/// daw_audio との handshake: `AudioEvent::Hello` を受けて fingerprint を
/// 検証し、 `AudioCommand::Ack` を返す。 不一致は [`FingerprintMismatch`]
/// で明示 fail (呼び出し側が downcast して respawn loop を止める)。
async fn handshake_audio(mut server: NamedPipeServer) -> Result<(AudioEvent, NamedPipeServer)> {
    server.connect().await.context("failed to accept client")?;
    let hello: AudioEvent = read_msg(&mut server).await?;
    let AudioEvent::Hello {
        protocol_fingerprint,
        ..
    } = &hello
    else {
        anyhow::bail!("expected Hello from daw_audio, got {:?}", hello);
    };
    if *protocol_fingerprint != PROTOCOL_FINGERPRINT {
        return Err(FingerprintMismatch {
            kind: ChildKind::Audio,
            child_fingerprint: *protocol_fingerprint,
        }
        .into());
    }
    write_msg(&mut server, &AudioCommand::Ack).await?;
    Ok((hello, server))
}

/// daw_plugin_host との handshake: `PluginEvent::Hello` + fingerprint 検証 +
/// `PluginCommand::Ack`。
async fn handshake_plugin(mut server: NamedPipeServer) -> Result<(PluginEvent, NamedPipeServer)> {
    server.connect().await.context("failed to accept client")?;
    let hello: PluginEvent = read_msg(&mut server).await?;
    let PluginEvent::Hello {
        protocol_fingerprint,
        ..
    } = &hello
    else {
        anyhow::bail!("expected Hello from daw_plugin_host, got {:?}", hello);
    };
    if *protocol_fingerprint != PROTOCOL_FINGERPRINT {
        return Err(FingerprintMismatch {
            kind: ChildKind::PluginHost,
            child_fingerprint: *protocol_fingerprint,
        }
        .into());
    }
    write_msg(&mut server, &PluginCommand::Ack).await?;
    Ok((hello, server))
}

/// pipe loop の終わり方。 writer task は「write 失敗 = 子が読まない/パイプ死」
/// を true で返し、 「rx closed = 親側の意図的な teardown (respawn で tx
/// 差し替え / shutdown)」 を false で返す。 前者のみ `ChildDisconnected` を
/// 合成する — 従来は read 断しか検知せず、 writer task 死 (16MB 超 encode
/// 失敗等) が沈黙のゾンビになっていた (`docs/plan_arch_refactor.md` §3)。
async fn audio_pipe_loop(
    pipe: NamedPipeServer,
    mut rx: UnboundedReceiver<AudioCommand>,
    incoming_tx: UnboundedSender<ChildEvent>,
) {
    // read/write half を別タスクに分けて read を絶対 cancel しない
    // (詳細は plugin_pipe_loop / daw_plugin_host::pipe_loop)。旧 select! 構造は
    // read_msg を write 発火で途中 cancel し stream を desync させていた。
    let (mut read_half, mut write_half) = tokio::io::split(pipe);
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = write_msg(&mut write_half, &msg).await {
                tracing::error!(error = ?e, ?msg, "failed to send message to daw_audio");
                return true; // writer death = 切断扱い
            }
        }
        false // tx dropped = 意図的 teardown
    });
    let reader_incoming = incoming_tx.clone();
    let mut reader = tokio::spawn(async move {
        loop {
            match read_msg::<_, AudioEvent>(&mut read_half).await {
                Ok(m) => {
                    if reader_incoming.send(ChildEvent::Audio(m)).is_err() {
                        tracing::info!("incoming receiver dropped; audio pipe loop exiting");
                        return false; // 受け手消滅 = shutdown
                    }
                }
                Err(e) => {
                    tracing::info!(error = ?e, "daw_audio pipe closed");
                    return true;
                }
            }
        }
    });
    let disconnected = tokio::select! {
        w = &mut writer => {
            reader.abort();
            w.unwrap_or(true)
        }
        r = &mut reader => {
            writer.abort();
            r.unwrap_or(true)
        }
    };
    if disconnected {
        // 子プロセス側 (or 自前 write) が die / decode 失敗で抜けたケース。
        // 上位 (AppData::handle_event) で respawn + state restore に拾ってもらう。
        let _ = incoming_tx.send(ChildEvent::Audio(AudioEvent::ChildDisconnected));
    }
    tracing::info!("audio pipe loop ended");
}

async fn plugin_pipe_loop(
    pipe: NamedPipeServer,
    mut rx: UnboundedReceiver<PluginCommand>,
    incoming_tx: UnboundedSender<ChildEvent>,
) {
    // read_msg / write_msg (wire.rs の read_exact / write_all) は
    // cancellation-unsafe。旧構造の `select! { read_msg, rx.recv()=>write_msg }`
    // は、大きい message (例: 3.8MB の LoadSong response) の read 途中で write が
    // 発火すると read future を drop し消費済みバイトを捨てて stream を desync
    // させる (1GB garbage length → 切断ループの真因)。read/write half を別タスクに
    // 分けて read を絶対 cancel しない (daw_plugin_host::pipe_loop と同じ)。
    let (mut read_half, mut write_half) = tokio::io::split(pipe);
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = write_msg(&mut write_half, &msg).await {
                tracing::error!(error = ?e, ?msg, "failed to send message to plugin_host");
                return true;
            }
        }
        false
    });
    let reader_incoming = incoming_tx.clone();
    let mut reader = tokio::spawn(async move {
        loop {
            match read_msg::<_, PluginEvent>(&mut read_half).await {
                Ok(m) => {
                    if reader_incoming.send(ChildEvent::Plugin(m)).is_err() {
                        tracing::info!("incoming receiver dropped; pipe loop exiting");
                        return false;
                    }
                }
                Err(e) => {
                    tracing::info!(error = ?e, "plugin_host pipe closed");
                    return true;
                }
            }
        }
    });
    let disconnected = tokio::select! {
        w = &mut writer => {
            reader.abort();
            w.unwrap_or(true)
        }
        r = &mut reader => {
            writer.abort();
            r.unwrap_or(true)
        }
    };
    if disconnected {
        let _ = incoming_tx.send(ChildEvent::Plugin(PluginEvent::ChildDisconnected));
    }
    tracing::info!("plugin pipe loop ended");
}

fn load_or_build_plugin_db() -> Option<Arc<PluginDatabase>> {
    use common::plugin_db::default_cache_path;
    if let Some(cache) = default_cache_path() {
        match PluginDatabase::load_from_file(&cache) {
            Ok(Some(mut db)) => {
                // builtin (VOICEVOX 等) はコードが Single Source of Truth。
                // 古い cache には欠落 / 旧版が混じりうるので、load 後に必ず
                // 最新の builtin descriptors を注入する (cache を消さずに新
                // builtin が反映され、vocal track の instrument ロードが通る)。
                db.ensure_builtins();
                tracing::info!(
                    count = db.entries.len(),
                    path = %cache.display(),
                    "loaded cached plugin database"
                );
                return Some(Arc::new(db));
            }
            Ok(None) => {
                tracing::info!(path = %cache.display(), "no cache, scanning…");
            }
            Err(e) => {
                tracing::warn!(error = ?e, "failed to load cache, scanning");
            }
        }
        // DLL 実ロードによる scan は plugin-host の `--scan-plugins` 使い捨てプロセスが行う
        // (GUI プロセスは dlopen しない、S5-3)。失敗時は builtin のみで起動する。
        match crate::subprocess::scan_plugins() {
            Some(db) => {
                if let Err(e) = db.save_to_file(&cache) {
                    tracing::warn!(error = ?e, "failed to write plugin cache");
                } else {
                    tracing::info!(
                        count = db.entries.len(),
                        path = %cache.display(),
                        "wrote plugin database cache"
                    );
                }
                return Some(Arc::new(db));
            }
            None => {
                tracing::error!("plugin scan subprocess failed; starting with builtins only");
            }
        }
    }
    None
}

/// r.md #36: プラグインエディタ窓で拾ってよいキー (= `SHORTCUTS` の
/// `forward_from_external_window` が立った行) を plugin-host へ通知する command。
///
/// 「どのキーが何をするか」 の意味論は daw_gui 側の `SHORTCUTS` だけが持ち、
/// plugin-host には Win32 chord (数値) しか渡らない。 **子プロセス起動ごとに必ず送る
/// 初期状態** なので、 初回 bootstrap と respawn の両方から呼ぶ。
fn forwarded_editor_keys_cmd() -> PluginCommand {
    let chords: Vec<common::protocol::KeyChord> = crate::view::shortcuts::forwarded_editor_chords()
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    PluginCommand::SetEditorForwardedKeys { chords }
}
