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

/// Pre-allocated capacity (samples per channel) for each track's input delay
/// line, so PDC / sidechain alignment never reallocates on the audio thread
/// (D1 / PR3). 48000 = 1 s at 48 kHz — comfortably above any real plugin's
/// reported latency.
const INPUT_DELAY_PREALLOC_SAMPLES: usize = 48_000;

/// E5 (r.md #8): 1 track が同時に持てる granular grain-lock-in ring の数 (= track 内
/// audio event の最大 index)。 これを超える index の event は lock 無し (= 従来の LP
/// smoothing 挙動) に degrade する。 1 track に数百 clip は実用上稀なので 256 で足りる。
const MAX_GRANULAR_EVENTS_PER_TRACK: usize = 256;

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
    /// E5 (r.md #8): track 内 event ごとの granular grain-lock-in ring (添字 = event の
    /// schedule 順 index)。 `render_audio_events` が Stretch mode の grain offset を trigger 時に
    /// 固定するのに使い、 tempo 変化での source position 跳び (= click) を防ぐ。 起動時に
    /// `MAX_GRANULAR_EVENTS_PER_TRACK` ぶん pre-alloc し RT で再確保しない。
    pub granular_rings: Vec<crate::audio_clip_renderer::GrainLockRing>,
    /// E5 sibling (r.md #8): Repitch (tape) mode の **連続 source 位置 accumulator** (event 単位、
    /// 添字 = granular_rings と同じ event index)。 `(last_event_local, accumulated_source_pos)`。
    /// Repitch は `event_local × ratio` で絶対位置を毎 buffer 再計算していたため tempo automation
    /// で ratio が変わると位置が跳んで click した (jump 量は event_local に比例 = granular より
    /// 重症)。 contiguous 再生では ratio を積分 (= 連続)、 seek/schedule 変化 (event_local 不連続)
    /// では再 anchor して click を防ぐ。 `u64::MAX` = 未初期化。
    pub repitch_accum: Vec<(u64, f64)>,
    /// Per-sample volume gain ramp for the buffer about to be processed.
    /// `MAX_FRAMES` long, allocated once at construction and overwritten
    /// in place every buffer by `fill_track_param_ramps`. The fx-chain
    /// post-process loop reads this to apply sample-accurate volume
    /// automation. When the track has no `Volume` lane (or the lane is
    /// disabled), the buffer is filled with the constant
    /// `track.volume`.
    pub volume_per_sample: Vec<f32>,
    /// Per-sample pan ramp, same lifecycle as `volume_per_sample`.
    /// Range `-1.0..=1.0` (left..right). Default constant fill is
    /// `track.pan`.
    pub pan_per_sample: Vec<f32>,
    /// Post-fx, **pre-fader** snapshot of this track's signal (taken
    /// before the volume / pan strip overwrites `track_l/r` in place).
    /// Written by `process_track_owned` / `run_group_fx_chain` only when
    /// the track has a pre-fader aux send, and read by a `MixSend` whose
    /// send `mode == PreFader`. `MAX_FRAMES` long, allocated once.
    pub pre_fader_l: Vec<f32>,
    pub pre_fader_r: Vec<f32>,
    /// **Pre-FX** snapshot of this track's signal (the raw audio clip /
    /// input *before* the device chain runs). Written by
    /// `process_track_owned` / `run_group_fx_chain` only when a
    /// `TapPoint::PreFx` tap / mod source reads this track
    /// (`track_needs_prefx_snapshot`), and read by a `SidechainTap` /
    /// `EnvelopeFollow` resolving `BufRef::PreFxScratch`. `MAX_FRAMES` long,
    /// allocated once. docs/plan_modulation_followups.md §1.
    pub pre_fx_l: Vec<f32>,
    pub pre_fx_r: Vec<f32>,
}

impl TrackScratch {
    pub fn new() -> Self {
        Self {
            track_l: vec![0.0; MAX_FRAMES],
            track_r: vec![0.0; MAX_FRAMES],
            midi_bus_a: Vec::with_capacity(MAX_EVENTS),
            midi_bus_b: Vec::with_capacity(MAX_EVENTS),
            // capacity は sequencer の `ACTIVE_NOTES_CAP` (= `MAX_EVENTS`) と
            // 一致させる。 push 前 clamp が `MAX_EVENTS` で効くので、 ここを
            // それ未満にすると clamp が防げない区間で RT realloc が起きる。
            state: PerTrackState::with_capacity(MAX_EVENTS),
            peak_l: 0.0,
            peak_r: 0.0,
            effective_mute: false,
            // D1 / PR3: pre-allocate the sidechain/PDC input delay ring so the
            // `refresh_schedule` alignment step never reallocates on the audio
            // thread. `INPUT_DELAY_PREALLOC_SAMPLES` (1 s @ 48 kHz) is above any
            // real plugin's reported latency; the refresh path still grows it
            // for the pathological >1 s case (which no real plugin hits).
            input_delay_line: DelayLine::with_capacity(INPUT_DELAY_PREALLOC_SAMPLES),
            granular_rings: vec![[(u64::MAX, 0); 8]; MAX_GRANULAR_EVENTS_PER_TRACK],
            repitch_accum: vec![(u64::MAX, 0.0); MAX_GRANULAR_EVENTS_PER_TRACK],
            volume_per_sample: vec![1.0; MAX_FRAMES],
            pan_per_sample: vec![0.0; MAX_FRAMES],
            pre_fader_l: vec![0.0; MAX_FRAMES],
            pre_fader_r: vec![0.0; MAX_FRAMES],
            pre_fx_l: vec![0.0; MAX_FRAMES],
            pre_fx_r: vec![0.0; MAX_FRAMES],
        }
    }
}

impl Default for TrackScratch {
    fn default() -> Self {
        Self::new()
    }
}
