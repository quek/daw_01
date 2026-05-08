//! Audio clip renderer.
//!
//! Defines the data structures the live audio thread reads every buffer
//! to mix audio events into per-track scratch buffers. PR2 stood up
//! the types + an empty default so `EngineShared::audio_clip_renderer`
//! has a wait-free snapshot to `load()` from day one; PR6 added the
//! schedule compiler + Raw / Repitch render loop on top.
//!
//! VOICEVOX vocal output (the old `vocal.rs` / `VocalAudio`) shares the
//! same `AudioSourceBuffer` shape — PR8 routed it through
//! `MainToChild::SetGeneratedAudio` →
//! `EngineShared::generated_audio_store`, keyed by
//! `vocal_gen_id(track_id, clip_id)`. The actual per-clip vocal mix
//! still lives in `engine::process_track_owned`'s vocal block (Vocal
//! clips are MIDI-shaped with lyrics, so they don't appear in
//! `AudioContent` and thus aren't picked up by `compile_audio_schedule`
//! / `render_audio_events`); a future PR can migrate them onto
//! `AudioContent::Audio` once the model can express
//! "MIDI-with-baked-audio" cleanly.
//!
//! Spec: `docs/plan_audio_clip.md` §6 / §9.3.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use common::audio_render::{fade_envelope, pitch_ratio_for};
use common::model::{
    AudioSourceId, AudioSourcePath, ClipContent, FadeCurve, Song, StretchMode,
};

/// Decoded sample buffer for a single `AudioSource`. Planar storage
/// (`samples[channel][frame_idx]`). Shared via `Arc` between the IPC
/// receive loop, the decode worker, and the audio render thread —
/// the audio thread only ever clones the `Arc`, never the bytes.
pub struct AudioSourceBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub samples: Vec<Vec<f32>>,
}

impl AudioSourceBuffer {
    /// Empty silent buffer — used as a placeholder when the source is
    /// missing or still decoding. Allocates `frames` zeros per channel.
    pub fn silent(sample_rate: u32, channels: u16, frames: u64) -> Self {
        let ch = channels.max(1) as usize;
        Self {
            sample_rate,
            channels,
            frames,
            samples: (0..ch).map(|_| vec![0.0; frames as usize]).collect(),
        }
    }
}

/// One playable event flattened from an `AudioEvent` for the render
/// loop. Times are in absolute song frames (= playhead units), so the
/// render loop can `binary_search` by `start_frame` without re-walking
/// the song graph each buffer.
pub struct RenderedEvent {
    pub track_idx: usize,
    pub clip_idx: usize,
    /// First song-frame this event contributes audio at.
    pub start_frame: u64,
    /// Exclusive end song-frame.
    pub end_frame: u64,
    pub source_id: AudioSourceId,
    pub source_start_frames: u64,
    pub source_end_frames: u64,
    pub gain_lin: f32,
    pub pan: f32,
    /// Source frame stride per output frame (Repitch / sample-rate
    /// conversion). 1.0 = same speed as engine SR.
    pub pitch_ratio: f64,
    pub fade_in_frames: u64,
    pub fade_out_frames: u64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
    pub reversed: bool,
    pub stretch_mode: StretchMode,
}

/// Wait-free snapshot of "what audio events should the audio thread
/// mix on the next buffer." Built off the audio thread (in
/// `compile_audio_schedule` — PR6) and published via `ArcSwap`. The
/// audio thread `load()`s a snapshot and reads it for the duration of
/// one buffer; new edits land via `store()` on the next callback.
pub struct AudioClipRenderer {
    /// Sorted by `start_frame` ascending. PR6's render loop bisects
    /// here to find events overlapping the current buffer.
    pub schedule: Vec<RenderedEvent>,
    /// `AudioSourceId → decoded buffer`. The render loop clones the
    /// `Arc` once per active event — no hashmap lookup beyond that.
    pub sources: HashMap<AudioSourceId, Arc<AudioSourceBuffer>>,
}

impl AudioClipRenderer {
    pub fn empty() -> Self {
        Self {
            schedule: Vec::new(),
            sources: HashMap::new(),
        }
    }
}

impl Default for AudioClipRenderer {
    fn default() -> Self {
        Self::empty()
    }
}

// ---------------------------------------------------------------------------
// Phase 1 PR6: schedule compilation + WAV decode + render loop
// ---------------------------------------------------------------------------

/// Decode a WAV file into a planar `AudioSourceBuffer`. Mirror of
/// `daw_gui::import_audio::decode_wav` (Phase 1 keeps the two crates'
/// decoders independent so file-backed sources are decoded twice — once
/// per process — without IPC for sample data, see §6.1 / §8.3).
pub fn decode_wav(path: &Path) -> Result<AudioSourceBuffer> {
    use hound::SampleFormat;

    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("open wav {}", path.display()))?;
    let spec = reader.spec();
    if spec.channels == 0 {
        anyhow::bail!("{}: channels = 0", path.display());
    }
    if spec.sample_rate == 0 {
        anyhow::bail!("{}: sample_rate = 0", path.display());
    }
    let frames = reader.duration() as u64;
    let channels = spec.channels as usize;

    let mut planar: Vec<Vec<f32>> = (0..channels)
        .map(|_| Vec::with_capacity(frames as usize))
        .collect();
    match spec.sample_format {
        SampleFormat::Float => {
            for (idx, sample) in reader.samples::<f32>().enumerate() {
                let s = sample.with_context(|| format!("read f32 {}", path.display()))?;
                planar[idx % channels].push(s);
            }
        }
        SampleFormat::Int => {
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            for (idx, sample) in reader.samples::<i32>().enumerate() {
                let s = sample.with_context(|| format!("read i32 {}", path.display()))?;
                planar[idx % channels].push(s as f32 / max_val);
            }
        }
    }
    Ok(AudioSourceBuffer {
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        frames,
        samples: planar,
    })
}

/// Build an `AudioClipRenderer` snapshot from the current Song. Walks
/// `Song.audio_sources` (decoding file-backed entries via `hound` and
/// looking up generated entries in `generated_audio_store`), then
/// flattens every `ClipContent::Audio` event in every track into the
/// schedule. Sorted by `start_frame` ascending so the render loop can
/// short-circuit once `start_frame >= buf_end`.
///
/// Decode is synchronous on the caller (the IPC receive loop in Phase 1).
/// Phase 2 moves decode to a background thread so >100 MB samples don't
/// stall the receive loop.
pub fn compile_audio_schedule(
    song: &Song,
    project_dir: Option<&Path>,
    engine_sample_rate: u32,
    generated_audio_store: &HashMap<u64, Arc<AudioSourceBuffer>>,
) -> AudioClipRenderer {
    let mut sources: HashMap<AudioSourceId, Arc<AudioSourceBuffer>> = HashMap::new();
    if engine_sample_rate == 0 || song.bpm <= 0.0 {
        return AudioClipRenderer::empty();
    }
    let samples_per_beat = engine_sample_rate as f64 * 60.0 / song.bpm as f64;

    // -- Resolve every AudioSource into a decoded buffer (or skip on error) --
    for (&id, source) in &song.audio_sources {
        let buffer = match &source.path {
            AudioSourcePath::ProjectRelative(rel) => {
                let Some(dir) = project_dir else {
                    tracing::warn!(?rel, "ProjectRelative source but project_dir is unset; skipping");
                    continue;
                };
                let abs = dir.join(rel);
                match decode_wav(&abs) {
                    Ok(buf) => Arc::new(buf),
                    Err(e) => {
                        tracing::error!(error = ?e, path = %abs.display(), "decode failed");
                        continue;
                    }
                }
            }
            AudioSourcePath::Absolute(abs) => match decode_wav(abs) {
                Ok(buf) => Arc::new(buf),
                Err(e) => {
                    tracing::error!(error = ?e, path = %abs.display(), "decode failed");
                    continue;
                }
            },
            AudioSourcePath::Generated { id: gen_id } => {
                let Some(buf) = generated_audio_store.get(gen_id) else {
                    tracing::warn!(gen_id, "Generated source not yet delivered via SetGeneratedAudio; skipping");
                    continue;
                };
                Arc::clone(buf)
            }
        };
        sources.insert(id, buffer);
    }

    // -- Flatten every audio clip's events into RenderedEvent ----------------
    let mut schedule: Vec<RenderedEvent> = Vec::new();
    for (track_idx, track) in song.tracks.iter().enumerate() {
        for (clip_idx, clip) in track.clips.iter().enumerate() {
            let Some(content) = song.clip_contents.get(&clip.content_id) else {
                continue;
            };
            let ClipContent::Audio(audio) = content else {
                continue;
            };
            for event in &audio.events {
                let Some(buffer) = sources.get(&event.source_id) else {
                    continue;
                };
                let event_start_beat = clip.start_beat + event.event_start_in_clip_beats;
                let event_end_beat = event_start_beat + event.event_length_beats;
                let start_frame = (event_start_beat * samples_per_beat).max(0.0) as u64;
                let end_frame = (event_end_beat * samples_per_beat).max(0.0) as u64;
                if end_frame <= start_frame {
                    continue;
                }
                let pitch_ratio = pitch_ratio_for(
                    event.stretch_mode,
                    buffer.sample_rate,
                    engine_sample_rate,
                    event.pitch_semitones,
                );
                let gain_lin = 10f32.powf(event.gain_db / 20.0);
                let fade_in_frames =
                    (event.fade_in_beats.max(0.0) * samples_per_beat).max(0.0) as u64;
                let fade_out_frames =
                    (event.fade_out_beats.max(0.0) * samples_per_beat).max(0.0) as u64;
                if event.muted {
                    continue;
                }
                schedule.push(RenderedEvent {
                    track_idx,
                    clip_idx,
                    start_frame,
                    end_frame,
                    source_id: event.source_id,
                    source_start_frames: event.source_start_frames,
                    source_end_frames: event.source_end_frames,
                    gain_lin,
                    pan: event.pan.clamp(-1.0, 1.0),
                    pitch_ratio,
                    fade_in_frames,
                    fade_out_frames,
                    fade_in_curve: event.fade_in_curve,
                    fade_out_curve: event.fade_out_curve,
                    reversed: event.reversed,
                    stretch_mode: event.stretch_mode,
                });
            }
        }
    }
    schedule.sort_by_key(|e| e.start_frame);
    tracing::info!(
        n_events = schedule.len(),
        n_sources = sources.len(),
        engine_sr = engine_sample_rate,
        bpm = song.bpm,
        "compiled audio schedule"
    );
    AudioClipRenderer { schedule, sources }
}

/// Mix every audio event for `track_idx` into `track_l/track_r` for the
/// frame range `[playhead .. playhead+frames)`. Called from
/// `process_track_owned` after the track buffers are zeroed and before
/// the audio FX chain. Adds (`+=`) to the existing buffer so the
/// instrument plugin's audio output is preserved (= Bitwig Hybrid Track:
/// audio clip output bypasses the instrument and joins the FX chain
/// input alongside it, see §13 Q6).
pub fn render_audio_events(
    renderer: &AudioClipRenderer,
    track_idx: usize,
    track_l: &mut [f32],
    track_r: &mut [f32],
    playhead: u64,
    frames: u32,
) {
    if frames == 0 {
        return;
    }
    let n = frames as usize;
    let buf_start = playhead;
    let buf_end = playhead + frames as u64;

    for event in &renderer.schedule {
        if event.track_idx != track_idx {
            continue;
        }
        // schedule is sorted by start_frame, so once start_frame >=
        // buf_end no later event can overlap. We could binary_search
        // but a linear early-out is fine for typical clip counts.
        if event.start_frame >= buf_end {
            break;
        }
        if event.end_frame <= buf_start {
            continue;
        }
        let Some(buffer) = renderer.sources.get(&event.source_id) else {
            continue;
        };
        if buffer.samples.is_empty() {
            continue;
        }

        let render_start = event.start_frame.max(buf_start);
        let render_end = event.end_frame.min(buf_end);
        if render_end <= render_start {
            continue;
        }
        let buf_off_start = (render_start - buf_start) as usize;
        let buf_off_end = (render_end - buf_start) as usize;
        if buf_off_end > n {
            continue;
        }

        let event_total_frames = event.end_frame - event.start_frame;
        let source_len = event
            .source_end_frames
            .saturating_sub(event.source_start_frames);

        // Channel layout: planar samples[ch][frame].
        let l_plane: &[f32] = buffer
            .samples
            .first()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let r_plane: &[f32] = if buffer.channels >= 2 {
            buffer
                .samples
                .get(1)
                .map(Vec::as_slice)
                .unwrap_or(l_plane)
        } else {
            l_plane
        };

        // Equal-power pan for mono → stereo / balance pan for stereo.
        // pan = 0 → no change; pan > 0 → right; pan < 0 → left.
        let pan_rad = (event.pan + 1.0) * std::f32::consts::FRAC_PI_4;
        let pan_l = pan_rad.cos();
        let pan_r = pan_rad.sin();

        for i in buf_off_start..buf_off_end {
            let abs_song_frame = buf_start + i as u64;
            let event_local = abs_song_frame - event.start_frame;

            // Fade envelope (in × out)
            let fade_in = fade_envelope(event_local, event.fade_in_frames, event.fade_in_curve);
            let tail = event_total_frames.saturating_sub(event_local + 1);
            let fade_out = fade_envelope(tail, event.fade_out_frames, event.fade_out_curve);
            let env = fade_in * fade_out * event.gain_lin;
            if env == 0.0 {
                continue;
            }

            // Source position with linear interpolation (Repitch / SR mismatch)
            let source_pos = event_local as f64 * event.pitch_ratio;
            let source_pos = if event.reversed {
                source_len as f64 - 1.0 - source_pos
            } else {
                source_pos
            };
            if source_pos < 0.0 {
                continue;
            }
            let i0 = source_pos.floor() as i64;
            let frac = (source_pos - i0 as f64) as f32;
            if i0 < 0 {
                continue;
            }
            let abs_idx0 = event.source_start_frames + i0 as u64;
            let abs_idx1 = abs_idx0 + 1;
            if abs_idx0 >= event.source_end_frames || abs_idx0 >= buffer.frames {
                continue;
            }
            let s_l0 = l_plane.get(abs_idx0 as usize).copied().unwrap_or(0.0);
            let s_r0 = r_plane.get(abs_idx0 as usize).copied().unwrap_or(0.0);
            let s_l1 = l_plane.get(abs_idx1 as usize).copied().unwrap_or(s_l0);
            let s_r1 = r_plane.get(abs_idx1 as usize).copied().unwrap_or(s_r0);
            let s_l = s_l0 + (s_l1 - s_l0) * frac;
            let s_r = s_r0 + (s_r1 - s_r0) * frac;

            track_l[i] += s_l * env * pan_l * std::f32::consts::SQRT_2;
            track_r[i] += s_r * env * pan_r * std::f32::consts::SQRT_2;
        }
    }
}
