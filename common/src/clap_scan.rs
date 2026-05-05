use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Re-entry safety upper bound for the directory recursion. CLAP install
/// trees are flat (1-2 levels of vendor folder); 8 absorbs symlink loops
/// without imposing a real limit.
const MAX_DEPTH: u8 = 8;

/// Enumerates CLAP plugins installed under the system-wide Common Files
/// directory (e.g. `C:\Program Files\Common Files\CLAP` on Windows).
/// **Recurses into subdirectories** (vendor folders are common, e.g.
/// `…\CLAP\Surge XT\Surge XT.clap`). Per CLAP entry.h: hosts should
/// recursively scan the standard locations.
pub fn scan_system_clap_directory() -> Result<Vec<PathBuf>> {
    let common_files = std::env::var_os("COMMONPROGRAMFILES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Common Files"));
    let clap_dir = common_files.join("CLAP");
    scan_directory(&clap_dir)
}

fn scan_directory(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut plugins = Vec::new();
    walk(dir, &mut plugins, 0)?;
    plugins.sort();
    Ok(plugins)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: u8) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_file() && path.extension().is_some_and(|ext| ext == "clap") {
            out.push(path);
        } else if file_type.is_dir() {
            // Per-subdir I/O errors (e.g. access denied) shouldn't kill the
            // whole scan; log and continue.
            if let Err(e) = walk(&path, out, depth + 1) {
                tracing::warn!(
                    error = ?e,
                    path = %path.display(),
                    "CLAP subdirectory scan failed, skipping"
                );
            }
        }
    }
    Ok(())
}

/// Picks a default CLAP plugin path: the `DAW_CLAP_PATH` environment variable
/// if set, otherwise the first entry returned by [`scan_system_clap_directory`].
pub fn default_plugin_path() -> Option<PathBuf> {
    if let Some(env_path) = std::env::var_os("DAW_CLAP_PATH") {
        return Some(PathBuf::from(env_path));
    }
    scan_system_clap_directory().ok()?.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn scan_finds_clap_at_root() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.clap"), b"x").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), b"x").unwrap();
        let v = scan_directory(dir.path()).unwrap();
        assert_eq!(v, vec![dir.path().join("a.clap")]);
    }

    #[test]
    fn scan_recurses_into_vendor_subdir() {
        let dir = tempdir().unwrap();
        let vendor = dir.path().join("VendorX");
        std::fs::create_dir(&vendor).unwrap();
        std::fs::write(vendor.join("p.clap"), b"x").unwrap();
        let v = scan_directory(dir.path()).unwrap();
        assert_eq!(v, vec![vendor.join("p.clap")]);
    }

    #[test]
    fn scan_collects_root_plus_subdir() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("root.clap"), b"x").unwrap();
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("nested.clap"), b"x").unwrap();
        let mut v = scan_directory(dir.path()).unwrap();
        v.sort();
        assert_eq!(
            v,
            vec![nested.join("nested.clap"), dir.path().join("root.clap")]
        );
    }

    #[test]
    fn scan_ignores_non_clap_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.dll"), b"x").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"x").unwrap();
        assert!(scan_directory(dir.path()).unwrap().is_empty());
    }
}
