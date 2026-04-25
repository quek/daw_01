//! Arrangement view: track headers on the left, timeline canvas on the
//! right. Each clip is a rectangle the user can drag (move) or grab the
//! right edge of (resize). Empty-area double-click creates a new clip.
//!
//! The view is split in two pieces wired into one HStack from `main.rs`:
//!   - `TrackHeadersView`  — list of mute/solo/select rows, fixed width.
//!   - `ArrangementCanvas` — custom Vizia `View` that owns drag state and
//!     draws clip rectangles + the playhead via `vizia::vg`.

use vizia::prelude::*;
use vizia::vg;

use crate::app::{
    ARRANGE_TRACK_HEIGHT, AppData, AppEvent, ClipBox, ClipRef, TrackHeader,
};

const TRACK_HEADER_WIDTH: f32 = 160.0;
const RESIZE_HANDLE_PX: f32 = 6.0;
const RULER_HEIGHT: f32 = 20.0;
/// Maximum song length (in beats) the canvas tries to render. Anything
/// past this just isn't drawn — fine for v1 since songs are short.
const MAX_BEATS: f32 = 256.0;

const COLOR_BG: Color = Color::rgb(28, 28, 32);
const COLOR_LANE_LINE: Color = Color::rgb(48, 48, 56);
const COLOR_BAR_LINE: Color = Color::rgb(60, 60, 70);
const COLOR_PLAYHEAD: Color = Color::rgb(220, 70, 70);
const COLOR_CLIP: Color = Color::rgb(70, 130, 180);
const COLOR_CLIP_SELECTED: Color = Color::rgb(120, 180, 230);
const COLOR_CLIP_BORDER: Color = Color::rgb(20, 20, 28);
const COLOR_CLIP_TEXT: Color = Color::rgb(240, 240, 240);
const COLOR_RULER_BG: Color = Color::rgb(36, 36, 40);

/// Convert vizia's `Color` into the skia color used by Paint. `vizia::vg`
/// is just a re-export of `skia_safe`, so `vg::Color` is `skia_safe::Color`.
fn skia_rgba(c: Color) -> vg::Color {
    vg::Color::from_argb(c.a(), c.r(), c.g(), c.b())
}

fn fill_paint(c: Color) -> vg::Paint {
    let mut p = vg::Paint::default();
    p.set_color(skia_rgba(c));
    p.set_anti_alias(false);
    p
}

fn stroke_paint(c: Color, width: f32) -> vg::Paint {
    let mut p = vg::Paint::default();
    p.set_color(skia_rgba(c));
    p.set_stroke_width(width);
    p.set_style(vg::PaintStyle::Stroke);
    p.set_anti_alias(false);
    p
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> vg::Rect {
    vg::Rect::new(x, y, x + w, y + h)
}

pub struct ArrangementView;

impl ArrangementView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            HStack::new(cx, |cx| {
                build_track_headers(cx);
                ArrangementCanvas::new(cx)
                    .width(Stretch(1.0))
                    .height(Stretch(1.0));
            });
        })
    }
}

impl View for ArrangementView {
    fn element(&self) -> Option<&'static str> {
        Some("arrangement-view")
    }
}

// ---------------------------------------------------------------------------
// Track headers panel
// ---------------------------------------------------------------------------

fn build_track_headers(cx: &mut Context) {
    VStack::new(cx, |cx| {
        Element::new(cx)
            .width(Stretch(1.0))
            .height(Pixels(RULER_HEIGHT))
            .background_color(Color::rgb(36, 36, 40));
        List::new(cx, AppData::track_headers, |cx, _idx, item| {
            track_header_row(cx, item);
        })
        .class("track-headers-list")
        .width(Stretch(1.0));
    })
    .width(Pixels(TRACK_HEADER_WIDTH))
    .background_color(Color::rgb(40, 40, 44));
}

fn track_header_row<L>(cx: &mut Context, item: L)
where
    L: Lens<Target = TrackHeader> + Send + Sync,
{
    HStack::new(cx, |cx| {
        // Either a click-to-select Button (default) or a Textbox (when
        // this row is in rename mode). The branch flips inside a Binding
        // so toggling rename only rebuilds the name cell, not the full
        // header row.
        Binding::new(cx, AppData::track_rename_idx, move |cx, rename_idx| {
            let editing_idx: Option<u32> = rename_idx.get(cx);
            let this_idx: u32 = item.get(cx).index;
            if editing_idx == Some(this_idx) {
                Textbox::new(cx, AppData::track_rename_text)
                    .on_edit(|ex, text| ex.emit(AppEvent::RenameTrackChanged(text)))
                    .on_submit(|ex, _, _| ex.emit(AppEvent::CommitRenameTrack))
                    .on_cancel(|ex| ex.emit(AppEvent::CancelRenameTrack))
                    .width(Stretch(1.0))
                    .height(Pixels(24.0))
                    .focused(true);
            } else {
                let click_item = item;
                let dbl_item = item;
                Button::new(cx, |cx| {
                    Label::new(cx, item.map(|h| h.name.clone())).font_size(11.0)
                })
                .on_press(move |ex| {
                    let h = click_item.get(ex);
                    ex.emit(AppEvent::SelectTrack(h.index));
                })
                .on_double_click(move |ex, _| {
                    let h = dbl_item.get(ex);
                    ex.emit(AppEvent::BeginRenameTrack(h.index));
                })
                .background_color(item.map(|h| {
                    if h.selected {
                        Color::rgb(70, 90, 120)
                    } else {
                        Color::rgb(48, 48, 52)
                    }
                }))
                .width(Stretch(1.0))
                .height(Pixels(24.0));
            }
        });

        let mute_item = item;
        Button::new(cx, |cx| Label::new(cx, "M").font_size(10.0))
            .on_press(move |ex| {
                let h = mute_item.get(ex);
                ex.emit(AppEvent::ToggleTrackMute(h.index));
            })
            .background_color(item.map(|h| {
                if h.muted {
                    Color::rgb(200, 90, 70)
                } else {
                    Color::rgb(55, 55, 60)
                }
            }))
            .width(Pixels(20.0))
            .height(Pixels(20.0));

        let solo_item = item;
        Button::new(cx, |cx| Label::new(cx, "S").font_size(10.0))
            .on_press(move |ex| {
                let h = solo_item.get(ex);
                ex.emit(AppEvent::ToggleTrackSolo(h.index));
            })
            .background_color(item.map(|h| {
                if h.solo {
                    Color::rgb(230, 200, 80)
                } else {
                    Color::rgb(55, 55, 60)
                }
            }))
            .width(Pixels(20.0))
            .height(Pixels(20.0));
    })
    .padding(Pixels(2.0))
    .gap(Pixels(2.0))
    .alignment(Alignment::Center)
    .height(Pixels(ARRANGE_TRACK_HEIGHT));
}

// ---------------------------------------------------------------------------
// Custom canvas
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum DragKind {
    /// Move every entry by the same beat-delta. The snapshot pins the
    /// original start_beat per clip so re-renders mid-drag don't drift
    /// against an already-moved model.
    MoveClips {
        snapshots: Vec<(ClipRef, f64)>,
    },
    ResizeClip {
        clip: ClipRef,
        original_length_beats: f64,
    },
    /// Box-select. `origin_*` is canvas-relative.
    Marquee {
        origin_x: f32,
        origin_y: f32,
        cur_x: f32,
        cur_y: f32,
    },
}

pub struct ArrangementCanvas {
    drag: Option<DragKind>,
    drag_origin_x: f32,
}

impl ArrangementCanvas {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        // Vizia 0.3 doesn't auto-redraw a custom View when the lenses it
        // reads inside `draw` change — it only invalidates on layout /
        // style updates. We bind the lenses we read so a model edit
        // (clip add / move, playhead tick, etc.) marks the view dirty.
        Self {
            drag: None,
            drag_origin_x: 0.0,
        }
        .build(cx, |_cx| {})
        .bind(AppData::clip_boxes, |mut handle, _| handle.needs_redraw())
        .bind(AppData::track_count, |mut handle, _| handle.needs_redraw())
        .bind(AppData::arrange_zoom_x, |mut handle, _| handle.needs_redraw())
        .bind(AppData::arrange_scroll_beat, |mut handle, _| handle.needs_redraw())
        .bind(AppData::playhead_beat, |mut handle, _| handle.needs_redraw())
    }

    /// Convert a canvas-relative `(x, y)` into the logical `(beat, track)`
    /// the timeline rules see. Includes the active scroll offset so a
    /// scrolled view hits the right clip.
    fn coord_to_beat_track(
        mx: f32,
        my: f32,
        zoom: f32,
        scroll_beat: f32,
    ) -> Option<(f64, u32)> {
        if my < RULER_HEIGHT {
            return None;
        }
        let beat = mx / zoom + scroll_beat;
        let track = ((my - RULER_HEIGHT) / ARRANGE_TRACK_HEIGHT) as u32;
        Some((beat as f64, track))
    }

    fn hit_clip(clip_boxes: &[ClipBox], beat: f64, track: u32) -> Option<&ClipBox> {
        clip_boxes.iter().find(|c| {
            c.track == track
                && (beat as f32) >= c.start_beat
                && (beat as f32) <= c.start_beat + c.length_beats
        })
    }
}

impl View for ArrangementCanvas {
    fn element(&self) -> Option<&'static str> {
        Some("arrangement-canvas")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let bounds = cx.bounds();
        let zoom: f32 = AppData::arrange_zoom_x.get(cx);
        let scroll_beat: f32 = AppData::arrange_scroll_beat.get(cx);
        let track_count: u32 = AppData::track_count.get(cx);
        let clip_boxes: Vec<ClipBox> = AppData::clip_boxes.get(cx);
        let playhead: Option<f32> = AppData::playhead_beat.get(cx);

        // Background.
        canvas.draw_rect(
            rect(bounds.x, bounds.y, bounds.w, bounds.h),
            &fill_paint(COLOR_BG),
        );

        // Ruler strip at the top.
        canvas.draw_rect(
            rect(bounds.x, bounds.y, bounds.w, RULER_HEIGHT),
            &fill_paint(COLOR_RULER_BG),
        );

        // Vertical bar lines every 4 beats. Start from the first bar that
        // is at or before the visible left edge so the grid stays aligned
        // even when scrolled.
        let bar_paint = stroke_paint(COLOR_BAR_LINE, 1.0);
        let beats_per_bar = 4u32;
        let first_bar = ((scroll_beat as i32) / beats_per_bar as i32).max(0);
        let visible_beats = bounds.w / zoom;
        let last_bar = ((scroll_beat + visible_beats) as u32 / beats_per_bar) + 1;
        for bar in first_bar..=(last_bar as i32) {
            let beat = (bar * beats_per_bar as i32) as f32;
            let x = bounds.x + (beat - scroll_beat) * zoom;
            if x < bounds.x - 1.0 {
                continue;
            }
            if x > bounds.x + bounds.w {
                break;
            }
            let mut p = vg::Path::new();
            p.move_to((x, bounds.y));
            p.line_to((x, bounds.y + bounds.h));
            canvas.draw_path(&p, &bar_paint);
        }

        // Horizontal track lane separators.
        let lane_paint = stroke_paint(COLOR_LANE_LINE, 1.0);
        for i in 0..=track_count {
            let y = bounds.y + RULER_HEIGHT + (i as f32) * ARRANGE_TRACK_HEIGHT;
            if y > bounds.y + bounds.h {
                break;
            }
            let mut p = vg::Path::new();
            p.move_to((bounds.x, y));
            p.line_to((bounds.x + bounds.w, y));
            canvas.draw_path(&p, &lane_paint);
        }

        // Clip rectangles.
        let border_paint = stroke_paint(COLOR_CLIP_BORDER, 1.0);
        let mut text_paint = vg::Paint::default();
        text_paint.set_color(skia_rgba(COLOR_CLIP_TEXT));
        text_paint.set_anti_alias(true);
        let font = vg::Font::default();
        for c in &clip_boxes {
            let x = bounds.x + (c.start_beat - scroll_beat) * zoom;
            let y = bounds.y + RULER_HEIGHT + (c.track as f32) * ARRANGE_TRACK_HEIGHT;
            let w = (c.length_beats * zoom).max(2.0);
            let h = ARRANGE_TRACK_HEIGHT - 4.0;
            if x + w < bounds.x || x > bounds.x + bounds.w {
                continue;
            }
            let body_color = if c.selected {
                COLOR_CLIP_SELECTED
            } else {
                COLOR_CLIP
            };
            let r = rect(x, y + 2.0, w, h);
            canvas.draw_rect(r, &fill_paint(body_color));
            canvas.draw_rect(r, &border_paint);
            if w > 28.0 {
                canvas.draw_str(
                    c.name.as_str(),
                    (x + 4.0, y + h * 0.5 + 6.0),
                    &font,
                    &text_paint,
                );
            }
        }

        // Playhead vertical line.
        if let Some(beat) = playhead {
            let x = bounds.x + (beat - scroll_beat) * zoom;
            if x >= bounds.x && x <= bounds.x + bounds.w {
                let mut path = vg::Path::new();
                path.move_to((x, bounds.y));
                path.line_to((x, bounds.y + bounds.h));
                let mut p = stroke_paint(COLOR_PLAYHEAD, 1.5);
                p.set_anti_alias(true);
                canvas.draw_path(&path, &p);
            }
        }
        let _ = MAX_BEATS; // kept as a doc-tying constant; not used at draw time
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                let bounds = cx.bounds();
                let mx = cx.mouse().cursor_x - bounds.x;
                let my = cx.mouse().cursor_y - bounds.y;
                let zoom = AppData::arrange_zoom_x.get(cx);
                let scroll_beat = AppData::arrange_scroll_beat.get(cx);
                let clip_boxes: Vec<ClipBox> = AppData::clip_boxes.get(cx);
                let shift = cx.modifiers().shift();
                let Some((beat, track)) =
                    Self::coord_to_beat_track(mx, my, zoom, scroll_beat)
                else {
                    return;
                };
                if let Some(c) = Self::hit_clip(&clip_boxes, beat, track) {
                    let target = ClipRef {
                        track: c.track,
                        clip: c.clip,
                    };
                    cx.emit(AppEvent::SelectClip {
                        target,
                        additive: shift,
                    });
                    // Shift+click toggles selection — no drag.
                    if shift {
                        meta.consume();
                        return;
                    }
                    let right_edge_px =
                        (c.start_beat + c.length_beats - scroll_beat) * zoom;
                    let from_right = right_edge_px - mx;
                    self.drag_origin_x = mx;
                    self.drag = if from_right < RESIZE_HANDLE_PX {
                        Some(DragKind::ResizeClip {
                            clip: target,
                            original_length_beats: c.length_beats as f64,
                        })
                    } else {
                        // Snapshot every selected clip's start_beat so the
                        // group slides together. If the clicked clip
                        // wasn't in the prior selection, SelectClip above
                        // replaced selection with `[target]` already, so
                        // reading the cached clip_boxes' selected flag
                        // would lag — explicitly include `target` plus the
                        // others that were already selected.
                        let mut snapshots: Vec<(ClipRef, f64)> = clip_boxes
                            .iter()
                            .filter(|c| c.selected)
                            .map(|c| {
                                (
                                    ClipRef { track: c.track, clip: c.clip },
                                    c.start_beat as f64,
                                )
                            })
                            .collect();
                        if !snapshots.iter().any(|(r, _)| *r == target) {
                            snapshots.push((target, c.start_beat as f64));
                        }
                        Some(DragKind::MoveClips { snapshots })
                    };
                    cx.capture();
                    meta.consume();
                } else if my >= RULER_HEIGHT {
                    // Empty lane — start a marquee unless the click is in
                    // the ruler. If not Shift, the in-progress selection
                    // stays cleared on MouseUp without box dragging too.
                    if !shift {
                        cx.emit(AppEvent::ClearSelection);
                    }
                    self.drag = Some(DragKind::Marquee {
                        origin_x: mx,
                        origin_y: my,
                        cur_x: mx,
                        cur_y: my,
                    });
                    cx.capture();
                    meta.consume();
                }
            }
            WindowEvent::MouseMove(_, _) => {
                let Some(kind) = self.drag.clone() else { return };
                let bounds = cx.bounds();
                let mx = cx.mouse().cursor_x - bounds.x;
                let my = cx.mouse().cursor_y - bounds.y;
                let zoom = AppData::arrange_zoom_x.get(cx);
                let dx = mx - self.drag_origin_x;
                let dbeat = (dx / zoom) as f64;
                match kind {
                    DragKind::MoveClips { snapshots } => {
                        let entries: Vec<(ClipRef, u64)> = snapshots
                            .iter()
                            .map(|(r, s)| (*r, (s + dbeat).max(0.0).to_bits()))
                            .collect();
                        cx.emit(AppEvent::SetClipPositions(entries));
                    }
                    DragKind::ResizeClip {
                        clip,
                        original_length_beats,
                    } => {
                        let new_len = (original_length_beats + dbeat).max(0.25);
                        cx.emit(AppEvent::ResizeClip {
                            target: clip,
                            length_bits: new_len.to_bits(),
                        });
                    }
                    DragKind::Marquee { origin_x, origin_y, .. } => {
                        // Track latest cursor for redraw + on-MouseUp commit.
                        self.drag = Some(DragKind::Marquee {
                            origin_x,
                            origin_y,
                            cur_x: mx,
                            cur_y: my,
                        });
                    }
                }
            }
            WindowEvent::MouseUp(MouseButton::Left) if self.drag.is_some() => {
                if let Some(DragKind::Marquee {
                    origin_x,
                    origin_y,
                    cur_x,
                    cur_y,
                }) = self.drag.clone()
                {
                    let zoom = AppData::arrange_zoom_x.get(cx);
                    let scroll_beat = AppData::arrange_scroll_beat.get(cx);
                    let clip_boxes: Vec<ClipBox> = AppData::clip_boxes.get(cx);
                    let x0 = origin_x.min(cur_x);
                    let x1 = origin_x.max(cur_x);
                    let y0 = origin_y.min(cur_y);
                    let y1 = origin_y.max(cur_y);
                    let beat0 = (x0 / zoom + scroll_beat).max(0.0);
                    let beat1 = (x1 / zoom + scroll_beat).max(0.0);
                    let track0 =
                        ((y0 - RULER_HEIGHT).max(0.0) / ARRANGE_TRACK_HEIGHT) as u32;
                    let track1 =
                        ((y1 - RULER_HEIGHT).max(0.0) / ARRANGE_TRACK_HEIGHT) as u32;
                    let hits: Vec<ClipRef> = clip_boxes
                        .iter()
                        .filter(|c| {
                            c.track >= track0
                                && c.track <= track1
                                && c.start_beat <= beat1
                                && c.start_beat + c.length_beats >= beat0
                        })
                        .map(|c| ClipRef { track: c.track, clip: c.clip })
                        .collect();
                    cx.emit(AppEvent::SetClipSelection(hits));
                }
                self.drag = None;
                cx.release();
            }
            WindowEvent::MouseDoubleClick(MouseButton::Left) => {
                let bounds = cx.bounds();
                let mx = cx.mouse().cursor_x - bounds.x;
                let my = cx.mouse().cursor_y - bounds.y;
                let zoom = AppData::arrange_zoom_x.get(cx);
                let scroll_beat = AppData::arrange_scroll_beat.get(cx);
                let clip_boxes: Vec<ClipBox> = AppData::clip_boxes.get(cx);
                let track_count = AppData::track_count.get(cx);
                let Some((beat, track)) =
                    Self::coord_to_beat_track(mx, my, zoom, scroll_beat)
                else {
                    return;
                };
                if track >= track_count {
                    return;
                }
                // Bitwig: double-click on a clip opens the piano roll for
                // it. Double-click on empty lane creates a new clip.
                if let Some(c) = Self::hit_clip(&clip_boxes, beat, track) {
                    cx.emit(AppEvent::SelectClip {
                        target: ClipRef {
                            track: c.track,
                            clip: c.clip,
                        },
                        additive: false,
                    });
                    cx.emit(AppEvent::SelectBottomPanel(1));
                    meta.consume();
                    return;
                }
                // Snap to nearest beat.
                let snapped = beat.floor().max(0.0);
                cx.emit(AppEvent::CreateClip {
                    track,
                    start_beat_bits: snapped.to_bits(),
                });
                cx.emit(AppEvent::SelectBottomPanel(1));
                meta.consume();
            }
            WindowEvent::MouseScroll(_, dy) => {
                let mods = cx.modifiers();
                if mods.ctrl() {
                    // Zoom on Ctrl+wheel — exponential so each notch is a
                    // perceptually equal step.
                    let zoom = AppData::arrange_zoom_x.get(cx);
                    let factor = 1.15_f32.powf(*dy);
                    let new_zoom = (zoom * factor).clamp(2.0, 400.0);
                    cx.emit(AppEvent::SetArrangeZoom(new_zoom.to_bits()));
                } else {
                    // Scroll on plain wheel — sign matches Bitwig: wheel-up
                    // scrolls left (towards earlier beats).
                    let scroll = AppData::arrange_scroll_beat.get(cx);
                    let speed = 1.0_f32; // beats per notch
                    let new_scroll = (scroll - dy * speed).max(0.0);
                    cx.emit(AppEvent::SetArrangeScroll(new_scroll.to_bits()));
                }
                meta.consume();
            }
            _ => {}
        });
    }
}
