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

/// menu item の click 時に実行される closure 型。
pub type MenuItemAction<'a, M> = Box<dyn FnOnce() -> Edit<M> + 'a>;

/// menu の 1 エントリ — 通常 item か sub-menu (再帰的に entries を持つ)。
/// `action` は `Option` で wrap、click 時に `Option::take()` で奪う (FnOnce のため)。
pub enum MenuEntry<'a, M: ?Sized + 'static> {
    Item {
        label: &'a str,
        action: Option<MenuItemAction<'a, M>>,
    },
    SubMenu {
        label: &'a str,
        entries: Vec<MenuEntry<'a, M>>,
    },
}

impl<'a, M: ?Sized + 'static> MenuEntry<'a, M> {
    fn label(&self) -> &'a str {
        match self {
            MenuEntry::Item { label, .. } | MenuEntry::SubMenu { label, .. } => label,
        }
    }
}

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

/// menu_bar の 1 つの top-level menu を構築するためのビルダー。`item` / `sub_menu` を順に並べる。
pub struct MenuBuilder<'a, M: ?Sized + 'static> {
    pub(crate) entries: Vec<MenuEntry<'a, M>>,
}

impl<'a, M: ?Sized + 'static> Default for MenuBuilder<'a, M> {
    fn default() -> Self {
        Self { entries: Vec::new() }
    }
}

impl<'a, M: ?Sized + 'static> MenuBuilder<'a, M> {
    /// 通常の item を追加。click で `on_click()` の Edit が発火 + 親 popup close。
    pub fn item<F: FnOnce() -> Edit<M> + 'a>(&mut self, label: &'a str, on_click: F) -> &mut Self {
        self.entries.push(MenuEntry::Item { label, action: Some(Box::new(on_click)) });
        self
    }

    /// sub-menu を追加。hover で sub-popup が開く (DAW 標準挙動)。再帰的に sub_menu を入れ子可。
    pub fn sub_menu<F>(&mut self, label: &'a str, f: F) -> &mut Self
    where
        F: FnOnce(&mut MenuBuilder<'a, M>),
    {
        let mut sub = MenuBuilder::default();
        f(&mut sub);
        self.entries.push(MenuEntry::SubMenu { label, entries: sub.entries });
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
    /// `f` は `MenuBuilder` を受け取り `item()` / `sub_menu()` を順に並べる。
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

        // popup_rect = entries 全体を含む rect。
        let n = builder.entries.len();
        let popup_rect = Rect {
            x: label_rect.x,
            y: label_rect.y + label_rect.h,
            w: MENU_W_DEFAULT,
            h: (n as f32) * MENU_ITEM_H,
        };
        let anchor = union_rect(label_rect, popup_rect);

        // クリックで popup を toggle (click は consume)
        if inside && pointer.primary_just_released {
            if already_open {
                self.ui.close_popup(menu_id);
            } else {
                self.ui.open_popup(menu_id, anchor, true);
            }
            self.ui.consume_pointer_click();
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

        // popup 描画 (popup_layer 経由、sub-menu cascade を draw_menu_entries 内で再帰処理)
        let mut clicked_action: Option<MenuItemAction<'a, M>> = None;
        let id_path = format!("menu_bar/{label}");
        let mut entries = builder.entries;
        self.ui.popup_layer(menu_id, |ui| {
            clicked_action = draw_menu_entries(ui, &mut entries, popup_rect, &id_path);
        });
        if let Some(action) = clicked_action {
            let edit = action();
            self.ui.push_edit(edit);
            self.ui.close_popup(menu_id);
        }
    }
}

/// `entries` を popup として縦並べに描画。
/// - `Item`: hover で highlight、click で `action` を `take()` して返す
/// - `SubMenu`: 「▶」マーカー + hover で sub-popup を開く (再帰的に `draw_menu_entries` 呼び出し)
///
/// 戻り値: clicked item の `action` (`Item::action` から `Option::take()` した closure)。
/// `None` ならまだ click なし。caller は `action()` を呼んで Edit を発行する。
pub(crate) fn draw_menu_entries<'a, M: ?Sized + 'static>(
    ui: &mut Ui<'a, M>,
    entries: &mut [MenuEntry<'a, M>],
    popup_rect: Rect,
    id_path: &str,
) -> Option<MenuItemAction<'a, M>> {
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

    let mut return_action: Option<MenuItemAction<'a, M>> = None;
    let arrow_color = Color::rgb(0.65, 0.68, 0.74);

    for (i, entry) in entries.iter_mut().enumerate() {
        let item_rect = Rect {
            x: popup_rect.x,
            y: popup_rect.y + i as f32 * MENU_ITEM_H,
            w: popup_rect.w,
            h: MENU_ITEM_H,
        };
        let hovered = pointer
            .pos
            .is_some_and(|(px, py)| item_rect.contains(px, py));
        let label = entry.label();
        let is_sub = matches!(entry, MenuEntry::SubMenu { .. });

        // hover highlight (sub-menu も同じ色で highlight、▶ は別描画)
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
            text: label.to_string(),
            left: item_rect.x + MENU_PAD_X,
            top: item_rect.y + (MENU_ITEM_H - MENU_FONT * 1.2) * 0.5,
            font_size: MENU_FONT,
            line_height: MENU_FONT * 1.2,
            color: Color::rgb(0.92, 0.92, 0.94),
            clip_rect: None,
        });
        if is_sub {
            // 右端に「▶」マーカー
            ui.push_text(GlyphArea {
                text: "▶".to_string(),
                left: item_rect.x + item_rect.w - MENU_PAD_X - MENU_FONT,
                top: item_rect.y + (MENU_ITEM_H - MENU_FONT * 1.2) * 0.5,
                font_size: MENU_FONT,
                line_height: MENU_FONT * 1.2,
                color: arrow_color,
                clip_rect: None,
            });
        }

        match entry {
            MenuEntry::Item { action, .. } => {
                if hovered && pointer.primary_just_released && let Some(a) = action.take() {
                    return_action = Some(a);
                }
            }
            MenuEntry::SubMenu { entries: sub_entries, .. } => {
                let sub_id = format!("{id_path}/{i}");
                let sub_popup_rect = Rect {
                    x: item_rect.x + item_rect.w,
                    y: item_rect.y,
                    w: MENU_W_DEFAULT,
                    h: (sub_entries.len() as f32) * MENU_ITEM_H,
                };
                let sub_anchor = union_rect(item_rect, sub_popup_rect);

                // hover で sub-popup を open (DAW 標準挙動)
                if hovered && !ui.is_popup_open(&sub_id) {
                    ui.open_popup(&sub_id, sub_anchor, true);
                }

                // sub-popup 描画 (popup_layer の中で再帰呼び出し、click を受け取る)
                let mut sub_action: Option<MenuItemAction<'a, M>> = None;
                let sub_id_for_anchor = sub_id.clone();
                ui.popup_layer(&sub_id, |ui_inner| {
                    if let Some(rect) = ui_inner.popup_anchor(&sub_id_for_anchor) {
                        // anchor は item_rect + sub_popup_rect の union。sub_popup_rect は
                        // anchor の右半分なので、anchor.x + item_rect.w 以降が sub_popup の領域。
                        let sub_rect = Rect {
                            x: rect.x + item_rect.w,
                            y: rect.y,
                            w: rect.w - item_rect.w,
                            h: rect.h,
                        };
                        sub_action = draw_menu_entries(ui_inner, sub_entries, sub_rect, &sub_id);
                    }
                });
                if sub_action.is_some() {
                    return_action = sub_action;
                }
            }
        }
    }

    return_action
}

/// 2 つの矩形を含む最小 rect (popup の anchor 計算用)。
fn union_rect(a: Rect, b: Rect) -> Rect {
    let left = a.x.min(b.x);
    let top = a.y.min(b.y);
    let right = (a.x + a.w).max(b.x + b.w);
    let bottom = (a.y + a.h).max(b.y + b.h);
    Rect { x: left, y: top, w: right - left, h: bottom - top }
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
        let menu_id = ("context_menu", rect.x.to_bits(), rect.y.to_bits());
        let n = items.len();

        // 右クリック検出 → popup を開く (anchor は items 全体の rect で固定 = popup の見える範囲)
        if pointer.secondary_just_pressed
            && let Some((px, py)) = pointer.pos
            && rect.contains(px, py)
        {
            let anchor = Rect {
                x: px,
                y: py,
                w: MENU_W_DEFAULT,
                h: (n as f32) * MENU_ITEM_H,
            };
            self.open_popup(menu_id, anchor, true);
        }

        // popup_rect は state.anchor から取得 (open 時の固定座標、毎フレーム同じ)
        let mut clicked_idx: Option<usize> = None;
        if let Some(popup_rect) = self.popup_anchor(menu_id) {
            self.popup_layer(menu_id, |ui| {
                clicked_idx = draw_items_popup(ui, items, popup_rect);
            });
        }
        if let Some(idx) = clicked_idx {
            let edit = on_select(idx);
            self.push_edit(edit);
            self.close_popup(menu_id);
        }
    }
}
