use vizia::prelude::*;

pub struct ArrangementView;

impl ArrangementView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            Label::new(cx, "Arrangement View")
                .padding(Pixels(12.0))
                .font_size(18.0);
        })
        .background_color(Color::rgb(32, 32, 36))
    }
}

impl View for ArrangementView {
    fn element(&self) -> Option<&'static str> {
        Some("arrangement-view")
    }
}
