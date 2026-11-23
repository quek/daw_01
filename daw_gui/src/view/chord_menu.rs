//! コードトラックの帯の右クリックメニュー (r.md #157 §3.1)。`ProjectEphemeral::chord_band.menu` に
//! 帯が積んだ `(右クリックした箱, pos)` を使い、毎フレーム `context_menu_at` で描く
//! (`render_section_menu_overlay` と同じ idiom: open は 1-shot)。箱の上なら箱と選択への操作、
//! 空きなら挿入と全体への操作。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, ColorPickerTarget, EditSurface};
use crate::event_chord::ChordTrackEvent;

/// ↑↓ = 選んでいる箱を半音上 / 下へ移調する。**対象面が帯 (`EditSurface::Chords`) のときだけ消費する** —
/// ノートの ↑↓ (`root::dispatch_note_nudge`) と同じバインドを面で振り分ける (last-selection-wins、
/// `AppData::edit_surface`)。Shift+↑↓ (ノートのオクターブ) はコードには割り当てない。
pub(crate) fn dispatch_keys(ui: &mut Ui<'_, AppData>, surface: Option<EditSurface>) {
    if !matches!(surface, Some(EditSurface::Chords)) {
        return;
    }
    // 押しっぱなしで 1 フレームに複数回積まれる分をまとめる (上限は `dispatch_note_nudge` と同じ)。
    const MAX_REPEATS_PER_FRAME: usize = 64;
    for (name, dir) in [("daw.nudge_note_up", 1_i32), ("daw.nudge_note_down", -1)] {
        let n = i32::try_from(ui.take_shortcut_count(name).min(MAX_REPEATS_PER_FRAME)).unwrap_or(0);
        if n > 0 {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| app.nudge_selected_chords(dir * n)));
        }
    }
}

/// 箱の上のメニュー。並びは [`run_box_item`] の index と対。
const BOX_ITEMS: [&str; 8] = [
    "色...",
    "同じコードを全部選択",
    "選択を半音上へ移調",
    "選択を半音下へ移調",
    "選択を削除",
    "プリセットに保存...",
    "プリセットを挿入...",
    "全部削除",
];
/// 空きのメニュー。並びは [`run_empty_item`] の index と対。
const EMPTY_ITEMS: [&str; 3] = ["プリセットを挿入...", "プリセットに保存...", "全部削除"];

pub(crate) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>) {
    let band = &app.cur.peph.chord_band;
    let Some((target, pos)) = band.menu else { return };
    let open_at = band.menu_open.then_some(pos);
    if band.menu_open {
        ui.push_edit(Edit::mutate(|app: &mut AppData| app.cur.peph.chord_band.menu_open = false));
    }
    // 挿入位置 = 右クリックした拍 (帯と同じ snap)。
    let lane = band.lane;
    let snap = crate::view::snap::arrange_snap_config(app);
    let at = snap.snap_beat(lane.beat_of(pos.0).max(0.0), false, app.cur.view.arrange_zoom_x).max(0.0);
    let items: &[&str] = if target.is_some() { &BOX_ITEMS } else { &EMPTY_ITEMS };
    ui.context_menu_at("arrange_chord_menu", open_at, items, move |idx, ui| {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            match target {
                Some(id) => run_box_item(app, idx, id, pos, at),
                None => run_empty_item(app, idx, at),
            }
            app.cur.peph.chord_band.menu = None;
        }));
    });
}

fn run_box_item(app: &mut AppData, idx: usize, id: u32, pos: (f32, f32), at: f64) {
    match idx {
        0 => app.open_color_picker(ColorPickerTarget::Chord(id), Rect { x: pos.0, y: pos.1, w: 1.0, h: 1.0 }),
        1 => app.select_same_chords(id),
        2 => app.transpose_selected_chords(1),
        3 => app.transpose_selected_chords(-1),
        4 => {
            let ids = app.live_chord_ids();
            app.handle_event(AppEvent::Chord(ChordTrackEvent::Delete { ids }));
        }
        5 => app.begin_save_chord_preset(),
        6 => app.begin_insert_chord_preset(at),
        7 => app.handle_event(AppEvent::Chord(ChordTrackEvent::Clear)),
        _ => {}
    }
}

fn run_empty_item(app: &mut AppData, idx: usize, at: f64) {
    match idx {
        0 => app.begin_insert_chord_preset(at),
        1 => app.begin_save_chord_preset(),
        2 => app.handle_event(AppEvent::Chord(ChordTrackEvent::Clear)),
        _ => {}
    }
}
