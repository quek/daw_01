//! プロジェクト (Song) 非依存の、 アプリ全体の永続化設定 —
//! `%LOCALAPPDATA%\daw_01\app_config.json`。 現状は resource monitor の常駐
//! 表示 on/off のみ。 `window_state` と同じ load/save 流儀だが、 設定は常に
//! 有効値が欲しいので load 失敗は `Option` でなく `Default` で吸収する。

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// status bar のリソースモニター常駐表示を出すか。 default = true。
    #[serde(default = "default_true")]
    pub resource_monitor_enabled: bool,
    /// r.md #29: 編集履歴 window が開いているか (再起動を跨いで復元)。 default = false。
    #[serde(default)]
    pub undo_history_open: bool,
    /// r.md #29: 編集履歴 window の位置・サイズ `[x, y, w, h]` (px)。 `None` =
    /// 未配置 (初回は既定の右上に出す)。 drag / resize 確定時に保存する。
    #[serde(default)]
    pub undo_history_rect: Option<[f32; 4]>,
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            resource_monitor_enabled: true,
            undo_history_open: false,
            undo_history_rect: None,
        }
    }
}

/// ファイルが無い / 読めない / parse 失敗なら `Default` を返す (常に有効な設定)。
pub fn load(path: impl AsRef<Path>) -> AppConfig {
    let Ok(text) = std::fs::read_to_string(path.as_ref()) else {
        return AppConfig::default();
    };
    if text.trim().is_empty() {
        return AppConfig::default();
    }
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(path: impl AsRef<Path>, config: &AppConfig) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(config)?;
    std::fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("app_config.json");
        let cfg = AppConfig {
            resource_monitor_enabled: false,
            undo_history_open: true,
            undo_history_rect: Some([10.0, 20.0, 260.0, 360.0]),
        };
        save(&path, &cfg).unwrap();
        let loaded = load(&path);
        assert!(!loaded.resource_monitor_enabled);
        assert!(loaded.undo_history_open);
        assert_eq!(loaded.undo_history_rect, Some([10.0, 20.0, 260.0, 360.0]));
    }

    #[test]
    fn load_returns_default_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(load(&path).resource_monitor_enabled); // default = true
    }

    #[test]
    fn load_tolerates_unknown_and_missing_fields() {
        // 将来フィールドが増えても古い JSON が読め、 欠落は default で埋まる。
        let dir = tempdir().unwrap();
        let path = dir.path().join("partial.json");
        std::fs::write(&path, "{}").unwrap();
        assert!(load(&path).resource_monitor_enabled); // 欠落 → default true
    }
}
