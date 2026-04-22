use anyhow::Result;

fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_plugin_host started");
    Ok(())
}
