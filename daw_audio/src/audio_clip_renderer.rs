//! Audio clip renderer skeleton (Phase 1 PR2).
//!
//! Defines the data structures the live audio thread will read every
//! buffer to mix audio events into per-track scratch buffers. The
//! actual mixing loop is implemented in PR6 (Raw + Repitch playback);
//! this PR only stands up the types and an empty default so
//! `EngineShared::audio_clip_renderer` has a wait-free snapshot to
//! `load()` from day one.
//!
//! Why a separate module from `vocal.rs`: the existing
//! [`crate::vocal::VocalAudio`] is mono-only and pre-rendered for a
//! single clip on a single track (VOICEVOX output). The audio clip
//! renderer is the generalised replacement — multi-channel, multi-
//! event, with stretch / pitch / fade per event. PR8 retires
//! `VocalAudio` and routes VOICEVOX through `AudioSourceBuffer +
//! AudioSourcePath::Generated` instead.
//!
//! Spec: `docs/plan_audio_clip.md` §6.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use common::model::{AudioSourceId, FadeCurve, StretchMode};

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
