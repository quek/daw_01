//! Autosave & crash-recovery file management.
//!
//! Two storage strategies live side by side:
//!
//! - **Sidecar**: 保存済みプロジェクト (`<file>.daw`) を編集中の autosave は
//!   `<file>.daw.autosave.daw` に書く。 元ファイル名で復元できる。
//! - **Recovery dir**: まだ Save されていないプロジェクトの autosave は
//!   `%LOCALAPPDATA%\daw_01\recovery\<session_uuid>.autosave.daw` に書く。
//!   復元時は新規プロジェクト扱い (file_path=None) で、 ユーザーが Save As で
//!   名前を付ける運用。
//!
//! 起動時に recovery dir + (Open 時に) sidecar を scan して候補を modal に
//! 出し、 「復元 / 破棄」 を選んでもらう。

use std::path::{Path, PathBuf};

const AUTOSAVE_SUFFIX: &str = ".autosave.daw";
const LOCK_SUFFIX: &str = ".lock";

/// 未保存の文書を per-user データフォルダの中で識別する id (uuid v4)。
///
/// 1 つの id が、その文書の **autosave** (`recovery/<id>.autosave.daw`)、**素材の置き場**
/// (`import_cache/<id>/` / `bounce_cache/<id>/`、`daw_gui::unsaved_place`)、置き場の
/// **使用中ロック** (`recovery/<id>.lock`) の名前をそろえる。recovery から復元した文書は
/// ファイル名の id を引き継ぐので、復元前に取り込んだ素材の置き場もそのまま自分のものになる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocId(uuid::Uuid);

impl DocId {
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// ファイル / フォルダ名から読む。**書き出す形 (hyphenated 小文字) と一字一句同じ**名前だけを
    /// 受ける — 名前をそのままパスへ繋ぐので、`.` / `..` や別表記 (`{...}` / 大文字) を id として
    /// 通すと、掃除が置き場の親や別のフォルダを指してしまう。
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let id = uuid::Uuid::try_parse(name).ok().map(Self)?;
        (id.to_string() == name).then_some(id)
    }

    /// recovery dir の `<id>.autosave.daw` の id。sidecar (`<file>.daw.autosave.daw`) は `None`。
    #[must_use]
    pub fn of_recovery_file(autosave: &Path) -> Option<Self> {
        let name = autosave.file_name()?.to_str()?;
        Self::parse(name.strip_suffix(AUTOSAVE_SUFFIX)?)
    }

    /// `recovery/<id>.lock` の id。
    #[must_use]
    pub fn of_lock_file(lock: &Path) -> Option<Self> {
        let name = lock.file_name()?.to_str()?;
        Self::parse(name.strip_suffix(LOCK_SUFFIX)?)
    }
}

impl Default for DocId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for DocId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

/// recovery dir を `create_dir_all` で作る。 `dir` は呼び出し側が
/// [`crate::app_dirs::AppDirs::recovery_dir`] から解決して渡す。
pub fn ensure_recovery_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// `dir` 内の `*.autosave.daw` を列挙。 ディレクトリが無ければ空 vec。
/// I/O エラーは tracing::warn! で記録した上で空 vec を返し、起動シーケンスを止めない。
/// `NotFound` (初回起動で recovery dir 未作成) は警告対象外。
pub fn scan_recovery_files(dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(error = ?e, dir = ?dir, "recovery dir read failed");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for e in entries {
        let entry = match e {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(error = ?e, dir = ?dir, "recovery dir entry read failed");
                continue;
            }
        };
        let p = entry.path();
        if p.is_file()
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(AUTOSAVE_SUFFIX))
        {
            out.push(p);
        }
    }
    out
}

/// 未保存の文書 `id` の recovery file path (`dir / "<id>.autosave.daw"`)。
/// `dir` は呼び出し側が [`crate::app_dirs::AppDirs::recovery_dir`] から渡す。
pub fn recovery_path_for(dir: &Path, id: DocId) -> PathBuf {
    dir.join(format!("{id}{AUTOSAVE_SUFFIX}"))
}

/// 未保存の文書 `id` の素材の置き場を使用中と示すロックファイル (`dir / "<id>.lock"`)。
pub fn lock_path_for(dir: &Path, id: DocId) -> PathBuf {
    dir.join(format!("{id}{LOCK_SUFFIX}"))
}

/// `<file>.daw` に対する sidecar autosave path (`<file>.daw.autosave.daw`)。
pub fn sidecar_for(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_os_string();
    s.push(AUTOSAVE_SUFFIX);
    PathBuf::from(s)
}

/// path が autosave file (suffix 一致) かどうかを判定する helper。
pub fn is_autosave_file(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(AUTOSAVE_SUFFIX))
}

/// recovery file が「sidecar 形式」 (`<file>.daw.autosave.daw`) なら、 元の
/// `<file>.daw` を返す。 recovery_dir 内の単独 file (`<uuid>.autosave.daw`)
/// なら `None` (= 新規プロジェクト扱いで開く)。
pub fn original_file_for_sidecar(autosave: &Path) -> Option<PathBuf> {
    let name = autosave.file_name()?.to_str()?;
    let stripped = name.strip_suffix(AUTOSAVE_SUFFIX)?;
    // recovery_dir の単独 file は stem が ".daw" で終わらない uuid 文字列。
    if !stripped.ends_with(".daw") {
        return None;
    }
    let parent = autosave.parent()?;
    Some(parent.join(stripped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_appends_suffix() {
        let s = sidecar_for(Path::new("song.daw"));
        assert_eq!(s, PathBuf::from("song.daw.autosave.daw"));
    }

    #[test]
    fn is_autosave_detects_suffix() {
        assert!(is_autosave_file(Path::new("x.daw.autosave.daw")));
        assert!(is_autosave_file(Path::new(
            "abc-123.autosave.daw"
        )));
        assert!(!is_autosave_file(Path::new("x.daw")));
    }

    #[test]
    fn original_for_sidecar_extracts_daw() {
        let orig = original_file_for_sidecar(Path::new(
            "C:\\proj\\song.daw.autosave.daw",
        ));
        assert_eq!(
            orig,
            Some(PathBuf::from("C:\\proj\\song.daw"))
        );
    }

    #[test]
    fn original_for_recovery_dir_uuid_returns_none() {
        let orig = original_file_for_sidecar(Path::new(
            "C:\\appdata\\recovery\\abc-uuid.autosave.daw",
        ));
        assert!(orig.is_none());
    }

    #[test]
    fn recovery_and_lock_paths_round_trip_the_doc_id() {
        let dir = Path::new("C:\\appdata\\daw_01\\recovery");
        let id = DocId::new();
        assert_eq!(DocId::of_recovery_file(&recovery_path_for(dir, id)), Some(id));
        assert_eq!(DocId::of_lock_file(&lock_path_for(dir, id)), Some(id));
        assert_eq!(DocId::of_recovery_file(Path::new("C:\\proj\\song.daw.autosave.daw")), None);
    }

    /// 名前をそのままパスへ繋ぐので、書き出す形以外は id として受けない。
    #[test]
    fn doc_id_rejects_names_that_are_not_its_own_spelling() {
        let id = DocId::new();
        assert_eq!(DocId::parse(&id.to_string()), Some(id));
        for bad in [
            String::new(),
            ".".into(),
            "..".into(),
            id.to_string().to_uppercase(),
            format!("{{{id}}}"),
            id.to_string().replace('-', ""),
        ] {
            assert_eq!(DocId::parse(&bad), None, "{bad:?}");
        }
        assert_eq!(DocId::of_recovery_file(Path::new("..autosave.daw")), None);
    }
}
