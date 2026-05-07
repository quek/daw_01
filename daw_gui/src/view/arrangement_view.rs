//! Arrangement view (track headers / ruler / lanes / clip drag) を gui_01 の
//! `Ui::arrangement` widget 1 呼び出しに集約。
//!
//! AppData は引き続き track / clip を index ベースで持つので、ここで stable id
//! (Track.id / Clip.id) と index の変換層を担う。

use std::sync::Arc;

use daw_ui_core::{
    ArrangementClip, ArrangementEditRequest, ArrangementStyle, ArrangementTrack,
    ArrangementView, ClipKey, Edit, ToggleButtonStyle, Ui,
};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent, ClipRef};
use crate::view::mixer_strips::{amp_to_fader, fader_to_amp};
use crate::view::snap::{self, SNAP_LABELS};

const TRACK_HEADER_W: f32 = 160.0;
const RULER_H: f32 = 20.0;
const TOOLBAR_H: f32 = 24.0;
const COLOR_TOOLBAR_BG: Color = Color { r: 0.10, g: 0.10, b: 0.12, a: 1.0 };

const SNAP_TOGGLE_STYLE: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.30, g: 0.50, b: 0.70, a: 1.0 },
    hint_band: None,
    hint_band_h: 2.0,
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 3.0,
    font_size: 12.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    // 上部 24 px を Snap toolbar に。残りを arrangement widget に渡す。
    let toolbar_rect = Rect { x: area.x, y: area.y, w: area.w, h: TOOLBAR_H };
    let body = Rect {
        x: area.x,
        y: area.y + TOOLBAR_H,
        w: area.w,
        h: (area.h - TOOLBAR_H).max(0.0),
    };
    draw_snap_toolbar(app, ui, toolbar_rect);
    let area = body;

    // auto-fit (X キー / Fit ボタン) のために、現フレームの canvas (lanes) サイズを記録。
    let canvas_w = (area.w - TRACK_HEADER_W).max(0.0);
    let canvas_h = (area.h - RULER_H).max(0.0);
    let canvas_size = (canvas_w, canvas_h);
    if app.last_arrange_canvas_size != canvas_size {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.last_arrange_canvas_size = canvas_size;
        }));
    }

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
                    // gui_01 #019 (M14 Phase 63e): refcount >= 2 (= 共有 clip)
                    // のときだけ Some(hue)。 widget が hue + style.share_group_S/L
                    // で HSL→RGB 変換してアクセント色 + link glyph を描画。
                    // hue は content_id の golden-ratio hash で `[0.0, 1.0)` に
                    // 一様分布させ、 共有グループ間で色が衝突しにくいようにする。
                    share_group_color: if app
                        .song
                        .clip_content_refcount(c.content_id)
                        >= 2
                    {
                        Some(content_id_to_hue(c.content_id))
                    } else {
                        None
                    },
                })
                .collect(),
            // gui_01 #016 で追加された group hierarchy fields:
            parent_id: t.parent_group_id,
            depth: app.compute_track_depth(t),
            collapsed: app.collapsed_groups.contains(&t.id),
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

    // gui_01 #016 で `selected_track: u32` → `selected_tracks: &[u32]`
    // (track id 列) に変更されたので、 そのまま渡す。
    let selected_tracks: &[u32] = &app.selected_track_ids;

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
        bpm: app.song.bpm,
        time_sig: app.song.time_sig,
        snap: snap::arrange_snap_config(app),
    };

    let style = ArrangementStyle::default();

    // arrangement widget へ流す Edit 生成。
    // gui_01 #010 (M14 Phase 60) 以降、DoubleClickEmpty.beat / MoveClips / ResizeClips の
    // delta は widget 内で snap 済み (Alt 一時無効化も内部処理) なので、daw_01 側で
    // post-process は不要。

    let resp = ui.arrangement(
        "arrangement",
        area,
        &tracks,
        view,
        &selected_clips,
        selected_tracks,
        &style,
        make_edit,
    );

    // gui_01 #020 (M14 Phase 63f): clip 上の右クリックメニュー (Make Unique)。
    // widget が `clip_rects: Vec<(ClipKey, Rect)>` を返してくれるので、
    // track_header_rects と同じパターンで context_menu_for を重ねる。
    // refcount==1 の clip では `MakeClipUnique` handler が「すでに独立 clip」
    // status_message を出すだけなので、 context menu はすべての clip に
    // 同形で出す (条件分岐で項目を省くと UX が分かりにくい)。
    for (clip_key, rect) in &resp.clip_rects {
        let key = *clip_key;
        ui.context_menu_for(*rect, &["Make Unique"], move |idx, ui| {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                let Some(target) = clip_key_to_ref(app, key) else {
                    return;
                };
                match idx {
                    0 => app.handle_event(AppEvent::MakeClipUnique(target)),
                    _ => {}
                }
            }));
        });
    }

    // track header の右クリックメニュー (Rename / Delete) を widget 外で重ねる。
    // widget は track_header_rects と BeginRenameTrack / DeleteTrack の発行までを担う。
    // rename mode 中の track には text_input を rect に重ね描きする。
    let renaming_track_id = app
        .track_rename_idx
        .and_then(|idx| app.song.tracks.get(idx as usize).map(|t| t.id));
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

        if Some(track_id) == renaming_track_id {
            // text_input は track header rect の上端に被せる (M/S トグル等は隠れる)。
            // text_input widget が click で focus を取る。Enter で commit、Esc は
            // root の escape shortcut handler が CancelRenameTrack を発行する。
            let input_rect = Rect {
                x: rect.x + 2.0,
                y: rect.y + 2.0,
                w: rect.w - 4.0,
                h: 22.0,
            };
            let resp = ui.text_input_at_focused(
                ("track_rename", track_id),
                input_rect,
                &app.track_rename_text,
                |new| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::RenameTrackChanged(new.clone()));
                    })
                },
            );
            if resp.committed {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CommitRenameTrack);
                }));
            }
        }
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
            // gui_01 #016: multi-select 対応。 widget が決定した
            // `next: Vec<u32>` (id 列、 modifier-aware で Single /
            // RangeFromAnchor / Toggle が解決済) をそのまま反映する。
            Edit::mutate(move |app: &mut AppData| {
                app.selected_track_ids = next.clone();
                // selected_clip / selected_clips も末尾の cursor track
                // 上の clip だけ残す形で同期したいが、 multi-select 中は
                // clip 選択の優先度が低いので暫定的に変更しない。
            })
        }
        ArrangementEditRequest::ToggleGroupCollapsed(track_id) => {
            Edit::mutate(move |app: &mut AppData| {
                if app.collapsed_groups.contains(&track_id) {
                    app.collapsed_groups.remove(&track_id);
                } else {
                    app.collapsed_groups.insert(track_id);
                }
            })
        }
        ArrangementEditRequest::SetTrackParent {
            tracks,
            parent,
            anchor_after,
        } => {
            // gui_01 #016 reply: drag&drop reparent + reorder の統合 Edit。
            // 3 段再構築 (a) source tracks を song.tracks から remove
            // (b) parent_group_id を新親に書き換え (c) anchor_after の
            // 直後 (None で先頭) に insert。
            Edit::mutate(move |app: &mut AppData| {
                let mut moved: Vec<common::model::Track> = tracks
                    .iter()
                    .filter_map(|id| {
                        let pos = app.song.tracks.iter().position(|t| t.id == *id)?;
                        Some(app.song.tracks.remove(pos))
                    })
                    .collect();
                if moved.is_empty() {
                    return;
                }
                for t in &mut moved {
                    t.parent_group_id = parent;
                }
                let insert_at = match anchor_after {
                    None => 0,
                    Some(after_id) => app
                        .song
                        .tracks
                        .iter()
                        .position(|t| t.id == after_id)
                        .map(|i| i + 1)
                        .unwrap_or(app.song.tracks.len()),
                };
                for (offset, t) in moved.into_iter().enumerate() {
                    app.song.tracks.insert(insert_at + offset, t);
                }
                app.sync_song_to_plugin_host();
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
            // gui_01 #010 (M14 Phase 60) 以降、widget 内 snap 済み (Alt 一時無効化込み)。
            let start_beat = beat.max(0.0);
            Edit::mutate(move |app: &mut AppData| {
                if let Some(t_idx) = app.song.tracks.iter().position(|t| t.id == track) {
                    app.handle_event(AppEvent::CreateClip {
                        track: t_idx as u32,
                        start_beat,
                    });
                    app.handle_event(AppEvent::SelectBottomPanel(1));
                }
            })
        }
        ArrangementEditRequest::MoveClips(deltas) => {
            // `MoveClipDelta::to_track` を保持して track 跨ぎ move に対応。
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, u32, f64)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.from)
                            .map(|r| (r, d.to_track, d.next_start_beat))
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::SetClipPositions(entries));
                }
            })
        }
        ArrangementEditRequest::CloneClipsLinked(deltas) => {
            // gui_01 #019: Ctrl+drag → release。 元 clip を残し、 各 source の
            // drop 位置に共有コピーを生成。 daw_01 側で `content_id` を流用する。
            // to_track が source の track と異なれば track 跨ぎコピー。
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, u32, f64)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.from)
                            .map(|r| (r, d.to_track, d.next_start_beat))
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::CloneClipsLinked(entries));
                }
            })
        }
        ArrangementEditRequest::CloneClipsIndependent(deltas) => {
            // gui_01 #019: Ctrl+Shift+drag → release。 同上だが content を
            // deep clone + 新 ContentId で独立コピー。
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, u32, f64)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.from)
                            .map(|r| (r, d.to_track, d.next_start_beat))
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::CloneClipsIndependent(entries));
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

/// gui_01 #019: 共有 clip 群を視覚区別するためのアクセント色 hue 計算。
/// golden ratio (0.61803...) を掛けて fract で `[0.0, 1.0)` に一様分布させる
/// (連番の content_id でも色が満遍なくバラける)。 widget 側で
/// `ArrangementStyle.share_group_saturation` / `share_group_fill_lightness` /
/// `share_group_border_lightness` と組み合わせて HSL → RGB 変換される。
fn content_id_to_hue(content_id: common::model::ContentId) -> f32 {
    const GOLDEN_RATIO_CONJUGATE: f32 = 0.618_034;
    (content_id as f32 * GOLDEN_RATIO_CONJUGATE).fract()
}

fn clip_key_to_ref(app: &AppData, key: ClipKey) -> Option<ClipRef> {
    let t_idx = app.song.tracks.iter().position(|t| t.id == key.track)?;
    let c_idx = app.song.tracks[t_idx].clips.iter().position(|c| c.id == key.clip)?;
    Some(ClipRef { track: t_idx as u32, clip: c_idx as u32 })
}

/// 上部 24 px の Snap toolbar を描画。
/// 配置: [Snap toggle 60px] [snap unit dropdown 90px] [Fit button 50px]
fn draw_snap_toolbar(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect) {
    ui.panel("arr_toolbar_bg", rect, COLOR_TOOLBAR_BG, 0.0);

    let pad = 6.0;
    let h = 18.0;
    let y = rect.y + (rect.h - h) * 0.5;

    let toggle_w = 60.0;
    let dropdown_w = 90.0;
    let fit_w = 50.0;

    let toggle_rect = Rect { x: rect.x + pad, y, w: toggle_w, h };
    let dropdown_rect = Rect {
        x: toggle_rect.x + toggle_rect.w + pad,
        y,
        w: dropdown_w,
        h,
    };
    let fit_rect = Rect {
        x: dropdown_rect.x + dropdown_rect.w + pad,
        y,
        w: fit_w,
        h,
    };

    ui.toggle_button_at(
        "arr_snap_toggle",
        "Snap",
        toggle_rect,
        app.arrange_snap_enabled,
        &SNAP_TOGGLE_STYLE,
        |new| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetArrangeSnapEnabled(new));
            })
        },
    );

    if let Some(idx) = ui.dropdown(
        "arr_snap_unit",
        dropdown_rect,
        SNAP_LABELS,
        app.arrange_snap_choice as usize,
    ) {
        let new = idx as u8;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetArrangeSnapChoice(new));
        }));
    }

    ui.button_at("arr_fit", "Fit", fit_rect, || {
        Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::FitArrangeToContent);
        })
    });
}
