//! Schedule + NodeOp + BufRef.
//!
//! A `Schedule` is the compiled execution plan that the audio thread
//! steps through every buffer. It owns its delay-line ring buffers and
//! its port-buffer pool so the RT path never allocates.

#![allow(dead_code)]

use common::protocol::PluginSlot;

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

    /// PR4 sidechain: copy `src` into the plugin at `(dst_track, dst_slot)`'s
    /// `aux_in_port` shmem buffer **before** the plugin's `process()` runs.
    /// `compile_schedule` keys by `(track, slot)` because the runtime
    /// `plugin_id` (assigned by daw_plugin_host) isn't visible at compile
    /// time. The engine resolves to a concrete `plugin_id` via
    /// `slot_to_plugin_id` at dispatch time.
    SidechainTap {
        src: BufRef,
        dst_track: u32,
        dst_slot: PluginSlot,
        aux_in_port: u8,
    },
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
    /// is `max(path_latency(src) for src in fx_chain[*].sidechain_sources)`,
    /// or 0 if the track has no fx_chain sidechain wiring.
    ///
    /// MVP scope: only `fx_chain` plugin sidechain is reflected here.
    /// `midi_fx_chain` / `instrument` sidechain alignment requires
    /// delaying MIDI events too (out of scope for PR4.5).
    pub input_delay_per_track: Vec<u32>,
}

impl Schedule {
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            delay_lines: Vec::new(),
            port_buffers: PortBufferPool::new(),
            input_delay_per_track: Vec::new(),
        }
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self::empty()
    }
}
