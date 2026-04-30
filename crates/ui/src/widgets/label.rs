//! `label` ウィジェット — テキスト 1 行を表示するだけ。

use daw_ui_renderer::{Color, GlyphArea};

use crate::id::WidgetId;
use crate::ui::Ui;

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 与えた矩形にテキストを置く (ヒットテストなし)。
    pub fn label_at(
        &mut self,
        _id: impl std::hash::Hash,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        color: Color,
    ) {
        self.push_text(GlyphArea {
            text: text.to_string(),
            left: x,
            top: y,
            font_size,
            line_height: font_size * 1.2,
            color,
        });
    }

    /// vstack カーソル位置に 1 行ラベルを追加。
    pub fn label(&mut self, id: impl std::hash::Hash, text: &str) {
        let _id = WidgetId::ROOT.child(id);
        let pad = 8.0;
        let font_size = 16.0;
        let line_h = font_size * 1.2;

        let x = self.cursor.x + pad;
        let y = self.cursor.y + self.next_y + pad * 0.5;
        self.push_text(GlyphArea {
            text: text.to_string(),
            left: x,
            top: y,
            font_size,
            line_height: line_h,
            color: Color::rgb(0.92, 0.92, 0.94),
        });
        self.next_y += line_h + pad;
    }
}
