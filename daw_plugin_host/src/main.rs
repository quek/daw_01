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
    let loop_state = Arc::new(AtomicBool::new(false));
    let mut audio_handle: Option<AudioHandle> = None;

    tracing::info!("awaiting shutdown");
    recv_loop(
        pipe,
        &session,
        Arc::clone(&playback_state),
        Arc::clone(&song_store),
        Arc::clone(&loop_state),
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
    loop_state: Arc<AtomicBool>,
) -> Result<AudioHandle> {
    let mut plugin = Plugin::load(path).context("failed to load plugin")?;
    plugin
        .activate(
            f64::from(session.sample_rate),
            64,
            session.max_frames,
        )
        .context("plugin.activate failed")?;
    spawn_audio_thread(plugin, session, playback_state, song_store, loop_state)
}

fn spawn_audio_thread(
    mut plugin: Plugin,
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
                loop_state,
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
    loop_state: Arc<AtomicBool>,
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
                // Buffer reached end-of-clip. Emit note-offs for sounding keys
                // at the end of this buffer, then either wrap playhead back to
                // the clip start (loop) or stop playback.
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

#[allow(clippy::too_many_arguments)]
async fn recv_loop(
    mut pipe: NamedPipeClient,
    session: &AudioSession,
    playback_state: Arc<AtomicU8>,
    song_store: Arc<ArcSwapOption<Song>>,
    loop_state: Arc<AtomicBool>,
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
                    Arc::clone(&loop_state),
                    audio_handle,
                );
            }
            Ok(MainToChild::SetLoop(on)) => {
                tracing::info!(on, "received SetLoop");
                loop_state.store(on, Ordering::Release);
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
    loop_state: Arc<AtomicBool>,
    audio_handle: &mut Option<AudioHandle>,
) {
    // Bring playback to Stop so the new plugin starts from a clean state.
    playback_state.store(PlaybackCommand::Stop as u8, Ordering::Release);

    // Stop and clean up the existing plugin on the main thread.
    if let Some(h) = audio_handle.take() {
        h.shutdown();
    }

    match load_and_spawn(path, session, playback_state, song_store, loop_state) {
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

/// Returns `(clip_start_samples, clip_end_samples)` for the MVP playback
/// target (track 0 / clip 0), or `None` if there is nothing playable:
/// no song, empty track list, missing clip, `bpm <= 0`, or zero-length clip.
/// The `None` case means "do not loop, do not play": downstream code should
/// treat it as end-of-song.
fn clip_bounds_samples(song: Option<&Song>, sample_rate: u32) -> Option<(u64, u64)> {
    let song = song?;
    let track = song.tracks.first()?;
    let clip = track.clips.first()?;
    if song.bpm <= 0.0 || clip.length_beats <= 0.0 {
        return None;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let start = (clip.start_beat * samples_per_beat).max(0.0) as u64;
    let length = (clip.length_beats * samples_per_beat) as u64;
    if length == 0 {
        return None;
    }
    Some((start, start + length))
}

fn song_ended(song: Option<&Song>, sample_rate: u32, playhead: u64) -> bool {
    match clip_bounds_samples(song, sample_rate) {
        Some((_, end)) => playhead >= end,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{Clip, InstrumentSource, Track};

    fn song_with_clip(bpm: f32, start_beat: f64, length_beats: f64) -> Song {
        Song {
            bpm,
            tracks: vec![Track {
                name: "T".into(),
                source: InstrumentSource::BuiltinSynth,
                fx_chain: vec![],
                volume: 1.0,
                pan: 0.0,
                clips: vec![Clip {
                    name: "C".into(),
                    start_beat,
                    length_beats,
                    rows_per_beat: 4,
                    rows: vec![],
                }],
            }],
            ..Song::default()
        }
    }

    #[test]
    fn clip_bounds_none_for_no_song() {
        assert_eq!(clip_bounds_samples(None, 48000), None);
    }

    #[test]
    fn clip_bounds_none_for_empty_tracks() {
        let song = Song::default();
        assert_eq!(clip_bounds_samples(Some(&song), 48000), None);
    }

    #[test]
    fn clip_bounds_none_for_zero_bpm() {
        let song = song_with_clip(0.0, 0.0, 4.0);
        assert_eq!(clip_bounds_samples(Some(&song), 48000), None);
    }

    #[test]
    fn clip_bounds_none_for_zero_length_clip() {
        let song = song_with_clip(120.0, 0.0, 0.0);
        assert_eq!(clip_bounds_samples(Some(&song), 48000), None);
    }

    #[test]
    fn clip_bounds_standard_clip() {
        // 120 BPM, 48 kHz: 1 beat = 24000 samples; 4 beats = 96000 samples.
        let song = song_with_clip(120.0, 0.0, 4.0);
        assert_eq!(
            clip_bounds_samples(Some(&song), 48000),
            Some((0, 96_000))
        );
    }

    #[test]
    fn clip_bounds_with_offset() {
        // Start at beat 2 → 48000 samples in; length 4 beats → end at 144000.
        let song = song_with_clip(120.0, 2.0, 4.0);
        assert_eq!(
            clip_bounds_samples(Some(&song), 48000),
            Some((48_000, 144_000))
        );
    }

    #[test]
    fn song_ended_never_triggers_with_no_song() {
        assert!(!song_ended(None, 48000, 0));
        assert!(!song_ended(None, 48000, u64::MAX));
    }

    #[test]
    fn song_ended_after_clip_end() {
        let song = song_with_clip(120.0, 0.0, 4.0);
        assert!(!song_ended(Some(&song), 48000, 95_999));
        assert!(song_ended(Some(&song), 48000, 96_000));
        assert!(song_ended(Some(&song), 48000, 96_001));
    }

    #[test]
    fn song_ended_treats_zero_length_as_not_ended() {
        // With no valid bounds we can't "end"; recv_loop should simply not
        // enter playback mode, but the audio thread must not flip to stop.
        let song = song_with_clip(120.0, 0.0, 0.0);
        assert!(!song_ended(Some(&song), 48000, 1_000_000));
    }
}
