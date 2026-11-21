//! 設定 window の「オーディオ」ページ (r.md #156)。

use daw_ui_core::{Edit, Ui};

use super::page::Page;
use crate::app::{AppData, AppEvent};

pub(super) fn draw(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    draw_render_ahead(ui, pg, true);
}

/// 出力の先読みの on / off (`UiPrefs::render_ahead`)。
fn draw_render_ahead(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>, first: bool) {
    let app = pg.app;
    pg.heading(ui, "render_ahead", "先読み", first);
    let rect = pg.full_row();
    ui.checkbox_at(
        "settings_render_ahead",
        rect,
        app.ui_prefs.render_ahead,
        "先読みで音切れを防ぐ",
        |on| Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::SetRenderAhead(on))),
    );
    pg.hint(
        ui,
        "render_ahead",
        "2 周期分を先に描いておきます。操作への反応が 2 周期分遅れます。切り替えは停止中に効きます。",
    );
}
