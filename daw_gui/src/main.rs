mod app;
mod job;
mod subprocess;

use anyhow::{Context, Result};
use common::pipe::pipe_path;
use common::protocol::{ChildKind, ChildToMain, MainToChild};
use common::wire::{read_msg, write_msg};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::process::Child;
use tokio::runtime::Runtime;
use vizia::prelude::*;

use crate::job::JobHandle;

fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_gui starting");

    let job = JobHandle::new()?;
    let rt = Runtime::new().context("failed to create tokio runtime")?;

    // Children are held for the lifetime of `main`; dropping them (or `job`) triggers
    // Job Object cleanup via JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE when we exit.
    let _children = rt.block_on(spawn_and_handshake(&job))?;
    tracing::info!("opening main window");

    run_gui()?;
    tracing::info!("daw_gui exiting");
    Ok(())
}

async fn spawn_and_handshake(job: &JobHandle) -> Result<(Child, Child)> {
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

    let (audio_hello, plugin_hello) = tokio::try_join!(
        handshake(audio_server, ChildKind::Audio),
        handshake(plugin_server, ChildKind::PluginHost),
    )?;
    tracing::info!(?audio_hello, "audio handshake complete");
    tracing::info!(?plugin_hello, "plugin_host handshake complete");

    Ok((audio_child, plugin_child))
}

async fn handshake(mut server: NamedPipeServer, expected: ChildKind) -> Result<ChildToMain> {
    server.connect().await.context("failed to accept client")?;
    let hello: ChildToMain = read_msg(&mut server).await?;
    let kind = match &hello {
        ChildToMain::Hello { kind, .. } => *kind,
    };
    anyhow::ensure!(
        kind == expected,
        "child kind mismatch: expected {:?}, got {:?}",
        expected,
        kind
    );
    write_msg(&mut server, &MainToChild::Ack).await?;
    Ok(hello)
}

fn run_gui() -> Result<()> {
    Application::new(|cx| {
        app::AppData.build(cx);
        VStack::new(cx, |cx| {
            Label::new(cx, "daw_01").font_size(32.0);
        })
        .alignment(Alignment::Center);
    })
    .title("daw_01")
    .inner_size((1280, 800))
    .run()
    .map_err(|e| anyhow::anyhow!("Vizia application error: {e:?}"))
}
