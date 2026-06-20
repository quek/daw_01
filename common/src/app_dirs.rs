//! daw_01 のper-user データディレクトリと、その下に永続化する全ファイルの
//! **Single Source of Truth**。
//!
//! 従来 `recent::default_path` / `recovery::recovery_dir` /
//! `window_state::default_path` がそれぞれ `dirs::data_local_dir()?.join("daw_01")`
//! を個別に解決していた (= 同じ root が 4 箇所に重複)。 root を 1 度だけ解決して
//! `AppData` へ注入することで:
//!
//! - root 解決ロジックが 1 箇所に集約される (DRY / SSoT)
//! - test は [`AppDirs::under`] で tempdir を渡し、 永続化を隔離できる
//!   (= 実 `%LOCALAPPDATA%\daw_01\` を汚染しない)。 `dispatcher` の
//!   `BackgroundDispatcher` / `JobDispatcher` と同じ DI パターン
//!
//! `AppData::new` は `Option<AppDirs>` を受け取る。 `None` は「永続化しない」
//! を意味し、 永続化先を不要とする test がこれを渡す。

use std::path::{Path, PathBuf};

/// per-user データディレクトリ (`<root>`) と、 その下の各永続化ファイルの
/// パスを導出する。 root は [`AppDirs::production`] / [`AppDirs::under`] で
/// 1 度だけ確定し、 以降は不変。
#[derive(Debug, Clone)]
pub struct AppDirs {
    root: PathBuf,
}

impl AppDirs {
    /// production の root: `%LOCALAPPDATA%\daw_01\` (非 Windows は同等の
    /// local data dir)。 platform の local data dir が解決できない極端な
    /// 環境では `None` (= 呼び出し側は従来どおり「永続化なし」 として扱う)。
    pub fn production() -> Option<Self> {
        Some(Self {
            root: dirs::data_local_dir()?.join("daw_01"),
        })
    }

    /// 任意のディレクトリを root として全永続化ファイルをその下に置く。
    /// test が tempdir を渡して永続化を隔離するために使う。
    pub fn under(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// root ディレクトリそのもの (`<root>`)。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>\recent.json` — 「最近開いたファイル」 履歴。
    pub fn recent(&self) -> PathBuf {
        self.root.join("recent.json")
    }

    /// `<root>\recent_saved.json` — 「最近保存したファイル」 履歴。
    pub fn recent_saved(&self) -> PathBuf {
        self.root.join("recent_saved.json")
    }

    /// `<root>\recovery\` — autosave / crash-recovery ディレクトリ。
    pub fn recovery_dir(&self) -> PathBuf {
        self.root.join("recovery")
    }

    /// `<root>\window_state.json` — メインウィンドウ geometry。
    pub fn window_state(&self) -> PathBuf {
        self.root.join("window_state.json")
    }

    /// `<root>\logs\` — 各プロセスの日次ローテーション tracing ログ置き場。
    /// release で windows-subsystem 化 (コンソール無し) しても、 ここに
    /// `<process>.YYYY-MM-DD` が常時書かれる。 docs/plan_icon_and_console.md (#48)。
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `<root>\voicevox_cache\` — VOICEVOX 合成結果 (WAV) の per-user 永続
    /// キャッシュ (FIXME #77)。 合成 wav は (歌詞 / pitch / bpm / speaker) の
    /// 純粋関数 = コンテンツアドレス可能なので、 プロジェクト跨ぎで再利用できる
    /// per-user global に置く。 プロジェクトを開き直しても再合成しないための
    /// ディスクキャッシュ。 合成プロセス (daw_plugin_host) も `dirs::data_local
    /// _dir` (env ベース) で同じ root を解決できる。
    pub fn voicevox_cache_dir(&self) -> PathBuf {
        self.root.join("voicevox_cache")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_all_paths_under_root() {
        let dirs = AppDirs::under("C:\\probe\\daw_01");
        assert_eq!(dirs.root(), Path::new("C:\\probe\\daw_01"));
        assert_eq!(dirs.recent(), PathBuf::from("C:\\probe\\daw_01\\recent.json"));
        assert_eq!(
            dirs.recent_saved(),
            PathBuf::from("C:\\probe\\daw_01\\recent_saved.json")
        );
        assert_eq!(
            dirs.recovery_dir(),
            PathBuf::from("C:\\probe\\daw_01\\recovery")
        );
        assert_eq!(
            dirs.window_state(),
            PathBuf::from("C:\\probe\\daw_01\\window_state.json")
        );
    }
}
