//! Arrangement view (track headers / ruler / lanes / clip drag) を gui_01 の
//! `Ui::arrangement` widget 1 呼び出しに集約。
//!
//! AppData は引き続き track / clip を index ベースで持つので、ここで stable id
//! (Track.id / Clip.id) と index の変換層を担う。

use std::sync::Arc;

use daw_ui_core::{
    ArrangementAutomationClip, ArrangementAutomationLane, ArrangementAutomationPoint,
    ArrangementClip, ArrangementClipAudioEdit, ArrangementCurveKind, ArrangementEditRequest,
    ArrangementStyle, ArrangementTrack, ArrangementView, AutomationClipKey,
    AutomationLaneKey, ChannelLayout, ClipKey, Edit, FadeCurve as WidgetFadeCurve,
    FadeEdge, SampleSlices, ToggleButtonStyle, Ui, WaveformRenderMode, WaveformSource,
    WaveformStyle, WaveformView,
};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent, ClipRef, FadeEdgeKind};
use crate::view::mixer_strips::{amp_to_fader, fader_to_amp};
use crate::view::snap::{self, SNAP_LABELS};

/// gui_01 widget の `FadeCurve` (#025) ↔ daw_01 model `FadeCurve` の
/// 対応変換。 同 3 種を 1:1 で対応させているだけだが、 type が別 crate
/// なので変換 helper を経由する。
fn widget_curve_from_model(c: common::model::FadeCurve) -> WidgetFadeCurve {
    match c {
        common::model::FadeCurve::Linear => WidgetFadeCurve::Linear,
        common::model::FadeCurve::Exponential => WidgetFadeCurve::Exponential,
        common::model::FadeCurve::SCurve => WidgetFadeCurve::SCurve,
    }
}

fn model_curve_from_widget(c: WidgetFadeCurve) -> common::model::FadeCurve {
    match c {
        WidgetFadeCurve::Linear => common::model::FadeCurve::Linear,
        WidgetFadeCurve::Exponential => common::model::FadeCurve::Exponential,
        WidgetFadeCurve::SCurve => common::model::FadeCurve::SCurve,
    }
}

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
                    // gui_01 #025 (M14 Phase 63k): audio clip のとき first event の
                    // 値を渡して widget に dB handle / fade 角 grip / envelope を
                    // 描かせる。 Phase 1 で 1 clip 1 event 前提なので first event
                    // を「clip 全体の field」 として表示。 widget は drag release で
                    // `SetClipGainDb` / `SetClipFade` / `SetClipFadeCurve` を発行
                    // するので make_edit 側で受けて AppEvent に変換する。
                    audio_edit: app
                        .song
                        .clip_contents
                        .get(&c.content_id)
                        .and_then(|ct| ct.audio_events())
                        .and_then(|events| events.first())
                        .map(|ev| ArrangementClipAudioEdit {
                            gain_db: ev.gain_db,
                            fade_in_beats: ev.fade_in_beats,
                            fade_out_beats: ev.fade_out_beats,
                            fade_in_curve: widget_curve_from_model(ev.fade_in_curve),
                            fade_out_curve: widget_curve_from_model(ev.fade_out_curve),
                        }),
                })
                .collect(),
            // gui_01 #016 で追加された group hierarchy fields:
            parent_id: t.parent_group_id,
            depth: app.compute_track_depth(t),
            collapsed: app.collapsed_groups.contains(&t.id),
            // gui_01 #028 (M14 Phase 63n-1): automation lane 行。 daw_01 は
            // 「展開中の track id 集合」 を `expanded_automation_tracks` に持ち、
            // 含まれない track は collapsed。 default 全 collapsed (Bitwig 流)。
            // lane が空の track でも collapsed flag は設定するが、 widget は
            // 「lane 0 件 → disclosure ▶/▼ 非表示」 で扱う。
            automation_lanes_collapsed: !app.expanded_automation_tracks.contains(&t.id),
            automation_lanes: build_arrangement_automation_lanes(t, &app.song),
            // gui_01 #031 (M14 Phase 63n-6): 個別 track row 高さ override。
            // None なら global `view.track_row_h` (= Alt+wheel で動く既存値) を
            // 使う。Some(px) なら override (Alt+drag / 下端 splitter drag で
            // SetSingleTrackRowH を発火 → AppData.track_row_overrides に反映)。
            row_h: app.track_row_overrides.get(&t.id).copied(),
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

    // audio_edit が Some の clip に widget が描画する dB handle line (gain_db = 0
    // で clip 中央を貫通する細い水平線) は、 視覚的には波形と被って邪魔になる。
    // hit zone (`audio_db_handle_band_h`) は色と無関係なので、 線色を完全透明に
    // して描画だけ抑制する (drag は引き続き機能する)。
    //
    // gui_01 #028 / #029 直後の visual feedback (2026-05-09):
    // automation clip が CloneLinked された後 (refcount >= 2) に widget が
    // share_group_color の hue + default lightness 0.55 で塗ると、 clip 上の
    // 名前文字 (default text color = (0.92,0.92,0.94)) との contrast が低く
    // 読みづらい。 fill / border lightness を暗めに override して contrast
    // を上げる。 (MIDI clip の linked 表示にも同 style が適用される。)
    let style = ArrangementStyle {
        audio_db_handle_color: Color { r: 0.0, g: 0.0, b: 0.0, a: 0.0 },
        share_group_fill_lightness: 0.30,
        share_group_border_lightness: 0.55,
        // selected clip の塗り (default = (1.0, 0.85, 0.30) yellow) は default
        // text color (0.92, 0.92, 0.94 白系) との contrast が低く、 selected
        // automation clip 名「Volume curve」 が読みづらい (visual feedback
        // 2026-05-09)。 暗めの blue-grey で contrast 確保。 border は明るめ
        // で selection マーカー視認性を維持。
        clip_selected_fill: Color::rgb(0.20, 0.30, 0.50),
        clip_selected_border: Color::rgb(0.95, 0.95, 1.00),
        ..ArrangementStyle::default()
    };

    // arrangement widget へ流す Edit 生成。
    // gui_01 #010 (M14 Phase 60) 以降、DoubleClickEmpty.beat / MoveClips / ResizeClips の
    // delta は widget 内で snap 済み (Alt 一時無効化も内部処理) なので、daw_01 側で
    // post-process は不要。

    // gui_01 #028 (M14 Phase 63n-3): 選択中の automation clip を widget 型
    // にそのまま渡す (daw_01 model と field 名一致なので 1:1 cast)。
    let selected_automation_clips_widget: Vec<daw_ui_core::AutomationClipKey> = app
        .selected_automation_clips
        .iter()
        .map(|k| daw_ui_core::AutomationClipKey {
            track: k.track,
            lane: k.lane,
            clip: k.clip,
        })
        .collect();

    let resp = ui.arrangement(
        "arrangement",
        area,
        &tracks,
        view,
        &selected_clips,
        selected_tracks,
        &selected_automation_clips_widget,
        &style,
        make_edit,
    );

    // gui_01 #020 (M14 Phase 63f): clip 上の右クリックメニュー (Make Unique)。
    // widget が `clip_rects: Vec<(ClipKey, Rect)>` を返してくれるので、
    // track_header_rects と同じパターンで context_menu_for を重ねる。
    // refcount==1 の clip では `MakeClipUnique` handler が「すでに独立 clip」
    // status_message を出すだけなので、 context menu はすべての clip に
    // 同形で出す (条件分岐で項目を省くと UX が分かりにくい)。
    //
    // Phase 1 PR4: audio clip の波形描画も同 loop で重ね描き。
    // Ui::arrangement が描いた clip rect の上に `Ui::waveform` を配置し、
    // ContentId が `ClipContent::Audio` の場合だけ rect 内に波形を表示する。
    // gui_01 #023 で drop position が取れるようになったらここに resolve
    // ロジックも追加する (PR4 範囲外)。
    // Phase 2 PR5: Auto-Fade / Auto-Crossfade を context_menu に追加
    // (`docs/plan_audio_clip.md` §3.5)。 選択 clip 群に対して動くので、
    // 右クリックされた clip 自体の selection を変える/変えないは handler
    // 側に任せる (= MakeClipUnique も同 pattern)。
    let lanes_x = area.x + TRACK_HEADER_W;
    for (clip_key, rect) in &resp.clip_rects {
        let key = *clip_key;
        ui.context_menu_for(
            *rect,
            &[
                "Make Unique",
                "Auto-Fade",
                "Auto-Crossfade",
                "Reverse",
                "Bounce In Place",
                "Bounce (with FX)",
            ],
            move |idx, ui| {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let Some(target) = clip_key_to_ref(app, key) else {
                        return;
                    };
                    match idx {
                        0 => app.handle_event(AppEvent::MakeClipUnique(target)),
                        1 => app.handle_event(AppEvent::AutoFadeSelectedClips),
                        2 => app.handle_event(AppEvent::AutoCrossfadeSelectedClips),
                        // Reverse は右クリック対象 clip 1 つだけを toggle
                        // (Auto-Fade と違って selection 全体ではなく当該
                        // clip のみ。 Bitwig clip メニューでも同様)。
                        3 => app.handle_event(AppEvent::ToggleClipReversed(target)),
                        // Bounce In Place: Pre-FX (= plugin chain 通さず)、
                        // 当該 clip の content を 1 event の baked audio に
                        // 置換 (= 元 track 内で同 path)。 Phase 2 PR9
                        // (`docs/plan_audio_clip.md` §3.8)。
                        4 => app.handle_event(AppEvent::BounceClipInPlace(target)),
                        // Bounce (with FX): plugin chain を **通した** 結果を
                        // **新 track + 新 Clip** に書き出す (元 clip は不変)。
                        // async (= IPC freewheel render → 完了通知)。
                        // Phase 2 PR-C (`docs/plan_audio_followup.md`)。
                        5 => app.handle_event(AppEvent::BounceClipWithFx(target)),
                        _ => {}
                    }
                }));
            },
        );
        let is_selected = selected_clips.contains(clip_key);
        draw_audio_clip_waveform(app, ui, *clip_key, *rect, lanes_x, is_selected);
        draw_audio_clip_value_overlay(app, ui, *clip_key, *rect);
    }

    // gui_01 #028 (M14 Phase 63n-2): automation point 上の右クリック →
    // Hold / Linear / Bezier の curve type popup。 widget が
    // `automation_point_rects: Vec<(AutomationPointKey, Rect)>` を返すので
    // clip_rects と同 idiom で `context_menu_for` を毎 frame 重ねる。
    // popup 選択 → ArrangementCurveKind を `SetAutomationCurveType` に
    // 変換、 prev は popup open 時点の `clip.points[idx].curve` を retrieve。
    //
    // **重要 (visual feedback fix 2026-05-09)**: automation_point_rects は
    // automation_clip_rects と空間的に overlap している (= point は clip
    // 内に居る)。 `context_menu_for` は rect 内右クリックで popup を open
    // するため、 同位置に **両方の popup が同 frame で open される**
    // bug があった。 user が point の "Linear" (idx=1) を click すると
    // clip popup の "Delete" (idx=1) も同時発火 → clip 消失。
    //
    // 対策: point popup を **先に** ループで register し、 同 frame で
    // 右クリックが point rect 上で起きていたら clip popup ループを **skip**
    // する。 これで point popup だけが新規 open され、 clip popup の
    // open_popup が呼ばれない。
    for (point_key, rect) in &resp.automation_point_rects {
        let key = *point_key;
        ui.context_menu_for(
            *rect,
            &["Hold", "Linear", "Bezier"],
            move |idx, ui| {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    let next = match idx {
                        0 => ArrangementCurveKind::Hold,
                        1 => ArrangementCurveKind::Linear,
                        2 => ArrangementCurveKind::Bezier { tension: 0.0 },
                        _ => return,
                    };
                    // prev curve を retrieve (Undo 用)。 lookup できなかった
                    // ら no-op で抜ける (= 編集中に lane / clip が削除された
                    // race を防ぐ)。
                    let prev = app
                        .song
                        .track_by_id(key.clip.track)
                        .and_then(|t| t.lane_by_id(key.clip.lane))
                        .and_then(|l| l.clip_by_id(key.clip.clip))
                        .and_then(|c| app.song.clip_contents.get(&c.content_id))
                        .and_then(|cc| cc.automation_points())
                        .and_then(|pts| pts.get(key.point_idx as usize))
                        .map(|p| p.curve);
                    let Some(prev) = prev else { return };
                    app.handle_event(AppEvent::SetAutomationCurveType {
                        track_id: key.clip.track,
                        lane_id: key.clip.lane,
                        clip_id: key.clip.clip,
                        point_idx: key.point_idx,
                        prev,
                        next: widget_curve_to_model(next),
                    });
                }));
            },
        );
    }

    // gui_01 #028 (M14 Phase 63n-3): automation clip 上の右クリック →
    // Make Unique / Delete。 ただし上で point popup を先に register してい
    // て、 同 frame で右クリックが **point rect 上** だったら clip popup の
    // 登録を skip する (= 同位置で 2 つの popup が同時 open する bug 回避)。
    let pointer = ui.pointer();
    let suppress_clip_menu = pointer.secondary_just_pressed
        && pointer.pos.is_some_and(|(px, py)| {
            resp.automation_point_rects
                .iter()
                .any(|(_, r)| r.contains(px, py))
        });
    if !suppress_clip_menu {
        for (auto_key, rect) in &resp.automation_clip_rects {
            let widget_key = *auto_key;
            ui.context_menu_for(
                *rect,
                &["Make Unique", "Delete"],
                move |idx, ui| {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        let model_key = common::model::AutomationClipKey {
                            track: widget_key.track,
                            lane: widget_key.lane,
                            clip: widget_key.clip,
                        };
                        match idx {
                            0 => app.handle_event(AppEvent::MakeAutomationClipUnique(
                                model_key,
                            )),
                            1 => app.handle_event(AppEvent::DeleteAutomationClips {
                                keys: vec![model_key],
                            }),
                            _ => {}
                        }
                    }));
                },
            );
        }
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
    if let Some(drop) = ui.take_file_drop_in_rect(canvas_area) {
        // gui_01 #023 (resolved) + drop target 解決:
        // `DroppedFiles { paths, position }` の position.y から track
        // index を計算し、 `ImportAudio` の `target_track_idx` で渡す。
        // 同 widget の hover_clip 計算 (= 数行下) と同じ
        // `(local_y / row_h)` 式。 canvas 外なら None で fallback。
        let paths = drop.paths;
        let drop_y = drop.position.1;
        let canvas_top = canvas_area.y;
        let local_y = drop_y - canvas_top;
        let target_track_idx = if local_y >= 0.0 {
            Some((local_y / row_h.max(1.0)) as u32)
        } else {
            None
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::ImportAudio {
                paths,
                target_track_idx,
            });
        }));
    }

    // Phase 1 PR7 (`docs/plan_audio_clip.md` §3.3): Split (E) は
    // 「マウスカーソル位置」 で分割するため、 毎フレーム mouse pos →
    // beat と (track, clip) を計算して `AppData` に push する。
    // - `arrangement_hover_beat` は snap 適用版 (E 用)
    // - `arrangement_hover_beat_raw` は snap なし版 (Alt+E 用)
    // - `arrangement_hover_clip` はマウスが乗っている clip の (track, clip)
    //   index — Split が selection 不要で動くために使う
    let raw_beat: Option<f64> = ui.pointer().pos.and_then(|(px, py)| {
        if !canvas_area.contains(px, py) {
            return None;
        }
        let beat =
            view.start_beat + ((px - canvas_area.x) as f64 / zoom as f64);
        Some(beat.max(0.0))
    });
    let snapped_beat: Option<f64> = raw_beat
        .map(|raw| view.snap.snap_beat(raw, /* alt: */ false, zoom));
    let hover_clip: Option<ClipRef> = raw_beat.and_then(|beat| {
        // Mouse y → track index. arrangement widget uses
        // `track_top + (track_idx - top) * track_row_h` for ruler-aware
        // layout. We approximate via `(py - canvas_top) / row_h` since
        // there's no scroll-y on Phase 1 (track_top is 0 for default).
        let (_, py) = ui.pointer().pos?;
        let row_h = row_h.max(1.0);
        let canvas_top = canvas_area.y;
        let local_y = py - canvas_top;
        if local_y < 0.0 {
            return None;
        }
        let track_idx = (local_y / row_h) as usize;
        let track = app.song.tracks.get(track_idx)?;
        for (clip_idx, clip) in track.clips.iter().enumerate() {
            if beat >= clip.start_beat && beat < clip.start_beat + clip.length_beats {
                return Some(ClipRef {
                    track: track_idx as u32,
                    clip: clip_idx as u32,
                });
            }
        }
        None
    });
    if app.arrangement_hover_beat != snapped_beat
        || app.arrangement_hover_beat_raw != raw_beat
        || app.arrangement_hover_clip != hover_clip
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.arrangement_hover_beat = snapped_beat;
            app.arrangement_hover_beat_raw = raw_beat;
            app.arrangement_hover_clip = hover_clip;
        }));
    }
}

/// arrangement widget からの edit request を AppEvent に変換する。
/// id ベース → index ベース (app の事情) は Edit::mutate 内で行う (ロックフリーで
/// Edit 列をフレーム末尾 apply する gui_01 の流儀に乗る)。
fn make_edit(req: ArrangementEditRequest) -> Edit<AppData> {
    match req {
        // gui_01 #024 (resolved): ruler click / drag で発火する seek
        // 要求。 PR-V4 fix: GUI 側 playhead_beat を更新するだけでなく
        // audio engine にも `MainToChild::SeekTo { samples }` を IPC
        // 送信して再生位置を同期する (= Stop 中も Play 中も click 位置
        // に飛ぶ)。 sample 換算は engine sample rate (= 48000) と
        // song.bpm から `beat × 60 / bpm × sr`。
        ArrangementEditRequest::SetPlayheadBeat(beat) => {
            Edit::mutate(move |app: &mut AppData| {
                let beat = beat.max(0.0);
                app.playhead_beat = Some(beat as f32);
                let sr = common::audio_bridge::SAMPLE_RATE as f64;
                let bpm = app.song.bpm.max(1.0) as f64;
                let samples = (beat * 60.0 / bpm * sr).max(0.0) as u64;
                app.send_audio(common::protocol::MainToChild::SeekTo {
                    samples,
                });
            })
        }
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
        // gui_01 #029 (M14 Phase 63n-4): lane body 内 clip ギャップで dblclick
        // → 新規 automation clip 作成。MIDI clip の `DoubleClickEmpty →
        // CreateClip` と同 idiom の lane 版。`start_beat` は widget が snap
        // 適用済、`len_beats` は widget の `automation_clip_default_len_beats`
        // (default 4.0) — caller 側で自前ポリシーに上書きしたければ
        // `ArrangementStyle` を変える。
        ArrangementEditRequest::CreateAutomationClip {
            lane,
            start_beat,
            len_beats,
        } => Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::CreateAutomationClip {
                lane: common::model::AutomationLaneKey {
                    track: lane.track,
                    lane: lane.lane,
                },
                start_beat,
                len_beats,
            });
        }),
        // gui_01 #028 (M14 Phase 63n-1): track 行右端の disclosure ▶/▼ click。
        // automation lane 行の表示・折り畳み toggle を AppEvent に変換。
        ArrangementEditRequest::ToggleTrackAutomationCollapsed { track } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ToggleTrackAutomationCollapsed {
                    track_id: track,
                });
            })
        }
        // gui_01 #028 (M14 Phase 63n-2): lane header `★` click。
        ArrangementEditRequest::SetLaneEnabled { lane, enabled } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLaneEnabled {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    enabled,
                });
            })
        }
        ArrangementEditRequest::SetLaneVisible { lane, visible } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLaneVisible {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    visible,
                });
            })
        }
        // lane header default slider drag (live preview + release 確定)。
        // prev / next は normalized、handler 側で plain 化。
        ArrangementEditRequest::SetLaneDefault { lane, prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLaneDefault {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    prev_norm: prev,
                    next_norm: next,
                });
            })
        }
        ArrangementEditRequest::DeleteLane(lane) => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::DeleteLane {
                    track_id: lane.track,
                    lane_id: lane.lane,
                });
            })
        }
        // gui_01 #030 (M14 Phase 63n-5): lane 高さ drag (Alt+drag or
        // 下端 splitter)。 widget 側で min/max clamp 済。
        ArrangementEditRequest::SetLaneHeight { lane, prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLaneHeight {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    prev_px: prev,
                    next_px: next,
                });
            })
        }
        // gui_01 #031 (M14 Phase 63n-6): MIDI track row 高さの個別 drag。
        // Alt+drag or 下端 splitter drag。 既存 Alt+wheel (= SetTrackRowH
        // global) とは独立。 widget 側で min/max clamp 済。
        ArrangementEditRequest::SetSingleTrackRowH { track, prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSingleTrackRowH {
                    track_id: track,
                    prev_px: prev,
                    next_px: next,
                });
            })
        }
        // lane body 内 dblclick で 1 point 追加。time_beat は clip-local、
        // value_norm は normalized。
        ArrangementEditRequest::AddAutomationPoint {
            clip,
            time_beat,
            value_norm,
        } => Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::AddAutomationPoint {
                track_id: clip.track,
                lane_id: clip.lane,
                clip_id: clip.clip,
                time_beat,
                value_norm,
            });
        }),
        // point drag release。同 frame 内 valid な point_idx を gui_01
        // から受け、handler 側で sort 維持しつつ更新。
        ArrangementEditRequest::MoveAutomationPoints(widget_deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<crate::app::MoveAutomationPointEntry> = widget_deltas
                    .into_iter()
                    .map(|d| crate::app::MoveAutomationPointEntry {
                        key: crate::app::AutomationPointKeyRef {
                            track_id: d.point.clip.track,
                            lane_id: d.point.clip.lane,
                            clip_id: d.point.clip.clip,
                            point_idx: d.point.point_idx,
                        },
                        prev_time_beat: d.prev_time_beat,
                        prev_value_norm: d.prev_value_norm,
                        next_time_beat: d.next_time_beat,
                        next_value_norm: d.next_value_norm,
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::MoveAutomationPoints { deltas: entries });
                }
            })
        }
        // Alt+click on point → 即時 1 件削除 (将来は rect select で N 件)。
        ArrangementEditRequest::DeleteAutomationPoints(keys) => {
            Edit::mutate(move |app: &mut AppData| {
                let refs: Vec<crate::app::AutomationPointKeyRef> = keys
                    .into_iter()
                    .map(|k| crate::app::AutomationPointKeyRef {
                        track_id: k.clip.track,
                        lane_id: k.clip.lane,
                        clip_id: k.clip.clip,
                        point_idx: k.point_idx,
                    })
                    .collect();
                if !refs.is_empty() {
                    app.handle_event(AppEvent::DeleteAutomationPoints { points: refs });
                }
            })
        }
        // Right-click curve type popup の選択結果。caller 側で
        // `automation_point_rects` を `context_menu_for` で表示し、
        // 選んだ index → ArrangementCurveKind を gui_01 が返してくる。
        ArrangementEditRequest::SetAutomationCurveType { point, prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetAutomationCurveType {
                    track_id: point.clip.track,
                    lane_id: point.clip.lane,
                    clip_id: point.clip.clip,
                    point_idx: point.point_idx,
                    prev: widget_curve_to_model(prev),
                    next: widget_curve_to_model(next),
                });
            })
        }
        // gui_01 #028 (M14 Phase 63n-3): automation clip drag。修飾子の
        // 違いで Move / Linked / Independent の 3 系統。lane 跨ぎは
        // target 不一致でも widget 側は accept、daw_01 側でも
        // §5.4 の通り全 accept (reject / demote しない)。
        ArrangementEditRequest::MoveAutomationClips(widget_deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries = widget_deltas
                    .into_iter()
                    .map(widget_to_model_clip_delta)
                    .collect::<Vec<_>>();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::MoveAutomationClips { deltas: entries });
                }
            })
        }
        ArrangementEditRequest::CloneAutomationClipsLinked(widget_deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries = widget_deltas
                    .into_iter()
                    .map(widget_to_model_clip_delta)
                    .collect::<Vec<_>>();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::CloneAutomationClipsLinked {
                        deltas: entries,
                    });
                }
            })
        }
        ArrangementEditRequest::CloneAutomationClipsIndependent(widget_deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries = widget_deltas
                    .into_iter()
                    .map(widget_to_model_clip_delta)
                    .collect::<Vec<_>>();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::CloneAutomationClipsIndependent {
                        deltas: entries,
                    });
                }
            })
        }
        ArrangementEditRequest::ResizeAutomationClips(widget_deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries = widget_deltas
                    .into_iter()
                    .map(|d| crate::app::ResizeAutomationClipEntry {
                        key: widget_to_model_clip_key(d.key),
                        prev_start: d.prev_start,
                        prev_len: d.prev_len,
                        next_start: d.next_start,
                        next_len: d.next_len,
                    })
                    .collect::<Vec<_>>();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::ResizeAutomationClips { deltas: entries });
                }
            })
        }
        ArrangementEditRequest::DeleteAutomationClips(keys) => {
            Edit::mutate(move |app: &mut AppData| {
                let model_keys: Vec<common::model::AutomationClipKey> =
                    keys.into_iter().map(widget_to_model_clip_key).collect();
                if !model_keys.is_empty() {
                    app.handle_event(AppEvent::DeleteAutomationClips {
                        keys: model_keys,
                    });
                }
            })
        }
        // 短 click on automation clip → selection 上書き。
        ArrangementEditRequest::SelectAutomationClips { prev, next } => {
            Edit::mutate(move |app: &mut AppData| {
                let prev_model: Vec<common::model::AutomationClipKey> =
                    prev.into_iter().map(widget_to_model_clip_key).collect();
                let next_model: Vec<common::model::AutomationClipKey> =
                    next.into_iter().map(widget_to_model_clip_key).collect();
                app.handle_event(AppEvent::SelectAutomationClips {
                    prev: prev_model,
                    next: next_model,
                });
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
                    // Phase 2 PR6: audio clip は Audio Editor を、 それ以外
                    // (MIDI / Vocal) は Piano Roll を開く (`docs/plan_audio
                    // _clip.md` §3.10)。 bottom_panel 切替は handler 内で
                    // 行われるので、 ここでは AppEvent を発火するだけ。
                    if app.is_audio_clip(target) {
                        app.handle_event(AppEvent::OpenAudioEditor(target));
                    } else {
                        app.handle_event(AppEvent::CloseAudioEditor);
                        app.handle_event(AppEvent::SelectBottomPanel(1));
                    }
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
                        // PR4: gui_01 widget は左端 grip drag → next_start
                        // 進、 next_len 縮の delta を、 右端 grip drag →
                        // next_start == prev_start、 next_len 変の delta を
                        // emit する。 daw_01 は両方を `ResizeClip` handler
                        // にそのまま渡し、 audio event の追従は handler 内で
                        // 計算する (`docs/plan_audio_clip.md` §3.2)。
                        app.handle_event(AppEvent::ResizeClip {
                            target,
                            start_beat: d.next_start,
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
        // gui_01 #025 (M14 Phase 63k): clip 上 grip drag 経路。 widget は
        // drag release で 1 度だけ delta 群を発行する。 Phase 2 PR-B で
        // multi-clip 一括 drag を 1 Undo step にまとめるため、 batch
        // AppEvent (`SetClipGainDbBatch` / `SetClipFadeBeatsBatch` /
        // `SetClipFadeCurveBatch`) に変換して 1 度だけ発火する。 Inspector
        // commit 経路の単発 AppEvent (`SetClipGainDb` 等) は引き続き存在。
        ArrangementEditRequest::SetClipGainDb(deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, f32)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.key).map(|t| (t, d.next_gain_db))
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::SetClipGainDbBatch(entries));
                }
            })
        }
        ArrangementEditRequest::SetClipFade(deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, FadeEdgeKind, f64)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.key).map(|t| {
                            let edge = match d.edge {
                                FadeEdge::In => FadeEdgeKind::In,
                                FadeEdge::Out => FadeEdgeKind::Out,
                            };
                            (t, edge, d.next_beats)
                        })
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::SetClipFadeBeatsBatch(entries));
                }
            })
        }
        ArrangementEditRequest::SetClipFadeCurve(deltas) => {
            Edit::mutate(move |app: &mut AppData| {
                let entries: Vec<(ClipRef, FadeEdgeKind, common::model::FadeCurve)> = deltas
                    .iter()
                    .filter_map(|d| {
                        clip_key_to_ref(app, d.key).map(|t| {
                            let edge = match d.edge {
                                FadeEdge::In => FadeEdgeKind::In,
                                FadeEdge::Out => FadeEdgeKind::Out,
                            };
                            let curve = model_curve_from_widget(d.next_curve);
                            (t, edge, curve)
                        })
                    })
                    .collect();
                if !entries.is_empty() {
                    app.handle_event(AppEvent::SetClipFadeCurveBatch(entries));
                }
            })
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

/// gui_01 #028 (M14 Phase 63n-1): `Track.automation_lanes` を widget が
/// 受け取れる `ArrangementAutomationLane` 列に変換。 各 lane の
/// `target` から label / icon_glyph / color / default_value_norm を導出
/// し、 clip ごとに `Song.clip_contents` から `AutomationContent` を
/// 解決して point 列を normalize する。
///
/// Phase 1 は track-builtin Volume / Pan / Mute のみ実装。 Plugin
/// param / Song-level (tempo / time_sig) は Phase 2+ で IPC 経由の
/// param info を受け取れるようになってから extend する。
fn build_arrangement_automation_lanes(
    track: &common::model::Track,
    song: &common::model::Song,
) -> Vec<ArrangementAutomationLane> {
    track
        .automation_lanes
        .iter()
        .map(|lane| {
            let display = lane_target_display(&lane.target);
            let default_value_norm = plain_to_norm(&lane.target, lane.default_value);
            let clips: Vec<ArrangementAutomationClip> = lane
                .clips
                .iter()
                .map(|c| {
                    let points: Vec<ArrangementAutomationPoint> = song
                        .clip_contents
                        .get(&c.content_id)
                        .and_then(|cc| cc.automation_points())
                        .unwrap_or(&[])
                        .iter()
                        .map(|p| ArrangementAutomationPoint {
                            time_beat: p.time_beat,
                            value_norm: plain_to_norm(&lane.target, p.value),
                            curve: model_curve_to_widget(p.curve),
                        })
                        .collect();
                    ArrangementAutomationClip {
                        id: c.id,
                        start_beat: c.start_beat,
                        len_beats: c.length_beats,
                        name: Arc::from(c.name.as_str()),
                        points,
                        share_group_color: if song.clip_content_refcount(c.content_id) >= 2 {
                            Some(content_id_to_hue(c.content_id))
                        } else {
                            None
                        },
                    }
                })
                .collect();
            ArrangementAutomationLane {
                id: lane.id,
                label: display.label,
                icon_glyph: display.icon_glyph,
                color: display.color,
                enabled: lane.enabled,
                visible: lane.visible,
                height_px: lane.height_px,
                default_value_norm,
                clips,
            }
        })
        .collect()
}

struct LaneDisplay {
    label: Arc<str>,
    icon_glyph: char,
    color: Color,
}

/// `AutomationTarget` ごとの label / icon / 識別色。 label 文字列は
/// `Arc::from` で都度生成 (per-frame だが lane 数は片手で数える程度なので
/// allocation コストは無視できる)。
fn lane_target_display(target: &common::model::AutomationTarget) -> LaneDisplay {
    use common::model::{AutomationTarget, TrackBuiltinParam};
    match target {
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => LaneDisplay {
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(0.42, 0.78, 0.95),
        },
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => LaneDisplay {
            label: Arc::from("Pan"),
            icon_glyph: 'P',
            color: Color::rgb(0.55, 0.92, 0.55),
        },
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute) => LaneDisplay {
            label: Arc::from("Mute"),
            icon_glyph: 'M',
            color: Color::rgb(0.92, 0.45, 0.40),
        },
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_idx }) => {
            LaneDisplay {
                label: Arc::from(format!("Send {}", send_idx + 1)),
                icon_glyph: 'S',
                color: Color::rgb(0.85, 0.75, 0.40),
            }
        }
        AutomationTarget::PluginParam { param_id, .. } => LaneDisplay {
            // Phase 2 で IPC param info を受けたら "Cutoff (Serum)" 等に書き換え。
            label: Arc::from(format!("Param {}", param_id)),
            icon_glyph: 'F',
            color: Color::rgb(0.78, 0.55, 0.92),
        },
        AutomationTarget::SongTempo => LaneDisplay {
            label: Arc::from("Tempo"),
            icon_glyph: 'T',
            color: Color::rgb(0.95, 0.85, 0.55),
        },
        AutomationTarget::SongTimeSigNumerator => LaneDisplay {
            label: Arc::from("Time Sig"),
            icon_glyph: 'T',
            color: Color::rgb(0.95, 0.85, 0.55),
        },
    }
}

/// Plain (target's native unit) → normalized 0..1 で widget に渡すため
/// の変換。 Phase 1 で必要な範囲のみ実装。 plugin param は min/max を
/// 知らないと正規化できないので、 とりあえず `clamp(0, 1)` で渡す
/// (Phase 2 で `AppData.plugin_params` lookup に置換)。
fn plain_to_norm(target: &common::model::AutomationTarget, plain: f64) -> f32 {
    use common::model::{AutomationTarget, TrackBuiltinParam};
    let v = match target {
        // Track.volume は通常 0.0..=2.0 で扱う (1.0 = unity、 amp_to_fader
        // を通せば dB 表現)。 widget の slider band も 0..1 範囲なので
        // 1/2 で normalize。 fader 表示としては不正確だが、 lane の
        // default_value_norm は単に slider 帯の位置決めなので OK。
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => plain / 2.0,
        // Pan は -1..=1 → 0..1。
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => (plain + 1.0) / 2.0,
        // Mute は bool 相当 (0 or 1)。
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute) => {
            if plain >= 0.5 {
                1.0
            } else {
                0.0
            }
        }
        // SendGain も 0..2 と仮定。
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { .. }) => plain / 2.0,
        // PluginParam は min/max 不明 → そのまま clamp。
        AutomationTarget::PluginParam { .. } => plain,
        // Song-level: Phase 5 で実装。 適当に 0 を返す。
        AutomationTarget::SongTempo | AutomationTarget::SongTimeSigNumerator => 0.0,
    };
    v.clamp(0.0, 1.0) as f32
}

/// `common::model::AutomationCurve` (incoming curve) → widget
/// `ArrangementCurveKind` の対応変換。 同 3 種を 1:1 で対応させる。
/// `Exponential` は Phase 3+ で widget 側にも variant を追加してから
/// 扱う想定だが、 暫定で `Bezier { tension: 0.0 }` (= Catmull-Rom) に
/// fallback する。
fn model_curve_to_widget(c: common::model::AutomationCurve) -> ArrangementCurveKind {
    use common::model::AutomationCurve;
    match c {
        AutomationCurve::Hold => ArrangementCurveKind::Hold,
        AutomationCurve::Linear => ArrangementCurveKind::Linear,
        AutomationCurve::Bezier { tension } => ArrangementCurveKind::Bezier { tension },
        AutomationCurve::Exponential { .. } => {
            ArrangementCurveKind::Bezier { tension: 0.0 }
        }
    }
}

/// 逆変換。 widget が popup で返してきた `ArrangementCurveKind` を
/// model の `AutomationCurve` に戻す。 widget には `Exponential` が
/// 無いので 1:1 で安全。
fn widget_curve_to_model(c: ArrangementCurveKind) -> common::model::AutomationCurve {
    use common::model::AutomationCurve;
    match c {
        ArrangementCurveKind::Hold => AutomationCurve::Hold,
        ArrangementCurveKind::Linear => AutomationCurve::Linear,
        ArrangementCurveKind::Bezier { tension } => AutomationCurve::Bezier { tension },
    }
}

/// widget の `AutomationClipKey { track, lane, clip }` → model の同名
/// 構造体。field 名が一致するので 1:1 cast。
fn widget_to_model_clip_key(k: AutomationClipKey) -> common::model::AutomationClipKey {
    common::model::AutomationClipKey {
        track: k.track,
        lane: k.lane,
        clip: k.clip,
    }
}

/// widget の `AutomationLaneKey { track, lane }` → model の同名構造体。
fn widget_to_model_lane_key(k: AutomationLaneKey) -> common::model::AutomationLaneKey {
    common::model::AutomationLaneKey {
        track: k.track,
        lane: k.lane,
    }
}

/// gui_01 #028 (Phase 63n-3): widget の `MoveAutomationClipDelta` を
/// daw_01 内部の `MoveAutomationClipEntry` に変換。 field 名はほぼ
/// 同形なので逐一 copy するだけ。
fn widget_to_model_clip_delta(
    d: daw_ui_core::MoveAutomationClipDelta,
) -> crate::app::MoveAutomationClipEntry {
    crate::app::MoveAutomationClipEntry {
        from: widget_to_model_clip_key(d.from),
        to_lane: widget_to_model_lane_key(d.to_lane),
        prev_start_beat: d.prev_start_beat,
        next_start_beat: d.next_start_beat,
    }
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

// ---------------------------------------------------------------------------
// Phase 1 PR4: audio clip 内の波形描画 (`Ui::waveform` を clip rect に重ねる)
// ---------------------------------------------------------------------------

/// 1 つの audio clip rect 内に波形を描く。 ContentId が `ClipContent::Audio`
/// でなければ何もしない (= MIDI / Vocal clip は通常通り描画される)。
///
/// `Ui::arrangement` は clip rect (border + name) を既に描画しているので、
/// その中に少し margin を取って `Ui::waveform` を呼ぶ。 PR4 段階では 1
/// clip = 1 event を前提 (Audio Editor で複数 event を編集できるのは
/// Phase 2 / PR8)。 source 参照が `audio_source_cache` に無い (=
/// 起動直後でまだ decode されていない / missing source) clip は無音で表示
/// される (波形なし)。
fn draw_audio_clip_waveform(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    clip_key: ClipKey,
    clip_rect: Rect,
    lanes_x: f32,
    is_selected: bool,
) {
    let Some(t_idx) = app.song.tracks.iter().position(|t| t.id == clip_key.track) else {
        return;
    };
    let Some(c_idx) = app.song.tracks[t_idx]
        .clips
        .iter()
        .position(|c| c.id == clip_key.clip)
    else {
        return;
    };
    let clip = &app.song.tracks[t_idx].clips[c_idx];
    let Some(content) = app.song.clip_contents.get(&clip.content_id) else {
        return;
    };
    let Some(events) = content.audio_events() else {
        return; // MIDI / Vocal clip は対象外
    };
    let Some(event) = events.first() else {
        return; // 空の audio content (= 起こらないはずだが defensive)
    };
    let Some(buffer) = app.audio_source_cache.get(event.source_id) else {
        return; // まだ decode されていない / missing source — silent display
    };

    // clip rect は widget が border + name 領域を含めて描画している。
    // 波形は内側 padding を取って描く: 上部 14 px (= name)、 左右 2 px。
    let inset_top: f32 = 14.0;
    let inset_lr: f32 = 2.0;
    let mut view_rect = Rect {
        x: clip_rect.x + inset_lr,
        y: clip_rect.y + inset_top,
        w: (clip_rect.w - inset_lr * 2.0).max(0.0),
        h: (clip_rect.h - inset_top - inset_lr).max(0.0),
    };
    // arrangement widget は viewport の左端より早く始まる clip も full rect で
    // 返してくる (部分カリング rect は culled せず caller 側で扱う仕様)。
    // そのまま `Ui::waveform` に渡すと track header 領域まで波形が伸びるため、
    // lanes_x で左端を clamp し、削った分の frame だけ start_sample をシフトする。
    let event_len_frames = event
        .source_end_frames
        .saturating_sub(event.source_start_frames);
    let mut view_start_sample = event.source_start_frames;
    let mut view_len_samples = event_len_frames.max(1);
    if view_rect.x < lanes_x {
        let cut_px = lanes_x - view_rect.x;
        if cut_px >= view_rect.w {
            return; // 完全に lanes 外
        }
        let frames_per_px = (event_len_frames as f64 / clip_rect.w.max(1.0) as f64).max(0.0);
        let skip_frames = (cut_px as f64 * frames_per_px) as u64;
        view_start_sample = view_start_sample.saturating_add(skip_frames);
        view_len_samples = view_len_samples.saturating_sub(skip_frames).max(1);
        view_rect.x = lanes_x;
        view_rect.w -= cut_px;
    }
    if view_rect.w <= 0.0 || view_rect.h <= 0.0 {
        return;
    }

    // SampleSlices::Planar 用に &[&[f32]] スライスを作る (毎フレーム
    // alloc は許容、 RT path ではなく GUI 描画 path)。
    let planes_borrowed: Vec<&[f32]> = buffer.samples.iter().map(Vec::as_slice).collect();

    let source = WaveformSource {
        samples: SampleSlices::Planar(&planes_borrowed),
        valid_len: buffer.frames as usize,
        generation: event.source_id as u64,
        sample_rate: buffer.sample_rate,
    };
    let view = WaveformView {
        start_sample: view_start_sample,
        len_samples: view_len_samples,
        vertical_gain: 1.0,
    };
    // 選択 clip 背景は黄色 (clip_selected_fill = rgb(1.0, 0.85, 0.30)) なので、
    // 通常時の水色波形だと視認性が悪い。 選択時は濃紺に切り替える。
    let (fg, fg_clipped) = if is_selected {
        (
            Color::rgba(0.05, 0.10, 0.25, 0.95),
            Color::rgb(0.55, 0.05, 0.05),
        )
    } else {
        (
            Color::rgba(0.55, 0.85, 0.95, 0.85),
            Color::rgb(0.95, 0.45, 0.40),
        )
    };
    let style = WaveformStyle {
        fg,
        fg_clipped,
        fill: None,
        baseline: None,
        channel_layout: ChannelLayout::Overlay,
        render_mode: WaveformRenderMode::Auto,
        line_width_px: 1.0,
    };
    let _ = ui.waveform(
        ("audio_clip_wf", clip_key.track, clip_key.clip),
        view_rect,
        source,
        view,
        style,
    );
}

// ---------------------------------------------------------------------------
// Phase 2 PR8: audio clip rect 上に値ラベルを重ね描き (read-only feedback)
// ---------------------------------------------------------------------------

/// audio clip 上に「Gain dB / Fade In / Fade Out」 を small font で
/// オーバーレイ表示する。 grip drag UI (gui_01 #025) が来るまでの
/// 視覚 feedback として、 ユーザーが Inspector に行かなくても値が
/// 確認できるようにする。 値が default (0 dB / 0 fade) の clip では
/// 描かない (= 視覚ノイズを抑える)。 clip rect が 60 px より狭い場合も
/// 描かない (= ラベルが入らない)。
fn draw_audio_clip_value_overlay(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    clip_key: ClipKey,
    clip_rect: Rect,
) {
    if clip_rect.w < 60.0 || clip_rect.h < 24.0 {
        return;
    }
    let Some(t_idx) = app.song.tracks.iter().position(|t| t.id == clip_key.track) else {
        return;
    };
    let Some(c_idx) = app.song.tracks[t_idx]
        .clips
        .iter()
        .position(|c| c.id == clip_key.clip)
    else {
        return;
    };
    let clip = &app.song.tracks[t_idx].clips[c_idx];
    let Some(content) = app.song.clip_contents.get(&clip.content_id) else {
        return;
    };
    let Some(events) = content.audio_events() else {
        return;
    };
    let Some(event) = events.first() else {
        return;
    };

    // Default 値は無表示 (= clip 名で混雑するのを避ける)。
    let show_gain = event.gain_db.abs() > 0.05;
    let show_fade_in = event.fade_in_beats > 0.0;
    let show_fade_out = event.fade_out_beats > 0.0;
    if !(show_gain || show_fade_in || show_fade_out) {
        return;
    }

    // 描画位置: clip rect の右下 (= name は左上、 重ならないように)。
    let text_color = Color::rgba(0.85, 0.92, 1.0, 0.85);
    let font_size = 9.0;
    let pad = 3.0;
    let mut x_right = clip_rect.x + clip_rect.w - pad;
    let y = clip_rect.y + clip_rect.h - font_size - 2.0;

    // 右から左に並べる: [Fade Out] [Fade In] [Gain]。 push_text を
    // 使うと汎用 left-anchored だが、 ここでは y_baseline と x_left
    // で十分 (右端揃えは label 幅推定で代替)。 簡易: 全部左揃えで
    // 順に出す + 適当な width 推定で右端から逆配置。
    if show_fade_out {
        let s = format!("Fo {:.2}b", event.fade_out_beats);
        let w = (s.chars().count() as f32) * (font_size * 0.55);
        x_right -= w;
        ui.label_at(
            ("audio_clip_lbl_fo", clip_key.track, clip_key.clip),
            &s,
            x_right,
            y,
            font_size,
            text_color,
        );
        x_right -= 6.0;
    }
    if show_fade_in {
        let s = format!("Fi {:.2}b", event.fade_in_beats);
        let w = (s.chars().count() as f32) * (font_size * 0.55);
        x_right -= w;
        ui.label_at(
            ("audio_clip_lbl_fi", clip_key.track, clip_key.clip),
            &s,
            x_right,
            y,
            font_size,
            text_color,
        );
        x_right -= 6.0;
    }
    if show_gain {
        let s = format!("{:+.1} dB", event.gain_db);
        let w = (s.chars().count() as f32) * (font_size * 0.55);
        x_right -= w;
        ui.label_at(
            ("audio_clip_lbl_gain", clip_key.track, clip_key.clip),
            &s,
            x_right,
            y,
            font_size,
            text_color,
        );
    }
}
