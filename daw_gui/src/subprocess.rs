// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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

/// Windows: 子プロセスのコンソール窓を抑制する creation flag。 release では
/// `CREATE_NO_WINDOW` (親が windows-subsystem で console を持たないとき、
/// console-subsystem の子が新しいコンソール窓を開くのを防ぐ belt-and-suspenders。
/// 子も release では windows-subsystem 化済み)。 debug では 0 (= フラグ無し。
/// 子を standalone 起動したとき stdout/tracing が見える)。 どちらでも
/// `Stdio::piped()` の stdout 取得は壊れない。 docs/plan_icon_and_console.md (#48)。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CHILD_CREATION_FLAGS: u32 = if cfg!(debug_assertions) { 0 } else { CREATE_NO_WINDOW };

/// 子プロセスを起動し、 `timeout` 内の完了を待ちつつ **stdout を専用スレッドで
/// 読み切って** 文字列で返す。 stdout を wait 完了後にまとめて読む naive 実装は、
/// 子の出力が OS の pipe buffer (~4-64KB) を超えると子が `write()` でブロックして
/// 終了できず、 `try_wait` が永久に `Ok(None)` を返して timeout まで deadlock する
/// (plugin DB JSON は容易に超える)。 並行 reader が buffer を drain し続けることで
/// これを回避する。 `cmd` の stdout / stderr / creation_flags は本関数が設定する。
/// timeout / spawn 失敗 / I/O 異常はすべて `None`。
fn run_capture_stdout(mut cmd: std::process::Command, timeout: Duration, label: &str) -> Option<String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CHILD_CREATION_FLAGS);
    }
    let mut child = cmd.spawn().ok()?;
    // stdout を take して専用スレッドで drain する (pipe-buffer deadlock 回避)。
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(label, "child process timed out");
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break;
            }
        }
    }
    // reader は子の stdout が閉じると (正常終了 or kill) 必ず戻る。
    let out = reader.join().ok();
    if timed_out { None } else { out }
}

/// daw_plugin_host を `--probe-vst3` / `--probe-clap` 使い捨て
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
    let mut cmd = std::process::Command::new(&exe);
    cmd.args([flag, &path.display().to_string(), target_id]);
    // stdout を並行 drain して読む (pipe-buffer deadlock 回避、 scan と共通経路)。
    let out = run_capture_stdout(cmd, TIMEOUT, "plugin port probe")?;
    out.lines().find_map(PortConfig::parse_line)
}

/// daw_plugin_host を `--scan-plugins` 使い捨てプロセスとして起動し、システムのプラグイン DB
/// (builtin + CLAP + VST3 の enumerated descriptors) を得る。**sync** (cold-start / rescan の
/// `std::thread` から呼ぶ)。プラグイン DLL の実ロードは **このサブプロセス** が行い、GUI プロセスは
/// dlopen しない (arch-refactor S5-3。probe subprocess と同じ crash 隔離)。timeout / spawn 失敗 /
/// 異常終了 / JSON parse 失敗はすべて `None` を返す (呼び元は builtin fallback で退行しない)。
pub fn scan_plugins() -> Option<common::plugin_db::PluginDatabase> {
    // scan は多数の DLL を load するので probe より長い timeout。
    const TIMEOUT: Duration = Duration::from_secs(120);
    let exe = resolve_sibling_binary("daw_plugin_host").ok()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["--scan-plugins"]);
    // stdout を並行 drain して読む。 旧実装は wait 完了後に read していたため、
    // plugin DB JSON が pipe buffer を超える (プラグイン多数の) 環境で子が write で
    // block → timeout まで cold-start が deadlock していた (H1)。
    let out = run_capture_stdout(cmd, TIMEOUT, "plugin scan")?;
    // scan は最後に DB を 1 行 JSON で出す。 雑音行を弾いて JSON 行を探す (probe と同 idiom)。
    out.lines()
        .find_map(|line| serde_json::from_str::<common::plugin_db::PluginDatabase>(line).ok())
}

pub fn spawn_sibling<I, S>(name: &str, args: I) -> Result<Child>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let path = resolve_sibling_binary(name)?;
    tracing::info!(binary = %path.display(), "spawning child process");
    let mut cmd = Command::new(&path);
    cmd.args(args);
    // tokio::process::Command::creation_flags は Windows の inherent method
    // (trait import 不要)。 tokio が CREATE_UNICODE_ENVIRONMENT を OR する。
    #[cfg(windows)]
    cmd.creation_flags(CHILD_CREATION_FLAGS);
    cmd.spawn()
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
