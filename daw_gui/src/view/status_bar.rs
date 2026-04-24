use std::path::PathBuf;

use vizia::prelude::*;

use crate::app::AppData;

pub struct StatusBarView;

impl StatusBarView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            HStack::new(cx, |cx| {
                Label::new(
                    cx,
                    AppData::file_path.map(|p: &Option<PathBuf>| {
                        p.as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "(untitled)".to_string())
                    }),
                )
                .color(Color::rgb(200, 200, 200));

                Element::new(cx).width(Stretch(1.0));

                // Right-aligned status message for background tasks
                // (synthesis, rescan, etc.).
                Label::new(cx, AppData::status_message)
                    .color(Color::rgb(180, 220, 180));
            })
            .padding_left(Pixels(8.0))
            .padding_right(Pixels(8.0));
        })
        .background_color(Color::rgb(28, 28, 32))
    }
}

impl View for StatusBarView {
    fn element(&self) -> Option<&'static str> {
        Some("status-bar")
    }
}
