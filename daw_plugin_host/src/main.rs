mod clap_host;
mod clap_plugin;
mod plugin_instance;
mod vst3_events;
mod vst3_host;
mod vst3_plugin;
mod vst3_stream;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use common::audio_bridge::{AudioBridgeHandle, CHANNELS};
use common::model::{NoteEvent, Song};
use common::plugin_format::PluginFormat;
use common::protocol::{AudioSession, ChildKind, ChildToMain, MainToChild, PluginSlot, SlotState};
use common::timing::{song_bounds_samples, song_ended};
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

use crate::plugin_instance::{
    HostCallbacks, LoadedPlugin, NoteTransition, TimedNoteEvent, load_plugin,
};

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
    },
    SlotPluginState {
        track: u32,
        slot: PluginSlot,
        data: Option<Vec<u8>>,
    },
    AllPluginStates {
        entries: Vec<SlotState>,
    },
    ExportWavComplete {
        error: Option<String>,
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
            PluginEvent::SlotPluginLoaded { track, slot, id, name } => {
                ChildToMain::SlotPluginLoaded { track, slot, id, name }
            }
            PluginEvent::SlotPluginState { track, slot, data } => {
                ChildToMain::SlotPluginState { track, slot, data }
            }
            PluginEvent::AllPluginStates { entries } => ChildToMain::AllPluginStates { entries },
            PluginEvent::ExportWavComplete { error } => {
                ChildToMain::ExportWavComplete { error }
            }
        }
    }
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
    LoadSong(Song),
    Play,
    Stop,
    SetLoop(bool),
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
    /// Per-track mixer atomics. Applied immediately on the plugin-main
    /// thread without stopping the audio thread (the audio thread reads
    /// the atomic through the shared `Arc<TrackAudioParams>`).
    SetTrackVolume {
        track: u32,
        volume: f32,
    },
    SetTrackPan {
        track: u32,
        pan: f32,
    },
    SetTrackMuted {
        track: u32,
        muted: bool,
    },
    SetTrackSolo {
        track: u32,
        solo: bool,
    },
    SetVocalAudio {
        track: u32,
        clip_start_samples: u64,
        samples: Vec<f32>,
    },
    ExportWav {
        path: std::path::PathBuf,
    },
    Shutdown,
}

/// Atomics shared between the plugin-main thread (writer, on IPC commands
/// and `LoadSong`) and the audio thread (reader, every buffer). `volume`
/// and `pan` are bit-packed `f32` so the enclosing struct can stay
/// `Send + Sync` without a mutex.
struct TrackAudioParams {
    volume: std::sync::atomic::AtomicU32,
    pan: std::sync::atomic::AtomicU32,
    muted: std::sync::atomic::AtomicBool,
    solo: std::sync::atomic::AtomicBool,
}

impl TrackAudioParams {
    fn new(volume: f32, pan: f32, muted: bool, solo: bool) -> Self {
        Self {
            volume: std::sync::atomic::AtomicU32::new(volume.to_bits()),
            pan: std::sync::atomic::AtomicU32::new(pan.to_bits()),
            muted: std::sync::atomic::AtomicBool::new(muted),
            solo: std::sync::atomic::AtomicBool::new(solo),
        }
    }

    fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Acquire))
    }

    fn pan(&self) -> f32 {
        f32::from_bits(self.pan.load(Ordering::Acquire))
    }

    fn muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    fn solo(&self) -> bool {
        self.solo.load(Ordering::Acquire)
    }

    fn set_volume(&self, v: f32) {
        self.volume.store(v.to_bits(), Ordering::Release);
    }
    fn set_pan(&self, v: f32) {
        self.pan.store(v.to_bits(), Ordering::Release);
    }
    fn set_muted(&self, v: bool) {
        self.muted.store(v, Ordering::Release);
    }
    fn set_solo(&self, v: bool) {
        self.solo.store(v, Ordering::Release);
    }
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
    let mut tracks = TracksHandle::new();

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
                    tracks.shutdown();
                    tracing::info!("plugin-main thread exiting");
                    return;
                }
                PluginCommand::SetSlotPlugin {
                    track,
                    slot,
                    format,
                    path,
                    plugin_id,
                    initial_state,
                } => {
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
                            let result = tracks.mutate(
                                &session,
                                &playback_state,
                                &song_store,
                                &loop_state,
                                |t| install_plugin(t.ensure_track(track), slot, plugin),
                            );
                            if let Err(e) = result {
                                tracing::error!(error = ?e, track, ?slot, "failed to install plugin");
                            } else {
                                let _ = evt_tx.send(PluginEvent::SlotPluginLoaded {
                                    track,
                                    slot,
                                    id: loaded_id,
                                    name: loaded_name,
                                });
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, ?format, path = %path.display(), "load failed");
                        }
                    }
                }
                PluginCommand::RemoveSlotPlugin { track, slot } => {
                    let _ = tracks.mutate(
                        &session,
                        &playback_state,
                        &song_store,
                        &loop_state,
                        |t| {
                            if let Some(chain) = t.chains.get_mut(&track) {
                                remove_plugin(chain, slot);
                            }
                        },
                    );
                }
                PluginCommand::MoveSlot { track, from, to } => {
                    let _ = tracks.mutate(
                        &session,
                        &playback_state,
                        &song_store,
                        &loop_state,
                        |t| {
                            if let Some(chain) = t.chains.get_mut(&track) {
                                move_plugin(chain, from, to);
                            }
                        },
                    );
                }
                PluginCommand::RemoveTrack { track } => {
                    let _ = tracks.mutate(
                        &session,
                        &playback_state,
                        &song_store,
                        &loop_state,
                        |t| {
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
                        },
                    );
                }
                PluginCommand::LoadSong(song) => {
                    tracing::info!(bpm = song.bpm, tracks = song.tracks.len(), "LoadSong");
                    // Sync each track's mixer atomics with the model the
                    // GUI just sent us. New tracks get their params
                    // created on demand; existing ones have their atomics
                    // updated in place so the audio thread picks the new
                    // values up on its next buffer without a restart.
                    for (i, track) in song.tracks.iter().enumerate() {
                        let idx = i as u32;
                        let params = tracks.tracks.ensure_params(idx);
                        params.set_volume(track.volume);
                        params.set_pan(track.pan);
                        params.set_muted(track.muted);
                        params.set_solo(track.solo);
                    }
                    song_store.store(Some(Arc::new(song)));
                }
                PluginCommand::SetTrackVolume { track, volume } => {
                    tracks.tracks.ensure_params(track).set_volume(volume);
                }
                PluginCommand::SetTrackPan { track, pan } => {
                    tracks.tracks.ensure_params(track).set_pan(pan);
                }
                PluginCommand::SetTrackMuted { track, muted } => {
                    tracks.tracks.ensure_params(track).set_muted(muted);
                }
                PluginCommand::SetTrackSolo { track, solo } => {
                    tracks.tracks.ensure_params(track).set_solo(solo);
                }
                PluginCommand::ExportWav { path } => {
                    let error = export_wav_offline(
                        &mut tracks,
                        &session,
                        &playback_state,
                        &song_store,
                        &loop_state,
                        &path,
                    );
                    let _ = evt_tx.send(PluginEvent::ExportWavComplete {
                        error: error.err().map(|e| format!("{e:#}")),
                    });
                }
                PluginCommand::SetVocalAudio {
                    track,
                    clip_start_samples,
                    samples,
                } => {
                    // Hot-swap: the audio thread holds an Arc clone of
                    // the ArcSwapOption and will see the new value on
                    // its very next buffer — no restart needed.
                    let swap = tracks
                        .tracks
                        .vocal
                        .entry(track)
                        .or_insert_with(|| Arc::new(ArcSwapOption::empty()));
                    swap.store(Some(Arc::new(VocalAudio {
                        clip_start_samples,
                        samples,
                    })));
                    tracing::info!(track, clip_start_samples, "vocal audio hot-swapped");
                }
                PluginCommand::Play => {
                    playback_state.store(PlaybackCommand::Play as u8, Ordering::Release);
                    // Start the audio thread if it isn't running yet.
                    // This handles the pure-vocal workflow where no
                    // plugin was ever installed (so `mutate` was never
                    // called and `start_audio` was never triggered).
                    if tracks.audio.is_none()
                        && let Err(e) = tracks.start_audio(
                            &session,
                            &playback_state,
                            &song_store,
                            &loop_state,
                        )
                    {
                        tracing::error!(error = ?e, "failed to start audio thread on Play");
                    }
                }
                PluginCommand::Stop => {
                    playback_state.store(PlaybackCommand::Stop as u8, Ordering::Release);
                }
                PluginCommand::SetLoop(on) => {
                    loop_state.store(on, Ordering::Release);
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
fn install_plugin(chain: &mut Chain, slot: PluginSlot, plugin: Box<dyn LoadedPlugin>) {
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
                chain.fx_chain.push(plugin);
            }
        }
        PluginSlot::MidiFx(i) => {
            let i = i as usize;
            if i < chain.midi_fx_chain.len() {
                if let Some(old) = chain.midi_fx_chain.get_mut(i) {
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
        PluginSlot::MidiFx(i) => {
            let i = i as usize;
            if i < chain.midi_fx_chain.len() {
                let mut old = chain.midi_fx_chain.remove(i);
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
        MainToChild::RequestSlotState { track, slot } => {
            tracing::info!(track, ?slot, "received RequestSlotState");
            plugin.send(PluginCommand::RequestSlotState { track, slot });
        }
        MainToChild::RequestAllStates => {
            tracing::info!("received RequestAllStates");
            plugin.send(PluginCommand::RequestAllStates);
        }
        MainToChild::SetLoop(on) => {
            tracing::info!(on, "received SetLoop");
            plugin.send(PluginCommand::SetLoop(on));
        }
        MainToChild::SetTrackVolume { track, volume } => {
            plugin.send(PluginCommand::SetTrackVolume { track, volume });
        }
        MainToChild::SetTrackPan { track, pan } => {
            plugin.send(PluginCommand::SetTrackPan { track, pan });
        }
        MainToChild::SetTrackMuted { track, muted } => {
            plugin.send(PluginCommand::SetTrackMuted { track, muted });
        }
        MainToChild::SetTrackSolo { track, solo } => {
            plugin.send(PluginCommand::SetTrackSolo { track, solo });
        }
        MainToChild::ExportWav { path } => {
            tracing::info!(path = %path.display(), "received ExportWav");
            plugin.send(PluginCommand::ExportWav { path });
        }
        MainToChild::SetVocalAudio {
            track,
            clip,
            clip_start_samples,
            sample_rate: _,
            samples,
        } => {
            tracing::info!(
                track,
                clip,
                clip_start_samples,
                len = samples.len(),
                "received SetVocalAudio"
            );
            plugin.send(PluginCommand::SetVocalAudio {
                track,
                clip_start_samples,
                samples,
            });
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
struct PluginPtr(*mut (dyn LoadedPlugin + 'static));
unsafe impl Send for PluginPtr {}

/// Per-track signal chain owned on the plugin-main thread. The audio thread
/// receives raw pointer snapshots at spawn time (see [`AudioRouting`]).
///
/// Each slot holds a `Box<dyn LoadedPlugin>` so CLAP (`ClapPlugin`) and
/// VST3 (`Vst3Plugin`) implementations can coexist on the same chain.
/// Boxing keeps the plugin pinned on the heap so raw pointers snapshotted
/// into the audio thread remain valid across `Vec` reallocations.
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

/// Pre-rendered vocal audio for a track. Shared between the plugin-main
/// thread (writer) and the audio thread (reader) via `ArcSwapOption` so
/// new synthesis results are picked up on the next buffer without
/// restarting the audio thread.
#[derive(Default, Clone)]
struct VocalAudio {
    /// Absolute sample position in the song where playback of `samples`
    /// should begin.
    clip_start_samples: u64,
    /// Mono f32 samples.
    samples: Vec<f32>,
}

/// All tracks with loaded plugins. Lazily-populated: `ensure_track` creates
/// an empty chain on first access so a Track with no plugins isn't stored.
#[derive(Default)]
struct Tracks {
    chains: HashMap<u32, Chain>,
    /// Mixer atomics per track (volume/pan/mute/solo). Kept alive by this
    /// `Arc` on the plugin-main thread and snapshotted into each
    /// `TrackRouting` at `start_audio` time, so the audio thread never
    /// takes a lock.
    params: HashMap<u32, Arc<TrackAudioParams>>,
    /// Pre-rendered vocal audio per track. Updated via `ArcSwapOption`
    /// so the audio thread sees new data immediately (no restart needed).
    vocal: HashMap<u32, Arc<ArcSwapOption<VocalAudio>>>,
}

impl Tracks {
    fn ensure_track(&mut self, track: u32) -> &mut Chain {
        self.chains.entry(track).or_default()
    }

    fn ensure_params(&mut self, track: u32) -> Arc<TrackAudioParams> {
        self.params
            .entry(track)
            .or_insert_with(|| Arc::new(TrackAudioParams::new(1.0, 0.0, false, false)))
            .clone()
    }

    fn plugin_at_mut(
        &mut self,
        track: u32,
        slot: PluginSlot,
    ) -> Option<&mut (dyn LoadedPlugin + '_)> {
        self.chains.get_mut(&track).and_then(|c| c.plugin_at_mut(slot))
    }
}

/// Plain-pointer snapshot of one track's chain handed to the audio thread.
/// Valid as long as the chain is not mutated (which first triggers an
/// audio-thread stop + restart on the main side).
///
/// `params` is shared (Arc) with the plugin-main thread: SetTrackXxx
/// commands write the atomics inside while this snapshot is live, so
/// the audio thread sees the new values on its next buffer.
struct TrackRouting {
    track_id: u32,
    params: Arc<TrackAudioParams>,
    vocal: Arc<ArcSwapOption<VocalAudio>>,
    midi_fx_chain: Vec<PluginPtr>,
    instrument: Option<PluginPtr>,
    fx_chain: Vec<PluginPtr>,
}

struct AudioRouting {
    tracks: Vec<TrackRouting>,
}

/// RAII owner for every track's signal chain plus the shared audio thread.
/// Any chain mutation (set/remove/move a slot) goes through the helper
/// methods which stop + restart the audio thread so the raw pointers stay
/// valid.
struct TracksHandle {
    tracks: Tracks,
    audio: Option<AudioThread>,
}

struct AudioThread {
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    request_sem: Arc<Semaphore>,
}

impl TracksHandle {
    fn new() -> Self {
        Self {
            tracks: Tracks::default(),
            audio: None,
        }
    }

    fn plugin_at_mut(
        &mut self,
        track: u32,
        slot: PluginSlot,
    ) -> Option<&mut (dyn LoadedPlugin + '_)> {
        self.tracks.plugin_at_mut(track, slot)
    }

    /// Stop the audio thread, run `f` to mutate the tracks on the main
    /// thread, and restart the audio thread with the new routing.
    fn mutate<F>(
        &mut self,
        session: &AudioSession,
        playback_state: &Arc<AtomicU8>,
        song_store: &Arc<ArcSwapOption<Song>>,
        loop_state: &Arc<AtomicBool>,
        f: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut Tracks),
    {
        self.stop_audio();
        f(&mut self.tracks);
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
        for chain in self.tracks.chains.values_mut() {
            for mfx in &mut chain.midi_fx_chain {
                mfx.deactivate();
            }
            if let Some(inst) = chain.instrument.as_mut() {
                inst.deactivate();
            }
            for fx in &mut chain.fx_chain {
                fx.deactivate();
            }
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
        for chain in self.tracks.chains.values_mut() {
            for plugin in chain
                .midi_fx_chain
                .iter_mut()
                .chain(chain.instrument.iter_mut())
                .chain(chain.fx_chain.iter_mut())
            {
                plugin
                    .activate(f64::from(session.sample_rate), 64, session.max_frames)
                    .context("plugin.activate failed")?;
            }
        }

        // Snapshot raw pointers into an AudioRouting. Track iteration order
        // is the HashMap's natural order (arbitrary but stable for the
        // lifetime of this snapshot), which is fine since tracks sum into
        // the master bus commutatively.
        let mut tracks_routing: Vec<TrackRouting> = Vec::with_capacity(self.tracks.chains.len());
        for (track_id, chain) in self.tracks.chains.iter_mut() {
            // &raw mut **b gives the *dyn LoadedPlugin* fat pointer behind
            // the Box (data + vtable), NOT a pointer to the Box struct.
            let midi_fx_chain: Vec<PluginPtr> = chain
                .midi_fx_chain
                .iter_mut()
                .map(|p| PluginPtr(&raw mut **p))
                .collect();
            let instrument = chain
                .instrument
                .as_mut()
                .map(|p| PluginPtr(&raw mut **p));
            let fx_chain: Vec<PluginPtr> = chain
                .fx_chain
                .iter_mut()
                .map(|p| PluginPtr(&raw mut **p))
                .collect();
            let params = self
                .tracks
                .params
                .entry(*track_id)
                .or_insert_with(|| Arc::new(TrackAudioParams::new(1.0, 0.0, false, false)))
                .clone();
            let vocal = self
                .tracks
                .vocal
                .entry(*track_id)
                .or_insert_with(|| Arc::new(ArcSwapOption::empty()))
                .clone();
            tracks_routing.push(TrackRouting {
                track_id: *track_id,
                params,
                vocal,
                midi_fx_chain,
                instrument,
                fx_chain,
            });
        }
        // Include tracks that have vocal audio but no plugin chain —
        // otherwise the audio thread won't see them and the vocal
        // samples never reach the master bus.
        for (track_id, vocal_swap) in &self.tracks.vocal {
            if !self.tracks.chains.contains_key(track_id) {
                let params = self
                    .tracks
                    .params
                    .entry(*track_id)
                    .or_insert_with(|| Arc::new(TrackAudioParams::new(1.0, 0.0, false, false)))
                    .clone();
                tracks_routing.push(TrackRouting {
                    track_id: *track_id,
                    params,
                    vocal: vocal_swap.clone(),
                    midi_fx_chain: Vec::new(),
                    instrument: None,
                    fx_chain: Vec::new(),
                });
            }
        }

        let routing = AudioRouting {
            tracks: tracks_routing,
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

/// Per-track mutable state owned exclusively by the audio thread.
#[derive(Default)]
struct PerTrackState {
    /// Keys currently sounding, used to auto-generate NoteOff when a new
    /// NoteOn overwrites the monophonic voice or playback stops.
    active_notes: Vec<u8>,
    /// NoteOffs that must be emitted at frame 0 of the *next* buffer (after
    /// Stop / clip-end) so notes don't hang.
    pending_offs: Vec<u8>,
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
    let max_frames = common::audio_bridge::MAX_FRAMES as usize;

    // Master accumulator and per-track scratch (pre-allocated).
    let mut master_l: Vec<f32> = vec![0.0; max_frames];
    let mut master_r: Vec<f32> = vec![0.0; max_frames];
    let mut track_l: Vec<f32> = vec![0.0; max_frames];
    let mut track_r: Vec<f32> = vec![0.0; max_frames];

    // Ping-pong MIDI event buffers: one is the "current" events fed into a
    // plugin, the other receives the plugin's output. After each plugin we
    // swap and clear. Capacity avoids reallocation at typical event rates.
    let mut midi_bus_a: Vec<TimedNoteEvent> = Vec::with_capacity(256);
    let mut midi_bus_b: Vec<TimedNoteEvent> = Vec::with_capacity(256);

    // Per-track state. Pre-populate so the audio loop never allocates.
    let mut track_state: HashMap<u32, PerTrackState> =
        HashMap::with_capacity(routing.tracks.len() * 2);
    for tr in &routing.tracks {
        track_state.insert(tr.track_id, PerTrackState::default());
    }

    // Start processing every plugin in every track chain.
    for tr in &routing.tracks {
        for mfx_ptr in &tr.midi_fx_chain {
            if let Err(e) = unsafe { &mut *mfx_ptr.0 }.start_processing() {
                tracing::error!(error = ?e, track = tr.track_id, "midi_fx.start_processing failed");
            }
        }
        if let Some(inst_ptr) = tr.instrument.as_ref()
            && let Err(e) = unsafe { &mut *inst_ptr.0 }.start_processing()
        {
            // Don't bail the whole audio thread — other tracks may still
            // have working instruments, and the failed plugin's later
            // `process()` call will simply return its own error and the
            // track stays silent.
            tracing::error!(error = ?e, track = tr.track_id, "instrument.start_processing failed");
        }
        for fx_ptr in &tr.fx_chain {
            if let Err(e) = unsafe { &mut *fx_ptr.0 }.start_processing() {
                tracing::error!(error = ?e, track = tr.track_id, "fx.start_processing failed");
            }
        }
    }

    let out_channels = CHANNELS as usize;
    let mut playing = false;
    let mut playhead: u64 = 0;
    #[cfg(debug_assertions)]
    let mut frames_since_log: u64 = 0;
    #[cfg(debug_assertions)]
    let log_interval_frames: u64 = sample_rate as u64;
    #[cfg(debug_assertions)]
    let mut track_event_count: HashMap<u32, u32> = HashMap::new();
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
                for st in track_state.values_mut() {
                    st.active_notes.clear();
                    st.pending_offs.clear();
                }
            }
            (true, PlaybackCommand::Stop) => {
                playing = false;
                for st in track_state.values_mut() {
                    for &k in &st.active_notes {
                        st.pending_offs.push(k);
                    }
                    st.active_notes.clear();
                }
            }
            _ => {}
        }

        let frames = bridge.frames_requested();
        let n = frames as usize;
        master_l[..n].fill(0.0);
        master_r[..n].fill(0.0);

        let snapshot = song_store.load();
        let song_ref = snapshot.as_deref();

        // Evaluate the global solo rule once per buffer. If any track is
        // soloed, non-solo tracks get silenced — same semantics as every
        // classic mixer.
        let any_solo = routing.tracks.iter().any(|tr| tr.params.solo());

        for tr in &routing.tracks {
            // Build this track's MIDI event bus: pending offs first (so they
            // fire at frame 0 after Stop / clip-end), then any events from
            // the song's current clip.
            midi_bus_a.clear();
            let st = track_state
                .get_mut(&tr.track_id)
                .expect("track_state pre-populated");
            for &k in &st.pending_offs {
                midi_bus_a.push(TimedNoteEvent {
                    time: 0,
                    event: NoteTransition::Off { key: k },
                });
            }
            st.pending_offs.clear();
            if playing {
                collect_events_for_buffer(
                    song_ref,
                    tr.track_id,
                    sample_rate,
                    playhead,
                    frames,
                    &mut midi_bus_a,
                    &mut st.active_notes,
                );
            }

            // MIDI FX chain: each plugin consumes midi_bus_a and emits into
            // midi_bus_b, which becomes the next stage's input.
            for mfx_ptr in &tr.midi_fx_chain {
                let mfx = unsafe { &mut *mfx_ptr.0 };
                if let Err(e) = mfx.process(frames, &midi_bus_a, &[]) {
                    tracing::error!(error = ?e, track = tr.track_id, "midi_fx.process failed");
                    break;
                }
                midi_bus_b.clear();
                mfx.drain_out_notes_into(&mut midi_bus_b);
                // CLAP requires in-events sorted by time; most plugins emit
                // in order but sort defensively.
                midi_bus_b.sort_by_key(|e| e.time);
                std::mem::swap(&mut midi_bus_a, &mut midi_bus_b);
            }

            // Instrument: consumes the (possibly-FX'd) MIDI bus and writes
            // audio into track_{l,r}. For vocal tracks without a plugin
            // instrument, pre-rendered VOICEVOX audio is copied in
            // instead. Completely empty tracks produce silence.
            track_l[..n].fill(0.0);
            track_r[..n].fill(0.0);

            // Vocal audio path: if this track has pre-rendered samples
            // and no plugin instrument, play those samples at the right
            // position relative to the playhead.
            if tr.instrument.is_none() && playing {
                let vocal_guard = tr.vocal.load();
                if let Some(vocal) = vocal_guard.as_deref()
                    && !vocal.samples.is_empty()
                {
                    let buf_start = playhead;
                    for i in 0..n {
                        let abs_sample = buf_start + i as u64;
                        if abs_sample >= vocal.clip_start_samples {
                            let idx = (abs_sample - vocal.clip_start_samples) as usize;
                            if idx < vocal.samples.len() {
                                let s = vocal.samples[idx];
                                track_l[i] = s;
                                track_r[i] = s;
                            }
                        }
                    }
                }
            }

            if let Some(inst_ptr) = tr.instrument.as_ref() {
                let inst = unsafe { &mut *inst_ptr.0 };
                #[cfg(debug_assertions)]
                {
                    let events_this_buf = midi_bus_a.len() as u32;
                    if events_this_buf > 0 {
                        *track_event_count.entry(tr.track_id).or_insert(0) += events_this_buf;
                    }
                }
                if let Err(e) = inst.process(frames, &midi_bus_a, &[]) {
                    tracing::error!(error = ?e, track = tr.track_id, "instrument.process failed");
                    continue;
                }
                if let Some(l) = inst.output_buffer(0) {
                    track_l[..n].copy_from_slice(&l[..n]);
                }
                if let Some(r) = inst.output_buffer(1).or(inst.output_buffer(0)) {
                    track_r[..n].copy_from_slice(&r[..n]);
                }
            }

            // Audio FX chain: each plugin reads track_{l,r} and writes its
            // output back into the same buffers. Reborrow via raw pointer to
            // sidestep borrow-checker friction.
            for fx_ptr in &tr.fx_chain {
                let fx = unsafe { &mut *fx_ptr.0 };
                let input = [&track_l[..n], &track_r[..n]];
                if let Err(e) = fx.process(frames, &[], &input) {
                    tracing::error!(error = ?e, track = tr.track_id, "fx.process failed");
                    break;
                }
                if let Some(l) = fx.output_buffer(0) {
                    track_l[..n].copy_from_slice(&l[..n]);
                }
                if let Some(r) = fx.output_buffer(1).or(fx.output_buffer(0)) {
                    track_r[..n].copy_from_slice(&r[..n]);
                }
            }

            // Apply the mixer strip (volume / pan / mute / solo) and sum
            // the track into the master bus. Post-fader peaks are what we
            // publish to the shmem meter, matching Renoise's behaviour.
            let volume = tr.params.volume();
            let pan = tr.params.pan().clamp(-1.0, 1.0);
            let muted = tr.params.muted();
            let solo = tr.params.solo();
            let effective_mute = muted || (any_solo && !solo);
            if effective_mute {
                bridge.set_track_peak(tr.track_id as usize, 0.0, 0.0);
            } else {
                // Equal-power pan: pan=-1 → L only, pan=0 → both at -3 dB,
                // pan=+1 → R only.
                let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
                let gain_l = angle.cos() * volume;
                let gain_r = angle.sin() * volume;
                let mut peak_l = 0.0_f32;
                let mut peak_r = 0.0_f32;
                for i in 0..n {
                    let l = track_l[i] * gain_l;
                    let r = track_r[i] * gain_r;
                    if l.abs() > peak_l {
                        peak_l = l.abs();
                    }
                    if r.abs() > peak_r {
                        peak_r = r.abs();
                    }
                    master_l[i] += l;
                    master_r[i] += r;
                }
                bridge.set_track_peak(tr.track_id as usize, peak_l, peak_r);
            }
        }

        // Once per ~second, log master peak + per-track event totals so
        // "Play clicked but no sound" can be diagnosed from the log alone.
        // Debug-assert-gated so release builds stay RT-clean.
        #[cfg(debug_assertions)]
        if playing {
            frames_since_log += frames as u64;
            if frames_since_log >= log_interval_frames {
                let master_peak: f32 = master_l[..n]
                    .iter()
                    .chain(master_r[..n].iter())
                    .fold(0.0f32, |acc, v| acc.max(v.abs()));
                tracing::info!(
                    master_peak,
                    playhead,
                    events = ?track_event_count,
                    "audio heartbeat"
                );
                frames_since_log = 0;
                track_event_count.clear();
            }
        }

        if playing {
            playhead += frames as u64;
            if song_ended(song_ref, sample_rate, playhead) {
                // Queue offs so the next buffer drains active notes cleanly
                // even if playback stops.
                for st in track_state.values_mut() {
                    for &k in &st.active_notes {
                        st.pending_offs.push(k);
                    }
                    st.active_notes.clear();
                }
                let wrap_to = if loop_state.load(Ordering::Acquire) {
                    song_bounds_samples(song_ref, sample_rate).map(|(start, _)| start)
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

        let published = if playing { playhead } else { u64::MAX };
        bridge.set_playhead_samples(published);

        // Copy the master bus into shmem as interleaved stereo.
        unsafe {
            let dst = bridge.samples_ptr();
            for i in 0..n {
                *dst.add(i * out_channels) = master_l[i];
                *dst.add(i * out_channels + 1) = master_r[i];
            }
        }

        if let Err(e) = ready_sem.release() {
            tracing::error!(error = ?e, "ready semaphore release failed");
            break;
        }
    }

    // Stop every plugin before returning; main thread's deactivate runs once
    // it joins this thread.
    for tr in &routing.tracks {
        for mfx_ptr in &tr.midi_fx_chain {
            unsafe { &mut *mfx_ptr.0 }.stop_processing();
        }
        if let Some(inst_ptr) = tr.instrument.as_ref() {
            unsafe { &mut *inst_ptr.0 }.stop_processing();
        }
        for fx_ptr in &tr.fx_chain {
            unsafe { &mut *fx_ptr.0 }.stop_processing();
        }
    }
    tracing::info!("audio thread exiting");
}

fn collect_events_for_buffer(
    song: Option<&Song>,
    track_idx: u32,
    sample_rate: u32,
    playhead: u64,
    frames: u32,
    out: &mut Vec<TimedNoteEvent>,
    active_notes: &mut Vec<u8>,
) {
    let Some(song) = song else { return };
    let Some(track) = song.tracks.get(track_idx as usize) else {
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

/// Offline-render the entire song to a stereo WAV file. Runs on the
/// plugin-main thread with the audio thread stopped. After rendering,
/// the audio thread is restarted so live playback resumes cleanly.
///
/// Tail detection: after the last song sample, rendering continues for
/// up to 10 seconds. If the master peak stays below −80 dB for 1 full
/// second, the tail is considered finished and rendering stops early.
fn export_wav_offline(
    tracks: &mut TracksHandle,
    session: &AudioSession,
    playback_state: &Arc<AtomicU8>,
    song_store: &Arc<ArcSwapOption<Song>>,
    loop_state: &Arc<AtomicBool>,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    use hound::{SampleFormat, WavSpec, WavWriter};

    let snapshot = song_store.load();
    let song = snapshot
        .as_deref()
        .context("no song loaded for export")?;

    let sr = session.sample_rate;
    let max_frames = session.max_frames;
    let bpm = song.bpm;
    let samples_per_beat = sr as f64 * 60.0 / bpm as f64;

    // Find the actual end of content: last clip end or last vocal sample,
    // whichever is later. Using song.length_beats alone would render
    // minutes of silence when the song length is much longer than the
    // clips in it.
    let mut content_end: u64 = 0;
    for track in &song.tracks {
        for clip in &track.clips {
            let end = ((clip.start_beat + clip.length_beats) * samples_per_beat) as u64;
            if end > content_end {
                content_end = end;
            }
        }
    }
    for vocal_swap in tracks.tracks.vocal.values() {
        let guard = vocal_swap.load();
        if let Some(v) = guard.as_deref() {
            let end = v.clip_start_samples + v.samples.len() as u64;
            if end > content_end {
                content_end = end;
            }
        }
    }
    // Fall back to song length if no clips/vocals at all.
    if content_end == 0 {
        content_end = (song.length_beats * samples_per_beat) as u64;
    }

    // Maximum tail: 10 seconds after content ends.
    let max_tail = sr as u64 * 10;
    let total_max = content_end + max_tail;
    // Silence threshold for tail cutoff (−60 dB ≈ 0.001 linear).
    // Some plugins emit tiny residual noise even with no input, so the
    // original −80 dB threshold never triggered.
    let silence_thresh: f32 = 0.001;
    // Half a second of continuous silence → stop rendering the tail.
    let silence_cutoff_samples = sr as u64 / 2;

    tracing::info!(
        content_end,
        max_tail,
        path = %path.display(),
        "starting offline WAV export"
    );

    // Stop the live audio thread so we own all the plugins.
    tracks.stop_audio();

    // Re-activate + start_processing every plugin for the offline run.
    for chain in tracks.tracks.chains.values_mut() {
        for plugin in chain
            .midi_fx_chain
            .iter_mut()
            .chain(chain.instrument.iter_mut())
            .chain(chain.fx_chain.iter_mut())
        {
            let _ = plugin.activate(f64::from(sr), 64, max_frames);
            let _ = plugin.start_processing();
        }
    }

    // Open the WAV writer.
    let spec = WavSpec {
        channels: 2,
        sample_rate: sr,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer = WavWriter::create(path, spec)
        .with_context(|| format!("failed to create WAV {}", path.display()))?;

    // Scratch buffers.
    let n = max_frames as usize;
    let mut master_l = vec![0.0f32; n];
    let mut master_r = vec![0.0f32; n];
    let mut track_l = vec![0.0f32; n];
    let mut track_r = vec![0.0f32; n];
    let mut midi_bus_a: Vec<TimedNoteEvent> = Vec::with_capacity(256);
    let mut midi_bus_b: Vec<TimedNoteEvent> = Vec::with_capacity(256);
    let mut active_notes: HashMap<u32, Vec<u8>> = HashMap::new();

    let mut playhead: u64 = 0;
    let mut silence_counter: u64 = 0;

    while playhead < total_max {
        let frames = max_frames.min((total_max - playhead) as u32);
        let buf_n = frames as usize;
        master_l[..buf_n].fill(0.0);
        master_r[..buf_n].fill(0.0);

        // Process each track.
        for (track_id, chain) in tracks.tracks.chains.iter_mut() {
            midi_bus_a.clear();
            let notes = active_notes.entry(*track_id).or_default();

            // Collect MIDI events for this buffer.
            if playhead < content_end {
                collect_events_for_buffer(
                    Some(song),
                    *track_id,
                    sr,
                    playhead,
                    frames,
                    &mut midi_bus_a,
                    notes,
                );
            }

            // MIDI FX chain.
            for mfx in &mut chain.midi_fx_chain {
                let _ = mfx.process(frames, &midi_bus_a, &[]);
                midi_bus_b.clear();
                mfx.drain_out_notes_into(&mut midi_bus_b);
                midi_bus_b.sort_by_key(|e| e.time);
                std::mem::swap(&mut midi_bus_a, &mut midi_bus_b);
            }

            // Instrument.
            track_l[..buf_n].fill(0.0);
            track_r[..buf_n].fill(0.0);
            if let Some(inst) = chain.instrument.as_mut() {
                let _ = inst.process(frames, &midi_bus_a, &[]);
                if let Some(l) = inst.output_buffer(0) {
                    track_l[..buf_n].copy_from_slice(&l[..buf_n]);
                }
                if let Some(r) = inst.output_buffer(1).or(inst.output_buffer(0)) {
                    track_r[..buf_n].copy_from_slice(&r[..buf_n]);
                }
            }

            // FX chain.
            for fx in &mut chain.fx_chain {
                let input = [&track_l[..buf_n], &track_r[..buf_n]];
                let _ = fx.process(frames, &[], &input);
                if let Some(l) = fx.output_buffer(0) {
                    track_l[..buf_n].copy_from_slice(&l[..buf_n]);
                }
                if let Some(r) = fx.output_buffer(1).or(fx.output_buffer(0)) {
                    track_r[..buf_n].copy_from_slice(&r[..buf_n]);
                }
            }

            // Apply volume/pan and sum into master.
            let params = tracks
                .tracks
                .params
                .get(track_id)
                .cloned()
                .unwrap_or_else(|| Arc::new(TrackAudioParams::new(1.0, 0.0, false, false)));
            let volume = params.volume();
            let pan = params.pan().clamp(-1.0, 1.0);
            let muted = params.muted();
            if !muted {
                let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
                let gain_l = angle.cos() * volume;
                let gain_r = angle.sin() * volume;
                for i in 0..buf_n {
                    master_l[i] += track_l[i] * gain_l;
                    master_r[i] += track_r[i] * gain_r;
                }
            }
        }

        // Vocal tracks (no chain).
        for (track_id, vocal_swap) in &tracks.tracks.vocal {
            if tracks.tracks.chains.contains_key(track_id) {
                continue; // already processed above
            }
            let vocal_guard = vocal_swap.load();
            if let Some(vocal) = vocal_guard.as_deref()
                && !vocal.samples.is_empty()
            {
                let params = tracks
                    .tracks
                    .params
                    .get(track_id)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(TrackAudioParams::new(1.0, 0.0, false, false)));
                let volume = params.volume();
                let pan = params.pan().clamp(-1.0, 1.0);
                if !params.muted() {
                    let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
                    let gain_l = angle.cos() * volume;
                    let gain_r = angle.sin() * volume;
                    for i in 0..buf_n {
                        let abs = playhead + i as u64;
                        if abs >= vocal.clip_start_samples {
                            let idx = (abs - vocal.clip_start_samples) as usize;
                            if idx < vocal.samples.len() {
                                let s = vocal.samples[idx];
                                master_l[i] += s * gain_l;
                                master_r[i] += s * gain_r;
                            }
                        }
                    }
                }
            }
        }

        // Write interleaved stereo samples.
        let mut peak: f32 = 0.0;
        for i in 0..buf_n {
            writer.write_sample(master_l[i])?;
            writer.write_sample(master_r[i])?;
            peak = peak.max(master_l[i].abs()).max(master_r[i].abs());
        }

        playhead += frames as u64;

        // Tail silence detection (only after content is done).
        if playhead > content_end {
            if peak < silence_thresh {
                silence_counter += frames as u64;
                if silence_counter >= silence_cutoff_samples {
                    tracing::info!(playhead, "tail silence detected, stopping export");
                    break;
                }
            } else {
                silence_counter = 0;
            }
        }
    }

    writer.finalize().context("failed to finalize WAV")?;

    // Stop processing + deactivate all plugins.
    for chain in tracks.tracks.chains.values_mut() {
        for plugin in chain
            .midi_fx_chain
            .iter_mut()
            .chain(chain.instrument.iter_mut())
            .chain(chain.fx_chain.iter_mut())
        {
            plugin.stop_processing();
            plugin.deactivate();
        }
    }

    // Restart the live audio thread.
    let _ = tracks.start_audio(session, playback_state, song_store, loop_state);

    tracing::info!(path = %path.display(), playhead, "WAV export finished");
    Ok(())
}
