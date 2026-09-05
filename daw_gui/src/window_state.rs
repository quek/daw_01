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
    /// r.md #107: Video Preview 窓の直近 geometry。 `None` = まだ一度も開いて
    /// いない (= 次に開くときは映像解像度から既定サイズを決める)。 旧ファイルには
    /// 無いので `default`。
    #[serde(default)]
    pub preview: Option<PreviewGeometry>,
}

/// r.md #107: Video Preview 窓の位置 / サイズ。 main 窓と同じく位置は physical
/// (screen 座標)、 サイズは logical。 開き直し・次回起動で復元する。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PreviewGeometry {
    pub x: i32,
    pub y: i32,
    pub width: f64,
    pub height: f64,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 1280.0,
            height: 800.0,
            x: 100,
            y: 100,
            maximized: false,
            preview: None,
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

/// screen 座標の矩形 (physical px)。 モニタと窓の外枠の両方に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 「窓が掴める」 と見なす、 いずれかのモニタとの交差の最小幅 / 高さ (px)。
/// 幅は最小化ボタン群ぶん、 高さはタイトルバー 1 本ぶんが出ていれば掴める。
const MIN_VISIBLE_W: i32 = 120;
const MIN_VISIBLE_H: i32 = 40;
/// 窓の上端がモニタ上端より上に出ていてよい量 (px)。 Windows は最大化 / スナップした
/// 窓の外枠を不可視の frame ぶん (-8, -8) だけ画面外へ出す。 これを「タイトルバーが
/// 画面外」 と見なすと、 最大化して閉じた窓が起動のたびに主モニタへ寄せられる
/// (実機で `x: -8, y: -8` を観測)。 実際に掴めなくなるのはタイトルバー丸ごと
/// (≈ 30px) が出たときなので、 それより手前で線を引く。
const TOP_EDGE_TOLERANCE: i32 = 16;

/// r.md #106: 窓の外枠 `win` がどのモニタにも掴める程度に見えていなければ、
/// `primary` の中央へ収めた矩形を返す (モニタより大きければ縮める)。 見えていれば
/// `None` = 触らない。 起動時 (保存位置の復元) と稼働中のモニタ構成変更
/// (`WM_DISPLAYCHANGE`) の両方がこの 1 本で判定する。
///
/// 「見えている」 = いずれかのモニタとの交差が `MIN_VISIBLE_W × MIN_VISIBLE_H` 以上
/// **かつ 窓の上端 (タイトルバー) がそのモニタの上端から `TOP_EDGE_TOLERANCE` 以内**。
/// 上端がモニタ外だと本体が見えていてもドラッグで動かせないので、 それも画面外扱いにする。
#[must_use]
pub fn place_on_screen(
    win: ScreenRect,
    monitors: &[ScreenRect],
    primary: ScreenRect,
) -> Option<ScreenRect> {
    let visible = monitors.iter().any(|m| {
        let ix0 = win.x.max(m.x);
        let iy0 = win.y.max(m.y);
        let ix1 = (win.x + win.w).min(m.x + m.w);
        let iy1 = (win.y + win.h).min(m.y + m.h);
        ix1 - ix0 >= MIN_VISIBLE_W
            && iy1 - iy0 >= MIN_VISIBLE_H
            && win.y >= m.y - TOP_EDGE_TOLERANCE
            && win.y < m.y + m.h
    });
    if visible {
        return None;
    }
    let w = win.w.min(primary.w).max(1);
    let h = win.h.min(primary.h).max(1);
    Some(ScreenRect {
        x: primary.x + (primary.w - w) / 2,
        y: primary.y + (primary.h - h) / 2,
        w,
        h,
    })
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
            preview: None,
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

    const fn r(x: i32, y: i32, w: i32, h: i32) -> ScreenRect {
        ScreenRect { x, y, w, h }
    }

    #[test]
    fn place_on_screen_relocates_only_when_window_is_off_every_monitor() {
        // r.md #106: 2 画面 (主 1920×1080 + 右 1920×1080) → 主だけ。
        let primary = r(0, 0, 1920, 1080);
        let two = [primary, r(1920, 0, 1920, 1080)];
        let one = [primary];
        let on_second = r(2100, 200, 1280, 800);

        // 2 画面のときは第 2 画面上の窓は見えている → 触らない。
        assert_eq!(place_on_screen(on_second, &two, primary), None);
        // 1 画面へ戻したら見えない → 主画面の中央へ (サイズは維持)。
        assert_eq!(
            place_on_screen(on_second, &one, primary),
            Some(r((1920 - 1280) / 2, (1080 - 800) / 2, 1280, 800))
        );
        // 主画面の右端に 200px だけ掛かっている (掴める) → 触らない。
        assert_eq!(place_on_screen(r(1720, 100, 1280, 800), &one, primary), None);
        // 右端 50px しか掛かっていない (掴めない) → 移動。
        assert!(place_on_screen(r(1870, 100, 1280, 800), &one, primary).is_some());
        // 本体は見えているが上端 (タイトルバー) が画面上端の外 → 移動。
        assert!(place_on_screen(r(100, -60, 1280, 800), &one, primary).is_some());
        // 最大化 / スナップの不可視 frame ぶん (-8, -8) は画面外ではない (実機で観測)。
        assert_eq!(place_on_screen(r(-8, -8, 1936, 1096), &one, primary), None);
        // モニタより大きい窓はモニタに収まるまで縮める。
        assert_eq!(
            place_on_screen(r(5000, 5000, 3000, 2000), &one, primary),
            Some(r(0, 0, 1920, 1080))
        );
    }
}
