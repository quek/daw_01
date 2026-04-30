//! `button` ウィジェット — クリックされると `Edit<M>` を発行する。

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::ui::{Ui, clicked, hovered, lerp_color, pressed_inside};

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定でボタンを描画+ヒットテスト。クリック時 `on_click()` の返り値を Edit 列に積む。
    pub fn button_at(
        &mut self,
        id: impl std::hash::Hash,
        text: &str,
        rect: Rect,
        on_click: impl FnOnce() -> Edit<M>,
    ) {
        let _wid = WidgetId::ROOT.child((b"button", &id));

        let base = Color::rgb(0.18, 0.20, 0.26);
        let hover = Color::rgb(0.24, 0.27, 0.34);
        let press = Color::rgb(0.32, 0.55, 0.85);

        let pointer = self.pointer;
        let fill = if pressed_inside(rect, pointer) {
            press
        } else if hovered(rect, pointer) {
            lerp_color(base, hover, 0.85)
        } else {
            base
        };

        self.push_rect(RectCommand {
            rect,
            fill,
            border: Color::rgb(0.35, 0.38, 0.45),
            border_width: 1.0,
            radius: [6.0; 4],
        });

        // テキストを矩形中央付近に
        let font_size = 16.0;
        let line_h = font_size * 1.2;
        // 簡易: ASCII 文字幅を 9px 仮定。日本語は適当に配置 (M1)。
        let approx_w = (text.chars().count() as f32) * 9.0;
        let tx = rect.x + (rect.w - approx_w).max(0.0) * 0.5;
        let ty = rect.y + (rect.h - line_h).max(0.0) * 0.5;
        self.push_text(GlyphArea {
            text: text.to_string(),
            left: tx,
            top: ty,
            font_size,
            line_height: line_h,
            color: Color::rgb(0.95, 0.95, 0.97),
        });

        if clicked(rect, pointer) {
            let edit = on_click();
            self.push_edit(edit);
        }
    }

    /// vstack カーソル位置に 1 行ボタンを追加 (幅は cursor 幅 - padding)。
    pub fn button(
        &mut self,
        id: impl std::hash::Hash,
        text: &str,
        on_click: impl FnOnce() -> Edit<M>,
    ) {
        let pad = 8.0;
        let h = 32.0;
        let rect = Rect {
            x: self.cursor.x + pad,
            y: self.cursor.y + self.next_y,
            w: self.cursor.w - pad * 2.0,
            h,
        };
        self.button_at(id, text, rect, on_click);
        self.next_y += h + pad;
    }
}
