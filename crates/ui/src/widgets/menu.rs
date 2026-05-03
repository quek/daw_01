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

use crate::ui::Ui;

/// menu item の click 時に実行される closure 型 (M9 P1-5 で `&mut Ui<'_, M>` を受け取る形に変更)。
///
/// closure 内で任意の `Ui` method (`push_edit` / `request_undo` / `request_redo` / `set_focus` /
/// `take_clipboard_paste` 等) を呼べる。`Edit::mutate(...)` を発行するだけなら `|ui|
/// ui.push_edit(Edit::mutate(...))` のように書く。
///
/// 設計判断 (P1-5 で B 案 enum 列挙ではなく C 案 `&mut Ui` 採用):
/// - menu item の action は単一の Edit ではなく「任意の UI 操作」(undo/redo / popup close /
///   focus 移動 / 複数 Edit 発行) を取りたい。Ui を渡すのが最も自由度高い。
/// - fader/knob の `on_change: Fn(f32) -> Edit<M>` は「単一値の変更」のため Ui 不要。
///   menu item は本質的に異なる widget なので別 signature。
pub type MenuItemAction<'a, M> = Box<dyn for<'ui> FnOnce(&mut Ui<'ui, M>) + 'a>;

/// menu item の構築指定 (M9 P1-5)。`enabled` / `shortcut_hint` を指定したいときに使う。
/// 短縮形 `MenuBuilder::item(label, on_click)` は `enabled: true / shortcut_hint: None` 同等。
///
/// `shortcut_hint` は `Option<String>` (所有版)。`Ui::shortcut_for(name)` が `Option<String>`
/// を返すのでそのまま渡せる。`Option<&str>` 版にしないのは、closure 越しに hint を渡す際に
/// borrow checker 上 `&'a str` が builder lifetime に縛られて使いづらいため (実用例:
/// daw_prototype の Edit menu で `let hint = ui.shortcut_for("undo"); ui.menu_bar(..., move |m| { ... shortcut_hint: hint })`).
pub struct MenuItemSpec<'a, M: ?Sized + 'static> {
    pub label: &'a str,
    pub on_click: MenuItemAction<'a, M>,
    /// `false` なら item を灰色表示 + hover highlight 無効 + click ignore。
    pub enabled: bool,
    /// 右端に灰色で表示するキーバインドヒント (例: `Some("Ctrl+Z".into())`)。
    pub shortcut_hint: Option<String>,
}

/// menu の 1 エントリ — 通常 item か sub-menu (再帰的に entries を持つ)。
/// `action` は `Option` で wrap、click 時に `Option::take()` で奪う (FnOnce のため)。
pub enum MenuEntry<'a, M: ?Sized + 'static> {
    Item {
        label: &'a str,
        action: Option<MenuItemAction<'a, M>>,
        /// M9 P1-5: false なら disabled (灰色 + hover/click 無効)。
        enabled: bool,
        /// M9 P1-5: 右端の shortcut hint (例 `Some("Ctrl+Z".into())`)。
        shortcut_hint: Option<String>,
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
    /// 通常の item を追加 (短縮形)。click で `on_click(ui)` が呼ばれ、closure 内で任意の
    /// Ui 操作 (`push_edit` / `request_undo` / etc) を行う。click 後に親 popup close。
    /// `enabled: true / shortcut_hint: None` 相当。`item_with` で完全指定可。
    ///
    /// ```ignore
    /// menu.item("Add", |ui| ui.push_edit(Edit::mutate(|m: &mut M| m.add())));
    /// menu.item("Undo", |ui| ui.request_undo());
    /// ```
    pub fn item<F>(&mut self, label: &'a str, on_click: F) -> &mut Self
    where
        F: for<'ui> FnOnce(&mut Ui<'ui, M>) + 'a,
    {
        self.entries.push(MenuEntry::Item {
            label,
            action: Some(Box::new(on_click)),
            enabled: true,
            shortcut_hint: None,
        });
        self
    }

    /// M9 P1-5: `MenuItemSpec` で完全指定。`enabled: false` で disabled item、
    /// `shortcut_hint: Some("Ctrl+Z")` で右端に灰色 hint を描画。
    ///
    /// ```ignore
    /// menu.item_with(MenuItemSpec {
    ///     label: "Undo",
    ///     on_click: Box::new(|| Edit::mutate(|m: &mut MyModel| m.undo())),
    ///     enabled: ui.can_undo(),
    ///     shortcut_hint: ui.shortcut_for("undo").as_deref(),
    /// });
    /// ```
    pub fn item_with(&mut self, spec: MenuItemSpec<'a, M>) -> &mut Self {
        self.entries.push(MenuEntry::Item {
            label: spec.label,
            action: Some(spec.on_click),
            enabled: spec.enabled,
            shortcut_hint: spec.shortcut_hint,
        });
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
            // M9 P1-5 (C 案): action は &mut Ui を受け、closure 内で push_edit / request_undo
            // 等の任意操作を行う。library 側は popup close のみ自動。
            action(self.ui);
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
#[allow(clippy::too_many_lines)]
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
        // M9 P1-5: enabled / shortcut_hint を取り出す (SubMenu は常に enabled、hint なし)。
        let (item_enabled, item_hint): (bool, Option<&str>) = match entry {
            MenuEntry::Item { enabled, shortcut_hint, .. } => {
                (*enabled, shortcut_hint.as_deref())
            }
            MenuEntry::SubMenu { .. } => (true, None),
        };

        // hover highlight (enabled な item のみ; disabled / sub-menu は別)
        if hovered && item_enabled {
            ui.push_rect(RectCommand {
                rect: item_rect,
                fill: Color::rgb(0.32, 0.55, 0.85),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [2.0; 4],
                clip_rect: None,
            });
        }
        // text 色: enabled なら通常色、disabled なら灰色
        let text_color = if item_enabled {
            Color::rgb(0.92, 0.92, 0.94)
        } else {
            Color::rgb(0.50, 0.52, 0.56)
        };
        ui.push_text(GlyphArea {
            text: label.to_string(),
            left: item_rect.x + MENU_PAD_X,
            top: item_rect.y + (MENU_ITEM_H - MENU_FONT * 1.2) * 0.5,
            font_size: MENU_FONT,
            line_height: MENU_FONT * 1.2,
            color: text_color,
            clip_rect: None,
        });
        // shortcut hint を右端に灰色で
        if let Some(hint) = item_hint {
            let hint_w = (hint.chars().count() as f32) * 7.0;
            ui.push_text(GlyphArea {
                text: hint.to_string(),
                left: item_rect.x + item_rect.w - MENU_PAD_X - hint_w,
                top: item_rect.y + (MENU_ITEM_H - MENU_FONT * 1.2) * 0.5,
                font_size: MENU_FONT,
                line_height: MENU_FONT * 1.2,
                color: arrow_color,
                clip_rect: None,
            });
        }
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
            MenuEntry::Item { action, enabled, .. } => {
                // disabled item は click を ignore
                if *enabled
                    && hovered
                    && pointer.primary_just_released
                    && let Some(a) = action.take()
                {
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
    /// M9 P1-5 (C 案): `on_select` は `(idx, &mut Ui<'_, M>)` を受け、closure 内で任意の
    /// `Ui` 操作 (`push_edit` / `request_undo` / etc) を行う。
    ///
    /// ```ignore
    /// ui.context_menu_for(clip_rect, &["Cut", "Copy", "Delete"], |idx, ui| match idx {
    ///     0 => ui.push_edit(make_cut_edit()),
    ///     1 => ui.push_edit(make_copy_edit()),
    ///     2 => ui.push_edit(make_delete_edit()),
    ///     _ => {}
    /// });
    /// ```
    pub fn context_menu_for<F>(&mut self, rect: Rect, items: &[&str], on_select: F)
    where
        F: for<'ui> FnOnce(usize, &mut Ui<'ui, M>),
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
            on_select(idx, self);
            self.close_popup(menu_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Edit;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;
    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::Scene;

    /// item_with で disabled な item を作って menu_bar 経由で開き、click しても Edit が発行されない。
    #[test]
    fn item_with_disabled_does_not_fire_edit() {
        struct M {
            fired: bool,
        }
        let mut host: UiHost<M> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = M { fired: false };
        let screen = PhysicalSize { width: 800, height: 600 };

        // Frame 1: menu_bar の "Edit" を click して open
        let click_at_edit = FrameInput {
            pointer: PointerFrame {
                pos: Some((20.0, 16.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        host.frame(&mut model, &mut scene, screen, click_at_edit, |_, ui| {
            ui.menu_bar(Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 }, |bar| {
                bar.menu("Edit", |menu| {
                    menu.item_with(MenuItemSpec {
                        label: "Undo",
                        on_click: Box::new(|ui| ui.push_edit(Edit::mutate(|m: &mut M| m.fired = true))),
                        enabled: false,
                        shortcut_hint: Some("Ctrl+Z".into()),
                    });
                });
            });
        });
        // Frame 2: popup が開いた状態で Undo item をクリック
        let click_at_undo = FrameInput {
            pointer: PointerFrame {
                pos: Some((20.0, 32.0 + MENU_ITEM_H * 0.5)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        host.frame(&mut model, &mut scene, screen, click_at_undo, |_, ui| {
            ui.menu_bar(Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 }, |bar| {
                bar.menu("Edit", |menu| {
                    menu.item_with(MenuItemSpec {
                        label: "Undo",
                        on_click: Box::new(|ui| ui.push_edit(Edit::mutate(|m: &mut M| m.fired = true))),
                        enabled: false,
                        shortcut_hint: Some("Ctrl+Z".into()),
                    });
                });
            });
        });
        assert!(!model.fired, "disabled item の click は Edit 発行しない");
    }

    /// item_with で shortcut_hint を指定すると popup の glyph_areas に hint text が含まれる。
    #[test]
    fn item_with_shortcut_hint_text_drawn() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        // 1 frame 目で popup を open
        let click_at_edit = FrameInput {
            pointer: PointerFrame {
                pos: Some((20.0, 16.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        host.frame_to_edits(&(), &mut scene, screen, click_at_edit, |(), ui| {
            ui.menu_bar(Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 }, |bar| {
                bar.menu("Edit", |menu| {
                    menu.item_with(MenuItemSpec {
                        label: "Undo",
                        on_click: Box::new(|ui| ui.push_edit(Edit::mutate(|()| {}))),
                        enabled: true,
                        shortcut_hint: Some("Ctrl+Z".into()),
                    });
                });
            });
        });
        scene.clear();
        // 2 frame 目で popup_layer が描画される
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.menu_bar(Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 }, |bar| {
                bar.menu("Edit", |menu| {
                    menu.item_with(MenuItemSpec {
                        label: "Undo",
                        on_click: Box::new(|ui| ui.push_edit(Edit::mutate(|()| {}))),
                        enabled: true,
                        shortcut_hint: Some("Ctrl+Z".into()),
                    });
                });
            });
        });
        // popup_glyph_areas に "Undo" と "Ctrl+Z" の両方が含まれる
        let texts: Vec<&str> =
            scene.popup_glyph_areas.iter().map(|g| g.text.as_str()).collect();
        assert!(texts.contains(&"Undo"), "label が popup に描画される: {texts:?}");
        assert!(texts.contains(&"Ctrl+Z"), "shortcut_hint が popup に描画される: {texts:?}");
    }

    /// 短縮形 item() は enabled: true, shortcut_hint: None と等価。
    #[test]
    fn item_short_form_is_enabled_no_hint() {
        struct M {
            fired: bool,
        }
        let mut host: UiHost<M> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = M { fired: false };
        let screen = PhysicalSize { width: 800, height: 600 };

        // 1 frame 目: open
        let open = FrameInput {
            pointer: PointerFrame {
                pos: Some((20.0, 16.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        host.frame(&mut model, &mut scene, screen, open, |_, ui| {
            ui.menu_bar(Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 }, |bar| {
                bar.menu("Edit", |menu| {
                    menu.item("Undo", |ui| ui.push_edit(Edit::mutate(|m: &mut M| m.fired = true)));
                });
            });
        });
        // 2 frame 目: click on item
        let click_item = FrameInput {
            pointer: PointerFrame {
                pos: Some((20.0, 32.0 + MENU_ITEM_H * 0.5)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        host.frame(&mut model, &mut scene, screen, click_item, |_, ui| {
            ui.menu_bar(Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 }, |bar| {
                bar.menu("Edit", |menu| {
                    menu.item("Undo", |ui| ui.push_edit(Edit::mutate(|m: &mut M| m.fired = true)));
                });
            });
        });
        assert!(model.fired, "短縮形 item は enabled デフォルト → click で fire");
    }
}
