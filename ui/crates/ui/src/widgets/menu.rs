// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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

use crate::input::PointerFrame;
use crate::ui::Ui;

/// menu item の click 時に実行される closure 型 (M9 P1-5 で `&mut Ui<'_, M>` を受け取る形に変更)。
///
/// closure 内で任意の `Ui` method (`push_edit` / `set_focus` / `take_clipboard_paste` 等) を
/// 呼べる。`Edit::mutate(...)` を発行するだけなら `|ui| ui.push_edit(Edit::mutate(...))` のように
/// 書く (undo/redo はアプリ層の責務で、S4a 以降 lib には undo API が無い)。
///
/// 設計判断 (P1-5 で B 案 enum 列挙ではなく C 案 `&mut Ui` 採用):
/// - menu item の action は単一の Edit ではなく「任意の UI 操作」(popup close /
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
pub(crate) const MENU_FONT: f32 = 14.0;
/// popup 幅の**下限** (項目が短くても最低これだけ広げる)。
const MENU_W_DEFAULT: f32 = 180.0;
/// popup 幅の上限。 これを超える項目は ellipsis で省略する (画面を横断する
/// 巨大 popup を防ぐ安全弁)。
const MENU_W_MAX: f32 = 480.0;

/// `&[&str]` 版の popup 推奨幅 (dropdown 用)。 全項目が省略なしで収まる幅を返す。
/// 下限は `min_w` (dropdown 本体幅 = combobox の慣用)、 上限は [`MENU_W_MAX`]。
pub(crate) fn items_popup_width<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    items: &[&str],
    min_w: f32,
) -> f32 {
    let widest = items
        .iter()
        .map(|s| ui.measure_text(s, MENU_FONT))
        .fold(0.0_f32, f32::max);
    (widest + MENU_PAD_X * 2.0).min(MENU_W_MAX).max(min_w)
}

/// `MenuEntry` 版の popup 推奨幅。 各項目の「ラベル実幅 + 右側予約 (shortcut hint /
/// ▶ マーカー)」の最大値に左右 padding を足す。
///
/// 固定 180px だと日本語ラベル (全角 1 文字 = font_size) がすぐ枠外へ出る
/// (例:「クリップ色をトラックに揃える」= 全角 14 文字 ≒ 207px)。 DAW の
/// コンテキストメニューは内容に合わせて伸びるのが標準なので、実 advance で
/// 測って伸ばす。 上限を超えた分だけ [`draw_menu_entries`] が ellipsis する。
pub(crate) fn entries_popup_width<'a, M: ?Sized + 'static>(
    ui: &mut Ui<'a, M>,
    entries: &[MenuEntry<'a, M>],
) -> f32 {
    let mut widest = 0.0_f32;
    for e in entries {
        let label_w = ui.measure_text(e.label(), MENU_FONT);
        let right = match e {
            MenuEntry::Item { shortcut_hint: Some(h), .. } => {
                ui.measure_text(h, MENU_FONT) + MENU_PAD_X
            }
            MenuEntry::SubMenu { .. } => MENU_FONT + MENU_PAD_X,
            MenuEntry::Item { .. } => 0.0,
        };
        widest = widest.max(label_w + right);
    }
    (widest + MENU_PAD_X * 2.0).clamp(MENU_W_DEFAULT, MENU_W_MAX)
}

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
    let p = ui.palette();

    // 背景パネル
    ui.push_rect(RectCommand {
        rect: popup_rect,
        fill: p.panel,
        border: p.border,
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
                fill: p.accent,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [2.0; 4],
                clip_rect: None,
            });
        }
        // 項目が popup 幅に収まらないときは末尾 ellipsis + clip (daw_01 #079 の
        // 「widget は自分の rect 境界に責任を持つ」)。 dropdown の popup 幅は
        // 呼び出し側 dropdown の rect.w なので、 狭い dropdown ほど溢れやすい。
        let item_max_w = (item_rect.w - MENU_PAD_X * 2.0).max(1.0);
        let (display, _) = ui.fit_text_ellipsized(item, MENU_FONT, item_max_w);
        ui.push_text(GlyphArea {
            text: display.as_ref().into(),
            left: item_rect.x + MENU_PAD_X,
            top: item_rect.y + (MENU_ITEM_H - MENU_FONT * 1.2) * 0.5,
            font_size: MENU_FONT,
            line_height: MENU_FONT * 1.2,
            color: p.text,
            clip_rect: Some(Rect {
                x: item_rect.x + MENU_PAD_X,
                y: item_rect.y,
                w: item_max_w,
                h: item_rect.h,
            }),
            ..GlyphArea::default()
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
    /// Ui 操作 (`push_edit` / `set_focus` / etc) を行う。click 後に親 popup close。
    /// `enabled: true / shortcut_hint: None` 相当。`item_with` で完全指定可。
    ///
    /// ```ignore
    /// menu.item("Add", |ui| ui.push_edit(Edit::mutate(|m: &mut M| m.add())));
    /// menu.item("Undo", |ui| ui.push_edit(Edit::mutate(|m: &mut M| m.undo())));
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
    ///     on_click: Box::new(|ui| ui.push_edit(Edit::mutate(|m: &mut MyModel| m.undo()))),
    ///     enabled: app_can_undo,
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
///
/// M14 Phase 98 (daw_01 #070): `menu()` 呼び出し時は entries を **収集するだけ** で、
/// layout / 入力処理 (open / close / toggle / hover 切替) / 描画は全 top-level menu が
/// 出揃った後に `Ui::menu_bar` がまとめて行う (two-phase)。これにより「open 中 menu の
/// popup anchor が隣ラベルの click を消費して切替不能」だった bug を構造的に解消する
/// (切替判断を popup_layer 呼び出しより前の 1 箇所に集約)。
pub struct MenuBarBuilder<'a, M: ?Sized + 'static> {
    menus: Vec<(&'static str, Vec<MenuEntry<'a, M>>)>,
}

impl<'a, M: ?Sized + 'static> Default for MenuBarBuilder<'a, M> {
    fn default() -> Self {
        Self { menus: Vec::new() }
    }
}

impl<'a, M: ?Sized + 'static> MenuBarBuilder<'a, M> {
    /// "File" / "Edit" 等の top-level menu を 1 つ追加する。
    /// `f` は `MenuBuilder` を受け取り `item()` / `sub_menu()` を順に並べる。
    ///
    /// M14 Phase 98 (daw_01 #070): ここでは entries を収集するだけ。実際の
    /// open / close / toggle / hover 切替 / 描画は、全 menu が出揃ってから
    /// `Ui::menu_bar` が 1 箇所で行う。
    pub fn menu<F>(&mut self, label: &'static str, f: F)
    where
        F: FnOnce(&mut MenuBuilder<'a, M>),
    {
        let mut builder = MenuBuilder::default();
        f(&mut builder);
        self.menus.push((label, builder.entries));
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
    let p = ui.palette();

    // 背景パネル
    ui.push_rect(RectCommand {
        rect: popup_rect,
        fill: p.panel,
        border: p.border,
        border_width: 1.0,
        radius: [4.0; 4],
        clip_rect: None,
    });

    let mut return_action: Option<MenuItemAction<'a, M>> = None;
    let arrow_color = p.text_dim;
    let entries_len = entries.len();

    // 兄弟 sub-popup 排他 (daw_01 #037 fix): hover している item を loop 前に確定し、
    // 他 sibling の sub-popup を全 close する。 loop 内で close すると i=0 の
    // popup_layer 描画が i=1 の close より先に走り cascade が重なるため、 loop 前に
    // 一括処理する。
    let hovered_index: Option<usize> = (0..entries_len).find(|&i| {
        let item_rect = Rect {
            x: popup_rect.x,
            y: popup_rect.y + i as f32 * MENU_ITEM_H,
            w: popup_rect.w,
            h: MENU_ITEM_H,
        };
        pointer.pos.is_some_and(|(px, py)| item_rect.contains(px, py))
    });
    if let Some(idx) = hovered_index {
        for j in 0..entries_len {
            if j != idx {
                ui.close_popup(format!("{id_path}/{j}"));
            }
        }
    }

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
                fill: p.accent,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [2.0; 4],
                clip_rect: None,
            });
        }
        // text 色: enabled なら通常色、disabled なら灰色
        let text_color = if item_enabled { p.text } else { p.text_faint };
        // 右端の shortcut hint / ▶ マーカーが占める幅を先に確定し、 label はその
        // 手前までに収める。 実 advance で測るので CJK ラベル (日本語メニュー) でも
        // 正しい (旧実装は clip も ellipsis も無く、 長いラベルが hint / ▶ の上に
        // 重なった上で popup 外へはみ出していた)。
        let hint_w = item_hint.map_or(0.0, |h| ui.measure_text(h, MENU_FONT));
        let right_reserved = if item_hint.is_some() {
            hint_w + MENU_PAD_X
        } else if is_sub {
            MENU_FONT + MENU_PAD_X
        } else {
            0.0
        };
        let label_max_w = (item_rect.w - MENU_PAD_X * 2.0 - right_reserved).max(1.0);
        let (label_display, _) = ui.fit_text_ellipsized(label, MENU_FONT, label_max_w);
        ui.push_text(GlyphArea {
            text: label_display.as_ref().into(),
            left: item_rect.x + MENU_PAD_X,
            top: item_rect.y + (MENU_ITEM_H - MENU_FONT * 1.2) * 0.5,
            font_size: MENU_FONT,
            line_height: MENU_FONT * 1.2,
            color: text_color,
            clip_rect: Some(Rect {
                x: item_rect.x + MENU_PAD_X,
                y: item_rect.y,
                w: label_max_w,
                h: item_rect.h,
            }),
            ..GlyphArea::default()
        });
        // shortcut hint を右端に灰色で
        if let Some(hint) = item_hint {
            ui.push_text(GlyphArea {
                text: hint.into(),
                left: item_rect.x + item_rect.w - MENU_PAD_X - hint_w,
                top: item_rect.y + (MENU_ITEM_H - MENU_FONT * 1.2) * 0.5,
                font_size: MENU_FONT,
                line_height: MENU_FONT * 1.2,
                color: arrow_color,
                clip_rect: None,
                ..GlyphArea::default()
            });
        }
        if is_sub {
            // 右端に「▶」マーカー
            ui.push_text(GlyphArea {
                text: "▶".into(),
                left: item_rect.x + item_rect.w - MENU_PAD_X - MENU_FONT,
                top: item_rect.y + (MENU_ITEM_H - MENU_FONT * 1.2) * 0.5,
                font_size: MENU_FONT,
                line_height: MENU_FONT * 1.2,
                color: arrow_color,
                clip_rect: None,
                ..GlyphArea::default()
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
                    // 中身に合わせて伸ばす (Open Recent の長いファイル名が枠外に
                    // 出ていた)。 上限を超えたら item 側の ellipsis が効く。
                    w: entries_popup_width(ui, sub_entries),
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

/// M14 Phase 120 (daw_01 #095): top-level menu popup が閉じる / 閉じた frame に、 その配下で hover
/// open された cascade sub-popup を **再帰的に** 閉じる (orphan 防止)。 sub-popup id は
/// `draw_menu_entries` の open 規約 `{id_path}/{i}` (SubMenu entry の index i) / ネストは
/// `{id_path}/{i}/{j}…` と一致させる。 `close_popup` は閉じている id には no-op なので、 実際に開いて
/// いる分だけ閉じる。 **top-level を閉じる前に呼ぶ** ことで focus 復元 (`prev_focus`) が最終的に
/// top-level の原本 focus に戻る (孫 → 子の深い順に閉じ、 最後に caller が top-level を閉じる)。
///
/// 必要な理由: cascade は親 popup の `popup_layer` closure 内 (`draw_menu_entries`) で hover open
/// され、 sibling 排他 close / outside-click dismiss も `draw_menu_entries` が走る frame だけ動く。
/// top-level が閉じると `draw_menu_entries` が呼ばれなくなり、 開いていた cascade は **自力で
/// dismiss できず** modal popup として `open_popups` に居残る → 見えないのに anchor 内 pointer /
/// keyboard を遮断し続ける (daw_01 実機: cascade item で project を開いた後、 アレンジ上部 ~1/3 の
/// track double-click rename が `pointer_blocked_by_modal_popup` で不発)。
fn close_orphaned_cascades<'a, M: ?Sized + 'static>(
    ui: &mut Ui<'a, M>,
    entries: &[MenuEntry<'a, M>],
    id_path: &str,
) {
    for (i, entry) in entries.iter().enumerate() {
        if let MenuEntry::SubMenu { entries: sub, .. } = entry {
            let sub_id = format!("{id_path}/{i}");
            close_orphaned_cascades(ui, sub, &sub_id);
            ui.close_popup(&sub_id);
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// menu_bar の top-level 入力処理 (open / close / toggle / hover 切替) を 1 箇所で行う。
    /// `popup_layer` 描画より **前** に呼び、現在 open の menu (`open_idx`) と hover 中ラベル
    /// (`hovered_idx`) から状態遷移を確定する。これにより隣 menu の popup anchor が click を
    /// 消費する余地を構造的に排除する (daw_01 #070)。`anchors[i]` は union(label, popup)。
    fn switch_menu_bar_top_level(
        &mut self,
        menus: &[(&'static str, Vec<MenuEntry<'a, M>>)],
        anchors: &[Rect],
        open_idx: Option<usize>,
        hovered_idx: Option<usize>,
        pointer: PointerFrame,
    ) {
        let Some(i) = hovered_idx else {
            return;
        };
        let this_open = open_idx == Some(i);
        let id_i = ("menu_bar_top", menus[i].0);
        if pointer.primary_just_released {
            // click: open 中の同ラベル → toggle close、別ラベル → 旧を閉じて切替/open。
            if this_open {
                self.close_popup(id_i);
            } else {
                if let Some(j) = open_idx {
                    self.close_popup(("menu_bar_top", menus[j].0));
                }
                self.open_popup(id_i, anchors[i], true);
            }
            self.consume_pointer_click();
        } else {
            // hover 切替: いずれかが open 中に別ラベルへ pointer が乗ったら切替 (DAW 標準)。
            // 「閉じている状態では hover で開かない」ため open_idx.is_some() を条件にする。
            // ボタン押下中 (drag) は切替しない (release 時に上の click 経路で切替)。
            if open_idx.is_some() && !this_open && !pointer.primary_pressed {
                if let Some(j) = open_idx {
                    self.close_popup(("menu_bar_top", menus[j].0));
                }
                self.open_popup(id_i, anchors[i], true);
            }
            // ラベル上の press は、open 中 popup の outside_click 誤判定を防ぐため消費する
            // (popup anchor 幅に依存せず確実にブロック)。
            if pointer.primary_just_pressed {
                self.consume_pointer_click();
            }
        }
    }

    /// menu_bar widget。`rect` 内に top-level menu のラベル列を描画。
    /// クリックでサブメニューを popup_layer 経由で開く。
    ///
    /// M14 Phase 98 (daw_01 #070): 全 top-level menu を収集してから layout → 入力 → 描画を
    /// 行う two-phase 構成。切替判断を popup_layer より前に一元化し「open 中 menu の popup
    /// anchor が隣ラベルの click を消費して切替不能」だった bug を解消する。
    pub fn menu_bar<F>(&mut self, rect: Rect, f: F)
    where
        F: FnOnce(&mut MenuBarBuilder<'a, M>),
    {
        let p = self.palette();

        // 背景バー
        self.push_rect(RectCommand {
            rect,
            fill: p.header,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });

        // --- Phase 1: 全 top-level menu を収集 (描画も入力処理もまだしない) ---
        let mut builder = MenuBarBuilder::default();
        f(&mut builder);
        let menus = builder.menus;
        if menus.is_empty() {
            return;
        }

        // --- Phase 2: layout — 各ラベル rect / popup rect / anchor を全 menu 分先に確定 ---
        // anchor は union(label, popup)。toggle-close で press/release が別フレームに割れたとき、
        // popup_layer の outside_click (press 判定) がラベルを「外」と誤判定して即閉じる回帰を
        // 防ぐためラベル帯を含める。
        let mut label_rects: Vec<Rect> = Vec::with_capacity(menus.len());
        let mut next_x = 0.0;
        for (label, _entries) in &menus {
            // ラベル帯の幅は実 advance で測る。 固定 8px/文字 の概算は CJK
            // (1 文字 ≒ font_size = 14px) を半分に見積もるので、 日本語メニュー名では
            // 文字が隣のラベル帯へはみ出し、 hover / click 判定もラベル位置とずれる。
            let label_w = self.measure_text(label, MENU_FONT) + MENU_PAD_X * 2.0;
            label_rects.push(Rect { x: rect.x + next_x, y: rect.y, w: label_w, h: rect.h });
            next_x += label_w;
        }

        // --- Phase 3: 入力処理 (popup_layer より前に open/close を確定) ---
        let open_idx =
            (0..menus.len()).find(|&i| self.is_popup_open(("menu_bar_top", menus[i].0)));
        let pointer = self.pointer();
        let hovered_idx = pointer
            .pos
            .and_then(|(px, py)| label_rects.iter().position(|r| r.contains(px, py)));

        // popup 幅の実測 (`entries_popup_width`) は全 entry を shape するので、 popup が
        // 要る menu (開いている / hover していて次に開きうる) だけ計算する。 全 menu 分を
        // 毎フレーム測ると menu bar だけで数十回の shape が走って UI ループが重くなる。
        // 閉じていて hover もしていない menu の anchor は open されるまで使われない
        // (open は必ず hover か click 経由 = hovered_idx が Some の frame)。
        let mut popup_rects: Vec<Rect> = Vec::with_capacity(menus.len());
        let mut anchors: Vec<Rect> = Vec::with_capacity(menus.len());
        for (i, (_label, entries)) in menus.iter().enumerate() {
            let label_rect = label_rects[i];
            let popup_h = (entries.len() as f32) * MENU_ITEM_H;
            let popup_w = if open_idx == Some(i) || hovered_idx == Some(i) {
                entries_popup_width(self, entries)
            } else {
                MENU_W_DEFAULT
            };
            let popup_rect = crate::popup::popup_rect_below_or_above(
                label_rect,
                popup_w,
                popup_h,
                self.screen(),
            );
            anchors.push(union_rect(label_rect, popup_rect));
            popup_rects.push(popup_rect);
        }
        self.switch_menu_bar_top_level(&menus, &anchors, open_idx, hovered_idx, pointer);

        // --- Phase 4: 描画 — ラベル列 + (open している menu のみ) popup ---
        for (i, (label, entries)) in menus.into_iter().enumerate() {
            let label_rect = label_rects[i];
            let popup_rect = popup_rects[i];
            let id = ("menu_bar_top", label);
            let is_open = self.is_popup_open(id);

            // top-level label (hover / open でハイライト)
            if hovered_idx == Some(i) || is_open {
                self.push_rect(RectCommand {
                    rect: label_rect,
                    fill: p.control_hover,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [2.0; 4],
                    clip_rect: None,
                });
            }
            self.push_text(GlyphArea {
                text: label.into(),
                left: label_rect.x + MENU_PAD_X,
                top: label_rect.y + (label_rect.h - MENU_FONT * 1.2) * 0.5,
                font_size: MENU_FONT,
                line_height: MENU_FONT * 1.2,
                color: p.text,
                clip_rect: None,
                ..GlyphArea::default()
            });

            if !is_open {
                // M14 Phase 120 (daw_01 #095): top-level が (action click / outside-click / toggle /
                // 隣 menu 切替 の) いずれかの経路で閉じている frame は、 hover open された cascade
                // sub-popup を道連れに閉じる。 さもなくば cascade が orphan して anchor 内入力を遮断し
                // 続ける (Esc / 外 click でも dismiss 経路が走らず居残るのを ≤1 frame で回収する safety
                // net)。 popup が皆無の idle frame は cascade も存在し得ないので `has_open_popups()` で
                // 早期 skip し、 closed menu 毎フレームの id_path `format!` alloc を avoid する (orphan は
                // 必ず open_popups 非空なので拾い損ねない)。
                if self.has_open_popups() {
                    close_orphaned_cascades(self, &entries, &format!("menu_bar/{label}"));
                }
                continue;
            }

            // open 中 menu の anchor を毎フレーム最新 layout に同期 (resize で popup が flip しても
            // stale にならない)。
            self.update_popup_anchor(id, anchors[i]);

            // popup 描画 (popup_layer 経由、sub-menu cascade を draw_menu_entries 内で再帰処理)。
            // id_path は cascade sub-popup id prefix の規約 `{id_path}/{i}` を draw_menu_entries と共有。
            let id_path = format!("menu_bar/{label}");
            let mut entries = entries;
            let mut clicked_action: Option<MenuItemAction<'a, M>> = None;
            self.popup_layer(id, |ui| {
                clicked_action = draw_menu_entries(ui, &mut entries, popup_rect, &id_path);
            });
            if let Some(action) = clicked_action {
                // M9 P1-5 (C 案): action は &mut Ui を受け、closure 内で push_edit / set_focus
                // 等の任意操作を行う。library 側は popup close のみ自動。
                action(self);
                // M14 Phase 120 (daw_01 #095): cascade item の click は `close_popup(id)` (top-level)
                // だけだと cascade sub-popup を orphan させる。 同 frame で開いている cascade を全部
                // 閉じてから (zero-frame) top-level を閉じる。 通常 top-level item は cascade が無いので
                // close_orphaned_cascades は no-op (= 既存挙動不変)。
                close_orphaned_cascades(self, &entries, &id_path);
                self.close_popup(id);
            }
        }
    }

    /// 右クリックで popup を出す context_menu。library が右クリック検出を担当。
    /// `rect` 内で右クリックが発生したら、その位置で `items` を popup として表示。
    /// 各 item を選んだら対応する `Edit<M>` を発行。
    ///
    /// 利用者は右クリック判定 / popup ライフサイクル管理が一切不要 (M7 設計判断: library 吸収)。
    /// M9 P1-5 (C 案): `on_select` は `(idx, &mut Ui<'_, M>)` を受け、closure 内で任意の
    /// `Ui` 操作 (`push_edit` / `set_focus` / etc) を行う。
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
        // 右**クリック** (押して動かさずに離した) が rect 内なら、その press pos を open trigger に
        // する。検出を済ませたら描画 / 選択処理は programmatic な `context_menu_at` に委譲 (DRY)。
        // id は rect 由来で従来互換 (同じ rect → 毎フレーム同じ popup)。
        //
        // daw_01 r.md #35: 旧実装は `secondary_just_pressed` (= 押した瞬間) で開いていたが、
        // 右ドラッグを矩形選択に使えるよう **release かつ移動 4px 未満** に変更した。
        // Windows の `WM_CONTEXTMENU` も右ボタン UP で飛ぶので、こちらが本来の標準でもある。
        let open_at = self.take_secondary_click_in_rect(rect);
        self.context_menu_at(
            ("context_menu", rect.x.to_bits(), rect.y.to_bits()),
            open_at,
            items,
            on_select,
        );
    }

    /// 任意座標に programmatic にコンテキストメニューを開く。`context_menu_for` の
    /// 「右クリック検出を caller 側が済ませた」版で、`open_at` が `Some(pos)` の frame に
    /// `pos` を anchor (画面下端 / 右端 clamp 込み、flip しない DAW 標準) にメニューを開く。
    /// 以後は item 選択 / メニュー外 click まで描画を維持する (immediate-mode、**毎フレーム
    /// 呼ぶ**)。
    ///
    /// `arrangement` の `SecondaryClickEmpty` 等、**既に検出済みのイベント**に応じてメニューを
    /// 出す用途。典型的には caller は (a) イベント発生 frame に pos + 文脈 (track / beat 等) を
    /// model へ stash し、(b) 次フレーム以降 `open_at = stash_pos.take()` (1 frame だけ `Some`)
    /// で開き、(c) `on_select` で stash した文脈を使って `push_edit` する。
    ///
    /// `id`: popup の同一性キー (同じ menu を指す限り毎フレーム同じ値を渡す)。
    /// `on_select`: item index と `&mut Ui` を受け、選択時に任意操作を行う
    /// (`context_menu_for` と同一シグネチャ)。
    pub fn context_menu_at<F>(
        &mut self,
        id: impl std::hash::Hash,
        open_at: Option<(f32, f32)>,
        items: &[&str],
        on_select: F,
    ) where
        F: for<'ui> FnOnce(usize, &mut Ui<'ui, M>),
    {
        let menu_id = ("context_menu_at", &id);
        let n = items.len();
        if let Some((px, py)) = open_at {
            let popup_h = (n as f32) * MENU_ITEM_H;
            // 幅は項目に合わせて伸ばす (日本語の項目名が 180px 固定枠から
            // はみ出して背後の画面に直描きされていた)。
            let popup_w = items_popup_width(self, items, MENU_W_DEFAULT);
            let anchor = crate::popup::popup_rect_clamped_at(
                (px, py),
                popup_w,
                popup_h,
                self.screen(),
            );
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
            scene.iter_popup_glyphs().map(|g| g.text.as_ref()).collect();
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

    /// 兄弟 sub_menu に hover が移ったとき、 旧 cascade は close され、 新 cascade のみ open
    /// になる (daw_01 #037 root cause regression test)。
    #[test]
    fn sibling_sub_menus_are_mutually_exclusive_on_hover() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let bar_rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 };

        // Frame 1: "File" を click して top-level popup を open
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 16.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| {
                ui.menu_bar(bar_rect, |bar| {
                    bar.menu("File", |menu| {
                        menu.sub_menu("Open Recent", |sub| {
                            sub.item("wav01", |_| {});
                        });
                        menu.sub_menu("Recently Saved", |sub| {
                            sub.item("saved01", |_| {});
                        });
                    });
                });
            },
        );

        // Frame 2: "Open Recent" (popup item 0、 y = 32 + 12 = 44) に hover → cascade A open
        scene.clear();
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 44.0)),
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| {
                ui.menu_bar(bar_rect, |bar| {
                    bar.menu("File", |menu| {
                        menu.sub_menu("Open Recent", |sub| {
                            sub.item("wav01", |_| {});
                        });
                        menu.sub_menu("Recently Saved", |sub| {
                            sub.item("saved01", |_| {});
                        });
                    });
                });
            },
        );
        let texts: Vec<&str> =
            scene.iter_popup_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(
            texts.contains(&"wav01"),
            "Open Recent cascade が open し sub item text が描画される: {texts:?}"
        );

        // Frame 3: "Recently Saved" (popup item 1、 y = 32 + 24 + 12 = 68) に hover →
        // cascade A は close、 cascade B のみ open
        scene.clear();
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 68.0)),
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| {
                ui.menu_bar(bar_rect, |bar| {
                    bar.menu("File", |menu| {
                        menu.sub_menu("Open Recent", |sub| {
                            sub.item("wav01", |_| {});
                        });
                        menu.sub_menu("Recently Saved", |sub| {
                            sub.item("saved01", |_| {});
                        });
                    });
                });
            },
        );
        let texts: Vec<&str> =
            scene.iter_popup_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(
            texts.contains(&"saved01"),
            "Recently Saved cascade が open に切り替わる: {texts:?}"
        );
        assert!(
            !texts.contains(&"wav01"),
            "旧 Open Recent cascade は close される (兄弟排他): {texts:?}"
        );
    }

    /// sub_menu cascade item を click すると action が発火する (daw_01 #038 root cause
    /// regression test、 親 popup の outside_click 判定で握りつぶされていた bug)。
    #[test]
    fn cascade_item_click_fires_action() {
        struct M {
            fired: bool,
        }
        let mut host: UiHost<M> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = M { fired: false };
        let screen = PhysicalSize { width: 800, height: 600 };
        let bar_rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 };

        // Frame 1: "File" を click して top-level popup を open
        host.frame(
            &mut model,
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 16.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |_, ui| {
                ui.menu_bar(bar_rect, |bar| {
                    bar.menu("File", |menu| {
                        menu.sub_menu("Open Recent", |sub| {
                            sub.item("wav01", |ui_inner| {
                                ui_inner.push_edit(Edit::mutate(|m: &mut M| {
                                    m.fired = true;
                                }));
                            });
                        });
                    });
                });
            },
        );

        // Frame 2: "Open Recent" に hover (popup item 0、 y = 44) → cascade open
        host.frame(
            &mut model,
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 44.0)),
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |_, ui| {
                ui.menu_bar(bar_rect, |bar| {
                    bar.menu("File", |menu| {
                        menu.sub_menu("Open Recent", |sub| {
                            sub.item("wav01", |ui_inner| {
                                ui_inner.push_edit(Edit::mutate(|m: &mut M| {
                                    m.fired = true;
                                }));
                            });
                        });
                    });
                });
            },
        );

        // Frame 3: cascade item "wav01" を click。 sub-popup は親 popup の右隣 (=
        // popup_rect.x + MENU_W_DEFAULT 以降) に開くので x を cascade 内に取る。
        let cascade_x = MENU_W_DEFAULT + 20.0;
        host.frame(
            &mut model,
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((cascade_x, 44.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |_, ui| {
                ui.menu_bar(bar_rect, |bar| {
                    bar.menu("File", |menu| {
                        menu.sub_menu("Open Recent", |sub| {
                            sub.item("wav01", |ui_inner| {
                                ui_inner.push_edit(Edit::mutate(|m: &mut M| {
                                    m.fired = true;
                                }));
                            });
                        });
                    });
                });
            },
        );

        assert!(
            model.fired,
            "cascade item の click で action が発火する (= 親 popup の outside_click で\
             握りつぶされない)"
        );
    }

    /// daw_01 #095 (重大バグ): cascade item を click すると **cascade sub-popup も閉じる**。
    /// 旧実装は top-level popup (`menu_bar_top/File`) のみ close し、 hover open した cascade
    /// (`menu_bar/File/0`) が modal popup として `open_popups` に orphan していた。 見えないのに
    /// anchor 内 pointer/keyboard を遮断し続け、 アレンジ上部の track rename を不発にした。
    #[test]
    fn cascade_item_click_closes_sub_popup() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let bar_rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 };

        let build = |ui: &mut Ui<'_, ()>| {
            ui.menu_bar(bar_rect, |bar| {
                bar.menu("File", |menu| {
                    menu.sub_menu("Open Recent", |sub| {
                        sub.item("wav01", |_| {});
                    });
                });
            });
        };

        // Frame 1: "File" click → top-level open。
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 16.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build(ui),
        );
        // Frame 2: "Open Recent" (popup item 0、 y=44) hover → cascade open。
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame { pos: Some((20.0, 44.0)), ..PointerFrame::default() },
                ..Default::default()
            },
            |(), ui| build(ui),
        );
        // Frame 3: cascade item "wav01" を click (cascade は親の右隣 x、 y=44)。
        let cascade_x = MENU_W_DEFAULT + 20.0;
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((cascade_x, 44.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build(ui),
        );
        // Frame 4: menu_bar を呼ぶ前に frame 3 が残した popup 状態を観測 (cascade orphan の検出)。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            assert!(
                !ui.is_popup_open(("menu_bar_top", "File")),
                "cascade item click 後、 top-level popup は閉じる"
            );
            assert!(
                !ui.is_popup_open(format!("menu_bar/File/{}", 0)),
                "cascade item click 後、 cascade sub-popup も閉じる (orphan しない)"
            );
        });
    }

    /// daw_01 #095: cascade を開いたまま **popup 外を click** して menu を閉じた場合も、 cascade
    /// sub-popup を orphan させない (実機で「Esc / 外 click でも消えない」 と確認された経路。 top-level が
    /// `popup_layer` の outside-click で閉じ `draw_menu_entries` が走らなくなるため、 menu_bar の
    /// 次フレーム `!is_open` cleanup で ≤1 frame で回収する)。
    #[test]
    fn cascade_orphan_cleared_after_outside_click() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let bar_rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 };

        let build = |ui: &mut Ui<'_, ()>| {
            ui.menu_bar(bar_rect, |bar| {
                bar.menu("File", |menu| {
                    menu.sub_menu("Open Recent", |sub| {
                        sub.item("wav01", |_| {});
                    });
                });
            });
        };

        // Frame 1: File click → open。 Frame 2: Open Recent hover → cascade open。
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 16.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build(ui),
        );
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame { pos: Some((20.0, 44.0)), ..PointerFrame::default() },
                ..Default::default()
            },
            |(), ui| build(ui),
        );
        // Frame 3: 全 popup の外 (700, 500) を press → top-level が outside-click で閉じる。
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((700.0, 500.0)),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build(ui),
        );
        // Frame 4: menu_bar の `!is_open` cleanup が orphan cascade を回収する。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| build(ui));
        // Frame 5: cleanup 後の状態を観測。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            assert!(
                !ui.is_popup_open(("menu_bar_top", "File")),
                "外 click で top-level popup は閉じる"
            );
            assert!(
                !ui.is_popup_open(format!("menu_bar/File/{}", 0)),
                "外 click で menu を閉じた後、 cascade sub-popup も回収される (orphan しない)"
            );
        });
    }

    // ===== M14 Phase 98 (daw_01 #070): top-level menu 切替 =====
    //
    // File / Edit / View を並べ、各 menu に固有 item を 1 つ持たせる。top-level ラベルは
    // 各 56px (File[0,56) / Edit[56,112) / View[112,168))。popup item text を
    // `iter_popup_glyphs` で読み「どの menu が open か」を判定する (top-level ラベル自体は
    // popup_layer の外で描かれるので popup glyphs には乗らない)。

    fn build_three_menus(ui: &mut Ui<'_, ()>) {
        ui.menu_bar(Rect { x: 0.0, y: 0.0, w: 800.0, h: 32.0 }, |bar| {
            bar.menu("File", |m| {
                m.item("file_only", |_| {});
            });
            bar.menu("Edit", |m| {
                m.item("edit_only", |_| {});
            });
            bar.menu("View", |m| {
                m.item("view_only", |_| {});
            });
        });
    }

    fn popup_texts(scene: &Scene) -> Vec<&str> {
        scene.iter_popup_glyphs().map(|g| g.text.as_ref()).collect()
    }

    /// File が open 中に View ラベルを click → View に切替 (旧 File は閉じる)。主訴の修正。
    #[test]
    fn top_level_click_switches_to_other_menu() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        // Frame 1: File ラベル (20,16) を click → File open
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 16.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build_three_menus(ui),
        );

        // Frame 2: File open のまま View ラベル (130,16) を click → View へ切替
        scene.clear();
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((130.0, 16.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build_three_menus(ui),
        );
        let texts = popup_texts(&scene);
        assert!(
            texts.contains(&"view_only"),
            "click で View へ切替 → View popup が描画される: {texts:?}"
        );
        assert!(
            !texts.contains(&"file_only"),
            "切替で旧 File popup は閉じる: {texts:?}"
        );
    }

    /// いずれかが open 中に別ラベルへ hover (ボタン非押下) → 切替。
    #[test]
    fn top_level_hover_switches_when_a_menu_is_open() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        // Frame 1: File を click して open
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 16.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build_three_menus(ui),
        );

        // Frame 2: ボタンを押さず View ラベル (130,16) へ hover → View へ切替
        scene.clear();
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((130.0, 16.0)),
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build_three_menus(ui),
        );
        let texts = popup_texts(&scene);
        assert!(
            texts.contains(&"view_only"),
            "hover で View へ切替 → View popup が描画される: {texts:?}"
        );
        assert!(
            !texts.contains(&"file_only"),
            "hover 切替で旧 File popup は閉じる: {texts:?}"
        );
    }

    /// open 中の同ラベルを再 click → toggle close。
    #[test]
    fn top_level_click_on_open_label_toggles_closed() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        // Frame 1: File を click して open
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 16.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build_three_menus(ui),
        );

        // Frame 2: 同じ File ラベルを再 click → toggle close
        scene.clear();
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 16.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build_three_menus(ui),
        );
        let texts = popup_texts(&scene);
        assert!(
            texts.is_empty(),
            "open 中ラベルの再 click で popup が閉じる (toggle): {texts:?}"
        );
    }

    /// 全 menu が閉じている状態では hover しても開かない (click で開く挙動は維持)。
    #[test]
    fn closed_menu_bar_does_not_open_on_hover() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        // hover のみ (click なし) → 何も開かない
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 16.0)),
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| build_three_menus(ui),
        );
        let texts = popup_texts(&scene);
        assert!(
            texts.is_empty(),
            "閉じている状態では hover で開かない: {texts:?}"
        );
    }

    // ===== M14 Phase 99 (daw_01 #071): context_menu_at (programmatic open) =====

    /// `open_at = Some(pos)` で popup を開き、item text が popup glyph に描画される。
    #[test]
    fn context_menu_at_opens_and_draws_items() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 400, height: 300 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.context_menu_at("ctx", Some((50.0, 60.0)), &["Alpha", "Beta"], |_idx, _ui| {});
        });
        let texts: Vec<&str> = scene.iter_popup_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(
            texts.contains(&"Alpha") && texts.contains(&"Beta"),
            "open_at=Some で items が popup に描画される: {texts:?}"
        );
    }

    /// `open_at = None` のまま (一度も開いていない) なら popup は出ない。
    #[test]
    fn context_menu_at_none_does_not_open() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 400, height: 300 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.context_menu_at("ctx", None, &["Alpha", "Beta"], |_idx, _ui| {});
        });
        let texts: Vec<&str> = scene.iter_popup_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(texts.is_empty(), "open_at=None で popup は出ない: {texts:?}");
    }

    /// open 後に item をクリック → on_select が当該 index で発火する。
    #[test]
    fn context_menu_at_click_fires_on_select() {
        struct Sel {
            picked: Option<usize>,
        }
        let mut host: UiHost<Sel> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 400, height: 300 };
        let mut model = Sel { picked: None };

        // frame 1: (50,60) で開く (anchor = {50,60,180,48}、 item0 = y[60,84))。
        host.frame(&mut model, &mut scene, screen, FrameInput::default(), |_m, ui| {
            ui.context_menu_at("ctx", Some((50.0, 60.0)), &["Alpha", "Beta"], |idx, ui| {
                ui.push_edit(Edit::mutate(move |m: &mut Sel| m.picked = Some(idx)));
            });
        });
        // frame 2: item0 中央 (140,72) を click (release) → on_select(0)。
        scene.clear();
        let click = FrameInput {
            pointer: PointerFrame {
                pos: Some((140.0, 72.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        host.frame(&mut model, &mut scene, screen, click, |_m, ui| {
            ui.context_menu_at("ctx", None, &["Alpha", "Beta"], |idx, ui| {
                ui.push_edit(Edit::mutate(move |m: &mut Sel| m.picked = Some(idx)));
            });
        });
        assert_eq!(model.picked, Some(0), "item0 click → on_select(0) 発火");
    }

    /// M14 Phase 100 (#071 review): open 中 popup の **外** を **右クリック** すると閉じる
    /// (popup_layer の outside-close に secondary_just_pressed を含めた regression test)。
    #[test]
    fn context_menu_at_secondary_press_outside_closes() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 400, height: 300 };
        // frame 1: (50,60) で開く (anchor = {50,60,180,48} = x[50,230) y[60,108))。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.context_menu_at("ctx", Some((50.0, 60.0)), &["Alpha", "Beta"], |_idx, _ui| {});
        });
        // frame 2: anchor 外 (300,250) を右クリック → 閉じる。
        scene.clear();
        let rclick_outside = FrameInput {
            pointer: PointerFrame {
                pos: Some((300.0, 250.0)),
                secondary_just_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        host.frame_to_edits(&(), &mut scene, screen, rclick_outside, |(), ui| {
            ui.context_menu_at("ctx", None, &["Alpha", "Beta"], |_idx, _ui| {});
        });
        let texts: Vec<&str> = scene.iter_popup_glyphs().map(|g| g.text.as_ref()).collect();
        assert!(texts.is_empty(), "anchor 外の右クリックで popup が閉じる: {texts:?}");
    }
}
