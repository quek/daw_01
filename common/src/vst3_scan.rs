//! VST3 plugin discovery on Windows.
//!
//! The VST3 SDK (since 3.6.10) specifies that plugins ship as bundles —
//! directories with a `.vst3` extension containing the actual DLL inside a
//! platform-specific subfolder (`Contents/x86_64-win/<name>.vst3` on
//! Windows). A handful of legacy plugins still ship as single `.vst3` DLLs
//! placed directly under `Common Files\VST3`, so we support both shapes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Discovered VST3 entry: the folder/file the user sees plus the actual
/// DLL libloading should load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vst3Entry {
    /// The `.vst3` bundle directory (or single DLL on legacy installs).
    /// This is what gets persisted in `PluginEntry.path` so projects stay
    /// portable across machines that happen to keep the same bundle name.
    pub bundle_path: PathBuf,
    /// Absolute path to the PE32+ DLL inside the bundle (or equal to
    /// `bundle_path` for the legacy single-DLL layout).
    pub dll_path: PathBuf,
}

/// Scans `%COMMONPROGRAMFILES%\VST3` (or the default `C:\Program Files\Common Files\VST3`)
/// non-recursively for `.vst3` entries and resolves each one's DLL path.
/// Individual unresolvable entries are logged and skipped.
pub fn scan_system_vst3_directory() -> Result<Vec<Vst3Entry>> {
    let common_files = std::env::var_os("COMMONPROGRAMFILES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Common Files"));
    let vst3_dir = common_files.join("VST3");
    scan_directory(&vst3_dir)
}

fn scan_directory(dir: &Path) -> Result<Vec<Vst3Entry>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?;
    let mut plugins = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "vst3") {
            continue;
        }
        match resolve_vst3_dll(&path) {
            Ok(dll) => plugins.push(Vst3Entry {
                bundle_path: path,
                dll_path: dll,
            }),
            Err(e) => {
                tracing::warn!(error = ?e, path = %path.display(), "VST3 entry unresolved, skipping");
            }
        }
    }
    plugins.sort_by(|a, b| a.bundle_path.cmp(&b.bundle_path));
    Ok(plugins)
}

/// Returns the actual DLL path for the given `.vst3` bundle or legacy file.
///
/// Bundle layout (Windows x86_64):
///   `<name>.vst3/Contents/x86_64-win/<name>.vst3`
///
/// Legacy: `<name>.vst3` directly as a single PE32+ DLL.
pub fn resolve_vst3_dll(path: &Path) -> Result<PathBuf> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    if meta.is_file() {
        return Ok(path.to_path_buf());
    }
    anyhow::ensure!(
        meta.is_dir(),
        "{} is neither a .vst3 DLL nor a bundle directory",
        path.display()
    );
    let Some(stem) = path.file_stem() else {
        anyhow::bail!("bundle {} has no file stem", path.display());
    };
    let mut dll = stem.to_os_string();
    dll.push(".vst3");
    let candidate = path
        .join("Contents")
        .join("x86_64-win")
        .join(&dll);
    anyhow::ensure!(
        candidate.exists(),
        "expected {} inside VST3 bundle {} but did not find it",
        candidate.display(),
        path.display()
    );
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolves_legacy_single_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.vst3");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(resolve_vst3_dll(&path).unwrap(), path);
    }

    #[test]
    fn resolves_bundle_layout() {
        let dir = tempdir().unwrap();
        let bundle = dir.path().join("foo.vst3");
        let dll_dir = bundle.join("Contents").join("x86_64-win");
        std::fs::create_dir_all(&dll_dir).unwrap();
        let dll = dll_dir.join("foo.vst3");
        std::fs::write(&dll, b"").unwrap();
        assert_eq!(resolve_vst3_dll(&bundle).unwrap(), dll);
    }

    #[test]
    fn missing_bundle_dll_errors() {
        let dir = tempdir().unwrap();
        let bundle = dir.path().join("broken.vst3");
        std::fs::create_dir_all(bundle.join("Contents").join("x86_64-win")).unwrap();
        assert!(resolve_vst3_dll(&bundle).is_err());
    }
}
