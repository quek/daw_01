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
    /// 子プロセス自動再起動用 supervisor。 `audio_pipe_loop` /
    /// `plugin_pipe_loop` が break して `ChildToMain::ChildDisconnected`
    /// を発火したら、 AppData が `supervisor.respawn(kind)` で新 child
    /// を spawn + handshake + Session/OpenWorkerPool 再送し、 新 tx を
    /// 受け取って AppData の `audio_tx` / `plugin_tx` を差し替える。
    pub supervisor: Arc<ChildSupervisor>,
    _request_sem: Semaphore,
    _ready_sem: Semaphore,
    _worker_bridge: common::worker_bridge::WorkerBridgeHandle,
    #[cfg(windows)]
    _wake_handles: Vec<windows::Win32::Foundation::HANDLE>,
    #[cfg(windows)]
    _done_handles: Vec<windows::Win32::Foundation::HANDLE>,
}

/// 子プロセス re-spawn 用の context。 daw_audio / daw_plugin_host が
/// pipe break で死亡したとき、 `respawn(kind)` で新 child を spawn +
/// handshake + Session/OpenWorkerPool 再送し、 新 `UnboundedSender<
/// MainToChild>` を返す。
///
/// 子プロセスは `Mutex<Option<Child>>` で保持 (= Drop で自動 kill、
/// 再 spawn 時は古い child を `start_kill` してから置換)。 OS 上の
/// shared memory / semaphore は `Bootstrap` 側で保持し続けるので、
/// 新 child は OpenShared で join できる。
pub struct ChildSupervisor {
    pid: u32,
    rt_handle: tokio::runtime::Handle,
    job: Arc<JobHandle>,
    session: AudioSession,
    n_workers: u32,
    worker_bridge_shmem_id: String,
    wake_event_names: Vec<String>,
    done_event_names: Vec<String>,
    incoming_tx: UnboundedSender<ChildToMain>,
    audio_child: std::sync::Mutex<Option<Child>>,
    plugin_child: std::sync::Mutex<Option<Child>>,
}

impl ChildSupervisor {
    /// 指定 kind の子プロセスを再起動し、 新しい IPC 送信用 channel
    /// (`UnboundedSender<MainToChild>`) を返す。 失敗時は元の child は
    /// 既に消えている (= caller が status_message で通知すれば user が
    /// 手動でリトライ or 再起動できる)。
    pub fn respawn(&self, kind: ChildKind) -> Result<UnboundedSender<MainToChild>> {
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

        // 2) 新 server を作成 → 子 spawn → handshake → Session +
        // OpenWorkerPool 送信、 を tokio runtime 内で実行。
        let session = self.session.clone();
        let n_workers = self.n_workers;
        let worker_bridge_shmem_id = self.worker_bridge_shmem_id.clone();
        let wake_event_names = self.wake_event_names.clone();
        let done_event_names = self.done_event_names.clone();
        let job = self.job.clone();
        let pipe_for_spawn = pipe.clone();
        let (server, child) = self
            .rt_handle
            .block_on(async move {
                let server = ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(&pipe_for_spawn)
                    .with_context(|| format!("failed to create pipe {pipe_for_spawn}"))?;
                let child = crate::subprocess::spawn_sibling(binary, [&pipe_for_spawn])?;
                job.assign(&child)?;
                // handshake に timeout を付ける: respawn は GUI main thread の
                // block_on で呼ばれるので、 新 child が Hello を送る前にハング/
                // crash すると main thread が永久に固まる。 5s で諦めて Err を返す。
                let (_hello, server) =
                    tokio::time::timeout(std::time::Duration::from_secs(5), handshake(server, kind))
                        .await
                        .context("respawn handshake timed out (new child died before handshake?)")??;
                anyhow::Ok((server, child))
            })?;

        // 3) Session + OpenWorkerPool を新 pipe に流す。 既存 caller
        // (= AppData) は state restore で SetProjectDir + LoadSong を
        // 続けて送ってくる前提なので、 ここでは「engine が再生可能な
        // 最小限の state」 だけを再構築する。
        let open_pool = MainToChild::OpenWorkerPool {
            n_workers,
            worker_bridge_shmem_id,
            wake_event_names,
            done_event_names,
        };
        let mut server = server;
        self.rt_handle.block_on(async {
            write_msg(&mut server, &MainToChild::Session(session)).await?;
            write_msg(&mut server, &open_pool).await?;
            anyhow::Ok(())
        })?;

        // 4) 新 channel + pipe loop spawn。 incoming_tx は既存
        // (= AppEvent 経路に紐づいた) のを clone して渡す。 古い pipe
        // loop が drop されているので、 新 loop だけが流す。
        let (tx, rx) = unbounded_channel::<MainToChild>();
        let incoming_tx = self.incoming_tx.clone();
        match kind {
            ChildKind::Audio => {
                self.rt_handle.spawn(audio_pipe_loop(server, rx, incoming_tx));
            }
            ChildKind::PluginHost => {
                self.rt_handle.spawn(plugin_pipe_loop(server, rx, incoming_tx));
            }
        }

        // 5) child を slot に保存 (= 次回 respawn まで生かし、 Drop で
        // kill される)。
        if let Ok(mut guard) = child_slot.lock() {
            *guard = Some(child);
        }
        tracing::info!(?kind, "child respawned");
        Ok(tx)
    }
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
    rt.spawn(plugin_pipe_loop(plugin_server, plugin_rx, incoming_tx.clone()));

    let plugin_db = load_or_build_plugin_db();

    let supervisor = Arc::new(ChildSupervisor {
        pid,
        rt_handle: rt.handle().clone(),
        job: job.clone(),
        session: session.clone(),
        n_workers,
        worker_bridge_shmem_id: worker_bridge_shmem_id.clone(),
        wake_event_names: wake_event_names.clone(),
        done_event_names: done_event_names.clone(),
        incoming_tx,
        audio_child: std::sync::Mutex::new(Some(audio_child)),
        plugin_child: std::sync::Mutex::new(Some(plugin_child)),
    });

    Ok(Bootstrap {
        audio_tx,
        plugin_tx,
        incoming_rx: Some(incoming_rx),
        bridge,
        job,
        plugin_db,
        rt,
        supervisor,
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
    pipe: NamedPipeServer,
    mut rx: UnboundedReceiver<MainToChild>,
    incoming_tx: UnboundedSender<ChildToMain>,
) {
    // read/write half を別タスクに分けて read を絶対 cancel しない
    // (詳細は plugin_pipe_loop / daw_plugin_host::pipe_loop)。旧 select! 構造は
    // read_msg を write 発火で途中 cancel し stream を desync させていた。
    let (mut read_half, mut write_half) = tokio::io::split(pipe);
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = write_msg(&mut write_half, &msg).await {
                tracing::error!(error = ?e, ?msg, "failed to send message to daw_audio");
                break;
            }
        }
    });
    loop {
        match read_msg::<_, ChildToMain>(&mut read_half).await {
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
    writer.abort();
    // 子プロセス側 (or 自前 write) が die / decode 失敗で抜けたケース。
    // 上位 (AppData::handle_event) で respawn + state restore に拾ってもらう。
    let _ = incoming_tx.send(ChildToMain::ChildDisconnected { kind: ChildKind::Audio });
    tracing::info!("audio pipe loop ended");
}

async fn plugin_pipe_loop(
    pipe: NamedPipeServer,
    mut rx: UnboundedReceiver<MainToChild>,
    incoming_tx: UnboundedSender<ChildToMain>,
) {
    // read_msg / write_msg (wire.rs の read_exact / write_all) は
    // cancellation-unsafe。旧構造の `select! { read_msg, rx.recv()=>write_msg }`
    // は、大きい message (例: 3.8MB の LoadSong response) の read 途中で write が
    // 発火すると read future を drop し消費済みバイトを捨てて stream を desync
    // させる (1GB garbage length → 切断ループの真因)。read/write half を別タスクに
    // 分けて read を絶対 cancel しない (daw_plugin_host::pipe_loop と同じ)。
    let (mut read_half, mut write_half) = tokio::io::split(pipe);
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = write_msg(&mut write_half, &msg).await {
                tracing::error!(error = ?e, ?msg, "failed to send message to plugin_host");
                break;
            }
        }
    });
    loop {
        match read_msg::<_, ChildToMain>(&mut read_half).await {
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
    writer.abort();
    let _ = incoming_tx.send(ChildToMain::ChildDisconnected { kind: ChildKind::PluginHost });
    tracing::info!("plugin pipe loop ended");
}

fn load_or_build_plugin_db() -> Option<Arc<PluginDatabase>> {
    use common::plugin_db::{default_cache_path, scan_system};
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
