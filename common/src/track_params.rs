//! Per-track mixer parameters published wait-free between the GUI and audio
//! threads.
//!
//! `volume` / `pan` are stored as `AtomicU32` carrying the bit pattern of an
//! `f32`, so the surrounding struct stays `Send + Sync` without any mutex.
//! `muted` / `solo` are plain `AtomicBool`. All loads use `Acquire`, all
//! stores use `Release`, so the audio thread always sees a consistent
//! snapshot of any single field even when multiple flags are touched in
//! quick succession.
//!
//! Owned by `daw_audio` (the audio engine). The plugin host does **not**
//! consume these — it only knows about plugin instances.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub struct TrackAudioParams {
    volume: AtomicU32,
    pan: AtomicU32,
    muted: AtomicBool,
    solo: AtomicBool,
}

impl TrackAudioParams {
    pub fn new(volume: f32, pan: f32, muted: bool, solo: bool) -> Self {
        Self {
            volume: AtomicU32::new(volume.to_bits()),
            pan: AtomicU32::new(pan.to_bits()),
            muted: AtomicBool::new(muted),
            solo: AtomicBool::new(solo),
        }
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Acquire))
    }

    pub fn pan(&self) -> f32 {
        f32::from_bits(self.pan.load(Ordering::Acquire))
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    pub fn solo(&self) -> bool {
        self.solo.load(Ordering::Acquire)
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.to_bits(), Ordering::Release);
    }

    pub fn set_pan(&self, v: f32) {
        self.pan.store(v.to_bits(), Ordering::Release);
    }

    pub fn set_muted(&self, v: bool) {
        self.muted.store(v, Ordering::Release);
    }

    pub fn set_solo(&self, v: bool) {
        self.solo.store(v, Ordering::Release);
    }
}

impl Default for TrackAudioParams {
    fn default() -> Self {
        Self::new(1.0, 0.0, false, false)
    }
}
