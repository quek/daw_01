//! Host-side ARA audio sources and the random-access sample read that backs
//! `ARAAudioAccessControllerInterface::readAudioSamples`.
//!
//! daw_01 audio sources are either file-backed (imported WAV) or in-memory
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

    /// Decode a WAV file fully into memory, preserving all channels and bit
    /// depth (16/24/32-bit int and 32-bit float). Unlike
    /// `common::voicevox::decode_wav_to_f32`, this keeps every channel (ARA
    /// needs the native channel layout, not a mono mixdown).
    pub fn from_wav_file(path: &Path) -> Result<Self> {
        let mut reader = hound::WavReader::open(path)
            .with_context(|| format!("open WAV {}", path.display()))?;
        let spec = reader.spec();
        anyhow::ensure!(spec.sample_rate > 0, "WAV sample_rate is 0");
        anyhow::ensure!(spec.channels > 0, "WAV has 0 channels");

        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader
                .samples::<f32>()
                .collect::<Result<_, _>>()
                .context("reading f32 WAV samples")?,
            hound::SampleFormat::Int => {
                // hound sign-extends any int width into i32; normalise to
                // [-1, 1) by the full-scale value for the declared bit depth.
                // Clamp the shift to [1, 32]: hound only reads int samples as
                // i32, but a malformed fmt chunk must not overflow the shift.
                let bits = spec.bits_per_sample.clamp(1, 32);
                let scale = 1.0 / (1i64 << (bits - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|v| v as f32 * scale))
                    .collect::<Result<_, _>>()
                    .context("reading int WAV samples")?
            }
        };

        Ok(Self::from_interleaved(
            samples.into(),
            f64::from(spec.sample_rate),
            u32::from(spec.channels),
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
