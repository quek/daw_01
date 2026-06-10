//! Audio file import: hash-based dedup, project-dir copy, WAV decode.
//!
//! Pipeline (spec `docs/plan_audio_clip.md` §3.1.1, §7):
//!
//! 1. Compute SHA-256 of the source file (first 4 bytes → 8 hex chars
//!    used as a dedup key in the `samples/` filename).
//! 2. Copy the file into `<project_dir>/samples/<basename>_<hash>.<ext>`
//!    if not already present. Unsaved projects fall back to a per-
//!    session import_cache directory; saving the project later moves
//!    those files into the real `samples/` dir.
//! 3. Decode with `hound` into a planar `AudioSourceBuffer`.
//! 4. Build the `AudioSource` model entry referencing
//!    `AudioSourcePath::ProjectRelative("samples/<filename>")`.
//!
//! Decode runs on a background thread; completion is delivered to the
//! GUI via `EventLoopProxy::send_event(AppEvent::AudioImported { ... })`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use common::model::{AudioSource, AudioSourcePath};
use sha2::{Digest, Sha256};

use crate::audio_source_cache::AudioSourceBuffer;

/// Maximum WAV file size accepted on import (Phase 1 cap, §7.2 = 4 GiB).
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Successful decode result. The `source.path` is already populated
/// (`ProjectRelative` or `Absolute` depending on whether the import
/// went into the project samples dir or the unsaved-project cache).
/// `display_name` is the *original* file stem (no hash suffix), used
/// as the default clip name so the user sees what they dropped — the
/// hashed `samples/` filename never surfaces in the UI.
pub struct ImportedAudio {
    pub buffer: Arc<AudioSourceBuffer>,
    pub source: AudioSource,
    pub display_name: String,
}

#[derive(Debug)]
pub enum ImportError {
    UnsupportedFormat(String),
    TooLarge { actual: u64, limit: u64 },
    DecodeFailed(String),
    IoError(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::UnsupportedFormat(ext) => write!(
                f,
                "Unsupported audio format: .{ext} (Phase 1: WAV only)"
            ),
            ImportError::TooLarge { actual, limit } => write!(
                f,
                "Audio file too large: {actual} bytes (limit {limit} bytes)"
            ),
            ImportError::DecodeFailed(s) => write!(f, "Decode failed: {s}"),
            ImportError::IoError(s) => write!(f, "I/O error: {s}"),
        }
    }
}

impl std::error::Error for ImportError {}

/// SHA-256 prefix (8 hex chars / 4 bytes) of the file's full contents.
/// Crypto-strength hash because we use it for content addressing and
/// dedup; 4 bytes is enough for hundreds of imports without collision
/// concerns.
pub fn file_hash8(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut s = String::with_capacity(8);
    for b in &digest[..4] {
        use std::fmt::Write;
        write!(&mut s, "{b:02x}").unwrap();
    }
    Ok(s)
}

/// Sanitize a basename: keep ASCII alphanumerics, `_`, `-`. Anything
/// else → `_`. Empty stem (e.g. `.wav`) becomes `audio`.
fn sanitize_stem(stem: &str) -> String {
    let s: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "audio".into() } else { s }
}

/// Build the `<project_dir>/samples/<basename>_<hash>.<ext>` filename
/// for a given source file.
pub fn samples_filename(src: &Path, hash8: &str) -> String {
    let stem = sanitize_stem(
        src.file_stem().and_then(|s| s.to_str()).unwrap_or("audio"),
    );
    let ext = src
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("wav");
    format!("{stem}_{hash8}.{ext}")
}

/// Copy `src` into `<dest_dir>/<filename>` if not already present, and
/// return the absolute destination path. `dest_dir` must already exist
/// or be creatable (we `create_dir_all`).
pub fn copy_into_dir(src: &Path, dest_dir: &Path, filename: &str) -> Result<PathBuf> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("create_dir_all {}", dest_dir.display()))?;
    let dst = dest_dir.join(filename);
    if !dst.exists() {
        fs::copy(src, &dst).with_context(|| {
            format!("copy {} -> {}", src.display(), dst.display())
        })?;
    }
    Ok(dst)
}

/// Decode a WAV file (mono / stereo, 16/24/32-bit PCM, f32/f64) into
/// a planar `AudioSourceBuffer`. Phase 1 supports WAV only (§7.1).
pub fn decode_wav(path: &Path) -> Result<AudioSourceBuffer, ImportError> {
    use hound::SampleFormat;

    let metadata = fs::metadata(path)
        .map_err(|e| ImportError::IoError(format!("{}: {}", path.display(), e)))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ImportError::TooLarge {
            actual: metadata.len(),
            limit: MAX_FILE_BYTES,
        });
    }

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if ext != "wav" {
        return Err(ImportError::UnsupportedFormat(ext));
    }

    let mut reader = hound::WavReader::open(path)
        .map_err(|e| ImportError::DecodeFailed(format!("open: {e}")))?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let channels = spec.channels;
    if channels == 0 {
        return Err(ImportError::DecodeFailed("channels = 0".into()));
    }
    if sample_rate == 0 {
        return Err(ImportError::DecodeFailed("sample_rate = 0".into()));
    }
    let frames = reader.duration() as u64;

    let mut planar: Vec<Vec<f32>> = (0..channels as usize)
        .map(|_| Vec::with_capacity(frames as usize))
        .collect();

    match spec.sample_format {
        SampleFormat::Float => {
            for (idx, sample) in reader.samples::<f32>().enumerate() {
                let s = sample
                    .map_err(|e| ImportError::DecodeFailed(format!("read f32: {e}")))?;
                let ch = idx % channels as usize;
                planar[ch].push(s);
            }
        }
        SampleFormat::Int => {
            // Normalise to [-1, 1] using the dynamic range of the
            // source bit depth (16 / 24 / 32).
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            for (idx, sample) in reader.samples::<i32>().enumerate() {
                let s = sample
                    .map_err(|e| ImportError::DecodeFailed(format!("read i32: {e}")))?;
                let ch = idx % channels as usize;
                planar[ch].push(s as f32 / max_val);
            }
        }
    }

    Ok(AudioSourceBuffer {
        sample_rate,
        channels,
        frames,
        samples: planar,
    })
}

/// Where to copy import files when there is no project_dir yet. Callers
/// are expected to thread the chosen directory through and migrate the
/// files into `<project_dir>/samples/` on the next save (see
/// [`migrate_unsaved_audio_sources_into`]).
pub fn unsaved_import_cache_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("daw_01").join("import_cache")
}

/// Move every `AudioSource` whose path is an `Absolute` pointing into
/// the unsaved-project import cache into `<project_dir>/samples/` and
/// rewrite the path to `ProjectRelative("samples/<filename>")`. Called
/// from the GUI's save flow so that on first save (or save-as from an
/// in-memory-only state) every imported audio file lands inside the
/// project bundle (`docs/plan_audio_clip.md` §13 Q2).
///
/// `Absolute` entries that point *outside* the import cache are left
/// alone (= future "link to external sample" use case). `ProjectRelative`
/// and `Generated` entries are no-ops here.
///
/// Returns the number of sources actually migrated. Errors propagate
/// from filesystem operations (create_dir_all / rename / copy).
pub fn migrate_unsaved_audio_sources_into(
    song: &mut common::model::Song,
    project_dir: &Path,
) -> Result<usize> {
    migrate_unsaved_cache_into(
        song,
        project_dir,
        &unsaved_import_cache_dir(),
        "samples",
    )
}

/// Where Bounce In Place / Bounce (with FX) write WAVs in unsaved
/// projects. Mirror of [`unsaved_import_cache_dir`] for bounce output.
/// Saving the project later moves the files into `<project_dir>/bounce/`
/// (see [`migrate_unsaved_bounce_sources_into`]).
pub fn unsaved_bounce_cache_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("daw_01").join("bounce_cache")
}

/// Mirror of [`migrate_unsaved_audio_sources_into`] for the bounce
/// cache: move every `AudioSource` whose path is an `Absolute`
/// pointing into the unsaved-project bounce cache into
/// `<project_dir>/bounce/` and rewrite the path to
/// `ProjectRelative("bounce/<filename>")`. Called from the GUI save
/// flow so that on first save every bounced WAV lands inside the
/// project bundle (= same destination as bounces written when a
/// project_dir was already known).
///
/// `Absolute` entries that point *outside* the bounce cache are left
/// alone (e.g. import cache entries are migrated by the sibling
/// `migrate_unsaved_audio_sources_into`).
pub fn migrate_unsaved_bounce_sources_into(
    song: &mut common::model::Song,
    project_dir: &Path,
) -> Result<usize> {
    migrate_unsaved_cache_into(
        song,
        project_dir,
        &unsaved_bounce_cache_dir(),
        "bounce",
    )
}

/// Plan an import_cache → samples/ (or bounce_cache → bounce/) migration for
/// `song` **without touching the filesystem**. Walks every `AudioSource`,
/// matches `Absolute` paths under `cache_root`, rewrites each to
/// `ProjectRelative(dst_subdir / <filename>)` **in place**, and returns the
/// list of physical `(cache_abs, dst_abs)` moves that committing requires.
///
/// Splitting the path rewrite (pure, reversible by dropping the song) from the
/// physical move (destructive) lets the save flow serialize the project file
/// *first* and only [`commit_migration`] the moves once that write succeeds —
/// so a failed serialize never leaves audio files half-moved out of the cache.
pub fn plan_unsaved_audio_migration(
    song: &mut common::model::Song,
    project_dir: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    plan_unsaved_cache_migration(song, project_dir, &unsaved_import_cache_dir(), "samples")
}

/// [`plan_unsaved_audio_migration`] for the bounce cache (→ `bounce/`).
pub fn plan_unsaved_bounce_migration(
    song: &mut common::model::Song,
    project_dir: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    plan_unsaved_cache_migration(song, project_dir, &unsaved_bounce_cache_dir(), "bounce")
}

fn plan_unsaved_cache_migration(
    song: &mut common::model::Song,
    project_dir: &Path,
    cache_root: &Path,
    dst_subdir: &str,
) -> Vec<(PathBuf, PathBuf)> {
    let dst_dir = project_dir.join(dst_subdir);
    let mut moves = Vec::new();
    for source in song.audio_sources.values_mut() {
        let abs = match &source.path {
            AudioSourcePath::Absolute(p) => p.clone(),
            _ => continue,
        };
        if !abs.starts_with(cache_root) {
            continue;
        }
        let Some(filename) = abs.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // filename borrows abs; finish computing the owned dst / rel paths
        // before moving abs into the plan.
        let dst = dst_dir.join(filename);
        let rel = PathBuf::from(dst_subdir).join(filename);
        source.path = AudioSourcePath::ProjectRelative(rel);
        moves.push((abs, dst));
    }
    moves
}

/// Execute the physical moves planned by `plan_unsaved_*_migration`. Call this
/// **after** the project file has been written successfully. Idempotent: if the
/// destination already exists (dedup, or a prior plan already moved it) the
/// cache copy is dropped and the move is skipped. Errors propagate; the save
/// flow logs + surfaces them but keeps going (the affected source is treated as
/// missing rather than aborting the whole save).
pub fn commit_migration(moves: &[(PathBuf, PathBuf)]) -> Result<()> {
    for (abs, dst) in moves {
        if let Some(dst_dir) = dst.parent() {
            fs::create_dir_all(dst_dir).with_context(|| {
                format!("create_dir_all {}", dst_dir.display())
            })?;
        }
        if dst.exists() {
            // Same content already present (= dedup hit or prior migration).
            let _ = fs::remove_file(abs);
        } else if fs::rename(abs, dst).is_err() {
            // rename within the same volume is atomic; fall back to
            // copy + remove for cross-volume imports.
            fs::copy(abs, dst).with_context(|| {
                format!("copy {} -> {}", abs.display(), dst.display())
            })?;
            let _ = fs::remove_file(abs);
        }
    }
    Ok(())
}

/// Plan **and immediately commit** a cache migration for `song` (= the
/// historical "rewrite paths + move files now" behavior). Used for the live
/// working song in the save flow, *after* a successful serialize.
fn migrate_unsaved_cache_into(
    song: &mut common::model::Song,
    project_dir: &Path,
    cache_root: &Path,
    dst_subdir: &str,
) -> Result<usize> {
    let moves = plan_unsaved_cache_migration(song, project_dir, cache_root, dst_subdir);
    let moved = moves.len();
    commit_migration(&moves)?;
    Ok(moved)
}

/// One-shot helper: hash → copy → decode → build `AudioSource` model.
///
/// `project_dir = Some(dir)`: copy goes to `<dir>/samples/`, recorded as
/// `AudioSourcePath::ProjectRelative("samples/<filename>")`.
/// `project_dir = None`: copy goes to the unsaved-project cache,
/// recorded as `AudioSourcePath::Absolute(absolute_cache_path)`.
pub fn import_one(
    src: &Path,
    project_dir: Option<&Path>,
) -> Result<ImportedAudio, ImportError> {
    // Decode first so we surface format / size errors before we bother
    // hashing or copying anything.
    let buffer = decode_wav(src)?;

    let hash8 = file_hash8(src)
        .map_err(|e| ImportError::IoError(format!("hash {}: {}", src.display(), e)))?;
    let filename = samples_filename(src, &hash8);

    let path_kind = match project_dir {
        Some(dir) => {
            let samples_dir = dir.join("samples");
            copy_into_dir(src, &samples_dir, &filename).map_err(|e| {
                ImportError::IoError(format!(
                    "copy into samples/: {e}",
                ))
            })?;
            AudioSourcePath::ProjectRelative(PathBuf::from("samples").join(&filename))
        }
        None => {
            let cache = unsaved_import_cache_dir();
            let dst = copy_into_dir(src, &cache, &filename).map_err(|e| {
                ImportError::IoError(format!("copy into import_cache: {e}"))
            })?;
            AudioSourcePath::Absolute(dst)
        }
    };

    let source = AudioSource {
        path: path_kind,
        sample_rate: buffer.sample_rate,
        channels: buffer.channels,
        frames: buffer.frames,
        original_bpm: None,
        root_key: None,
    };

    let display_name = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Audio Clip")
        .to_string();

    Ok(ImportedAudio {
        buffer: Arc::new(buffer),
        source,
        display_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use tempfile::tempdir;

    fn write_test_wav(path: &Path, frames: usize, channels: u16, sample_rate: u32) {
        let spec = WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(path, spec).unwrap();
        for f in 0..frames {
            for _ in 0..channels {
                let v = ((f as i32) % 32_000) as i16;
                writer.write_sample(v).unwrap();
            }
        }
        writer.finalize().unwrap();
    }

    fn mk_source(path: AudioSourcePath) -> AudioSource {
        AudioSource {
            path,
            sample_rate: 48_000,
            channels: 2,
            frames: 1,
            original_bpm: None,
            root_key: None,
        }
    }

    /// atomicity の核: `plan_unsaved_cache_migration` は path を ProjectRelative へ
    /// 書き換えるが **ファイルを動かさない**。 実際の move は `commit_migration` が
    /// 行う。 これにより save flow は serialize 成功後にのみ commit でき、 書き出し
    /// 失敗時は plan を捨てれば import_cache のファイルが無傷で残る。
    #[test]
    fn plan_rewrites_paths_without_moving_files_then_commit_moves() {
        let cache = tempdir().unwrap();
        let proj = tempdir().unwrap();
        let src = cache.path().join("foo.wav");
        write_test_wav(&src, 8, 1, 48_000);

        let mut song = common::model::Song::default();
        song.audio_sources
            .insert(1, mk_source(AudioSourcePath::Absolute(src.clone())));

        // plan: path だけ書き換え、 ファイルは cache に残る (I/O なし)。
        let moves =
            plan_unsaved_cache_migration(&mut song, proj.path(), cache.path(), "samples");
        assert_eq!(moves.len(), 1, "one move planned");
        assert!(src.exists(), "plan must NOT move the file");
        assert!(
            matches!(
                &song.audio_sources[&1].path,
                AudioSourcePath::ProjectRelative(p)
                    if p == &PathBuf::from("samples").join("foo.wav")
            ),
            "plan rewrites path to ProjectRelative(samples/foo.wav)"
        );

        // commit: 実際に move する。
        commit_migration(&moves).unwrap();
        assert!(!src.exists(), "commit moved the file out of the cache");
        assert!(
            proj.path().join("samples").join("foo.wav").exists(),
            "file now lives under <project>/samples/"
        );
    }

    /// `commit_migration` は dst が既存 (dedup / 先行 plan が move 済み) のとき
    /// cache コピーを落とすだけで二重 move しない。
    #[test]
    fn commit_dedups_when_destination_exists() {
        let cache = tempdir().unwrap();
        let proj = tempdir().unwrap();
        let src = cache.path().join("bar.wav");
        let dst_dir = proj.path().join("samples");
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("bar.wav");
        write_test_wav(&src, 8, 1, 48_000);
        write_test_wav(&dst, 8, 1, 48_000); // dst が既に存在

        commit_migration(&[(src.clone(), dst.clone())]).unwrap();
        assert!(!src.exists(), "cache copy dropped on dedup");
        assert!(dst.exists(), "existing destination is kept");
    }

    #[test]
    fn decode_wav_returns_planar_buffer_for_stereo_pcm16() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_test_wav(&path, 1024, 2, 48_000);
        let buf = decode_wav(&path).unwrap();
        assert_eq!(buf.sample_rate, 48_000);
        assert_eq!(buf.channels, 2);
        assert_eq!(buf.frames, 1024);
        assert_eq!(buf.samples.len(), 2);
        assert_eq!(buf.samples[0].len(), 1024);
        assert_eq!(buf.samples[1].len(), 1024);
    }

    #[test]
    fn decode_rejects_non_wav_extension() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.flac");
        fs::write(&path, b"\0\0\0\0").unwrap();
        let err = decode_wav(&path).unwrap_err();
        assert!(matches!(err, ImportError::UnsupportedFormat(_)));
    }

    #[test]
    fn import_one_copies_into_samples_dir_with_hash() {
        let dir = tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src = dir.path().join("kick.wav");
        write_test_wav(&src, 512, 1, 44_100);

        let imported = import_one(&src, Some(&project)).unwrap();
        // ProjectRelative path
        match &imported.source.path {
            AudioSourcePath::ProjectRelative(p) => {
                assert!(p.starts_with("samples"));
                assert!(
                    p.to_string_lossy().contains("kick_"),
                    "filename should contain sanitized stem"
                );
            }
            other => panic!("expected ProjectRelative, got {other:?}"),
        }
        // file actually copied
        let samples_dir = project.join("samples");
        let entries: Vec<_> = std::fs::read_dir(&samples_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn import_one_dedups_same_content() {
        let dir = tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src = dir.path().join("kick.wav");
        write_test_wav(&src, 256, 1, 44_100);

        let _ = import_one(&src, Some(&project)).unwrap();
        let _ = import_one(&src, Some(&project)).unwrap();
        // Two imports of the same file → still 1 entry in samples/
        let entries: Vec<_> = std::fs::read_dir(project.join("samples"))
            .unwrap()
            .collect();
        assert_eq!(entries.len(), 1, "dedup should leave 1 file");
    }

    #[test]
    fn import_one_unsaved_project_uses_absolute_cache_path() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("kick.wav");
        write_test_wav(&src, 256, 1, 44_100);

        let imported = import_one(&src, None).unwrap();
        assert!(matches!(
            imported.source.path,
            AudioSourcePath::Absolute(_)
        ));
    }
}
