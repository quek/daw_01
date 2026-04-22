mod subprocess;

use anyhow::Result;

fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_gui starting");

    let mut audio = subprocess::spawn_sibling("daw_audio")?;
    let mut plugin_host = subprocess::spawn_sibling("daw_plugin_host")?;

    let audio_status = audio.wait()?;
    tracing::info!(status = %audio_status, "daw_audio exited");

    let plugin_status = plugin_host.wait()?;
    tracing::info!(status = %plugin_status, "daw_plugin_host exited");

    Ok(())
}
