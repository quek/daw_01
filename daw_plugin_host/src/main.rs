mod clap_host;
mod plugin;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use common::audio_bridge::{AudioBridgeHandle, CHANNELS};
use common::model::{NoteEvent, Song};
use common::protocol::{AudioSession, ChildKind, ChildToMain, MainToChild};
use common::timing::{clip_bounds_samples, song_ended};
use common::win_sem::Semaphore;
use common::wire::{read_msg, write_msg};
use tokio::net::windows::named_pipe::NamedPipeClient;
use tokio::sync::mpsc as tmpsc;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_REMOVE, PeekMessageW, PostThreadMessageW,
    TranslateMessage, WM_APP,
};

use crate::clap_host::HostCallbacks;
use crate::plugin::{NoteTransition, Plugin, TimedNoteEvent};

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum PlaybackCommand {
    Stop = 0,
    Play = 1,
}

impl PlaybackCommand {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Play,
            _ => Self::Stop,
        }
    }
}

/// Custom Win32 message id used to wake the plugin-main thread's `GetMessage`
/// loop after a command has been pushed into the mpsc queue.
const WM_COMMAND_WAKE: u32 = WM_APP + 1;

/// Slot-addressed events pushed from the plugin-main thread (or its CLAP
/// callbacks) to the IPC sender. MVP: track is always 0 until the GUI adds
/// track-addressing; protocol is already shaped for it.
#[derive(Debug, Clone)]
pub enum PluginEvent {
    SlotGuiOpened {
        slot: common::protocol::PluginSlot,
        width: u32,
        height: u32,
    },
    SlotGuiRequestResize {
        slot: common::protocol::PluginSlot,
        width: u32,
        height: u32,
    },
    SlotGuiClosed {
        slot: common::protocol::PluginSlot,
    },
    SlotPluginLoaded {
        slot: common::protocol::PluginSlot,
        id: String,
        name: String,
    },
    SlotPluginState {
        slot: common::protocol::PluginSlot,
        data: Option<Vec<u8>>,
    },
    AllPluginStates {
        entries: Vec<common::protocol::SlotState>,
    },
}

impl From<PluginEvent> for ChildToMain {
    fn from(e: PluginEvent) -> Self {
        let track = 0;
        match e {
            PluginEvent::SlotGuiOpened { slot, width, height } => {
                ChildToMain::SlotGuiOpened { track, slot, width, height }
            }
            PluginEvent::SlotGuiRequestResize { slot, width, height } => {
                ChildToMain::SlotGuiRequestResize { track, slot, width, height }
            }
            PluginEvent::SlotGuiClosed { slot } => ChildToMain::SlotGuiClosed { track, slot },
            PluginEvent::SlotPluginLoaded { slot, id, name } => {
                ChildToMain::SlotPluginLoaded { track, slot, id, name }
            }
            PluginEvent::SlotPluginState { slot, data } => {
                ChildToMain::SlotPluginState { track, slot, data }
            }
            PluginEvent::AllPluginStates { entries } => ChildToMain::AllPluginStates { entries },
        }
    }
}

/// Commands processed serially on the plugin-main thread.
enum PluginCommand {
    SetSlotPlugin {
        slot: common::protocol::PluginSlot,
        path: PathBuf,
        plugin_id: String,
        initial_state: Option<Vec<u8>>,
    },
    RemoveSlotPlugin {
        slot: common::protocol::PluginSlot,
    },
    MoveSlot {
        from: common::protocol::PluginSlot,
        to: common::protocol::PluginSlot,
    },
    LoadSong(Song),
    Play,
    Stop,
    SetLoop(bool),
    RequestSlotState {
        slot: common::protocol::PluginSlot,
    },
    RequestAllStates,
    OpenSlotGui {
        slot: common::protocol::PluginSlot,
        host_hwnd: u64,
    },
    CloseSlotGui {
        slot: common::protocol::PluginSlot,
    },
    ResizeSlotGui {
        slot: common::protocol::PluginSlot,
        width: u32,
        height: u32,
    },
    Shutdown,
}

#[tokio::main]
async fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_plugin_host started");

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    let mut pipe = common::client::perform_handshake(&pipe_name, ChildKind::PluginHost).await?;
    tracing::info!("daw_plugin_host handshake complete");

    let session = common::client::read_session(&mut pipe).await?;
    tracing::info!(?session, "audio session received");

    let (evt_tx, evt_rx) = tmpsc::unbounded_channel::<PluginEvent>();
    let plugin_thread = PluginThread::spawn(session, evt_tx)?;

    // Multiplex pipe I/O: read commands in, write events out, on the same
    // socket (no cloning needed).
    pipe_loop(pipe, plugin_thread.sender(), evt_rx).await;

    tracing::info!("daw_plugin_host shutting down");
    plugin_thread.shutdown();
    tracing::info!("daw_plugin_host exiting");
    Ok(())
}

// --- PluginThread wrapper --------------------------------------------------

struct PluginThread {
    join: Option<JoinHandle<()>>,
    cmd_tx: mpsc::Sender<PluginCommand>,
    thread_id: u32,
}

impl PluginThread {
    fn spawn(session: AudioSession, evt_tx: tmpsc::UnboundedSender<PluginEvent>) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PluginCommand>();
        let (tid_tx, tid_rx) = mpsc::channel::<u32>();

        let join = std::thread::Builder::new()
            .name("plugin-main".into())
            .spawn(move || {
                let tid = unsafe { GetCurrentThreadId() };
                let _ = tid_tx.send(tid);
                plugin_main_loop(session, cmd_rx, evt_tx);
            })
            .context("failed to spawn plugin-main thread")?;

        let thread_id = tid_rx
            .recv()
            .context("plugin-main thread failed to report its id")?;

        Ok(Self {
            join: Some(join),
            cmd_tx,
            thread_id,
        })
    }

    fn sender(&self) -> PluginThreadSender {
        PluginThreadSender {
            cmd_tx: self.cmd_tx.clone(),
            thread_id: self.thread_id,
        }
    }

    fn shutdown(mut self) {
        let _ = self.cmd_tx.send(PluginCommand::Shutdown);
        wake_thread(self.thread_id);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
struct PluginThreadSender {
    cmd_tx: mpsc::Sender<PluginCommand>,
    thread_id: u32,
}

impl PluginThreadSender {
    fn send(&self, cmd: PluginCommand) {
        if self.cmd_tx.send(cmd).is_err() {
            tracing::warn!("plugin-main thread channel closed; command dropped");
            return;
        }
        wake_thread(self.thread_id);
    }
}

fn wake_thread(thread_id: u32) {
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_COMMAND_WAKE, WPARAM(0), LPARAM(0));
    }
}

// --- Plugin-main thread loop ----------------------------------------------

fn plugin_main_loop(
    session: AudioSession,
    cmd_rx: mpsc::Receiver<PluginCommand>,
    evt_tx: tmpsc::UnboundedSender<PluginEvent>,
) {
    let playback_state = Arc::new(AtomicU8::new(PlaybackCommand::Stop as u8));
    let song_store: Arc<ArcSwapOption<Song>> = Arc::new(ArcSwapOption::from(None));
    let loop_state = Arc::new(AtomicBool::new(false));
    let mut chain = ChainHandle::new();

    // Per-slot host callbacks: each loaded plugin captures its slot so the
    // async CLAP callback (request_resize / closed) can stamp the event
    // with the correct slot before reaching daw_gui.
    let make_callbacks = |slot: common::protocol::PluginSlot| HostCallbacks {
        on_request_resize: {
            let tx = evt_tx.clone();
            Arc::new(move |w, h| {
                let _ = tx.send(PluginEvent::SlotGuiRequestResize {
                    slot,
                    width: w,
                    height: h,
                });
            })
        },
        on_closed: {
            let tx = evt_tx.clone();
            Arc::new(move || {
                let _ = tx.send(PluginEvent::SlotGuiClosed { slot });
            })
        },
    };

    tracing::info!("plugin-main thread running");

    loop {
        loop {
            let cmd = match cmd_rx.try_recv() {
                Ok(c) => c,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    chain.shutdown();
                    return;
                }
            };
            match cmd {
                PluginCommand::Shutdown => {
                    chain.shutdown();
                    tracing::info!("plugin-main thread exiting");
                    return;
                }
                PluginCommand::SetSlotPlugin {
                    slot,
                    path,
                    plugin_id,
                    initial_state,
                } => {
                    playback_state.store(PlaybackCommand::Stop as u8, Ordering::Release);
                    let callbacks = make_callbacks(slot);
                    match Plugin::load(&path, &plugin_id, callbacks) {
                        Ok(plugin) => {
                            if let Some(bytes) = initial_state
                                && let Err(e) = plugin.state_load(&bytes)
                            {
                                tracing::error!(error = ?e, "state_load failed");
                            }
                            let loaded_id = plugin.id().to_string();
                            let loaded_name = plugin.name().to_string();
                            let result = chain.mutate_chain(
                                &session,
                                &playback_state,
                                &song_store,
                                &loop_state,
                                |c| install_plugin(c, slot, plugin),
                            );
                            if let Err(e) = result {
                                tracing::error!(error = ?e, ?slot, "failed to install plugin");
                            } else {
                                let _ = evt_tx.send(PluginEvent::SlotPluginLoaded {
                                    slot,
                                    id: loaded_id,
                                    name: loaded_name,
                                });
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, path = %path.display(), "load failed");
                        }
                    }
                }
                PluginCommand::RemoveSlotPlugin { slot } => {
                    let _ = chain.mutate_chain(
                        &session,
                        &playback_state,
                        &song_store,
                        &loop_state,
                        |c| {
                            remove_plugin(c, slot);
                        },
                    );
                }
                PluginCommand::MoveSlot { from, to } => {
                    let _ = chain.mutate_chain(
                        &session,
                        &playback_state,
                        &song_store,
                        &loop_state,
                        |c| {
                            move_plugin(c, from, to);
                        },
                    );
                }
                PluginCommand::LoadSong(song) => {
                    tracing::info!(bpm = song.bpm, tracks = song.tracks.len(), "LoadSong");
                    song_store.store(Some(Arc::new(song)));
                }
                PluginCommand::Play => {
                    playback_state.store(PlaybackCommand::Play as u8, Ordering::Release);
                }
                PluginCommand::Stop => {
                    playback_state.store(PlaybackCommand::Stop as u8, Ordering::Release);
                }
                PluginCommand::SetLoop(on) => {
                    loop_state.store(on, Ordering::Release);
                }
                PluginCommand::RequestSlotState { slot } => {
                    let data = match chain.plugin_at_mut(slot) {
                        Some(plugin) => match plugin.state_save() {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!(error = ?e, "state_save failed");
                                None
                            }
                        },
                        None => None,
                    };
                    let _ = evt_tx.send(PluginEvent::SlotPluginState { slot, data });
                }
                PluginCommand::RequestAllStates => {
                    let entries = collect_all_states(&mut chain);
                    let _ = evt_tx.send(PluginEvent::AllPluginStates { entries });
                }
                PluginCommand::OpenSlotGui { slot, host_hwnd } => {
                    match open_gui(&mut chain, slot, host_hwnd) {
                        Ok(Some((w, h))) => {
                            let _ = evt_tx.send(PluginEvent::SlotGuiOpened {
                                slot,
                                width: w,
                                height: h,
                            });
                        }
                        Ok(None) => {
                            let _ = evt_tx.send(PluginEvent::SlotGuiClosed { slot });
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, ?slot, "failed to open GUI");
                            close_gui(&mut chain, slot);
                            let _ = evt_tx.send(PluginEvent::SlotGuiClosed { slot });
                        }
                    }
                }
                PluginCommand::CloseSlotGui { slot } => {
                    close_gui(&mut chain, slot);
                    let _ = evt_tx.send(PluginEvent::SlotGuiClosed { slot });
                }
                PluginCommand::ResizeSlotGui {
                    slot,
                    width,
                    height,
                } => {
                    resize_gui(&mut chain, slot, width, height);
                }
            }
        }

        unsafe {
            let mut msg = MSG::default();
            let ret = GetMessageW(&mut msg, Some(HWND(std::ptr::null_mut())), 0, 0);
            if ret.0 <= 0 {
                break;
            }
            if msg.message != WM_COMMAND_WAKE {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    chain.shutdown();
    tracing::info!("plugin-main thread exiting (WM_QUIT)");
}

/// Place `plugin` into the slot, replacing any previous occupant. Audio
/// thread is stopped by the caller (via `mutate_chain`).
fn install_plugin(chain: &mut Chain, slot: common::protocol::PluginSlot, plugin: Plugin) {
    use common::protocol::PluginSlot;
    match slot {
        PluginSlot::Instrument => {
            if let Some(mut old) = chain.instrument.replace(plugin) {
                old.gui_destroy();
            }
        }
        PluginSlot::Fx(i) => {
            let i = i as usize;
            if i < chain.fx_chain.len() {
                if let Some(old) = chain.fx_chain.get_mut(i) {
                    old.gui_destroy();
                }
                chain.fx_chain[i] = plugin;
            } else {
                // Append — users add to the tail.
                chain.fx_chain.push(plugin);
            }
        }
        PluginSlot::MidiFx(_) => {
            tracing::warn!("MIDI FX chain not wired in audio thread yet; ignored");
        }
    }
}

fn remove_plugin(chain: &mut Chain, slot: common::protocol::PluginSlot) {
    use common::protocol::PluginSlot;
    match slot {
        PluginSlot::Instrument => {
            if let Some(mut old) = chain.instrument.take() {
                old.gui_destroy();
            }
        }
        PluginSlot::Fx(i) => {
            let i = i as usize;
            if i < chain.fx_chain.len() {
                let mut old = chain.fx_chain.remove(i);
                old.gui_destroy();
            }
        }
        PluginSlot::MidiFx(_) => {}
    }
}

fn move_plugin(
    chain: &mut Chain,
    from: common::protocol::PluginSlot,
    to: common::protocol::PluginSlot,
) {
    use common::protocol::PluginSlot;
    // Only Fx↔Fx reorder is supported for MVP.
    if let (PluginSlot::Fx(a), PluginSlot::Fx(b)) = (from, to) {
        let a = a as usize;
        let b = b as usize;
        if a < chain.fx_chain.len() && b < chain.fx_chain.len() && a != b {
            let plugin = chain.fx_chain.remove(a);
            chain.fx_chain.insert(b, plugin);
        }
    }
}

fn collect_all_states(chain: &mut ChainHandle) -> Vec<common::protocol::SlotState> {
    use common::protocol::{PluginSlot, SlotState};
    let mut out = Vec::new();
    if let Some(plugin) = chain.plugin_at_mut(PluginSlot::Instrument) {
        let data = plugin.state_save().ok().flatten();
        out.push(SlotState {
            track: 0,
            slot: PluginSlot::Instrument,
            data,
        });
    }
    let fx_count = chain.chain.fx_chain.len();
    for i in 0..fx_count {
        let slot = PluginSlot::Fx(i as u32);
        if let Some(plugin) = chain.plugin_at_mut(slot) {
            let data = plugin.state_save().ok().flatten();
            out.push(SlotState {
                track: 0,
                slot,
                data,
            });
        }
    }
    out
}

fn open_gui(
    chain: &mut ChainHandle,
    slot: common::protocol::PluginSlot,
    host_hwnd: u64,
) -> Result<Option<(u32, u32)>> {
    let Some(plugin) = chain.plugin_at_mut(slot) else {
        return Ok(None);
    };
    if !plugin.gui_is_embed_supported() {
        tracing::warn!(plugin = %plugin.name(), "plugin does not support embedded win32 gui");
        return Ok(None);
    }
    // CLAP embedded GUI sequence per gui.h:
    //   create → set_scale → (can_resize info only) → get_size → set_parent → show
    //
    // We do NOT call set_size here: per spec that's reserved for restoring a
    // persisted size from a previous session. Calling it on first open
    // breaks plugins like VCV Rack that treat it as an invalid operation
    // before show.
    plugin.gui_create_embedded()?;

    // MVP: hardcode scale = 1.0. A DPI-aware version would query
    // `GetDpiForWindow` on the host HWND.
    if let Err(e) = plugin.gui_set_scale(1.0) {
        tracing::warn!(error = ?e, "gui.set_scale failed (ignored)");
    }

    let resizable = plugin.gui_can_resize();
    let size = plugin.gui_get_size().unwrap_or((800, 600));
    tracing::info!(
        plugin = %plugin.name(),
        resizable,
        width = size.0,
        height = size.1,
        "plugin gui initial size"
    );

    plugin.gui_set_parent_hwnd(host_hwnd)?;

    // Some plugins post themselves an internal "finish init" message from
    // inside set_parent. Drain whatever the plugin queued before calling
    // show so it can complete initialization on the current thread.
    pump_pending_messages();

    let shown = plugin.gui_show()?;
    if !shown {
        // VCV Rack 2 returns false here even though its GUI is actually
        // visible in our container. Since create + set_parent succeeded,
        // keep the GUI alive and just log — tearing down on a false return
        // from `show` destroys a working editor for these plugins.
        tracing::warn!(
            plugin = %plugin.name(),
            "gui.show returned false; keeping GUI alive (plugin may have already shown itself)"
        );
    }
    tracing::info!(plugin = %plugin.name(), width = size.0, height = size.1, "plugin gui opened");
    Ok(Some(size))
}

/// Non-blocking drain of pending Win32 messages on the current thread. Used
/// between CLAP GUI calls that rely on a host message pump being present
/// (plugins that use `PostMessage` internally during initialization).
fn pump_pending_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_COMMAND_WAKE {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn close_gui(chain: &mut ChainHandle, slot: common::protocol::PluginSlot) {
    let Some(plugin) = chain.plugin_at_mut(slot) else { return };
    let _ = plugin.gui_hide();
    plugin.gui_destroy();
}

fn resize_gui(
    chain: &mut ChainHandle,
    slot: common::protocol::PluginSlot,
    width: u32,
    height: u32,
) {
    let Some(plugin) = chain.plugin_at_mut(slot) else { return };
    if let Err(e) = plugin.gui_set_size(width, height) {
        tracing::warn!(error = ?e, width, height, ?slot, "gui.set_size failed");
    }
}

// --- pipe_loop: multiplex read (commands) + write (events) ---------------

async fn pipe_loop(
    mut pipe: NamedPipeClient,
    plugin: PluginThreadSender,
    mut evt_rx: tmpsc::UnboundedReceiver<PluginEvent>,
) {
    loop {
        tokio::select! {
            msg = read_msg::<_, MainToChild>(&mut pipe) => {
                match msg {
                    Ok(m) => handle_main_to_child(m, &plugin),
                    Err(e) => {
                        tracing::info!(error = ?e, "pipe ended");
                        return;
                    }
                }
            }
            evt = evt_rx.recv() => {
                let Some(evt) = evt else { return };
                let child_msg = ChildToMain::from(evt);
                if let Err(e) = write_msg(&mut pipe, &child_msg).await {
                    tracing::error!(error = ?e, ?child_msg, "failed to forward plugin event");
                    return;
                }
            }
        }
    }
}

fn handle_main_to_child(msg: MainToChild, plugin: &PluginThreadSender) {
    match msg {
        MainToChild::Play => {
            tracing::info!("received Play");
            plugin.send(PluginCommand::Play);
        }
        MainToChild::Stop => {
            tracing::info!("received Stop");
            plugin.send(PluginCommand::Stop);
        }
        MainToChild::LoadSong(song) => {
            tracing::info!(
                bpm = song.bpm,
                tracks = song.tracks.len(),
                "received LoadSong"
            );
            plugin.send(PluginCommand::LoadSong(song));
        }
        MainToChild::SetSlotPlugin {
            track,
            slot,
            path,
            plugin_id,
            initial_state,
        } => {
            tracing::info!(
                track,
                ?slot,
                path = %path.display(),
                id = %plugin_id,
                has_state = initial_state.is_some(),
                "received SetSlotPlugin"
            );
            plugin.send(PluginCommand::SetSlotPlugin {
                slot,
                path,
                plugin_id,
                initial_state,
            });
        }
        MainToChild::RemoveSlotPlugin { track, slot } => {
            tracing::info!(track, ?slot, "received RemoveSlotPlugin");
            plugin.send(PluginCommand::RemoveSlotPlugin { slot });
        }
        MainToChild::MoveSlot { track, from, to } => {
            tracing::info!(track, ?from, ?to, "received MoveSlot");
            plugin.send(PluginCommand::MoveSlot { from, to });
        }
        MainToChild::RequestSlotState { track, slot } => {
            tracing::info!(track, ?slot, "received RequestSlotState");
            plugin.send(PluginCommand::RequestSlotState { slot });
        }
        MainToChild::RequestAllStates => {
            tracing::info!("received RequestAllStates");
            plugin.send(PluginCommand::RequestAllStates);
        }
        MainToChild::SetLoop(on) => {
            tracing::info!(on, "received SetLoop");
            plugin.send(PluginCommand::SetLoop(on));
        }
        MainToChild::OpenSlotGuiEmbedded {
            track,
            slot,
            host_hwnd,
        } => {
            tracing::info!(track, ?slot, host_hwnd, "received OpenSlotGuiEmbedded");
            plugin.send(PluginCommand::OpenSlotGui { slot, host_hwnd });
        }
        MainToChild::CloseSlotGui { track, slot } => {
            tracing::info!(track, ?slot, "received CloseSlotGui");
            plugin.send(PluginCommand::CloseSlotGui { slot });
        }
        MainToChild::ResizeSlotGui {
            track,
            slot,
            width,
            height,
        } => {
            tracing::info!(track, ?slot, width, height, "received ResizeSlotGui");
            plugin.send(PluginCommand::ResizeSlotGui {
                slot,
                width,
                height,
            });
        }
        other => {
            tracing::info!(?other, "received (no handler)");
        }
    }
}

// --- Chain + audio thread ------------------------------------------------

/// Wraps a raw pointer so it can be moved into the audio thread closure.
/// The CLAP spec partitions plugin state between main-thread and audio-thread,
/// so simultaneous main-thread GUI calls and audio-thread `process()` calls
/// touch disjoint fields (this assumes plugins conform to the spec).
struct PluginPtr(*mut Plugin);
unsafe impl Send for PluginPtr {}

/// Per-track signal chain owned on the plugin-main thread. The audio thread
/// receives raw pointer snapshots at spawn time (see [`AudioRouting`]).
///
/// `Plugin` is stored by value; self-referential pointers inside it (the
/// host's `clap_host` address passed to `create_plugin`) live in the `Box<Host>`
/// it owns, so moving the `Plugin` between `Vec` reallocations is safe. The
/// audio thread's raw pointers are always refreshed during `start_audio`.
#[derive(Default)]
struct Chain {
    /// Instrument slot (note→audio). `None` = no instrument loaded on the
    /// track; audio thread produces silence at the instrument stage.
    instrument: Option<Plugin>,
    /// Audio effects applied in order after the instrument.
    fx_chain: Vec<Plugin>,
}

impl Chain {
    fn plugin_at_mut(&mut self, slot: common::protocol::PluginSlot) -> Option<&mut Plugin> {
        use common::protocol::PluginSlot;
        match slot {
            PluginSlot::Instrument => self.instrument.as_mut(),
            PluginSlot::Fx(i) => self.fx_chain.get_mut(i as usize),
            PluginSlot::MidiFx(_) => None, // Phase B: audio routing only covers FX
        }
    }
}

/// Plain-pointer snapshot of the chain handed to the audio thread. Valid as
/// long as the chain is not mutated (which first triggers an audio-thread
/// stop + restart on the main side).
struct AudioRouting {
    instrument: Option<PluginPtr>,
    fx_chain: Vec<PluginPtr>,
}

/// RAII owner for a track's signal chain plus its audio thread. Any chain
/// mutation (set/remove/move a slot) goes through the helper methods which
/// stop + restart the audio thread so the raw pointers stay valid.
struct ChainHandle {
    chain: Chain,
    audio: Option<AudioThread>,
}

struct AudioThread {
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    request_sem: Arc<Semaphore>,
}

impl ChainHandle {
    fn new() -> Self {
        Self {
            chain: Chain::default(),
            audio: None,
        }
    }

    fn plugin_at_mut(&mut self, slot: common::protocol::PluginSlot) -> Option<&mut Plugin> {
        self.chain.plugin_at_mut(slot)
    }

    /// Stop the audio thread, run `f` to mutate the chain on the main
    /// thread, and restart the audio thread with the new routing.
    fn mutate_chain<F>(
        &mut self,
        session: &AudioSession,
        playback_state: &Arc<AtomicU8>,
        song_store: &Arc<ArcSwapOption<Song>>,
        loop_state: &Arc<AtomicBool>,
        f: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut Chain),
    {
        self.stop_audio();
        f(&mut self.chain);
        self.start_audio(session, playback_state, song_store, loop_state)
    }

    fn stop_audio(&mut self) {
        let Some(mut at) = self.audio.take() else {
            return;
        };
        at.shutdown.store(true, Ordering::Release);
        let _ = at.request_sem.release();
        if let Some(handle) = at.handle.take()
            && handle.join().is_err()
        {
            tracing::error!("audio thread panicked");
        }
        // Safe to touch plugins again now that the audio thread has exited.
        if let Some(inst) = self.chain.instrument.as_mut() {
            inst.deactivate();
        }
        for fx in &mut self.chain.fx_chain {
            fx.deactivate();
        }
    }

    fn start_audio(
        &mut self,
        session: &AudioSession,
        playback_state: &Arc<AtomicU8>,
        song_store: &Arc<ArcSwapOption<Song>>,
        loop_state: &Arc<AtomicBool>,
    ) -> Result<()> {
        // (Re-)activate every plugin — the previous stop_audio deactivated
        // them to keep the main-thread invariant clean.
        for plugin in self
            .chain
            .instrument
            .iter_mut()
            .chain(self.chain.fx_chain.iter_mut())
        {
            plugin
                .activate(f64::from(session.sample_rate), 64, session.max_frames)
                .context("plugin.activate failed")?;
        }

        let routing = AudioRouting {
            instrument: self
                .chain
                .instrument
                .as_mut()
                .map(|p| PluginPtr(&raw mut *p)),
            fx_chain: self
                .chain
                .fx_chain
                .iter_mut()
                .map(|p| PluginPtr(&raw mut *p))
                .collect(),
        };

        let bridge = Arc::new(
            AudioBridgeHandle::open(&session.shmem_id)
                .context("failed to open audio shmem")?,
        );
        let request_sem = Arc::new(
            Semaphore::open(&session.request_sem_id)
                .context("failed to open request semaphore")?,
        );
        let ready_sem = Arc::new(
            Semaphore::open(&session.ready_sem_id)
                .context("failed to open ready semaphore")?,
        );
        let shutdown = Arc::new(AtomicBool::new(false));

        let th_bridge = Arc::clone(&bridge);
        let th_req = Arc::clone(&request_sem);
        let th_ready = Arc::clone(&ready_sem);
        let th_shutdown = Arc::clone(&shutdown);
        let th_playback = Arc::clone(playback_state);
        let th_song = Arc::clone(song_store);
        let th_loop = Arc::clone(loop_state);
        let th_sample_rate = session.sample_rate;

        let handle = std::thread::Builder::new()
            .name("clap-audio".into())
            .spawn(move || {
                run_audio(
                    routing,
                    th_bridge,
                    th_req,
                    th_ready,
                    th_shutdown,
                    th_playback,
                    th_song,
                    th_loop,
                    th_sample_rate,
                );
            })
            .context("failed to spawn audio thread")?;

        self.audio = Some(AudioThread {
            handle: Some(handle),
            shutdown,
            request_sem,
        });
        Ok(())
    }

    fn shutdown(mut self) {
        self.stop_audio();
        // Drop plugins on the main thread after stopping audio.
        if let Some(mut inst) = self.chain.instrument.take() {
            inst.gui_destroy();
        }
        for fx in &mut self.chain.fx_chain {
            fx.gui_destroy();
        }
        // Box<Plugin>s drop here, running `Plugin::drop` on the main thread.
    }
}

#[allow(clippy::too_many_arguments)]
fn run_audio(
    routing: AudioRouting,
    bridge: Arc<AudioBridgeHandle>,
    request_sem: Arc<Semaphore>,
    ready_sem: Arc<Semaphore>,
    shutdown: Arc<AtomicBool>,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
    loop_state: Arc<AtomicBool>,
    sample_rate: u32,
) {
    // SAFETY: the main thread guarantees every Plugin Box outlives this
    // thread via `ChainHandle::stop_audio()`.
    let fx_ptrs: Vec<*mut Plugin> = routing.fx_chain.iter().map(|p| p.0).collect();

    if let Some(inst_ptr) = routing.instrument.as_ref() {
        let inst = unsafe { &mut *inst_ptr.0 };
        if let Err(e) = inst.start_processing() {
            tracing::error!(error = ?e, "instrument.start_processing failed");
            return;
        }
    }
    for fx_ptr in &fx_ptrs {
        let fx = unsafe { &mut **fx_ptr };
        if let Err(e) = fx.start_processing() {
            tracing::error!(error = ?e, "fx.start_processing failed");
        }
    }

    // Pre-allocated ping-pong buffers for the FX chain. Each chain step
    // writes into the next plugin's input; we avoid borrow-checker fights
    // between successive plugins by first copying the previous output into
    // `bus_*`, then passing `&bus_*` as the FX's input audio.
    let max_frames = common::audio_bridge::MAX_FRAMES as usize;
    let mut bus_l: Vec<f32> = vec![0.0; max_frames];
    let mut bus_r: Vec<f32> = vec![0.0; max_frames];

    let out_channels = CHANNELS as usize;
    let mut playing = false;
    let mut playhead: u64 = 0;
    let mut active_notes: Vec<u8> = Vec::with_capacity(16);
    let mut scheduled: Vec<TimedNoteEvent> = Vec::with_capacity(64);
    tracing::info!("audio thread running");
    loop {
        match request_sem.wait_timeout_ms(100) {
            Ok(true) => {}
            Ok(false) => {
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            Err(e) => {
                tracing::error!(error = ?e, "request semaphore wait failed");
                break;
            }
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        let desired = PlaybackCommand::from_u8(playback_state.load(Ordering::Acquire));
        match (playing, desired) {
            (false, PlaybackCommand::Play) => {
                playing = true;
                playhead = 0;
                active_notes.clear();
            }
            (true, PlaybackCommand::Stop) => {
                playing = false;
                scheduled.clear();
                for &key in &active_notes {
                    scheduled.push(TimedNoteEvent {
                        time: 0,
                        event: NoteTransition::Off { key },
                    });
                }
                active_notes.clear();
            }
            _ => {}
        }

        let frames = bridge.frames_requested();

        if playing {
            scheduled.clear();
            let snapshot = song_store.load();
            let song_ref = snapshot.as_deref();
            collect_events_for_buffer(
                song_ref,
                sample_rate,
                playhead,
                frames,
                &mut scheduled,
                &mut active_notes,
            );
            playhead += frames as u64;
            if song_ended(song_ref, sample_rate, playhead) {
                for &key in &active_notes {
                    scheduled.push(TimedNoteEvent {
                        time: frames.saturating_sub(1),
                        event: NoteTransition::Off { key },
                    });
                }
                active_notes.clear();

                let wrap_to = if loop_state.load(Ordering::Acquire) {
                    clip_bounds_samples(song_ref, sample_rate).map(|(start, _)| start)
                } else {
                    None
                };
                if let Some(start) = wrap_to {
                    playhead = start;
                    tracing::debug!(playhead = start, "looped back to clip start");
                } else {
                    playing = false;
                    playback_state.store(PlaybackCommand::Stop as u8, Ordering::Release);
                    tracing::info!("playback reached end of clip, auto-stopping");
                }
            }
        }

        // --- Chain processing ------------------------------------------
        // 1. Instrument: produces audio from note events; no audio input.
        let n = frames as usize;
        let mut have_audio = false;
        if let Some(inst_ptr) = routing.instrument.as_ref() {
            let inst = unsafe { &mut *inst_ptr.0 };
            if let Err(e) = inst.process(frames, &scheduled, &[]) {
                tracing::error!(error = ?e, "instrument.process failed");
                break;
            }
            if let Some(l) = inst.output_buffer(0) {
                bus_l[..n].copy_from_slice(&l[..n]);
                have_audio = true;
            }
            let right = inst.output_buffer(1).or(inst.output_buffer(0));
            if let Some(r) = right {
                bus_r[..n].copy_from_slice(&r[..n]);
            }
        }
        if !have_audio {
            bus_l[..n].fill(0.0);
            bus_r[..n].fill(0.0);
        }
        scheduled.clear();

        // 2. FX chain: each effect reads bus_* and writes new audio. We
        //    reborrow via raw pointer to sidestep the borrow checker for
        //    the read-then-copy-back pattern.
        for fx_ptr in &fx_ptrs {
            let fx = unsafe { &mut **fx_ptr };
            let input_channels = [&bus_l[..n], &bus_r[..n]];
            if let Err(e) = fx.process(frames, &[], &input_channels) {
                tracing::error!(error = ?e, "fx.process failed");
                break;
            }
            if let Some(l) = fx.output_buffer(0) {
                bus_l[..n].copy_from_slice(&l[..n]);
            }
            if let Some(r) = fx.output_buffer(1).or(fx.output_buffer(0)) {
                bus_r[..n].copy_from_slice(&r[..n]);
            }
        }

        let published = if playing { playhead } else { u64::MAX };
        bridge.set_playhead_samples(published);

        // 3. Copy final bus to shmem (interleaved stereo).
        unsafe {
            let dst = bridge.samples_ptr();
            for i in 0..n {
                *dst.add(i * out_channels) = bus_l[i];
                *dst.add(i * out_channels + 1) = bus_r[i];
            }
        }

        if let Err(e) = ready_sem.release() {
            tracing::error!(error = ?e, "ready semaphore release failed");
            break;
        }
    }

    // Stop every plugin before returning; main thread's deactivate will
    // follow once it joins this thread.
    if let Some(inst_ptr) = routing.instrument.as_ref() {
        unsafe { &mut *inst_ptr.0 }.stop_processing();
    }
    for fx_ptr in &fx_ptrs {
        unsafe { &mut **fx_ptr }.stop_processing();
    }
    tracing::info!("audio thread exiting");
}

fn collect_events_for_buffer(
    song: Option<&Song>,
    sample_rate: u32,
    playhead: u64,
    frames: u32,
    out: &mut Vec<TimedNoteEvent>,
    active_notes: &mut Vec<u8>,
) {
    let Some(song) = song else { return };
    let Some(track) = song.tracks.first() else {
        return;
    };
    let Some(clip) = track.clips.first() else {
        return;
    };
    if clip.rows_per_beat == 0 || song.bpm <= 0.0 {
        return;
    }

    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let samples_per_row = samples_per_beat / f64::from(clip.rows_per_beat);
    let clip_start_samples = (clip.start_beat * samples_per_beat).max(0.0) as u64;

    let buf_end = playhead + u64::from(frames);

    for (i, row) in clip.rows.iter().enumerate() {
        let row_sample = clip_start_samples + (i as f64 * samples_per_row) as u64;
        if row_sample < playhead || row_sample >= buf_end {
            continue;
        }
        let Some(note) = &row.note else { continue };
        let time = (row_sample - playhead) as u32;
        match note {
            NoteEvent::On(n) => {
                for &key in active_notes.iter() {
                    out.push(TimedNoteEvent {
                        time,
                        event: NoteTransition::Off { key },
                    });
                }
                active_notes.clear();
                out.push(TimedNoteEvent {
                    time,
                    event: NoteTransition::On {
                        key: n.key,
                        velocity: f64::from(n.velocity) / 127.0,
                    },
                });
                active_notes.push(n.key);
            }
            NoteEvent::Off => {
                for &key in active_notes.iter() {
                    out.push(TimedNoteEvent {
                        time,
                        event: NoteTransition::Off { key },
                    });
                }
                active_notes.clear();
            }
        }
    }
}
