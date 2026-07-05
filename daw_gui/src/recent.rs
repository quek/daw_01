//! Recent-files list persisted to `%LOCALAPPDATA%\daw_01\recent.json`
//! (or the platform equivalent via `dirs`). Plain JSON to keep the file
//! readable / hand-editable; bincode would be overkill for ~5 paths.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Maximum entries kept in the list. Matches the typical "Open Recent"
/// menu size in mainstream DAWs.
pub const MAX_RECENT: usize = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecentFiles {
    pub paths: Vec<PathBuf>,
}

impl RecentFiles {
    /// Add `path` to the front of the list, removing any existing entry
    /// with the same canonicalised value so duplicates don't accumulate.
    /// Truncates the list to [`MAX_RECENT`] afterwards.
    ///
    /// Dedup keys off [`std::fs::canonicalize`], which resolves symlinks and
    /// `.`/`..` so equivalent paths collapse to one entry. When canonicalize
    /// fails (e.g. the file no longer exists), we fall back to comparing the
    /// raw `PathBuf`s: this is *best-effort* — two distinct spellings of the
    /// same missing file (e.g. relative vs absolute) may not be deduplicated.
    pub fn push(&mut self, path: PathBuf) {
        let canon =
            std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        self.paths.retain(|p| {
            let pc = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            pc != canon
        });
        self.paths.insert(0, path);
        if self.paths.len() > MAX_RECENT {
            self.paths.truncate(MAX_RECENT);
        }
    }
}

pub fn load(path: impl AsRef<Path>) -> Result<RecentFiles> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(RecentFiles::default());
    }
    let text = std::fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(RecentFiles::default());
    }
    Ok(serde_json::from_str(&text)?)
}

pub fn save(path: impl AsRef<Path>, list: &RecentFiles) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(list)?;
    std::fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn push_dedupes_and_caps_at_max() {
        let mut r = RecentFiles::default();
        for i in 0..MAX_RECENT + 5 {
            r.push(PathBuf::from(format!("p{i}.daw")));
        }
        assert_eq!(r.paths.len(), MAX_RECENT);
        // Most recently pushed lives at index 0.
        assert_eq!(
            r.paths[0],
            PathBuf::from(format!("p{}.daw", MAX_RECENT + 4))
        );
    }

    #[test]
    fn push_existing_path_moves_it_to_front() {
        let mut r = RecentFiles::default();
        r.push(PathBuf::from("a.daw"));
        r.push(PathBuf::from("b.daw"));
        r.push(PathBuf::from("a.daw"));
        assert_eq!(r.paths.len(), 2);
        assert_eq!(r.paths[0], PathBuf::from("a.daw"));
        assert_eq!(r.paths[1], PathBuf::from("b.daw"));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("recent.json");
        let mut r = RecentFiles::default();
        r.push(PathBuf::from("foo.daw"));
        r.push(PathBuf::from("bar.daw"));
        save(&path, &r).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.paths, r.paths);
    }

    #[test]
    fn load_returns_default_when_file_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let r = load(&path).unwrap();
        assert!(r.paths.is_empty());
    }
}
