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
                // Loop toggle — highlighted when ON. The label itself swaps
                // between ON/OFF so the current state is obvious even before
                // the user learns the colour cue.
                Button::new(cx, |cx| {
                    Label::new(
                        cx,
                        AppData::is_looping
                            .map(|on: &bool| if *on { "🔁 Loop ON" } else { "🔁 Loop" }),
                    )
                })
                .on_press(|ex| ex.emit(AppEvent::ToggleLoop))
                .background_color(AppData::is_looping.map(|on: &bool| {
                    if *on {
                        Color::rgb(80, 140, 90)
                    } else {
                        Color::rgb(60, 60, 64)
                    }
                }));
                Label::new(cx, "0:00 / 64 beats")
                    .padding_left(Pixels(16.0))
                    .color(Color::rgb(220, 220, 220));
                Label::new(
                    cx,
                    AppData::song.map(|s: &Song| format!("BPM {}", s.bpm)),
                )
                .padding_left(Pixels(16.0))
                .color(Color::rgb(220, 220, 220));
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
