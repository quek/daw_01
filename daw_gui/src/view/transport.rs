use common::model::Song;
use vizia::prelude::*;

use crate::app::{AppData, AppEvent};

pub struct TransportView;

impl TransportView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            HStack::new(cx, |cx| {
                Button::new(cx, |cx| Label::new(cx, "▶ Play"))
                    .on_press(|ex| ex.emit(AppEvent::Play));
                Button::new(cx, |cx| Label::new(cx, "⏹ Stop"))
                    .on_press(|ex| ex.emit(AppEvent::Stop));
                Label::new(cx, "0:00 / 64 beats").padding_left(Pixels(16.0));
                Label::new(
                    cx,
                    AppData::song.map(|s: &Song| format!("BPM {}", s.bpm)),
                )
                .padding_left(Pixels(16.0));
            })
            .gap(Pixels(8.0))
            .padding(Pixels(6.0))
            .alignment(Alignment::Left);
        })
        .background_color(Color::rgb(48, 48, 52))
    }
}

impl View for TransportView {
    fn element(&self) -> Option<&'static str> {
        Some("transport")
    }
}
