//! 設定 window の「テーマ」ページ (r.md #48): テーマの一覧と、追加先の案内。
//!
//! 選択中テーマの SSoT は `AppData.theme.id`。window は背景を暗転しないので、行を click した
//! 瞬間に背後のアレンジ / ピアノロール / ミキサーが切り替わるのを見ながら選べる。

use std::cell::Cell;

use daw_ui_core::{Edit, Ui, WidgetId};
use daw_ui_renderer::{Color, Rect, RectCommand};

use super::page::Page;
use crate::app::{AppData, AppEvent};
use crate::theme::ThemeSource;

const ROW_H: f32 = 24.0;
const ROW_GAP: f32 = 1.0;

pub(super) fn draw(ui: &mut Ui<'_, AppData>, pg: &mut Page<'_>) {
    let app = pg.app;
    let p = &app.theme.core;
    pg.heading(ui, "theme", "テーマ", true);

    // 一覧はこのページが画面に出たときに `AppData::settings_page_shown` が取ったキャッシュ。
    // ここで `available_themes()` を呼ぶと **毎フレーム read_dir + JSON パース**になる。
    let themes = &app.ui_ephemeral.available_themes;
    let row_total = ROW_H + ROW_GAP;
    let top = pg.y;
    let clicked = Cell::new(None::<usize>);
    let pointer = ui.pointer();
    // 開いている popup (メニュー) の下の行は hover も click も受けない。
    let hit_pos = if ui.has_open_popups() { None } else { pointer.pos };
    // 見えている行だけ描く (ユーザーテーマが多くても page の scroll 範囲外は描かない)。
    for i in ui.visible_rows(top, row_total, themes.len()) {
        let theme = &themes[i];
        let row_rect = Rect { x: pg.x, y: top + i as f32 * row_total, w: pg.w, h: ROW_H };
        let inside = hit_pos.is_some_and(|(px, py)| row_rect.contains(px, py));
        let is_current = theme.id == app.theme.id;
        let bg = if is_current {
            Some(p.accent)
        } else if inside {
            Some(p.control_hover)
        } else {
            None
        };
        if let Some(fill) = bg {
            ui.push_rect(RectCommand {
                rect: row_rect,
                fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [3.0; 4],
                clip_rect: None,
            });
        }
        let text_color = if is_current { p.ink_on_accent() } else { p.text };
        ui.label_at(
            ("settings_theme_row", i),
            &theme.name,
            row_rect.x + 10.0,
            row_rect.y + (ROW_H - 12.0) * 0.5,
            12.0,
            text_color,
        );
        // ユーザーテーマは出どころ (ファイル名) を右に dim で添える。
        if let ThemeSource::User(path) = &theme.source {
            let file = path.file_name().and_then(|s| s.to_str()).unwrap_or_default().to_string();
            let dim = if is_current { p.ink_on_accent() } else { p.text_faint };
            // 右寄せ位置は実測幅で決める (固定幅近似だと日本語ファイル名でずれる)。
            let w = ui.measure_text(&file, 10.0);
            ui.label_at(
                ("settings_theme_src", i),
                &file,
                row_rect.x + (row_rect.w - 8.0 - w).max(0.0),
                row_rect.y + (ROW_H - 10.0) * 0.5,
                10.0,
                dim,
            );
        }
        if ui.primary_click(WidgetId::ROOT.child((b"settings_theme_row", i)), inside).clicked {
            clicked.set(Some(i));
        }
    }
    pg.y = top + themes.len() as f32 * row_total + 8.0;

    if let Some(index) = clicked.get() {
        let id = themes[index].id.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetTheme(id.clone()));
        }));
    }

    // 「テーマの置き場所」の案内。ユーザーが自分でテーマを増やせることを画面上で示す。
    let hint = app.ui_prefs.app_dirs.as_ref().map_or_else(
        || "テーマの追加先が未設定です".to_string(),
        |d| format!("テーマ追加: {}\\*.json (開き直すと反映)", d.themes_dir().display()),
    );
    pg.hint(ui, "theme_dir", &hint);
}
