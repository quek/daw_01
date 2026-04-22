use anyhow::{Context, Result};
use tokio::net::windows::named_pipe::ClientOptions;

use crate::protocol::{ChildKind, ChildToMain, MainToChild};
use crate::wire::{read_msg, write_msg};

pub async fn perform_handshake(pipe_name: &str, kind: ChildKind) -> Result<()> {
    let mut client = ClientOptions::new()
        .open(pipe_name)
        .with_context(|| format!("failed to open pipe {pipe_name}"))?;

    let hello = ChildToMain::Hello {
        kind,
        pid: std::process::id(),
    };
    write_msg(&mut client, &hello).await?;
    tracing::info!(?hello, "sent Hello");

    let ack: MainToChild = read_msg(&mut client).await?;
    tracing::info!(?ack, "received from parent");
    Ok(())
}
