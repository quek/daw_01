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

    /// `<root>\recovery\` — autosave / crash-recovery ディレクトリ。 未保存の文書の
    /// 素材の置き場を使用中と示すロック (`<id>.lock`) も同じ id でここに置く
    /// ([`crate::recovery::lock_path_for`])。
    pub fn recovery_dir(&self) -> PathBuf {
        self.root.join("recovery")
    }

    /// `<root>\window_state.json` — メインウィンドウ geometry。
    pub fn window_state(&self) -> PathBuf {
        self.root.join("window_state.json")
    }

    /// `<root>\app_config.json` — プロジェクト非依存のアプリ全体設定
    /// (resource monitor の常駐表示 on/off など)。
    pub fn app_config(&self) -> PathBuf {
        self.root.join("app_config.json")
    }

    /// `<root>\themes\` — ユーザーが追加したテーマ (`*.json`) の置き場 (r.md #48)。
    /// ここに置いたファイルが設定画面のテーマ一覧に出る。ディレクトリが無くてもよい
    /// (= 組込みテーマだけになる)。
    pub fn themes_dir(&self) -> PathBuf {
        self.root.join("themes")
    }

    /// `<root>\logs\` — 各プロセスの日次ローテーション tracing ログ置き場。
    /// release で windows-subsystem 化 (コンソール無し) しても、 ここに
    /// `<process>.YYYY-MM-DD` が常時書かれる。 docs/plan_icon_and_console.md (#48)。
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `<root>\voicevox_cache\` — VOICEVOX 合成結果 (WAV) の per-user 永続
    /// キャッシュ。 合成 wav は (歌詞 / pitch / bpm / speaker) の
    /// 純粋関数 = コンテンツアドレス可能なので、 プロジェクト跨ぎで再利用できる
    /// per-user global に置く。 プロジェクトを開き直しても再合成しないための
    /// ディスクキャッシュ。 合成プロセス (daw_plugin_host) も
    /// `dirs::data_local_dir` で同じ root を解決できる
    /// (= `SHGetKnownFolderPath`。 環境変数は経由しない)。
    pub fn voicevox_cache_dir(&self) -> PathBuf {
        self.root.join("voicevox_cache")
    }

    /// `<root>\import_cache\` — **未保存プロジェクト**に取り込んだ素材
    /// (audio / video / image) の置き場の親。 文書ごとに `<id>\` を切る
    /// ([`crate::recovery::DocId`]、所有と後始末は `daw_gui::unsaved_place`)。 保存時に
    /// `<project_dir>/{samples,images}/` へ移送される (`daw_gui::media_bundle`)。
    /// 取り込み側は注入された `AppDirs` からだけ解決する (`daw_gui::media_dest`) —
    /// ここを `production()` から直接引くと、テストも検証起動もユーザーの実データへ書く。
    ///
    /// 直下に文書の id を持たないファイルがあれば、文書ごとに切る前の版が置いたもの。
    /// 旧版で未保存のまま動画 / 画像を取り込んでから保存したプロジェクト (当時は保存時に
    /// 移していなかった) が絶対パスで指しているかもしれないので、掃除では触らない。
    ///
    /// 以前ここは `%LOCALAPPDATA%` の**環境変数直読み**で解決していた
    /// (r.md #81)。 make 経由だと env が丸ごと落ちるため
    /// `<repo>/target/tmp/daw_01/import_cache` に着地し、 `make clean`
    /// (= `cargo clean`) が**未保存プロジェクトの実データを黙って消す**
    /// 経路になっていた。 root を [`AppDirs`] に一本化して塞いだ。
    pub fn import_cache_dir(&self) -> PathBuf {
        self.root.join("import_cache")
    }

    /// `<root>\bounce_cache\` — **未保存プロジェクト**の Bounce 出力 WAV の置き場の親
    /// (文書ごとの `<id>\` は [`AppDirs::import_cache_dir`] と同じ)。
    /// 保存時に `<project_dir>/bounce/` へ移送される。
    /// [`AppDirs::import_cache_dir`] と同じ経緯で env 直読みから移した。
    pub fn bounce_cache_dir(&self) -> PathBuf {
        self.root.join("bounce_cache")
    }
}

/// 検証用の起動 (`daw_gui --script` / `--smoke-test`) の per-user データ root。
///
/// これらは `cargo test` や agent の検証から起動され、**ユーザーが使っている daw_gui と
/// 同時に**走る。production の root を使うと、ユーザーの recent / recovery / app_config を
/// 読み書きし、未保存プロジェクトの取り込みキャッシュへテストの素材を置いていく
/// (実際に `import_cache` / `bounce_cache` にテストの残骸が溜まっていた)。
/// 起動ごとに一意な一時フォルダを root にし、drop で丸ごと消す (watchdog の
/// `process::exit` などで drop を通らなかった回は残る — 一時フォルダの下なので害は無い)。
///
/// 対象は `AppDirs` が持つ per-user 状態だけ。ログ (`logging`)、プラグイン DB、VOICEVOX の
/// エンジン設定 / 合成キャッシュは機械の設定 / 内容アドレスの共有物として従来どおり引く。
pub struct IsolatedAppDirs {
    dirs: AppDirs,
}

impl IsolatedAppDirs {
    /// `<temp>/daw_01_<label>_<uuid>/` を作る。
    pub fn create(label: &str) -> std::io::Result<Self> {
        let root = std::env::temp_dir()
            .join(format!("daw_01_{label}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root)?;
        Ok(Self { dirs: AppDirs::under(root) })
    }

    pub fn dirs(&self) -> &AppDirs {
        &self.dirs
    }
}

impl Drop for IsolatedAppDirs {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(self.dirs.root()) {
            tracing::warn!(error = %e, root = %self.dirs.root().display(), "isolated app data root を消せない");
        }
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
