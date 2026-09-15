//! Audio file import: hash-based dedup, project-dir copy, audio decode.
//!
//! Pipeline (spec `docs/plan_audio_clip.md` §3.1.1, §7):
//!
//! 1. Compute SHA-256 of the source file (first 4 bytes → 8 hex chars
//!    used as a dedup key in the `samples/` filename).
//! 2. Copy the file into `<project_dir>/samples/<basename>_<hash>.<ext>`
//!    if not already present. Unsaved projects use the per-user
//!    import_cache directory ([`crate::media_dest`]); saving the project
//!    later moves those files into the real `samples/` dir.
//! 3. Decode via `common::audio_decode` (symphonia) into a planar
//!    `AudioSourceBuffer` — WAV / AIFF / FLAC / MP3 / OGG / M4A (r.md #19).
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
use common::app_dirs::AppDirs;
use common::model::{AudioSource, AudioSourcePath};
use sha2::{Digest, Sha256};

use crate::audio_source_cache::AudioSourceBuffer;
use crate::media_dest::{MediaDest, MediaPool};

/// Maximum audio file size accepted on import (§7.2 = 4 GiB). Guards the
/// whole-file decode-into-memory against a pathological source, for every
/// format (WAV / AIFF / FLAC / MP3 / OGG / M4A), not just WAV.
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
    /// The file's container/codec could not be recognized or is not built into
    /// this binary's `symphonia` feature set (detail carries the underlying
    /// message + path).
    UnsupportedFormat(String),
    TooLarge { actual: u64, limit: u64 },
    DecodeFailed(String),
    IoError(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::UnsupportedFormat(detail) => write!(
                f,
                "Unsupported or unrecognized audio: {detail}"
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
///
/// 名前は内容 hash で決まるので、既にあれば完成品として使う。だから書きかけを最終名に
/// 出してはいけない ([`common::atomic_file`]): 別プロセスの同じ取り込みが途中の複製を掴む上、
/// 複製中に落ちると壊れたファイルが以後ずっと重複排除で再利用される。
pub fn copy_into_dir(src: &Path, dest_dir: &Path, filename: &str) -> Result<PathBuf> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("create_dir_all {}", dest_dir.display()))?;
    let dst = dest_dir.join(filename);
    common::atomic_file::copy_new(src, &dst)
        .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    Ok(dst)
}

/// Decode an audio file into a planar `AudioSourceBuffer`. Delegates format
/// handling to `common::audio_decode` (symphonia): WAV / AIFF / FLAC / MP3 /
/// OGG-Vorbis / M4A(AAC+ALAC) — the container is detected by content, not by
/// extension (r.md #19). Only the up-front oversize guard is import-specific.
pub fn decode_audio(path: &Path) -> Result<AudioSourceBuffer, ImportError> {
    let metadata = fs::metadata(path)
        .map_err(|e| ImportError::IoError(format!("{}: {}", path.display(), e)))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ImportError::TooLarge {
            actual: metadata.len(),
            limit: MAX_FILE_BYTES,
        });
    }

    let decoded =
        common::audio_decode::decode_audio_file(path).map_err(ImportError::from)?;
    Ok(AudioSourceBuffer {
        origin: path.to_path_buf(),
        sample_rate: decoded.sample_rate,
        channels: decoded.channels,
        frames: decoded.frames,
        samples: decoded.samples,
    })
}

impl From<common::audio_decode::DecodeError> for ImportError {
    fn from(e: common::audio_decode::DecodeError) -> Self {
        use common::audio_decode::DecodeError;
        match e {
            DecodeError::Io(s) => ImportError::IoError(s),
            DecodeError::Unsupported(s) => ImportError::UnsupportedFormat(s),
            DecodeError::Decode(s) => ImportError::DecodeFailed(s),
            DecodeError::Empty => {
                ImportError::DecodeFailed("audio file contained no samples".into())
            }
        }
    }
}

/// Plan an import_cache → samples/ (or bounce_cache → bounce/) migration for
/// `song` **without touching the filesystem**. Walks every `AudioSource`,
/// matches `Absolute` paths under the pool's unsaved cache (resolved from the
/// injected `dirs`, [`MediaPool::unsaved_dir`]), rewrites each to
/// `ProjectRelative(<subdir> / <filename>)` **in place**, and returns the
/// list of physical `(cache_abs, dst_abs)` moves that committing requires.
///
/// Splitting the path rewrite (pure, reversible by dropping the song) from the
/// physical move (destructive) lets the save flow serialize the project file
/// *first* and only [`commit_migration`] the moves once that write succeeds —
/// so a failed serialize never leaves audio files half-moved out of the cache.
pub fn plan_unsaved_audio_migration(
    song: &mut common::model::Song,
    project_dir: &Path,
    dirs: &AppDirs,
) -> Vec<(PathBuf, PathBuf)> {
    plan_unsaved_cache_migration(song, project_dir, dirs, MediaPool::Samples)
}

/// [`plan_unsaved_audio_migration`] for the bounce cache (→ `bounce/`).
pub fn plan_unsaved_bounce_migration(
    song: &mut common::model::Song,
    project_dir: &Path,
    dirs: &AppDirs,
) -> Vec<(PathBuf, PathBuf)> {
    plan_unsaved_cache_migration(song, project_dir, dirs, MediaPool::Bounce)
}

fn plan_unsaved_cache_migration(
    song: &mut common::model::Song,
    project_dir: &Path,
    dirs: &AppDirs,
    pool: MediaPool,
) -> Vec<(PathBuf, PathBuf)> {
    let cache_root = pool.unsaved_dir(dirs);
    let dst_subdir = pool.bundle_subdir();
    let dst_dir = project_dir.join(dst_subdir);
    let mut moves = Vec::new();
    for source in song.media.audio_sources.values_mut() {
        let abs = match &source.path {
            AudioSourcePath::Absolute(p) => p.clone(),
            _ => continue,
        };
        if !abs.starts_with(&cache_root) {
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
            // copy + remove for cross-volume imports. 複製は書きかけを `dst` に出さない —
            // 途中で落ちた `dst` を次の保存が上の `exists` で「移行済み」と見なすと、
            // cache の原本を消して完成品がどこにも残らなくなる。
            common::atomic_file::copy_new(abs, dst).with_context(|| {
                format!("copy {} -> {}", abs.display(), dst.display())
            })?;
            let _ = fs::remove_file(abs);
        }
    }
    Ok(())
}

/// One-shot helper: hash → copy → decode → build `AudioSource` model.
///
/// The copy goes to `dest` ([`MediaDest`]): a saved project's `samples/`
/// (recorded as `ProjectRelative("samples/<filename>")`) or the unsaved-project
/// cache (recorded as `Absolute(absolute_cache_path)`).
pub fn import_one(src: &Path, dest: &MediaDest) -> Result<ImportedAudio, ImportError> {
    // Decode first so we surface format / size errors before we bother
    // hashing or copying anything.
    let mut buffer = decode_audio(src)?;

    let hash8 = file_hash8(src)
        .map_err(|e| ImportError::IoError(format!("hash {}: {}", src.display(), e)))?;
    let filename = samples_filename(src, &hash8);

    let dest_dir = dest.dir();
    let resolved = copy_into_dir(src, &dest_dir, &filename).map_err(|e| {
        ImportError::IoError(format!("copy into {}: {e}", dest_dir.display()))
    })?;
    // `origin` は「この source が **今どこに在るか**」= キャッシュ再利用の同一性。
    // decode 元はユーザーが選んだ元ファイルだが、import はそれを samples/ (or
    // 未保存 project の import cache) へ複製し、以後 Song はそちらを指す。
    // origin を複製先に揃えておかないと、次に同じ project を開いたとき
    // `begin_asset_decode` の照合 (解決済み絶対パス) と食い違って無駄に
    // decode し直すことになる。
    buffer.origin = resolved;

    let source = AudioSource {
        path: dest.audio_path(&filename),
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
        let data = tempdir().unwrap();
        let dirs = AppDirs::under(data.path());
        let proj = tempdir().unwrap();
        fs::create_dir_all(dirs.import_cache_dir()).unwrap();
        let src = dirs.import_cache_dir().join("foo.wav");
        write_test_wav(&src, 8, 1, 48_000);

        let mut song = common::model::Song::default();
        song.media.audio_sources
            .insert(1, mk_source(AudioSourcePath::Absolute(src.clone())));

        // plan: path だけ書き換え、 ファイルは cache に残る (I/O なし)。
        let moves = plan_unsaved_audio_migration(&mut song, proj.path(), &dirs);
        assert_eq!(moves.len(), 1, "one move planned");
        assert!(src.exists(), "plan must NOT move the file");
        assert!(
            matches!(
                &song.media.audio_sources[&1].path,
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
    fn decode_audio_returns_planar_buffer_for_stereo_pcm16() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_test_wav(&path, 1024, 2, 48_000);
        let buf = decode_audio(&path).unwrap();
        assert_eq!(buf.sample_rate, 48_000);
        assert_eq!(buf.channels, 2);
        assert_eq!(buf.frames, 1024);
        assert_eq!(buf.samples.len(), 2);
        assert_eq!(buf.samples[0].len(), 1024);
        assert_eq!(buf.samples[1].len(), 1024);
    }

    #[test]
    fn decode_rejects_unrecognized_content() {
        // A file that is not decodable audio errors regardless of extension
        // (symphonia probes by content, so the `.flac` name does not save it).
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.flac");
        fs::write(&path, b"\0\0\0\0not audio").unwrap();
        let err = decode_audio(&path).unwrap_err();
        assert!(matches!(
            err,
            ImportError::UnsupportedFormat(_) | ImportError::DecodeFailed(_)
        ));
    }

    #[test]
    fn import_one_copies_into_samples_dir_with_hash() {
        let dir = tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let src = dir.path().join("kick.wav");
        write_test_wav(&src, 512, 1, 44_100);

        let imported = import_one(&src, &MediaDest::bundle(&project, MediaPool::Samples)).unwrap();
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

        let dest = MediaDest::bundle(&project, MediaPool::Samples);
        let _ = import_one(&src, &dest).unwrap();
        let _ = import_one(&src, &dest).unwrap();
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
        // ユーザーの実 import_cache ではなく注入した per-user root の下に置く。
        let dirs = AppDirs::under(dir.path().join("appdata"));
        let dest = MediaDest::resolve(MediaPool::Samples, None, Some(&dirs)).unwrap();

        let imported = import_one(&src, &dest).unwrap();
        assert!(matches!(
            &imported.source.path,
            AudioSourcePath::Absolute(p) if p.starts_with(dirs.import_cache_dir())
        ));
    }
}
