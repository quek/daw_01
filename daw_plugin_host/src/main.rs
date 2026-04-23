mod clap_host;
mod plugin;

use std::path::{Path, PathBuf};
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

/// Events pushed from the plugin-main thread (or its CLAP callbacks) to the
/// IPC sender so they can be relayed to daw_gui as `ChildToMain`.
#[derive(Debug, Clone)]
pub enum PluginEvent {
    GuiOpened { width: u32, height: u32 },
    GuiRequestResize { width: u32, height: u32 },
    GuiClosed,
    PluginLoaded { id: String, name: String },
    PluginState(Option<Vec<u8>>),
}

impl From<PluginEvent> for ChildToMain {
    fn from(e: PluginEvent) -> Self {
        match e {
            PluginEvent::GuiOpened { width, height } => ChildToMain::GuiOpened { width, height },
            PluginEvent::GuiRequestResize { width, height } => {
                ChildToMain::GuiRequestResize { width, height }
            }
            PluginEvent::GuiClosed => ChildToMain::GuiClosed,
            PluginEvent::PluginLoaded { id, name } => ChildToMain::PluginLoaded { id, name },
            PluginEvent::PluginState(s) => ChildToMain::PluginState(s),
        }
    }
}

/// Commands processed serially on the plugin-main thread.
enum PluginCommand {
    SetClap {
        path: PathBuf,
        /// Stable CLAP id; empty string means "first descriptor in the file"
        /// (backward-compatible ad-hoc load).
        plugin_id: String,
        /// Optional initial state to restore right after activate via
        /// `clap_plugin_state.load`.
        initial_state: Option<Vec<u8>>,
    },
    LoadSong(Song),
    Play,
    Stop,
    SetLoop(bool),
    RequestState,
    OpenGuiEmbedded { host_hwnd: u64 },
    CloseGui,
    ResizeGui { width: u32, height: u32 },
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
    let mut audio_handle: Option<AudioHandle> = None;

    // Build the host-side GUI callbacks once; they capture a clone of the
    // outbound event sender so the plugin's call-backs translate directly
    // into `ChildToMain` messages to daw_gui.
    let make_callbacks = || HostCallbacks {
        on_request_resize: {
            let tx = evt_tx.clone();
            Arc::new(move |w, h| {
                let _ = tx.send(PluginEvent::GuiRequestResize {
                    width: w,
                    height: h,
                });
            })
        },
        on_closed: {
            let tx = evt_tx.clone();
            Arc::new(move || {
                let _ = tx.send(PluginEvent::GuiClosed);
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
                    if let Some(h) = audio_handle.take() {
                        h.shutdown();
                    }
                    return;
                }
            };
            match cmd {
                PluginCommand::Shutdown => {
                    if let Some(h) = audio_handle.take() {
                        h.shutdown();
                    }
                    tracing::info!("plugin-main thread exiting");
                    return;
                }
                PluginCommand::SetClap {
                    path,
                    plugin_id,
                    initial_state,
                } => swap_plugin(
                    &path,
                    &plugin_id,
                    initial_state.as_deref(),
                    &session,
                    Arc::clone(&playback_state),
                    Arc::clone(&song_store),
                    Arc::clone(&loop_state),
                    make_callbacks(),
                    &mut audio_handle,
                    &evt_tx,
                ),
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
                PluginCommand::RequestState => {
                    let state = match audio_handle.as_mut() {
                        Some(h) => match h.plugin_mut().state_save() {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!(error = ?e, "state_save failed");
                                None
                            }
                        },
                        None => None,
                    };
                    let _ = evt_tx.send(PluginEvent::PluginState(state));
                }
                PluginCommand::OpenGuiEmbedded { host_hwnd } => {
                    match open_gui(&mut audio_handle, host_hwnd) {
                        Ok(Some((w, h))) => {
                            let _ = evt_tx.send(PluginEvent::GuiOpened {
                                width: w,
                                height: h,
                            });
                        }
                        Ok(None) => {
                            tracing::warn!("plugin has no GUI or no plugin loaded; ignored");
                            // Tell daw_gui to reset the "GUI is open" UI state.
                            let _ = evt_tx.send(PluginEvent::GuiClosed);
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, "failed to open plugin GUI");
                            // Roll back any partial create() so the next
                            // attempt starts clean, and notify daw_gui.
                            close_gui(&mut audio_handle);
                            let _ = evt_tx.send(PluginEvent::GuiClosed);
                        }
                    }
                }
                PluginCommand::CloseGui => {
                    close_gui(&mut audio_handle);
                    // Always ack: daw_gui may be in the "open" state locally
                    // even when the plugin-host side never successfully
                    // created a GUI (e.g. after the ✕ button hid the
                    // container window, or after a failed show).
                    let _ = evt_tx.send(PluginEvent::GuiClosed);
                }
                PluginCommand::ResizeGui { width, height } => {
                    resize_gui(&mut audio_handle, width, height);
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

    if let Some(h) = audio_handle.take() {
        h.shutdown();
    }
    tracing::info!("plugin-main thread exiting (WM_QUIT)");
}

fn open_gui(
    audio_handle: &mut Option<AudioHandle>,
    host_hwnd: u64,
) -> Result<Option<(u32, u32)>> {
    let Some(h) = audio_handle else {
        return Ok(None);
    };
    let plugin = h.plugin_mut();
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

fn close_gui(audio_handle: &mut Option<AudioHandle>) {
    let Some(h) = audio_handle else { return };
    let plugin = h.plugin_mut();
    let _ = plugin.gui_hide();
    plugin.gui_destroy();
}

fn resize_gui(audio_handle: &mut Option<AudioHandle>, width: u32, height: u32) {
    let Some(h) = audio_handle else { return };
    let plugin = h.plugin_mut();
    if let Err(e) = plugin.gui_set_size(width, height) {
        tracing::warn!(error = ?e, width, height, "gui.set_size failed");
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
        MainToChild::SetClapPlugin {
            path,
            plugin_id,
            initial_state,
        } => {
            tracing::info!(
                path = %path.display(),
                id = %plugin_id,
                has_state = initial_state.is_some(),
                "received SetClapPlugin"
            );
            plugin.send(PluginCommand::SetClap {
                path,
                plugin_id,
                initial_state,
            });
        }
        MainToChild::RequestPluginState => {
            tracing::info!("received RequestPluginState");
            plugin.send(PluginCommand::RequestState);
        }
        MainToChild::SetLoop(on) => {
            tracing::info!(on, "received SetLoop");
            plugin.send(PluginCommand::SetLoop(on));
        }
        MainToChild::OpenGuiEmbedded { host_hwnd } => {
            tracing::info!(host_hwnd, "received OpenGuiEmbedded");
            plugin.send(PluginCommand::OpenGuiEmbedded { host_hwnd });
        }
        MainToChild::CloseGui => {
            tracing::info!("received CloseGui");
            plugin.send(PluginCommand::CloseGui);
        }
        MainToChild::ResizeGui { width, height } => {
            tracing::info!(width, height, "received ResizeGui");
            plugin.send(PluginCommand::ResizeGui { width, height });
        }
        other => {
            tracing::info!(?other, "received (no handler)");
        }
    }
}

// --- AudioHandle: main-thread-owned Plugin, raw ptr for audio thread -----

/// Wraps a raw `*mut Plugin` so it can be sent into the audio thread closure.
/// The CLAP spec partitions plugin state between main-thread and audio-thread,
/// so simultaneous main-thread GUI calls and audio-thread `process()` calls
/// touch disjoint fields (this assumes plugins conform to the spec).
struct PluginPtr(*mut Plugin);
unsafe impl Send for PluginPtr {}

/// RAII handle for the audio thread. `shutdown()` joins the thread and
/// deactivates the plugin on the main thread before it is dropped.
struct AudioHandle {
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    request_sem: Arc<Semaphore>,
    /// Plugin owned on the main (plugin-main) thread; a raw `*mut Plugin`
    /// is shared with the audio thread for `process()` only.
    plugin: Box<Plugin>,
}

impl AudioHandle {
    fn plugin_mut(&mut self) -> &mut Plugin {
        &mut self.plugin
    }

    fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.request_sem.release();

        if let Some(handle) = self.handle.take() {
            match handle.join() {
                Ok(()) => {}
                Err(_) => tracing::error!("audio thread panicked"),
            }
        }
        // Audio thread has exited; tear down plugin on this (main) thread.
        self.plugin.gui_destroy();
        self.plugin.deactivate();
        // drop(self.plugin) runs on this thread
    }
}

#[allow(clippy::too_many_arguments)]
fn load_and_spawn(
    path: &Path,
    plugin_id: &str,
    initial_state: Option<&[u8]>,
    session: &AudioSession,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
    loop_state: Arc<AtomicBool>,
    callbacks: HostCallbacks,
) -> Result<AudioHandle> {
    let mut plugin = Plugin::load(path, plugin_id, callbacks).context("failed to load plugin")?;
    plugin
        .activate(f64::from(session.sample_rate), 64, session.max_frames)
        .context("plugin.activate failed")?;
    // Apply any persisted state before processing starts.
    if let Some(bytes) = initial_state
        && let Err(e) = plugin.state_load(bytes)
    {
        tracing::error!(error = ?e, "failed to restore plugin state (continuing)");
    }
    spawn_audio_thread(Box::new(plugin), session, playback_state, song_store, loop_state)
}

fn spawn_audio_thread(
    mut plugin: Box<Plugin>,
    session: &AudioSession,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
    loop_state: Arc<AtomicBool>,
) -> Result<AudioHandle> {
    let bridge = Arc::new(
        AudioBridgeHandle::open(&session.shmem_id).context("failed to open audio shmem")?,
    );
    let request_sem = Arc::new(
        Semaphore::open(&session.request_sem_id).context("failed to open request semaphore")?,
    );
    let ready_sem = Arc::new(
        Semaphore::open(&session.ready_sem_id).context("failed to open ready semaphore")?,
    );
    let shutdown = Arc::new(AtomicBool::new(false));

    // Stable raw pointer to the heap allocation. Valid as long as `plugin`
    // Box (held in the returned AudioHandle) lives, which we guarantee by
    // joining the audio thread before dropping the AudioHandle.
    let plugin_ptr = PluginPtr(&mut *plugin as *mut Plugin);

    let th_bridge = Arc::clone(&bridge);
    let th_req = Arc::clone(&request_sem);
    let th_ready = Arc::clone(&ready_sem);
    let th_shutdown = Arc::clone(&shutdown);
    let th_sample_rate = session.sample_rate;

    let handle = std::thread::Builder::new()
        .name("clap-audio".into())
        .spawn(move || {
            run_audio(
                plugin_ptr,
                th_bridge,
                th_req,
                th_ready,
                th_shutdown,
                playback_state,
                song_store,
                loop_state,
                th_sample_rate,
            );
        })
        .context("failed to spawn audio thread")?;

    Ok(AudioHandle {
        handle: Some(handle),
        shutdown,
        request_sem,
        plugin,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_audio(
    plugin_ptr: PluginPtr,
    bridge: Arc<AudioBridgeHandle>,
    request_sem: Arc<Semaphore>,
    ready_sem: Arc<Semaphore>,
    shutdown: Arc<AtomicBool>,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
    loop_state: Arc<AtomicBool>,
    sample_rate: u32,
) {
    // SAFETY: the main thread guarantees the Plugin Box outlives this thread
    // via `AudioHandle::shutdown()`.
    let plugin: &mut Plugin = unsafe { &mut *plugin_ptr.0 };

    if let Err(e) = plugin.start_processing() {
        tracing::error!(error = ?e, "plugin.start_processing failed");
        return;
    }

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

        if let Err(e) = plugin.process(frames, &scheduled) {
            tracing::error!(error = ?e, "plugin.process failed");
            break;
        }
        scheduled.clear();

        let published = if playing { playhead } else { u64::MAX };
        bridge.set_playhead_samples(published);

        let n = frames as usize;
        let left = plugin.output_buffer(0);
        let right = plugin.output_buffer(1).or(left);
        unsafe {
            let dst = bridge.samples_ptr();
            match (left, right) {
                (Some(l), Some(r)) => {
                    for i in 0..n {
                        *dst.add(i * out_channels) = l[i];
                        *dst.add(i * out_channels + 1) = r[i];
                    }
                }
                _ => {
                    for i in 0..n * out_channels {
                        *dst.add(i) = 0.0;
                    }
                }
            }
        }

        if let Err(e) = ready_sem.release() {
            tracing::error!(error = ?e, "ready semaphore release failed");
            break;
        }
    }
    plugin.stop_processing();
    tracing::info!("audio thread exiting");
}

#[allow(clippy::too_many_arguments)]
fn swap_plugin(
    path: &Path,
    plugin_id: &str,
    initial_state: Option<&[u8]>,
    session: &AudioSession,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
    loop_state: Arc<AtomicBool>,
    callbacks: HostCallbacks,
    audio_handle: &mut Option<AudioHandle>,
    evt_tx: &tmpsc::UnboundedSender<PluginEvent>,
) {
    playback_state.store(PlaybackCommand::Stop as u8, Ordering::Release);

    if let Some(h) = audio_handle.take() {
        h.shutdown();
    }

    match load_and_spawn(
        path,
        plugin_id,
        initial_state,
        session,
        playback_state,
        song_store,
        loop_state,
        callbacks,
    ) {
        Ok(mut h) => {
            // Report the actually loaded plugin id + name so daw_gui can
            // bind the correct descriptor (vs the one requested).
            let loaded_id = h.plugin_mut().id().to_string();
            let loaded_name = h.plugin_mut().name().to_string();
            *audio_handle = Some(h);
            tracing::info!(path = %path.display(), id = %loaded_id, "plugin swapped in");
            let _ = evt_tx.send(PluginEvent::PluginLoaded {
                id: loaded_id,
                name: loaded_name,
            });
        }
        Err(e) => {
            tracing::error!(error = ?e, path = %path.display(), "failed to load plugin");
        }
    }
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
