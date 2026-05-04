//! Shared-memory layout for a single plugin instance's `process()` call.
//!
//! `daw_audio` writes the inputs (frames, events, buffer_in), signals the
//! plugin host via `event_request`, the host runs `plugin.process()` filling
//! the outputs (events_out, buffer_out), then signals `event_response`.
//!
//! Sizes are fixed at compile time so the whole struct is plain old data
//! living in shared memory: no allocations, no descriptors, no headers.
//!
//! Multiple plugin instances each get their own `ProcessData` slot (one
//! shmem region + one event pair per plugin), so worker threads on the
//! audio side can hand work to different plugins concurrently.

pub const MAX_FRAMES: usize = 1024;
pub const MAX_CHANNELS: usize = 2;
pub const MAX_EVENTS: usize = 256;

#[repr(C)]
pub struct ProcessData {
    /// How many frames to process this call (`<= MAX_FRAMES`).
    pub frames: u32,
    /// Sample counter monotonically advanced by the audio engine. Fed into
    /// the CLAP transport so plugins see a consistent timeline.
    pub steady_time: u64,
    /// Sample rate the engine was activated at (Hz). Stored so the plugin
    /// host can pass it through the CLAP process struct without a separate
    /// IPC hop.
    pub sample_rate: u32,
    /// Whether the transport is rolling. Lets plugins distinguish "render
    /// silence" from "host is paused".
    pub playing: u8,
    pub _pad0: [u8; 3],

    pub n_events_in: u32,
    pub events_in: [Event; MAX_EVENTS],
    pub n_events_out: u32,
    pub events_out: [Event; MAX_EVENTS],

    /// Planar f32 input audio (channel × frame).
    pub buffer_in: [[f32; MAX_FRAMES]; MAX_CHANNELS],
    /// Planar f32 output audio.
    pub buffer_out: [[f32; MAX_FRAMES]; MAX_CHANNELS],
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct Event {
    pub kind: EventKind,
    pub _pad: [u8; 3],
    /// Frame offset within the buffer (`< frames`).
    pub time: u32,
    /// Note number (NoteOn / NoteOff). Param events ignore this.
    pub key: u8,
    pub channel: u8,
    pub _pad1: [u8; 2],
    /// Velocity (NoteOn) — ignored otherwise.
    pub velocity: f64,
    /// CLAP param id (Param events) — 0 for note events.
    pub param_id: u32,
    pub _pad2: [u8; 4],
    /// Param value (Param events) — ignored otherwise.
    pub value: f64,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    NoteOn = 1,
    NoteOff = 2,
    ParamValue = 3,
}

impl ProcessData {
    pub const fn empty() -> Self {
        Self {
            frames: 0,
            steady_time: 0,
            sample_rate: 48_000,
            playing: 0,
            _pad0: [0; 3],
            n_events_in: 0,
            events_in: [EMPTY_EVENT; MAX_EVENTS],
            n_events_out: 0,
            events_out: [EMPTY_EVENT; MAX_EVENTS],
            buffer_in: [[0.0; MAX_FRAMES]; MAX_CHANNELS],
            buffer_out: [[0.0; MAX_FRAMES]; MAX_CHANNELS],
        }
    }

    /// Reset event counts and silence input/output buffers headers. Called
    /// before each dispatch so stale events from a previous buffer don't
    /// leak through.
    pub fn prepare(&mut self) {
        self.n_events_in = 0;
        self.n_events_out = 0;
    }

    /// Push a NoteOn into `events_in`. Silently truncates if the buffer is
    /// full — at MAX_EVENTS=256 per buffer this should never happen for
    /// normal MIDI traffic, and panicking inside RT is worse than dropping.
    pub fn push_note_on(&mut self, time: u32, key: u8, velocity: f64, channel: u8) {
        let i = self.n_events_in as usize;
        if i >= MAX_EVENTS {
            return;
        }
        self.events_in[i] = Event {
            kind: EventKind::NoteOn,
            _pad: [0; 3],
            time,
            key,
            channel,
            _pad1: [0; 2],
            velocity,
            param_id: 0,
            _pad2: [0; 4],
            value: 0.0,
        };
        self.n_events_in += 1;
    }

    pub fn push_note_off(&mut self, time: u32, key: u8, channel: u8) {
        let i = self.n_events_in as usize;
        if i >= MAX_EVENTS {
            return;
        }
        self.events_in[i] = Event {
            kind: EventKind::NoteOff,
            _pad: [0; 3],
            time,
            key,
            channel,
            _pad1: [0; 2],
            velocity: 0.0,
            param_id: 0,
            _pad2: [0; 4],
            value: 0.0,
        };
        self.n_events_in += 1;
    }

    pub fn push_param(&mut self, time: u32, param_id: u32, value: f64) {
        let i = self.n_events_in as usize;
        if i >= MAX_EVENTS {
            return;
        }
        self.events_in[i] = Event {
            kind: EventKind::ParamValue,
            _pad: [0; 3],
            time,
            key: 0,
            channel: 0,
            _pad1: [0; 2],
            velocity: 0.0,
            param_id,
            _pad2: [0; 4],
            value,
        };
        self.n_events_in += 1;
    }
}

const EMPTY_EVENT: Event = Event {
    kind: EventKind::NoteOn,
    _pad: [0; 3],
    time: 0,
    key: 0,
    channel: 0,
    _pad1: [0; 2],
    velocity: 0.0,
    param_id: 0,
    _pad2: [0; 4],
    value: 0.0,
};

#[cfg(windows)]
mod shmem_handle {
    use anyhow::{Context, Result};
    use shared_memory::{Shmem, ShmemConf};

    use super::ProcessData;

    /// Owning handle to a `ProcessData` shared memory region. The audio
    /// engine creates it; the plugin host opens it by the same id.
    pub struct ProcessDataHandle {
        shmem: Shmem,
    }

    impl ProcessDataHandle {
        pub fn create(os_id: &str) -> Result<Self> {
            let shmem = ShmemConf::new()
                .size(std::mem::size_of::<ProcessData>())
                .os_id(os_id)
                .create()
                .with_context(|| format!("failed to create shmem {os_id}"))?;
            // Zero the region so the reading side never sees uninit memory.
            unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, std::mem::size_of::<ProcessData>()) };
            Ok(Self { shmem })
        }

        pub fn open(os_id: &str) -> Result<Self> {
            let shmem = ShmemConf::new()
                .os_id(os_id)
                .open()
                .with_context(|| format!("failed to open shmem {os_id}"))?;
            anyhow::ensure!(
                shmem.len() >= std::mem::size_of::<ProcessData>(),
                "shmem too small for ProcessData: {} < {}",
                shmem.len(),
                std::mem::size_of::<ProcessData>()
            );
            Ok(Self { shmem })
        }

        pub fn ptr(&self) -> *mut ProcessData {
            self.shmem.as_ptr() as *mut ProcessData
        }
    }

    // The single ProcessData slot is exclusively written by the audio
    // engine (inputs) and the plugin host worker (outputs); the named
    // event handshake serialises every access.
    unsafe impl Send for ProcessDataHandle {}
    unsafe impl Sync for ProcessDataHandle {}
}

#[cfg(windows)]
pub use shmem_handle::ProcessDataHandle;
