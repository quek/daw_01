//! 帯の入力 (press / 継続 / release / ダブルクリック / 右クリック / カーソル)。
//!
//! - 箱: 中央のドラッグで移動 (選択に含まれれば選択ごと、相対位置を保つ)、Ctrl+ドラッグで複製、
//!   端のドラッグで伸縮 (下半分は接している隣と境界ごと、Alt は選択全体の比例伸縮)、
//!   短クリックで選択 (Shift = 範囲 / Ctrl = 切替)。
//! - 空き: ドラッグで範囲にかかる箱を選ぶ (Shift = 足す / Ctrl = 切替)、短クリックで選択を外す、
//!   ダブルクリックで 1 小節 (時間範囲があればその長さ) の箱を作ってポップアップを開く。
//! - 箱のダブルクリックでポップアップ、右クリックでメニュー。

use daw_ui_core::{CursorIcon, Edit, Ui, WidgetId};
use daw_ui_renderer::Rect;

use super::gesture::{ChordDrag, EdgeMode, Grab, RangeDrag, Resolved, Scale};
use super::{ChordBox, ChordLaneState, Live, box_rect};
use crate::app::{AppData, AppEvent};
use crate::event_chord::ChordTrackEvent;
use crate::handler::chord_band::RangeSelectMode;
use crate::widgets::arrangement::frame::ArrangementFrame;
use crate::widgets::arrangement::geometry::{
    beats_per_bar, header_resize_splitter_at, px_to_beat, section_zone_at,
};
use crate::widgets::arrangement::ClipDragKind;
use crate::widgets::select_modifier::SelectModifier;

pub(super) fn state_id(f: &ArrangementFrame<'_>) -> WidgetId {
    f.wid.child(b"chord_lane")
}

fn scale<'a>(f: &'a ArrangementFrame<'_>) -> Scale<'a> {
    Scale { beat_per_px: f.beat_per_px, snap: &f.view.snap, zoom: f.zoom_x_px_per_beat }
}

fn chord_edit(ev: ChordTrackEvent) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::Chord(ev)))
}

/// `x` に当たる箱と掴む場所。`section_hit` と同じ 2 段 (箱の内側を、端の外側拡張しか当たらない
/// 箱より無条件に優先し、同じ段なら端に近い方) — 接している 2 箱の境界で右の箱の左端が
/// 左の箱の右端を奪わない (ui/CLAUDE.md「隣接 resize widget の共有境界は in-rect 優先」)。
fn hit<'v>(f: &'v ArrangementFrame<'_>, x: f32) -> Option<(&'v ChordBox, Grab)> {
    let mut best: Option<(&ChordBox, Grab)> = None;
    let mut best_inside = false;
    let mut best_dist = f32::INFINITY;
    for b in &f.chord.boxes {
        let r = box_rect(b.start, b.len, f);
        let Some(kind) = section_zone_at(r, x, f.style.resize_handle_px) else { continue };
        let inside = x >= r.x && x < r.x + r.w;
        let (grab, edge_x) = match kind {
            ClipDragKind::ResizeLeft => (Grab::Left, r.x),
            ClipDragKind::ResizeRight => (Grab::Right, r.x + r.w),
            ClipDragKind::Move => (Grab::Move, x),
        };
        let dist = (x - edge_x).abs();
        if (inside == best_inside && dist <= best_dist) || (inside && !best_inside) {
            best = Some((b, grab));
            best_inside = inside;
            best_dist = dist;
        }
    }
    best
}

/// 箱の内側 (端の外側拡張を含まない) — ダブルクリック / 右クリックの対象 (`section_at_inrect` と同じ)。
fn box_at<'v>(f: &'v ArrangementFrame<'_>, x: f32) -> Option<&'v ChordBox> {
    f.chord.boxes.iter().find(|b| {
        let r = box_rect(b.start, b.len, f);
        x >= r.x && x < r.x + r.w
    })
}

/// 押したまま Esc = ドラッグを捨てる (r.md #127)。帯は release で確定するので捨てるだけ。
pub(super) fn escape(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    if ui.drag_cancel_requested() {
        let s: &mut ChordLaneState = ui.widget_state(state_id(f));
        s.drag = None;
        s.range = None;
    }
}

pub(super) fn press(ui: &mut Ui<'_, AppData>, app: &AppData, f: &ArrangementFrame<'_>) {
    if !f.pointer.primary_just_pressed {
        return;
    }
    let Some((px, py)) = f.pointer.pos else { return };
    // ポップアップ / メニューが開いている間の press はそれを閉じるだけ (背景へ漏らさない)。
    if !f.chord_rect.contains(px, py) || ui.has_open_popups() || header_resize_splitter_at(f.rect, f.header_w, f.style, px, py) {
        return;
    }
    let m = f.pointer.modifiers;
    let selected = &app.cur.selection.selected_chord_ids;
    let mut select_now: Option<u32> = None;
    let session = if let Some((b, grab)) = hit(f, px) {
        let in_selection = selected.contains(&b.id);
        let targets: Vec<(u32, f64, f64)> = if in_selection {
            f.chord.boxes.iter().filter(|c| selected.contains(&c.id)).map(|c| (c.id, c.start, c.len)).collect()
        } else {
            vec![(b.id, b.start, b.len)]
        };
        let edge = if grab == Grab::Move {
            EdgeMode::Single
        } else {
            let lower_half = py >= f.chord_rect.y + f.chord_rect.h * 0.5;
            super::gesture::edge_mode(f.chord, b, grab, lower_half, m.alt, selected)
        };
        // 選択の外の箱を素で掴んだら、その場でその箱だけを選ぶ (クリップと同じ)。
        if !in_selection && !m.shift && !m.ctrl {
            select_now = Some(b.id);
        }
        Some(ChordDrag {
            grab,
            id: b.id,
            anchor: (b.start, b.len),
            targets,
            edge,
            anchor_mouse: (px, py),
            last_mouse: (px, py),
            last_alt: m.alt,
            last_ctrl: m.ctrl,
            press_shift: m.shift,
            press_ctrl: m.ctrl,
        })
    } else {
        None
    };
    {
        let s: &mut ChordLaneState = ui.widget_state(state_id(f));
        if let Some(d) = session {
            s.drag = Some(d);
        } else {
            s.range = Some(RangeDrag {
                anchor_beat: px_to_beat(px, f.chord_rect.x, f.chord_rect.w, f.view).max(0.0),
                anchor_mouse: (px, py),
                last_mouse: (px, py),
                press_shift: m.shift,
                press_ctrl: m.ctrl,
            });
        }
    }
    ui.claim_press(state_id(f));
    if let Some(id) = select_now {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| app.select_chord(id, SelectModifier::Single)));
    }
}

/// 継続フレームで `last_*` を進める。release フレームは press 位置への巻き戻し (winit) を捨て、
/// modifier は直前の値を保つ (`drag::advance` と同じ規則)。
pub(super) fn advance(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let p = f.pointer;
    let s: &mut ChordLaneState = ui.widget_state(state_id(f));
    if !p.primary_pressed && !p.primary_just_released {
        // release を取りこぼした (窓の外で離した等) — 確定せず捨てる。
        s.drag = None;
        s.range = None;
        return;
    }
    let Some((px, py)) = p.pos else { return };
    let is_release = p.primary_just_released;
    let moved = |anchor: f32| !is_release || (px - anchor).abs() > f32::EPSILON;
    if let Some(d) = s.drag.as_mut() {
        if moved(d.anchor_mouse.0) {
            d.last_mouse = (px, py);
        }
        if !is_release {
            d.last_alt = p.modifiers.alt;
            d.last_ctrl = p.modifiers.ctrl;
        }
    }
    if let Some(r) = s.range.as_mut()
        && moved(r.anchor_mouse.0)
    {
        r.last_mouse = (px, py);
    }
}

/// ドラッグ中にポインタが左右の端へ寄ったら横へ送る (Arranger 帯のドラッグと同じ横だけの端オートスクロール、
/// `drag::edge_auto_scroll` と同じ量と起動条件)。送った分だけ session の起点を逆へずらし、掴んだ箱を
/// ポインタに付いてこさせる。範囲は帯からレーンの下端まで (箱をレーンへ運んでいる間も送る)。
pub(super) fn edge_scroll(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    use daw_ui_core::widgets::edge_scroll::{ACTIVATE_PX, EdgeScrollCfg, edge_scroll_delta};
    let Some((px, py)) = f.pointer.pos else { return };
    if f.pointer.primary_just_released || f.beat_per_px <= 1e-6 {
        return;
    }
    let anchor = {
        let s: &mut ChordLaneState = ui.widget_state(state_id(f));
        s.drag.as_ref().map(|d| d.anchor_mouse).or(s.range.map(|r| r.anchor_mouse))
    };
    let Some(anchor) = anchor else { return };
    if (px - anchor.0).hypot(py - anchor.1) < ACTIVATE_PX {
        return;
    }
    let zone = Rect { x: f.lanes.x, y: f.chord_rect.y, w: f.lanes.w, h: f.lanes.y + f.lanes.h - f.chord_rect.y };
    let (dx, _) = edge_scroll_delta(f.pointer.pos, zone, EdgeScrollCfg::default(), true, false);
    if dx == 0.0 {
        return;
    }
    let new_start = (f.view.scroll_beat_raw + f64::from(dx) * f.beat_per_px).max(0.0);
    let applied = ((new_start - f.view.scroll_beat_raw) / f.beat_per_px) as f32;
    if applied == 0.0 {
        return;
    }
    ui.push_edit(Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::AutoScrollArrange(new_start as f32))));
    let s: &mut ChordLaneState = ui.widget_state(state_id(f));
    if let Some(d) = s.drag.as_mut() {
        d.anchor_mouse.0 -= applied;
    }
    if let Some(r) = s.range.as_mut() {
        r.anchor_mouse.0 -= applied;
    }
    ui.request_redraw();
}

/// release フレームで取り出した session。
pub(super) struct Released {
    drag: Option<ChordDrag>,
    range: Option<RangeDrag>,
}

pub(super) fn take(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) -> (Live, Released) {
    let release = f.pointer.primary_just_released;
    let s: &mut ChordLaneState = ui.widget_state(state_id(f));
    let live = Live { drag: s.drag.clone(), range: s.range };
    let released = if release {
        Released { drag: s.drag.take(), range: s.range.take() }
    } else {
        Released { drag: None, range: None }
    };
    (live, released)
}

pub(super) fn release(ui: &mut Ui<'_, AppData>, app: &AppData, f: &ArrangementFrame<'_>, r: Released) {
    if let Some(d) = r.drag {
        if super::handoff::release_chord_drag(ui, app, f, &d) {
            // レーンの上で離した = クリップにした / 落とせない行だった (どちらも帯の箱は動かさない)。
        } else if d.is_click() {
            let modifier = SelectModifier::from_modifiers(d.press_shift, d.press_ctrl);
            let id = d.id;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.select_chord(id, modifier);
                app.audition_chord_box(id);
            }));
        } else {
            let ev = match d.resolve(scale(f)) {
                Resolved::Move { ids, delta, duplicate: true } => ChordTrackEvent::Duplicate { ids, delta },
                Resolved::Move { ids, delta, duplicate: false } => ChordTrackEvent::Move { ids, delta },
                Resolved::Spans(spans) => ChordTrackEvent::SetSpans { spans },
            };
            ui.push_edit(chord_edit(ev));
        }
    }
    if let Some(rd) = r.range {
        let mode = RangeSelectMode::from_modifiers(rd.press_shift, rd.press_ctrl);
        let ids = if rd.is_click() { Vec::new() } else { rd.hits(f.chord, scale(f)) };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| app.select_chords_in_range(ids, mode)));
    }
}

pub(super) fn double_click(ui: &mut Ui<'_, AppData>, app: &AppData, f: &ArrangementFrame<'_>) {
    let Some((cx, _)) = ui.take_double_click_in_rect(f.chord_rect) else { return };
    if let Some(b) = box_at(f, cx) {
        let id = b.id;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| app.open_chord_popup(id)));
        return;
    }
    let raw = px_to_beat(cx, f.chord_rect.x, f.chord_rect.w, f.view).max(0.0);
    let start = f.view.snap.snap_beat(raw, f.pointer.modifiers.alt, f.zoom_x_px_per_beat).max(0.0);
    // 時間範囲を引いていればその長さ、無ければ 1 小節。
    let len = app
        .cur
        .selection
        .time
        .as_ref()
        .map(|t| t.end_beat - t.start_beat)
        .filter(|l| *l > 0.0)
        .unwrap_or_else(|| beats_per_bar(f.view.time_sig));
    ui.push_edit(Edit::mutate(move |app: &mut AppData| app.create_chord_box(start, len)));
}

pub(super) fn secondary_click(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let Some((cx, cy)) = ui.take_secondary_click_in_rect(f.chord_rect) else { return };
    let id = box_at(f, cx).map(|b| b.id);
    ui.push_edit(Edit::mutate(move |app: &mut AppData| app.open_chord_menu(id, (cx, cy))));
}

pub(super) fn cursor(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>, live: &Live) {
    let grab = if let Some(d) = live.drag.as_ref() {
        Some(d.grab)
    } else if let Some((x, y)) = f.hover_pos
        && f.chord_rect.contains(x, y)
        && live.range.is_none()
    {
        hit(f, x).map(|(_, g)| g)
    } else {
        None
    };
    match grab {
        Some(Grab::Move) => ui.set_cursor(CursorIcon::Move),
        Some(Grab::Left | Grab::Right) => ui.set_cursor(CursorIcon::EwResize),
        None => {}
    }
}
