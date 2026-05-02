//! `tab_view` widget — タブで切り替え可能な複数 view (M7 Phase 26)。
//!
//! builder パターンで `tabs.tab(label, |ui, pane_rect| ...)` を順に並べる。
//! 選択中のタブだけ closure を実行 (= 各 closure は `FnOnce` でも問題なし)。

use std::hash::Hash;

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::id::WidgetId;
use crate::ui::Ui;

const TAB_BAR_H: f32 = 32.0;
const TAB_FONT: f32 = 14.0;
const TAB_PAD_X: f32 = 16.0;

/// `Ui::tab_view` の永続状態 (現在選択中の index)。
#[derive(Debug, Default)]
pub(crate) struct TabState {
    pub selected: usize,
}

/// `Ui::tab_view` のビルダー。`tab(label, |ui, pane_rect| ...)` で各タブを宣言する。
pub struct TabBuilder<'b, 'a, M: ?Sized + 'static> {
    ui: &'b mut Ui<'a, M>,
    bar_rect: Rect,
    pane_rect: Rect,
    next_x: f32,
    selected: usize,
    /// 0,1,2... と順に増える index counter
    next_index: usize,
    /// このフレームで click された index (label 描画フェーズで決定、次フレームの selected に反映)
    clicked: Option<usize>,
    state_wid: WidgetId,
}

impl<'b, 'a, M: ?Sized + 'static> TabBuilder<'b, 'a, M> {
    /// 1 つのタブを宣言。`label` がバーに表示される、選択中なら `f` が呼ばれて pane に描画。
    pub fn tab<F>(&mut self, label: &str, f: F)
    where
        F: FnOnce(&mut Ui<'a, M>, Rect),
    {
        let i = self.next_index;
        self.next_index += 1;

        // タブ label の rect (幅は文字数 × 8px + padding)
        let w = (label.chars().count() as f32) * 8.0 + TAB_PAD_X * 2.0;
        let tab_rect = Rect { x: self.bar_rect.x + self.next_x, y: self.bar_rect.y, w, h: TAB_BAR_H };
        self.next_x += w;

        let pointer = self.ui.pointer();
        let inside = pointer.pos.is_some_and(|(px, py)| tab_rect.contains(px, py));
        if inside && pointer.primary_just_released {
            self.clicked = Some(i);
        }

        let is_sel = i == self.selected;
        let fill = if is_sel {
            Color::rgb(0.20, 0.23, 0.28)
        } else if inside {
            Color::rgb(0.16, 0.18, 0.22)
        } else {
            Color::TRANSPARENT
        };
        if fill.a > 0.0 {
            self.ui.push_rect(RectCommand {
                rect: tab_rect,
                fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }
        self.ui.push_text(GlyphArea {
            text: label.to_string(),
            left: tab_rect.x + TAB_PAD_X,
            top: tab_rect.y + (tab_rect.h - TAB_FONT * 1.2) * 0.5,
            font_size: TAB_FONT,
            line_height: TAB_FONT * 1.2,
            color: if is_sel {
                Color::rgb(0.95, 0.97, 1.0)
            } else {
                Color::rgb(0.65, 0.68, 0.74)
            },
            clip_rect: None,
        });

        // 選択中なら pane を描画 (clip 適用)
        if is_sel {
            let pane = self.pane_rect;
            self.ui.with_clip_rect(pane, |ui| f(ui, pane));
        }
    }
}

impl<'b, 'a, M: ?Sized + 'static> Drop for TabBuilder<'b, 'a, M> {
    fn drop(&mut self) {
        // クリックがあれば次フレームの selected を更新
        if let Some(idx) = self.clicked {
            let n = self.next_index;
            let state: &mut TabState = self.ui.widget_state(self.state_wid);
            state.selected = idx.min(n.saturating_sub(1));
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// tab_view widget。`rect` 上部にタブバー、下に選択中タブの中身。
    /// builder を使って `tabs.tab(label, |ui, pane_rect| { ... })` を並べる。
    /// 選択中タブの closure のみ実行される (非選択は呼ばれない)。
    pub fn tab_view<F>(&mut self, id: impl Hash, rect: Rect, f: F)
    where
        F: FnOnce(&mut TabBuilder<'_, 'a, M>),
    {
        let wid = WidgetId::ROOT.child((b"tab_view", &id));
        let selected = {
            let state: &mut TabState = self.widget_state(wid);
            state.selected
        };

        // バー背景
        let bar_rect = Rect { x: rect.x, y: rect.y, w: rect.w, h: TAB_BAR_H };
        self.push_rect(RectCommand {
            rect: bar_rect,
            fill: Color::rgb(0.13, 0.14, 0.17),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });

        let pane_rect = Rect {
            x: rect.x,
            y: rect.y + TAB_BAR_H,
            w: rect.w,
            h: (rect.h - TAB_BAR_H).max(0.0),
        };

        let mut builder = TabBuilder {
            ui: self,
            bar_rect,
            pane_rect,
            next_x: 0.0,
            selected,
            next_index: 0,
            clicked: None,
            state_wid: wid,
        };
        f(&mut builder);
        // builder.drop() で state 更新
    }
}
