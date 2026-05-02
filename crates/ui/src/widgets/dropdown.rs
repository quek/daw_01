//! `dropdown` widget — combobox 風の値選択 UI (M7 Phase 25)。
//!
//! `popup_layer` + `menu::draw_items_popup` を再利用。クリックで items を popup 表示、
//! 選択で `Some(idx)` を返す (利用者が Edit を発行)。

use std::hash::Hash;

use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect, RectCommand};

use crate::id::WidgetId;
use crate::ui::Ui;
use crate::widgets::menu::draw_items_popup;

const DROPDOWN_FONT: f32 = 14.0;
const DROPDOWN_PAD_X: f32 = 8.0;
const DROPDOWN_ARROW_W: f32 = 16.0;
const DROPDOWN_ITEM_H: f32 = 24.0;

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// dropdown / combobox widget。クリックで items の popup を開き、選択で `Some(idx)` を返す。
    /// 利用者は `Some(idx)` から `Edit::mutate(...)` で Model 側の選択 index を更新する。
    ///
    /// `selected` は現在選択中の index (0-based、`items` 範囲外なら何も表示しない)。
    pub fn dropdown(
        &mut self,
        id: impl Hash,
        rect: Rect,
        items: &[&str],
        selected: usize,
    ) -> Option<usize> {
        let _wid = WidgetId::ROOT.child((b"dropdown", &id));
        let pointer = self.pointer;
        let popup_id = ("dropdown_popup", rect.x.to_bits(), rect.y.to_bits());
        let inside = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));
        let already_open = self.is_popup_open(popup_id);

        // 1. 本体描画 (現在値の表示 + 三角アロー)
        let bg_fill = if inside {
            Color::rgb(0.18, 0.20, 0.24)
        } else {
            Color::rgb(0.12, 0.13, 0.16)
        };
        let border = if already_open {
            Color::rgb(0.55, 0.78, 0.95)
        } else {
            Color::rgb(0.30, 0.33, 0.39)
        };
        self.push_rect(RectCommand {
            rect,
            fill: bg_fill,
            border,
            border_width: 1.0,
            radius: [3.0; 4],
            clip_rect: None,
        });

        let label = items.get(selected).copied().unwrap_or("");
        if !label.is_empty() {
            self.push_text(GlyphArea {
                text: label.to_string(),
                left: rect.x + DROPDOWN_PAD_X,
                top: rect.y + (rect.h - DROPDOWN_FONT * 1.2) * 0.5,
                font_size: DROPDOWN_FONT,
                line_height: DROPDOWN_FONT * 1.2,
                color: Color::rgb(0.92, 0.92, 0.94),
                clip_rect: None,
            });
        }

        // ▼ アロー (右端、線で三角)
        let arrow_x = rect.x + rect.w - DROPDOWN_ARROW_W * 0.5;
        let arrow_y = rect.y + rect.h * 0.5;
        let arrow_size = 4.0;
        let arrow_color = Color::rgb(0.75, 0.78, 0.85);
        self.push_lines(LineBatch {
            segments: vec![
                LineSegment {
                    a: [arrow_x - arrow_size, arrow_y - arrow_size * 0.5],
                    b: [arrow_x, arrow_y + arrow_size * 0.5],
                    color: arrow_color,
                },
                LineSegment {
                    a: [arrow_x, arrow_y + arrow_size * 0.5],
                    b: [arrow_x + arrow_size, arrow_y - arrow_size * 0.5],
                    color: arrow_color,
                },
            ],
            line_width_px: 1.5,
            clip_rect: None,
        });

        // popup_rect = items 全体の rect。anchor は body rect + popup_rect の union で
        // outside_click 判定が popup の見える範囲全体で行われるようにする。
        let popup_rect = Rect {
            x: rect.x,
            y: rect.y + rect.h,
            w: rect.w,
            h: (items.len() as f32) * DROPDOWN_ITEM_H,
        };
        let anchor = Rect {
            x: rect.x.min(popup_rect.x),
            y: rect.y.min(popup_rect.y),
            w: rect.w.max(popup_rect.w),
            h: (rect.y + rect.h + popup_rect.h) - rect.y,
        };

        // 2. クリックで popup toggle (click は consume して下層に流さない)
        if inside && pointer.primary_just_released {
            if already_open {
                self.close_popup(popup_id);
            } else {
                self.open_popup(popup_id, anchor, true);
            }
            self.consume_pointer_click();
        }

        // 3. popup 描画 + 選択検出
        let mut chosen: Option<usize> = None;
        self.popup_layer(popup_id, |ui| {
            chosen = draw_items_popup(ui, items, popup_rect);
        });
        if let Some(idx) = chosen {
            self.close_popup(popup_id);
            return Some(idx);
        }
        None
    }
}
