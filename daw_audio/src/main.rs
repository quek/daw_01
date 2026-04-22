use anyhow::{Context, Result};
use common::protocol::ChildKind;

#[tokio::main]
async fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_audio started");

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    common::client::perform_handshake(&pipe_name, ChildKind::Audio).await?;
    tracing::info!("daw_audio handshake complete, awaiting shutdown");
    std::future::pending::<()>().await;
    Ok(())
}
