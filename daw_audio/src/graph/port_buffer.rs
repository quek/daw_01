//! Pre-allocated stereo buffer pool. Used (PR2 onwards) for nodes whose
//! output doesn't naturally live in a per-track scratch — group bus
//! inputs (sum of children before group fx), sidechain auxiliary
//! buffers when source / dest layouts don't match, parallel-out fan-in
//! buffers, etc.
//!
//! All buffers are sized at `MAX_FRAMES` so the audio thread can cap a
//! buffer-of-the-buffer with a slice without allocating.

#![allow(dead_code)]

use common::process_data::MAX_FRAMES;

/// One stereo buffer of `MAX_FRAMES` samples.
pub struct PortBuffer {
    pub l: Vec<f32>,
    pub r: Vec<f32>,
}

impl PortBuffer {
    pub fn new() -> Self {
        Self {
            l: vec![0.0; MAX_FRAMES],
            r: vec![0.0; MAX_FRAMES],
        }
    }

    pub fn clear(&mut self, frames: usize) {
        let n = frames.min(self.l.len()).min(self.r.len());
        self.l[..n].fill(0.0);
        self.r[..n].fill(0.0);
    }
}

impl Default for PortBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Owning pool of stereo buffers. Compiled into `Schedule::port_buffers`
/// on every recompile so the schedule is RT-self-contained — the audio
/// thread holds the `Arc<Schedule>` and never reaches outside it.
pub struct PortBufferPool {
    buffers: Vec<PortBuffer>,
}

impl PortBufferPool {
    pub fn new() -> Self {
        Self {
            buffers: Vec::new(),
        }
    }

    pub fn with_capacity(n: usize) -> Self {
        Self {
            buffers: (0..n).map(|_| PortBuffer::new()).collect(),
        }
    }

    pub fn get(&self, idx: usize) -> Option<&PortBuffer> {
        self.buffers.get(idx)
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut PortBuffer> {
        self.buffers.get_mut(idx)
    }

    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    pub fn clear_all(&mut self, frames: usize) {
        for buf in self.buffers.iter_mut() {
            buf.clear(frames);
        }
    }
}

impl Default for PortBufferPool {
    fn default() -> Self {
        Self::new()
    }
}
