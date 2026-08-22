//! M8 Phase 34: native file dialog (open / save)。
//!
//! `Ui::request_open_file_dialog(name, title, filters)` で frame 末尾に dialog を出す要求を積む。
//! UiHost が `rfd::FileDialog::pick_file()` などで **同期実行** (UI thread block、modal UX) し、
//! 結果を `pending_dialog_results` に格納。次フレーム頭で `Ui::take_dialog_result(name)` で
//! 取り出される (clipboard と同じ request/response paradigm)。
//!
//! 同期 / 非同期判断: M8 では同期を採用。DAW 業界標準 (Logic / Cubase / Bitwig) で modal、winit と
//! rfd の thread 互換性も Windows / macOS では問題なし。Linux GTK/portal で問題が出た場合は
//! 非同期版 (thread spawn + channel) に降りる retreat path を docs に明記。

use std::path::PathBuf;

/// 拡張子フィルタ。`name` = ユーザに見える表記 ("Audio files")、`extensions` = `["wav", "mp3"]` 等。
#[derive(Debug, Clone, Copy)]
pub struct FileDialogFilter {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
}

/// dialog の結果。
#[derive(Debug, Clone)]
pub enum DialogResult {
    /// ユーザが cancel した。
    Cancelled,
    /// open file (single)。
    OpenFile(PathBuf),
    /// open multiple files。
    OpenFiles(Vec<PathBuf>),
    /// save file (パスは拡張子込み)。
    SaveFile(PathBuf),
}

impl DialogResult {
    /// 単一の `PathBuf` を返す (`OpenFile` / `SaveFile`)。複数や cancel なら None。
    #[must_use]
    pub fn single_path(&self) -> Option<&PathBuf> {
        match self {
            Self::OpenFile(p) | Self::SaveFile(p) => Some(p),
            _ => None,
        }
    }
}

/// `UiHost` が積む dialog 要求 (内部使用)。
#[derive(Debug, Clone)]
pub(crate) struct DialogRequest {
    pub name: &'static str,
    pub kind: DialogKind,
    pub title: String,
    pub default_name: String,
    pub filters: Vec<FileDialogFilter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogKind {
    OpenFile,
    OpenFiles,
    SaveFile,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_path_returns_inner_for_open_save() {
        let p = PathBuf::from("/tmp/x.wav");
        assert_eq!(DialogResult::OpenFile(p.clone()).single_path(), Some(&p));
        assert_eq!(DialogResult::SaveFile(p.clone()).single_path(), Some(&p));
        assert!(DialogResult::Cancelled.single_path().is_none());
        assert!(DialogResult::OpenFiles(vec![p]).single_path().is_none());
    }
}
