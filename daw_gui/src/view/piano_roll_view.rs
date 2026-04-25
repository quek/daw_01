//! Piano roll view for the currently selected clip.
//!
//! Vertical axis is MIDI pitch (visible range fixed in v1 to a 3-octave
//! window around middle C), horizontal axis is beats within the clip.
//! Notes are drawn as rectangles; mouse interactions mirror the
//! arrangement view (click=select, drag=move, edge-drag=resize,
//! double-click empty=add).

use vizia::prelude::*;
use vizia::vg;

use crate::app::{AppData, AppEvent, ClipRef, DEFAULT_NOTE_DURATION, NoteBox};

const COLOR_BG: Color = Color::rgb(28, 28, 32);
const COLOR_GRID: Color = Color::rgb(48, 48, 56);
const COLOR_BAR: Color = Color::rgb(70, 70, 80);
const COLOR_BLACK_KEY_LANE: Color = Color::rgb(34, 34, 40);
const COLOR_NOTE: Color = Color::rgb(120, 200, 130);
const COLOR_NOTE_SELECTED: Color = Color::rgb(220, 240, 180);
const COLOR_NOTE_BORDER: Color = Color::rgb(20, 28, 20);
const COLOR_NOTE_TEXT: Color = Color::rgb(20, 30, 20);
const COLOR_KEYBOARD_BG: Color = Color::rgb(40, 40, 44);
const COLOR_WHITE_KEY: Color = Color::rgb(220, 220, 220);
const COLOR_BLACK_KEY: Color = Color::rgb(40, 40, 44);

const KEYBOARD_WIDTH: f32 = 48.0;
const NOTE_RESIZE_HANDLE_PX: f32 = 4.0;

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

pub struct PianoRollView;

impl PianoRollView {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |cx| {
            Binding::new(cx, AppData::selected_clip, |cx, sel| {
                if sel.get(cx).is_none() {
                    Label::new(cx, "クリップを選択してください")
                        .color(Color::rgb(150, 150, 150))
                        .font_size(13.0)
                        .alignment(Alignment::Center)
                        .width(Stretch(1.0))
                        .height(Stretch(1.0));
                } else {
                    PianoRollCanvas::new(cx)
                        .width(Stretch(1.0))
                        .height(Stretch(1.0));
                }
            });
        })
    }
}

impl View for PianoRollView {
    fn element(&self) -> Option<&'static str> {
        Some("piano-roll-view")
    }
}

#[derive(Clone, Debug)]
enum NoteDrag {
    Move {
        snapshots: Vec<(u32, f64, u8)>, // (note_idx, original_start_beat, original_pitch)
    },
    Resize {
        note_idx: u32,
        original_duration: f64,
    },
    Marquee {
        origin_x: f32,
        origin_y: f32,
        cur_x: f32,
        cur_y: f32,
    },
}

pub struct PianoRollCanvas {
    drag: Option<NoteDrag>,
    drag_origin_x: f32,
    drag_origin_y: f32,
}

impl PianoRollCanvas {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        // See ArrangementCanvas — custom views must explicitly subscribe
        // to lens changes for `draw` to refresh.
        Self {
            drag: None,
            drag_origin_x: 0.0,
            drag_origin_y: 0.0,
        }
        .build(cx, |_cx| {})
        .bind(AppData::note_boxes, |mut handle, _| handle.needs_redraw())
        .bind(AppData::pianoroll_zoom_x, |mut handle, _| handle.needs_redraw())
        .bind(AppData::pianoroll_zoom_y, |mut handle, _| handle.needs_redraw())
        .bind(AppData::pianoroll_scroll_beat, |mut handle, _| {
            handle.needs_redraw()
        })
        .bind(AppData::pianoroll_top_pitch, |mut handle, _| handle.needs_redraw())
    }

    fn coord_to_beat_pitch(
        canvas_x: f32,
        y: f32,
        zoom_x: f32,
        zoom_y: f32,
        scroll_beat: f32,
        top_pitch: u8,
    ) -> Option<(f64, u8)> {
        let beat = ((canvas_x / zoom_x).max(0.0) + scroll_beat) as f64;
        let row = (y / zoom_y).floor();
        if row < 0.0 {
            return None;
        }
        let pitch_i32 = top_pitch as i32 - row as i32;
        if !(0..=127).contains(&pitch_i32) {
            return None;
        }
        Some((beat, pitch_i32 as u8))
    }

    fn hit_note(notes: &[NoteBox], beat: f64, pitch: u8) -> Option<&NoteBox> {
        notes.iter().find(|n| {
            n.pitch == pitch
                && (beat as f32) >= n.start_beat
                && (beat as f32) <= n.start_beat + n.duration_beats
        })
    }
}

fn is_black_key(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

impl View for PianoRollCanvas {
    fn element(&self) -> Option<&'static str> {
        Some("piano-roll-canvas")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let bounds = cx.bounds();
        let zoom_x: f32 = AppData::pianoroll_zoom_x.get(cx);
        let zoom_y: f32 = AppData::pianoroll_zoom_y.get(cx);
        let scroll_beat: f32 = AppData::pianoroll_scroll_beat.get(cx);
        let top_pitch: u8 = AppData::pianoroll_top_pitch.get(cx);
        let notes: Vec<NoteBox> = AppData::note_boxes.get(cx);

        // Background.
        canvas.draw_rect(
            rect(bounds.x, bounds.y, bounds.w, bounds.h),
            &fill_paint(COLOR_BG),
        );

        // Keyboard column.
        canvas.draw_rect(
            rect(bounds.x, bounds.y, KEYBOARD_WIDTH, bounds.h),
            &fill_paint(COLOR_KEYBOARD_BG),
        );
        let mut text_paint = vg::Paint::default();
        text_paint.set_color(skia_rgba(Color::rgb(60, 60, 60)));
        text_paint.set_anti_alias(true);
        let font = vg::Font::default();
        // Number of semitone rows that fit in the visible bounds, plus
        // one extra row to cover partial rows at the bottom edge.
        let visible_rows = ((bounds.h / zoom_y) as u32) + 1;
        for i in 0..=visible_rows {
            let pitch_i = top_pitch as i32 - i as i32;
            if pitch_i < 0 {
                break;
            }
            let pitch = pitch_i as u8;
            let y = bounds.y + (i as f32) * zoom_y;
            if y > bounds.y + bounds.h {
                break;
            }
            let key_color = if is_black_key(pitch) {
                COLOR_BLACK_KEY
            } else {
                COLOR_WHITE_KEY
            };
            canvas.draw_rect(
                rect(bounds.x + 1.0, y, KEYBOARD_WIDTH - 2.0, zoom_y - 1.0),
                &fill_paint(key_color),
            );
            if pitch.is_multiple_of(12) {
                let octave = (pitch / 12) as i32 - 1;
                canvas.draw_str(
                    format!("C{octave}"),
                    (bounds.x + 4.0, y + zoom_y - 2.0),
                    &font,
                    &text_paint,
                );
            }
        }

        let canvas_x0 = bounds.x + KEYBOARD_WIDTH;
        let canvas_w = bounds.w - KEYBOARD_WIDTH;

        // Black-key lanes.
        for i in 0..=visible_rows {
            let pitch_i = top_pitch as i32 - i as i32;
            if pitch_i < 0 {
                break;
            }
            let pitch = pitch_i as u8;
            if !is_black_key(pitch) {
                continue;
            }
            let y = bounds.y + (i as f32) * zoom_y;
            canvas.draw_rect(
                rect(canvas_x0, y, canvas_w, zoom_y),
                &fill_paint(COLOR_BLACK_KEY_LANE),
            );
        }

        // Horizontal grid lines (one per pitch).
        let grid_paint = stroke_paint(COLOR_GRID, 1.0);
        for i in 0..=visible_rows {
            let y = bounds.y + (i as f32) * zoom_y;
            if y > bounds.y + bounds.h {
                break;
            }
            let mut p = vg::Path::new();
            p.move_to((canvas_x0, y));
            p.line_to((canvas_x0 + canvas_w, y));
            canvas.draw_path(&p, &grid_paint);
        }

        // Vertical beat / bar lines. Start from the first whole beat at
        // or before the visible left edge so the grid doesn't drift when
        // scrolled.
        let first_beat = scroll_beat.floor() as i32;
        let visible_beats = (canvas_w / zoom_x) as i32 + 1;
        for offset in 0..=visible_beats {
            let beat = first_beat + offset;
            if beat < 0 {
                continue;
            }
            let x = canvas_x0 + (beat as f32 - scroll_beat) * zoom_x;
            if x > canvas_x0 + canvas_w {
                break;
            }
            let stroke = if (beat as u32).is_multiple_of(4) { 1.5 } else { 0.5 };
            let mut p = vg::Path::new();
            p.move_to((x, bounds.y));
            p.line_to((x, bounds.y + bounds.h));
            canvas.draw_path(&p, &stroke_paint(COLOR_BAR, stroke));
        }

        // Notes.
        let border_paint = stroke_paint(COLOR_NOTE_BORDER, 1.0);
        let mut note_text_paint = vg::Paint::default();
        note_text_paint.set_color(skia_rgba(COLOR_NOTE_TEXT));
        note_text_paint.set_anti_alias(true);
        for n in &notes {
            let row_i = top_pitch as i32 - n.pitch as i32;
            if row_i < 0 {
                continue;
            }
            let row = row_i as f32;
            let x = canvas_x0 + (n.start_beat - scroll_beat) * zoom_x;
            let y = bounds.y + row * zoom_y;
            if y > bounds.y + bounds.h {
                continue;
            }
            let w = (n.duration_beats * zoom_x).max(2.0);
            let h = (zoom_y - 1.0).max(2.0);
            if x + w < canvas_x0 || x > canvas_x0 + canvas_w {
                continue;
            }
            let body_color = if n.selected {
                COLOR_NOTE_SELECTED
            } else {
                COLOR_NOTE
            };
            let r = rect(x, y, w, h);
            canvas.draw_rect(r, &fill_paint(body_color));
            canvas.draw_rect(r, &border_paint);
            if !n.lyric.is_empty() && w > 16.0 {
                canvas.draw_str(
                    n.lyric.as_str(),
                    (x + 3.0, y + h - 3.0),
                    &font,
                    &note_text_paint,
                );
            }
        }
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                let bounds = cx.bounds();
                let mx = cx.mouse().cursor_x - bounds.x;
                let my = cx.mouse().cursor_y - bounds.y;
                if mx < KEYBOARD_WIDTH {
                    return;
                }
                let canvas_x = mx - KEYBOARD_WIDTH;
                let zoom_x = AppData::pianoroll_zoom_x.get(cx);
                let zoom_y = AppData::pianoroll_zoom_y.get(cx);
                let scroll_beat = AppData::pianoroll_scroll_beat.get(cx);
                let top_pitch = AppData::pianoroll_top_pitch.get(cx);
                let shift = cx.modifiers().shift();
                let Some((beat, pitch)) = Self::coord_to_beat_pitch(
                    canvas_x, my, zoom_x, zoom_y, scroll_beat, top_pitch,
                ) else {
                    return;
                };
                let notes: Vec<NoteBox> = AppData::note_boxes.get(cx);
                if let Some(hit) = Self::hit_note(&notes, beat, pitch) {
                    cx.emit(AppEvent::SelectNote {
                        note: hit.note,
                        additive: shift,
                    });
                    if shift {
                        meta.consume();
                        return;
                    }
                    let right_px =
                        (hit.start_beat + hit.duration_beats - scroll_beat) * zoom_x;
                    let from_right = right_px - canvas_x;
                    self.drag_origin_x = canvas_x;
                    self.drag_origin_y = my;
                    self.drag = if from_right < NOTE_RESIZE_HANDLE_PX {
                        Some(NoteDrag::Resize {
                            note_idx: hit.note,
                            original_duration: hit.duration_beats as f64,
                        })
                    } else {
                        // Snapshot every selected note plus the freshly
                        // clicked one (which SelectNote above just made
                        // primary, replacing prior selection if no shift).
                        let mut snapshots: Vec<(u32, f64, u8)> = notes
                            .iter()
                            .filter(|n| n.selected)
                            .map(|n| (n.note, n.start_beat as f64, n.pitch))
                            .collect();
                        if !snapshots.iter().any(|(idx, _, _)| *idx == hit.note) {
                            snapshots.push((
                                hit.note,
                                hit.start_beat as f64,
                                hit.pitch,
                            ));
                        }
                        Some(NoteDrag::Move { snapshots })
                    };
                    cx.capture();
                    meta.consume();
                } else {
                    if !shift {
                        cx.emit(AppEvent::ClearNoteSelection);
                    }
                    self.drag_origin_x = canvas_x;
                    self.drag_origin_y = my;
                    self.drag = Some(NoteDrag::Marquee {
                        origin_x: canvas_x,
                        origin_y: my,
                        cur_x: canvas_x,
                        cur_y: my,
                    });
                    cx.capture();
                    meta.consume();
                }
            }
            WindowEvent::MouseDoubleClick(MouseButton::Left) => {
                let bounds = cx.bounds();
                let mx = cx.mouse().cursor_x - bounds.x;
                let my = cx.mouse().cursor_y - bounds.y;
                if mx < KEYBOARD_WIDTH {
                    return;
                }
                let canvas_x = mx - KEYBOARD_WIDTH;
                let zoom_x = AppData::pianoroll_zoom_x.get(cx);
                let zoom_y = AppData::pianoroll_zoom_y.get(cx);
                let scroll_beat = AppData::pianoroll_scroll_beat.get(cx);
                let top_pitch = AppData::pianoroll_top_pitch.get(cx);
                let Some((beat, pitch)) = Self::coord_to_beat_pitch(
                    canvas_x, my, zoom_x, zoom_y, scroll_beat, top_pitch,
                ) else {
                    return;
                };
                let notes: Vec<NoteBox> = AppData::note_boxes.get(cx);
                if Self::hit_note(&notes, beat, pitch).is_some() {
                    return;
                }
                let Some(target) = AppData::selected_clip.get(cx) else {
                    return;
                };
                // Snap to 1/16 beat grid.
                let snapped = (beat * 4.0).floor() / 4.0;
                let snapped = snapped.max(0.0);
                cx.emit(AppEvent::AddNote {
                    track: target.track,
                    clip: target.clip,
                    start_beat_bits: snapped.to_bits(),
                    duration_bits: DEFAULT_NOTE_DURATION.to_bits(),
                    pitch,
                });
                meta.consume();
            }
            WindowEvent::MouseMove(_, _) => {
                let Some(kind) = self.drag.clone() else { return };
                let Some(target): Option<ClipRef> =
                    AppData::selected_clip.get(cx)
                else {
                    return;
                };
                let bounds = cx.bounds();
                let mx = cx.mouse().cursor_x - bounds.x;
                let my = cx.mouse().cursor_y - bounds.y;
                let canvas_x = (mx - KEYBOARD_WIDTH).max(0.0);
                let zoom_x = AppData::pianoroll_zoom_x.get(cx);
                let zoom_y = AppData::pianoroll_zoom_y.get(cx);
                let dx = canvas_x - self.drag_origin_x;
                let dbeat = (dx / zoom_x) as f64;
                match kind {
                    NoteDrag::Move { snapshots } => {
                        let dy = my - self.drag_origin_y;
                        let drow = (dy / zoom_y).round() as i32;
                        let entries: Vec<(u32, u64, u8)> = snapshots
                            .iter()
                            .map(|(idx, beat, pitch)| {
                                let new_start = (beat + dbeat).max(0.0);
                                let new_pitch = (*pitch as i32)
                                    .saturating_sub(drow)
                                    .clamp(0, 127)
                                    as u8;
                                (*idx, new_start.to_bits(), new_pitch)
                            })
                            .collect();
                        cx.emit(AppEvent::SetNotePositions(entries));
                    }
                    NoteDrag::Resize {
                        note_idx,
                        original_duration,
                    } => {
                        let new_dur = (original_duration + dbeat).max(0.0625);
                        cx.emit(AppEvent::ResizeNote {
                            track: target.track,
                            clip: target.clip,
                            note: note_idx,
                            duration_bits: new_dur.to_bits(),
                        });
                    }
                    NoteDrag::Marquee { origin_x, origin_y, .. } => {
                        self.drag = Some(NoteDrag::Marquee {
                            origin_x,
                            origin_y,
                            cur_x: canvas_x,
                            cur_y: my,
                        });
                    }
                }
            }
            WindowEvent::MouseUp(MouseButton::Left) if self.drag.is_some() => {
                if let Some(NoteDrag::Marquee {
                    origin_x,
                    origin_y,
                    cur_x,
                    cur_y,
                }) = self.drag.clone()
                {
                    let zoom_x = AppData::pianoroll_zoom_x.get(cx);
                    let zoom_y = AppData::pianoroll_zoom_y.get(cx);
                    let scroll_beat = AppData::pianoroll_scroll_beat.get(cx);
                    let top_pitch = AppData::pianoroll_top_pitch.get(cx);
                    let notes: Vec<NoteBox> = AppData::note_boxes.get(cx);
                    let x0 = origin_x.min(cur_x);
                    let x1 = origin_x.max(cur_x);
                    let y0 = origin_y.min(cur_y);
                    let y1 = origin_y.max(cur_y);
                    let beat0 = (x0 / zoom_x + scroll_beat).max(0.0);
                    let beat1 = (x1 / zoom_x + scroll_beat).max(0.0);
                    let row0 = (y0 / zoom_y) as i32;
                    let row1 = (y1 / zoom_y) as i32;
                    let pitch_high = top_pitch as i32 - row0;
                    let pitch_low = top_pitch as i32 - row1;
                    let hits: Vec<u32> = notes
                        .iter()
                        .filter(|n| {
                            let p = n.pitch as i32;
                            p >= pitch_low
                                && p <= pitch_high
                                && n.start_beat <= beat1
                                && n.start_beat + n.duration_beats >= beat0
                        })
                        .map(|n| n.note)
                        .collect();
                    cx.emit(AppEvent::SetNoteSelection(hits));
                }
                self.drag = None;
                cx.release();
            }
            WindowEvent::MouseScroll(_, dy) => {
                let mods = cx.modifiers();
                if mods.ctrl() && mods.shift() {
                    // Vertical (pitch) zoom.
                    let z = AppData::pianoroll_zoom_y.get(cx);
                    let new_z = (z * 1.15_f32.powf(*dy)).clamp(6.0, 40.0);
                    cx.emit(AppEvent::SetPianoRollZoomY(new_z.to_bits()));
                } else if mods.ctrl() {
                    // Horizontal (time) zoom.
                    let z = AppData::pianoroll_zoom_x.get(cx);
                    let new_z = (z * 1.15_f32.powf(*dy)).clamp(8.0, 400.0);
                    cx.emit(AppEvent::SetPianoRollZoomX(new_z.to_bits()));
                } else if mods.shift() {
                    // Horizontal scroll.
                    let s = AppData::pianoroll_scroll_beat.get(cx);
                    let new_s = (s - dy * 1.0).max(0.0);
                    cx.emit(AppEvent::SetPianoRollScrollX(new_s.to_bits()));
                } else {
                    // Vertical scroll — wheel-up moves view up (top pitch
                    // increases, you see higher notes).
                    let top = AppData::pianoroll_top_pitch.get(cx) as i32;
                    let step = if *dy > 0.0 { 2 } else { -2 };
                    let new_top = (top + step).clamp(11, 127) as u8;
                    cx.emit(AppEvent::SetPianoRollTopPitch(new_top));
                }
                meta.consume();
            }
            _ => {}
        });
    }
}
