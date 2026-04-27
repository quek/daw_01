//! Modal plugin picker overlay. Displays every entry from the plugin
//! database as a clickable row; selecting one emits
//! `AppEvent::SelectPluginFromDb` which swaps in the corresponding plugin.

use vizia::prelude::*;

use crate::app::{AppEvent, PluginPickEntry};

#[derive(Copy, Clone)]
pub struct PluginPickerSignals {
    pub plugin_picker_visible: Signal<Vec<PluginPickEntry>>,
    pub is_rescanning: Signal<bool>,
}

pub struct PluginPickerView;

impl PluginPickerView {
    pub fn new(cx: &mut Context, sig: PluginPickerSignals) -> Handle<'_, Self> {
        Self.build(cx, move |cx| {
            VStack::new(cx, move |cx| {
                // Title row: "Select Plugin" + Rescan + close buttons.
                HStack::new(cx, move |cx| {
                    Label::new(cx, "Select Plugin")
                        .font_size(16.0)
                        .color(Color::rgb(230, 230, 230));
                    Element::new(cx).width(Stretch(1.0));
                    Button::new(cx, move |cx| {
                        Label::new(
                            cx,
                            sig.is_rescanning.map(|r: &bool| {
                                if *r { "Rescanning..." } else { "Rescan" }.to_string()
                            }),
                        )
                        .color(Color::rgb(230, 230, 230))
                    })
                    .on_press(|ex| ex.emit(AppEvent::RescanPluginDb))
                    .background_color(Color::rgb(55, 55, 60))
                    .padding(Pixels(4.0));
                    Button::new(cx, |cx| Label::new(cx, "✕"))
                        .on_press(|ex| ex.emit(AppEvent::ClosePluginPicker));
                })
                .alignment(Alignment::Center)
                .padding(Pixels(8.0))
                .gap(Pixels(6.0))
                .height(Pixels(40.0));

                ScrollView::new(cx, move |cx| {
                    List::new(cx, sig.plugin_picker_visible, |cx, _idx, item| {
                        Button::new(cx, move |cx| {
                            HStack::new(cx, move |cx| {
                                Label::new(cx, item.map(|e| e.name.clone()))
                                    .color(Color::rgb(230, 230, 230));
                                Element::new(cx).width(Stretch(1.0));
                                Label::new(cx, item.map(|e| e.vendor.clone()))
                                    .color(Color::rgb(170, 170, 170))
                                    .font_size(11.0);
                                Label::new(cx, item.map(|e| e.format_label.clone()))
                                    .color(Color::rgb(140, 180, 220))
                                    .font_size(10.0)
                                    .width(Pixels(44.0));
                            })
                            .alignment(Alignment::Center)
                            .gap(Pixels(6.0))
                            .padding(Pixels(4.0))
                        })
                        .on_press(move |ex| {
                            let id = item.get().id.clone();
                            ex.emit(AppEvent::SelectPluginFromDb(id));
                        })
                        .background_color(Color::rgb(55, 55, 60))
                        .width(Stretch(1.0))
                        .height(Pixels(28.0));
                    })
                    .class("plugin-picker-list")
                    .gap(Pixels(2.0));
                })
                .height(Stretch(1.0))
                .background_color(Color::rgb(30, 30, 34));
            })
            .width(Pixels(480.0))
            .height(Pixels(420.0))
            .background_color(Color::rgb(40, 40, 44))
            .padding(Pixels(8.0));
        })
        .position_type(PositionType::Absolute)
        .alignment(Alignment::Center)
        .width(Stretch(1.0))
        .height(Stretch(1.0))
        .background_color(Color::rgba(0, 0, 0, 160))
    }
}

impl View for PluginPickerView {
    fn element(&self) -> Option<&'static str> {
        Some("plugin-picker")
    }
}
