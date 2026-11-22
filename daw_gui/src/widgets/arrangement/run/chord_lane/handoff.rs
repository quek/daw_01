//! 帯とトラックのあいだのドラッグの受け渡し (計画書 §3.1)。
//!
//! - 箱 → トラックのレーン: 箱を掴んだまま下のレーンへ運んで離すと、その箱 (選択ごと) を落とした
//!   トラックの MIDI クリップにする (`ChordToolEvent::ChordsToClip`、voicing は c157-tools の設定)。
//!   レーンの上にいる間は帯の箱を動かさず、落ちる行に箱の時間の幅のゴーストを出す。
//! - クリップ → 帯: アレンジのクリップを掴んで帯の上で離すと、クリップは動かさずにコードを抽出して
//!   帯に置く (`ChordToolEvent::ExtractChords` — MIDI はノートから、オーディオは音源から off-thread、
//!   種類が混ざっていても 1 つ)。帯の上にいる間は帯を縁取る。
//! - エクスプローラの MIDI ファイル → 帯: 落とした拍からコードを置く (`ChordToolEvent::ImportMidiFile`)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::{Color, Rect};

use super::gesture::{ChordDrag, Grab};
use crate::app::{AppData, AppEvent};
use crate::event_chord::ChordToolEvent;
use crate::widgets::arrangement::draw::{push_filled_rect, push_section_border};
use crate::widgets::arrangement::frame::ArrangementFrame;
use crate::widgets::arrangement::geometry::track_index_from_y;
use crate::widgets::arrangement::sessions::ReleasedSessions;
use crate::widgets::arrangement::{ArrangementState, ClipDragKind, MASTER_TRACK_ID, effective_track_row_h};

/// 箱のドラッグがレーンの上にあるなら、落ちるトラック `(id, 行の top, 行の高さ)`。マスター行は落とし先にしない。
pub(super) fn drop_track(f: &ArrangementFrame<'_>, d: &ChordDrag) -> Option<(u32, f32, f32)> {
    let (x, y) = d.last_mouse;
    if d.grab != Grab::Move || !f.lanes.contains(x, y) {
        return None;
    }
    let i = track_index_from_y(y, f.lanes.y, &f.tops)?;
    let t = f.visible_tracks.get(i).filter(|t| t.id != MASTER_TRACK_ID)?;
    Some((t.id, f.tops[i], effective_track_row_h(t, f.view.track_row_h)))
}

/// 箱のドラッグをレーンの上で離した → クリップにする。帯の箱の移動は確定しない (`true` を返す)。
pub(super) fn release_chord_drag(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>, d: &ChordDrag) -> bool {
    let Some((track_id, _, _)) = drop_track(f, d) else { return false };
    let chord_ids: Vec<u32> = d.targets.iter().map(|t| t.0).collect();
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::ChordTool(ChordToolEvent::ChordsToClip { chord_ids, track_id }));
    }));
    true
}

/// 運んでいるクリップのポインタが帯の上か (縁取りに使う)。
pub(super) fn clip_over_band(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) -> bool {
    let s: &mut ArrangementState = ui.widget_state(f.wid);
    s.clip_drag.as_ref().is_some_and(|d| d.kind == ClipDragKind::Move && f.chord_rect.contains(d.last_mouse.0, d.last_mouse.1))
}

/// release フレーム: 帯の上で離したクリップのドラッグを取り上げ (クリップは動かさない)、コードを抽出する。
/// `release::commit_releases` より前に呼ぶ (取り上げないとクリップが一番上のトラックへ動く)。
pub(in crate::widgets::arrangement) fn take_clip_drop(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    released: &mut ReleasedSessions,
) {
    if f.chord_rect.h <= 0.0 {
        return;
    }
    let over = released
        .clip_drag
        .as_ref()
        .is_some_and(|d| d.kind == ClipDragKind::Move && f.chord_rect.contains(d.last_mouse.0, d.last_mouse.1));
    if !over {
        return;
    }
    let Some(d) = released.clip_drag.take() else { return };
    let mut clips: Vec<(u32, u32)> = Vec::new();
    for a in &d.anchors {
        let k = (a.key.track_id, a.key.clip_id);
        if !clips.contains(&k) {
            clips.push(k);
        }
    }
    if clips.is_empty() {
        return;
    }
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::ChordTool(ChordToolEvent::ExtractChords { clips }));
    }));
}

/// エクスプローラから帯へ落としたファイル: MIDI ファイルならコードを検出して落とした拍から置く
/// (`ChordToolEvent::ImportMidiFile`、tick 0 がその拍)。帯に落ちたものはアレンジの取り込みへ流さない。
pub(super) fn take_file_drop(ui: &mut Ui<'_, AppData>, app: &AppData, f: &ArrangementFrame<'_>) {
    if ui.is_file_hovering_in_rect(f.chord_rect) {
        let style = f.style;
        ui.panel_with_border("chord_band_file_drop_hint", f.chord_rect, Color::TRANSPARENT, style.clip_selected_border, 2.0, 0.0);
    }
    let Some(drop) = ui.take_file_drop_in_rect(f.chord_rect) else { return };
    let raw = f.view.start_beat + f64::from(drop.position.0 - f.chord_rect.x) * f.beat_per_px;
    let at = crate::view::snap::arrange_snap_config(app).snap_beat(raw.max(0.0), false, f.zoom_x_px_per_beat).max(0.0);
    let midi: Vec<std::path::PathBuf> = drop.paths.into_iter().filter(|p| crate::midi_import::looks_like_midi(p)).collect();
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        if midi.is_empty() {
            app.ui_ephemeral.status_message = "コードの帯に置けるのは MIDI ファイルだけです".to_string();
        }
        for path in midi {
            app.handle_event(AppEvent::ChordTool(ChordToolEvent::ImportMidiFile { path, at }));
        }
    }));
}

/// レーンの上の落とし先のゴースト (帯の描画より後、レーンの上に乗せる)。
pub(super) fn draw_drop_ghost(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>, d: &ChordDrag) {
    let Some((_, top, h)) = drop_track(f, d) else { return };
    let lo = d.targets.iter().map(|t| t.1).fold(f64::INFINITY, f64::min);
    let hi = d.targets.iter().map(|t| t.1 + t.2).fold(f64::NEG_INFINITY, f64::max);
    if hi <= lo {
        return;
    }
    let x_of = |b: f64| f.lanes.x + ((b - f.view.start_beat) / f.beat_per_px) as f32;
    let r = Rect { x: x_of(lo), y: top + 2.0, w: (x_of(hi) - x_of(lo)).max(2.0), h: (h - 4.0).max(2.0) };
    let style = f.style;
    ui.heavy(("arrangement_chord_drop", &f.id), |hctx| {
        hctx.with_clip_rect(f.lanes, |hctx| {
            push_filled_rect(hctx, r, style.arranger_preview_fill);
            push_section_border(hctx, r, style.clip_selected_border, 1.0);
        });
    });
}

/// クリップを帯の上へ運んでいる間の縁取り色 (選択枠の明線 = どの塗りの上でも立つ)。
pub(super) fn band_drop_border(f: &ArrangementFrame<'_>) -> Color {
    f.style.clip_selected_border
}
