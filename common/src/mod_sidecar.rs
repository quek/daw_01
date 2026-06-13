//! docs/plan_modulation.md §7: modulation envelope sidecar.
//!
//! Video export can't read the live `AudioBridge::mod_scalars` plane (the
//! audio engine isn't running during the GUI's offline frame render). So the
//! offline audio export (`daw_audio::export`) bakes each `ModSource`'s
//! envelope-follower value per render buffer into a sidecar file written next
//! to the WAV; the video renderer (`daw_gui::render_video`) reads it back and
//! samples it at each frame's beat, feeding the same `§2` composition the live
//! preview uses. One implementation, three paths (preview / bake / sample) →
//! no drift.
//!
//! Layout (little-endian): `MAGIC u32 | n_sources u32 | n_buffers u32 |
//! beats[n_buffers] f32 | scalars[n_buffers * n_sources] f32` (scalars are
//! row-major `[buffer][source slot]`, slot = `ModSource` position).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: u32 = 0x4d4f_4445; // "MODE"

/// Baked per-buffer envelope-follower scalars over an export, keyed by beat.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ModEnvSidecar {
    /// Number of `ModSource` slots per buffer row.
    pub n_sources: usize,
    /// Ascending beat of each recorded buffer (export `playhead_beats`).
    pub beats: Vec<f32>,
    /// Row-major scalars: `beats.len() * n_sources`, `[buffer][source slot]`.
    pub scalars: Vec<f32>,
}

impl ModEnvSidecar {
    pub fn new(n_sources: usize) -> Self {
        Self {
            n_sources,
            beats: Vec::new(),
            scalars: Vec::new(),
        }
    }

    /// Append one buffer's `(beat, per-source env)` row. `env.len()` must equal
    /// `n_sources`.
    pub fn push(&mut self, beat: f32, env: &[f32]) {
        debug_assert_eq!(env.len(), self.n_sources);
        self.beats.push(beat);
        self.scalars.extend_from_slice(env);
    }

    pub fn is_empty(&self) -> bool {
        self.n_sources == 0 || self.beats.is_empty()
    }

    /// Fill `out` with the scalars of the latest recorded buffer whose beat is
    /// `<= beat` (step / sample-and-hold, matching the live block-rate plane).
    /// `out` ends up length `n_sources` (empty if the sidecar is empty).
    pub fn sample_at(&self, beat: f64, out: &mut Vec<f32>) {
        out.clear();
        if self.is_empty() {
            return;
        }
        let n = self.n_sources;
        // last index with beats[idx] <= beat (clamp to first row before start).
        let idx = self
            .beats
            .partition_point(|b| f64::from(*b) <= beat)
            .saturating_sub(1);
        let start = idx * n;
        if let Some(row) = self.scalars.get(start..start + n) {
            out.extend_from_slice(row);
        }
    }

    /// The sidecar path for a given WAV path (`foo.wav` → `foo.modenv`).
    pub fn sidecar_path(wav: &Path) -> PathBuf {
        wav.with_extension("modenv")
    }

    pub fn write(&self, path: &Path) -> io::Result<()> {
        let mut f = io::BufWriter::new(std::fs::File::create(path)?);
        f.write_all(&MAGIC.to_le_bytes())?;
        f.write_all(&u32::try_from(self.n_sources).unwrap_or(u32::MAX).to_le_bytes())?;
        f.write_all(&u32::try_from(self.beats.len()).unwrap_or(u32::MAX).to_le_bytes())?;
        for b in &self.beats {
            f.write_all(&b.to_le_bytes())?;
        }
        for s in &self.scalars {
            f.write_all(&s.to_le_bytes())?;
        }
        f.flush()
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let mut c = io::Cursor::new(bytes.as_slice());
        let mut b4 = [0u8; 4];
        c.read_exact(&mut b4)?;
        if u32::from_le_bytes(b4) != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "modenv: bad magic",
            ));
        }
        c.read_exact(&mut b4)?;
        let n_sources = u32::from_le_bytes(b4) as usize;
        c.read_exact(&mut b4)?;
        let n_buffers = u32::from_le_bytes(b4) as usize;
        let mut beats = Vec::with_capacity(n_buffers);
        for _ in 0..n_buffers {
            c.read_exact(&mut b4)?;
            beats.push(f32::from_le_bytes(b4));
        }
        let n_scalars = n_buffers.saturating_mul(n_sources);
        let mut scalars = Vec::with_capacity(n_scalars);
        for _ in 0..n_scalars {
            c.read_exact(&mut b4)?;
            scalars.push(f32::from_le_bytes(b4));
        }
        Ok(Self {
            n_sources,
            beats,
            scalars,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_at_step_holds_and_clamps() {
        let mut s = ModEnvSidecar::new(2);
        s.push(0.0, &[0.1, 0.2]);
        s.push(1.0, &[0.3, 0.4]);
        s.push(2.0, &[0.5, 0.6]);
        let mut out = Vec::new();
        s.sample_at(-1.0, &mut out); // before first → first row
        assert_eq!(out, vec![0.1, 0.2]);
        s.sample_at(1.5, &mut out); // between → last <= 1.5
        assert_eq!(out, vec![0.3, 0.4]);
        s.sample_at(99.0, &mut out); // past end → last row
        assert_eq!(out, vec![0.5, 0.6]);
    }

    #[test]
    fn roundtrip_write_read(// uses a temp path derived from the test name
    ) {
        let dir = std::env::temp_dir();
        let wav = dir.join("daw01_modenv_roundtrip.wav");
        let path = ModEnvSidecar::sidecar_path(&wav);
        let mut s = ModEnvSidecar::new(1);
        s.push(0.0, &[0.25]);
        s.push(0.5, &[0.75]);
        s.write(&path).unwrap();
        let back = ModEnvSidecar::read(&path).unwrap();
        assert_eq!(s, back);
        let _ = std::fs::remove_file(&path);
    }
}
