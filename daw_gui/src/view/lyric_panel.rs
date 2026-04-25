//! Side panel showing the lyric of the currently selected note. Hidden
//! behind a placeholder when no note is selected.

use vizia::prelude::*;

use crate::app::{AppData, AppEvent};

pub struct LyricPanel;

impl LyricPanel {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            VStack::new(cx, |cx| {
                Label::new(cx, "Lyric")
                    .font_size(13.0)
                    .color(Color::rgb(180, 180, 180));
                Binding::new(cx, AppData::selected_notes, |cx, sel| {
                    if sel.get(cx).is_empty() {
                        Label::new(cx, "(ノート未選択)")
                            .color(Color::rgb(120, 120, 120))
                            .font_size(12.0);
                    } else {
                        Textbox::new(cx, AppData::selected_lyric)
                            .on_edit(|ex, t| {
                                ex.emit(AppEvent::SetSelectedNoteLyric(t));
                            })
                            .width(Stretch(1.0))
                            .height(Pixels(28.0));
                    }
                });
            })
            .padding(Pixels(8.0))
            .gap(Pixels(6.0))
            .background_color(Color::rgb(36, 36, 40));
        })
    }
}

impl View for LyricPanel {
    fn element(&self) -> Option<&'static str> {
        Some("lyric-panel")
    }
}
