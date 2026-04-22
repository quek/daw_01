use anyhow::Result;

fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_audio started");
    Ok(())
}
