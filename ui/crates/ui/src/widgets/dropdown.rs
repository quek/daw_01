//! `dropdown` widget — combobox 風の値選択 UI (M7 Phase 25)。
//!
//! `popup_layer` + `menu::draw_items_popup` を再利用。クリックで items を popup 表示、
//! 選択で `Some(idx)` を返す (利用者が Edit を発行)。

use std::hash::Hash;

use daw_ui_renderer::{GlyphArea, LineBatch, LineSegment, Rect, RectCommand};
use crate::theme;

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
        let pointer = self.pointer;
        // popup state は caller-id ベース (rect 座標を入れると 1px 動いて state 蒸発 / 同位置別
        // dropdown で衝突する)。
        let popup_id = ("dropdown_popup", WidgetId::ROOT.child((b"dropdown", &id)));
        let inside = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));
        let already_open = self.is_popup_open(popup_id);

        // 1. 本体描画 (現在値の表示 + 三角アロー)
        let bg_fill = if inside {
            theme::INSET_BG.lighten(0.06)
        } else {
            theme::INSET_BG
        };
        let border = if already_open {
            theme::BORDER_FOCUS
        } else {
            theme::BORDER
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
                text: label.into(),
                left: rect.x + DROPDOWN_PAD_X,
                top: rect.y + (rect.h - DROPDOWN_FONT * 1.2) * 0.5,
                font_size: DROPDOWN_FONT,
                line_height: DROPDOWN_FONT * 1.2,
                color: theme::TEXT,
                clip_rect: None,
                ..GlyphArea::default()
            });
        }

        // ▼ アロー (右端、線で三角)
        let arrow_x = rect.x + rect.w - DROPDOWN_ARROW_W * 0.5;
        let arrow_y = rect.y + rect.h * 0.5;
        let arrow_size = 4.0;
        let arrow_color = theme::TEXT_DIM;
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
            ]
            .into(),
            line_width_px: 1.5,
            clip_rect: None,
        });

        // popup_rect は popup_rect_below_or_above で auto-flip + clamp 込み計算
        // (画面下端で popup がはみ出す場合は上に flip)。 anchor は body rect + popup_rect
        // の汎用 union で flip 後でも outside_click 判定が両方を「内」 として扱える。
        let popup_h = (items.len() as f32) * DROPDOWN_ITEM_H;
        let popup_rect =
            crate::popup::popup_rect_below_or_above(rect, rect.w, popup_h, self.screen());
        let union_left = rect.x.min(popup_rect.x);
        let union_top = rect.y.min(popup_rect.y);
        let anchor = Rect {
            x: union_left,
            y: union_top,
            w: (rect.x + rect.w).max(popup_rect.x + popup_rect.w) - union_left,
            h: (rect.y + rect.h).max(popup_rect.y + popup_rect.h) - union_top,
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
