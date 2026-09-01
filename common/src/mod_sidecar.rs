//! docs/plan_modulation.md §7: modulation envelope sidecar.
//!
//! Video export can't read the live `AudioBridge` modulation plane (the audio
//! engine isn't running during the GUI's offline frame render). So the offline
//! audio export (`daw_audio::export`) bakes each `ModSource`'s value per render
//! buffer into a sidecar file written next to the WAV; the video renderer
//! (`daw_gui::render_video`) reads it back and samples it at each frame's beat,
//! feeding the same `§2` composition the live preview uses. One implementation,
//! three paths (preview / bake / sample) → no drift.
//!
//! **キーは `ModSource::id`** (`docs/plan_rmd_88_89_cross_modulation.md` §4-5、
//! アーキ不変条件 1)。以前は列が `Song::mod_sources` の**位置**で、書き出し後に
//! ソースを 1 つ消してから動画を描くと列が丸ごとずれて別のソースの値で絵が動いた。
//! magic を `MOD2` に上げてあるので旧 sidecar は読めない — 派生物なので書き出しを
//! やり直せば再生成される。
//!
//! Layout (little-endian):
//! `MAGIC u32 | n_sources u32 | n_buffers u32 | ids[n_sources] u32 |
//!  beats[n_buffers] f32 | scalars[n_buffers * n_sources] f32`
//! (scalars は row-major `[buffer][slot]`、slot の意味は `ids[slot]`)。

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::audio_bridge::MAX_MOD_SOURCES;
use crate::mod_plane::ModPlane;

const MAGIC: u32 = 0x4d4f_4432; // "MOD2"

/// Baked per-buffer modulator scalars over an export, keyed by beat (行) と
/// `ModSource::id` (列)。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ModEnvSidecar {
    /// 列の意味 — slot → `ModSource::id`。
    pub ids: Vec<u32>,
    /// Ascending beat of each recorded buffer (export `playhead_beats`).
    pub beats: Vec<f32>,
    /// Row-major scalars: `beats.len() * ids.len()`, `[buffer][slot]`.
    pub scalars: Vec<f32>,
}

impl ModEnvSidecar {
    /// `ids` は engine の compile 順 (`Schedule::follower_keys`)。空 = 記録しない。
    #[must_use]
    pub fn new(ids: Vec<u32>) -> Self {
        Self {
            ids,
            beats: Vec::new(),
            scalars: Vec::new(),
        }
    }

    /// 列数 (= ソース数)。
    #[must_use]
    pub fn n_sources(&self) -> usize {
        self.ids.len()
    }

    /// Append one buffer's `(beat, per-slot value)` row. `values.len()` must
    /// equal `ids.len()`.
    pub fn push(&mut self, beat: f32, values: &[f32]) {
        debug_assert_eq!(values.len(), self.ids.len());
        self.beats.push(beat);
        self.scalars.extend_from_slice(values);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty() || self.beats.is_empty()
    }

    /// Fill `out` with the values of the latest recorded buffer whose beat is
    /// `<= beat` (step / sample-and-hold, matching the live block-rate plane)。
    /// `out` は `ids` を持った値面になる (空の sidecar では空の面)。
    pub fn sample_at(&self, beat: f64, out: &mut ModPlane) {
        out.clear();
        if self.is_empty() {
            return;
        }
        let n = self.ids.len();
        // last index with beats[idx] <= beat (clamp to first row before start).
        let idx = self
            .beats
            .partition_point(|b| f64::from(*b) <= beat)
            .saturating_sub(1);
        let start = idx * n;
        let Some(row) = self.scalars.get(start..start + n) else {
            return;
        };
        for (id, v) in self.ids.iter().zip(row.iter()) {
            out.push(*id, *v);
        }
    }

    /// The sidecar path for a given WAV path (`foo.wav` → `foo.modenv`).
    #[must_use]
    pub fn sidecar_path(wav: &Path) -> PathBuf {
        wav.with_extension("modenv")
    }

    pub fn write(&self, path: &Path) -> io::Result<()> {
        let mut f = io::BufWriter::new(std::fs::File::create(path)?);
        f.write_all(&MAGIC.to_le_bytes())?;
        f.write_all(&u32::try_from(self.ids.len()).unwrap_or(u32::MAX).to_le_bytes())?;
        f.write_all(&u32::try_from(self.beats.len()).unwrap_or(u32::MAX).to_le_bytes())?;
        for id in &self.ids {
            f.write_all(&id.to_le_bytes())?;
        }
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
            return Err(io::Error::new(io::ErrorKind::InvalidData, "modenv: bad magic"));
        }
        c.read_exact(&mut b4)?;
        let n_sources = u32::from_le_bytes(b4) as usize;
        c.read_exact(&mut b4)?;
        let n_buffers = u32::from_le_bytes(b4) as usize;
        // ヘッダの数だけ信じて `with_capacity` すると、壊れた / 悪意ある
        // sidecar の 4G 行で OOM する。実ファイル長で先に弾く
        // (`tempo_map.rs` の `MAX_TABLE_BEATS` と同じ理由)。
        if n_sources > MAX_MOD_SOURCES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "modenv: n_sources が上限を超えている",
            ));
        }
        let need = 12
            + n_sources
                .saturating_mul(4)
                .saturating_add(n_buffers.saturating_mul(4))
                .saturating_add(n_buffers.saturating_mul(n_sources).saturating_mul(4));
        if bytes.len() < need {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "modenv: ヘッダが宣言した長さに本体が足りない",
            ));
        }
        let mut ids = Vec::with_capacity(n_sources);
        for _ in 0..n_sources {
            c.read_exact(&mut b4)?;
            ids.push(u32::from_le_bytes(b4));
        }
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
        Ok(Self { ids, beats, scalars })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_at_step_holds_and_clamps() {
        let mut s = ModEnvSidecar::new(vec![7, 3]);
        s.push(0.0, &[0.1, 0.2]);
        s.push(1.0, &[0.3, 0.4]);
        s.push(2.0, &[0.5, 0.6]);
        let mut out = ModPlane::default();
        s.sample_at(-1.0, &mut out); // before first → first row
        assert_eq!((out.scalar(7), out.scalar(3)), (0.1, 0.2));
        s.sample_at(1.5, &mut out); // between → last <= 1.5
        assert_eq!((out.scalar(7), out.scalar(3)), (0.3, 0.4));
        s.sample_at(99.0, &mut out); // past end → last row
        assert_eq!((out.scalar(7), out.scalar(3)), (0.5, 0.6));
    }

    #[test]
    fn roundtrip_write_read(// uses a temp path derived from the test name
    ) {
        let dir = std::env::temp_dir();
        let wav = dir.join("daw01_modenv_roundtrip.wav");
        let path = ModEnvSidecar::sidecar_path(&wav);
        let mut s = ModEnvSidecar::new(vec![42]);
        s.push(0.0, &[0.25]);
        s.push(0.5, &[0.75]);
        s.write(&path).unwrap();
        let back = ModEnvSidecar::read(&path).unwrap();
        assert_eq!(s, back);
        let _ = std::fs::remove_file(&path);
    }

    /// r.md #89: **列は id で引く。** 書き出し後にソースを消しても、残った
    /// ソースの値は同じ id で同じ値のまま引ける (位置キーだとずれていた)。
    #[test]
    fn 列は位置ではなく_id_で引ける() {
        let mut s = ModEnvSidecar::new(vec![5, 8, 13]);
        s.push(0.0, &[0.1, 0.2, 0.3]);
        let mut out = ModPlane::default();
        s.sample_at(0.0, &mut out);
        assert_eq!(out.scalar(8), 0.2);
        assert_eq!(out.scalar(13), 0.3);
        // sidecar に無い id は 0 (= 変調なし)。位置キーなら別の列を拾っていた。
        assert_eq!(out.scalar(99), 0.0);
    }

    /// 壊れた / 悪意ある sidecar のヘッダで巨大確保しない。
    #[test]
    fn 壊れたヘッダでは確保せずに落ちる() {
        let dir = std::env::temp_dir();
        let path = dir.join("daw01_modenv_corrupt.modenv");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes()); // n_sources
        bytes.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // n_buffers (嘘)
        std::fs::write(&path, &bytes).unwrap();
        assert!(ModEnvSidecar::read(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
