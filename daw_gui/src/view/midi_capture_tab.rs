//! 下部パネル「MIDI Capture」タブ (`docs/plan_global_sampler.md` §3.4)。
//!
//! 横 = wall-clock (右端 = 今、幅 = 設定秒数)、縦 = ピッチ。MIDI 入力の全ノートを
//! 常時溜めたものを矩形で描く。押しっぱなしは右端まで伸びる。再生していた区間は
//! Sampler と同じセグメント (同じ時計) から小節線を重ねる。
//! 範囲選択 / 持ち出し ([`MIDI_CAPTURE_DRAG_KIND`]) の操作は Sampler タブと同じ。

use std::sync::Arc;

use daw_ui_core::{DragKind, Edit, Ui};
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::app::{AppData, AppEvent};
use crate::event_sampler::SamplerEvent;
use crate::state::midi_capture::{MIDI_CAPTURE_DRAG_KIND, MidiCaptureDragPayload, WallAxis};
use crate::state::sampler::{segment_spans, wall_clock_ns};
use crate::view::sampler_tab::{
    BarSource, HEADER_H, draw_bar_lines, draw_seconds_ruler, pause_and_preview, seconds_field,
};

const PAD: f32 = 6.0;
const FONT: f32 = 12.0;
const RULER_H: f32 = 14.0;
/// 縦に必ず見せる最小のピッチ幅 (半音)。
const MIN_PITCH_SPAN: i32 = 24;
/// ピッチ範囲の上下余白 (半音)。
const PITCH_MARGIN: i32 = 3;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let p = &app.theme.core;
    ui.panel("midi_capture_bg", area, p.panel, 0.0);
    let header = Rect { x: area.x, y: area.y, w: area.w, h: HEADER_H };
    let body = Rect {
        x: area.x + PAD,
        y: area.y + HEADER_H,
        w: (area.w - PAD * 2.0).max(1.0),
        h: (area.h - HEADER_H - PAD).max(1.0),
    };
    draw_header(app, ui, header);

    let st = &app.midi_capture;
    let now = wall_clock_ns();
    let axis = WallAxis {
        x: body.x,
        w: body.w,
        now_ns: now,
        span_ns: u64::from(app.sampler_seconds()) * 1_000_000_000,
    };
    let grid = Rect { x: body.x, y: body.y, w: body.w, h: (body.h - RULER_H).max(1.0) };
    ui.push_rect(RectCommand {
        rect: body,
        fill: p.inset_bg,
        border: p.border,
        border_width: 1.0,
        radius: [3.0; 4],
        clip_rect: None,
    });
    draw_bar_lines(app, ui, grid, |ns| axis.ns_to_x(ns), &wall_bar_source(app));
    draw_seconds_ruler(
        app,
        ui,
        body,
        RULER_H,
        |secs_ago| axis.ns_to_x(now.saturating_sub((secs_ago * 1e9) as u64)),
        f64::from(app.sampler_seconds()),
    );
    draw_notes(app, ui, grid, &axis, now);

    // ---- 選択 / 持ち出し ----
    let sel_rect = st.selection.map(|(s, e)| Rect {
        x: axis.ns_to_x(s),
        y: grid.y,
        w: (axis.ns_to_x(e) - axis.ns_to_x(s)).max(1.0),
        h: grid.h,
    });
    if let Some(r) = sel_rect {
        ui.push_rect(RectCommand {
            rect: r,
            fill: p.accent_wash,
            border: p.accent,
            border_width: 1.0,
            radius: [0.0; 4],
            clip_rect: Some(grid),
        });
    }
    let pointer = ui.pointer();
    let press_in_sel = pointer.primary_just_pressed
        && pointer.pos.is_some_and(|(px, py)| sel_rect.is_some_and(|r| r.contains(px, py)));
    if press_in_sel {
        if let (Some(_), Some((s, e))) = (ui.take_primary_press_in_rect(grid), st.selection) {
            ui.begin_drag(MIDI_CAPTURE_DRAG_KIND, MidiCaptureDragPayload { start_ns: s, end_ns: e });
        }
    } else if let Some(d) = ui.take_drag_in_rect("midi_capture_select", grid) {
        let a = axis.x_to_ns(d.anchor.0);
        let b = axis.x_to_ns(d.current.0);
        let sel = if d.kind == DragKind::Released && (d.current.0 - d.anchor.0).abs() < 2.0 {
            None
        } else {
            Some((a.min(b), a.max(b)))
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Sampler(SamplerEvent::SetMidiSelection(sel)));
        }));
    }
}

fn draw_header(app: &AppData, ui: &mut Ui<'_, AppData>, header: Rect) {
    let p = &app.theme.core;
    let y = header.y + (HEADER_H - 22.0) / 2.0;
    let mut x = header.x + PAD;
    let port = app.recording.midi_input_label.as_str();
    let port_label = if port.is_empty() { "MIDI 入力なし".to_string() } else { format!("入力: {port}") };
    ui.label_at("midi_capture_port", &port_label, x, y + 5.0, FONT, p.text_dim);
    x += ui.measure_text(&port_label, FONT) + PAD * 3.0;
    x = seconds_field(app, ui, x, y, "midi_capture_secs");
    x = pause_and_preview(
        ui,
        p,
        x,
        y,
        ("midi_capture_pause", app.midi_capture.paused, AppEvent::Sampler(SamplerEvent::ToggleMidiPaused)),
        (
            "midi_capture_preview",
            app.midi_capture.preview_until.is_some(),
            app.midi_capture.selection.is_some(),
            AppEvent::Sampler(SamplerEvent::ToggleMidiPreview),
        ),
    );
    let text = match app.midi_capture.selection {
        Some((s, e)) => {
            let n = app.midi_capture.notes_in(s, e, wall_clock_ns()).count();
            format!("選択 {:.2} s / {n} ノート — アレンジ / セルへドラッグ", (e - s) as f64 / 1e9)
        }
        None => format!("{} ノート · ドラッグで範囲を選ぶ", app.midi_capture.notes.len()),
    };
    ui.label_at("midi_capture_status", &text, x, y + 5.0, FONT, p.text_dim);
}

/// 再生していた区間を wall-clock 座標で (`samples_per_unit` = ns → samples)。
fn wall_bar_source(app: &AppData) -> BarSource {
    let st = &app.sampler;
    let sr = f64::from(st.sample_rate().max(1));
    let spans = segment_spans(&st.segments, st.write_frames)
        .into_iter()
        .filter_map(|(s, e, seg)| {
            let ph = seg.playhead_samples?;
            let dur_ns = ((e - s) as f64 * 1e9 / sr) as u64;
            Some((seg.wall_ns, seg.wall_ns + dur_ns, ph))
        })
        .collect();
    BarSource { spans, samples_per_unit: sr / 1e9 }
}

fn draw_notes(app: &AppData, ui: &mut Ui<'_, AppData>, grid: Rect, axis: &WallAxis, now: u64) {
    let p = &app.theme.core;
    let st = &app.midi_capture;
    let oldest = axis.oldest();
    let visible: Vec<_> = st.notes_in(oldest, now, now).collect();
    let (lo, hi) = visible
        .iter()
        .fold((i32::MAX, i32::MIN), |(lo, hi), n| (lo.min(i32::from(n.pitch)), hi.max(i32::from(n.pitch))));
    let (lo, hi) = if visible.is_empty() { (48, 72) } else { (lo - PITCH_MARGIN, hi + PITCH_MARGIN) };
    let (lo, hi) = widen_span(lo, hi);
    let rows = (hi - lo + 1) as f32;
    let row_h = grid.h / rows;
    // オクターブ線 (C)。
    let mut lines = Vec::new();
    for pitch in lo..=hi {
        if pitch % 12 == 0 {
            let y = grid.y + grid.h - (pitch - lo + 1) as f32 * row_h;
            lines.push(LineSegment { a: [grid.x, y], b: [grid.x + grid.w, y], color: p.border });
        }
    }
    if !lines.is_empty() {
        ui.push_lines(LineBatch { segments: Arc::from(lines), line_width_px: 1.0, clip_rect: Some(grid) });
    }
    let fill = p.accent;
    let held = app.theme.daw.play;
    for n in visible {
        let x0 = axis.ns_to_x(n.on_ns.max(oldest));
        let x1 = axis.ns_to_x(n.end_ns(now));
        let y = grid.y + grid.h - (i32::from(n.pitch) - lo + 1) as f32 * row_h;
        let alpha = 0.55 + 0.45 * f32::from(n.velocity) / 127.0;
        ui.push_rect(RectCommand {
            rect: Rect { x: x0, y: y + 0.5, w: (x1 - x0).max(2.0), h: (row_h - 1.0).max(1.0) },
            fill: if n.off_ns.is_none() { held.with_alpha(alpha) } else { fill.with_alpha(alpha) },
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [1.0; 4],
            clip_rect: Some(grid),
        });
    }
    if st.paused {
        let r = Rect { x: grid.x + grid.w - 90.0, y: grid.y + 4.0, w: 84.0, h: 18.0 };
        ui.push_rect(RectCommand { rect: r, fill: app.theme.daw.record.with_alpha(0.85), border: Color::TRANSPARENT, border_width: 0.0, radius: [3.0; 4], clip_rect: None });
        ui.label_at("midi_capture_paused_badge", "一時停止中", r.x + 10.0, r.y + 3.0, FONT, p.ink_for(app.theme.daw.record));
    }
}

/// ピッチ範囲を最低 [`MIN_PITCH_SPAN`] 半音に広げ、0..=127 に収める。
fn widen_span(lo: i32, hi: i32) -> (i32, i32) {
    let mut lo = lo;
    let mut hi = hi;
    while hi - lo + 1 < MIN_PITCH_SPAN {
        if lo > 0 {
            lo -= 1;
        }
        if hi - lo + 1 < MIN_PITCH_SPAN && hi < 127 {
            hi += 1;
        }
        if lo == 0 && hi == 127 {
            break;
        }
    }
    (lo.max(0), hi.min(127))
}
