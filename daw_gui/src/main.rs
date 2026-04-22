mod app;
mod job;
mod subprocess;
mod view;

use anyhow::{Context as _, Result};
use common::audio_bridge::{
    AudioBridgeHandle, CHANNELS, MAX_FRAMES, SAMPLE_RATE, ready_sem_id, request_sem_id, shmem_id,
};
use common::pipe::pipe_path;
use common::protocol::{AudioSession, ChildKind, ChildToMain, MainToChild};
use common::win_sem::Semaphore;
use common::wire::{read_msg, write_msg};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::process::Child;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use vizia::prelude::*;

use crate::app::{AppData, AppEvent};
use crate::job::JobHandle;
use crate::view::{ArrangementView, StatusBarView, TrackInspectorView, TransportView};

fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_gui starting");

    let job = JobHandle::new()?;
    let rt = Runtime::new().context("failed to create tokio runtime")?;

    // Build the audio session names and create shmem / semaphores up front so
    // they exist before the children try to open them.
    let pid = std::process::id();
    let session = AudioSession {
        shmem_id: shmem_id(pid),
        request_sem_id: request_sem_id(pid),
        ready_sem_id: ready_sem_id(pid),
        sample_rate: SAMPLE_RATE,
        max_frames: MAX_FRAMES,
        channels: CHANNELS as u16,
    };
    let _bridge = AudioBridgeHandle::create(&session.shmem_id)
        .context("failed to create audio shmem")?;
    let _request_sem = Semaphore::create(&session.request_sem_id, 0, 2)
        .context("failed to create request semaphore")?;
    let _ready_sem = Semaphore::create(&session.ready_sem_id, 0, 2)
        .context("failed to create ready semaphore")?;
    tracing::info!(?session, "created audio session handles");

    let (audio_child, plugin_child, mut audio_server, mut plugin_server) =
        rt.block_on(spawn_and_handshake(&job))?;

    // Send the session descriptor to both children before any other commands.
    rt.block_on(async {
        write_msg(&mut audio_server, &MainToChild::Session(session.clone())).await?;
        write_msg(&mut plugin_server, &MainToChild::Session(session.clone())).await?;
        anyhow::Ok(())
    })
    .context("failed to send audio session")?;

    let _children = (audio_child, plugin_child);

    let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel::<MainToChild>();
    let (plugin_tx, plugin_rx) = tokio::sync::mpsc::unbounded_channel::<MainToChild>();
    rt.spawn(send_loop(audio_server, audio_rx));
    rt.spawn(send_loop(plugin_server, plugin_rx));

    tracing::info!("opening main window");
    run_gui(audio_tx, plugin_tx)?;
    tracing::info!("daw_gui exiting");
    Ok(())
}

async fn spawn_and_handshake(
    job: &JobHandle,
) -> Result<(Child, Child, NamedPipeServer, NamedPipeServer)> {
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

    let (audio_result, plugin_result) = tokio::try_join!(
        handshake(audio_server, ChildKind::Audio),
        handshake(plugin_server, ChildKind::PluginHost),
    )?;
    let (audio_hello, audio_server) = audio_result;
    let (plugin_hello, plugin_server) = plugin_result;
    tracing::info!(?audio_hello, "audio handshake complete");
    tracing::info!(?plugin_hello, "plugin_host handshake complete");

    Ok((audio_child, plugin_child, audio_server, plugin_server))
}

async fn handshake(
    mut server: NamedPipeServer,
    expected: ChildKind,
) -> Result<(ChildToMain, NamedPipeServer)> {
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
    Ok((hello, server))
}

async fn send_loop(mut pipe: NamedPipeServer, mut rx: UnboundedReceiver<MainToChild>) {
    while let Some(msg) = rx.recv().await {
        if let Err(e) = write_msg(&mut pipe, &msg).await {
            tracing::error!(error = ?e, ?msg, "failed to send message to child");
            break;
        }
    }
    tracing::info!("send loop ended");
}

fn run_gui(
    audio_tx: UnboundedSender<MainToChild>,
    plugin_tx: UnboundedSender<MainToChild>,
) -> Result<()> {
    Application::new(move |cx| {
        cx.set_default_font(&["HackGen Console NF"]);
        AppData::new(audio_tx.clone(), plugin_tx.clone()).build(cx);
        register_shortcuts(cx);

        VStack::new(cx, |cx| {
            build_menu_bar(cx);
            TransportView::new(cx).height(Pixels(44.0));

            HStack::new(cx, |cx| {
                TrackInspectorView::new(cx).width(Pixels(220.0));
                ArrangementView::new(cx).width(Stretch(1.0));
            })
            .height(Stretch(1.0));

            StatusBarView::new(cx).height(Pixels(26.0));
        });
    })
    .title("daw_01")
    .inner_size((1280, 800))
    .run()
    .map_err(|e| anyhow::anyhow!("Vizia application error: {e:?}"))
}

fn register_shortcuts(cx: &mut Context) {
    Keymap::from(vec![
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyN),
            KeymapEntry::new(AppEvent::New, |cx| cx.emit(AppEvent::New)),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyO),
            KeymapEntry::new(AppEvent::Open, |cx| cx.emit(AppEvent::Open)),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyS),
            KeymapEntry::new(AppEvent::Save, |cx| cx.emit(AppEvent::Save)),
        ),
        (
            KeyChord::new(Modifiers::CTRL | Modifiers::SHIFT, Code::KeyS),
            KeymapEntry::new(AppEvent::SaveAs, |cx| cx.emit(AppEvent::SaveAs)),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::KeyH),
            KeymapEntry::new(AppEvent::CursorLeft, |cx| cx.emit(AppEvent::CursorLeft)),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::KeyJ),
            KeymapEntry::new(AppEvent::CursorDown, |cx| cx.emit(AppEvent::CursorDown)),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::KeyK),
            KeymapEntry::new(AppEvent::CursorUp, |cx| cx.emit(AppEvent::CursorUp)),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::KeyL),
            KeymapEntry::new(AppEvent::CursorRight, |cx| cx.emit(AppEvent::CursorRight)),
        ),
    ])
    .build(cx);
}

fn build_menu_bar(cx: &mut Context) {
    MenuBar::new(cx, |cx| {
        Submenu::new(
            cx,
            |cx| Label::new(cx, "File"),
            |cx| {
                menu_item(cx, "New", "Ctrl+N", AppEvent::New);
                menu_item(cx, "Open...", "Ctrl+O", AppEvent::Open);
                Divider::new(cx);
                menu_item(cx, "Save", "Ctrl+S", AppEvent::Save);
                menu_item(cx, "Save As...", "Ctrl+Shift+S", AppEvent::SaveAs);
            },
        );
        Submenu::new(
            cx,
            |cx| Label::new(cx, "Track"),
            |cx| {
                menu_item(cx, "Add Vocal Track", "", AppEvent::AddVocalTrack);
                menu_item(cx, "Remove Last Track", "", AppEvent::RemoveLastTrack);
            },
        );
    });
}

fn menu_item(cx: &mut Context, label: &'static str, shortcut: &'static str, event: AppEvent) {
    MenuButton::new(
        cx,
        move |cx| cx.emit(event),
        move |cx| {
            HStack::new(cx, |cx| {
                Label::new(cx, label);
                Label::new(cx, shortcut).class("shortcut");
            })
            .gap(Stretch(1.0))
        },
    );
}
