//! Arrangement view: track headers on the left, timeline canvas on the
//! right. Each clip is a rectangle the user can drag (move) or grab the
//! right edge of (resize). Empty-area double-click creates a new clip.

use std::cell::RefCell;

use vizia::prelude::*;
use vizia::vg;

use crate::app::{ARRANGE_TRACK_HEIGHT, AppEvent, ClipBox, ClipRef, TrackHeader};

const TRACK_HEADER_WIDTH: f32 = 160.0;
const RESIZE_HANDLE_PX: f32 = 6.0;
const RULER_HEIGHT: f32 = 20.0;
/// Maximum song length (in beats) the canvas tries to render.
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

#[derive(Copy, Clone)]
pub struct ArrangementSignals {
    pub track_headers: Memo<Vec<TrackHeader>>,
    pub track_rename_idx: Signal<Option<u32>>,
    pub track_rename_text: Signal<String>,
    pub clip_boxes: Memo<Vec<ClipBox>>,
    pub track_count: Memo<u32>,
    pub arrange_zoom_x: Signal<f32>,
    pub arrange_scroll_beat: Signal<f32>,
    pub playhead_beat: Signal<Option<f32>>,
    pub loop_start_beat: Memo<f32>,
    pub loop_end_beat: Memo<f32>,
}

pub struct ArrangementView;

impl ArrangementView {
    pub fn new(cx: &mut Context, sig: ArrangementSignals) -> Handle<'_, Self> {
        Self.build(cx, move |cx| {
            HStack::new(cx, move |cx| {
                build_track_headers(cx, sig);
                ArrangementCanvas::new(cx, sig)
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

fn build_track_headers(cx: &mut Context, sig: ArrangementSignals) {
    VStack::new(cx, move |cx| {
        Element::new(cx)
            .width(Stretch(1.0))
            .height(Pixels(RULER_HEIGHT))
            .background_color(Color::rgb(36, 36, 40));
        List::new(cx, sig.track_headers, move |cx, _idx, item| {
            track_header_row(cx, item, sig);
        })
        .class("track-headers-list")
        .width(Stretch(1.0));
    })
    .width(Pixels(TRACK_HEADER_WIDTH))
    .background_color(Color::rgb(40, 40, 44));
}

fn track_header_row(cx: &mut Context, item: Signal<TrackHeader>, sig: ArrangementSignals) {
    VStack::new(cx, move |cx| {
        track_header_top_row(cx, item, sig);
        track_header_bottom_row(cx, item);
    })
    .padding(Pixels(2.0))
    .gap(Pixels(2.0))
    .height(Pixels(ARRANGE_TRACK_HEIGHT));
}

fn track_header_top_row(cx: &mut Context, item: Signal<TrackHeader>, sig: ArrangementSignals) {
    HStack::new(cx, move |cx| {
        // Either a click-to-select Button (default) or a Textbox (when
        // this row is in rename mode). The branch flips inside a Binding
        // so toggling rename only rebuilds the name cell.
        Binding::new(cx, sig.track_rename_idx, move |cx| {
            let editing_idx = sig.track_rename_idx.get();
            let this_idx: u32 = item.get().index;
            if editing_idx == Some(this_idx) {
                Textbox::new(cx, sig.track_rename_text)
                    .on_edit(|ex, text| ex.emit(AppEvent::RenameTrackChanged(text)))
                    .on_submit(|ex, _, _| ex.emit(AppEvent::CommitRenameTrack))
                    .on_cancel(|ex| ex.emit(AppEvent::CancelRenameTrack))
                    .width(Stretch(1.0))
                    .height(Pixels(24.0))
                    .focused(true);
            } else {
                Button::new(cx, move |cx| {
                    Label::new(cx, item.map(|h| h.name.clone())).font_size(11.0)
                })
                .on_press(move |ex| {
                    let h = item.get();
                    ex.emit(AppEvent::SelectTrack(h.index));
                })
                .on_double_click(move |ex, _| {
                    let h = item.get();
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

        Button::new(cx, |cx| Label::new(cx, "M").font_size(10.0))
            .on_press(move |ex| {
                let h = item.get();
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

        Button::new(cx, |cx| Label::new(cx, "S").font_size(10.0))
            .on_press(move |ex| {
                let h = item.get();
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
    .gap(Pixels(2.0))
    .alignment(Alignment::Center)
    .height(Pixels(24.0));
}

fn track_header_bottom_row(cx: &mut Context, item: Signal<TrackHeader>) {
    HStack::new(cx, move |cx| {
        Button::new(cx, |cx| Label::new(cx, "▲").font_size(10.0))
            .on_press(move |ex| {
                let h = item.get();
                ex.emit(AppEvent::MoveTrackUp(h.index));
            })
            .background_color(Color::rgb(55, 55, 60))
            .width(Pixels(22.0))
            .height(Pixels(18.0));

        Button::new(cx, |cx| Label::new(cx, "▼").font_size(10.0))
            .on_press(move |ex| {
                let h = item.get();
                ex.emit(AppEvent::MoveTrackDown(h.index));
            })
            .background_color(Color::rgb(55, 55, 60))
            .width(Pixels(22.0))
            .height(Pixels(18.0));

        Element::new(cx).width(Stretch(1.0));

        Button::new(cx, |cx| Label::new(cx, "✕").font_size(10.0))
            .on_press(move |ex| {
                let h = item.get();
                ex.emit(AppEvent::DeleteTrack(h.index));
            })
            .background_color(Color::rgb(80, 50, 50))
            .width(Pixels(22.0))
            .height(Pixels(18.0));
    })
    .gap(Pixels(2.0))
    .alignment(Alignment::Center)
    .height(Pixels(18.0));
}

// ---------------------------------------------------------------------------
// Custom canvas
// ---------------------------------------------------------------------------

const COLOR_LOOP_BAND: Color = Color::rgba(80, 200, 230, 80);
const COLOR_LOOP_EDGE: Color = Color::rgb(80, 200, 230);

#[derive(Clone, Debug)]
enum DragKind {
    LoopRange {
        start_beat: f64,
    },
    MoveClips {
        snapshots: Vec<(ClipRef, f64)>,
    },
    ResizeClip {
        clip: ClipRef,
        original_length_beats: f64,
    },
    Marquee {
        origin_x: f32,
        origin_y: f32,
        cur_x: f32,
        cur_y: f32,
    },
}

pub struct ArrangementCanvas {
    drag: RefCell<Option<DragKind>>,
    drag_origin_x: RefCell<f32>,
    sig: ArrangementSignals,
}

impl ArrangementCanvas {
    pub fn new(cx: &mut Context, sig: ArrangementSignals) -> Handle<'_, Self> {
        Self {
            drag: RefCell::new(None),
            drag_origin_x: RefCell::new(0.0),
            sig,
        }
        .build(cx, |_cx| {})
        .bind(sig.clip_boxes, |mut handle| handle.needs_redraw())
        .bind(sig.track_count, |mut handle| handle.needs_redraw())
        .bind(sig.arrange_zoom_x, |mut handle| handle.needs_redraw())
        .bind(sig.arrange_scroll_beat, |mut handle| handle.needs_redraw())
        .bind(sig.playhead_beat, |mut handle| handle.needs_redraw())
        .bind(sig.loop_start_beat, |mut handle| handle.needs_redraw())
        .bind(sig.loop_end_beat, |mut handle| handle.needs_redraw())
    }

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

    fn hit_clip(clip_boxes: &[ClipBox], beat: f64, track: u32) -> Option<ClipBox> {
        clip_boxes
            .iter()
            .find(|c| {
                c.track == track
                    && (beat as f32) >= c.start_beat
                    && (beat as f32) <= c.start_beat + c.length_beats
            })
            .cloned()
    }
}

impl View for ArrangementCanvas {
    fn element(&self) -> Option<&'static str> {
        Some("arrangement-canvas")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let bounds = cx.bounds();
        let zoom = self.sig.arrange_zoom_x.get_untracked();
        let scroll_beat = self.sig.arrange_scroll_beat.get_untracked();
        let track_count = self.sig.track_count.get_untracked();
        let clip_boxes = self.sig.clip_boxes.get_untracked();
        let playhead = self.sig.playhead_beat.get_untracked();

        canvas.draw_rect(
            rect(bounds.x, bounds.y, bounds.w, bounds.h),
            &fill_paint(COLOR_BG),
        );

        canvas.draw_rect(
            rect(bounds.x, bounds.y, bounds.w, RULER_HEIGHT),
            &fill_paint(COLOR_RULER_BG),
        );

        let loop_start = self.sig.loop_start_beat.get_untracked();
        let loop_end = self.sig.loop_end_beat.get_untracked();
        if loop_end > loop_start {
            let lx = bounds.x + (loop_start - scroll_beat) * zoom;
            let lw = (loop_end - loop_start) * zoom;
            let visible_x = lx.max(bounds.x);
            let visible_right = (lx + lw).min(bounds.x + bounds.w);
            let visible_w = visible_right - visible_x;
            if visible_w > 0.0 {
                canvas.draw_rect(
                    rect(visible_x, bounds.y, visible_w, RULER_HEIGHT),
                    &fill_paint(COLOR_LOOP_BAND),
                );
                let edge_paint = stroke_paint(COLOR_LOOP_EDGE, 1.5);
                let mut p = vg::PathBuilder::new();
                if lx >= bounds.x && lx <= bounds.x + bounds.w {
                    p.move_to((lx, bounds.y));
                    p.line_to((lx, bounds.y + RULER_HEIGHT));
                }
                let rx = lx + lw;
                if rx >= bounds.x && rx <= bounds.x + bounds.w {
                    p.move_to((rx, bounds.y));
                    p.line_to((rx, bounds.y + RULER_HEIGHT));
                }
                let p = p.detach();
                canvas.draw_path(&p, &edge_paint);
            }
        }

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
            let mut p = vg::PathBuilder::new();
            p.move_to((x, bounds.y));
            p.line_to((x, bounds.y + bounds.h));
            let p = p.detach();
            canvas.draw_path(&p, &bar_paint);
        }

        let lane_paint = stroke_paint(COLOR_LANE_LINE, 1.0);
        for i in 0..=track_count {
            let y = bounds.y + RULER_HEIGHT + (i as f32) * ARRANGE_TRACK_HEIGHT;
            if y > bounds.y + bounds.h {
                break;
            }
            let mut p = vg::PathBuilder::new();
            p.move_to((bounds.x, y));
            p.line_to((bounds.x + bounds.w, y));
            let p = p.detach();
            canvas.draw_path(&p, &lane_paint);
        }

        let border_paint = stroke_paint(COLOR_CLIP_BORDER, 1.0);
        let mut text_paint = vg::Paint::default();
        text_paint.set_color(skia_rgba(COLOR_CLIP_TEXT));
        text_paint.set_anti_alias(true);
        let font = vg::Font::default();
        // clip_boxes は (track, clip) 昇順ソート (Memo の compute 順)。
        // Y がビューポートを超えた時点で残り全部不可視なので break。
        let visible_y_bottom = bounds.y + bounds.h;
        for c in &clip_boxes {
            let y = bounds.y + RULER_HEIGHT + (c.track as f32) * ARRANGE_TRACK_HEIGHT;
            if y > visible_y_bottom {
                break;
            }
            let x = bounds.x + (c.start_beat - scroll_beat) * zoom;
            let w = (c.length_beats * zoom).max(2.0);
            if x + w < bounds.x || x > bounds.x + bounds.w {
                continue;
            }
            let h = ARRANGE_TRACK_HEIGHT - 4.0;
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

        if let Some(beat) = playhead {
            let x = bounds.x + (beat - scroll_beat) * zoom;
            if x >= bounds.x && x <= bounds.x + bounds.w {
                let mut path = vg::PathBuilder::new();
                path.move_to((x, bounds.y));
                path.line_to((x, bounds.y + bounds.h));
                let path = path.detach();
                let mut p = stroke_paint(COLOR_PLAYHEAD, 1.5);
                p.set_anti_alias(true);
                canvas.draw_path(&path, &p);
            }
        }
        let _ = MAX_BEATS;
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        let sig = self.sig;
        event.map(|window_event, meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                let bounds = cx.bounds();
                let mx = cx.mouse().cursor_x - bounds.x;
                let my = cx.mouse().cursor_y - bounds.y;
                let zoom = sig.arrange_zoom_x.get_untracked();
                let scroll_beat = sig.arrange_scroll_beat.get_untracked();
                let clip_boxes = sig.clip_boxes.get_untracked();
                let shift = cx.modifiers().shift();
                if (0.0..RULER_HEIGHT).contains(&my) {
                    let beat_at_mouse =
                        ((mx / zoom) + scroll_beat).max(0.0) as f64;
                    let snapped = beat_at_mouse.floor();
                    *self.drag.borrow_mut() = Some(DragKind::LoopRange {
                        start_beat: snapped,
                    });
                    *self.drag_origin_x.borrow_mut() = mx;
                    cx.emit(AppEvent::SetLoopRange {
                        start: snapped,
                        end: snapped,
                    });
                    cx.emit(AppEvent::BeginDrag);
                    cx.capture();
                    meta.consume();
                    return;
                }
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
                    if shift {
                        meta.consume();
                        return;
                    }
                    let right_edge_px =
                        (c.start_beat + c.length_beats - scroll_beat) * zoom;
                    let from_right = right_edge_px - mx;
                    *self.drag_origin_x.borrow_mut() = mx;
                    cx.emit(AppEvent::PushUndoSnapshot);
                    *self.drag.borrow_mut() = if from_right < RESIZE_HANDLE_PX {
                        Some(DragKind::ResizeClip {
                            clip: target,
                            original_length_beats: c.length_beats as f64,
                        })
                    } else {
                        let mut snapshots: Vec<(ClipRef, f64)> = clip_boxes
                            .iter()
                            .filter(|c| c.selected)
                            .map(|c| {
                                (
                                    ClipRef {
                                        track: c.track,
                                        clip: c.clip,
                                    },
                                    c.start_beat as f64,
                                )
                            })
                            .collect();
                        if !snapshots.iter().any(|(r, _)| *r == target) {
                            snapshots.push((target, c.start_beat as f64));
                        }
                        Some(DragKind::MoveClips { snapshots })
                    };
                    cx.emit(AppEvent::BeginDrag);
                    cx.capture();
                    meta.consume();
                } else if my >= RULER_HEIGHT {
                    if !shift {
                        cx.emit(AppEvent::ClearSelection);
                    }
                    *self.drag.borrow_mut() = Some(DragKind::Marquee {
                        origin_x: mx,
                        origin_y: my,
                        cur_x: mx,
                        cur_y: my,
                    });
                    cx.emit(AppEvent::BeginDrag);
                    cx.capture();
                    meta.consume();
                }
            }
            WindowEvent::MouseMove(_, _) => {
                let kind = match self.drag.borrow().clone() {
                    Some(k) => k,
                    None => return,
                };
                let bounds = cx.bounds();
                let mx = cx.mouse().cursor_x - bounds.x;
                let my = cx.mouse().cursor_y - bounds.y;
                let zoom = sig.arrange_zoom_x.get_untracked();
                let dx = mx - *self.drag_origin_x.borrow();
                let dbeat = (dx / zoom) as f64;
                match kind {
                    DragKind::LoopRange { start_beat } => {
                        let cur_beat = ((mx / zoom)
                            + sig.arrange_scroll_beat.get_untracked())
                            .max(0.0) as f64;
                        let snapped_cur = cur_beat.round();
                        let (s, e) = if snapped_cur > start_beat {
                            (start_beat, snapped_cur)
                        } else {
                            (snapped_cur, start_beat)
                        };
                        cx.emit(AppEvent::SetLoopRange { start: s, end: e });
                    }
                    DragKind::MoveClips { snapshots } => {
                        let entries: Vec<(ClipRef, f64)> = snapshots
                            .iter()
                            .map(|(r, s)| (*r, (s + dbeat).max(0.0)))
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
                            length: new_len,
                        });
                    }
                    DragKind::Marquee {
                        origin_x, origin_y, ..
                    } => {
                        *self.drag.borrow_mut() = Some(DragKind::Marquee {
                            origin_x,
                            origin_y,
                            cur_x: mx,
                            cur_y: my,
                        });
                    }
                }
            }
            WindowEvent::MouseUp(MouseButton::Left) if self.drag.borrow().is_some() => {
                let drag = self.drag.borrow_mut().take();
                if let Some(DragKind::Marquee {
                    origin_x,
                    origin_y,
                    cur_x,
                    cur_y,
                }) = drag
                {
                    let zoom = sig.arrange_zoom_x.get_untracked();
                    let scroll_beat = sig.arrange_scroll_beat.get_untracked();
                    let clip_boxes = sig.clip_boxes.get_untracked();
                    let x0 = origin_x.min(cur_x);
                    let x1 = origin_x.max(cur_x);
                    let y0 = origin_y.min(cur_y);
                    let y1 = origin_y.max(cur_y);
                    let beat0 = (x0 / zoom + scroll_beat).max(0.0);
                    let beat1 = (x1 / zoom + scroll_beat).max(0.0);
                    let track0 = ((y0 - RULER_HEIGHT).max(0.0) / ARRANGE_TRACK_HEIGHT) as u32;
                    let track1 = ((y1 - RULER_HEIGHT).max(0.0) / ARRANGE_TRACK_HEIGHT) as u32;
                    let hits: Vec<ClipRef> = clip_boxes
                        .iter()
                        .filter(|c| {
                            c.track >= track0
                                && c.track <= track1
                                && c.start_beat <= beat1
                                && c.start_beat + c.length_beats >= beat0
                        })
                        .map(|c| ClipRef {
                            track: c.track,
                            clip: c.clip,
                        })
                        .collect();
                    cx.emit(AppEvent::SetClipSelection(hits));
                }
                cx.release();
                cx.emit(AppEvent::EndDrag);
            }
            WindowEvent::MouseDoubleClick(MouseButton::Left) => {
                let bounds = cx.bounds();
                let mx = cx.mouse().cursor_x - bounds.x;
                let my = cx.mouse().cursor_y - bounds.y;
                let zoom = sig.arrange_zoom_x.get_untracked();
                let scroll_beat = sig.arrange_scroll_beat.get_untracked();
                let clip_boxes = sig.clip_boxes.get_untracked();
                let track_count = sig.track_count.get_untracked();
                if (0.0..RULER_HEIGHT).contains(&my) {
                    cx.emit(AppEvent::SetLoopRange {
                        start: 0.0,
                        end: 0.0,
                    });
                    meta.consume();
                    return;
                }
                let Some((beat, track)) =
                    Self::coord_to_beat_track(mx, my, zoom, scroll_beat)
                else {
                    return;
                };
                if track >= track_count {
                    return;
                }
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
                let snapped = beat.floor().max(0.0);
                cx.emit(AppEvent::CreateClip {
                    track,
                    start_beat: snapped,
                });
                cx.emit(AppEvent::SelectBottomPanel(1));
                meta.consume();
            }
            WindowEvent::MouseScroll(_, dy) => {
                let mods = cx.modifiers();
                if mods.ctrl() {
                    let zoom = sig.arrange_zoom_x.get_untracked();
                    let factor = 1.15_f32.powf(*dy);
                    let new_zoom = (zoom * factor).clamp(2.0, 400.0);
                    cx.emit(AppEvent::SetArrangeZoom(new_zoom));
                } else {
                    let scroll = sig.arrange_scroll_beat.get_untracked();
                    let speed = 1.0_f32;
                    let new_scroll = (scroll - dy * speed).max(0.0);
                    cx.emit(AppEvent::SetArrangeScroll(new_scroll));
                }
                meta.consume();
            }
            _ => {}
        });
    }
}
