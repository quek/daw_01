mod clap_host;
mod plugin;
mod scan;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use common::audio_bridge::{AudioBridgeHandle, CHANNELS};
use common::protocol::{AudioSession, ChildKind, MainToChild};
use common::win_sem::Semaphore;
use common::wire::read_msg;
use tokio::net::windows::named_pipe::NamedPipeClient;

use crate::plugin::Plugin;

#[tokio::main]
async fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_plugin_host started");

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    let mut pipe = common::client::perform_handshake(&pipe_name, ChildKind::PluginHost).await?;
    tracing::info!("daw_plugin_host handshake complete");

    let session = common::client::read_session(&mut pipe).await?;
    tracing::info!(?session, "audio session received");

    let plugin = if let Some(path) = std::env::var_os("DAW_CLAP_PATH") {
        let path = std::path::PathBuf::from(path);
        tracing::info!(path = %path.display(), "DAW_CLAP_PATH override");
        match Plugin::load(&path) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to load override plugin");
                None
            }
        }
    } else {
        let candidates = match scan::scan_system_clap_directory() {
            Ok(list) => list,
            Err(e) => {
                tracing::error!(error = ?e, "failed to scan CLAP directory");
                Vec::new()
            }
        };
        tracing::info!(count = candidates.len(), "CLAP plugins discovered");
        for p in &candidates {
            tracing::info!(path = %p.display(), "CLAP plugin found");
        }
        pick_plugin(&candidates)
    };

    let audio_handle = match plugin {
        Some(mut p) => match p.activate(
            session.sample_rate as f64,
            64,
            session.max_frames,
        ) {
            Ok(()) => spawn_audio_thread(p, &session).ok(),
            Err(e) => {
                tracing::error!(error = ?e, "plugin activate failed");
                None
            }
        },
        None => None,
    };

    tracing::info!("awaiting shutdown");
    wait_for_pipe_close(pipe).await;
    tracing::info!("daw_plugin_host shutting down");

    if let Some(h) = audio_handle {
        h.shutdown();
    }

    tracing::info!("daw_plugin_host exiting");
    Ok(())
}

fn pick_plugin(candidates: &[std::path::PathBuf]) -> Option<Plugin> {
    for path in candidates {
        match Plugin::load_matching(path, plugin::is_instrument_features) {
            Ok(Some(p)) => {
                tracing::info!(path = %path.display(), "loaded instrument plugin");
                return Some(p);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = ?e, path = %path.display(), "plugin scan failed"),
        }
    }
    for path in candidates {
        match Plugin::load(path) {
            Ok(p) => {
                tracing::warn!(
                    path = %path.display(),
                    "no instrument found; loaded first plugin as fallback"
                );
                return Some(p);
            }
            Err(e) => tracing::warn!(error = ?e, path = %path.display(), "fallback load failed"),
        }
    }
    None
}

/// RAII handle for the audio thread. `shutdown()` joins the thread and
/// deactivates the plugin on the main thread before it is dropped.
struct AudioHandle {
    handle: Option<JoinHandle<Result<Plugin>>>,
    shutdown: Arc<AtomicBool>,
    request_sem: Arc<Semaphore>,
}

impl AudioHandle {
    fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Wake the thread if it is blocked on the request semaphore.
        let _ = self.request_sem.release();

        let Some(handle) = self.handle.take() else {
            return;
        };
        match handle.join() {
            Ok(Ok(mut plugin)) => {
                plugin.deactivate();
                // Drop here on the main thread triggers destroy on the main thread.
                drop(plugin);
            }
            Ok(Err(e)) => tracing::error!(error = ?e, "audio thread errored"),
            Err(_) => tracing::error!("audio thread panicked"),
        }
    }
}

fn spawn_audio_thread(mut plugin: Plugin, session: &AudioSession) -> Result<AudioHandle> {
    let bridge = Arc::new(
        AudioBridgeHandle::open(&session.shmem_id).context("failed to open audio shmem")?,
    );
    let request_sem = Arc::new(
        Semaphore::open(&session.request_sem_id).context("failed to open request semaphore")?,
    );
    let ready_sem = Arc::new(
        Semaphore::open(&session.ready_sem_id).context("failed to open ready semaphore")?,
    );
    let shutdown = Arc::new(AtomicBool::new(false));

    plugin
        .start_processing()
        .context("plugin.start_processing failed")?;

    let th_bridge = Arc::clone(&bridge);
    let th_req = Arc::clone(&request_sem);
    let th_ready = Arc::clone(&ready_sem);
    let th_shutdown = Arc::clone(&shutdown);

    let handle = std::thread::Builder::new()
        .name("clap-audio".into())
        .spawn(move || run_audio(plugin, th_bridge, th_req, th_ready, th_shutdown))
        .context("failed to spawn audio thread")?;

    Ok(AudioHandle {
        handle: Some(handle),
        shutdown,
        request_sem,
    })
}

fn run_audio(
    mut plugin: Plugin,
    bridge: Arc<AudioBridgeHandle>,
    request_sem: Arc<Semaphore>,
    ready_sem: Arc<Semaphore>,
    shutdown: Arc<AtomicBool>,
) -> Result<Plugin> {
    let out_channels = CHANNELS as usize;
    tracing::info!("audio thread running");
    loop {
        match request_sem.wait_timeout_ms(100) {
            Ok(true) => {}
            Ok(false) => {
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            Err(e) => {
                tracing::error!(error = ?e, "request semaphore wait failed");
                break;
            }
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        let frames = bridge.frames_requested();
        if let Err(e) = plugin.process(frames) {
            tracing::error!(error = ?e, "plugin.process failed");
            break;
        }

        // Copy planar plugin output (take first 2 channels) to interleaved shmem.
        let n = frames as usize;
        let left = plugin.output_buffer(0);
        let right = plugin.output_buffer(1).or(left);
        unsafe {
            let dst = bridge.samples_ptr();
            match (left, right) {
                (Some(l), Some(r)) => {
                    for i in 0..n {
                        *dst.add(i * out_channels) = l[i];
                        *dst.add(i * out_channels + 1) = r[i];
                    }
                }
                _ => {
                    // Plugin has no output channels — silence.
                    for i in 0..n * out_channels {
                        *dst.add(i) = 0.0;
                    }
                }
            }
        }

        if let Err(e) = ready_sem.release() {
            tracing::error!(error = ?e, "ready semaphore release failed");
            break;
        }
    }
    plugin.stop_processing();
    tracing::info!("audio thread exiting");
    Ok(plugin)
}

async fn wait_for_pipe_close(mut pipe: NamedPipeClient) {
    loop {
        match read_msg::<_, MainToChild>(&mut pipe).await {
            Ok(msg) => {
                tracing::info!(?msg, "received (ignored by plugin host for now)");
            }
            Err(e) => {
                tracing::info!(error = ?e, "pipe ended");
                break;
            }
        }
    }
}
