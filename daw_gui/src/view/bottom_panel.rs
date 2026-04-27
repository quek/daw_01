//! Bitwig-style bottom panel: a tab strip with two buttons (Mixer / Piano
//! Roll), and a content area that swaps based on `bottom_panel`.
//!
//! The mixer uses a fixed MAX_STRIPS slot grid with `Visibility::Hidden`
//! for unused slots rather than a dynamic `List`. Vizia 0.3's morphorm
//! layout produces a non-invertible transform when a list flips between
//! 0 and N entities mid-frame; the visibility-hidden pattern sidesteps it.

use vizia::prelude::*;

use crate::app::{AppEvent, TrackMixEntry};
use crate::view::lyric_panel::{LyricPanel, LyricPanelSignals};
use crate::view::mixer_strips::{
    MasterStripSignals, STRIP_HEIGHT, STRIP_WIDTH, master_strip, strip,
};
use crate::view::piano_roll_view::{PianoRollSignals, PianoRollView};

/// Hard upper bound on user tracks shown in the mixer.
const MAX_STRIPS: usize = 16;
const TAB_HEIGHT: f32 = 26.0;
const PANEL_HEIGHT: f32 = STRIP_HEIGHT + 24.0;
const TAB_BG_INACTIVE: Color = Color::rgb(40, 40, 44);
const TAB_BG_ACTIVE: Color = Color::rgb(60, 80, 110);

#[derive(Copy, Clone)]
pub struct BottomPanelSignals {
    pub bottom_panel: Signal<u8>,
    pub track_mix: Signal<Vec<TrackMixEntry>>,
    pub track_count: Signal<u32>,
    pub master: MasterStripSignals,
    pub piano_roll: PianoRollSignals,
    pub lyric: LyricPanelSignals,
}

pub struct BottomPanelView;

impl BottomPanelView {
    pub fn new(cx: &mut Context, sig: BottomPanelSignals) -> Handle<'_, Self> {
        Self.build(cx, move |cx| {
            VStack::new(cx, move |cx| {
                build_tab_strip(cx, sig.bottom_panel);
                Binding::new(cx, sig.bottom_panel, move |cx| {
                    let p: u8 = sig.bottom_panel.get();
                    match p {
                        1 => build_pianoroll_panel(cx, sig.piano_roll, sig.lyric),
                        _ => build_mixer_panel(cx, sig.track_mix, sig.track_count, sig.master),
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

fn build_tab_strip(cx: &mut Context, bottom_panel: Signal<u8>) {
    HStack::new(cx, move |cx| {
        tab_button(cx, "Mixer", 0, bottom_panel);
        tab_button(cx, "Piano Roll", 1, bottom_panel);
        Element::new(cx).width(Stretch(1.0));
    })
    .gap(Pixels(0.0))
    .height(Pixels(TAB_HEIGHT))
    .background_color(Color::rgb(32, 32, 36));
}

fn tab_button(cx: &mut Context, label: &'static str, value: u8, bottom_panel: Signal<u8>) {
    Button::new(cx, |cx| Label::new(cx, label).font_size(12.0))
        .on_press(move |ex| ex.emit(AppEvent::SelectBottomPanel(value)))
        .background_color(bottom_panel.map(move |&p: &u8| {
            if p == value { TAB_BG_ACTIVE } else { TAB_BG_INACTIVE }
        }))
        .width(Pixels(96.0))
        .height(Pixels(TAB_HEIGHT));
}

fn build_mixer_panel(
    cx: &mut Context,
    track_mix: Signal<Vec<TrackMixEntry>>,
    track_count: Signal<u32>,
    master: MasterStripSignals,
) {
    HStack::new(cx, move |cx| {
        ScrollView::new(cx, move |cx| {
            HStack::new(cx, move |cx| {
                for slot in 0..MAX_STRIPS {
                    build_mixer_slot(cx, slot, track_mix, track_count);
                }
            })
            .height(Pixels(STRIP_HEIGHT));
        })
        .width(Stretch(1.0))
        .height(Pixels(STRIP_HEIGHT));

        VStack::new(cx, move |cx| {
            master_strip(cx, master);
        })
        .width(Pixels(STRIP_WIDTH))
        .height(Pixels(STRIP_HEIGHT));
    })
    .height(Pixels(PANEL_HEIGHT));
}

fn build_mixer_slot(
    cx: &mut Context,
    slot: usize,
    track_mix: Signal<Vec<TrackMixEntry>>,
    track_count: Signal<u32>,
) {
    let entry_memo = track_mix.map(move |v: &Vec<TrackMixEntry>| {
        if slot < v.len() {
            v[slot].clone()
        } else {
            TrackMixEntry::default()
        }
    });
    let vis_memo = track_count.map(move |&n: &u32| {
        if (slot as u32) < n {
            Visibility::Visible
        } else {
            Visibility::Hidden
        }
    });
    VStack::new(cx, move |cx| {
        strip(cx, entry_memo);
    })
    .visibility(vis_memo)
    .width(Pixels(STRIP_WIDTH))
    .height(Pixels(STRIP_HEIGHT));
}

fn build_pianoroll_panel(
    cx: &mut Context,
    piano_roll: PianoRollSignals,
    lyric: LyricPanelSignals,
) {
    HStack::new(cx, move |cx| {
        PianoRollView::new(cx, piano_roll)
            .width(Stretch(1.0))
            .height(Pixels(PANEL_HEIGHT));
        LyricPanel::new(cx, lyric)
            .width(Pixels(240.0))
            .height(Pixels(PANEL_HEIGHT));
    })
    .height(Pixels(PANEL_HEIGHT));
}
