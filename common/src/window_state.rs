//! Persisted main window geometry — `%LOCALAPPDATA%\daw_01\window_state.json`。
//! 起動時に位置 / サイズ / maximized を復元し、 終了時に最新値を保存する。

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// メインウィンドウの永続化対象 geometry。 `width`/`height` は logical pixels
/// (scale factor を割った値) で持つ — 別 monitor 間移動時のスケール差を抑える
/// ため。 `x`/`y` は physical pixels (screen 座標)、 maximized 復元と独立。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowState {
    pub width: f64,
    pub height: f64,
    pub x: i32,
    pub y: i32,
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 1280.0,
            height: 800.0,
            x: 100,
            y: 100,
            maximized: false,
        }
    }
}

/// ファイルが存在しない / 読めない / parse 失敗のいずれかなら `None`。
/// load 失敗は致命的でないので `Result` を返さず option で吸収。
pub fn load(path: impl AsRef<Path>) -> Option<WindowState> {
    let path = path.as_ref();
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&text).ok()
}

pub fn save(path: impl AsRef<Path>, state: &WindowState) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state)?;
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
        let path = dir.path().join("ws.json");
        let s = WindowState {
            width: 1600.0,
            height: 900.0,
            x: 200,
            y: 150,
            maximized: false,
        };
        save(&path, &s).unwrap();
        let loaded = load(&path).unwrap();
        assert!((loaded.width - 1600.0).abs() < 1e-6);
        assert!((loaded.height - 900.0).abs() < 1e-6);
        assert_eq!(loaded.x, 200);
        assert_eq!(loaded.y, 150);
        assert!(!loaded.maximized);
    }

    #[test]
    fn load_returns_none_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(load(&path).is_none());
    }
}
