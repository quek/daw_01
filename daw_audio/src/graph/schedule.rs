//! Schedule + NodeOp + BufRef.
//!
//! A `Schedule` is the compiled execution plan that the audio thread
//! steps through every buffer. It owns its delay-line ring buffers and
//! its port-buffer pool so the RT path never allocates.

#![allow(dead_code)]

use super::delay_line::DelayLine;
use super::port_buffer::PortBufferPool;

/// Reference to a stereo audio buffer.
///
/// `BufRef` is the only way `NodeOp` describes inputs and outputs; the
/// schedule executor resolves it to a concrete buffer (per-track scratch,
/// the master bus, the port pool, or a plugin's aux output port).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufRef {
    /// A track's post-fader scratch buffer (`mixer::TrackScratch`).
    /// Indexed by song-track index. PR1 only uses these.
    TrackScratch(u32),
    /// The master bus output.
    Master,
    /// A buffer drawn from `Schedule::port_buffers`. Used (PR2 onwards)
    /// for group-bus inputs that don't map to any track's own scratch.
    Pooled(u32),
    /// A plugin's aux output port (PR4 parallel out).
    PluginAuxOut { plugin_id: u32, port: u8 },
    /// A track's **pre-fader** scratch (after its fx chain, before the
    /// volume / pan strip). Written by `ProcessTrack` / `ProcessGroupFx`
    /// and read by a `MixSend` whose send `mode == PreFader`. Indexed by
    /// song-track index, parallel to `TrackScratch`.
    PreFaderScratch(u32),
    /// A track's **pre-FX** scratch (the raw signal *before* its device
    /// chain — audio clips / sidechain-aligned input, with no FX applied).
    /// Captured by `ProcessTrack` / `ProcessGroupFx` just before the device
    /// loop runs, and read by a `SidechainTap` / `EnvelopeFollow` whose tap
    /// point is `TapPoint::PreFx`. Indexed by song-track index, parallel to
    /// `TrackScratch`. docs/plan_modulation_followups.md §1.
    PreFxScratch(u32),
}

/// A unit of work in a `Schedule`. The RT thread iterates `Schedule::nodes`
/// in order and dispatches each variant; the audio worker pool fans
/// `ProcessTrack` / `ProcessGroupFx` ops out across cores.
#[derive(Debug, Clone)]
pub enum NodeOp {
    /// Run the full per-track pipeline (sequencer → MIDI FX →
    /// instrument / vocal → audio FX → strip). Output lands in the
    /// track's scratch (`BufRef::TrackScratch(track_idx)`).
    ProcessTrack { track_idx: u32 },

    /// PR2: process a `kind == Group` track's audio FX chain on its
    /// already-summed input scratch.
    ProcessGroupFx { track_idx: u32 },

    /// Mix `srcs` into `dst` with per-source linear gain. PR1 emits a
    /// single `Mix { dst: Master, ... }` at the end; PR2 also emits
    /// `Mix { dst: TrackScratch(group_idx), ... }` for group inputs.
    Mix {
        srcs: Vec<(BufRef, f32)>,
        dst: BufRef,
    },

    /// PR3: apply a PDC delay line to `buf` in place using `delay_lines[line_idx]`
    /// for `frames` samples of read-out latency.
    ApplyDelay {
        buf: BufRef,
        line_idx: u32,
        frames: u32,
    },

    /// PR4 sidechain: copy `src` into the plugin at `(dst_track, dst_index)`'s
    /// `aux_in_port` shmem buffer **before** the plugin's `process()` runs.
    /// `compile_schedule` keys by `(track, device_index)` because the runtime
    /// `plugin_id` (assigned by daw_plugin_host) isn't visible at compile
    /// time. The engine resolves to a concrete `plugin_id` via
    /// `slot_to_plugin_id` at dispatch time. v23 single-chain: `dst_index` is
    /// the device's position in `Track.devices`.
    SidechainTap {
        src: BufRef,
        dst_track: u32,
        dst_index: u32,
        aux_in_port: u8,
    },

    /// PR4 aux send: accumulate `src` (the source track's post- or
    /// pre-fader buffer) into `dst` (the destination return / bus track's
    /// scratch) scaled by the **live, per-sample-ramped** send gain of
    /// `song.tracks[src_track_idx].sends[send_idx]`. Emitted **after** the
    /// dst's clearing `Mix` (so it accumulates on top of any children) and
    /// **before** the dst's `ProcessGroupFx`. The gain is read live (not
    /// baked into the schedule) so knob drags / `SendGain` automation
    /// apply without recompiling, and a disabled send contributes silence.
    /// `src_track_idx` / `send_idx` resolve the gain; `src` resolves the
    /// audio (`PostFader` → `TrackScratch`, `PreFader` → `PreFaderScratch`).
    MixSend {
        src: BufRef,
        dst: BufRef,
        src_track_idx: u32,
        send_idx: u8,
    },

    /// docs/plan_modulation.md §3: advance the envelope follower for
    /// `ModSource` at `slot` over `src`'s final scratch. Emitted at the end
    /// of the schedule (all scratches are settled) since the follower only
    /// produces a control-rate scalar — it never feeds back into the audio
    /// graph. `slot` indexes both `Schedule::follower_slots` and the
    /// `AudioBridge::mod_scalars` plane (= the `ModSource`'s position in
    /// `Song::mod_sources`).
    EnvelopeFollow { src: BufRef, slot: u32 },
}

/// Compiled, immutable execution plan. Held inside an
/// `Arc<ArcSwap<Schedule>>` and replaced wholesale on every routing
/// edit; the RT thread `load`s a snapshot at the top of each buffer.
pub struct Schedule {
    /// Ordered list of node ops to execute this buffer.
    pub nodes: Vec<NodeOp>,
    /// PDC delay-line pool (PR3). Indexed by `NodeOp::ApplyDelay::line_idx`.
    pub delay_lines: Vec<DelayLine>,
    /// Pooled stereo buffers used by ops whose dst is `BufRef::Pooled`.
    /// PR1 leaves this empty.
    pub port_buffers: PortBufferPool,
    /// PR4.5 sidechain plugin-internal alignment: per-track input delay
    /// in samples, applied **after** vocal/clip render + instrument output
    /// but **before** the audio FX chain. This brings each track's main
    /// signal into musical alignment with its sidechain sources, so a
    /// sidechain plugin sees `main_in` and `aux_in` at the same musical
    /// time.
    ///
    /// Indexed by song track index (parallel to `song.tracks`). Entry `i`
    /// is `max(path_latency(src) for src in fx_chain[*].aux_inputs[*].tap)`,
    /// or 0 if the track has no fx_chain sidechain wiring.
    ///
    /// MVP scope: only `fx_chain` plugin sidechain is reflected here.
    /// `midi_fx_chain` / `instrument` sidechain alignment requires
    /// delaying MIDI events too (out of scope for PR4.5).
    pub input_delay_per_track: Vec<u32>,
    /// docs/plan_modulation.md §3: per-`ModSource` envelope follower state +
    /// baked coefficients, indexed by slot (= `ModSource` position in
    /// `Song::mod_sources`). `NodeOp::EnvelopeFollow { slot, .. }` advances
    /// `follower_slots[slot].env` each buffer; the engine publishes that env
    /// to `AudioBridge::mod_scalars[slot]`. Rebuilt on recompile (so env
    /// resets), persists across buffers within one schedule (like
    /// `delay_lines`).
    pub follower_slots: Vec<super::follower::FollowerSlot>,
    /// FIXME #56 (docs/plan_fixme_56_modulators.md): per-`ModSource` の種別を
    /// slot 順 (= `follower_slots` / `AudioBridge::mod_scalars` と 1:1) に保持。
    /// generator (LFO/Random/MSEG/Steps) は `common::modulators::generator_scalar`
    /// で `song_beat` から直接算出され、その slot の `follower_slots` 値は使われない
    /// (inert)。envelope follower の slot は `follower_slots[slot].env` を使う。
    pub mod_kinds: Vec<common::model::ModSourceKind>,
}

impl Schedule {
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            delay_lines: Vec::new(),
            port_buffers: PortBufferPool::new(),
            input_delay_per_track: Vec::new(),
            follower_slots: Vec::new(),
            mod_kinds: Vec::new(),
        }
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self::empty()
    }
}
