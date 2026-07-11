//! Undo 履歴 window (r.md #29): 「よく DAW にある編集履歴リスト」。 行を click
//! するとその state へ一発で Undo / Redo する。
//!
//! **常駐する移動・リサイズ可能な floating window** として実装する (grill-me
//! 2026-07-11 で確定):
//! - 外クリックでは閉じない。 Esc / ✕ / View メニュー / Ctrl+Alt+Z で閉じる。
//! - タイトルバードラッグで移動、 右端・下端・右下隅ドラッグでリサイズ。
//! - 位置・サイズ・開閉状態は app_config.json に永続 (再起動を跨いで復元)。
//! - リストは **最新が上**、 現在行へ auto-scroll。
//! - **真の非ブロッキング**: window を開いたままでも、 window の外はマウス操作
//!   できる (背後のアレンジでクリップ移動等)。 これは daw-ui の
//!   [`Ui::reserve_floating_region`] / [`Ui::with_floating_region`] で実現する
//!   (open_overlay の「背景全体を占有する capture_input modal」 とは別機構)。
//!
//! 開閉/位置/サイズの SSoT は `AppData.ui_prefs`
//! (`undo_history_open` / `undo_history_rect`)。 popup_layer は使わず inline に
//! 描く (背景描画の後に呼ぶことで z-order 最前面 = アレンジの上)。

use std::cell::Cell;

use daw_ui_core::{DragKind, Edit, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};
use crate::theme;

use crate::app::{AppData, AppEvent};

/// メニューバー高 (root::MENU_H のミラー)。 初期位置 / 縦 clamp の基準。
const MENU_H: f32 = 24.0;
/// 初期表示位置の上端 (MENU_H + TRANSPORT_H(44) + 8)。
const PANEL_TOP: f32 = 76.0;
const DEFAULT_W: f32 = 260.0;
const DEFAULT_H: f32 = 360.0;
const MIN_W: f32 = 180.0;
const MIN_H: f32 = 120.0;
const TITLE_H: f32 = 24.0;
const CLOSE_W: f32 = 26.0;
const ROW_H: f32 = 20.0;
const ROW_GAP: f32 = 1.0;
/// 端リサイズ grab 帯の幅 (px)。
const RESIZE_MARGIN: f32 = 6.0;
/// 右下隅リサイズ grip の一辺 (px)。
const CORNER: f32 = 14.0;
const SCROLL_ID: &str = "undo_history_scroll";
/// `scroll_area` 内部 scrollbar 幅 (mirror)。overflow 時に行幅から差し引く。
const SCROLLBAR_W: f32 = 10.0;

/// 初回 (未配置) の既定 window rect: 画面右上。
fn default_rect(screen: Rect) -> Rect {
    let w = DEFAULT_W.min((screen.w - 32.0).max(MIN_W));
    let h = DEFAULT_H.min((screen.h - PANEL_TOP - 16.0).max(MIN_H));
    Rect { x: screen.x + screen.w - w - 16.0, y: screen.y + PANEL_TOP, w, h }
}

/// 保存 rect をサイズ最小・タイトルバー可視の範囲に clamp (モニタ変更等で
/// 画面外に保存されていても復帰できるように)。
fn clamp_to_screen(r: Rect, screen: Rect) -> Rect {
    let w = r.w.clamp(MIN_W, screen.w.max(MIN_W));
    let h = r.h.clamp(MIN_H, screen.h.max(MIN_H));
    // 横は最低 60px、 縦はタイトルバーが画面内に残るように位置を寄せる。
    let x = r.x.clamp(screen.x + 60.0 - w, screen.x + screen.w - 60.0);
    let y = r.y.clamp(screen.y + MENU_H, (screen.y + screen.h - TITLE_H).max(screen.y + MENU_H));
    Rect { x, y, w, h }
}

/// 現在の committed window rect (未配置なら既定)。 reserve / draw の両方が
/// **同じ**基準として使う。
fn window_rect(app: &AppData, screen: Rect) -> Rect {
    let r = app.ui_prefs.undo_history_rect.unwrap_or_else(|| default_rect(screen));
    clamp_to_screen(r, screen)
}

/// build_root の **背景 widget 描画より前** に呼ぶ: window が開いていれば、
/// その rect 分だけ背後の pointer を占有 (= window の上の click / hover / scroll が
/// アレンジ等に漏れない)。 window の外は通常どおり操作できる。
pub fn reserve(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    if !app.ui_prefs.undo_history_open {
        return;
    }
    ui.reserve_floating_region(window_rect(app, screen));
}

/// build_root の **末尾近く** (背景描画の後) に呼ぶ: window 本体を inline 描画する
/// (z-order 最前面)。 window 内 widget は raw pointer で操作できる。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    if !app.ui_prefs.undo_history_open {
        return;
    }
    ui.with_floating_region(|ui| draw_window(app, ui, screen));
}

fn draw_window(app: &AppData, ui: &mut Ui<'_, AppData>, screen: Rect) {
    let committed = window_rect(app, screen);
    // drag / resize の delta を committed に載せた「表示 rect」。 release まで
    // ui_prefs は書き換えず、 committed 固定 + delta で追従する (delta が累積
    // 二重適用にならない)。
    let mut rect = committed;
    let mut commit = false;

    // ---- タイトルバードラッグ = 移動 (✕ ボタン領域は除外) ----
    let title_drag = Rect {
        x: committed.x,
        y: committed.y,
        w: (committed.w - CLOSE_W).max(0.0),
        h: TITLE_H,
    };
    if let Some(d) = ui.take_drag_in_rect("undohist_move", title_drag) {
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
    if let Some(d) = ui.take_drag_in_rect("undohist_size_br", corner) {
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
    if let Some(d) = ui.take_drag_in_rect("undohist_size_r", right_edge) {
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
    if let Some(d) = ui.take_drag_in_rect("undohist_size_b", bottom_edge) {
        rect.h = (committed.h + d.delta.1).max(MIN_H);
        commit |= matches!(d.kind, DragKind::Released);
    }

    rect = clamp_to_screen(rect, screen);

    draw_chrome_and_list(app, ui, rect);

    // release で確定 → ui_prefs へ書いて app_config に保存。
    if commit {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_prefs.undo_history_rect = Some(rect);
            app.persist_app_config();
        }));
    }
}

fn draw_chrome_and_list(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    // 背景 + 枠 + タイトルバー + ✕。
    ui.push_rect(RectCommand {
        rect,
        fill: theme::PANEL,
        border: theme::BORDER,
        border_width: 1.0,
        radius: [6.0; 4],
        clip_rect: None,
    });
    ui.panel(
        "undohist_titlebar",
        Rect { x: rect.x, y: rect.y, w: rect.w, h: TITLE_H },
        theme::HEADER,
        6.0,
    );
    ui.label_at(
        "undohist_title",
        "編集履歴",
        rect.x + 12.0,
        rect.y + 7.0,
        13.0,
        theme::TEXT,
    );
    ui.button_at(
        "undohist_close",
        "\u{2715}",
        Rect { x: rect.x + rect.w - CLOSE_W, y: rect.y + 4.0, w: 20.0, h: 18.0 },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ToggleUndoHistory)),
    );

    // リスト領域 (タイトル下、 下端はリサイズ帯のぶん残す)。
    let list_rect = Rect {
        x: rect.x + 1.0,
        y: rect.y + TITLE_H,
        w: (rect.w - 2.0).max(0.0),
        h: (rect.h - TITLE_H - 2.0).max(0.0),
    };
    let labels = app.song_doc.history_labels();
    let current = app.song_doc.history_current();
    draw_list(app, ui, list_rect, &labels, current);

    // 右下隅のリサイズ grip (斜めドット、 視認用)。
    for i in 0..3 {
        let off = 4.0 + 3.0 * i as f32;
        ui.panel(
            ("undohist_grip", i),
            Rect { x: rect.x + rect.w - off - 2.0, y: rect.y + rect.h - off - 2.0, w: 2.0, h: 2.0 },
            theme::TEXT_FAINT,
            0.0,
        );
    }
}

fn draw_list(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    list_rect: Rect,
    labels: &[&'static str],
    current: usize,
) {
    let row_total = ROW_H + ROW_GAP;
    let n = labels.len();
    let content_h = n as f32 * row_total;
    let needs_scrollbar = content_h > list_rect.h;
    let row_w = if needs_scrollbar {
        (list_rect.w - SCROLLBAR_W).max(0.0)
    } else {
        list_rect.w
    };

    // リストは新しい順 (最新が上、 baseline が下)。 表示行 r (0 = 最上 = 最新) は
    // 履歴 index `idx = n - 1 - r` に対応。 現在 state の表示行:
    let current_row = n.saturating_sub(1).saturating_sub(current);

    // auto-scroll: 現在位置が follow_pos から変わったフレームだけ current 行が
    // 中央付近に来るよう offset を合わせる (手動 wheel scroll は妨げない)。
    if app.ui_ephemeral.undo_history_follow_pos != current {
        let max_off = (content_h - list_rect.h).max(0.0);
        let target =
            (current_row as f32 * row_total - list_rect.h * 0.5 + row_total * 0.5).clamp(0.0, max_off);
        ui.set_scroll_offset(SCROLL_ID, (0.0, target));
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.undo_history_follow_pos = current;
        }));
    }

    let clicked = Cell::new(None::<usize>);
    let pointer = ui.pointer();
    ui.scroll_area(SCROLL_ID, list_rect, (list_rect.w, content_h), |ui, offset| {
        if n == 0 {
            return;
        }
        let r_start = (offset.1 / row_total).floor().max(0.0) as usize;
        let r_end = (((offset.1 + list_rect.h) / row_total).ceil() as usize).min(n);
        for r in r_start..r_end {
            let idx = n - 1 - r; // 表示行 → 履歴 index (newest-first)。
            let label = labels[idx];
            let row_y = list_rect.y - offset.1 + r as f32 * row_total;
            let row_rect = Rect { x: list_rect.x, y: row_y, w: row_w, h: ROW_H };
            let inside = pointer.pos.is_some_and(|(px, py)| row_rect.contains(px, py));
            let is_current = idx == current;
            let bg = if is_current {
                Some(theme::ACCENT)
            } else if inside {
                Some(theme::CONTROL_HOVER)
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
            // 未来 (redo 待ち = idx > current、 newest-first では current より上) は薄く。
            let text_color = if is_current {
                theme::TEXT_ON_ACCENT
            } else if idx > current {
                theme::TEXT_FAINT
            } else {
                theme::TEXT
            };
            ui.label_at(
                ("undohist_row", idx),
                label,
                row_rect.x + 10.0,
                row_rect.y + (ROW_H - 11.0) * 0.5,
                11.0,
                text_color,
            );
            if inside && pointer.primary_just_released {
                clicked.set(Some(idx));
            }
        }
    });

    if let Some(index) = clicked.get() {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::JumpHistory(index));
        }));
    }
}
