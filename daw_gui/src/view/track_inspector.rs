use vizia::prelude::*;

pub struct TrackInspectorView;

impl TrackInspectorView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            Label::new(cx, "Track Inspector")
                .padding(Pixels(12.0))
                .font_size(16.0);
        })
        .background_color(Color::rgb(40, 40, 44))
    }
}

impl View for TrackInspectorView {
    fn element(&self) -> Option<&'static str> {
        Some("track-inspector")
    }
}
