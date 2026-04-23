//! Modal plugin picker overlay. Displays every entry from the plugin
//! database as a clickable row; selecting one emits
//! `AppEvent::SelectPluginFromDb` which swaps in the corresponding plugin.
//!
//! Rendered as a self-directed overlay on top of the main window so it can
//! sit above the Arrangement / Inspector panels without disturbing their
//! layout.

use vizia::prelude::*;

use crate::app::{AppData, AppEvent};

pub struct PluginPickerView;

impl PluginPickerView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            VStack::new(cx, |cx| {
                // Title row
                HStack::new(cx, |cx| {
                    Label::new(cx, "Select Plugin")
                        .font_size(16.0)
                        .color(Color::rgb(230, 230, 230));
                    // Elastic spacer
                    Element::new(cx).width(Stretch(1.0));
                    Button::new(cx, |cx| Label::new(cx, "✕"))
                        .on_press(|ex| ex.emit(AppEvent::ClosePluginPicker));
                })
                .alignment(Alignment::Center)
                .padding(Pixels(8.0))
                .height(Pixels(40.0));

                ScrollView::new(cx, |cx| {
                    List::new(cx, AppData::plugin_picker_entries, |cx, _idx, item| {
                        // Each row: button labelled "Name — Vendor". We set
                        // the button background explicitly so the Vizia
                        // default light theme doesn't wash out the labels.
                        Button::new(cx, |cx| {
                            HStack::new(cx, |cx| {
                                Label::new(cx, item.map(|e| e.name.clone()))
                                    .color(Color::rgb(230, 230, 230));
                                Element::new(cx).width(Stretch(1.0));
                                Label::new(cx, item.map(|e| e.vendor.clone()))
                                    .color(Color::rgb(170, 170, 170))
                                    .font_size(11.0);
                            })
                            .alignment(Alignment::Center)
                            .padding(Pixels(4.0))
                        })
                        .on_press(move |ex| {
                            // Recover the id at press-time via a lens read:
                            let id = item.get(ex).id.clone();
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
        // Self-directed positioning puts us absolute on top of the parent
        // (the Application root in main.rs).
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
