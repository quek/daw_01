//! Piano roll: 鍵盤 (左) + ノート canvas + ベロシティレーン (下)。
//! 現状: 描画 + クリックでノート選択 + Shift+ドラッグで矩形選択 (M8 DragRect) +
//! Wheel で縦スクロール / Ctrl+Wheel で縦ズーム / Shift+Wheel で横スクロール。
//! TODO: マウスドラッグでノート移動 / リサイズ / ベロシティドラッグ /
//! ダブルクリックで AddNote。

use daw_ui_core::{
    BarBeatGridStyle, Edit, TimeDisplay, TimeMapping, Ui, ViewportState1D, WidgetId,
};
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::app::{AppData, AppEvent, DEFAULT_NOTE_DURATION};

const KEYBOARD_W: f32 = 56.0;
const VEL_LANE_H: f32 = 60.0;

const COLOR_BG: Color = Color { r: 0.10, g: 0.10, b: 0.12, a: 1.0 };
const COLOR_KEY_W: Color = Color { r: 0.92, g: 0.93, b: 0.95, a: 1.0 };
const COLOR_KEY_B: Color = Color { r: 0.18, g: 0.18, b: 0.22, a: 1.0 };
const COLOR_KEY_TEXT: Color = Color { r: 0.30, g: 0.30, b: 0.32, a: 1.0 };
const COLOR_GRID: Color = Color { r: 0.20, g: 0.20, b: 0.24, a: 1.0 };
const COLOR_NOTE: Color = Color { r: 0.45, g: 0.78, b: 0.95, a: 0.95 };
const COLOR_NOTE_SEL: Color = Color { r: 0.95, g: 0.85, b: 0.45, a: 0.95 };
const COLOR_NOTE_BORDER: Color = Color { r: 0.05, g: 0.05, b: 0.08, a: 1.0 };
const COLOR_NOTE_TEXT: Color = Color { r: 0.06, g: 0.08, b: 0.12, a: 1.0 };
const COLOR_PLAYHEAD: Color = Color { r: 0.90, g: 0.30, b: 0.30, a: 1.0 };
const COLOR_VEL_BG: Color = Color { r: 0.13, g: 0.13, b: 0.16, a: 1.0 };
const COLOR_VEL_BAR: Color = Color { r: 0.55, g: 0.78, b: 0.95, a: 0.85 };
const COLOR_HINT: Color = Color { r: 0.55, g: 0.58, b: 0.65, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    // 全体背景
    ui.heavy("pr_bg", |hctx| {
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

    if app.selected_clip.is_none() {
        ui.label_at(
            "pr_no_clip",
            "(\u{30af}\u{30ea}\u{30c3}\u{30d7}\u{304c}\u{9078}\u{629e}\u{3055}\u{308c}\u{3066}\u{3044}\u{307e}\u{305b}\u{3093})",
            area.x + 12.0,
            area.y + 12.0,
            12.0,
            COLOR_HINT,
        );
        return;
    }

    let canvas_area = Rect {
        x: area.x + KEYBOARD_W,
        y: area.y,
        w: area.w - KEYBOARD_W,
        h: area.h - VEL_LANE_H,
    };
    let keyboard_area = Rect {
        x: area.x,
        y: area.y,
        w: KEYBOARD_W,
        h: area.h - VEL_LANE_H,
    };
    let vel_area = Rect {
        x: area.x + KEYBOARD_W,
        y: area.y + area.h - VEL_LANE_H,
        w: area.w - KEYBOARD_W,
        h: VEL_LANE_H,
    };

    draw_canvas(app, ui, canvas_area);
    draw_keyboard(app, ui, keyboard_area);
    draw_velocity_lane(app, ui, vel_area);
    handle_input(app, ui, canvas_area);
}

fn draw_canvas(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let zoom_x = app.pianoroll_zoom_x.max(4.0);
    let zoom_y = app.pianoroll_zoom_y.max(6.0);
    let scroll_beat = app.pianoroll_scroll_beat;
    let top_pitch = app.pianoroll_top_pitch as f32;
    let notes = app.note_boxes();
    let playhead = app.playhead_beat;

    ui.heavy("pr_canvas", |hctx| {
        let key = (
            area.w.to_bits(),
            area.h.to_bits(),
            zoom_x.to_bits(),
            zoom_y.to_bits(),
            scroll_beat.to_bits(),
            top_pitch.to_bits(),
            notes.len(),
            playhead.unwrap_or(f32::NAN).to_bits(),
        );
        hctx.cached(key, |hctx| {
            // 縦線 (拍 / 小節) は library `ui.bar_beat_grid` に委譲 (heavy ブロックの外)。

            // 横線 (semitone)
            let visible_pitch_count = (area.h / zoom_y).ceil() as i32;
            let mut h_lines: Vec<LineSegment> = Vec::new();
            for i in 0..=visible_pitch_count {
                let y = area.y + (i as f32) * zoom_y;
                if y > area.y + area.h {
                    break;
                }
                h_lines.push(LineSegment {
                    a: [area.x, y],
                    b: [area.x + area.w, y],
                    color: COLOR_GRID,
                });
            }
            if !h_lines.is_empty() {
                hctx.push_lines(LineBatch {
                    segments: h_lines,
                    line_width_px: 1.0,
                    clip_rect: Some(area),
                });
            }

            // ノート矩形
            for n in &notes {
                let pitch_offset = top_pitch - n.pitch as f32;
                let y = area.y + pitch_offset * zoom_y;
                if y + zoom_y < area.y || y > area.y + area.h {
                    continue;
                }
                let x = area.x + (n.start_beat - scroll_beat) * zoom_x;
                let w = (n.duration_beats * zoom_x).max(2.0);
                if x + w < area.x || x > area.x + area.w {
                    continue;
                }
                let body = if n.selected { COLOR_NOTE_SEL } else { COLOR_NOTE };
                hctx.push_rect(RectCommand {
                    rect: Rect { x, y: y + 1.0, w, h: zoom_y - 2.0 },
                    fill: body,
                    border: COLOR_NOTE_BORDER,
                    border_width: 1.0,
                    radius: [2.0; 4],
                    clip_rect: Some(area),
                });
                if w > 18.0 && !n.lyric.is_empty() {
                    hctx.label_at(
                        ("pr_note_lyric", n.note as usize),
                        &n.lyric,
                        x + 3.0,
                        y + zoom_y * 0.5 - 5.0,
                        10.0,
                        COLOR_NOTE_TEXT,
                    );
                }
            }

            // playhead
            if let Some(b) = playhead {
                let x = area.x + (b - scroll_beat) * zoom_x;
                if x >= area.x && x <= area.x + area.w {
                    hctx.push_lines(LineBatch {
                        segments: vec![LineSegment {
                            a: [x, area.y],
                            b: [x, area.y + area.h],
                            color: COLOR_PLAYHEAD,
                        }],
                        line_width_px: 1.5,
                        clip_rect: Some(area),
                    });
                }
            }
        });
    });

    // bar / beat grid を library widget で重ねる。半透明 overlay として
    // semitone 線・note 矩形の上に薄く線が乗る。
    let bpm = (app.bpm() as f64).max(1.0);
    let mapping = TimeMapping {
        sample_rate: 48_000.0,
        tempo_bpm: bpm,
        time_sig: (4, 4),
        display: TimeDisplay::BarBeat,
    };
    let spb = mapping.samples_per_beat();
    let visible_beats = (area.w / zoom_x).max(1.0) as f64;
    let viewport = ViewportState1D::new(scroll_beat as f64 * spb, visible_beats * spb);
    ui.bar_beat_grid(
        "pr_grid",
        area,
        mapping,
        viewport,
        BarBeatGridStyle::default(),
    );
}

fn draw_keyboard(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let zoom_y = app.pianoroll_zoom_y.max(6.0);
    let top_pitch = app.pianoroll_top_pitch as i32;
    let visible_pitches = (area.h / zoom_y).ceil() as i32;

    ui.heavy("pr_keyboard", |hctx| {
        hctx.cached(
            (area.w.to_bits(), area.h.to_bits(), zoom_y.to_bits(), top_pitch),
            |hctx| {
                hctx.push_rect(RectCommand {
                    rect: area,
                    fill: COLOR_BG,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
                for i in 0..visible_pitches {
                    let pitch = top_pitch - i;
                    if !(0..=127).contains(&pitch) {
                        continue;
                    }
                    let y = area.y + (i as f32) * zoom_y;
                    let semitone = pitch.rem_euclid(12);
                    let is_black = matches!(semitone, 1 | 3 | 6 | 8 | 10);
                    let fill = if is_black { COLOR_KEY_B } else { COLOR_KEY_W };
                    hctx.push_rect(RectCommand {
                        rect: Rect {
                            x: area.x,
                            y,
                            w: area.w,
                            h: zoom_y - 1.0,
                        },
                        fill,
                        border: COLOR_NOTE_BORDER,
                        border_width: 0.5,
                        radius: [0.0; 4],
                        clip_rect: Some(area),
                    });
                    if semitone == 0 && zoom_y >= 10.0 {
                        let octave = (pitch / 12) - 1;
                        hctx.label_at(
                            ("pr_key_label", pitch),
                            &format!("C{octave}"),
                            area.x + 4.0,
                            y + zoom_y * 0.5 - 5.0,
                            10.0,
                            COLOR_KEY_TEXT,
                        );
                    }
                }
            },
        );
    });
}

fn draw_velocity_lane(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let zoom_x = app.pianoroll_zoom_x.max(4.0);
    let scroll_beat = app.pianoroll_scroll_beat;
    let notes = app.note_boxes();

    ui.heavy("pr_vel_lane", |hctx| {
        let key = (
            area.w.to_bits(),
            area.h.to_bits(),
            zoom_x.to_bits(),
            scroll_beat.to_bits(),
            notes.len(),
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
                let x = area.x + (n.start_beat - scroll_beat) * zoom_x;
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

fn handle_input(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let pointer = ui.pointer();
    let Some((px, py)) = pointer.pos else { return };
    if !area.contains(px, py) {
        return;
    }
    let zoom_x = app.pianoroll_zoom_x.max(4.0);
    let zoom_y = app.pianoroll_zoom_y.max(6.0);
    let scroll_beat = app.pianoroll_scroll_beat;
    let top_pitch = app.pianoroll_top_pitch as i32;
    let modifiers = pointer.modifiers;

    // 矩形選択 (M8 DragRect)。drag 量が 5px 未満なら単発クリックに委譲、
    // それ以上なら範囲内のノートを選択して以降のクリック処理をスキップする。
    let drag_consumed = handle_drag_rect_select(app, ui, area);

    ui.heavy("pr_input", |hctx| {
        let (sx, sy) = pointer.scroll_delta;
        if sy.abs() > 0.001 || sx.abs() > 0.001 {
            if modifiers.ctrl {
                let factor = (sy * 0.005).exp();
                let new_zoom = (zoom_y * factor).clamp(6.0, 40.0);
                hctx.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetPianoRollZoomY(new_zoom))
                }));
            } else if modifiers.shift {
                let dx_beats = -(sx + sy) / zoom_x;
                let new_scroll = (scroll_beat + dx_beats).max(0.0);
                hctx.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetPianoRollScrollX(new_scroll))
                }));
            } else {
                let delta = (sy / 12.0).round() as i32;
                if delta != 0 {
                    let new_top = (top_pitch + delta).clamp(11, 127) as u8;
                    hctx.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetPianoRollTopPitch(new_top))
                    }));
                }
            }
        }

        if pointer.primary_just_released && !drag_consumed {
            let area_x = area.x;
            let area_y = area.y;
            let additive = modifiers.shift;
            // 全ロジックを Edit::mutate 内に閉じ込めて last_click と同時参照。
            hctx.push_edit(Edit::mutate(move |app: &mut AppData| {
                let now = std::time::Instant::now();
                let is_double = matches!(
                    app.last_click,
                    Some((t, lx, ly))
                        if now.duration_since(t)
                            < std::time::Duration::from_millis(400)
                            && (px - lx).abs() < 5.0
                            && (py - ly).abs() < 5.0
                );

                let zoom_x = app.pianoroll_zoom_x.max(4.0);
                let zoom_y = app.pianoroll_zoom_y.max(6.0);
                let scroll_beat = app.pianoroll_scroll_beat;
                let top_pitch = app.pianoroll_top_pitch as i32;

                let beat = ((px - area_x) / zoom_x + scroll_beat) as f64;
                let pitch_offset_rows = ((py - area_y) / zoom_y) as i32;
                let pitch = top_pitch - pitch_offset_rows;
                if !(0..=127).contains(&pitch) {
                    app.last_click = Some((now, px, py));
                    return;
                }

                let Some(target) = app.selected_clip else {
                    app.last_click = Some((now, px, py));
                    return;
                };
                let hit_idx: Option<u32> = app
                    .song
                    .tracks
                    .get(target.track as usize)
                    .and_then(|t| t.clips.get(target.clip as usize))
                    .and_then(|c| {
                        c.notes.iter().enumerate().find_map(|(i, n)| {
                            if n.pitch as i32 == pitch
                                && beat >= n.start_beat
                                && beat <= n.start_beat + n.duration_beats
                            {
                                Some(i as u32)
                            } else {
                                None
                            }
                        })
                    });

                if is_double {
                    if hit_idx.is_some() {
                        // 既存ノートをダブルクリック: 何もしない (将来: ピッチ補正など)
                    } else {
                        // 空白ダブルクリック → AddNote (1/16 grid に snap)
                        let snapped = ((beat * 4.0).floor() / 4.0).max(0.0);
                        app.handle_event(AppEvent::AddNote {
                            track: target.track,
                            clip: target.clip,
                            start_beat: snapped,
                            duration: DEFAULT_NOTE_DURATION,
                            pitch: pitch as u8,
                        });
                    }
                    app.last_click = None;
                } else {
                    if let Some(idx) = hit_idx {
                        app.handle_event(AppEvent::SelectNote { note: idx, additive });
                    } else if !additive {
                        app.handle_event(AppEvent::ClearNoteSelection);
                    }
                    app.last_click = Some((now, px, py));
                }
            }));
        }
    });
}

/// `Ui::take_drag_rect_in_rect` で得た drag を使って矩形選択を実装する。
/// 戻り値: `true` ならこのフレームの release は drag に消費されたので、
/// 単発クリック処理 (handle_input) でスキップするべき。
fn handle_drag_rect_select(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) -> bool {
    let drag_wid = WidgetId::ROOT.child(b"pr_drag_select");
    let Some(drag) = ui.take_drag_rect_in_rect(drag_wid, area) else {
        return false;
    };
    if !drag.finished {
        return false;
    }
    let dx = (drag.end.0 - drag.start.0).abs();
    let dy = (drag.end.1 - drag.start.1).abs();
    if dx < 5.0 && dy < 5.0 {
        // 単発クリック相当 → 通常の release 処理に委譲。
        return false;
    }
    let r = drag.rect();
    let additive = drag.modifiers.shift;
    let zoom_x = app.pianoroll_zoom_x.max(4.0);
    let zoom_y = app.pianoroll_zoom_y.max(6.0);
    let scroll_beat = app.pianoroll_scroll_beat;
    let top_pitch = app.pianoroll_top_pitch as i32;
    let area_x = area.x;
    let area_y = area.y;
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        let Some(target) = app.selected_clip else {
            return;
        };
        let Some(clip) = app
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
        else {
            return;
        };
        let mut new_sel: Vec<u32> = Vec::new();
        for (i, n) in clip.notes.iter().enumerate() {
            let nx = area_x + (n.start_beat as f32 - scroll_beat) * zoom_x;
            let nw = ((n.duration_beats as f32) * zoom_x).max(2.0);
            let pitch_offset = top_pitch - n.pitch as i32;
            let ny = area_y + pitch_offset as f32 * zoom_y;
            let nh = zoom_y;
            let intersects = !(nx + nw < r.x
                || nx > r.x + r.w
                || ny + nh < r.y
                || ny > r.y + r.h);
            if intersects {
                new_sel.push(i as u32);
            }
        }
        let final_sel = if additive {
            let mut merged = app.selected_notes.clone();
            for n in new_sel {
                if !merged.contains(&n) {
                    merged.push(n);
                }
            }
            merged
        } else {
            new_sel
        };
        app.handle_event(AppEvent::SetNoteSelection(final_sel));
    }));
    true
}
