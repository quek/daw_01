use vizia::prelude::*;

use crate::app::{AppData, AppEvent};

pub struct TrackInspectorView;

impl TrackInspectorView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            VStack::new(cx, |cx| {
                Label::new(cx, "Track Inspector")
                    .font_size(16.0)
                    .color(Color::rgb(220, 220, 220));

                // --- Instrument plugin section -----------------------------
                Label::new(cx, "Instrument")
                    .padding_top(Pixels(12.0))
                    .font_size(12.0)
                    .color(Color::rgb(160, 160, 160));

                Label::new(cx, AppData::clap_plugin_label)
                    .padding_top(Pixels(2.0))
                    .color(Color::rgb(220, 220, 220));

                Button::new(cx, |cx| Label::new(cx, "Change Plugin..."))
                    .on_press(|ex| ex.emit(AppEvent::ChangeClapPlugin))
                    .padding_top(Pixels(6.0));
            })
            .padding(Pixels(12.0))
            .gap(Pixels(2.0))
            .alignment(Alignment::TopLeft);
        })
        .background_color(Color::rgb(40, 40, 44))
    }
}

impl View for TrackInspectorView {
    fn element(&self) -> Option<&'static str> {
        Some("track-inspector")
    }
}
