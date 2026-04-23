use vizia::prelude::*;

use crate::app::{AppData, AppEvent, PickerTarget};

pub struct TrackInspectorView;

impl TrackInspectorView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            VStack::new(cx, |cx| {
                Label::new(cx, "Track 1")
                    .font_size(16.0)
                    .color(Color::rgb(220, 220, 220));

                // Chain list: MIDI FX → Instrument → FX. Each row shows the
                // section label, the plugin name, and per-slot action
                // buttons [GUI] [×].
                Label::new(cx, "Chain")
                    .padding_top(Pixels(10.0))
                    .font_size(12.0)
                    .color(Color::rgb(160, 160, 160));

                List::new(cx, AppData::track0_chain, |cx, _idx, entry| {
                    HStack::new(cx, |cx| {
                        Label::new(cx, entry.map(|e| e.section_label.clone()))
                            .width(Pixels(72.0))
                            .font_size(10.0)
                            .color(Color::rgb(150, 150, 180));
                        Label::new(cx, entry.map(|e| e.plugin_name.clone()))
                            .width(Stretch(1.0))
                            .color(Color::rgb(220, 220, 220));
                        Button::new(cx, |cx| Label::new(cx, "GUI"))
                            .on_press(move |ex| {
                                let e = entry.get(ex);
                                ex.emit(AppEvent::ToggleSlotGui {
                                    slot_kind: e.slot_kind,
                                    slot_index: e.slot_index,
                                });
                            })
                            .width(Pixels(44.0))
                            .background_color(Color::rgb(55, 55, 60));
                        Button::new(cx, |cx| Label::new(cx, "×"))
                            .on_press(move |ex| {
                                let e = entry.get(ex);
                                ex.emit(AppEvent::RemoveSlot {
                                    slot_kind: e.slot_kind,
                                    slot_index: e.slot_index,
                                });
                            })
                            .width(Pixels(30.0))
                            .background_color(Color::rgb(70, 40, 40));
                    })
                    .alignment(Alignment::Center)
                    .padding(Pixels(3.0))
                    .gap(Pixels(6.0))
                    .height(Pixels(28.0));
                })
                .class("chain-list")
                .gap(Pixels(2.0));

                // Add-plugin buttons, one per CLAP section. Each opens the
                // Plugin Picker with the matching `PickerTarget`.
                HStack::new(cx, |cx| {
                    Button::new(cx, |cx| Label::new(cx, "+ Instrument"))
                        .on_press(|ex| {
                            ex.emit(AppEvent::OpenPluginPickerFor(PickerTarget::Instrument))
                        });
                    Button::new(cx, |cx| Label::new(cx, "+ Effect"))
                        .on_press(|ex| {
                            ex.emit(AppEvent::OpenPluginPickerFor(PickerTarget::Fx))
                        });
                    Button::new(cx, |cx| Label::new(cx, "+ MIDI FX"))
                        .on_press(|ex| {
                            ex.emit(AppEvent::OpenPluginPickerFor(PickerTarget::MidiFx))
                        });
                })
                .padding_top(Pixels(6.0))
                .gap(Pixels(4.0));
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
