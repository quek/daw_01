// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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

/// 起動 1 回ごとに発行する recovery session id (uuid v4)。
/// `recovery_path_for_session` の引数に使う。
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
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

/// 当セッション用 recovery file path (`dir / "<id>.autosave.daw"`)。
/// `dir` は呼び出し側が [`crate::app_dirs::AppDirs::recovery_dir`] から渡す。
pub fn recovery_path_for_session(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}{AUTOSAVE_SUFFIX}"))
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
    fn recovery_path_uses_session_id() {
        let p = recovery_path_for_session(
            Path::new("C:\\appdata\\daw_01\\recovery"),
            "test_session_id",
        );
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(name.contains("test_session_id"));
        assert!(name.ends_with(".autosave.daw"));
    }
}
