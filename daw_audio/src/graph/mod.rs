//! Routing graph for the audio engine.
//!
//! `Schedule` is an immutable, RT-safe execution plan compiled from the
//! current `Song`. The CPAL audio thread loads it via `ArcSwap` once per
//! buffer and drives every `NodeOp` in order — no DAG walk on the RT
//! path.
//!
//! PR1 ships only a flat reducer (every `kind == Audio` track → master);
//! group tracks (PR2), PDC (PR3), sidechain + parallel out (PR4) plug in
//! by extending [`NodeOp`] and [`compile_schedule`].

// PR1 ships the graph types as a self-contained module that engine.rs
// hasn't been switched over to yet — the `pub use` re-exports are
// genuinely unused at this stage and become live in PR1c. Suppress the
// warnings (rather than gate the re-exports) so the public surface
// stays stable across PR1a/b/c.
#![allow(dead_code, unused_imports)]

mod compile;
mod delay_line;
mod port_buffer;
mod schedule;

pub use compile::{GraphError, compile_schedule};
pub use delay_line::DelayLine;
pub use port_buffer::{PortBuffer, PortBufferPool};
pub use schedule::{BufRef, NodeOp, Schedule};
