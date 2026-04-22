use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

pub fn spawn_sibling<I, S>(name: &str, args: I) -> Result<Child>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let path = resolve_sibling_binary(name)?;
    tracing::info!(binary = %path.display(), "spawning child process");
    Command::new(&path)
        .args(args)
        .spawn()
        .with_context(|| format!("failed to spawn {}", path.display()))
}

fn resolve_sibling_binary(name: &str) -> Result<PathBuf> {
    let exe = env::current_exe().context("failed to get current_exe")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("current_exe has no parent: {}", exe.display()))?;
    Ok(binary_path_with_ext(dir, name, env::consts::EXE_EXTENSION))
}

fn binary_path_with_ext(dir: &Path, name: &str, exe_ext: &str) -> PathBuf {
    let mut path = dir.join(name);
    if !exe_ext.is_empty() {
        path.set_extension(exe_ext);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_extension_when_non_empty() {
        let path = binary_path_with_ext(Path::new("anywhere"), "daw_audio", "exe");
        assert_eq!(path.file_name(), Some(OsStr::new("daw_audio.exe")));
    }

    #[test]
    fn no_extension_when_empty() {
        let path = binary_path_with_ext(Path::new("anywhere"), "daw_audio", "");
        assert_eq!(path.file_name(), Some(OsStr::new("daw_audio")));
    }

    #[test]
    fn preserves_parent_directory() {
        let path = binary_path_with_ext(Path::new("/some/dir"), "daw_audio", "exe");
        assert_eq!(path.parent(), Some(Path::new("/some/dir")));
    }
}
