use anyhow::{Context, Result};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

use crate::protocol::{AudioSession, ChildKind, ChildToMain, MainToChild};
use crate::wire::{read_msg, write_msg};

/// Opens the named pipe, performs the handshake, and returns the open
/// client handle for continued communication.
pub async fn perform_handshake(pipe_name: &str, kind: ChildKind) -> Result<NamedPipeClient> {
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
    anyhow::ensure!(
        ack == MainToChild::Ack,
        "expected Ack from parent, got {:?}",
        ack
    );
    tracing::info!(?ack, "received from parent");
    Ok(client)
}

/// Reads the next message and expects it to be `MainToChild::Session`.
pub async fn read_session(client: &mut NamedPipeClient) -> Result<AudioSession> {
    match read_msg::<_, MainToChild>(client).await? {
        MainToChild::Session(s) => {
            tracing::info!(?s, "received audio session");
            Ok(s)
        }
        other => anyhow::bail!("expected Session, got {:?}", other),
    }
}
