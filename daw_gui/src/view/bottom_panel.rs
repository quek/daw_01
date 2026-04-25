//! Bitwig-style bottom panel: a tab strip with two buttons (Mixer / Piano
//! Roll), and a content area that swaps based on `AppData::bottom_panel`.
//!
//! The mixer uses a fixed MAX_STRIPS slot grid with `Visibility::Hidden`
//! for unused slots rather than a dynamic `List`. Vizia 0.3's morphorm
//! layout produces a non-invertible transform when a list flips between
//! 0 and N entities mid-frame (CLAUDE.md note about `draw.rs:35` panic),
//! and the visibility-hidden pattern sidesteps it by keeping every slot
//! in the layout tree from the first frame on.

use vizia::prelude::*;

use crate::app::{AppData, AppEvent, TrackMixEntry};
use crate::view::lyric_panel::LyricPanel;
use crate::view::mixer_strips::{STRIP_HEIGHT, STRIP_WIDTH, master_strip, strip};
use crate::view::piano_roll_view::PianoRollView;

/// Hard upper bound on user tracks shown in the mixer. Matches the
/// `audio_bridge::MAX_TRACKS` ceiling that the audio thread enforces.
const MAX_STRIPS: usize = 16;
const TAB_HEIGHT: f32 = 26.0;
const PANEL_HEIGHT: f32 = STRIP_HEIGHT + 24.0;
const TAB_BG_INACTIVE: Color = Color::rgb(40, 40, 44);
const TAB_BG_ACTIVE: Color = Color::rgb(60, 80, 110);

pub struct BottomPanelView;

impl BottomPanelView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            VStack::new(cx, |cx| {
                build_tab_strip(cx);
                Binding::new(cx, AppData::bottom_panel, |cx, panel| {
                    let p: u8 = panel.get(cx);
                    match p {
                        1 => build_pianoroll_panel(cx),
                        _ => build_mixer_panel(cx),
                    }
                });
            });
        })
        .height(Pixels(TAB_HEIGHT + PANEL_HEIGHT))
        .background_color(Color::rgb(28, 28, 32))
    }
}

impl View for BottomPanelView {
    fn element(&self) -> Option<&'static str> {
        Some("bottom-panel")
    }
}

fn build_tab_strip(cx: &mut Context) {
    HStack::new(cx, |cx| {
        tab_button(cx, "Mixer", 0);
        tab_button(cx, "Piano Roll", 1);
        Element::new(cx).width(Stretch(1.0));
    })
    .gap(Pixels(0.0))
    .height(Pixels(TAB_HEIGHT))
    .background_color(Color::rgb(32, 32, 36));
}

fn tab_button(cx: &mut Context, label: &'static str, value: u8) {
    Button::new(cx, |cx| Label::new(cx, label).font_size(12.0))
        .on_press(move |ex| ex.emit(AppEvent::SelectBottomPanel(value)))
        .background_color(AppData::bottom_panel.map(move |&p: &u8| {
            if p == value { TAB_BG_ACTIVE } else { TAB_BG_INACTIVE }
        }))
        .width(Pixels(96.0))
        .height(Pixels(TAB_HEIGHT));
}

fn build_mixer_panel(cx: &mut Context) {
    HStack::new(cx, |cx| {
        ScrollView::new(cx, |cx| {
            HStack::new(cx, |cx| {
                for slot in 0..MAX_STRIPS {
                    build_mixer_slot(cx, slot);
                }
            })
            .height(Pixels(STRIP_HEIGHT));
        })
        .width(Stretch(1.0))
        .height(Pixels(STRIP_HEIGHT));

        VStack::new(cx, |cx| {
            master_strip(cx);
        })
        .width(Pixels(STRIP_WIDTH))
        .height(Pixels(STRIP_HEIGHT));
    })
    .height(Pixels(PANEL_HEIGHT));
}

fn build_mixer_slot(cx: &mut Context, slot: usize) {
    let entry_lens = AppData::track_mix.map(move |v: &Vec<TrackMixEntry>| {
        if slot < v.len() {
            v[slot].clone()
        } else {
            TrackMixEntry::default()
        }
    });
    let vis_lens = AppData::track_count.map(move |&n: &u32| {
        if (slot as u32) < n {
            Visibility::Visible
        } else {
            Visibility::Hidden
        }
    });
    VStack::new(cx, |cx| {
        strip(cx, entry_lens);
    })
    .visibility(vis_lens)
    .width(Pixels(STRIP_WIDTH))
    .height(Pixels(STRIP_HEIGHT));
}

fn build_pianoroll_panel(cx: &mut Context) {
    HStack::new(cx, |cx| {
        PianoRollView::new(cx)
            .width(Stretch(1.0))
            .height(Pixels(PANEL_HEIGHT));
        LyricPanel::new(cx)
            .width(Pixels(240.0))
            .height(Pixels(PANEL_HEIGHT));
    })
    .height(Pixels(PANEL_HEIGHT));
}
