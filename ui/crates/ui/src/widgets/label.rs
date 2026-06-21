//! `label` ウィジェット — テキスト 1 行を表示するだけ。

use daw_ui_renderer::{Color, GlyphArea, Rect};

use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::Ui;

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 与えた矩形にテキストを置く (ヒットテストなし)。
    /// M4 Phase 11: text + 位置 + font_size + 色 が同じなら描画キャッシュ命中。
    pub fn label_at(
        &mut self,
        id: impl std::hash::Hash,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        color: Color,
    ) {
        let wid = WidgetId::ROOT.child((b"label_at", &id));
        let input_hash = hash_inputs((
            b"label_at",
            text,
            x.to_bits(),
            y.to_bits(),
            font_size.to_bits(),
            color.r.to_bits(),
            color.g.to_bits(),
            color.b.to_bits(),
            color.a.to_bits(),
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            ui.push_text(GlyphArea {
                text: text.into(),
                left: x,
                top: y,
                font_size,
                line_height: font_size * 1.2,
                color,
                clip_rect: None,
                ..GlyphArea::default()
            });
        });
    }

    /// `label_at` の矩形制限版。 `rect.w` を超えるテキストは末尾を ellipsis で省略し、
    /// `rect` で clip する (toggle_button / button と同じ daw_01 #079 idiom)。 収まる
    /// 場合は `label_at` と同一描画 (clip None, byte 互換)。 同じ行に別 widget を隣接
    /// させる狭い領域で、 長いラベルが相手にかぶらないようにするのに使う (例: mixer
    /// send slot の宛先名 + × ボタン)。 テキストは `rect` 左上から描く。
    pub fn label_at_clipped(
        &mut self,
        id: impl std::hash::Hash,
        text: &str,
        rect: Rect,
        font_size: f32,
        color: Color,
    ) {
        let wid = WidgetId::ROOT.child((b"label_at_clipped", &id));
        let input_hash = hash_inputs((
            b"label_at_clipped",
            text,
            rect.x.to_bits(),
            rect.y.to_bits(),
            rect.w.to_bits(),
            rect.h.to_bits(),
            font_size.to_bits(),
            color.r.to_bits(),
            color.g.to_bits(),
            color.b.to_bits(),
            color.a.to_bits(),
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            let (display, _w) = ui.fit_text_ellipsized(text, font_size, rect.w);
            let truncated = matches!(display, std::borrow::Cow::Owned(_));
            ui.push_text(GlyphArea {
                text: display.as_ref().into(),
                left: rect.x,
                top: rect.y,
                font_size,
                line_height: font_size * 1.2,
                color,
                clip_rect: truncated.then_some(rect),
                ..GlyphArea::default()
            });
        });
    }

    /// vstack カーソル位置に 1 行ラベルを追加。
    pub fn label(&mut self, id: impl std::hash::Hash, text: &str) {
        let wid = WidgetId::ROOT.child((b"label", &id));
        let pad = 8.0_f32;
        let font_size = 16.0_f32;
        let line_h = font_size * 1.2;

        let x = self.cursor.x + pad;
        let y = self.cursor.y + self.next_y + pad * 0.5;
        let input_hash = hash_inputs((
            b"label",
            text,
            x.to_bits(),
            y.to_bits(),
            font_size.to_bits(),
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            ui.push_text(GlyphArea {
                text: text.into(),
                left: x,
                top: y,
                font_size,
                line_height: line_h,
                color: Color::rgb(0.92, 0.92, 0.94),
                clip_rect: None,
                ..GlyphArea::default()
            });
        });
        self.next_y += line_h + pad;
    }
}
