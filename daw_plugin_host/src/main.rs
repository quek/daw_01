mod clap_host;
mod plugin;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use common::audio_bridge::{AudioBridgeHandle, CHANNELS};
use common::model::{NoteEvent, Song};
use common::protocol::{AudioSession, ChildKind, MainToChild};
use common::win_sem::Semaphore;
use common::wire::read_msg;
use tokio::net::windows::named_pipe::NamedPipeClient;

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

    let playback_state = Arc::new(AtomicU8::new(PlaybackCommand::Stop as u8));
    let song_store: Arc<ArcSwapOption<Song>> = Arc::new(ArcSwapOption::from(None));
    let mut audio_handle: Option<AudioHandle> = None;

    tracing::info!("awaiting shutdown");
    recv_loop(
        pipe,
        &session,
        Arc::clone(&playback_state),
        Arc::clone(&song_store),
        &mut audio_handle,
    )
    .await;
    tracing::info!("daw_plugin_host shutting down");

    if let Some(h) = audio_handle {
        h.shutdown();
    }

    tracing::info!("daw_plugin_host exiting");
    Ok(())
}

/// RAII handle for the audio thread. `shutdown()` joins the thread and
/// deactivates the plugin on the main thread before it is dropped.
struct AudioHandle {
    handle: Option<JoinHandle<Result<Plugin>>>,
    shutdown: Arc<AtomicBool>,
    request_sem: Arc<Semaphore>,
}

impl AudioHandle {
    fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Wake the thread if it is blocked on the request semaphore.
        let _ = self.request_sem.release();

        let Some(handle) = self.handle.take() else {
            return;
        };
        match handle.join() {
            Ok(Ok(mut plugin)) => {
                plugin.deactivate();
                // Drop here on the main thread triggers destroy on the main thread.
                drop(plugin);
            }
            Ok(Err(e)) => tracing::error!(error = ?e, "audio thread errored"),
            Err(_) => tracing::error!("audio thread panicked"),
        }
    }
}

fn load_and_spawn(
    path: &std::path::Path,
    session: &AudioSession,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
) -> Result<AudioHandle> {
    let mut plugin = Plugin::load(path).context("failed to load plugin")?;
    plugin
        .activate(
            f64::from(session.sample_rate),
            64,
            session.max_frames,
        )
        .context("plugin.activate failed")?;
    spawn_audio_thread(plugin, session, playback_state, song_store)
}

fn spawn_audio_thread(
    mut plugin: Plugin,
    session: &AudioSession,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
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

    plugin
        .start_processing()
        .context("plugin.start_processing failed")?;

    let th_bridge = Arc::clone(&bridge);
    let th_req = Arc::clone(&request_sem);
    let th_ready = Arc::clone(&ready_sem);
    let th_shutdown = Arc::clone(&shutdown);

    let th_sample_rate = session.sample_rate;
    let handle = std::thread::Builder::new()
        .name("clap-audio".into())
        .spawn(move || {
            run_audio(
                plugin,
                th_bridge,
                th_req,
                th_ready,
                th_shutdown,
                playback_state,
                song_store,
                th_sample_rate,
            )
        })
        .context("failed to spawn audio thread")?;

    Ok(AudioHandle {
        handle: Some(handle),
        shutdown,
        request_sem,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_audio(
    mut plugin: Plugin,
    bridge: Arc<AudioBridgeHandle>,
    request_sem: Arc<Semaphore>,
    ready_sem: Arc<Semaphore>,
    shutdown: Arc<AtomicBool>,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
    sample_rate: u32,
) -> Result<Plugin> {
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
            collect_events_for_buffer(
                song_store.load().as_deref(),
                sample_rate,
                playhead,
                frames,
                &mut scheduled,
                &mut active_notes,
            );
            playhead += frames as u64;
            if song_ended(song_store.load().as_deref(), sample_rate, playhead) {
                // Auto-stop and emit remaining note-offs at buffer end.
                playing = false;
                for &key in &active_notes {
                    scheduled.push(TimedNoteEvent {
                        time: frames.saturating_sub(1),
                        event: NoteTransition::Off { key },
                    });
                }
                active_notes.clear();
                playback_state.store(PlaybackCommand::Stop as u8, Ordering::Release);
                tracing::info!("playback reached end of clip, auto-stopping");
            }
        }

        if let Err(e) = plugin.process(frames, &scheduled) {
            tracing::error!(error = ?e, "plugin.process failed");
            break;
        }
        scheduled.clear();

        // Copy planar plugin output (take first 2 channels) to interleaved shmem.
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
                    // Plugin has no output channels — silence.
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
    Ok(plugin)
}

async fn recv_loop(
    mut pipe: NamedPipeClient,
    session: &AudioSession,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
    audio_handle: &mut Option<AudioHandle>,
) {
    loop {
        match read_msg::<_, MainToChild>(&mut pipe).await {
            Ok(MainToChild::Play) => {
                tracing::info!("received Play");
                playback_state.store(PlaybackCommand::Play as u8, Ordering::Release);
            }
            Ok(MainToChild::Stop) => {
                tracing::info!("received Stop");
                playback_state.store(PlaybackCommand::Stop as u8, Ordering::Release);
            }
            Ok(MainToChild::LoadSong(song)) => {
                tracing::info!(
                    bpm = song.bpm,
                    tracks = song.tracks.len(),
                    "received LoadSong"
                );
                song_store.store(Some(Arc::new(song)));
            }
            Ok(MainToChild::SetClapPlugin(path)) => {
                tracing::info!(path = %path.display(), "received SetClapPlugin");
                swap_plugin(
                    &path,
                    session,
                    Arc::clone(&playback_state),
                    Arc::clone(&song_store),
                    audio_handle,
                );
            }
            Ok(msg) => {
                tracing::info!(?msg, "received (no handler)");
            }
            Err(e) => {
                tracing::info!(error = ?e, "pipe ended");
                break;
            }
        }
    }
}

fn swap_plugin(
    path: &Path,
    session: &AudioSession,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
    audio_handle: &mut Option<AudioHandle>,
) {
    // Bring playback to Stop so the new plugin starts from a clean state.
    playback_state.store(PlaybackCommand::Stop as u8, Ordering::Release);

    // Stop and clean up the existing plugin on the main thread.
    if let Some(h) = audio_handle.take() {
        h.shutdown();
    }

    match load_and_spawn(path, session, playback_state, song_store) {
        Ok(h) => {
            *audio_handle = Some(h);
            tracing::info!(path = %path.display(), "plugin swapped in");
        }
        Err(e) => {
            tracing::error!(error = ?e, path = %path.display(), "failed to load plugin");
        }
    }
}

/// Pushes every row event that falls within `[playhead, playhead + frames)`
/// into `out`, converted to `TimedNoteEvent` with `time` as the buffer offset.
/// Updates `active_notes` to track currently sounding keys.
///
/// MVP: only the first clip of the first track is read.
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
                // Monophonic retrigger: cut any previously sounding notes before
                // starting the new one. (Matches typical tracker semantics.)
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

fn song_ended(song: Option<&Song>, sample_rate: u32, playhead: u64) -> bool {
    let Some(song) = song else { return false };
    let Some(track) = song.tracks.first() else {
        return false;
    };
    let Some(clip) = track.clips.first() else {
        return false;
    };
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let clip_start_samples = (clip.start_beat * samples_per_beat).max(0.0) as u64;
    let clip_length_samples = (clip.length_beats * samples_per_beat) as u64;
    playhead >= clip_start_samples + clip_length_samples
}
