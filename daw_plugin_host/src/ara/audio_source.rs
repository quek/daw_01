//! Host-side ARA audio sources and the random-access sample read that backs
//! `ARAAudioAccessControllerInterface::readAudioSamples`.
//!
//! daw_01 audio sources are either file-backed (any imported audio format) or in-memory
//! (`AudioSourcePath::Generated`, e.g. VOICEVOX vocals sent over
//! `SetGeneratedAudio` — no file on disk). Both decode to whole-source
//! interleaved f32 PCM held in memory: ARA model objects want whole-timeline
//! access, and daw_01 sources are short (sung phrases / loops), so serving
//! reads from a slice keeps `readAudioSamples` trivial and lock-free. Streaming
//! decode for very long imports is a future optimisation (docs/plan_ara2.md).

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

/// Host representation of one ARA audio source (the data behind an opaque
/// `ARAAudioSourceHostRef`). Immutable once created, matching ARA's notion of
/// an audio source as immutable sample data.
pub struct AraAudioSourceHost {
    pub sample_rate: f64,
    pub channel_count: u32,
    pub frame_count: u64,
    /// Whole-source interleaved f32 PCM (`channel_count`-interleaved).
    pub samples: Arc<[f32]>,
}

impl AraAudioSourceHost {
    /// Build from already-decoded interleaved PCM (e.g. VOICEVOX-generated
    /// audio that never touches disk).
    pub fn from_interleaved(samples: Arc<[f32]>, sample_rate: f64, channel_count: u32) -> Self {
        let frame_count = if channel_count == 0 {
            0
        } else {
            samples.len() as u64 / u64::from(channel_count)
        };
        Self {
            sample_rate,
            channel_count,
            frame_count,
            samples,
        }
    }

    /// Decode an audio file fully into memory, preserving all channels. Uses
    /// `common::audio_decode` (symphonia), so ARA plug-ins (e.g. Melodyne)
    /// operate on every format the DAW can import — WAV / AIFF / FLAC / MP3 /
    /// OGG / M4A (r.md #19). Unlike `voicevox_synth::decode_wav_to_f32`, this
    /// keeps every channel (ARA needs the native channel layout, not a mono
    /// mixdown).
    pub fn from_audio_file(path: &Path) -> Result<Self> {
        let decoded = common::audio_decode::decode_audio_file(path)
            .with_context(|| format!("decode audio source {}", path.display()))?;
        Ok(Self::from_interleaved(
            decoded.interleaved().into(),
            f64::from(decoded.sample_rate),
            u32::from(decoded.channels),
        ))
    }

    /// Single-sample random access used by `readAudioSamples`. Out-of-range
    /// frames (before start / past end) and out-of-range channels return
    /// silence, which ARA explicitly requires (it is not an error).
    pub fn sample_at(&self, channel: usize, frame: i64) -> f32 {
        if frame < 0
            || frame as u64 >= self.frame_count
            || channel >= self.channel_count as usize
        {
            return 0.0;
        }
        let idx = frame as usize * self.channel_count as usize + channel;
        self.samples.get(idx).copied().unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_at_handles_channels_and_bounds() {
        // 3 stereo frames: L/R interleaved.
        let s = AraAudioSourceHost::from_interleaved(
            vec![0.1, -0.1, 0.2, -0.2, 0.3, -0.3].into(),
            48_000.0,
            2,
        );
        assert_eq!(s.frame_count, 3);
        assert_eq!(s.sample_at(0, 0), 0.1); // L[0]
        assert_eq!(s.sample_at(1, 0), -0.1); // R[0]
        assert_eq!(s.sample_at(1, 1), -0.2); // R[1]
        assert_eq!(s.sample_at(0, -1), 0.0); // before start -> silence
        assert_eq!(s.sample_at(0, 3), 0.0); // past end -> silence
        assert_eq!(s.sample_at(2, 0), 0.0); // channel out of range -> silence
    }
}
