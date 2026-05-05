//! Piano roll: gui_01 の `Ui::piano_roll` widget でノート描画 / 編集 / velocity lane / playhead
//! を行う。
//! - Shift+drag で加算 rect select (widget が自動)
//! - Insert キー / 空白上 dbl-click で AddNote (widget Insert + daw_01 エミュレート、1/16 snap)
//! - drag move / 端 drag resize / Delete キー は widget 内蔵
//! - wheel: Ctrl→ZoomY, Shift→ScrollX, plain→TopPitch (drag 中は無効)

use std::sync::Arc;

use daw_ui_core::{
    Edit, MoveDelta, Note, NotesEditRequest, PianoRollStyle, PianoRollView, ResizeDelta, Ui,
    note_hit,
};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent, ClipRef, DEFAULT_NOTE_DURATION};

const KEYBOARD_W: f32 = 56.0;
const VEL_LANE_H: f32 = 60.0;
const RULER_H: f32 = 20.0;

const COLOR_BG: Color = Color { r: 0.10, g: 0.10, b: 0.12, a: 1.0 };
const COLOR_HINT: Color = Color { r: 0.55, g: 0.58, b: 0.65, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let Some(target) = app.selected_clip else {
        // クリップ未選択時のプレースホルダ
        ui.panel("pr_bg_empty", area, COLOR_BG, 0.0);
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

    // widget が ruler / velocity lane を内蔵 (M13 Phase 55 で ruler 追加)、
    // grid 部分は area から keyboard / ruler / vel lane を引いた領域。
    // note hit detection はこの grid_rect を使うので、widget 内部 layout と
    // 揃えておく (rect.y から ruler_h、その下に keyboard+grid、最下段に vel_lane)。
    let grid_h = area.h - VEL_LANE_H - RULER_H;
    let grid_rect = Rect {
        x: area.x + KEYBOARD_W,
        y: area.y + RULER_H,
        w: area.w - KEYBOARD_W,
        h: grid_h,
    };

    let widget_notes = build_widget_notes(app, target);
    let zoom_x = app.pianoroll_zoom_x.max(4.0);
    let zoom_y = app.pianoroll_zoom_y.max(6.0);
    let view = PianoRollView {
        start_beat: app.pianoroll_scroll_beat as f64,
        len_beats: (grid_rect.w / zoom_x) as f64,
        pitch_top: app.pianoroll_top_pitch as f32,
        pitch_visible: grid_h / zoom_y,
        keyboard_w: KEYBOARD_W,
        notes_generation: app.pianoroll_notes_generation,
        velocity_lane_h: VEL_LANE_H,
        playhead_beat: app.playhead_beat.map(|b| b as f64),
        ruler_h: RULER_H,
        bpm: app.song.bpm,
        time_sig: app.song.time_sig,
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
            NotesEditRequest::SetLyrics(updates) => {
                // gui_01 #017 で widget が L キー編集 → Enter commit 時に
                // 1 batch で発行する歌詞分配 request。 各 (note_id, lyric)
                // を current selected_clip 内で更新。
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetNoteLyrics(updates.clone()));
                })
            }
        }
    };

    let resp = ui.piano_roll(
        "piano_roll",
        area,
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

