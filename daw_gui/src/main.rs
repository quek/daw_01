mod app;
mod job;
mod midi;
mod subprocess;
mod view;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use common::audio_bridge::{
    AudioBridgeHandle, CHANNELS, MAX_FRAMES, SAMPLE_RATE, ready_sem_id, request_sem_id, shmem_id,
};
use common::pipe::pipe_path;
use common::protocol::{AudioSession, ChildKind, ChildToMain, MainToChild};
use common::win_sem::Semaphore;
use common::wire::{read_msg, write_msg};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::process::Child;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedReceiver;
use winit::dpi::LogicalSize;
use winit::event_loop::EventLoopProxy;
use winit::window::WindowAttributes;

use crate::app::{AppData, AppEvent};
use crate::job::JobHandle;
use crate::view::runner::{run as run_runner, RunnerInit};

fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_gui starting");

    let job = JobHandle::new()?;
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
        AudioBridgeHandle::create(&session.shmem_id)
            .context("failed to create audio shmem")?,
    );
    let _request_sem = Semaphore::create(&session.request_sem_id, 0, 2)
        .context("failed to create request semaphore")?;
    let _ready_sem = Semaphore::create(&session.ready_sem_id, 0, 2)
        .context("failed to create ready semaphore")?;
    tracing::info!(?session, "created audio session handles");

    let (audio_child, plugin_child, mut audio_server, mut plugin_server) =
        rt.block_on(spawn_and_handshake(&job))?;

    rt.block_on(async {
        write_msg(&mut audio_server, &MainToChild::Session(session.clone())).await?;
        write_msg(&mut plugin_server, &MainToChild::Session(session.clone())).await?;
        anyhow::Ok(())
    })
    .context("failed to send audio session")?;

    // Children プロセスは run_runner 終了まで生かす。
    let _children = (audio_child, plugin_child);

    let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel::<MainToChild>();
    let (plugin_tx, plugin_rx) = tokio::sync::mpsc::unbounded_channel::<MainToChild>();
    let (incoming_tx, incoming_rx) = tokio::sync::mpsc::unbounded_channel::<ChildToMain>();
    rt.spawn(send_loop(audio_server, audio_rx));
    rt.spawn(plugin_pipe_loop(plugin_server, plugin_rx, incoming_tx));

    let plugin_db = load_or_build_plugin_db();

    tracing::info!("opening main window");

    let bridge_for_app = Arc::clone(&bridge);
    let plugin_db_for_app = plugin_db.clone();

    // run_runner は AppData を build する closure を呼んだあと、winit イベントループへ
    // 移行する。clone した AudioBridgeHandle / IPC receiver は build_app の中で
    // background スレッドへ move され、EventLoopProxy 経由で AppEvent を投入し続ける。
    let init = RunnerInit {
        window_attrs: WindowAttributes::default()
            .with_title("daw_01")
            .with_inner_size(LogicalSize::new(1280.0, 800.0)),
        build_app: Box::new(move |proxy: EventLoopProxy<AppEvent>| {
            // AppData 本体を組み立てる。
            let app = AppData::new(audio_tx, plugin_tx, None, plugin_db_for_app, proxy.clone());

            // ----- 背景スレッド群 -----
            spawn_playhead_poller(bridge_for_app, proxy.clone());
            spawn_autosave_timer(proxy.clone());
            spawn_midi_input(proxy.clone());
            spawn_incoming_bridge(incoming_rx, proxy.clone());

            app
        }),
    };

    if let Err(e) = run_runner(init) {
        tracing::error!(error = ?e, "event loop error");
    }

    tracing::info!("daw_gui exiting");
    drop(bridge);
    Ok(())
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

    let audio_child = subprocess::spawn_sibling("daw_audio", [&audio_pipe])?;
    job.assign(&audio_child)?;
    let plugin_child = subprocess::spawn_sibling("daw_plugin_host", [&plugin_pipe])?;
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

async fn send_loop(mut pipe: NamedPipeServer, mut rx: UnboundedReceiver<MainToChild>) {
    while let Some(msg) = rx.recv().await {
        if let Err(e) = write_msg(&mut pipe, &msg).await {
            tracing::error!(error = ?e, ?msg, "failed to send message to child");
            break;
        }
    }
    tracing::info!("send loop ended");
}

async fn plugin_pipe_loop(
    mut pipe: NamedPipeServer,
    mut rx: UnboundedReceiver<MainToChild>,
    incoming_tx: tokio::sync::mpsc::UnboundedSender<ChildToMain>,
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

fn load_or_build_plugin_db() -> Option<Arc<common::plugin_db::PluginDatabase>> {
    use common::plugin_db::{PluginDatabase, default_cache_path, scan_system};
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

/// IPC bridge: tokio mpsc → EventLoopProxy。background スレッドで blocking
/// recv し、`AppEvent` に変換して proxy へ送る。
fn spawn_incoming_bridge(
    mut rx: UnboundedReceiver<ChildToMain>,
    proxy: EventLoopProxy<AppEvent>,
) {
    std::thread::spawn(move || {
        while let Some(msg) = rx.blocking_recv() {
            let event = match msg {
                ChildToMain::SlotGuiOpened { track, slot, width, height } => Some(
                    AppEvent::GuiOpenedFromChild { track, slot, width, height },
                ),
                ChildToMain::SlotGuiRequestResize { track, slot, width, height } => Some(
                    AppEvent::GuiRequestResizeFromChild { track, slot, width, height },
                ),
                ChildToMain::SlotGuiClosed { track, slot } => {
                    Some(AppEvent::GuiClosedFromChild { track, slot })
                }
                ChildToMain::ExportWavComplete { error } => {
                    Some(AppEvent::ExportWavComplete { error })
                }
                ChildToMain::SlotPluginLoaded { track, slot, id, name } => {
                    Some(AppEvent::SlotPluginLoadedFromChild { track, slot, id, name })
                }
                ChildToMain::SlotPluginState { .. } => None,
                ChildToMain::AllPluginStates { entries } => {
                    Some(AppEvent::AllStatesReceived(entries))
                }
                ChildToMain::Hello { .. } => None,
            };
            if let Some(event) = event
                && proxy.send_event(event).is_err()
            {
                break;
            }
        }
        tracing::info!("incoming bridge exited");
    });
}

fn spawn_midi_input(proxy: EventLoopProxy<AppEvent>) {
    let proxy_for_midi = proxy.clone();
    std::thread::spawn(move || {
        match crate::midi::open_default_input(proxy_for_midi) {
            Ok(Some(handle)) => {
                let name = handle.port_name.clone();
                Box::leak(Box::new(handle));
                let _ = proxy.send_event(AppEvent::MidiInputOpened(Some(name)));
            }
            Ok(None) => {
                tracing::info!("no MIDI input ports available");
                let _ = proxy.send_event(AppEvent::MidiInputOpened(None));
            }
            Err(e) => {
                tracing::warn!(error = ?e, "failed to open MIDI input");
                let _ = proxy.send_event(AppEvent::MidiInputOpened(None));
            }
        }
    });
}

fn spawn_autosave_timer(proxy: EventLoopProxy<AppEvent>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(30));
            if proxy.send_event(AppEvent::AutosaveTick).is_err() {
                break;
            }
        }
    });
}

fn spawn_playhead_poller(bridge: Arc<AudioBridgeHandle>, proxy: EventLoopProxy<AppEvent>) {
    std::thread::spawn(move || {
        let mut peaks_buf: Vec<(f32, f32)> = Vec::with_capacity(common::audio_bridge::MAX_TRACKS);
        loop {
            std::thread::sleep(Duration::from_millis(33));
            let samples = bridge.playhead_samples();
            let (peak_l, peak_r) = bridge.peaks();
            if proxy
                .send_event(AppEvent::Tick { samples, peak_l, peak_r })
                .is_err()
            {
                break;
            }
            bridge.track_peaks(&mut peaks_buf);
            if proxy
                .send_event(AppEvent::TrackPeaksTick(peaks_buf.clone()))
                .is_err()
            {
                break;
            }
        }
    });
}
