mod clap_host;
mod plugin;
mod scan;

use anyhow::{Context, Result};
use common::protocol::{ChildKind, MainToChild};
use common::wire::read_msg;
use tokio::net::windows::named_pipe::NamedPipeClient;

#[tokio::main]
async fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_plugin_host started");

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    let pipe = common::client::perform_handshake(&pipe_name, ChildKind::PluginHost).await?;
    tracing::info!("daw_plugin_host handshake complete");

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

    let _plugin = pick_plugin(&candidates);

    tracing::info!("awaiting shutdown");
    wait_for_pipe_close(pipe).await;
    tracing::info!("daw_plugin_host exiting");
    Ok(())
    // `_plugin` drops here → destroy / deinit / Library unload
}

fn pick_plugin(candidates: &[std::path::PathBuf]) -> Option<plugin::Plugin> {
    // Pass 1: prefer instruments so we can actually play notes.
    for path in candidates {
        match plugin::Plugin::load_matching(path, plugin::is_instrument_features) {
            Ok(Some(p)) => {
                tracing::info!(path = %path.display(), "loaded instrument plugin");
                return Some(p);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = ?e, path = %path.display(), "plugin scan failed"),
        }
    }
    // Pass 2: fallback to the first loadable plugin of any kind.
    for path in candidates {
        match plugin::Plugin::load(path) {
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
