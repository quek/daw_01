//! Renoise-style mixer strip building blocks. The unified
//! `view::tracker_mixer::TrackerMixerView` composes a per-track strip
//! under each tracker column using `strip()`, plus a permanent
//! `master_strip()` pinned on the right.
//!
//! Each strip shows:
//!   - track name
//!   - `M` (mute) / `S` (solo) toggle buttons
//!   - horizontal pan slider
//!   - horizontal volume fader + twin L/R VU meter
//!
//! Master is the final mix bus and always exists. Strips read their state
//! through a per-item lens (`TrackMixEntry`); the master reads
//! `AppData::master_gain` / `peak_*_norm` directly.

use vizia::prelude::*;
use vizia::views::Knob;

use crate::app::{AppData, AppEvent, TrackMixEntry};

/// Width of one mixer strip. Also drives the width of the corresponding
/// tracker column in `view::tracker_mixer` so the two stay aligned in
/// pixel space.
pub(super) const STRIP_WIDTH: f32 = 136.0;
/// Total height of one strip (label + M/S + pan row + fader/meter).
pub(super) const STRIP_HEIGHT: f32 = 220.0;
pub(super) const FADER_HEIGHT: f32 = 100.0;
pub(super) const FADER_WIDTH: f32 = 18.0;
pub(super) const METER_WIDTH: f32 = 5.0;

// dB range shared by fader and meter for visual alignment.
const DB_MIN: f32 = -80.0;
const DB_MAX: f32 = 6.0;
const DB_RANGE: f32 = DB_MAX - DB_MIN; // 86.0

/// Linear amplitude → fader position (0..1).
fn amp_to_fader(amp: f32) -> f32 {
    if amp <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * amp.log10();
    ((db - DB_MIN) / DB_RANGE).clamp(0.0, 1.0)
}

/// Fader position (0..1) → linear amplitude.
fn fader_to_amp(n: f32) -> f32 {
    let db = n * DB_RANGE + DB_MIN;
    if db <= DB_MIN {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// Linear peak amplitude → meter bar height (0..1) on the same dB scale.
fn amp_to_meter_norm(amp: f32) -> f32 {
    if amp <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * amp.log10();
    ((db - DB_MIN) / DB_RANGE).clamp(0.0, 1.0)
}

pub(super) fn strip<L>(cx: &mut Context, entry: L)
where
    L: Lens<Target = TrackMixEntry> + Send + Sync,
{
    VStack::new(cx, |cx| {
        // Track name
        Label::new(cx, entry.map(|e| e.name.clone()))
            .color(Color::rgb(220, 220, 220))
            .font_size(11.0)
            .height(Pixels(14.0));

        // Mute / Solo + Pan knob row
        HStack::new(cx, |cx| {
            let mute_entry = entry;
            Button::new(cx, |cx| Label::new(cx, "M").font_size(10.0))
                .on_press(move |ex| {
                    let idx = mute_entry.get(ex).index;
                    ex.emit(AppEvent::ToggleTrackMute(idx));
                })
                .background_color(entry.map(|e| {
                    if e.muted {
                        Color::rgb(200, 90, 70)
                    } else {
                        Color::rgb(55, 55, 60)
                    }
                }))
                .width(Pixels(22.0))
                .height(Pixels(20.0));

            let solo_entry = entry;
            Button::new(cx, |cx| Label::new(cx, "S").font_size(10.0))
                .on_press(move |ex| {
                    let idx = solo_entry.get(ex).index;
                    ex.emit(AppEvent::ToggleTrackSolo(idx));
                })
                .background_color(entry.map(|e| {
                    if e.solo {
                        Color::rgb(230, 200, 80)
                    } else {
                        Color::rgb(55, 55, 60)
                    }
                }))
                .width(Pixels(22.0))
                .height(Pixels(20.0));

            // Pan knob — centered at 0.0, normalized to 0..1 for the
            // Knob widget. Default 0.5 = center.
            let pan_entry = entry;
            Knob::new(cx, 0.5, entry.map(|e| (e.pan + 1.0) / 2.0), true)
                .on_change(move |ex, normalized| {
                    let pan = normalized * 2.0 - 1.0;
                    let idx = pan_entry.get(ex).index;
                    ex.emit(AppEvent::SetTrackPan {
                        track: idx,
                        bits: pan.to_bits(),
                    });
                })
                .size(Pixels(22.0));
        })
        .gap(Pixels(2.0))
        .height(Pixels(24.0))
        .alignment(Alignment::Center);

        // Vertical fader + stereo VU meter — all on the same dB scale.
        HStack::new(cx, |cx| {
            // Volume fader: vertical (height > width triggers vertical
            // mode in Vizia's Slider). Value is normalized to 0..1 on
            // the dB scale (-80..+6 dB).
            let vol_entry = entry;
            Slider::new(cx, entry.map(|e| amp_to_fader(e.volume)))
                .range(0.0..1.0)
                .on_change(move |ex, fader_pos| {
                    let amp = fader_to_amp(fader_pos);
                    let idx = vol_entry.get(ex).index;
                    ex.emit(AppEvent::SetTrackVolume {
                        track: idx,
                        bits: amp.to_bits(),
                    });
                })
                .width(Pixels(FADER_WIDTH))
                .height(Pixels(FADER_HEIGHT));

            // L/R peak meters on the same dB scale as the fader.
            meter_bar(cx, entry.map(|e| amp_to_meter_norm(e.peak_l_norm)));
            meter_bar(cx, entry.map(|e| amp_to_meter_norm(e.peak_r_norm)));
        })
        .gap(Pixels(2.0))
        .alignment(Alignment::Center)
        .height(Pixels(FADER_HEIGHT));
    })
    .padding(Pixels(4.0))
    .gap(Pixels(2.0));
}

/// Permanent master mixer strip: label + master fader + stereo VU. Unlike
/// the user strips, master has no mute/solo or pan — those concepts
/// don't make sense for the final output bus. Lens targets come from
/// `AppData` directly (no `TrackMixEntry` indirection).
pub(super) fn master_strip(cx: &mut Context) {
    VStack::new(cx, |cx| {
        Label::new(cx, "MASTER")
            .color(Color::rgb(230, 230, 230))
            .font_size(12.0)
            .height(Pixels(16.0));
        // Spacer row where the M/S buttons sit on user strips, so the
        // fader + meter line up vertically across every strip. Width
        // must be explicit — Vizia's Auto width on a content-less
        // Element yields 0 and triggers `matrix.invert().unwrap()` in
        // the draw pass.
        Element::new(cx)
            .width(Stretch(1.0))
            .height(Pixels(22.0));
        // Matching spacer for the pan slider row.
        Element::new(cx)
            .width(Stretch(1.0))
            .height(Pixels(14.0));
        // Master fader (vertical) + stereo meter, same dB scale.
        HStack::new(cx, |cx| {
            Slider::new(cx, AppData::master_gain.map(|g: &f32| amp_to_fader(*g)))
                .range(0.0..1.0)
                .on_change(|ex, fader_pos| {
                    let amp = fader_to_amp(fader_pos);
                    ex.emit(AppEvent::SetMasterGain(amp.to_bits()));
                })
                .width(Pixels(FADER_WIDTH))
                .height(Pixels(FADER_HEIGHT));
            meter_bar(cx, AppData::peak_l_norm.map(|n: &f32| amp_to_meter_norm(*n)));
            meter_bar(cx, AppData::peak_r_norm.map(|n: &f32| amp_to_meter_norm(*n)));
        })
        .gap(Pixels(2.0))
        .alignment(Alignment::Center)
        .height(Pixels(FADER_HEIGHT));
    })
    .padding(Pixels(4.0))
    .gap(Pixels(2.0))
    .background_color(Color::rgb(44, 44, 48));
}

fn meter_bar<L>(cx: &mut Context, norm: L)
where
    L: Lens<Target = f32>,
{
    VStack::new(cx, |cx| {
        Element::new(cx)
            .width(Pixels(METER_WIDTH))
            // Minimum 1 px so Vizia never draws a zero-sized entity.
            .height(norm.map(|n: &f32| {
                Pixels((FADER_HEIGHT * n.clamp(0.0, 1.0)).max(1.0))
            }))
            .background_color(norm.map(|n: &f32| {
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
    .height(Pixels(FADER_HEIGHT))
    .alignment(Alignment::BottomCenter)
    .background_color(Color::rgb(20, 20, 20));
}
