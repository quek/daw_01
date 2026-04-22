use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Enumerates CLAP plugins installed in the system-wide Common Files directory.
/// Does not recurse into subdirectories (MVP scope).
pub fn scan_system_clap_directory() -> Result<Vec<PathBuf>> {
    let common_files = std::env::var_os("COMMONPROGRAMFILES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Common Files"));
    let clap_dir = common_files.join("CLAP");
    scan_directory(&clap_dir)
}

fn scan_directory(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?;
    let mut plugins = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "clap") && entry.file_type()?.is_file() {
            plugins.push(path);
        }
    }
    plugins.sort();
    Ok(plugins)
}
