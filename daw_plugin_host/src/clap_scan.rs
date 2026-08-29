use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Re-entry safety upper bound for the directory recursion. CLAP install
/// trees are flat (1-2 levels of vendor folder); 8 absorbs symlink loops
/// without imposing a real limit.
const MAX_DEPTH: u8 = 8;

/// Enumerates CLAP plugins in **every** location CLAP's `entry.h` defines
/// (see [`crate::plugin_paths`] for the verbatim spec text): the system-wide
/// and per-user Common Files trees, plus everything listed in the
/// `CLAP_PATH` environment variable — which the spec makes a host's
/// obligation ("a CLAP host **must** query the environment for a CLAP_PATH
/// variable"), not a suggestion.
///
/// **Recurses into subdirectories** (vendor folders are common, e.g.
/// `…\CLAP\Surge XT\Surge XT.clap`). Per CLAP entry.h hosts *should*
/// recursively scan the standard locations.
///
/// Roots that don't exist are not an error — they simply aren't installed.
/// A root that exists but can't be read *is* reported (logged and skipped)
/// so "no plugins" and "couldn't look" stay distinguishable.
pub fn scan_system_clap_directory() -> Result<Vec<PathBuf>> {
    let roots = crate::plugin_paths::clap_search_roots();
    if roots.is_empty() {
        tracing::info!("no CLAP search root exists on this machine");
        return Ok(Vec::new());
    }
    let mut plugins = Vec::new();
    for root in &roots {
        match scan_directory(root) {
            Ok(found) => plugins.extend(found),
            Err(e) => tracing::warn!(
                error = ?e,
                path = %root.display(),
                "CLAP search root scan failed, skipping"
            ),
        }
    }
    plugins.sort();
    // 同じ .clap が 2 つのルートから見えるとピッカーに二重登録される。
    // ルートが 1 本だった頃は起きなかったので、 ここで初めて必要になる。
    plugins.dedup();
    Ok(plugins)
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
