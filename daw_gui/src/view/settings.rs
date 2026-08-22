//! 設定 window (r.md #48): アプリ全体の設定。現状はテーマ選択。
//!
//! **常駐する移動・リサイズ可能な floating window** として実装する (grill-me 2026-08-15 で確定):
//! - 外クリックでは閉じない。 Esc / ✕ / Edit メニュー「設定...」 で閉じる。
//! - タイトルバードラッグで移動、 右端・下端・右下隅ドラッグでリサイズ。
//! - 位置・サイズ・開閉状態は `app_config.json` に永続 (再起動を跨いで復元)。
//! - **真の非ブロッキング**: 背景を暗転しないので、テーマ行を click した瞬間に背後の
//!   アレンジ / ピアノロール / ミキサーが切り替わるのを見ながら選べる。これが
//!   modal ダイアログを採らなかった理由 ([`Ui::reserve_floating_region`] /
//!   [`Ui::with_floating_region`] を使う。`undo_history` と同じ機構)。
//!
//! 開閉/位置/サイズの SSoT は `AppData.ui_prefs` (`settings_open` / `settings_rect`)、
//! 選択中テーマの SSoT は `AppData.theme.id`。

use std::cell::Cell;

use daw_ui_core::{DragKind, Edit, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent};
use crate::theme::ThemeSource;

/// メニューバー高 (root::MENU_H のミラー)。 初期位置 / 縦 clamp の基準。
const MENU_H: f32 = 24.0;
/// 初期表示位置の上端 (MENU_H + TRANSPORT_H(44) + 8)。
const PANEL_TOP: f32 = 76.0;
const DEFAULT_W: f32 = 320.0;
const DEFAULT_H: f32 = 300.0;
const MIN_W: f32 = 240.0;
const MIN_H: f32 = 160.0;
const TITLE_H: f32 = 24.0;
const CLOSE_W: f32 = 26.0;
const ROW_H: f32 = 24.0;
const ROW_GAP: f32 = 1.0;
/// セクション見出し (「テーマ」) の高さ。
const SECTION_H: f32 = 22.0;
/// 下端に出す「テーマの置き場所」ヒント行の高さ。
const HINT_H: f32 = 30.0;
/// 端リサイズ grab 帯の幅 (px)。
const RESIZE_MARGIN: f32 = 6.0;
/// 右下隅リサイズ grip の一辺 (px)。
const CORNER: f32 = 14.0;
const SCROLL_ID: &str = "settings_theme_scroll";
/// `scroll_area` 内部 scrollbar 幅 (mirror)。overflow 時に行幅から差し引く。
const SCROLLBAR_W: f32 = 10.0;

/// 初回 (未配置) の既定 window rect: 画面中央やや上。
fn default_rect(screen: Rect) -> Rect {
    let w = DEFAULT_W.min((screen.w - 32.0).max(MIN_W));
    let h = DEFAULT_H.min((screen.h - PANEL_TOP - 16.0).max(MIN_H));
    Rect { x: screen.x + (screen.w - w) * 0.5, y: screen.y + PANEL_TOP, w, h }
}

/// 保存 rect をサイズ最小・タイトルバー可視の範囲に clamp (モニタ変更等で画面外に
/// 保存されていても復帰できるように)。
fn clamp_to_screen(r: Rect, screen: Rect) -> Rect {
    let w = r.w.clamp(MIN_W, screen.w.max(MIN_W));
    let h = r.h.clamp(MIN_H, screen.h.max(MIN_H));
    let x = r.x.clamp(screen.x + 60.0 - w, screen.x + screen.w - 60.0);
    let y = r.y.clamp(screen.y + MENU_H, (screen.y + screen.h - TITLE_H).max(screen.y + MENU_H));
    Rect { x, y, w, h }
}

/// 現在の committed window rect (未配置なら既定)。 reserve / draw の両方が
/// **同じ**基準として使う。
fn window_rect(app: &AppData, screen: Rect) -> Rect {
    let r = app.ui_prefs.settings_rect.unwrap_or_else(|| default_rect(screen));
    clamp_to_screen(r, screen)
}

/// build_root の **背景 widget 描画より前** に呼ぶ: window が開いていれば、その rect 分だけ
/// 背後の pointer を占有する (= window の上の click がアレンジ等に漏れない)。
pub fn reserve(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    if !app.ui_prefs.settings_open {
        return;
    }
    ui.reserve_floating_region(window_rect(app, screen));
}

/// build_root の **末尾近く** (背景描画の後) に呼ぶ: window 本体を inline 描画する
/// (z-order 最前面)。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    if !app.ui_prefs.settings_open {
        return;
    }
    ui.with_floating_region(|ui| draw_window(app, ui, screen));
}

fn draw_window(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    let committed = window_rect(app, screen);
    // drag / resize の delta を committed に載せた「表示 rect」。 release まで
    // ui_prefs は書き換えず、 committed 固定 + delta で追従する。
    let mut rect = committed;
    let mut commit = false;

    // ---- タイトルバードラッグ = 移動 (✕ ボタン領域は除外) ----
    let title_drag = Rect {
        x: committed.x,
        y: committed.y,
        w: (committed.w - CLOSE_W).max(0.0),
        h: TITLE_H,
    };
    if let Some(d) = ui.take_drag_in_rect("settings_move", title_drag) {
        rect.x = committed.x + d.delta.0;
        rect.y = committed.y + d.delta.1;
        commit |= matches!(d.kind, DragKind::Released);
    }
    // ---- 右下隅 = 幅+高さリサイズ ----
    let corner = Rect {
        x: committed.x + committed.w - CORNER,
        y: committed.y + committed.h - CORNER,
        w: CORNER,
        h: CORNER,
    };
    if let Some(d) = ui.take_drag_in_rect("settings_size_br", corner) {
        rect.w = (committed.w + d.delta.0).max(MIN_W);
        rect.h = (committed.h + d.delta.1).max(MIN_H);
        commit |= matches!(d.kind, DragKind::Released);
    }
    // ---- 右端 = 幅リサイズ (隅を除く) ----
    let right_edge = Rect {
        x: committed.x + committed.w - RESIZE_MARGIN,
        y: committed.y + TITLE_H,
        w: RESIZE_MARGIN,
        h: (committed.h - TITLE_H - CORNER).max(0.0),
    };
    if let Some(d) = ui.take_drag_in_rect("settings_size_r", right_edge) {
        rect.w = (committed.w + d.delta.0).max(MIN_W);
        commit |= matches!(d.kind, DragKind::Released);
    }
    // ---- 下端 = 高さリサイズ (隅を除く) ----
    let bottom_edge = Rect {
        x: committed.x,
        y: committed.y + committed.h - RESIZE_MARGIN,
        w: (committed.w - CORNER).max(0.0),
        h: RESIZE_MARGIN,
    };
    if let Some(d) = ui.take_drag_in_rect("settings_size_b", bottom_edge) {
        rect.h = (committed.h + d.delta.1).max(MIN_H);
        commit |= matches!(d.kind, DragKind::Released);
    }

    rect = clamp_to_screen(rect, screen);

    draw_chrome_and_body(app, ui, rect);

    // release で確定 → ui_prefs へ書いて app_config に保存。
    if commit {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_prefs.settings_rect = Some(rect);
            app.persist_app_config();
        }));
    }
}

fn draw_chrome_and_body(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let p = &app.theme.core;

    // 背景 + 枠 + タイトルバー + ✕。
    ui.push_rect(RectCommand {
        rect,
        fill: p.panel,
        border: p.border,
        border_width: 1.0,
        radius: [6.0; 4],
        clip_rect: None,
    });
    ui.panel(
        "settings_titlebar",
        Rect { x: rect.x, y: rect.y, w: rect.w, h: TITLE_H },
        p.header,
        6.0,
    );
    ui.label_at("settings_title", "設定", rect.x + 12.0, rect.y + 7.0, 13.0, p.text);
    ui.button_at(
        "settings_close",
        "\u{2715}",
        Rect { x: rect.x + rect.w - CLOSE_W, y: rect.y + 4.0, w: 20.0, h: 18.0 },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ToggleSettings)),
    );

    // セクション見出し「テーマ」。
    let section_y = rect.y + TITLE_H + 4.0;
    ui.label_at(
        "settings_sec_theme",
        "テーマ",
        rect.x + 12.0,
        section_y + (SECTION_H - 12.0) * 0.5,
        12.0,
        p.text_dim,
    );

    // テーマ一覧 (見出しの下、 下端はヒント行とリサイズ帯のぶん残す)。
    // 右端 RESIZE_MARGIN を空けないと scroll_area の縦スクロールバーがリサイズ帯と
    // 重なり、 サムを掴もうとすると窓幅リサイズが始まる (undo_history と同じ配慮)。
    let list_rect = Rect {
        x: rect.x + 1.0,
        y: section_y + SECTION_H,
        w: (rect.w - 1.0 - RESIZE_MARGIN).max(0.0),
        h: (rect.h - TITLE_H - SECTION_H - 4.0 - HINT_H - RESIZE_MARGIN).max(0.0),
    };
    draw_theme_list(app, ui, list_rect);

    // 「テーマの置き場所」ヒント。 ユーザーが自分でテーマを増やせることを画面上で示す。
    let hint_y = rect.y + rect.h - HINT_H - RESIZE_MARGIN * 0.5;
    let hint = app
        .ui_prefs
        .app_dirs
        .as_ref()
        .map_or_else(
            || "テーマの追加先が未設定です".to_string(),
            |d| format!("テーマ追加: {}\\*.json (開き直すと反映)", d.themes_dir().display()),
        );
    ui.label_at("settings_hint", &hint, rect.x + 12.0, hint_y, 10.0, p.text_faint);

    // 右下隅のリサイズ grip (斜めドット、 視認用)。
    for i in 0..3 {
        let off = 4.0 + 3.0 * i as f32;
        ui.panel(
            ("settings_grip", i),
            Rect { x: rect.x + rect.w - off - 2.0, y: rect.y + rect.h - off - 2.0, w: 2.0, h: 2.0 },
            p.text_faint,
            0.0,
        );
    }
}

fn draw_theme_list(app: &AppData, ui: &mut Ui<'_, AppData>, list_rect: Rect) {
    let p = &app.theme.core;
    // 一覧は window を開いたときに `AppData::refresh_available_themes` が取ったキャッシュ。
    // ここで `available_themes()` を呼ぶと **毎フレーム read_dir + JSON パース**になる。
    let themes = &app.ui_ephemeral.available_themes;

    let row_total = ROW_H + ROW_GAP;
    let n = themes.len();
    let content_h = n as f32 * row_total;
    let needs_scrollbar = content_h > list_rect.h;
    let row_w = if needs_scrollbar { (list_rect.w - SCROLLBAR_W).max(0.0) } else { list_rect.w };

    let clicked = Cell::new(None::<usize>);
    let pointer = ui.pointer();
    ui.scroll_area(SCROLL_ID, list_rect, (list_rect.w, content_h), |ui, offset| {
        for (i, theme) in themes.iter().enumerate() {
            let row_y = list_rect.y - offset.1 + i as f32 * row_total;
            let row_rect = Rect { x: list_rect.x, y: row_y, w: row_w, h: ROW_H };
            // scroll 範囲外は描かない (行数は高々数十だが list_rect の外へはみ出させない)。
            if row_rect.y + ROW_H < list_rect.y || row_rect.y > list_rect.y + list_rect.h {
                continue;
            }
            let inside = pointer.pos.is_some_and(|(px, py)| row_rect.contains(px, py));
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
                let file =
                    path.file_name().and_then(|s| s.to_str()).unwrap_or_default().to_string();
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
            if inside && pointer.primary_just_released {
                clicked.set(Some(i));
            }
        }
    });

    if let Some(index) = clicked.get() {
        let id = themes[index].id.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetTheme(id.clone()));
        }));
    }
}
