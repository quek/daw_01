//! プロジェクトロードの進捗を画面上端中央に出す **非ブロック** overlay
//! (FIXME #24 / `docs/plan_progress_streaming.md`)。modal ではないので構造の
//! 操作 (スクロール / 編集) はそのまま続けられる。`app.load_progress == Some`
//! の間だけ小さな determinate バーを描く。

use daw_ui_core::Ui;
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect};

use crate::app::AppData;

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 0.94 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const BAR_BG: Color = Color { r: 0.10, g: 0.10, b: 0.13, a: 1.0 };
const BAR_FILL: Color = Color { r: 0.36, g: 0.62, b: 0.92, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: PhysicalSize) {
    let Some((done, total)) = app.load_progress else {
        return;
    };
    if total == 0 {
        return;
    }
    let w = 300.0;
    let h = 46.0;
    let x = ((screen.width as f32) - w) * 0.5;
    let y = 12.0;
    ui.panel("load_overlay_bg", Rect { x, y, w, h }, BG, 6.0);
    ui.label_at(
        "load_overlay_label",
        &format!("\u{30d7}\u{30ed}\u{30b8}\u{30a7}\u{30af}\u{30c8}\u{8aad}\u{8fbc}\u{4e2d}  {done}/{total}"),
        x + 14.0,
        y + 9.0,
        13.0,
        TEXT,
    );
    let bar_x = x + 14.0;
    let bar_y = y + h - 15.0;
    let bar_w = w - 28.0;
    let bar_h = 6.0;
    ui.panel("load_overlay_bar_bg", Rect { x: bar_x, y: bar_y, w: bar_w, h: bar_h }, BAR_BG, 3.0);
    let pct = (done as f32 / total as f32).clamp(0.0, 1.0);
    ui.panel(
        "load_overlay_bar",
        Rect { x: bar_x, y: bar_y, w: bar_w * pct, h: bar_h },
        BAR_FILL,
        3.0,
    );
}
