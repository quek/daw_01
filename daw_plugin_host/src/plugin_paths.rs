//! プラグインの標準検索パスの **Single Source of Truth** (r.md #81)。
//!
//! ## なぜ環境変数から組み立てないか
//!
//! 以前 `clap_scan` と `vst3_scan` は **同じ 3 行を重複させて**
//! `std::env::var_os("COMMONPROGRAMFILES")` を直読みし、 取れなければ
//! `C:\Program Files\Common Files` を決め打ちしていた。 これは 2 つの意味で
//! 誤りだった:
//!
//! - **環境変数は起動経路によって丸ごと消える。** Git for Windows の bash から
//!   MSYS2 の make を起動すると POSIX 環境変数が 1 つも継承されず、 recipe から
//!   起動した子プロセスは 13 変数しか見ない (Makefile 冒頭の節)。
//! - **決め打ちの fallback は「見つからなければ静かに 0 件」**。 Program Files を
//!   C: 以外に置いたマシンでは、 プラグインが黙って 1 つも見つからなくなる。
//!   このリポジトリが `fetch_ffmpeg` で「発見方式は原理的に対応できない」と
//!   結論づけたのと同じ形。
//!
//! よって権威は **known folder API (`SHGetKnownFolderPath`)** ただ 1 つにする。
//! これは VST3 の仕様書が名指ししている API そのもので、 `common::app_dirs` が
//! 経由する `dirs` crate とも同じ権威。
//!
//! ## 一次情報
//!
//! - CLAP: `include/clap/entry.h`
//!   (<https://github.com/free-audio/clap/blob/005fc61583486d7721d8352572879f5f3d7f390f/include/clap/entry.h>)
//!   ```text
//!   // CLAP plugins standard search path:
//!   //
//!   // Linux
//!   //   - ~/.clap
//!   //   - /usr/lib/clap
//!   //
//!   // Windows
//!   //   - %COMMONPROGRAMFILES%\CLAP
//!   //   - %LOCALAPPDATA%\Programs\Common\CLAP
//!   //
//!   // MacOS
//!   //   - /Library/Audio/Plug-Ins/CLAP
//!   //   - ~/Library/Audio/Plug-Ins/CLAP
//!   //
//!   // In addition to the OS-specific default locations above, a CLAP host must query the environment
//!   // for a CLAP_PATH variable, which is a list of directories formatted in the same manner as the host
//!   // OS binary search path (PATH on Unix, separated by `:` and Path on Windows, separated by ';', as
//!   // of this writing).
//!   ```
//!   `CLAP_PATH` は **must** (直後の再帰探索は should と書き分けられている)。
//!   Windows に `(x86)` 版のパスは **無い** ので足さないこと。
//!
//! - VST3: Steinberg VST 3 Developer Portal "Plug-in Locations" の Windows 表。
//!   Prio 1 User = `FOLDERID_UserProgramFilesCommon\VST3`、
//!   Prio 2 Global = `FOLDERID_ProgramFilesCommon\VST3`、
//!   Prio 3 Application = `$APPFOLDER\VST3`。
//!   表には `/Program Files (x86)/Common Files/VST3/` の行もあるが、 これは
//!   「32bit Plug-ins on 64bit Windows」用で、 SDK の `getModulePaths()` にも
//!   対応する枝は無い (OS が process bitness に応じて FOLDERID を解決する)。
//!   **64bit ホストがここを足すとロードできない 32bit DLL を並べるだけ**なので
//!   足さない。 探索順は User → Global → Application。
//!
//! ## 「無い」と「該当しない」を区別する
//!
//! [`clap_search_roots`] / [`vst3_search_roots`] は **存在するディレクトリだけ**を
//! 返す。 存在しないルートを黙って落とすのではなく、 呼び出し側が
//! 「ルートが 1 つも無い」と「ルートはあったが .clap が 0 個」を区別できるよう、
//! 返すのは常に「実在するルートの列」であって Result ではない。

use std::path::PathBuf;

/// CLAP 仕様が定める検索ルート (実在するものだけ)。
///
/// 順序は entry.h の記載順 → `CLAP_PATH` の順。 `CLAP_PATH` は仕様上ホストの
/// 義務なので、 標準位置が 1 つも無くても必ず見る。
pub fn clap_search_roots() -> Vec<PathBuf> {
    let mut roots = standard_clap_roots();
    roots.extend(env_path_list("CLAP_PATH"));
    existing_unique(roots)
}

/// VST3 仕様が定める検索ルート (実在するものだけ)。優先度は User → Global →
/// Application。仕様に環境変数の規定は無いので env は読まない。
pub fn vst3_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(user) = user_program_files_common() {
        roots.push(user.join("VST3"));
    }
    if let Some(global) = program_files_common() {
        roots.push(global.join("VST3"));
    }
    if let Some(app_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    {
        roots.push(app_dir.join("VST3"));
    }
    existing_unique(roots)
}

#[cfg(windows)]
fn standard_clap_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(global) = program_files_common() {
        roots.push(global.join("CLAP"));
    }
    if let Some(user) = user_program_files_common() {
        roots.push(user.join("CLAP"));
    }
    roots
}

#[cfg(target_os = "macos")]
fn standard_clap_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/Library/Audio/Plug-Ins/CLAP")];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Library/Audio/Plug-Ins/CLAP"));
    }
    roots
}

#[cfg(all(unix, not(target_os = "macos")))]
fn standard_clap_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".clap"));
    }
    roots.push(PathBuf::from("/usr/lib/clap"));
    roots
}

/// `FOLDERID_ProgramFilesCommon` — 64bit プロセスなら
/// `C:\Program Files\Common Files`。 VST3 仕様の Prio 2 (Global)、
/// CLAP の `%COMMONPROGRAMFILES%` に相当する。
#[cfg(windows)]
fn program_files_common() -> Option<PathBuf> {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramFilesCommon)
}

/// `FOLDERID_UserProgramFilesCommon` — `%LOCALAPPDATA%\Programs\Common`。
/// VST3 仕様の Prio 1 (User)、 CLAP の
/// `%LOCALAPPDATA%\Programs\Common\CLAP` の親。
#[cfg(windows)]
fn user_program_files_common() -> Option<PathBuf> {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_UserProgramFilesCommon)
}

#[cfg(not(windows))]
fn program_files_common() -> Option<PathBuf> {
    None
}

#[cfg(not(windows))]
fn user_program_files_common() -> Option<PathBuf> {
    None
}

/// `SHGetKnownFolderPath` の薄いラッパ。 環境変数を一切参照しない。
///
/// 返された `PWSTR` は COM のタスクアロケータ由来なので、 読み取り後に
/// `CoTaskMemFree` で必ず返す (成功時のみ確保される)。
#[cfg(windows)]
fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    // SAFETY: `id` は windows crate が提供する静的な FOLDERID。 戻り値の
    // PWSTR は成功時のみ有効で、 その場で PathBuf へコピーしてから解放する。
    let raw = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
    let path = unsafe { raw.to_string() }.ok().map(PathBuf::from);
    unsafe { CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void)) };
    path
}

/// `PATH` と同じ形式のディレクトリ列を持つ環境変数を分解する。
/// 区切りは Windows が `;`、 それ以外が `:` (CLAP 仕様の記述どおり、
/// `std::env::split_paths` が OS ごとに同じ規則を持つ)。
fn env_path_list(name: &str) -> Vec<PathBuf> {
    let Some(raw) = std::env::var_os(name) else {
        return Vec::new();
    };
    std::env::split_paths(&raw)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// 実在するディレクトリだけを、 与えられた順序を保って重複なく返す。
///
/// 重複除去は必須。 ルートが複数になったことで、 同じプラグインが 2 つの
/// ルートから見えるとピッカーに二重登録される (ルートが 1 本だった頃は
/// 表面化しなかった)。
fn existing_unique(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = Vec::new();
    for root in roots {
        // canonicalize は存在確認も兼ねる。 失敗 (= 存在しない / 権限が無い)
        // したルートは静かに落とさず、 単に候補から外す。
        let Ok(real) = root.canonicalize() else {
            continue;
        };
        if !real.is_dir() || seen.contains(&real) {
            continue;
        }
        seen.push(real);
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_path_list_splits_on_the_os_separator() {
        let sep = if cfg!(windows) { ';' } else { ':' };
        let joined = format!("C:{sep}D:", sep = sep);
        // SAFETY: single-threaded unit test; no other thread reads the env here.
        unsafe { std::env::set_var("DAW01_TEST_PATH_LIST", &joined) };
        let list = env_path_list("DAW01_TEST_PATH_LIST");
        unsafe { std::env::remove_var("DAW01_TEST_PATH_LIST") };
        assert_eq!(list.len(), 2, "got {list:?}");
    }

    #[test]
    fn env_path_list_is_empty_when_unset() {
        assert!(env_path_list("DAW01_TEST_PATH_LIST_UNSET").is_empty());
    }

    #[test]
    fn existing_unique_drops_missing_and_duplicate_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().to_path_buf();
        let missing = real.join("does-not-exist");
        let roots = existing_unique(vec![real.clone(), missing, real]);
        assert_eq!(roots.len(), 1, "got {roots:?}");
    }

    #[cfg(windows)]
    #[test]
    fn known_folders_resolve_without_environment_variables() {
        // 環境変数ではなく Win32 の権威から引けていることの確認。
        // 値そのものはマシン依存なので「絶対パスが返る」ことだけを見る。
        let global = program_files_common().expect("FOLDERID_ProgramFilesCommon");
        let user = user_program_files_common().expect("FOLDERID_UserProgramFilesCommon");
        assert!(global.is_absolute(), "{}", global.display());
        assert!(user.is_absolute(), "{}", user.display());
        assert_ne!(global, user);
    }
}
