//! クリップ編集面 (Audio Editor / Automation Editor) の ruler。
//! 曲全体の絶対 bar 番号を出し、arrangement と同じ操作で seek / ループ範囲を動かす。

use std::hash::Hash;

use common::time::{TimeDisplay, TimeMapping};
use daw_ui_core::{DragKind, Edit, Ui, ViewportState1D};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::widgets::time_grid::{TimeGridExt, TimeRulerStyle};

/// ruler が映す範囲。`start_song` は左端の song 拍。
#[derive(Clone, Copy)]
pub(crate) struct RulerView {
    pub start_song: f64,
    pub len_beats: f64,
}

/// 描画 + 操作 (plain drag = 連続 seek / Shift+drag = release でループ範囲)。
/// `interactive = false` なら表示だけ (セルは song の時間軸に居ないので seek の意味が無い)。
pub(crate) fn draw(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    id: impl Hash + Copy,
    rect: Rect,
    view: RulerView,
    interactive: bool,
) {
    if rect.w <= 0.0 || view.len_beats <= 0.0 {
        return;
    }
    let RulerView { start_song, len_beats } = view;
    let p = &app.theme.core;
    let song = app.cur.song_doc.song();
    let mapping = TimeMapping {
        sample_rate: f64::from(app.ipc.sample_rate),
        tempo_bpm: f64::from(song.bpm),
        time_sig: song.time_sig,
        display: TimeDisplay::BarBeat,
    };
    let spb = mapping.samples_per_beat();
    let viewport = ViewportState1D { view_start: start_song * spb, view_len: len_beats * spb };
    ui.time_ruler(("clip_ruler", id), rect, mapping, viewport, TimeRulerStyle::from_palette(p));
    if !interactive {
        return;
    }

    // ----- 既存 loop region overlay (ruler 上に半透明バンド) -----
    // arrangement view と同色の cyan、 view 範囲との交差のみ描画。
    if let Some((lstart, lend)) = app.cur.transport.loop_region.range() {
        let visible_start = lstart.max(start_song);
        let visible_end = lend.min(start_song + len_beats);
        if visible_end > visible_start {
            #[allow(clippy::cast_possible_truncation)]
            let nstart = ((visible_start - start_song) / len_beats) as f32;
            #[allow(clippy::cast_possible_truncation)]
            let nend = ((visible_end - start_song) / len_beats) as f32;
            ui.push_rect(daw_ui_renderer::RectCommand {
                rect: Rect {
                    x: rect.x + nstart * rect.w,
                    y: rect.y,
                    w: ((nend - nstart) * rect.w).max(1.0),
                    h: rect.h,
                },
                fill: p.loop_band.with_alpha(0.18),
                border: p.loop_band.with_alpha(0.55),
                border_width: 1.0,
                radius: [0.0; 4],
                clip_rect: Some(rect),
            });
        }
    }

    // ----- 操作 (arrangement と同じ操作感) -----
    let Some(drag) = ui.take_drag_in_rect(("clip_ruler_drag", id), rect) else {
        return;
    };
    let to_song_beat = |x_px: f32| -> f64 {
        let local_x = (x_px - rect.x).clamp(0.0, rect.w);
        (start_song + f64::from(local_x / rect.w) * len_beats).max(0.0)
    };
    let cur_beat = to_song_beat(drag.current.0);
    if drag.start_modifiers.shift {
        // Shift+drag: release frame に commit。
        if drag.kind == DragKind::Released {
            let anchor_beat = to_song_beat(drag.anchor.0);
            let (start, end) =
                if anchor_beat <= cur_beat { (anchor_beat, cur_beat) } else { (cur_beat, anchor_beat) };
            if (end - start).abs() > 1e-6 {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetLoopRange { start, end });
                }));
            }
        }
    } else if drag.kind != DragKind::Released {
        // 素 drag / 単発 click: Started + Continuing で連続 seek。
        // seek_playhead_to は「停止で戻るホーム」も更新する。
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.seek_playhead_to(cur_beat);
        }));
    }
}
