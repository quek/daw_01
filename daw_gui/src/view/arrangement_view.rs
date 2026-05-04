//! Arrangement view (track headers / ruler / lanes / clip drag) を gui_01 の
//! `Ui::arrangement` widget 1 呼び出しに集約。
//!
//! AppData は引き続き track / clip を index ベースで持つので、ここで stable id
//! (Track.id / Clip.id) と index の変換層を担う。

use std::sync::Arc;

use daw_ui_core::{
    ArrangementClip, ArrangementEditRequest, ArrangementStyle, ArrangementTrack,
    ArrangementView, ClipKey, Edit, Ui,
};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent, ClipRef};
use crate::view::mixer_strips::{amp_to_fader, fader_to_amp};

const TRACK_HEADER_W: f32 = 160.0;
const RULER_H: f32 = 20.0;

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let tracks: Vec<ArrangementTrack> = app
        .song
        .tracks
        .iter()
        .map(|t| ArrangementTrack {
            id: t.id,
            name: Arc::from(t.name.as_str()),
            muted: t.muted,
            solo: t.solo,
            // mixer fader と同じ dB スケール (DB_MIN..DB_MAX) で表示する。
            // 編集 callback で受け取った値は fader_to_amp で linear に戻す。
            volume: amp_to_fader(t.volume),
            clips: t
                .clips
                .iter()
                .map(|c| ArrangementClip {
                    id: c.id,
                    start_beat: c.start_beat,
                    len_beats: c.length_beats,
                    name: Arc::from(c.name.as_str()),
                    color: None,
                })
                .collect(),
        })
        .collect();

    // selected_clips: ClipRef (index ベース) → ClipKey (id ベース) 変換。
    // 範囲外の参照は filter_map で取り除く。
    let selected_clips: Vec<ClipKey> = app
        .selected_clips
        .iter()
        .filter_map(|r| {
            let t = app.song.tracks.get(r.track as usize)?;
            let c = t.clips.get(r.clip as usize)?;
            Some(ClipKey { track: t.id, clip: c.id })
        })
        .collect();

    let selected_track_id = app
        .song
        .tracks
        .get(app.selected_track as usize)
        .map(|t| t.id);

    let zoom = app.arrange_zoom_x.max(1.0);
    let row_h = app.arrange_track_row_h.max(1.0);
    let lanes_w = (area.w - TRACK_HEADER_W).max(1.0);
    let loop_range = if app.song.loop_end_beat > app.song.loop_start_beat {
        Some((app.song.loop_start_beat, app.song.loop_end_beat))
    } else {
        None
    };
    // data_generation: schema 編集を反映する粗粒度 hash。track 数 + clip 総数 +
    // 各 track の name 長さ和 + track 並び順 (id × position) を含めることで
    // reorder でも bump する (selection / drag / playhead では bump しない)。
    let data_generation = (app.song.tracks.len() as u64).wrapping_mul(0x10000)
        + app
            .song
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                ((i as u64).wrapping_mul(31).wrapping_add(t.id as u64 + 1))
                    .wrapping_mul(0x100)
                    + (t.clips.len() as u64)
                    + (t.name.len() as u64)
                    // M10 Phase 47b: track volume も `data_generation` 因子に
                    // (band 表示更新のため、float→bits hash で bump する)。
                    + (t.volume.to_bits() as u64)
            })
            .sum::<u64>();

    let view = ArrangementView {
        start_beat: app.arrange_scroll_beat as f64,
        len_beats: (lanes_w / zoom) as f64,
        track_top: 0.0,
        tracks_visible: ((area.h - RULER_H) / row_h).max(1.0),
        track_row_h: row_h,
        header_w: TRACK_HEADER_W,
        ruler_h: RULER_H,
        playhead_beat: app.playhead_beat.map(|b| b as f64),
        loop_range,
        data_generation,
    };

    let style = ArrangementStyle::default();

    let resp = ui.arrangement(
        "arrangement",
        area,
        &tracks,
        view,
        &selected_clips,
        selected_track_id,
        &style,
        make_edit,
    );

    // track header の右クリックメニュー (Rename / Delete) を widget 外で重ねる。
    // widget は track_header_rects と BeginRenameTrack / DeleteTrack の発行までを担う。
    for (track_id, rect) in resp.track_header_rects {
        ui.context_menu_for(rect, &["Rename", "Delete"], move |idx, ui| {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                let Some(t_idx) = app.song.tracks.iter().position(|t| t.id == track_id)
                else {
                    return;
                };
                match idx {
                    0 => app.handle_event(AppEvent::BeginRenameTrack(t_idx as u32)),
                    1 => app.handle_event(AppEvent::DeleteTrack(t_idx as u32)),
                    _ => {}
                }
            }));
        });
    }

    // file drop の hint frame は widget の上に被せる。canvas (lanes) のみ受け付け。
    let canvas_area = Rect {
        x: area.x + TRACK_HEADER_W,
        y: area.y + RULER_H,
        w: area.w - TRACK_HEADER_W,
        h: area.h - RULER_H,
    };
    if ui.is_file_hovering_in_rect(canvas_area) {
        ui.panel_with_border(
            "arr_file_drop_hint",
            canvas_area,
            Color::TRANSPARENT,
            Color::rgb(0.55, 0.85, 0.95),
            2.0,
            0.0,
        );
    }
    if let Some(paths) = ui.take_file_drop_in_rect(canvas_area) {
        let display = paths
            .iter()
            .map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("(unnamed)")
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(", ");
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.status_message = format!("dropped: {display}");
        }));
    }
}

/// arrangement widget からの edit request を AppEvent に変換する。
/// id ベース → index ベース (app の事情) は Edit::mutate 内で行う (ロックフリーで
/// Edit 列をフレーム末尾 apply する gui_01 の流儀に乗る)。
fn make_edit(req: ArrangementEditRequest) -> Edit<AppData> {
    match req {
        ArrangementEditRequest::SelectClips { next, .. } => {
            Edit::mutate(move |app: &mut AppData| {
                let next_refs: Vec<ClipRef> =
                    next.iter().filter_map(|key| clip_key_to_ref(app, *key)).collect();
                app.handle_event(AppEvent::SetClipSelection(next_refs));
            })
        }
        ArrangementEditRequest::SelectTrack { next, .. } => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(track_id) = next
                    && let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id)
                {
                    app.handle_event(AppEvent::SelectTrack(idx as u32));
                }
            })
        }
        ArrangementEditRequest::DoubleClickClip(key) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(target) = clip_key_to_ref(app, key) {
                    app.handle_event(AppEvent::SelectClip { target, additive: false });
                    app.handle_event(AppEvent::SelectBottomPanel(1));
                }
            })
        }
        ArrangementEditRequest::DoubleClickEmpty { track, beat } => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(t_idx) = app.song.tracks.iter().position(|t| t.id == track) {
                    let snapped = beat.floor().max(0.0);
                    app.handle_event(AppEvent::CreateClip {
                        track: t_idx as u32,
                        start_beat: snapped,
                    });
                    app.handle_event(AppEvent::SelectBottomPanel(1));
                }
            })
        }
        ArrangementEditRequest::MoveClips(deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, f64)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.from).map(|r| (r, d.next_start_beat))
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::SetClipPositions(entries));
                }
            })
        }
        ArrangementEditRequest::ResizeClips(deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                for d in &deltas {
                    if let Some(target) = clip_key_to_ref(app, d.key) {
                        app.handle_event(AppEvent::ResizeClip {
                            target,
                            length: d.next_len,
                        });
                    }
                }
            })
        }
        ArrangementEditRequest::DeleteClips(keys) => {
            Edit::mutate(move |app: &mut AppData| {
                let refs: Vec<ClipRef> =
                    keys.iter().filter_map(|key| clip_key_to_ref(app, *key)).collect();
                if !refs.is_empty() {
                    app.handle_event(AppEvent::SetClipSelection(refs));
                    app.handle_event(AppEvent::DeleteSelectedClip);
                }
            })
        }
        ArrangementEditRequest::ToggleTrackMute(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::ToggleTrackMute(idx as u32));
                }
            })
        }
        ArrangementEditRequest::ToggleTrackSolo(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::ToggleTrackSolo(idx as u32));
                }
            })
        }
        ArrangementEditRequest::DeleteTrack(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::DeleteTrack(idx as u32));
                }
            })
        }
        ArrangementEditRequest::MoveTrackUp(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::MoveTrackUp(idx as u32));
                }
            })
        }
        ArrangementEditRequest::MoveTrackDown(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::MoveTrackDown(idx as u32));
                }
            })
        }
        ArrangementEditRequest::ReorderTracks(order) => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ReorderTracks(order.clone()));
            })
        }
        ArrangementEditRequest::SetTrackVolume { track: track_id, prev: _, next } => {
            // band は dB スケールで表示しているので、widget からは fader-scale value
            // (0..1) が来る。fader_to_amp で linear amp に戻して反映する。
            let amp = fader_to_amp(next.clamp(0.0, 1.0));
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::SetTrackVolume {
                        track: idx as u32,
                        amp,
                    });
                }
            })
        }
        ArrangementEditRequest::SetTrackRowH(h) => Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetArrangeTrackRowH(h));
        }),
        ArrangementEditRequest::BeginRenameTrack(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if let Some(idx) = app.song.tracks.iter().position(|t| t.id == track_id) {
                    app.handle_event(AppEvent::BeginRenameTrack(idx as u32));
                }
            })
        }
        ArrangementEditRequest::SetLoopRange { start, end } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLoopRange { start, end });
            })
        }
        ArrangementEditRequest::SetZoomX(z) => Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetArrangeZoom(z));
        }),
        ArrangementEditRequest::SetScrollX(beat) => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetArrangeScroll(beat as f32));
            })
        }
        ArrangementEditRequest::SetTrackTop(_) => {
            // 縦 scroll は本 view では未使用 (track row 全件表示)。
            Edit::mutate(|_| {})
        }
    }
}

fn clip_key_to_ref(app: &AppData, key: ClipKey) -> Option<ClipRef> {
    let t_idx = app.song.tracks.iter().position(|t| t.id == key.track)?;
    let c_idx = app.song.tracks[t_idx].clips.iter().position(|c| c.id == key.clip)?;
    Some(ClipRef { track: t_idx as u32, clip: c_idx as u32 })
}
