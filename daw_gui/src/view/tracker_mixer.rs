//! Unified tracker + mixer view. Each user track is a single VStack
//! containing its tracker rows on top and its mixer strip on the bottom,
//! so the two halves stay aligned in pixel space without manual
//! width-tweaking. Sticky header (track names + column labels) sits above
//! the per-track columns so it doesn't scroll with the content.
//!
//! Layout:
//!
//! ```text
//! ┌───────┬───────────┬───────────┬─────┬───────┐
//! │  ##   │ Track 1   │ Track 2   │ ··· │ MASTER│  <- sticky header HStack
//! │       │ NOT … LYR │ NOT … LYR │ ··· │       │
//! ├───────┼───────────┼───────────┼─────┼───────┤
//! │ 00    │  rows...  │  rows...  │     │       │
//! │ 01    │           │           │     │       │
//! │ ··    │           │           │     │       │
//! │ FF    │           │           │     │       │
//! │       ├───────────┼───────────┼─────┤       │  <- strip baseline
//! │       │ M S Pan   │ M S Pan   │ ··· │  fader│
//! │       │ ┃▓▓▓ L R  │ ┃▓▓▓ L R  │     │ + L/R │
//! └───────┴───────────┴───────────┴─────┴───────┘
//!  ROW_NUM   STRIP_W     STRIP_W   ...   STRIP_W
//! ```

use vizia::prelude::*;

use crate::app::{AppData, TrackMixEntry};
use crate::view::mixer_strips::{master_strip, strip, STRIP_HEIGHT, STRIP_WIDTH};

/// Maximum number of user-track columns. Slots beyond
/// `visible_track_count` are hidden (`Visibility::Hidden`) but kept in the
/// layout tree so dynamic show/hide doesn't churn entities — that's the
/// pattern that keeps Vizia's draw path off its `matrix.invert()` panic.
pub(super) const MAX_STRIPS: usize = 16;
/// Width of the leftmost row-number column. Sized to fit `> NN ` at
/// HackGen Console NF 13 px with a small right gutter.
const ROW_NUM_SPACER: f32 = 36.0;
/// Height of one row in the tracker grid (label height + tight padding).
const TRACKER_ROW_HEIGHT: f32 = 17.0;
/// Height of one line in the sticky header (track-name line and
/// column-label line are both this tall, so the header is `2 *
/// HEADER_LINE_HEIGHT` total).
const HEADER_LINE_HEIGHT: f32 = 14.0;
const TRACKER_FONT: &str = "HackGen Console NF";
const TRACKER_FONT_SIZE: f32 = 13.0;
/// Column-label line shown under each track name. Same width as the
/// per-cell payload (`CELL_HEADER` from `arrangement.rs`).
const COL_LABELS: &str = " NOT VOL FX  LYR ";

pub struct TrackerMixerView;

impl TrackerMixerView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            VStack::new(cx, |cx| {
                build_header_row(cx);
                build_body_row(cx);
            })
            .background_color(Color::rgb(28, 28, 32));
        })
    }
}

impl View for TrackerMixerView {
    fn element(&self) -> Option<&'static str> {
        Some("tracker-mixer-view")
    }
}

/// Sticky header row: row-number gutter title, per-track names + column
/// labels, and the master title pinned on the right.
fn build_header_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        // Row-number gutter title. Two stacked labels so it visually
        // occupies the same height as the per-track header VStack
        // (`HEADER_LINE_HEIGHT * 2`), keeping the body's row 0 aligned
        // across all columns.
        VStack::new(cx, |cx| {
            Label::new(cx, "##")
                .font_family(vec![FamilyOwned::Named(TRACKER_FONT.into())])
                .font_size(TRACKER_FONT_SIZE)
                .color(Color::rgb(180, 180, 180))
                .height(Pixels(HEADER_LINE_HEIGHT));
            Element::new(cx)
                .width(Stretch(1.0))
                .height(Pixels(HEADER_LINE_HEIGHT));
        })
        .width(Pixels(ROW_NUM_SPACER))
        .height(Pixels(HEADER_LINE_HEIGHT * 2.0));

        for slot in 0..MAX_STRIPS {
            // Plain indexing instead of `.get(slot)` to dodge Vizia's
            // blanket `Res::get(&impl DataContext)` resolving over the
            // `Vec`'s inherent `get(usize)` (the trait shadows the
            // method when both are in scope via `vizia::prelude::*`).
            let header_lens = AppData::visible_track_headers.map(move |v: &Vec<String>| {
                if slot < v.len() {
                    v[slot].clone()
                } else {
                    String::new()
                }
            });
            let vis_lens = visibility_lens(slot);
            VStack::new(cx, |cx| {
                Label::new(cx, header_lens)
                    .font_family(vec![FamilyOwned::Named(TRACKER_FONT.into())])
                    .font_size(TRACKER_FONT_SIZE)
                    .color(Color::rgb(220, 220, 220))
                    .height(Pixels(HEADER_LINE_HEIGHT));
                Label::new(cx, COL_LABELS)
                    .font_family(vec![FamilyOwned::Named(TRACKER_FONT.into())])
                    .font_size(TRACKER_FONT_SIZE)
                    .color(Color::rgb(180, 180, 180))
                    .height(Pixels(HEADER_LINE_HEIGHT));
            })
            .visibility(vis_lens)
            .width(Pixels(STRIP_WIDTH))
            .height(Pixels(HEADER_LINE_HEIGHT * 2.0));
        }

        // Master column header. Mirrors the per-track header height so
        // the master strip's body baseline aligns with the user strips.
        Element::new(cx)
            .width(Pixels(1.0))
            .height(Pixels(HEADER_LINE_HEIGHT * 2.0))
            .background_color(Color::rgb(60, 60, 66));
        VStack::new(cx, |cx| {
            Label::new(cx, "MASTER")
                .color(Color::rgb(230, 230, 230))
                .font_size(12.0)
                .height(Pixels(HEADER_LINE_HEIGHT));
            Element::new(cx)
                .width(Stretch(1.0))
                .height(Pixels(HEADER_LINE_HEIGHT));
        })
        .width(Pixels(STRIP_WIDTH))
        .height(Pixels(HEADER_LINE_HEIGHT * 2.0));
    })
    .height(Pixels(HEADER_LINE_HEIGHT * 2.0))
    .background_color(Color::rgb(36, 36, 40));
}

/// Body row: row-number column on the left, per-track VStacks
/// (tracker rows + strip), and the master column on the right.
fn build_body_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        build_row_number_column(cx);

        for slot in 0..MAX_STRIPS {
            let cells_lens = AppData::visible_tracker_cells.map(move |v: &Vec<Vec<String>>| {
                if slot < v.len() { v[slot].clone() } else { Vec::new() }
            });
            let mix_lens = AppData::visible_track_mix.map(move |v: &Vec<TrackMixEntry>| {
                if slot < v.len() {
                    v[slot].clone()
                } else {
                    TrackMixEntry::default()
                }
            });
            let vis_lens = visibility_lens(slot);
            VStack::new(cx, |cx| {
                build_track_rows(cx, cells_lens);
                VStack::new(cx, |cx| {
                    strip(cx, mix_lens);
                })
                .height(Pixels(STRIP_HEIGHT));
            })
            .visibility(vis_lens)
            .width(Pixels(STRIP_WIDTH));
        }

        // Divider + master column.
        Element::new(cx)
            .width(Pixels(1.0))
            .height(Stretch(1.0))
            .background_color(Color::rgb(60, 60, 66));
        VStack::new(cx, |cx| {
            // Spacer above the master strip so its fader/meter line up
            // with the user strips' fader/meter (i.e. the master strip
            // sits flush at the bottom).
            Element::new(cx)
                .width(Stretch(1.0))
                .height(Stretch(1.0));
            VStack::new(cx, |cx| {
                master_strip(cx);
            })
            .height(Pixels(STRIP_HEIGHT));
        })
        .width(Pixels(STRIP_WIDTH))
        .background_color(Color::rgb(44, 44, 48));
    })
    .height(Stretch(1.0));
}

fn build_row_number_column(cx: &mut Context) {
    VStack::new(cx, |cx| {
        List::new(cx, AppData::row_numbers, |cx, row_idx, item| {
            Label::new(cx, item)
                .font_family(vec![FamilyOwned::Named(TRACKER_FONT.into())])
                .font_size(TRACKER_FONT_SIZE)
                .color(Color::rgb(180, 180, 180))
                .height(Pixels(TRACKER_ROW_HEIGHT))
                .background_color(AppData::playhead_row.map(move |p: &Option<u32>| {
                    if *p == Some(row_idx as u32) {
                        Color::rgb(40, 80, 50)
                    } else {
                        Color::rgba(0, 0, 0, 0)
                    }
                }));
        })
        .class("tracker-list")
        .gap(Pixels(0.0))
        .height(Stretch(1.0));

        // Spacer beneath the row-number list so the bottom of this
        // column aligns with the bottom of every track's mixer strip.
        Element::new(cx)
            .width(Stretch(1.0))
            .height(Pixels(STRIP_HEIGHT));
    })
    .width(Pixels(ROW_NUM_SPACER));
}

/// Per-track tracker grid rows. Cursor cell already comes wrapped in
/// `[…]` from `render_visible_tracker_cells`; this routine only adds the
/// playhead-row background coloring on top.
fn build_track_rows<L>(cx: &mut Context, cells: L)
where
    L: Lens<Target = Vec<String>>,
{
    List::new(cx, cells, |cx, row_idx, item| {
        Label::new(cx, item)
            .font_family(vec![FamilyOwned::Named(TRACKER_FONT.into())])
            .font_size(TRACKER_FONT_SIZE)
            .color(Color::rgb(220, 220, 220))
            .height(Pixels(TRACKER_ROW_HEIGHT))
            .background_color(AppData::playhead_row.map(move |p: &Option<u32>| {
                if *p == Some(row_idx as u32) {
                    Color::rgb(40, 80, 50)
                } else {
                    Color::rgba(0, 0, 0, 0)
                }
            }));
    })
    .class("tracker-list")
    .gap(Pixels(0.0))
    .height(Stretch(1.0));
}

/// Maps `visible_track_count` → `Visibility` for a fixed slot index. We
/// avoid `Display::None` because Vizia panics in `draw.rs` when a
/// previously zero-sized entity gets a non-zero size on the same frame.
fn visibility_lens(slot: usize) -> impl Lens<Target = Visibility> {
    AppData::visible_track_count.map(move |&n: &u32| {
        if (slot as u32) < n {
            Visibility::Visible
        } else {
            Visibility::Hidden
        }
    })
}
