//! r.md #61: 「終了処理中…」オーバーレイ。
//!
//! 終了シーケンスは子プロセスに **プラグインを畳ませて exit するまで**
//! 待つ (最大 [`crate::shutdown::DRAIN_TIMEOUT`])。VCV Rack のような重量級を
//! 何枚も載せていると体感できる時間になるので、無表示で固まっているように
//! 見せない — 実ログに「応答が無いとき ✕ を 4 回押して強制終了した」記録がある。
//!
//! 見せ方は書き出し進捗 (`export_overlay`) と同じ idiom: `close_on_*` を倒した
//! true modal で下の UI 操作を遮断する。違いは **キャンセルできない**こと
//! (子はもう畳み始めている) と、進捗が「あと何秒で諦めるか」であること。

use daw_ui_core::{ModalStyle, Ui};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::Rect;

use crate::app::AppData;

const PANEL_W: f32 = 360.0;
const PANEL_H: f32 = 110.0;
const PAD: f32 = 16.0;
const BAR_H: f32 = 6.0;
const MODAL_ID: &str = "shutdown_progress";

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, _screen: PhysicalSize) {
    // 経過秒はこのオーバーレイの表示にしか使わないので、frame の `now` を
    // 引き回さずここで取る。
    let now = std::time::Instant::now();
    if !app.shutdown.is_draining() {
        if ui.is_modal_open(MODAL_ID) {
            ui.close_modal(MODAL_ID);
        }
        return;
    }
    if !ui.is_modal_open(MODAL_ID) {
        ui.open_modal(MODAL_ID);
    }
    // 中断させない: 子プロセスは既にプラグインを畳み始めていて、途中で
    // 「やめた」に戻す方法が無い (deactivate 済みの plugin を activate し直すのは
    // 終了の取り消しではなく別の復旧手順)。Esc / 外クリック / ✕ をすべて倒す。
    let style = ModalStyle {
        close_on_outside_click: false,
        close_on_escape: false,
        ..ModalStyle::from_palette(&app.theme.core)
    };
    let detail = app.shutdown_status_line(now);
    // determinate な進捗は作れない (プラグインの `deactivate` に進捗の概念が無い)
    // ので、**あとどれくらいで諦めるか** を見せる。正確に出せる唯一の量。
    let ratio = (app.shutdown.elapsed(now).as_secs_f32()
        / crate::shutdown::DRAIN_TIMEOUT.as_secs_f32())
    .clamp(0.0, 1.0);
    ui.modal(MODAL_ID, (PANEL_W, PANEL_H), &style, None, move |ui, panel| {
        let p = ui.palette();
        ui.label_at(
            "shutdown_title",
            "終了処理中…",
            panel.x + PAD,
            panel.y + PAD,
            16.0,
            p.text,
        );
        ui.label_at(
            "shutdown_detail",
            &detail,
            panel.x + PAD,
            panel.y + PAD + 30.0,
            13.0,
            p.text_dim,
        );
        let bar_y = panel.y + panel.h - PAD - BAR_H;
        let bar_w = panel.w - PAD * 2.0;
        ui.panel(
            "shutdown_bar_bg",
            Rect { x: panel.x + PAD, y: bar_y, w: bar_w, h: BAR_H },
            p.inset_bg,
            3.0,
        );
        ui.panel(
            "shutdown_bar_fill",
            Rect { x: panel.x + PAD, y: bar_y, w: bar_w * ratio, h: BAR_H },
            p.accent,
            3.0,
        );
    });
}
