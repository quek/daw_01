//! Pre-rendered vocal audio for a track (e.g. VOICEVOX synthesis result).
//! daw_audio reads these samples directly into the per-track scratch
//! buffer when a Vocal track has no instrument plugin.
//!
//! Hot-swapped via `ArcSwapOption` so a freshly-synthesised WAV is picked
//! up on the next buffer without restarting the audio thread. Migrated
//! from `daw_plugin_host` as part of A2.

#![allow(dead_code)]

#[derive(Default, Clone)]
pub struct VocalAudio {
    /// Absolute sample position in the song where playback of `samples`
    /// should begin.
    pub clip_start_samples: u64,
    /// Mono f32 samples.
    pub samples: Vec<f32>,
}
