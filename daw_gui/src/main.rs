mod app;
mod job;
mod subprocess;
mod view;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
use crate::view::{
    ArrangementView, PluginPickerView, StatusBarView, TrackInspectorView, TransportView,
};

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
    let bridge = Arc::new(
        AudioBridgeHandle::create(&session.shmem_id)
            .context("failed to create audio shmem")?,
    );
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
    // Incoming `ChildToMain` from plugin_host (GUI callbacks etc). The
    // receiver is handed to `run_gui` which spawns a blocking bridge on a
    // Vizia `cx.spawn` worker to deliver the messages as AppEvents.
    let (incoming_tx, incoming_rx) = tokio::sync::mpsc::unbounded_channel::<ChildToMain>();
    rt.spawn(send_loop(audio_server, audio_rx));
    rt.spawn(plugin_pipe_loop(plugin_server, plugin_rx, incoming_tx));

    // Build (or reuse cached) plugin database. Scanning can be slow on first
    // launch; subsequent launches read %LOCALAPPDATA%\daw_01\plugin_database.json.
    let plugin_db = load_or_build_plugin_db();

    // No automatic plugin load: the previous version picked the first CLAP
    // under `%COMMONPROGRAMFILES%\CLAP` and applied it to Track 1, which
    // also force-created a Track 1 via the `on_plugin_loaded_from_child`
    // path. That made the very first "+ Vocal Track" click look like it
    // added *two* tracks (Track 1 from the auto-load + the new Track 2).
    // Users must now pick plugins explicitly via the Track Inspector.
    let clap_plugin_path: Option<PathBuf> = None;

    tracing::info!("opening main window");
    run_gui(
        audio_tx,
        plugin_tx,
        clap_plugin_path,
        plugin_db,
        Arc::clone(&bridge),
        incoming_rx,
    )?;
    tracing::info!("daw_gui exiting");
    drop(bridge);
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
        other => anyhow::bail!("expected Hello from child, got {:?}", other),
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

/// Bidirectional pipe loop for daw_plugin_host. Multiplexes outgoing
/// `MainToChild` commands and incoming `ChildToMain` callbacks on the same
/// pipe using `tokio::select!`, so there is no contention and no need to
/// clone the pipe handle.
async fn plugin_pipe_loop(
    mut pipe: NamedPipeServer,
    mut rx: UnboundedReceiver<MainToChild>,
    incoming_tx: tokio::sync::mpsc::UnboundedSender<ChildToMain>,
) {
    loop {
        tokio::select! {
            msg = read_msg::<_, ChildToMain>(&mut pipe) => {
                match msg {
                    Ok(m) => {
                        if incoming_tx.send(m).is_err() {
                            tracing::info!("incoming receiver dropped; pipe loop exiting");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::info!(error = ?e, "plugin_host pipe closed");
                        break;
                    }
                }
            }
            Some(msg) = rx.recv() => {
                if let Err(e) = write_msg(&mut pipe, &msg).await {
                    tracing::error!(error = ?e, ?msg, "failed to send message to plugin_host");
                    break;
                }
            }
            else => break,
        }
    }
    tracing::info!("plugin pipe loop ended");
}

fn run_gui(
    audio_tx: UnboundedSender<MainToChild>,
    plugin_tx: UnboundedSender<MainToChild>,
    clap_plugin_path: Option<PathBuf>,
    plugin_db: Option<Arc<common::plugin_db::PluginDatabase>>,
    bridge: Arc<AudioBridgeHandle>,
    incoming_rx: UnboundedReceiver<ChildToMain>,
) -> Result<()> {
    // `incoming_rx` must move into the cx.spawn closure exactly once; wrap
    // in Option so the outer `Application::new` closure (FnMut) can consume
    // it on first call.
    let mut incoming_rx_slot = Some(incoming_rx);
    Application::new(move |cx| {
        cx.set_default_font(&["HackGen Console NF"]);
        // Default theme gives `list list-item { height: 30px; }` which leaves
        // a visible gap between tracker rows. Override for our tracker list
        // so each row sizes to its Label.
        let _ = cx.add_stylesheet(TRACKER_CSS);
        AppData::new(
            audio_tx.clone(),
            plugin_tx.clone(),
            clap_plugin_path.clone(),
            plugin_db.clone(),
        )
        .build(cx);
        register_shortcuts(cx);
        spawn_playhead_poller(cx, Arc::clone(&bridge));
        if let Some(rx) = incoming_rx_slot.take() {
            spawn_incoming_bridge(cx, rx);
        }

        VStack::new(cx, |cx| {
            build_menu_bar(cx);
            TransportView::new(cx).height(Pixels(44.0));

            HStack::new(cx, |cx| {
                TrackInspectorView::new(cx).width(Pixels(280.0));
                ArrangementView::new(cx).width(Stretch(1.0));
            })
            .height(Stretch(1.0));

            StatusBarView::new(cx).height(Pixels(26.0));
        });

        // Modal plugin-picker overlay. Shown only when requested, sits on
        // top of the main layout via PositionType::Absolute.
        Binding::new(cx, AppData::is_plugin_picker_open, |cx, open| {
            if open.get(cx) {
                PluginPickerView::new(cx);
            }
        });
    })
    .title("daw_01")
    .inner_size((1280, 800))
    .run()
    .map_err(|e| anyhow::anyhow!("Vizia application error: {e:?}"))
}

/// Overrides the default `list list-item { height: 30px }` so tracker rows are
/// flush. We keep a fixed pixel height rather than `auto` because Vizia's
/// draw path builds a transform per entity and panics on a zero-sized rect
/// (`matrix.invert().unwrap()`), which `auto` can produce for empty
/// ListItems.
const TRACKER_CSS: &str = r#"
list.tracker-list list-item {
    height: 17px;
}

/* Plugin-picker list: dark rows that match the overlay theme. Without this,
   Vizia's default list-item height and colour bleed through and make the
   labels unreadable on a near-white default background. */
list.plugin-picker-list list-item {
    height: 30px;
    background-color: rgb(30, 30, 34);
}
list.plugin-picker-list list-item:hover {
    background-color: rgb(70, 70, 78);
}
"#;

/// Bridges the tokio `ChildToMain` receiver to Vizia AppEvents. Runs on a
/// Vizia-spawned background thread (`cx.spawn`), consuming one message at a
/// time via `blocking_recv` and posting the matching `AppEvent`.
///
/// Exits when the receiver channel closes (plugin_host pipe died) or when
/// `proxy.emit` fails (UI dropped).
fn spawn_incoming_bridge(cx: &mut Context, mut rx: UnboundedReceiver<ChildToMain>) {
    cx.spawn(move |proxy| {
        while let Some(msg) = rx.blocking_recv() {
            let event = match msg {
                ChildToMain::SlotGuiOpened { track, slot, width, height } => {
                    Some(AppEvent::GuiOpenedFromChild { track, slot, width, height })
                }
                ChildToMain::SlotGuiRequestResize { track, slot, width, height } => {
                    Some(AppEvent::GuiRequestResizeFromChild { track, slot, width, height })
                }
                ChildToMain::SlotGuiClosed { track, slot } => {
                    Some(AppEvent::GuiClosedFromChild { track, slot })
                }
                ChildToMain::SlotPluginLoaded { track, slot, id, name } => {
                    Some(AppEvent::SlotPluginLoadedFromChild { track, slot, id, name })
                }
                // Single-slot state replies are unused by the current save
                // flow (the picker / GUI never request a single state).
                // Drop them rather than fabricating a synthetic event.
                ChildToMain::SlotPluginState { .. } => None,
                ChildToMain::AllPluginStates { entries } => {
                    Some(AppEvent::AllStatesReceived(entries))
                }
                ChildToMain::Hello { .. } => None,
            };
            if let Some(event) = event
                && proxy.emit(event).is_err()
            {
                break;
            }
        }
        tracing::info!("incoming bridge exited");
    });
}

/// Load the cached plugin database from disk, or run a fresh scan and
/// cache the result. Errors on either path are logged — we fall back to an
/// empty `Option::None` so the UI still boots.
fn load_or_build_plugin_db() -> Option<Arc<common::plugin_db::PluginDatabase>> {
    use common::plugin_db::{default_cache_path, scan_system, PluginDatabase};
    if let Some(cache) = default_cache_path() {
        match PluginDatabase::load_from_file(&cache) {
            Ok(Some(db)) => {
                tracing::info!(
                    count = db.entries.len(),
                    path = %cache.display(),
                    "loaded cached plugin database"
                );
                return Some(Arc::new(db));
            }
            Ok(None) => {
                tracing::info!(path = %cache.display(), "no cache, scanning…");
            }
            Err(e) => {
                tracing::warn!(error = ?e, "failed to load cache, scanning");
            }
        }
        match scan_system() {
            Ok(db) => {
                if let Err(e) = db.save_to_file(&cache) {
                    tracing::warn!(error = ?e, "failed to write plugin cache");
                } else {
                    tracing::info!(
                        count = db.entries.len(),
                        path = %cache.display(),
                        "wrote plugin database cache"
                    );
                }
                return Some(Arc::new(db));
            }
            Err(e) => {
                tracing::error!(error = ?e, "plugin scan failed");
            }
        }
    }
    None
}

/// Polls the shared-memory playhead and L/R peaks at ~30 Hz and dispatches
/// `AppEvent::Tick` so `AppData::on_tick` can update the playhead-row
/// highlight and the master meter. The worker exits when the UI is closed
/// and `proxy.emit` returns an error.
fn spawn_playhead_poller(cx: &mut Context, bridge: Arc<AudioBridgeHandle>) {
    cx.spawn(move |proxy| {
        loop {
            std::thread::sleep(Duration::from_millis(33));
            let samples = bridge.playhead_samples();
            let (peak_l, peak_r) = bridge.peaks();
            if proxy
                .emit(AppEvent::Tick(samples, peak_l.to_bits(), peak_r.to_bits()))
                .is_err()
            {
                break;
            }
        }
    });
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
        (
            KeyChord::new(Modifiers::empty(), Code::Space),
            KeymapEntry::new(AppEvent::PlayToggle, |cx| cx.emit(AppEvent::PlayToggle)),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::KeyP),
            KeymapEntry::new(AppEvent::ToggleLoop, |cx| cx.emit(AppEvent::ToggleLoop)),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::KeyN),
            KeymapEntry::new(AppEvent::NoteOff, |cx| cx.emit(AppEvent::NoteOff)),
        ),
        (
            KeyChord::new(Modifiers::empty(), Code::Delete),
            KeymapEntry::new(AppEvent::NoteClear, |cx| cx.emit(AppEvent::NoteClear)),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyJ),
            KeymapEntry::new(AppEvent::TransposeSemi(-1), |cx| {
                cx.emit(AppEvent::TransposeSemi(-1))
            }),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyK),
            KeymapEntry::new(AppEvent::TransposeSemi(1), |cx| {
                cx.emit(AppEvent::TransposeSemi(1))
            }),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyH),
            KeymapEntry::new(AppEvent::TransposeOctave(-1), |cx| {
                cx.emit(AppEvent::TransposeOctave(-1))
            }),
        ),
        (
            KeyChord::new(Modifiers::CTRL, Code::KeyL),
            KeymapEntry::new(AppEvent::TransposeOctave(1), |cx| {
                cx.emit(AppEvent::TransposeOctave(1))
            }),
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
        move |cx| cx.emit(event.clone()),
        move |cx| {
            HStack::new(cx, |cx| {
                Label::new(cx, label);
                Label::new(cx, shortcut).class("shortcut");
            })
            .gap(Stretch(1.0))
        },
    );
}
