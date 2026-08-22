// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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
    /// **このバッファの同一性**: decode 元の解決済み絶対パス。
    /// `daw_audio::audio_clip_renderer::AudioSourceBuffer::origin` と同じ規約。
    ///
    /// [`AudioSourceCache`] の key である `AudioSourceId` は **Song スコープの
    /// 名前**でしかない (`IdAllocators::next_audio_source_id` は project ごとに
    /// 1 から再採番される) ので、別 project を開くと必ず衝突する。id だけで
    /// キャッシュヒットを判定すると前 project の波形が描かれ、onset 検出 /
    /// ARA materialize も前 project の音を読む。再利用の可否はこの origin の
    /// 一致で判定する (= キャッシュの同一性はキャッシュされた値自身が持つ)。
    pub origin: std::path::PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub samples: Vec<Vec<f32>>,
}

impl AudioSourceBuffer {
    /// Mono downmix of frames `[start, end)` (clamped to the buffer),
    /// averaging all channels. OFF-RT analysis helper (e.g. onset detection
    /// for `StretchMode::Slice`). Empty channel set / empty range → empty Vec.
    pub fn downmix_mono(&self, start: usize, end: usize) -> Vec<f32> {
        let ch = self.samples.len();
        if ch == 0 || end <= start {
            return Vec::new();
        }
        let mut mono = vec![0.0f32; end - start];
        for plane in &self.samples {
            for (out, sample_idx) in (start..end).enumerate() {
                if let Some(&s) = plane.get(sample_idx) {
                    mono[out] += s;
                }
            }
        }
        let inv = 1.0 / ch as f32;
        for m in &mut mono {
            *m *= inv;
        }
        mono
    }
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
