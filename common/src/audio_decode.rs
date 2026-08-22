// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Cross-platform audio-file decoder — the **single** decode engine for
//! imported audio sources.
//!
//! Audio import is core DAW function and must work on every target (Linux is
//! supported alongside Windows). `rsmpeg`/libav is Windows-only in this
//! workspace and is reserved for VIDEO decode (`daw_gui::libav_decoder`);
//! routing audio through it would break Linux import and couple core audio to
//! the vendored FFmpeg binaries. So audio uses the pure-Rust `symphonia`, which
//! decodes WAV / AIFF / FLAC / MP3 / OGG-Vorbis / M4A(AAC+ALAC) into planar f32
//! with no system dependencies (r.md #19).
//!
//! Three processes decode file-backed sources **independently** — no bulk PCM
//! ever crosses the IPC wire (arch invariant #2 "wire is blob-less"): daw_gui
//! (import + waveform), daw_audio (playback), daw_plugin_host (ARA). They all
//! call [`decode_audio_file`] so the decode logic lives in exactly one place.
//!
//! **Not real-time safe**: [`decode_audio_file`] allocates and does file I/O.
//! Every caller runs it off the audio thread (import worker / off-thread
//! schedule compile / ARA session setup).

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::Signal;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Lower-case file extensions the import pipeline advertises (rfd dialog filter
/// SSoT) and that [`decode_audio_file`] handles. symphonia probes by file
/// CONTENT, so a wrong/absent extension still decodes — this list only bounds
/// what the native file picker lets the user *select*. Keep sorted by family.
pub const SUPPORTED_AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", // RIFF/WAVE PCM+ADPCM
    "aif", "aiff", "aifc", // AIFF / AIFF-C
    "flac", // FLAC
    "mp3", // MPEG-1 Layer III
    "ogg", "oga", // OGG (Vorbis)
    "m4a", "mp4", "aac", "alac", // ISO-MP4 (AAC / ALAC)
];

/// Fully-decoded audio in planar f32 layout (`samples[channel][frame]`) — the
/// shape every consumer's `AudioSourceBuffer` uses.
#[derive(Debug)]
pub struct DecodedAudio {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub samples: Vec<Vec<f32>>,
}

impl DecodedAudio {
    /// Interleave the planar samples (`ch0f0, ch1f0, ch0f1, …`). Used by the
    /// ARA host, which serves random-access reads from an interleaved slice.
    pub fn interleaved(&self) -> Vec<f32> {
        let ch = self.channels as usize;
        if ch == 0 {
            return Vec::new();
        }
        let frames = self.frames as usize;
        let mut out = vec![0.0f32; frames * ch];
        for (c, plane) in self.samples.iter().enumerate() {
            for (f, &s) in plane.iter().enumerate() {
                out[f * ch + c] = s;
            }
        }
        out
    }
}

#[derive(Debug)]
pub enum DecodeError {
    Io(String),
    /// symphonia could not identify the container/codec, or the build lacks the
    /// codec feature (unknown / corrupt / unsupported file).
    Unsupported(String),
    Decode(String),
    /// The file reported 0 channels / 0 Hz or decoded to zero frames.
    Empty,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Io(s) => write!(f, "I/O error: {s}"),
            DecodeError::Unsupported(s) => write!(f, "unsupported or unrecognized audio: {s}"),
            DecodeError::Decode(s) => write!(f, "decode failed: {s}"),
            DecodeError::Empty => write!(f, "audio file contained no samples"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decode an audio file at `path` fully into planar f32. Handles every format
/// the crate is built with (WAV, AIFF, FLAC, MP3, OGG-Vorbis, M4A AAC/ALAC —
/// see the `symphonia` feature list in the root `Cargo.toml`). The container is
/// detected by content; the extension is only a probe hint.
pub fn decode_audio_file(path: &Path) -> Result<DecodedAudio, DecodeError> {
    let file =
        File::open(path).map_err(|e| DecodeError::Io(format!("{}: {e}", path.display())))?;
    // `std::fs::File` already implements symphonia's `MediaSource` (Read + Seek).
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    // Gapless trimming gives sample-accurate length for MP3/AAC encoder padding.
    let fmt_opts = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let meta_opts = MetadataOptions::default();

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &meta_opts)
        .map_err(|e| DecodeError::Unsupported(format!("{}: {e}", path.display())))?;
    let mut format = probed.format;

    // First track with a real (decodeable) codec — skips cover-art / metadata.
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| DecodeError::Unsupported(format!("{}: no audio track", path.display())))?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| DecodeError::Unsupported(format!("{}: {e}", path.display())))?;

    let mut sample_rate: u32 = 0;
    let mut channels: usize = 0;
    let mut planar: Vec<Vec<f32>> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // symphonia signals a clean end-of-stream as an IoError with
            // UnexpectedEof out of `next_packet()`.
            Err(SymphoniaError::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            // Decoder parameters changed at a stream boundary: reset its state
            // and keep decoding OUR track. (A chained stream's *other* logical
            // streams carry a different `track_id` and are skipped below — we
            // import the first audio stream only; mixing streams with differing
            // rates/layouts into one buffer would be wrong anyway.)
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(DecodeError::Decode(format!("{}: {e}", path.display()))),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(buf) => buf,
            // A single malformed packet is recoverable — skip it, keep going.
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => continue,
            Err(e) => return Err(DecodeError::Decode(format!("{}: {e}", path.display()))),
        };

        // Learn geometry from the first decoded buffer, then allocate the planes.
        if planar.is_empty() {
            let spec = *decoded.spec();
            sample_rate = spec.rate;
            channels = spec.channels.count();
            if channels == 0 || sample_rate == 0 {
                return Err(DecodeError::Empty);
            }
            planar = (0..channels).map(|_| Vec::new()).collect();
        }

        // Convert ANY source sample format (U8/S16/S24/S32/F32/F64/…) to an
        // owned planar `AudioBuffer<f32>` in one step, then append per channel.
        // `Signal::chan(ch)` yields each channel's contiguous `&[f32]`, so the
        // planar layout is preserved without an interleave round-trip.
        let mut fbuf = decoded.make_equivalent::<f32>();
        decoded.convert(&mut fbuf);
        // Geometry was frozen from the first buffer, but a codec CAN change its
        // channel count mid-stream (e.g. AAC parametric stereo up/down-mixing,
        // same track_id, no ResetRequired). `AudioBuffer::chan` panics on an
        // out-of-range index, and every consumer relies on all planes staying
        // frame-aligned — so map only the channels present in this buffer and
        // pad any missing plane with silence for this packet (an increase drops
        // the extra channels). Both keep planes equal-length and never panic.
        let this_channels = fbuf.spec().channels.count();
        let this_frames = if this_channels > 0 { fbuf.chan(0).len() } else { 0 };
        for (ch, plane) in planar.iter_mut().enumerate() {
            if ch < this_channels {
                plane.extend_from_slice(fbuf.chan(ch));
            } else {
                plane.resize(plane.len() + this_frames, 0.0);
            }
        }
    }

    if planar.is_empty() || channels == 0 {
        return Err(DecodeError::Empty);
    }
    let frames = planar[0].len() as u64;
    if frames == 0 {
        return Err(DecodeError::Empty);
    }

    Ok(DecodedAudio {
        sample_rate,
        channels: channels as u16,
        frames,
        samples: planar,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::path::PathBuf;

    /// Write a deterministic stereo 16-bit PCM WAV and return its path.
    fn write_wav(dir: &Path, name: &str, frames: usize, sample_rate: u32) -> PathBuf {
        let path = dir.join(name);
        let spec = WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut w = WavWriter::create(&path, spec).unwrap();
        for f in 0..frames {
            // L ascending, R descending — distinguishable channels.
            w.write_sample(((f as i32 % 100) * 100) as i16).unwrap();
            w.write_sample((-((f as i32 % 100) * 100)) as i16).unwrap();
        }
        w.finalize().unwrap();
        path
    }

    #[test]
    fn decodes_wav_to_planar_with_correct_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wav(dir.path(), "a.wav", 512, 48_000);
        let dec = decode_audio_file(&path).unwrap();
        assert_eq!(dec.sample_rate, 48_000);
        assert_eq!(dec.channels, 2);
        assert_eq!(dec.frames, 512);
        assert_eq!(dec.samples.len(), 2);
        assert_eq!(dec.samples[0].len(), 512);
        assert_eq!(dec.samples[1].len(), 512);
        // Channel separation preserved: L[1] > 0, R[1] < 0 (see write_wav).
        assert!(dec.samples[0][1] > 0.0, "L ascending");
        assert!(dec.samples[1][1] < 0.0, "R descending");
    }

    #[test]
    fn interleaved_matches_planar() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wav(dir.path(), "b.wav", 8, 44_100);
        let dec = decode_audio_file(&path).unwrap();
        let inter = dec.interleaved();
        assert_eq!(inter.len(), 8 * 2);
        for f in 0..8 {
            assert_eq!(inter[f * 2], dec.samples[0][f], "L at frame {f}");
            assert_eq!(inter[f * 2 + 1], dec.samples[1][f], "R at frame {f}");
        }
    }

    #[test]
    fn rejects_non_audio_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.aif");
        std::fs::write(&path, b"not an audio file at all").unwrap();
        let err = decode_audio_file(&path).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported(_) | DecodeError::Decode(_)));
    }
}
