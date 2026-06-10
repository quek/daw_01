use std::env;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

/// FIXME #26 Phase B: daw_plugin_host を `--probe-vst3` 使い捨てプロセスとして
/// 起動し、 VST3 が note-effect (= ノート入出力あり・音声出力なし) か判定する。
/// **sync** (= rescan の std::thread から呼ぶので tokio Command は使わない)。
/// timeout (= ハングする VST3) / spawn 失敗 / 異常終了 / parse 失敗はすべて
/// **false** で fallback する — 壊れたプラグインで scan 本体を巻き込まず、 note
/// 判定が付かないだけ (= FX 扱い、 Phase B 前の挙動) なので退行しない。
pub fn probe_vst3_note_effect(vst3_path: &Path, target_id: &str) -> bool {
    const TIMEOUT: Duration = Duration::from_secs(8);
    let Ok(exe) = resolve_sibling_binary("daw_plugin_host") else {
        return false;
    };
    let spawned = std::process::Command::new(&exe)
        .args([
            "--probe-vst3",
            &vst3_path.display().to_string(),
            target_id,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(
                        path = %vst3_path.display(),
                        "VST3 note-effect probe timed out; treating as non-note-effect"
                    );
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return false,
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    out.contains("note_effect=true")
}

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
