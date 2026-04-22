use std::path::PathBuf;

use vizia::prelude::*;

use crate::app::AppData;

pub struct StatusBarView;

impl StatusBarView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            Label::new(
                cx,
                AppData::file_path.map(|p: &Option<PathBuf>| {
                    p.as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(untitled)".to_string())
                }),
            )
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
