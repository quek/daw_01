//! VOICEVOX engine subprocess の起動 / ヘルスチェック helper。
//!
//! 動作概要:
//! 1. **起動済み判定**: `/version` を 1 秒 timeout で叩いて 200 が返れば起動済
//! 2. **path 解決**: env `DAW_VOICEVOX_PATH` → 設定ファイル
//!    `%LOCALAPPDATA%/daw_01/voicevox_engine_path.txt` の順
//! 3. **spawn**: `std::process::Command` で起動 (stdout/stderr は null へ捨てる)。
//!    呼び出し側が `JobHandle::assign_std` で daw_01 終了時の自動 kill を担保
//! 4. **wait_until_ready**: `/version` を 500ms 間隔で polling、 60 秒 timeout
//!
//! 失敗時は何もしない (= ユーザーが手動で起動する fallback)。 daw_gui 起動を
//! 止めない。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::voicevox::VOICEVOX_URL;

const VERSION_PATH: &str = "version";
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const POLL_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_millis(1000);

/// 設定ファイル相対パス (`%LOCALAPPDATA%/daw_01/voicevox_engine_path.txt`)。
pub fn engine_path_config_file() -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("daw_01")
            .join("voicevox_engine_path.txt"),
    )
}

/// 起動 exe を解決する。 優先順:
///
/// 1. env `DAW_VOICEVOX_PATH` (絶対パス)
/// 2. 設定ファイル `voicevox_engine_path.txt` の 1 行目 (前後 trim)
/// 3. **デフォルトインストール先候補** (実体ありの順):
///     - `%LOCALAPPDATA%\Programs\VOICEVOX\VOICEVOX.exe` (ユーザー領域インストール)
///     - `C:\Program Files\VOICEVOX\VOICEVOX.exe` (system インストール)
///
/// どれも該当なしなら `None` (= 起動 skip、 ユーザーが手動で立ち上げる想定)。
pub fn resolve_engine_path() -> Option<PathBuf> {
    if let Some(env_path) = std::env::var_os("DAW_VOICEVOX_PATH") {
        let p = PathBuf::from(env_path);
        if p.exists() {
            return Some(p);
        }
        tracing::warn!(path = %p.display(), "DAW_VOICEVOX_PATH points to non-existent file");
    }
    if let Some(cfg) = engine_path_config_file()
        && let Ok(text) = std::fs::read_to_string(&cfg)
    {
        let p = PathBuf::from(text.trim());
        if p.exists() {
            return Some(p);
        }
        tracing::warn!(
            cfg = %cfg.display(),
            path = %p.display(),
            "voicevox_engine_path.txt points to non-existent file"
        );
    }
    for candidate in default_install_candidates() {
        if candidate.exists() {
            tracing::info!(
                path = %candidate.display(),
                "resolved VOICEVOX exe via default install candidate"
            );
            return Some(candidate);
        }
    }
    None
}

/// VOICEVOX のデフォルトインストール先候補。 上から順に existence check して、
/// 最初に見つかったものを採用する。 `dirs::data_local_dir()` が解決できない
/// 環境では user 領域候補は省略。
fn default_install_candidates() -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);
    if let Some(local_app_data) = dirs::data_local_dir() {
        out.push(
            local_app_data
                .join("Programs")
                .join("VOICEVOX")
                .join("VOICEVOX.exe"),
        );
    }
    out.push(PathBuf::from(r"C:\Program Files\VOICEVOX\VOICEVOX.exe"));
    out
}

/// VOICEVOX engine が起動中か (`/version` が 200 を返せば true)。
/// blocking、 1 秒 timeout。
pub fn is_running() -> bool {
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
    else {
        return false;
    };
    client
        .get(format!("{VOICEVOX_URL}/{VERSION_PATH}"))
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// `/version` を polling して engine が ready になるまで wait。 ready なら
/// `true`、 60 秒 timeout なら `false`。
pub fn wait_until_ready() -> bool {
    let start = Instant::now();
    while start.elapsed() < POLL_TIMEOUT {
        if is_running() {
            return true;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    false
}

/// VOICEVOX engine を spawn する。 stdout/stderr は捨て、 console window も
/// 出さない。 caller は返値の `Child` を `JobHandle::assign_std` で job に
/// 紐付けて、 daw_01 終了で auto-kill されるようにする。
pub fn spawn_engine(exe: &Path) -> std::io::Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(exe);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    // Windows: console window を出さない。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
}
