//! VOICEVOX engine subprocess の起動 / ヘルスチェック helper。
//!
//! 動作概要:
//! 1. **起動済み判定**: `/version` を 1 秒 timeout で叩いて成功 (2xx) が返れば起動済
//!    (手動で開いた VOICEVOX エディタも内部で engine を立てて :50021 を出すので、
//!    その場合はそれを使う)
//! 2. **path 解決**: env `DAW_VOICEVOX_PATH` → 設定ファイル
//!    `%LOCALAPPDATA%/daw_01/voicevox_engine_path.txt` → デフォルト install 候補。
//!    解決対象は GUI エディタ (`VOICEVOX.exe` = 204MB Electron) ではなく
//!    **ヘッドレスエンジン `vv-engine/run.exe` (9MB)**。エディタは起動すると
//!    ウィンドウが出てフォーカスを奪うが、その中身は結局 run.exe を spawn して
//!    いるだけなので、ウィンドウを持たない run.exe を直接立てる。
//! 3. **spawn**: `std::process::Command` で `run.exe --use_gpu` を起動
//!    (DirectML GPU 合成、stdout/stderr は null、console window も出さない)。
//!    呼び出し側が `JobHandle::assign_std` で daw_01 終了時の自動 kill を担保
//! 4. **wait_until_ready**: `/version` を 500ms 間隔で polling、 60 秒 timeout
//!
//! run.exe が見つからない (engine 非同梱の editor のみ) 環境では従来どおり
//! `VOICEVOX.exe` を起動する劣化フォールバック。失敗時は何もしない
//! (= ユーザーが手動で起動する fallback)。 daw_gui 起動を止めない。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::voicevox::VOICEVOX_URL;

const VERSION_PATH: &str = "version";
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const POLL_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_millis(1000);

/// 解決した起動対象。
///
/// 通常は headless engine (`vv-engine/run.exe`)。 engine が見つからない
/// (= engine 非同梱の editor のみ) 環境でだけ editor (`VOICEVOX.exe`) への
/// 劣化フォールバックになる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedEngine {
    /// ヘッドレスエンジン `run.exe`。 ウィンドウを持たず、 `--use_gpu` で起動する。
    Headless(PathBuf),
    /// GUI エディタ `VOICEVOX.exe`。 run.exe が見つからないときのみ。
    Editor(PathBuf),
}

impl ResolvedEngine {
    /// 起動する exe パス。
    pub fn exe(&self) -> &Path {
        match self {
            ResolvedEngine::Headless(p) | ResolvedEngine::Editor(p) => p,
        }
    }
}

/// 設定ファイルの絶対パス (`%LOCALAPPDATA%/daw_01/voicevox_engine_path.txt`)。
pub fn engine_path_config_file() -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("daw_01")
            .join("voicevox_engine_path.txt"),
    )
}

/// 起動対象を解決する。 優先順:
///
/// 1. env `DAW_VOICEVOX_PATH`
/// 2. 設定ファイル `voicevox_engine_path.txt` の 1 行目 (前後 trim)
/// 3. **デフォルト install root 候補** (優先順、 実体の有無は [`derive_engine`] が判定):
///     - `%LOCALAPPDATA%\Programs\VOICEVOX\` (ユーザー領域インストール)
///     - `C:\Program Files\VOICEVOX\` (system インストール)
///
/// 1/2 の値は「VOICEVOX の所在」(install root / `VOICEVOX.exe` / `run.exe` の
/// いずれか) として解釈し、 [`derive_engine`] で headless engine を導出する。
/// どの経路でも run.exe が見つからなければ editor への劣化フォールバック。
/// 何も該当しなければ `None` (= 起動 skip、 ユーザーが手動で立ち上げる想定)。
pub fn resolve_engine_path() -> Option<ResolvedEngine> {
    if let Some(env_path) = std::env::var_os("DAW_VOICEVOX_PATH") {
        let p = PathBuf::from(env_path);
        if let Some(engine) = derive_engine(&p) {
            return Some(engine);
        }
        tracing::warn!(
            path = %p.display(),
            "DAW_VOICEVOX_PATH does not resolve to a VOICEVOX engine or editor"
        );
    }
    if let Some(cfg) = engine_path_config_file()
        && let Ok(text) = std::fs::read_to_string(&cfg)
    {
        let p = PathBuf::from(text.trim());
        if let Some(engine) = derive_engine(&p) {
            return Some(engine);
        }
        tracing::warn!(
            cfg = %cfg.display(),
            path = %p.display(),
            "voicevox_engine_path.txt does not resolve to a VOICEVOX engine or editor"
        );
    }
    for root in default_install_roots() {
        if let Some(engine) = derive_engine(&root) {
            tracing::info!(?engine, "resolved VOICEVOX via default install candidate");
            return Some(engine);
        }
    }
    None
}

/// `input` (ファイル or ディレクトリ) から実際に起動する engine / editor を導出する。
///
/// - `run.exe` 直指定 → そのまま [`ResolvedEngine::Headless`]
/// - `VOICEVOX.exe` (等のファイル) 直指定 → 同階層から `run.exe` を探して
///   [`ResolvedEngine::Headless`]、 無ければ `VOICEVOX.exe` 自身を
///   [`ResolvedEngine::Editor`]
/// - ディレクトリ (install root) → `<dir>/vv-engine/run.exe` → `<dir>/run.exe`
///   → `<dir>/VOICEVOX.exe` の順
///
/// どれも実体が無ければ `None`。
fn derive_engine(input: &Path) -> Option<ResolvedEngine> {
    if input.is_file() {
        let name = input.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.eq_ignore_ascii_case("run.exe") {
            return Some(ResolvedEngine::Headless(input.to_path_buf()));
        }
        // VOICEVOX.exe 等 → まず同階層から headless engine を探す。
        if let Some(dir) = input.parent()
            && let Some(run) = find_headless_in(dir)
        {
            return Some(ResolvedEngine::Headless(run));
        }
        if name.eq_ignore_ascii_case("VOICEVOX.exe") {
            return Some(ResolvedEngine::Editor(input.to_path_buf()));
        }
        return None;
    }
    if input.is_dir() {
        if let Some(run) = find_headless_in(input) {
            return Some(ResolvedEngine::Headless(run));
        }
        let editor = input.join("VOICEVOX.exe");
        if editor.is_file() {
            return Some(ResolvedEngine::Editor(editor));
        }
    }
    None
}

/// `dir` 配下から headless engine (`run.exe`) を探す。 install root 直下の
/// `vv-engine/run.exe` を優先し、 次に `dir/run.exe` (= dir が engine ディレクトリ
/// そのものを指す engine 単体配布のケース)。
fn find_headless_in(dir: &Path) -> Option<PathBuf> {
    let nested = dir.join("vv-engine").join("run.exe");
    if nested.is_file() {
        return Some(nested);
    }
    let direct = dir.join("run.exe");
    if direct.is_file() {
        return Some(direct);
    }
    None
}

/// VOICEVOX のデフォルト install root 候補。 上から順に [`derive_engine`] に
/// かけ、 最初に解決できたものを採用する。 `dirs::data_local_dir()` が
/// 解決できない環境では user 領域候補は省略。
fn default_install_roots() -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);
    if let Some(local_app_data) = dirs::data_local_dir() {
        out.push(local_app_data.join("Programs").join("VOICEVOX"));
    }
    out.push(PathBuf::from(r"C:\Program Files\VOICEVOX"));
    out
}

/// VOICEVOX engine が起動中か (`/version` が成功ステータス (2xx) を返せば true)。
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

/// 解決した engine / editor を spawn する。 stdout/stderr は捨て、 console
/// window も出さない。 headless engine は `--use_gpu` (DirectML GPU 合成) で
/// 起動する。 caller は返値の `Child` を `JobHandle::assign_std` で job に
/// 紐付けて、 daw_01 終了で auto-kill されるようにする。
pub fn spawn_engine(engine: &ResolvedEngine) -> std::io::Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(engine.exe());
    // headless engine は GPU 合成で起動。 editor フォールバックは引数なし
    // (editor が自前で engine 設定 / GPU 切替を持つ)。
    if matches!(engine, ResolvedEngine::Headless(_)) {
        cmd.arg("--use_gpu");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// `path` (とその親) を作って空ファイルを置く。
    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn run_exe_direct_is_headless() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("vv-engine").join("run.exe");
        touch(&run);
        assert_eq!(derive_engine(&run), Some(ResolvedEngine::Headless(run)));
    }

    #[test]
    fn install_dir_resolves_nested_run_exe() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("vv-engine").join("run.exe");
        touch(&run);
        touch(&dir.path().join("VOICEVOX.exe"));
        assert_eq!(
            derive_engine(dir.path()),
            Some(ResolvedEngine::Headless(run))
        );
    }

    #[test]
    fn editor_exe_derives_sibling_run_exe() {
        let dir = tempfile::tempdir().unwrap();
        let editor = dir.path().join("VOICEVOX.exe");
        touch(&editor);
        let run = dir.path().join("vv-engine").join("run.exe");
        touch(&run);
        assert_eq!(derive_engine(&editor), Some(ResolvedEngine::Headless(run)));
    }

    #[test]
    fn editor_without_engine_falls_back_to_editor() {
        let dir = tempfile::tempdir().unwrap();
        let editor = dir.path().join("VOICEVOX.exe");
        touch(&editor);
        assert_eq!(
            derive_engine(&editor),
            Some(ResolvedEngine::Editor(editor))
        );
    }

    #[test]
    fn dir_with_only_editor_falls_back_to_editor() {
        let dir = tempfile::tempdir().unwrap();
        let editor = dir.path().join("VOICEVOX.exe");
        touch(&editor);
        assert_eq!(
            derive_engine(dir.path()),
            Some(ResolvedEngine::Editor(editor))
        );
    }

    #[test]
    fn engine_only_dir_resolves_direct_run_exe() {
        // dir 自身が engine ディレクトリ (vv-engine 無しで run.exe 直下)。
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run.exe");
        touch(&run);
        assert_eq!(
            derive_engine(dir.path()),
            Some(ResolvedEngine::Headless(run))
        );
    }

    #[test]
    fn nested_run_exe_wins_over_direct() {
        // `vv-engine/run.exe` と `dir/run.exe` が両方あるとき nested を優先する。
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("vv-engine").join("run.exe");
        touch(&nested);
        touch(&dir.path().join("run.exe"));
        assert_eq!(
            derive_engine(dir.path()),
            Some(ResolvedEngine::Headless(nested))
        );
    }

    #[test]
    fn nonexistent_path_resolves_to_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(derive_engine(&dir.path().join("nope.exe")), None);
        assert_eq!(derive_engine(&dir.path().join("missing_subdir")), None);
    }
}
