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

    /// PR4: copy `src` into `plugin_id`'s `aux_in_port` shmem buffer
    /// before the plugin's main `process()` runs.
    SidechainTap {
        src: BufRef,
        plugin_id: u32,
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
}

impl Schedule {
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            delay_lines: Vec::new(),
            port_buffers: PortBufferPool::new(),
        }
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self::empty()
    }
}
