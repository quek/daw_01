//! GUI-side decoded audio buffer cache.
//!
//! Each `AudioSource` in `Song.audio_sources` has at most one decoded
//! buffer here, keyed by `AudioSourceId`. Used by the arrangement
//! waveform view (and by the future Audio Editor in PR4 / Phase 2).
//! The audio engine maintains its own independent cache — file-backed
//! sources are decoded twice (once per process) to keep IPC lean.
//!
//! Spec: `docs/plan_audio_clip.md` §6.1, §8.3.

use std::collections::HashMap;
use std::sync::Arc;

use common::model::AudioSourceId;

/// Decoded sample buffer. Planar storage (`samples[ch][frame]`).
/// Mirror of `daw_audio::audio_clip_renderer::AudioSourceBuffer` —
/// kept as a separate type because the two crates decode
/// independently and don't share the buffer over IPC.
#[derive(Debug)]
pub struct AudioSourceBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub samples: Vec<Vec<f32>>,
}

#[derive(Default)]
pub struct AudioSourceCache {
    map: HashMap<AudioSourceId, Arc<AudioSourceBuffer>>,
}

impl AudioSourceCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, id: AudioSourceId, buffer: Arc<AudioSourceBuffer>) {
        self.map.insert(id, buffer);
    }

    pub fn get(&self, id: AudioSourceId) -> Option<Arc<AudioSourceBuffer>> {
        self.map.get(&id).cloned()
    }

    pub fn contains(&self, id: AudioSourceId) -> bool {
        self.map.contains_key(&id)
    }

    pub fn remove(&mut self, id: AudioSourceId) {
        self.map.remove(&id);
    }

    pub fn retain<F: FnMut(AudioSourceId) -> bool>(&mut self, mut keep: F) {
        self.map.retain(|id, _| keep(*id));
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
