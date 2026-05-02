//! `menu_bar` / `context_menu_for` / `dropdown` の共通基盤 + menu_bar widget (M7 Phase 23-25)。
//!
//! menu_bar は Window 上部に「File / Edit / View …」のラベル列を出し、
//! クリックで popup_layer 経由のサブメニューを開く。
//!
//! 本 module の役割:
//! - `MenuItem` 構造体: ラベル + on_click closure を一箇所にまとめる
//! - 共通描画関数 `draw_items_popup`: 縦リスト popup の描画 + click 検出を提供
//!   (menu_bar / context_menu_for / dropdown が同じ見た目を共有)
//! - `Ui::menu_bar` 公開 API
//! - `Ui::context_menu_for` 公開 API (right-click を library が吸収)
//!
//! `Ui::dropdown` は別 file (`widgets/dropdown.rs`)。

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::ui::Ui;

/// menu / context_menu / dropdown で共通の 1 アイテム。
pub type MenuItemSpec<'a, M> = (&'a str, MenuItemAction<'a, M>);

/// menu item の click 時に実行される closure 型。
pub type MenuItemAction<'a, M> = Box<dyn FnOnce() -> Edit<M> + 'a>;

const MENU_ITEM_H: f32 = 24.0;
const MENU_PAD_X: f32 = 12.0;
const MENU_FONT: f32 = 14.0;
const MENU_W_DEFAULT: f32 = 180.0;

/// item リストを縦並べる popup (menu / context_menu / dropdown 共通)。
/// `popup_id` は呼び出し側が責任を持って一意にする (例: `(b"menu_file", ...)`)。
/// `anchor` は popup を開く起点。`items_rect` は popup 自身の描画領域。
/// 戻り値: 選択された item の index (Some なら popup を close)。
pub(crate) fn draw_items_popup<'a, M: ?Sized + 'static>(
    ui: &mut Ui<'a, M>,
    items: &[&str],
    popup_rect: Rect,
) -> Option<usize> {
    let pointer = ui.pointer();

    // 背景パネル
    ui.push_rect(RectCommand {
        rect: popup_rect,
        fill: Color::rgb(0.10, 0.11, 0.13),
        border: Color::rgb(0.30, 0.33, 0.39),
        border_width: 1.0,
        radius: [4.0; 4],
        clip_rect: None,
    });

    let mut clicked: Option<usize> = None;
    for (i, item) in items.iter().enumerate() {
        let item_rect = Rect {
            x: popup_rect.x,
            y: popup_rect.y + i as f32 * MENU_ITEM_H,
            w: popup_rect.w,
            h: MENU_ITEM_H,
        };
        let hovered = pointer
            .pos
            .is_some_and(|(px, py)| item_rect.contains(px, py));
        if hovered {
            ui.push_rect(RectCommand {
                rect: item_rect,
                fill: Color::rgb(0.32, 0.55, 0.85),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [2.0; 4],
                clip_rect: None,
            });
        }
        ui.push_text(GlyphArea {
            text: (*item).to_string(),
            left: item_rect.x + MENU_PAD_X,
            top: item_rect.y + (MENU_ITEM_H - MENU_FONT * 1.2) * 0.5,
            font_size: MENU_FONT,
            line_height: MENU_FONT * 1.2,
            color: Color::rgb(0.92, 0.92, 0.94),
            clip_rect: None,
        });
        if hovered && pointer.primary_just_released {
            clicked = Some(i);
        }
    }
    clicked
}

/// menu_bar の 1 つの top-level menu を構築するためのビルダー。
pub struct MenuBuilder<'a, M: ?Sized + 'static> {
    items: Vec<MenuItemSpec<'a, M>>,
}

impl<'a, M: ?Sized + 'static> Default for MenuBuilder<'a, M> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

impl<'a, M: ?Sized + 'static> MenuBuilder<'a, M> {
    pub fn item<F: FnOnce() -> Edit<M> + 'a>(&mut self, label: &'a str, on_click: F) -> &mut Self {
        self.items.push((label, Box::new(on_click)));
        self
    }
}

/// menu_bar 全体を構築するためのビルダー。`Ui::menu_bar` で受け取る。
pub struct MenuBarBuilder<'b, 'a, M: ?Sized + 'static> {
    ui: &'b mut Ui<'a, M>,
    bar_rect: Rect,
    next_x: f32,
}

impl<'b, 'a, M: ?Sized + 'static> MenuBarBuilder<'b, 'a, M> {
    /// "File" / "Edit" 等の top-level menu を 1 つ追加する。
    /// `f` は `MenuBuilder` を受け取り `item()` を並べる。
    pub fn menu<F>(&mut self, label: &'static str, f: F)
    where
        F: FnOnce(&mut MenuBuilder<'a, M>),
    {
        let mut builder = MenuBuilder::default();
        f(&mut builder);
        let label_w = (label.chars().count() as f32) * 8.0 + MENU_PAD_X * 2.0;
        let label_rect = Rect {
            x: self.bar_rect.x + self.next_x,
            y: self.bar_rect.y,
            w: label_w,
            h: self.bar_rect.h,
        };
        self.next_x += label_w;

        let pointer = self.ui.pointer();
        let inside = pointer
            .pos
            .is_some_and(|(px, py)| label_rect.contains(px, py));
        let menu_id = ("menu_bar_top", label);
        let already_open = self.ui.is_popup_open(menu_id);

        // クリックで popup を toggle
        if inside && pointer.primary_just_released {
            if already_open {
                self.ui.close_popup(menu_id);
            } else {
                self.ui.open_popup(menu_id, label_rect, true);
            }
        }

        // top-level label の描画 (hover / open でハイライト)
        let highlight = inside || self.ui.is_popup_open(menu_id);
        if highlight {
            self.ui.push_rect(RectCommand {
                rect: label_rect,
                fill: Color::rgb(0.20, 0.23, 0.28),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [2.0; 4],
                clip_rect: None,
            });
        }
        self.ui.push_text(GlyphArea {
            text: label.to_string(),
            left: label_rect.x + MENU_PAD_X,
            top: label_rect.y + (label_rect.h - MENU_FONT * 1.2) * 0.5,
            font_size: MENU_FONT,
            line_height: MENU_FONT * 1.2,
            color: Color::rgb(0.92, 0.94, 0.97),
            clip_rect: None,
        });

        // popup 描画 (popup_layer 経由)
        let item_labels: Vec<&str> = builder.items.iter().map(|(s, _)| *s).collect();
        let n = item_labels.len();
        let popup_rect = Rect {
            x: label_rect.x,
            y: label_rect.y + label_rect.h,
            w: MENU_W_DEFAULT,
            h: (n as f32) * MENU_ITEM_H,
        };
        let mut clicked_idx: Option<usize> = None;
        self.ui.popup_layer(menu_id, |ui| {
            clicked_idx = draw_items_popup(ui, &item_labels, popup_rect);
        });
        if let Some(idx) = clicked_idx {
            // closure を消費して Edit を発行
            let mut items = builder.items;
            if idx < items.len() {
                let (_, on_click) = items.swap_remove(idx);
                let edit = on_click();
                self.ui.push_edit(edit);
            }
            self.ui.close_popup(menu_id);
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// menu_bar widget。`rect` 内に top-level menu のラベル列を描画。
    /// クリックでサブメニューを popup_layer 経由で開く。
    pub fn menu_bar<F>(&mut self, rect: Rect, f: F)
    where
        F: FnOnce(&mut MenuBarBuilder<'_, 'a, M>),
    {
        // 背景バー
        self.push_rect(RectCommand {
            rect,
            fill: Color::rgb(0.13, 0.14, 0.17),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });
        let mut builder = MenuBarBuilder { ui: self, bar_rect: rect, next_x: 0.0 };
        f(&mut builder);
    }

    /// 右クリックで popup を出す context_menu。library が右クリック検出を担当。
    /// `rect` 内で右クリックが発生したら、その位置で `items` を popup として表示。
    /// 各 item を選んだら対応する `Edit<M>` を発行。
    ///
    /// 利用者は右クリック判定 / popup ライフサイクル管理が一切不要 (M7 設計判断: library 吸収)。
    pub fn context_menu_for<F>(&mut self, rect: Rect, items: &[&str], on_select: F)
    where
        F: FnOnce(usize) -> Edit<M>,
    {
        let pointer = self.pointer;
        // 右クリック検出
        let menu_id = ("context_menu", rect.x.to_bits(), rect.y.to_bits());
        if pointer.secondary_just_pressed
            && let Some((px, py)) = pointer.pos
            && rect.contains(px, py)
        {
            // popup を開く (anchor は click 位置の小さい矩形)
            let anchor = Rect { x: px, y: py, w: 1.0, h: 1.0 };
            self.open_popup(menu_id, anchor, true);
        }

        // popup を描画
        let n = items.len();
        let popup_rect = if let Some((px, py)) = pointer.pos {
            // 開いた瞬間の anchor は記録されないので毎フレーム pointer 位置 …
            // ではなく、anchor を popup_state から復元する。
            // 簡略化: popup の anchor を anchor の左上から開始する固定矩形として描画。
            // anchor 位置は popup_state.anchor から取り出すため popup_layer 後の描画では使えない。
            // ここでは popup_state にアクセスできないので、画面内に収まるよう pointer 位置を
            // 直近の値で近似 (M7 では妥協、ext は M8 で改善)。
            Rect {
                x: px,
                y: py,
                w: MENU_W_DEFAULT,
                h: (n as f32) * MENU_ITEM_H,
            }
        } else {
            Rect { x: 0.0, y: 0.0, w: MENU_W_DEFAULT, h: (n as f32) * MENU_ITEM_H }
        };

        let mut clicked_idx: Option<usize> = None;
        self.popup_layer(menu_id, |ui| {
            clicked_idx = draw_items_popup(ui, items, popup_rect);
        });
        if let Some(idx) = clicked_idx {
            let edit = on_select(idx);
            self.push_edit(edit);
            self.close_popup(menu_id);
        }
    }
}
