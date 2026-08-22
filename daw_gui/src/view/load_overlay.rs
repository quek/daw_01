// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! プロジェクトロード / 非同期保存の進捗を画面上端中央に出す **非ブロック**
//! overlay (`docs/plan_progress_streaming.md`)。modal ではないので
//! 構造の操作 (スクロール / 編集) はそのまま続けられる。
//! - ロード中 (`load_progress == Some`): determinate バー (done/total)。
//! - 非同期保存中 (`is_async_save_pending`): indeterminate (「保存中…」のみ)。

use daw_ui_core::Ui;
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::Rect;

use crate::app::AppData;

/// カードの透過度。 上端中央に浮く非モーダルの progress カード (elevation 2) は、
/// 軽く透かして下の内容を見せる。 面そのものは `panel_raised`。
const CARD_ALPHA: f32 = 0.94;

const SAVE_LABEL: &str = "\u{4fdd}\u{5b58}\u{4e2d}\u{2026}"; // 保存中…

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: PhysicalSize) {
    // 進捗 (determinate: ロード / プラグイン走査) を優先。 無ければ非同期保存
    // (indeterminate)。 ラベルは `load_progress_label` が文脈別に持つ。
    if let Some((done, total)) = app.media.load_progress {
        if total > 0 {
            let pct = (done as f32 / total as f32).clamp(0.0, 1.0);
            let label = format!("{}  {done}/{total}", app.media.load_progress_label);
            draw_overlay(ui, screen, &label, Some(pct));
        }
    } else if app.is_async_save_pending() {
        draw_overlay(ui, screen, SAVE_LABEL, None);
    }
}

/// 上端中央に小パネル + ラベル + (任意の) determinate バーを描く。
/// `pct == None` は indeterminate (バー無し)。
fn draw_overlay(ui: &mut Ui<'_, AppData>, screen: PhysicalSize, label: &str, pct: Option<f32>) {
    // カードはパレットのクローム面 (`panel_raised`) なので、 ラベルは本文インク `text`。
    let p = ui.palette();
    let w = 300.0;
    let h = if pct.is_some() { 46.0 } else { 32.0 };
    let x = ((screen.width as f32) - w) * 0.5;
    let y = 12.0;
    ui.panel("load_overlay_bg", Rect { x, y, w, h }, p.panel_raised.with_alpha(CARD_ALPHA), 6.0);
    ui.label_at("load_overlay_label", label, x + 14.0, y + 9.0, 13.0, p.text);
    if let Some(pct) = pct {
        let bar_x = x + 14.0;
        let bar_y = y + h - 15.0;
        let bar_w = w - 28.0;
        let bar_h = 6.0;
        // 進捗バー: 溝 (`inset_bg`) を彫って accent で満たす (export_overlay と同形)。
        ui.panel(
            "load_overlay_bar_bg",
            Rect { x: bar_x, y: bar_y, w: bar_w, h: bar_h },
            p.inset_bg,
            3.0,
        );
        ui.panel(
            "load_overlay_bar",
            Rect { x: bar_x, y: bar_y, w: bar_w * pct, h: bar_h },
            p.accent,
            3.0,
        );
    }
}
