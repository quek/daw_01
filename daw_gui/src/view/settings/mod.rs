//! 設定 window (r.md #48 / #156): アプリ全体の設定。左にカテゴリの一覧、右に選んだカテゴリのページ。
//!
//! **常駐する移動・リサイズ可能な floating window** として実装する (grill-me 2026-08-15 で確定):
//! - 外クリックでは閉じない。 Esc / ✕ / Edit メニュー「設定...」 で閉じる。
//! - タイトルバードラッグで移動、 右端・下端・右下隅ドラッグでリサイズ。
//! - 位置・サイズ・開閉状態・選んでいるカテゴリは `app_config.json` に永続 (再起動を跨いで復元)。
//! - **真の非ブロッキング**: 背景を暗転しないので、テーマ行を click した瞬間に背後の
//!   アレンジ / ピアノロール / ミキサーが切り替わるのを見ながら選べる。これが
//!   modal ダイアログを採らなかった理由 ([`Ui::reserve_floating_region`] /
//!   [`Ui::with_floating_region`] を使う。`undo_history` と同じ機構)。
//!
//! 開閉 / 位置 / サイズ / カテゴリの SSoT は `AppData.ui_prefs`
//! (`settings_open` / `settings_rect` / `settings_category`)。
//!
//! ページは窓より長ければ縦にスクロールする。中身は [`page::Page`] で上から積み、
//! 高さは前フレームの実測を使う (immediate-mode の lag-by-one。インスペクタと同じ)。

mod audio;
mod audio_choices;
mod page;
mod theme;
mod voicevox;

use std::cell::Cell;

use daw_ui_core::{DragKind, Edit, SCROLLBAR_W, Ui, WidgetId};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent};
use crate::state::SettingsCategory;

/// メニューバー高 (root::MENU_H のミラー)。 初期位置 / 縦 clamp の基準。
const MENU_H: f32 = 24.0;
/// 初期表示位置の上端 (MENU_H + TRANSPORT_H(44) + 8)。
const PANEL_TOP: f32 = 76.0;
const DEFAULT_W: f32 = 660.0;
/// 既定の高さ。いちばん長い「オーディオ」のページがスクロール無しで全部見える値。
const DEFAULT_H: f32 = 520.0;
/// 最小の幅。カテゴリの列 ([`SIDEBAR_W`]) を引いた残りに、ページの「項目名 + 操作部品」の行が
/// 潰れずに入る値 (項目名は省略されず、操作部品に 190px 前後が残る)。
pub const MIN_W: f32 = 520.0;
/// 最小の高さ。カテゴリの一覧が全部見え、ページに数行が出る値 (ページは縦にスクロールするので、
/// 中身の全部が入る必要は無い)。
pub const MIN_H: f32 = 240.0;
const TITLE_H: f32 = 24.0;
const CLOSE_W: f32 = 26.0;
/// floating window としての id (`reserve_floating_region` / `with_floating_region` の対)。
const REGION_ID: &str = "settings";
/// 左のカテゴリの列の幅 (px)。
const SIDEBAR_W: f32 = 132.0;
/// カテゴリ 1 行の高さ (px)。
const CATEGORY_ROW_H: f32 = 28.0;
/// 端リサイズ grab 帯の幅 (px)。
const RESIZE_MARGIN: f32 = 6.0;
/// 右下隅リサイズ grip の一辺 (px)。
const CORNER: f32 = 14.0;

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
#[must_use]
pub fn window_rect(app: &AppData, screen: Rect) -> Rect {
    let r = app.ui_prefs.settings_rect.unwrap_or_else(|| default_rect(screen));
    clamp_to_screen(r, screen)
}

/// window の中の区画 (テストが座標を求めるのにも使う)。
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    /// 左のカテゴリの列 (タイトルバーの下から window の下端まで)。
    pub sidebar: Rect,
    /// 右のページ (スクロールする領域。右端と下端はリサイズ帯のぶん空ける)。
    pub page: Rect,
}

/// window rect から区画を決める。カテゴリの列とページは横に並び、重ならない。
#[must_use]
pub fn layout(rect: Rect) -> Layout {
    let body_y = rect.y + TITLE_H;
    let body_h = (rect.h - TITLE_H).max(0.0);
    let sidebar = Rect { x: rect.x + 1.0, y: body_y, w: SIDEBAR_W, h: (body_h - 1.0).max(0.0) };
    // 右端 RESIZE_MARGIN を空けないと、ページの縦スクロールバーがリサイズ帯と重なり、
    // サムを掴もうとすると窓幅リサイズが始まる (undo_history と同じ配慮)。
    let page_x = sidebar.x + sidebar.w + 1.0;
    let page = Rect {
        x: page_x,
        y: body_y,
        w: (rect.x + rect.w - RESIZE_MARGIN - page_x).max(0.0),
        h: (body_h - RESIZE_MARGIN).max(0.0),
    };
    Layout { sidebar, page }
}

/// カテゴリの列の `index` 行目 ([`SettingsCategory::ALL`] の順) の矩形。
#[must_use]
pub fn category_row_rect(sidebar: Rect, index: usize) -> Rect {
    Rect {
        x: sidebar.x + 4.0,
        y: sidebar.y + 6.0 + index as f32 * CATEGORY_ROW_H,
        w: (sidebar.w - 8.0).max(0.0),
        h: CATEGORY_ROW_H - 2.0,
    }
}

/// build_root の **背景 widget 描画より前** に呼ぶ: window が開いていれば、その rect 分だけ
/// 背後の pointer を占有する (= window の上の click がアレンジ等に漏れない)。
///
/// ページの dropdown の一覧が開いている間は **画面全体** を占有する。一覧は window の外へはみ出して
/// 描かれるので、window の rect だけだと、はみ出した項目の click が背景の widget にも届く。
/// (一覧の外を click すると一覧が閉じるだけで、その click は背景へ行かない。)
pub fn reserve(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    if !app.ui_prefs.settings_open {
        return;
    }
    // 一覧が開いている間は他の floating window (上に重なる仮想鍵盤も含む) にも届かせない。
    if page::any_dropdown_open(ui) {
        ui.reserve_floating_region_exclusive(REGION_ID, screen);
    } else {
        ui.reserve_floating_region(REGION_ID, window_rect(app, screen));
    }
}

/// build_root の **末尾近く** (背景描画の後) に呼ぶ: window 本体を inline 描画する
/// (z-order 最前面)。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    if !app.ui_prefs.settings_open {
        return;
    }
    ui.with_floating_region(REGION_ID, |ui| draw_window(app, ui, screen));
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

    draw_chrome(app, ui, rect);
    let areas = layout(rect);
    draw_sidebar(app, ui, areas.sidebar);
    draw_page(app, ui, areas.page);
    draw_grip(app, ui, rect);

    // release で確定 → ui_prefs へ書いて app_config に保存。
    if commit {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_prefs.settings_rect = Some(rect);
            app.persist_app_config();
        }));
    }
}

/// 背景 + 枠 + タイトルバー + ✕。
fn draw_chrome(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let p = &app.theme.core;
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
}

/// 左のカテゴリの一覧。選んでいる行は accent、click でそのカテゴリのページへ替える。
fn draw_sidebar(app: &AppData, ui: &mut Ui<'_, AppData>, sidebar: Rect) {
    let p = &app.theme.core;
    // 列の面を 1 段沈めて、ページ (panel) と区別する。右端に 1px の境界線。
    ui.panel("settings_sidebar", sidebar, p.header, 0.0);
    ui.panel(
        "settings_sidebar_edge",
        Rect { x: sidebar.x + sidebar.w, y: sidebar.y, w: 1.0, h: sidebar.h },
        p.border,
        0.0,
    );

    let pointer = ui.pointer();
    // 開いている popup (メニュー / ページの dropdown の一覧) の下の行は hover も click も受けない。
    let hit_pos = if ui.has_open_popups() { None } else { pointer.pos };
    let clicked = Cell::new(None::<SettingsCategory>);
    ui.with_clip_rect(sidebar, |ui| {
        for (i, category) in SettingsCategory::ALL.into_iter().enumerate() {
            let row = category_row_rect(sidebar, i);
            let inside = hit_pos.is_some_and(|(px, py)| row.contains(px, py) && sidebar.contains(px, py));
            let is_current = category == app.ui_prefs.settings_category;
            let fill = if is_current {
                Some(p.accent)
            } else if inside {
                Some(p.control_hover)
            } else {
                None
            };
            if let Some(fill) = fill {
                ui.push_rect(RectCommand {
                    rect: row,
                    fill,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [4.0; 4],
                    clip_rect: None,
                });
            }
            let color = if is_current { p.ink_on_accent() } else { p.text };
            ui.label_at(
                ("settings_category", category),
                category.label(),
                row.x + 10.0,
                row.y + (row.h - 13.0 * 1.2) * 0.5,
                13.0,
                color,
            );
            if ui.primary_click(WidgetId::ROOT.child((b"settings_category", category)), inside).clicked {
                clicked.set(Some(category));
            }
        }
    });
    if let Some(category) = clicked.get() {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetSettingsCategory(category));
        }));
    }
}

/// ページの実測の高さ (前フレーム)。カテゴリごとに持つ。
#[derive(Default)]
struct PageExtent {
    content_h: f32,
}

/// 右のページ。中身が `area` より長ければ縦にスクロールする。
fn draw_page(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let category = app.ui_prefs.settings_category;
    // スクロール位置と実測の高さはカテゴリごと (切り替えて戻ると元の位置に居る)。
    let extent_id = WidgetId::ROOT.child((b"settings_page_extent", category));
    let content_h = ui.widget_state::<PageExtent>(extent_id).content_h;
    // 縦スクロールバーが出るフレームは、その幅を中身から引く (部品がバーの下に入らない)。
    let bar_w = if content_h > area.h { SCROLLBAR_W } else { 0.0 };
    let measured = Cell::new(0.0_f32);
    ui.scroll_area(("settings_page", category), area, (area.w, content_h), |ui, offset| {
        let top = area.y - offset.1;
        let mut pg = page::Page {
            app,
            x: area.x + page::PAD,
            w: (area.w - bar_w - page::PAD * 2.0).max(0.0),
            y: top + page::PAD,
        };
        match category {
            SettingsCategory::Audio => audio::draw(ui, &mut pg),
            SettingsCategory::Voicevox => voicevox::draw(ui, &mut pg),
            SettingsCategory::Theme => theme::draw(ui, &mut pg),
        }
        measured.set(pg.y + page::PAD - top);
    });
    let measured = measured.get();
    if (measured - content_h).abs() > 0.5 {
        ui.widget_state::<PageExtent>(extent_id).content_h = measured;
        // 高さが変わったフレームの次で、スクロール範囲とバーの有無を合わせ直す。
        ui.request_redraw();
    }
}

/// 右下隅のリサイズ grip (斜めドット、 視認用)。
fn draw_grip(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let p = &app.theme.core;
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
