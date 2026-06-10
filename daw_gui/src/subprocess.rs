use std::env;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use common::plugin_format::PluginFormat;
use common::port_config::PortConfig;
use tokio::process::{Child, Command};

/// FIXME #26/#29: daw_plugin_host を `--probe-vst3` / `--probe-clap` 使い捨て
/// プロセスとして起動し、 プラグインの port 構成 (note in/out・audio out) を読む。
/// **sync** (= rescan の std::thread から呼ぶので tokio Command は使わない)。
/// timeout (= ハングするプラグイン) / spawn 失敗 / 異常終了 / parse 失敗はすべて
/// **`None`** を返す — 呼び元は scan-time 暫定値を保持するので退行しない。 probe は
/// 成功時のみ port 行を 1 行出すので、 ログ等の雑音行は `parse_line` が弾く。
/// builtin は code が SSoT なので probe しない (`None`)。
pub fn probe_plugin_ports(
    format: PluginFormat,
    path: &Path,
    target_id: &str,
) -> Option<PortConfig> {
    const TIMEOUT: Duration = Duration::from_secs(8);
    let flag = match format {
        PluginFormat::Vst3 => "--probe-vst3",
        PluginFormat::Clap => "--probe-clap",
        PluginFormat::Builtin => return None,
    };
    let exe = resolve_sibling_binary("daw_plugin_host").ok()?;
    let mut child = std::process::Command::new(&exe)
        .args([flag, &path.display().to_string(), target_id])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(
                        path = %path.display(),
                        "plugin port probe timed out; keeping scan-time defaults"
                    );
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    out.lines().find_map(PortConfig::parse_line)
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
