//! Per-track scratch buffers used by the audio worker.
//!
//! The audio engine's worker pool reuses one `TrackScratch` per track every
//! buffer — track audio output and the MIDI ping-pong buses live here so
//! the RT loop never allocates. Cache-line aligned to keep concurrent
//! workers from false-sharing each other's scratch.

#![allow(dead_code)]

use crate::graph::DelayLine;
use crate::sequencer::{PerTrackState, TimedNoteEvent};

pub const MAX_FRAMES: usize = common::process_data::MAX_FRAMES;
pub const MAX_EVENTS: usize = common::process_data::MAX_EVENTS;

#[repr(align(64))]
pub struct TrackScratch {
    /// Per-track audio output (left). Reduced into the master bus after
    /// every worker finishes its dispatch.
    pub track_l: Vec<f32>,
    /// Per-track audio output (right).
    pub track_r: Vec<f32>,
    /// MIDI event ping-pong buffer A. Plugins consume from one and emit
    /// into the other; the worker swaps them between stages.
    pub midi_bus_a: Vec<TimedNoteEvent>,
    pub midi_bus_b: Vec<TimedNoteEvent>,
    /// Stuck-note tracking + queued offs for next buffer.
    pub state: PerTrackState,
    pub peak_l: f32,
    pub peak_r: f32,
    /// Set during dispatch: `muted || (any_solo && !solo)`. The reduce step
    /// reads this to skip the accumulation entirely for muted tracks.
    pub effective_mute: bool,
    /// PR4.5 sidechain plugin-internal alignment: per-track input delay
    /// applied between instrument output and the fx_chain. Capacity grown
    /// only at edit-time (engine schedule swap) so the RT path stays free
    /// of the allocator. Capacity 0 = no delay (most tracks); a track with
    /// fx_chain sidechain gets its line resized to `Schedule::
    /// input_delay_per_track[track_idx] + 1` (DelayLine spec requires
    /// capacity ≥ delay + 1).
    pub input_delay_line: DelayLine,
}

impl TrackScratch {
    pub fn new() -> Self {
        Self {
            track_l: vec![0.0; MAX_FRAMES],
            track_r: vec![0.0; MAX_FRAMES],
            midi_bus_a: Vec::with_capacity(MAX_EVENTS),
            midi_bus_b: Vec::with_capacity(MAX_EVENTS),
            state: PerTrackState::with_capacity(64),
            peak_l: 0.0,
            peak_r: 0.0,
            effective_mute: false,
            input_delay_line: DelayLine::with_capacity(0),
        }
    }
}

impl Default for TrackScratch {
    fn default() -> Self {
        Self::new()
    }
}
