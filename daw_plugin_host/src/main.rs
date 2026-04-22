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

    let _plugin = match candidates.first() {
        Some(first) => match plugin::Plugin::load(first) {
            Ok(p) => {
                tracing::info!(path = %first.display(), "plugin loaded");
                Some(p)
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %first.display(), "failed to load plugin");
                None
            }
        },
        None => {
            tracing::warn!("no CLAP plugins found");
            None
        }
    };

    tracing::info!("awaiting shutdown");
    wait_for_pipe_close(pipe).await;
    tracing::info!("daw_plugin_host exiting");
    Ok(())
    // `_plugin` drops here → destroy / deinit / Library unload
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
