//! Piano roll: gui_01 の `Ui::piano_roll` widget でノート描画 / 編集を行い、
//! velocity lane と playhead は自前で描画する。
//! - Shift+drag で加算 rect select (widget が自動)
//! - Insert キー / 空白上 dbl-click で AddNote (widget Insert + daw_01 エミュレート、1/16 snap)
//! - drag move / 端 drag resize / Delete キー は widget 内蔵
//! - wheel: Ctrl→ZoomY, Shift→ScrollX, plain→TopPitch (drag 中は無効)

use std::sync::Arc;

use daw_ui_core::{
    Edit, MoveDelta, Note, NotesEditRequest, PianoRollStyle, PianoRollView, ResizeDelta, Ui,
    note_hit,
};
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::app::{AppData, AppEvent, ClipRef, DEFAULT_NOTE_DURATION};

const KEYBOARD_W: f32 = 56.0;
const VEL_LANE_H: f32 = 60.0;

const COLOR_BG: Color = Color { r: 0.10, g: 0.10, b: 0.12, a: 1.0 };
const COLOR_NOTE_SEL: Color = Color { r: 0.95, g: 0.85, b: 0.45, a: 0.95 };
const COLOR_PLAYHEAD: Color = Color { r: 0.90, g: 0.30, b: 0.30, a: 1.0 };
const COLOR_VEL_BG: Color = Color { r: 0.13, g: 0.13, b: 0.16, a: 1.0 };
const COLOR_VEL_BAR: Color = Color { r: 0.55, g: 0.78, b: 0.95, a: 0.85 };
const COLOR_HINT: Color = Color { r: 0.55, g: 0.58, b: 0.65, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let Some(target) = app.selected_clip else {
        // クリップ未選択時のプレースホルダ
        ui.heavy("pr_bg_empty", |hctx| {
            hctx.cached((area.w.to_bits(), area.h.to_bits()), |hctx| {
                hctx.push_rect(RectCommand {
                    rect: area,
                    fill: COLOR_BG,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            });
        });
        ui.label_at(
            "pr_no_clip",
            "(\u{30af}\u{30ea}\u{30c3}\u{30d7}\u{304c}\u{9078}\u{629e}\u{3055}\u{308c}\u{3066}\u{3044}\u{307e}\u{305b}\u{3093})",
            area.x + 12.0,
            area.y + 12.0,
            12.0,
            COLOR_HINT,
        );
        return;
    };

    let widget_area = Rect {
        x: area.x,
        y: area.y,
        w: area.w,
        h: area.h - VEL_LANE_H,
    };
    let vel_area = Rect {
        x: area.x + KEYBOARD_W,
        y: area.y + area.h - VEL_LANE_H,
        w: area.w - KEYBOARD_W,
        h: VEL_LANE_H,
    };
    let grid_rect = Rect {
        x: widget_area.x + KEYBOARD_W,
        y: widget_area.y,
        w: widget_area.w - KEYBOARD_W,
        h: widget_area.h,
    };

    let widget_notes = build_widget_notes(app, target);
    let zoom_x = app.pianoroll_zoom_x.max(4.0);
    let zoom_y = app.pianoroll_zoom_y.max(6.0);
    let view = PianoRollView {
        start_beat: app.pianoroll_scroll_beat as f64,
        len_beats: ((widget_area.w - KEYBOARD_W) / zoom_x) as f64,
        pitch_top: app.pianoroll_top_pitch as f32,
        pitch_visible: widget_area.h / zoom_y,
        keyboard_w: KEYBOARD_W,
        notes_generation: app.pianoroll_notes_generation,
    };
    let style = PianoRollStyle::default();
    let resize_handle_px = style.resize_handle_px;

    let make_edit = move |req: NotesEditRequest| -> Edit<AppData> {
        match req {
            NotesEditRequest::Add(notes) => {
                let Some(n) = notes.into_iter().next() else {
                    return Edit::mutate(|_| {});
                };
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::AddNote {
                        track: target.track,
                        clip: target.clip,
                        start_beat: n.start_beat,
                        duration: n.len_beats,
                        pitch: n.pitch,
                    });
                })
            }
            NotesEditRequest::Delete(notes) => {
                let ids: Vec<u32> = notes.iter().map(|n| n.id).collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNoteSelection(ids.clone()));
                    app.handle_event(AppEvent::DeleteSelectedNotes);
                })
            }
            NotesEditRequest::Move(deltas) => {
                let entries: Vec<(u32, f64, u8)> =
                    deltas.iter().map(|d: &MoveDelta| (d.0, d.3, d.4)).collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNotePositions(entries.clone()));
                })
            }
            NotesEditRequest::Resize(deltas) => {
                let entries: Vec<(u32, f64, f64)> = deltas
                    .iter()
                    .map(|d: &ResizeDelta| (d.0, d.3, d.4))
                    .collect();
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ResizeNotes(entries.clone()));
                })
            }
            NotesEditRequest::Select { next, .. } => Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetNoteSelection(next.clone()));
            }),
        }
    };

    let resp = ui.piano_roll(
        "piano_roll",
        widget_area,
        &widget_notes,
        view,
        &app.selected_notes,
        &style,
        make_edit,
    );

    // 空白上 dbl-click → AddNote (1/16 grid snap、gui_01 #003 のサンプルベース)
    if let Some((px, py)) = ui.take_double_click_in_rect(grid_rect)
        && note_hit(&widget_notes, view, grid_rect, px, py, resize_handle_px).is_none()
    {
        let beat_to_px = grid_rect.w as f64 / view.len_beats.max(1e-6);
        let pitch_to_px = grid_rect.h / view.pitch_visible.max(1e-6);
        let beat_raw = view.start_beat + (px - grid_rect.x) as f64 / beat_to_px;
        let snapped_beat = ((beat_raw * 16.0).round() / 16.0).max(0.0);
        let pitch_raw = view.pitch_top - (py - grid_rect.y) / pitch_to_px;
        let pitch = (pitch_raw.round() as i32).clamp(0, 127) as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::AddNote {
                track: target.track,
                clip: target.clip,
                start_beat: snapped_beat,
                duration: DEFAULT_NOTE_DURATION,
                pitch,
            });
        }));
    }

    // wheel handler — note drag 中は無効
    if resp.dragging.is_none() {
        let pointer = ui.pointer();
        if let Some((px, py)) = pointer.pos
            && grid_rect.contains(px, py)
        {
            let (sx, sy) = pointer.scroll_delta;
            if sy.abs() > 0.001 || sx.abs() > 0.001 {
                let scroll_beat = app.pianoroll_scroll_beat;
                let top_pitch = app.pianoroll_top_pitch as i32;
                let modifiers = pointer.modifiers;
                if modifiers.ctrl {
                    let factor = (sy * 0.005).exp();
                    let new_zoom = (zoom_y * factor).clamp(6.0, 40.0);
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetPianoRollZoomY(new_zoom));
                    }));
                } else if modifiers.shift {
                    let dx_beats = -(sx + sy) / zoom_x;
                    let new_scroll = (scroll_beat + dx_beats).max(0.0);
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetPianoRollScrollX(new_scroll));
                    }));
                } else {
                    let delta = (sy / 12.0).round() as i32;
                    if delta != 0 {
                        let new_top = (top_pitch + delta).clamp(11, 127) as u8;
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetPianoRollTopPitch(new_top));
                        }));
                    }
                }
            }
        }
    }

    draw_velocity_lane(app, ui, vel_area, view);
    draw_playhead(app, ui, grid_rect, vel_area, view);
}

/// `daw_ui_core::Note` 形式に変換 (毎フレーム alloc、widget 内 cached で性能 OK)。
fn build_widget_notes(app: &AppData, target: ClipRef) -> Vec<Note> {
    let Some(track) = app.song.tracks.get(target.track as usize) else {
        return Vec::new();
    };
    let Some(clip) = track.clips.get(target.clip as usize) else {
        return Vec::new();
    };
    clip.notes
        .iter()
        .enumerate()
        .map(|(i, n)| Note {
            id: i as u32,
            start_beat: n.start_beat,
            len_beats: n.duration_beats,
            pitch: n.pitch,
            velocity: n.velocity,
            lyric: n
                .lyric
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(Arc::from),
        })
        .collect()
}

fn draw_velocity_lane(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect, view: PianoRollView) {
    let notes = app.note_boxes();
    let beat_to_px = area.w as f64 / view.len_beats.max(1e-6);

    ui.heavy("pr_vel_lane", |hctx| {
        let key = (
            area.w.to_bits(),
            area.h.to_bits(),
            view.start_beat.to_bits(),
            view.len_beats.to_bits(),
            notes.len() as u64,
            app.pianoroll_notes_generation,
        );
        hctx.cached(key, |hctx| {
            hctx.push_rect(RectCommand {
                rect: area,
                fill: COLOR_VEL_BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
            for n in &notes {
                let x = area.x + ((n.start_beat as f64 - view.start_beat) * beat_to_px) as f32;
                if x < area.x - 4.0 || x > area.x + area.w + 4.0 {
                    continue;
                }
                let v = (n.velocity as f32) / 127.0;
                let h = (area.h - 6.0) * v;
                hctx.push_rect(RectCommand {
                    rect: Rect {
                        x: x - 1.5,
                        y: area.y + area.h - 3.0 - h,
                        w: 3.0,
                        h,
                    },
                    fill: if n.selected {
                        COLOR_NOTE_SEL
                    } else {
                        COLOR_VEL_BAR
                    },
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: Some(area),
                });
            }
        });
    });
}

fn draw_playhead(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    grid_rect: Rect,
    vel_area: Rect,
    view: PianoRollView,
) {
    let Some(b) = app.playhead_beat else {
        return;
    };
    let beat_to_px = grid_rect.w as f64 / view.len_beats.max(1e-6);
    let x = grid_rect.x + ((b as f64 - view.start_beat) * beat_to_px) as f32;
    if x < grid_rect.x || x > grid_rect.x + grid_rect.w {
        return;
    }
    ui.heavy("pr_playhead", |hctx| {
        let key = (
            x.to_bits(),
            grid_rect.h.to_bits(),
            vel_area.h.to_bits(),
        );
        hctx.cached(key, |hctx| {
            hctx.push_lines(LineBatch {
                segments: vec![
                    LineSegment {
                        a: [x, grid_rect.y],
                        b: [x, grid_rect.y + grid_rect.h],
                        color: COLOR_PLAYHEAD,
                    },
                    LineSegment {
                        a: [x, vel_area.y],
                        b: [x, vel_area.y + vel_area.h],
                        color: COLOR_PLAYHEAD,
                    },
                ],
                line_width_px: 1.5,
                clip_rect: None,
            });
        });
    });
}
