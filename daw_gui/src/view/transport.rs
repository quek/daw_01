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

                // Elastic spacer pushes the master strip to the right edge.
                Element::new(cx).width(Stretch(1.0));

                master_strip(cx);
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

const METER_HEIGHT: f32 = 28.0;
const METER_WIDTH: f32 = 5.0;

/// Master fader (horizontal slider) plus L/R peak meter. Emits
/// `AppEvent::SetMasterGain(f32::to_bits)` on slider drag; reads meter levels
/// from `AppData::peak_{l,r}_norm`.
fn master_strip(cx: &mut Context) {
    HStack::new(cx, |cx| {
        Label::new(cx, "MST")
            .font_size(11.0)
            .color(Color::rgb(180, 180, 180));

        Slider::new(cx, AppData::master_gain)
            .range(0.0..1.0)
            .on_change(|ex, value| ex.emit(AppEvent::SetMasterGain(value.to_bits())))
            .width(Pixels(120.0));

        // Two thin vertical bars. The outer VStack has alignment = Bottom so
        // the inner Element grows upward from the base when its height
        // (lens-bound) increases.
        meter_bar(cx, AppData::peak_l_norm);
        meter_bar(cx, AppData::peak_r_norm);
    })
    .gap(Pixels(6.0))
    .alignment(Alignment::Center);
}

fn meter_bar<L>(cx: &mut Context, norm: L)
where
    L: Lens<Target = f32>,
{
    VStack::new(cx, |cx| {
        Element::new(cx)
            .width(Pixels(METER_WIDTH))
            .height(norm.map(|n: &f32| Pixels(METER_HEIGHT * n.clamp(0.0, 1.0))))
            .background_color(norm.map(|n: &f32| {
                // Normal range green, hot yellow, clipping red. The thresholds
                // match the dB-normalized scale (METER_DB_MIN..METER_DB_MAX):
                //   > -6 dB (norm > 0.9) → yellow
                //   >= 0 dB (norm == 1.0) → red
                if *n >= 0.999 {
                    Color::rgb(220, 70, 70)
                } else if *n > 0.9 {
                    Color::rgb(230, 210, 80)
                } else {
                    Color::rgb(80, 200, 120)
                }
            }));
    })
    .width(Pixels(METER_WIDTH))
    .height(Pixels(METER_HEIGHT))
    .alignment(Alignment::BottomCenter)
    .background_color(Color::rgb(20, 20, 20));
}
