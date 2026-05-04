mod clap_host;
mod clap_plugin;
mod plugin_instance;
mod process_server;
mod vst3_events;
mod vst3_host;
mod vst3_plugin;
mod vst3_stream;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use common::plugin_format::PluginFormat;
use common::protocol::{AudioSession, ChildKind, ChildToMain, MainToChild, PluginSlot, SlotState};
use common::wire::{read_msg, write_msg};
use tokio::net::windows::named_pipe::NamedPipeClient;
use tokio::sync::mpsc as tmpsc;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_REMOVE, PeekMessageW, PostThreadMessageW,
    TranslateMessage, WM_APP,
};

use crate::plugin_instance::{HostCallbacks, LoadedPlugin, load_plugin};

/// Custom Win32 message id used to wake the plugin-main thread's `GetMessage`
/// loop after a command has been pushed into the mpsc queue.
const WM_COMMAND_WAKE: u32 = WM_APP + 1;

/// Track-and-slot-addressed events pushed from the plugin-main thread (or
/// its CLAP callbacks) to the IPC sender.
#[derive(Debug, Clone)]
pub enum PluginEvent {
    SlotGuiOpened {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    SlotGuiRequestResize {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    SlotGuiClosed {
        track: u32,
        slot: PluginSlot,
    },
    SlotPluginLoaded {
        track: u32,
        slot: PluginSlot,
        id: String,
        name: String,
        plugin_id: u32,
        shmem_id: String,
    },
    SlotPluginState {
        track: u32,
        slot: PluginSlot,
        data: Option<Vec<u8>>,
    },
    AllPluginStates {
        entries: Vec<SlotState>,
    },
}

impl From<PluginEvent> for ChildToMain {
    fn from(e: PluginEvent) -> Self {
        match e {
            PluginEvent::SlotGuiOpened { track, slot, width, height } => {
                ChildToMain::SlotGuiOpened { track, slot, width, height }
            }
            PluginEvent::SlotGuiRequestResize { track, slot, width, height } => {
                ChildToMain::SlotGuiRequestResize { track, slot, width, height }
            }
            PluginEvent::SlotGuiClosed { track, slot } => {
                ChildToMain::SlotGuiClosed { track, slot }
            }
            PluginEvent::SlotPluginLoaded {
                track,
                slot,
                id,
                name,
                plugin_id,
                shmem_id,
            } => ChildToMain::SlotPluginLoaded {
                track,
                slot,
                id,
                name,
                plugin_id,
                shmem_id,
            },
            PluginEvent::SlotPluginState { track, slot, data } => {
                ChildToMain::SlotPluginState { track, slot, data }
            }
            PluginEvent::AllPluginStates { entries } => ChildToMain::AllPluginStates { entries },
        }
    }
}

/// Atomically publish a new entry (or `None` to remove) in the plugin
/// registry. Clones the current `Vec` so old worker snapshots stay
/// valid until they're dropped.
fn publish_plugin_registry(
    registry: &PluginRegistry,
    plugin_id: u32,
    entry: Option<PluginEntry>,
) {
    let current = registry.load();
    let mut next: Vec<Option<PluginEntry>> = (**current)
        .iter()
        .map(|opt| opt.as_ref().map(|e| PluginEntry {
            plugin: PluginPtr(e.plugin.0),
            process_data: e.process_data,
        }))
        .collect();
    let idx = plugin_id as usize;
    if next.len() <= idx {
        next.resize_with(idx + 1, || None);
    }
    next[idx] = entry;
    registry.store(std::sync::Arc::new(next));
}

/// Commands processed serially on the plugin-main thread.
enum PluginCommand {
    SetSlotPlugin {
        track: u32,
        slot: PluginSlot,
        format: PluginFormat,
        path: PathBuf,
        plugin_id: String,
        initial_state: Option<Vec<u8>>,
    },
    RemoveSlotPlugin {
        track: u32,
        slot: PluginSlot,
    },
    MoveSlot {
        track: u32,
        from: PluginSlot,
        to: PluginSlot,
    },
    RemoveTrack {
        track: u32,
    },
    SwapTracks {
        a: u32,
        b: u32,
    },
    ReorderTracks(Vec<u32>),
    RequestSlotState {
        track: u32,
        slot: PluginSlot,
    },
    RequestAllStates,
    OpenSlotGui {
        track: u32,
        slot: PluginSlot,
        host_hwnd: u64,
    },
    CloseSlotGui {
        track: u32,
        slot: PluginSlot,
    },
    ResizeSlotGui {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    /// Stand up the per-buffer plugin process worker pool. Drives
    /// `process_server::WorkerPool::open` on the plugin-main thread so
    /// the audio engine on the daw_audio side can dispatch
    /// `plugin.process()` calls via the worker_wake/done event pairs.
    OpenWorkerPool {
        n_workers: u32,
        worker_bridge_shmem_id: String,
        wake_event_names: Vec<String>,
        done_event_names: Vec<String>,
    },
    /// Tear down the worker pool started by `OpenWorkerPool`.
    CloseWorkerPool,
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
    let mut tracks = TracksHandle::new();
    // A2: plugin-process worker pool paired 1:1 with audio-engine
    // workers. Stored as an Option so OpenWorkerPool can replace any
    // stale pool (e.g. on session restart).
    let mut worker_pool: Option<process_server::WorkerPool> = None;

    // A2: plugin instance registry.
    //   - `next_plugin_id` issues a session-unique id every time a
    //     plugin instance is loaded.
    //   - `plugin_shmems` owns the `ProcessData` shmem created here so
    //     daw_audio can `OpenShared` it via `ChildToMain::SlotPluginLoaded`.
    //   - `plugin_lookup` maps `(track, slot)` to the live plugin id so
    //     RemoveSlotPlugin / RemoveTrack / SwapTracks can clean up.
    //   - `plugin_registry` is the lock-free `plugin_id` → entry table
    //     read by the worker pool during dispatch.
    let plugin_host_pid = std::process::id();
    let mut next_plugin_id: u32 = 1;
    let mut plugin_shmems: HashMap<u32, common::process_data::ProcessDataHandle> = HashMap::new();
    let mut plugin_lookup: HashMap<(u32, PluginSlot), u32> = HashMap::new();
    // Defensive dedup: if the GUI somehow sends `SetSlotPlugin` twice
    // for the same (track, slot, plugin_id) (we've seen the picker
    // double-fire) we skip the second to avoid the workers racing on
    // a destroy → re-install path. Keyed by (track, slot) → loaded
    // plugin's stable id string.
    let mut loaded_id_for_slot: HashMap<(u32, PluginSlot), String> = HashMap::new();
    let plugin_registry: PluginRegistry =
        Arc::new(arc_swap::ArcSwap::from_pointee(Vec::new()));

    // Per-(track, slot) host callbacks: each loaded plugin captures its
    // (track, slot) so the async CLAP callback (request_resize / closed)
    // can stamp the event with the correct address before reaching daw_gui.
    let make_callbacks = |track: u32, slot: PluginSlot| HostCallbacks {
        on_request_resize: {
            let tx = evt_tx.clone();
            Arc::new(move |w, h| {
                let _ = tx.send(PluginEvent::SlotGuiRequestResize {
                    track,
                    slot,
                    width: w,
                    height: h,
                });
            })
        },
        on_closed: {
            let tx = evt_tx.clone();
            Arc::new(move || {
                let _ = tx.send(PluginEvent::SlotGuiClosed { track, slot });
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
                    tracks.shutdown();
                    return;
                }
            };
            match cmd {
                PluginCommand::Shutdown => {
                    if let Some(pool) = worker_pool.take() {
                        pool.shutdown();
                    }
                    tracks.shutdown();
                    tracing::info!("plugin-main thread exiting");
                    return;
                }
                PluginCommand::OpenWorkerPool {
                    n_workers,
                    worker_bridge_shmem_id,
                    wake_event_names,
                    done_event_names,
                } => {
                    if let Some(pool) = worker_pool.take() {
                        pool.shutdown();
                    }
                    match process_server::WorkerPool::open(
                        n_workers,
                        &worker_bridge_shmem_id,
                        &wake_event_names,
                        &done_event_names,
                        Arc::clone(&plugin_registry),
                    ) {
                        Ok(pool) => worker_pool = Some(pool),
                        Err(e) => {
                            tracing::error!(error = ?e, "failed to open plugin worker pool");
                        }
                    }
                }
                PluginCommand::CloseWorkerPool => {
                    if let Some(pool) = worker_pool.take() {
                        pool.shutdown();
                    }
                }
                PluginCommand::SetSlotPlugin {
                    track,
                    slot,
                    format,
                    path,
                    plugin_id,
                    initial_state,
                } => {
                    // Defensive dedup against picker double-fire. Same
                    // plugin id at the same slot ⇒ ignore.
                    if loaded_id_for_slot.get(&(track, slot)) == Some(&plugin_id) {
                        tracing::info!(
                            track,
                            ?slot,
                            id = %plugin_id,
                            "SetSlotPlugin: same plugin already loaded, ignoring duplicate"
                        );
                        continue;
                    }
                    // Note: tracks.mutate stops and restarts the audio
                    // thread to swap in the new plugin. We do NOT touch
                    // playback_state — the user's Play/Stop choice should
                    // survive a chain edit.
                    let callbacks = make_callbacks(track, slot);
                    match load_plugin(format, &path, &plugin_id, callbacks) {
                        Ok(plugin) => {
                            if let Some(bytes) = initial_state
                                && let Err(e) = plugin.state_load(&bytes)
                            {
                                tracing::error!(error = ?e, "state_load failed");
                            }
                            let loaded_id = plugin.id().to_string();
                            let loaded_name = plugin.name().to_string();
                            let sr = session.sample_rate;
                            let mf = session.max_frames;
                            tracks.mutate(|t| {
                                install_plugin(t.ensure_track(track), slot, plugin, sr, mf)
                            });
                            {
                                // A new instance landed; mint its
                                // plugin_id, create its ProcessData
                                // shmem, drop any stale shmem from a
                                // previous plugin in the same slot,
                                // then notify daw_gui (which forwards
                                // the shmem id to daw_audio).
                                if let Some(old_pid) = plugin_lookup.remove(&(track, slot)) {
                                    plugin_shmems.remove(&old_pid);
                                }
                                let new_plugin_id = next_plugin_id;
                                next_plugin_id += 1;
                                let shmem_id = format!(
                                    "daw_01_pd_{plugin_host_pid}_{new_plugin_id}"
                                );
                                match common::process_data::ProcessDataHandle::create(
                                    &shmem_id,
                                ) {
                                    Ok(handle) => {
                                        let pd_ptr = handle.ptr();
                                        plugin_shmems.insert(new_plugin_id, handle);
                                        plugin_lookup.insert((track, slot), new_plugin_id);
                                        loaded_id_for_slot.insert((track, slot), plugin_id.clone());

                                        // Capture the freshly-installed
                                        // plugin pointer in a borrow
                                        // scope that ends before any
                                        // other use of `tracks`.
                                        let plugin_ptr_raw: Option<
                                            *mut (dyn LoadedPlugin + 'static),
                                        > = {
                                            let opt = tracks
                                                .tracks
                                                .plugin_at_mut(track, slot);
                                            opt.map(|p| {
                                                // SAFETY: the Box that
                                                // owns this trait
                                                // object lives in
                                                // `tracks.chains` and is
                                                // only dropped via
                                                // `tracks.mutate`, which
                                                // stops the audio thread
                                                // before tearing down a
                                                // chain. Worker
                                                // dispatches always
                                                // synchronise via the
                                                // event handshake, so
                                                // every read happens
                                                // while the pointer is
                                                // still valid.
                                                let r: &mut dyn LoadedPlugin = p;
                                                let raw: *mut dyn LoadedPlugin = r;
                                                unsafe {
                                                    std::mem::transmute::<
                                                        *mut dyn LoadedPlugin,
                                                        *mut (dyn LoadedPlugin + 'static),
                                                    >(raw)
                                                }
                                            })
                                        };
                                        if let Some(p) = plugin_ptr_raw {
                                            publish_plugin_registry(
                                                &plugin_registry,
                                                new_plugin_id,
                                                Some(PluginEntry {
                                                    plugin: PluginPtr(p),
                                                    process_data: pd_ptr,
                                                }),
                                            );
                                        }
                                        let _ = evt_tx.send(PluginEvent::SlotPluginLoaded {
                                            track,
                                            slot,
                                            id: loaded_id,
                                            name: loaded_name,
                                            plugin_id: new_plugin_id,
                                            shmem_id,
                                        });
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            error = ?e,
                                            new_plugin_id,
                                            "failed to create ProcessData shmem"
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, ?format, path = %path.display(), "load failed");
                        }
                    }
                }
                PluginCommand::RemoveSlotPlugin { track, slot } => {
                    tracks.mutate(|t| {
                            if let Some(chain) = t.chains.get_mut(&track) {
                                remove_plugin(chain, slot);
                            }
                    });
                    if let Some(removed_pid) = plugin_lookup.remove(&(track, slot)) {
                        plugin_shmems.remove(&removed_pid);
                        publish_plugin_registry(&plugin_registry, removed_pid, None);
                    }
                }
                PluginCommand::MoveSlot { track, from, to } => {
                    tracks.mutate(|t| {
                            if let Some(chain) = t.chains.get_mut(&track) {
                                move_plugin(chain, from, to);
                            }
                    });
                }
                PluginCommand::RemoveTrack { track } => {
                    tracks.mutate(|t| {
                            // Destroy every plugin's GUI before dropping
                            // the chain so the CLAP gui lifecycle (destroy
                            // must precede plugin destroy) is honoured.
                            if let Some(mut chain) = t.chains.remove(&track) {
                                for mfx in &mut chain.midi_fx_chain {
                                    mfx.gui_destroy();
                                }
                                if let Some(inst) = chain.instrument.as_mut() {
                                    inst.gui_destroy();
                                }
                                for fx in &mut chain.fx_chain {
                                    fx.gui_destroy();
                                }
                                // Plugins drop here, on plugin-main.
                            }
                            // The GUI just removed `track` from
                            // `song.tracks`, shifting every higher track
                            // down by 1. Mirror that on our chain map so
                            // chain keys keep matching the song indices.
                            t.shift_after_remove(track);
                    });
                }
                PluginCommand::SwapTracks { a, b } => {
                    tracks.mutate(|t| {
                            t.swap_indices(a, b);
                    });
                }
                PluginCommand::ReorderTracks(order) => {
                    tracks.mutate(|t| {
                            t.reorder_indices(&order);
                    });
                }
                PluginCommand::RequestSlotState { track, slot } => {
                    let data = match tracks.plugin_at_mut(track, slot) {
                        Some(plugin) => match plugin.state_save() {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!(error = ?e, "state_save failed");
                                None
                            }
                        },
                        None => None,
                    };
                    let _ = evt_tx.send(PluginEvent::SlotPluginState { track, slot, data });
                }
                PluginCommand::RequestAllStates => {
                    let entries = collect_all_states(&mut tracks);
                    let _ = evt_tx.send(PluginEvent::AllPluginStates { entries });
                }
                PluginCommand::OpenSlotGui { track, slot, host_hwnd } => {
                    match open_gui(&mut tracks, track, slot, host_hwnd) {
                        Ok(Some((w, h))) => {
                            let _ = evt_tx.send(PluginEvent::SlotGuiOpened {
                                track,
                                slot,
                                width: w,
                                height: h,
                            });
                        }
                        Ok(None) => {
                            let _ = evt_tx.send(PluginEvent::SlotGuiClosed { track, slot });
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, track, ?slot, "failed to open GUI");
                            close_gui(&mut tracks, track, slot);
                            let _ = evt_tx.send(PluginEvent::SlotGuiClosed { track, slot });
                        }
                    }
                }
                PluginCommand::CloseSlotGui { track, slot } => {
                    close_gui(&mut tracks, track, slot);
                    let _ = evt_tx.send(PluginEvent::SlotGuiClosed { track, slot });
                }
                PluginCommand::ResizeSlotGui {
                    track,
                    slot,
                    width,
                    height,
                } => {
                    resize_gui(&mut tracks, track, slot, width, height);
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

    tracks.shutdown();
    tracing::info!("plugin-main thread exiting (WM_QUIT)");
}

/// Place `plugin` into the slot, replacing any previous occupant. Audio
/// thread is stopped by the caller (via `TracksHandle::mutate`).
fn install_plugin(
    chain: &mut Chain,
    slot: PluginSlot,
    mut plugin: Box<dyn LoadedPlugin>,
    sample_rate: u32,
    max_frames: u32,
) {
    // A2: the legacy audio thread used to call activate / start_processing
    // in its prologue. With the audio engine driving plugin.process() via
    // the process_server worker pool, the plugin must be in the started
    // state by the time it lands in the chain.
    if let Err(e) = plugin.activate(f64::from(sample_rate), 64, max_frames) {
        tracing::error!(error = ?e, ?slot, "plugin.activate failed");
    }
    if let Err(e) = plugin.start_processing() {
        tracing::error!(error = ?e, ?slot, "start_processing failed; plugin may be silent");
    }
    match slot {
        PluginSlot::Instrument => {
            if let Some(mut old) = chain.instrument.replace(plugin) {
                old.stop_processing();
                old.deactivate();
                old.gui_destroy();
            }
        }
        PluginSlot::Fx(i) => {
            let i = i as usize;
            if i < chain.fx_chain.len() {
                if let Some(old) = chain.fx_chain.get_mut(i) {
                    old.stop_processing();
                    old.deactivate();
                    old.gui_destroy();
                }
                chain.fx_chain[i] = plugin;
            } else {
                chain.fx_chain.push(plugin);
            }
        }
        PluginSlot::MidiFx(i) => {
            let i = i as usize;
            if i < chain.midi_fx_chain.len() {
                if let Some(old) = chain.midi_fx_chain.get_mut(i) {
                    old.stop_processing();
                    old.deactivate();
                    old.gui_destroy();
                }
                chain.midi_fx_chain[i] = plugin;
            } else {
                chain.midi_fx_chain.push(plugin);
            }
        }
    }
}

fn remove_plugin(chain: &mut Chain, slot: PluginSlot) {
    match slot {
        PluginSlot::Instrument => {
            if let Some(mut old) = chain.instrument.take() {
                old.stop_processing();
                old.deactivate();
                old.gui_destroy();
            }
        }
        PluginSlot::Fx(i) => {
            let i = i as usize;
            if i < chain.fx_chain.len() {
                let mut old = chain.fx_chain.remove(i);
                old.stop_processing();
                old.deactivate();
                old.gui_destroy();
            }
        }
        PluginSlot::MidiFx(i) => {
            let i = i as usize;
            if i < chain.midi_fx_chain.len() {
                let mut old = chain.midi_fx_chain.remove(i);
                old.stop_processing();
                old.deactivate();
                old.gui_destroy();
            }
        }
    }
}

fn move_plugin(chain: &mut Chain, from: PluginSlot, to: PluginSlot) {
    // Only Fx↔Fx and MidiFx↔MidiFx reorders are supported for MVP.
    match (from, to) {
        (PluginSlot::Fx(a), PluginSlot::Fx(b)) => {
            let a = a as usize;
            let b = b as usize;
            if a < chain.fx_chain.len() && b < chain.fx_chain.len() && a != b {
                let plugin = chain.fx_chain.remove(a);
                chain.fx_chain.insert(b, plugin);
            }
        }
        (PluginSlot::MidiFx(a), PluginSlot::MidiFx(b)) => {
            let a = a as usize;
            let b = b as usize;
            if a < chain.midi_fx_chain.len() && b < chain.midi_fx_chain.len() && a != b {
                let plugin = chain.midi_fx_chain.remove(a);
                chain.midi_fx_chain.insert(b, plugin);
            }
        }
        _ => {}
    }
}

fn collect_all_states(handle: &mut TracksHandle) -> Vec<SlotState> {
    let mut out = Vec::new();
    // Iterate tracks in deterministic id order so save files diff cleanly.
    let mut keys: Vec<u32> = handle.tracks.chains.keys().copied().collect();
    keys.sort();
    for &track_id in &keys {
        let (mfx_count, has_inst, fx_count) = {
            let Some(chain) = handle.tracks.chains.get(&track_id) else {
                continue;
            };
            (
                chain.midi_fx_chain.len(),
                chain.instrument.is_some(),
                chain.fx_chain.len(),
            )
        };
        for i in 0..mfx_count {
            let slot = PluginSlot::MidiFx(i as u32);
            if let Some(plugin) = handle.plugin_at_mut(track_id, slot) {
                let data = plugin.state_save().ok().flatten();
                out.push(SlotState {
                    track: track_id,
                    slot,
                    data,
                });
            }
        }
        if has_inst {
            let slot = PluginSlot::Instrument;
            if let Some(plugin) = handle.plugin_at_mut(track_id, slot) {
                let data = plugin.state_save().ok().flatten();
                out.push(SlotState {
                    track: track_id,
                    slot,
                    data,
                });
            }
        }
        for i in 0..fx_count {
            let slot = PluginSlot::Fx(i as u32);
            if let Some(plugin) = handle.plugin_at_mut(track_id, slot) {
                let data = plugin.state_save().ok().flatten();
                out.push(SlotState {
                    track: track_id,
                    slot,
                    data,
                });
            }
        }
    }
    out
}

fn open_gui(
    handle: &mut TracksHandle,
    track: u32,
    slot: PluginSlot,
    host_hwnd: u64,
) -> Result<Option<(u32, u32)>> {
    let Some(plugin) = handle.plugin_at_mut(track, slot) else {
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

fn close_gui(handle: &mut TracksHandle, track: u32, slot: PluginSlot) {
    let Some(plugin) = handle.plugin_at_mut(track, slot) else {
        return;
    };
    let _ = plugin.gui_hide();
    plugin.gui_destroy();
}

fn resize_gui(
    handle: &mut TracksHandle,
    track: u32,
    slot: PluginSlot,
    width: u32,
    height: u32,
) {
    let Some(plugin) = handle.plugin_at_mut(track, slot) else {
        return;
    };
    if let Err(e) = plugin.gui_set_size(width, height) {
        tracing::warn!(error = ?e, width, height, track, ?slot, "gui.set_size failed");
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
        MainToChild::SetSlotPlugin {
            track,
            slot,
            format,
            path,
            plugin_id,
            initial_state,
        } => {
            tracing::info!(
                track,
                ?slot,
                ?format,
                path = %path.display(),
                id = %plugin_id,
                has_state = initial_state.is_some(),
                "received SetSlotPlugin"
            );
            plugin.send(PluginCommand::SetSlotPlugin {
                track,
                slot,
                format,
                path,
                plugin_id,
                initial_state,
            });
        }
        MainToChild::RemoveSlotPlugin { track, slot } => {
            tracing::info!(track, ?slot, "received RemoveSlotPlugin");
            plugin.send(PluginCommand::RemoveSlotPlugin { track, slot });
        }
        MainToChild::MoveSlot { track, from, to } => {
            tracing::info!(track, ?from, ?to, "received MoveSlot");
            plugin.send(PluginCommand::MoveSlot { track, from, to });
        }
        MainToChild::RemoveTrack { track } => {
            tracing::info!(track, "received RemoveTrack");
            plugin.send(PluginCommand::RemoveTrack { track });
        }
        MainToChild::SwapTracks { a, b } => {
            tracing::info!(a, b, "received SwapTracks");
            plugin.send(PluginCommand::SwapTracks { a, b });
        }
        MainToChild::ReorderTracks(order) => {
            tracing::info!(?order, "received ReorderTracks");
            plugin.send(PluginCommand::ReorderTracks(order));
        }
        MainToChild::RequestSlotState { track, slot } => {
            tracing::info!(track, ?slot, "received RequestSlotState");
            plugin.send(PluginCommand::RequestSlotState { track, slot });
        }
        MainToChild::RequestAllStates => {
            tracing::info!("received RequestAllStates");
            plugin.send(PluginCommand::RequestAllStates);
        }
        MainToChild::OpenSlotGuiEmbedded {
            track,
            slot,
            host_hwnd,
        } => {
            tracing::info!(track, ?slot, host_hwnd, "received OpenSlotGuiEmbedded");
            plugin.send(PluginCommand::OpenSlotGui { track, slot, host_hwnd });
        }
        MainToChild::CloseSlotGui { track, slot } => {
            tracing::info!(track, ?slot, "received CloseSlotGui");
            plugin.send(PluginCommand::CloseSlotGui { track, slot });
        }
        MainToChild::ResizeSlotGui {
            track,
            slot,
            width,
            height,
        } => {
            tracing::info!(track, ?slot, width, height, "received ResizeSlotGui");
            plugin.send(PluginCommand::ResizeSlotGui {
                track,
                slot,
                width,
                height,
            });
        }
        MainToChild::OpenWorkerPool {
            n_workers,
            worker_bridge_shmem_id,
            wake_event_names,
            done_event_names,
        } => {
            tracing::info!(
                n_workers,
                shmem = %worker_bridge_shmem_id,
                "received OpenWorkerPool"
            );
            plugin.send(PluginCommand::OpenWorkerPool {
                n_workers,
                worker_bridge_shmem_id,
                wake_event_names,
                done_event_names,
            });
        }
        MainToChild::CloseWorkerPool => {
            tracing::info!("received CloseWorkerPool");
            plugin.send(PluginCommand::CloseWorkerPool);
        }
        // OpenPluginShmem / ClosePluginShmem flow daw_gui → daw_audio,
        // not into the plugin host (the plugin host is the *creator* of
        // the shmem and already owns the handle in `plugin_shmems`).
        // We log if these arrive here just to flag a routing bug.
        MainToChild::OpenPluginShmem { plugin_id, shmem_id, track, slot } => {
            tracing::warn!(
                plugin_id,
                shmem = %shmem_id,
                track,
                ?slot,
                "OpenPluginShmem reached plugin_host (should be daw_audio only)"
            );
        }
        MainToChild::ClosePluginShmem { plugin_id } => {
            tracing::warn!(
                plugin_id,
                "ClosePluginShmem reached plugin_host (should be daw_audio only)"
            );
        }
        other => {
            tracing::info!(?other, "received (no handler)");
        }
    }
}

// --- Chain + audio thread ------------------------------------------------

/// Wraps a raw pointer so it can be moved into the audio thread closure.
/// Both CLAP and VST3 partition their APIs between main-thread and
/// audio-thread, so simultaneous main-thread GUI calls and audio-thread
/// `process()` calls touch disjoint fields (this assumes plugins conform
/// to the spec). The pointer is a trait-object fat pointer (data +
/// vtable) so the audio thread can call `LoadedPlugin` methods against
/// whichever backend — CLAP or VST3 — is behind the slot.
pub struct PluginPtr(pub *mut (dyn LoadedPlugin + 'static));
unsafe impl Send for PluginPtr {}
// `Sync` is the contract that the plugin-main thread and the
// process-server worker that owns this plugin's slot won't touch the
// instance simultaneously. The plugin-main thread restarts the worker
// pool whenever it mutates the chain (load/remove/swap), so a plugin
// pointer is only ever accessed by one thread at a time.
unsafe impl Sync for PluginPtr {}

/// Per-plugin process-server entry. `plugin` is the trait-object pointer
/// the worker calls `process()` on; `process_data` is the shared-memory
/// `ProcessData` slot the audio engine wrote inputs into. The pair lives
/// in `plugin_registry` keyed by `plugin_id`.
pub struct PluginEntry {
    pub plugin: PluginPtr,
    pub process_data: *mut common::process_data::ProcessData,
}
unsafe impl Send for PluginEntry {}
unsafe impl Sync for PluginEntry {}

/// Lock-free `plugin_id` → `PluginEntry` lookup the worker pool reads
/// during dispatch. The plugin-main thread publishes a fresh `Vec` on
/// every plugin add / remove via `ArcSwap::store`; old snapshots stay
/// valid until the last worker drops its `Guard`.
pub type PluginRegistry =
    std::sync::Arc<arc_swap::ArcSwap<Vec<Option<PluginEntry>>>>;

/// Per-track signal chain owned on the plugin-main thread. The
/// process-server worker pool reads `PluginPtr` snapshots from the
/// shared `plugin_registry` to call `plugin.process()`.
///
/// Each slot holds a `Box<dyn LoadedPlugin>` so CLAP (`ClapPlugin`) and
/// VST3 (`Vst3Plugin`) implementations can coexist on the same chain.
/// Boxing keeps the plugin pinned on the heap so the raw pointers stored
/// in `PluginEntry` remain valid across `Vec` reallocations.
#[derive(Default)]
struct Chain {
    /// Note-effect plugins executed before the instrument (e.g. arpeggiators).
    /// Events flow left-to-right, with each plugin's emitted notes feeding
    /// the next.
    midi_fx_chain: Vec<Box<dyn LoadedPlugin>>,
    /// Instrument slot (note→audio). `None` = no instrument loaded on the
    /// track; audio thread produces silence at the instrument stage.
    instrument: Option<Box<dyn LoadedPlugin>>,
    /// Audio effects applied in order after the instrument.
    fx_chain: Vec<Box<dyn LoadedPlugin>>,
}

impl Chain {
    fn plugin_at_mut(&mut self, slot: PluginSlot) -> Option<&mut (dyn LoadedPlugin + '_)> {
        match slot {
            PluginSlot::MidiFx(i) => self
                .midi_fx_chain
                .get_mut(i as usize)
                .map(|b| &mut **b as &mut dyn LoadedPlugin),
            PluginSlot::Instrument => self
                .instrument
                .as_mut()
                .map(|b| &mut **b as &mut dyn LoadedPlugin),
            PluginSlot::Fx(i) => self
                .fx_chain
                .get_mut(i as usize)
                .map(|b| &mut **b as &mut dyn LoadedPlugin),
        }
    }
}

/// All tracks with loaded plugins. Lazily-populated: `ensure_track` creates
/// an empty chain on first access so a Track with no plugins isn't stored.
#[derive(Default)]
struct Tracks {
    chains: HashMap<u32, Chain>,
}

impl Tracks {
    fn ensure_track(&mut self, track: u32) -> &mut Chain {
        self.chains.entry(track).or_default()
    }

    fn plugin_at_mut(
        &mut self,
        track: u32,
        slot: PluginSlot,
    ) -> Option<&mut (dyn LoadedPlugin + '_)> {
        self.chains.get_mut(&track).and_then(|c| c.plugin_at_mut(slot))
    }

    /// After removing the chain at `removed`, shift every entry with a
    /// higher key down by one so the keys stay aligned with the song's
    /// `tracks` Vec.
    fn shift_after_remove(&mut self, removed: u32) {
        let mut keys: Vec<u32> = self
            .chains
            .keys()
            .copied()
            .filter(|&k| k > removed)
            .collect();
        keys.sort_unstable();
        for k in keys {
            if let Some(c) = self.chains.remove(&k) {
                self.chains.insert(k - 1, c);
            }
        }
    }

    /// Reorder chains so the entry previously at `order[i]` ends up at
    /// the new index `i`. Indices not mentioned keep their original key.
    fn reorder_indices(&mut self, order: &[u32]) {
        let identity = order.iter().enumerate().all(|(i, &o)| o == i as u32);
        if identity {
            return;
        }
        let mapping: std::collections::HashMap<u32, u32> = order
            .iter()
            .enumerate()
            .map(|(new_i, &old_i)| (old_i, new_i as u32))
            .collect();
        let chains_snapshot: Vec<(u32, _)> =
            std::mem::take(&mut self.chains).into_iter().collect();
        for (old_i, c) in chains_snapshot {
            let new_i = mapping.get(&old_i).copied().unwrap_or(old_i);
            self.chains.insert(new_i, c);
        }
    }

    /// Swap chains at `a` and `b`. No-op when either side is missing.
    fn swap_indices(&mut self, a: u32, b: u32) {
        if a == b {
            return;
        }
        if let (Some(ca), Some(cb)) = (self.chains.remove(&a), self.chains.remove(&b)) {
            self.chains.insert(a, cb);
            self.chains.insert(b, ca);
        } else if let Some(c) = self.chains.remove(&a) {
            self.chains.insert(b, c);
        } else if let Some(c) = self.chains.remove(&b) {
            self.chains.insert(a, c);
        }
    }
}

/// RAII owner for every track's signal chain. Mutations go through
/// `mutate` which is now a plain field update (the daw_audio engine
/// drives plugin.process() via the process_server worker pool).
struct TracksHandle {
    tracks: Tracks,
}

impl TracksHandle {
    fn new() -> Self {
        Self {
            tracks: Tracks::default(),
        }
    }

    fn plugin_at_mut(
        &mut self,
        track: u32,
        slot: PluginSlot,
    ) -> Option<&mut (dyn LoadedPlugin + '_)> {
        self.tracks.plugin_at_mut(track, slot)
    }

    /// Apply `f` to the chains. With the audio engine driving
    /// plugin.process() through the process_server worker pool (and
    /// `install_plugin` / `remove_plugin` handling activate /
    /// start_processing for us) the mutation is just a plain field
    /// update.
    fn mutate<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Tracks),
    {
        f(&mut self.tracks);
    }

    fn shutdown(mut self) {
        // Drop plugins on the main thread.
        for chain in self.tracks.chains.values_mut() {
            for mfx in &mut chain.midi_fx_chain {
                mfx.gui_destroy();
            }
            if let Some(inst) = chain.instrument.as_mut() {
                inst.gui_destroy();
            }
            for fx in &mut chain.fx_chain {
                fx.gui_destroy();
            }
        }
        // Plugins drop here, running `Plugin::drop` on the main thread.
    }
}


