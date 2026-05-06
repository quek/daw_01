//! Subprocess + IPC + shmem bootstrap、 GUI mode と script mode で共通化。
//!
//! `bootstrap_subprocess()` は daw_audio / daw_plugin_host を spawn し、
//! 名前付きパイプで Hello / Ack handshake を行い、 audio shmem +
//! semaphore + worker pool の各 OS リソースを作成して `Bootstrap` 構造体に
//! 詰めて返す。 GUI mode (`main.rs::run_gui`) と script mode
//! (`script::run_scripted`) は同じ `Bootstrap` を受け取り、 異なる driver
//! (winit event loop / boa script runtime) を被せる。
//!
//! `Bootstrap` を Drop すると、 keep-alive で握っている子プロセス /
//! Handle / shmem 群が一斉に解放される (Job Object 経由で子も kill される)。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use common::audio_bridge::{
    AudioBridgeHandle, CHANNELS, MAX_FRAMES, SAMPLE_RATE, ready_sem_id, request_sem_id, shmem_id,
};
use common::pipe::pipe_path;
use common::plugin_db::PluginDatabase;
use common::protocol::{AudioSession, ChildKind, ChildToMain, MainToChild};
use common::win_sem::Semaphore;
use common::wire::{read_msg, write_msg};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::process::Child;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::job::JobHandle;

/// 子プロセス起動 + IPC + shmem 全部を保持するセッション。 GUI mode と
/// script mode が共通で受け取る。 drop すると JobObject 経由で子プロセス
/// も auto-kill される (`JobHandle::new` で `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
/// を立てているため)。
pub struct Bootstrap {
    /// 子プロセスへの IPC 送信 chann。 production / script で同じ。
    pub audio_tx: UnboundedSender<MainToChild>,
    pub plugin_tx: UnboundedSender<MainToChild>,
    /// 子プロセスからの incoming events。 GUI mode では
    /// `spawn_incoming_bridge` がここから drain して `EventLoopProxy` に
    /// flow させる、 script mode では同期 `pump_until` が直接 drain する。
    /// 一度しか consume できないので `Option` で持って `take_incoming_rx`
    /// で取り出す。 script mode は struct 全体を move するので
    /// `incoming_rx` フィールドを直接見る。
    pub incoming_rx: Option<UnboundedReceiver<ChildToMain>>,
    /// Audio shmem ハンドル。 audio_bridge::AudioBridgeHandle は playhead /
    /// peak meter 等の state を保持する shared memory region を参照する。
    pub bridge: Arc<AudioBridgeHandle>,
    /// VOICEVOX engine 等の子プロセスを kill-on-close するための Job Object。
    pub job: Arc<JobHandle>,
    /// 起動時に読み込んだ plugin database (CLAP / VST3 enumeration)。
    pub plugin_db: Option<Arc<PluginDatabase>>,
    /// 子プロセス kill のために生かす tokio runtime + 子プロセス Handle。
    /// drop 順序が大事 (children → runtime の順)。
    pub rt: Runtime,
    _children: (Child, Child),
    _request_sem: Semaphore,
    _ready_sem: Semaphore,
    _worker_bridge: common::worker_bridge::WorkerBridgeHandle,
    #[cfg(windows)]
    _wake_handles: Vec<windows::Win32::Foundation::HANDLE>,
    #[cfg(windows)]
    _done_handles: Vec<windows::Win32::Foundation::HANDLE>,
}

/// 子プロセスを spawn → handshake → Session / OpenWorkerPool 配信 →
/// pipe loop spawn まで一連の起動処理。 GUI mode と script mode の前段は
/// 完全に同一なのでこの 1 関数でカバー。
pub fn bootstrap_subprocess() -> Result<Bootstrap> {
    let job = Arc::new(JobHandle::new()?);
    let rt = Runtime::new().context("failed to create tokio runtime")?;

    let pid = std::process::id();
    let session = AudioSession {
        shmem_id: shmem_id(pid),
        request_sem_id: request_sem_id(pid),
        ready_sem_id: ready_sem_id(pid),
        sample_rate: SAMPLE_RATE,
        max_frames: MAX_FRAMES,
        channels: CHANNELS as u16,
    };
    let bridge = Arc::new(
        AudioBridgeHandle::create(&session.shmem_id).context("failed to create audio shmem")?,
    );
    let request_sem = Semaphore::create(&session.request_sem_id, 0, 2)
        .context("failed to create request semaphore")?;
    let ready_sem = Semaphore::create(&session.ready_sem_id, 0, 2)
        .context("failed to create ready semaphore")?;
    tracing::info!(?session, "created audio session handles");

    let n_workers = pick_worker_count();
    let worker_bridge_shmem_id = common::plugin_ref::worker_bridge_shmem_id(pid);
    let worker_bridge = common::worker_bridge::WorkerBridgeHandle::create(&worker_bridge_shmem_id)
        .context("failed to create worker_bridge shmem")?;
    let (wake_event_names, done_event_names, wake_handles, done_handles) =
        create_worker_event_pairs(pid, n_workers)?;
    tracing::info!(n_workers, "created plugin worker pool handles");

    let (audio_child, plugin_child, mut audio_server, mut plugin_server) =
        rt.block_on(spawn_and_handshake(&job))?;

    rt.block_on(async {
        write_msg(&mut audio_server, &MainToChild::Session(session.clone())).await?;
        write_msg(&mut plugin_server, &MainToChild::Session(session.clone())).await?;
        let open_pool = MainToChild::OpenWorkerPool {
            n_workers,
            worker_bridge_shmem_id: worker_bridge_shmem_id.clone(),
            wake_event_names: wake_event_names.clone(),
            done_event_names: done_event_names.clone(),
        };
        write_msg(&mut audio_server, &open_pool).await?;
        write_msg(&mut plugin_server, &open_pool).await?;
        anyhow::Ok(())
    })
    .context("failed to send audio session / worker pool")?;

    let (audio_tx, audio_rx) = unbounded_channel::<MainToChild>();
    let (plugin_tx, plugin_rx) = unbounded_channel::<MainToChild>();
    let (incoming_tx, incoming_rx) = unbounded_channel::<ChildToMain>();
    rt.spawn(audio_pipe_loop(audio_server, audio_rx, incoming_tx.clone()));
    rt.spawn(plugin_pipe_loop(plugin_server, plugin_rx, incoming_tx));

    let plugin_db = load_or_build_plugin_db();

    Ok(Bootstrap {
        audio_tx,
        plugin_tx,
        incoming_rx: Some(incoming_rx),
        bridge,
        job,
        plugin_db,
        rt,
        _children: (audio_child, plugin_child),
        _request_sem: request_sem,
        _ready_sem: ready_sem,
        _worker_bridge: worker_bridge,
        #[cfg(windows)]
        _wake_handles: wake_handles,
        #[cfg(windows)]
        _done_handles: done_handles,
    })
}

impl Bootstrap {
    /// `incoming_rx` を 1 度だけ取り出す。 GUI mode は `spawn_incoming_bridge`
    /// に渡すために take、 script mode は host が `&mut self.incoming_rx` で
    /// 直接 poll する (= take せず option として残す)。
    pub fn take_incoming_rx(&mut self) -> Option<UnboundedReceiver<ChildToMain>> {
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

#[cfg(windows)]
#[allow(clippy::type_complexity)]
fn create_worker_event_pairs(
    pid: u32,
    n_workers: u32,
) -> Result<(
    Vec<String>,
    Vec<String>,
    Vec<windows::Win32::Foundation::HANDLE>,
    Vec<windows::Win32::Foundation::HANDLE>,
)> {
    let mut wake_names = Vec::with_capacity(n_workers as usize);
    let mut done_names = Vec::with_capacity(n_workers as usize);
    let mut wakes = Vec::with_capacity(n_workers as usize);
    let mut dones = Vec::with_capacity(n_workers as usize);
    for i in 0..n_workers {
        let wn = common::plugin_ref::worker_wake_event_name(pid, i);
        let dn = common::plugin_ref::worker_done_event_name(pid, i);
        let wh = common::plugin_ref::create_named_event(&wn)
            .with_context(|| format!("failed to create wake event {i}"))?;
        let dh = common::plugin_ref::create_named_event(&dn)
            .with_context(|| format!("failed to create done event {i}"))?;
        wake_names.push(wn);
        done_names.push(dn);
        wakes.push(wh);
        dones.push(dh);
    }
    Ok((wake_names, done_names, wakes, dones))
}

#[cfg(not(windows))]
#[allow(clippy::type_complexity)]
fn create_worker_event_pairs(
    _pid: u32,
    _n_workers: u32,
) -> Result<(Vec<String>, Vec<String>, Vec<()>, Vec<()>)> {
    // 非 Windows ビルドは現状未対応。
    Ok((Vec::new(), Vec::new(), Vec::new(), Vec::new()))
}

async fn spawn_and_handshake(
    job: &JobHandle,
) -> Result<(Child, Child, NamedPipeServer, NamedPipeServer)> {
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

    let (audio_result, plugin_result) = tokio::try_join!(
        handshake(audio_server, ChildKind::Audio),
        handshake(plugin_server, ChildKind::PluginHost),
    )?;
    let (audio_hello, audio_server) = audio_result;
    let (plugin_hello, plugin_server) = plugin_result;
    tracing::info!(?audio_hello, "audio handshake complete");
    tracing::info!(?plugin_hello, "plugin_host handshake complete");

    Ok((audio_child, plugin_child, audio_server, plugin_server))
}

async fn handshake(
    mut server: NamedPipeServer,
    expected: ChildKind,
) -> Result<(ChildToMain, NamedPipeServer)> {
    server.connect().await.context("failed to accept client")?;
    let hello: ChildToMain = read_msg(&mut server).await?;
    let kind = match &hello {
        ChildToMain::Hello { kind, .. } => *kind,
        other => anyhow::bail!("expected Hello from child, got {:?}", other),
    };
    anyhow::ensure!(
        kind == expected,
        "child kind mismatch: expected {:?}, got {:?}",
        expected,
        kind
    );
    write_msg(&mut server, &MainToChild::Ack).await?;
    Ok((hello, server))
}

async fn audio_pipe_loop(
    mut pipe: NamedPipeServer,
    mut rx: UnboundedReceiver<MainToChild>,
    incoming_tx: UnboundedSender<ChildToMain>,
) {
    loop {
        tokio::select! {
            msg = read_msg::<_, ChildToMain>(&mut pipe) => {
                match msg {
                    Ok(m) => {
                        if incoming_tx.send(m).is_err() {
                            tracing::info!("incoming receiver dropped; audio pipe loop exiting");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::info!(error = ?e, "daw_audio pipe closed");
                        break;
                    }
                }
            }
            Some(msg) = rx.recv() => {
                if let Err(e) = write_msg(&mut pipe, &msg).await {
                    tracing::error!(error = ?e, ?msg, "failed to send message to daw_audio");
                    break;
                }
            }
            else => break,
        }
    }
    tracing::info!("audio pipe loop ended");
}

async fn plugin_pipe_loop(
    mut pipe: NamedPipeServer,
    mut rx: UnboundedReceiver<MainToChild>,
    incoming_tx: UnboundedSender<ChildToMain>,
) {
    loop {
        tokio::select! {
            msg = read_msg::<_, ChildToMain>(&mut pipe) => {
                match msg {
                    Ok(m) => {
                        if incoming_tx.send(m).is_err() {
                            tracing::info!("incoming receiver dropped; pipe loop exiting");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::info!(error = ?e, "plugin_host pipe closed");
                        break;
                    }
                }
            }
            Some(msg) = rx.recv() => {
                if let Err(e) = write_msg(&mut pipe, &msg).await {
                    tracing::error!(error = ?e, ?msg, "failed to send message to plugin_host");
                    break;
                }
            }
            else => break,
        }
    }
    tracing::info!("plugin pipe loop ended");
}

fn load_or_build_plugin_db() -> Option<Arc<PluginDatabase>> {
    use common::plugin_db::{default_cache_path, scan_system};
    if let Some(cache) = default_cache_path() {
        match PluginDatabase::load_from_file(&cache) {
            Ok(Some(db)) => {
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
        match scan_system() {
            Ok(db) => {
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
            Err(e) => {
                tracing::error!(error = ?e, "plugin scan failed");
            }
        }
    }
    None
}

const _: Duration = Duration::from_millis(0); // suppress unused import warning
