//! S4b Phase D: arrangement widget の release-commit フェーズ (各 drag session の release で
//! 1 度だけ `Edit<AppData>` を発行 + shortcut / wheel / double-click / secondary-click)。
//! `arrangement()` から抽出。 immediate-mode の geometry / session は明示引数で受ける。

#![allow(clippy::too_many_arguments)]

use super::*;
use daw_ui_core::PointerFrame;

pub(super) fn commit_releases(
    ui: &mut Ui<'_, AppData>,
    wid: WidgetId,
    response: &mut ArrangementResponse,
    pointer: PointerFrame,
    view: ArrangementView,
    style: &ArrangementStyle,
    master_row: Option<&ArrangementMasterRow>,
    sections: &[SectionView],
    selected_clips: &[ClipKey],
    selected_tracks: &[u32],
    selected_automation_clips: &[AutomationClipKey],
    selected_automation_points: &[AutomationPointKey],
    visible_tracks: &[ArrangementTrack],
    press_tops: &[f32],
    lanes: Rect,
    ruler: Rect,
    header_pane: Rect,
    arranger_rect: Rect,
    lanes_h: f32,
    arranger_lane_h: f32,
    beat_per_px: f64,
    zoom_x_px_per_beat: f32,
    clip_drag_release: Option<ClipDragSession>,
    clip_short_click_pos: Option<((f32, f32), bool, bool)>,
    audio_drag_release: Option<AudioDragSession>,
    point_drag_release: Option<AutomationPointDragSession>,
    automation_clip_drag_release: Option<AutomationClipDragSession>,
    automation_curve_param_release: Option<AutomationCurveParamDragSession>,
    automation_lasso_release: Option<AutomationLassoSession>,
    lane_resize_drag_release: Option<AutomationLaneResizeDragSession>,
    section_drag_release: Option<SectionDragSession>,
    loop_drag_release: Option<LoopDragSession>,
    track_volume_release: Option<TrackVolumeDragSession>,
    pending_drop: Option<(Vec<u32>, Option<u32>, Option<u32>)>,
) {
        // ---- shortcut: Delete ----
        // Phase 47c: clip 選択優先、無ければ selected_tracks (multi-select) の先頭を削除。
        // M14 Phase 63c (#016): multi-track の一括削除はあえて単一にとどめる (ungroup → 残った
        // 子 track 群の parent_id 整理を caller 側で必要、 widget API としては `DeleteTrack(u32)` を
        // 既存 1:1 で維持し、 multi 削除は caller が selected_tracks を loop して呼べば実現できる)。
        // 現状は user 体験として「Delete shortcut で 1 track 削除」 を想定。
        if ui.take_shortcut("delete") {
            if !selected_clips.is_empty() {
                ui.push_edit({ let v_k = selected_clips.to_vec(); Edit::mutate(move |app: &mut AppData| { let refs: Vec<ClipRef> = v_k.iter().filter_map(|key| clip_key_to_ref(app, *key)).collect(); if !refs.is_empty() { app.handle_event(AppEvent::SetClipSelection(refs)); app.handle_event(AppEvent::DeleteSelectedClip); } }) });
            } else if let Some(&tid) = selected_tracks.first() {
                ui.push_edit({ let v_id = tid; Edit::mutate(move |app: &mut AppData| { if let Some(idx) = app.song_doc.song().tracks.iter().position(|t| t.id == v_id) { app.handle_event(AppEvent::DeleteTrack(idx as u32)); } }) });
            }
        }

        // ---- clip drag release → MoveClips / ResizeClips ----
        // M9 Phase 60: anchor 0 の delta を `view.snap.snap_beat_delta` で round → 全 anchor に
        // 同 delta 適用。 Resize の min_len は snap unit に合わせる (snap_unit < 0.05 なら 0.05)。
        // **alt は drag 中の最終 `nd.last_alt` を真値とする** — release frame の `pointer.modifiers.alt`
        // は OS event 順序 (ModifiersChanged が MouseInput(Released) より先に届く) によって false に
        // 化けることがあるため信用しない。 `last_alt` は continuation frame で更新され release frame
        // では `allow_update = false` で保持されるので OS event 順序に依存しない。 overlay の snap
        // 判定とも同一値で確定し、 「release で grid に飛ぶ」 不整合が起きない。
        let clip_drag_release_was_some = clip_drag_release.is_some();
        if let Some(nd) = clip_drag_release {
            let release_alt = nd.last_alt;
            let (beat_delta, track_delta): (f64, i32) = {
                let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
                let raw = f64::from(dx) * beat_per_px;
                // 絶対位置 snap (overlay と一貫)。 詳細は `compute_clip_drag_beat_delta` を参照。
                let snapped = compute_clip_drag_beat_delta(
                    &nd,
                    raw,
                    &view.snap,
                    zoom_x_px_per_beat,
                );
                // track 方向は y→visible 行 index 解決の差 (per-track 行高 / lane 展開対応、
                // overlay と同 helper)。
                let td = compute_clip_drag_track_delta(&nd, press_tops);
                (snapped, td)
            };
            let min_len = if view.snap.is_active(release_alt) {
                view.snap.beat_unit(zoom_x_px_per_beat).map_or(0.05, |u| u.max(0.05))
            } else {
                0.05
            };
            match nd.kind {
                ClipDragKind::Move => {
                    // M14 Phase 63c (#016): visible_tracks (collapsed 親の subtree skip 後) で
                    // index → track_id を解決。 anchor.track_index が visible-idx なので、
                    // press_i32 + track_delta も visible domain で clamp する。
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    let max_idx_i32 = (visible_tracks.len().saturating_sub(1)) as i32;
                    // master_row 有りなら visible_tracks[0] は synthetic master (id=MASTER_TRACK_ID)。
                    // clip drop 先から master を除外 (track_header drag / DoubleClickEmpty と
                    // 同じ guard、 ここだけ漏れていた)。 visible_tracks に通常 track が無い退化
                    // ケース (master のみ) は max < min となり clamp が panic するので max を
                    // min まで底上げして fallback (visible_tracks.get(1) = None → 元 track id)。
                    let min_idx_i32 = i32::from(master_row.is_some());
                    let clamp_max = max_idx_i32.max(min_idx_i32);
                    let mut deltas: Vec<MoveClipDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_start = (a.start_beat + beat_delta).max(0.0);
                        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                        let press_i32 = a.track_index as i32;
                        let new_idx = (press_i32 + track_delta).clamp(min_idx_i32, clamp_max);
                        #[allow(clippy::cast_sign_loss)]
                        let new_idx_u = new_idx.max(0) as usize;
                        let new_track_id = visible_tracks
                            .get(new_idx_u)
                            .map_or(a.key.track, |t| t.id);
                        let moved = (new_start - a.start_beat).abs() > 1e-6
                            || new_track_id != a.key.track;
                        if moved {
                            deltas.push(MoveClipDelta {
                                from: a.key,
                                to_track: new_track_id,
                                prev_start_beat: a.start_beat,
                                next_start_beat: new_start,
                            });
                        }
                    }
                    if !deltas.is_empty() {
                        // M14 Phase 63e (#019): Move + Ctrl + Shift → CloneClipsIndependent、
                        // Move + Ctrl → CloneClipsLinked、 それ以外 → 既存 MoveClips。
                        // `last_ctrl` / `last_shift` は overlay と同じ真値を読むので、 release
                        // frame の OS event 順序問題に依存せず確定する。 Alt は直交 (snap 一時
                        // 無効のみ) で、 既に上の `compute_clip_drag_beat_delta` で適用済。
                        let req = if nd.last_ctrl && nd.last_shift {
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
                        } else if nd.last_ctrl {
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
                        } else {
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
                        };
                        ui.push_edit(req);
                    }
                }
                ClipDragKind::ResizeRight => {
                    let mut deltas: Vec<ResizeClipDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_len = (a.len_beats + beat_delta).max(min_len);
                        if (new_len - a.len_beats).abs() > 1e-6 {
                            deltas.push(ResizeClipDelta {
                                key: a.key,
                                prev_start: a.start_beat,
                                prev_len: a.len_beats,
                                next_start: a.start_beat,
                                next_len: new_len,
                                stretch: nd.last_shift,
                            });
                        }
                    }
                    if !deltas.is_empty() {
                        ui.push_edit({ let v_d = deltas; Edit::mutate(move |app: &mut AppData| { for d in &v_d { if let Some(target) = clip_key_to_ref(app, d.key) { app.handle_event(AppEvent::ResizeClip { target, start_beat: d.next_start, length: d.next_len, stretch: d.stretch }); } } }) });
                    }
                }
                ClipDragKind::ResizeLeft => {
                    let mut deltas: Vec<ResizeClipDelta> = Vec::new();
                    for a in &nd.anchors {
                        let max_start = a.start_beat + a.len_beats - min_len;
                        let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                        let actual = new_start - a.start_beat;
                        let new_len = (a.len_beats - actual).max(min_len);
                        if (new_start - a.start_beat).abs() > 1e-6
                            || (new_len - a.len_beats).abs() > 1e-6
                        {
                            deltas.push(ResizeClipDelta {
                                key: a.key,
                                prev_start: a.start_beat,
                                prev_len: a.len_beats,
                                next_start: new_start,
                                next_len: new_len,
                                stretch: nd.last_shift,
                            });
                        }
                    }
                    if !deltas.is_empty() {
                        ui.push_edit({ let v_d = deltas; Edit::mutate(move |app: &mut AppData| { for d in &v_d { if let Some(target) = clip_key_to_ref(app, d.key) { app.handle_event(AppEvent::ResizeClip { target, start_beat: d.next_start, length: d.next_len, stretch: d.stretch }); } } }) });
                    }
                }
            }
        }

        // ---- M14 Phase 63k (#025): audio_drag release → SetClipGainDb / SetClipFade / SetClipFadeCurve ----
        // commit-by-release: drag 中は ghost overlay のみ、 release で `compute_audio_drag_outcome` の
        // 結果に応じて 1 件 emit する。 sticky direction 未確定 + drag 距離不足の場合は no-op
        // (= click 相当、 caller 側で selection 等は変化しない、 既存挙動)。 単一 clip 限定の `vec![delta]`
        // で発行 (multi-clip selection 一括は仕様 §scope 外、 将来拡張)。
        if let Some(ad) = audio_drag_release
            && let Some(out) = compute_audio_drag_outcome(&ad, beat_per_px, style)
        {
            match out {
                AudioDragOutcome::Gain { next_db } => {
                    let delta = ClipGainDelta {
                        key: ad.key,
                        prev_gain_db: ad.anchor.gain_db,
                        next_gain_db: next_db,
                    };
                    ui.push_edit({ let v_d = [delta]; Edit::mutate(move |app: &mut AppData| { let entries: Vec<(ClipRef, f32)> = v_d.iter().filter_map(|d| clip_key_to_ref(app, d.key).map(|t| (t, d.next_gain_db))).collect(); if !entries.is_empty() { app.handle_event(AppEvent::SetClipGainDbBatch(entries)); } }) });
                }
                AudioDragOutcome::FadeLength { edge, next_beats } => {
                    let prev_beats = match edge {
                        FadeEdge::In => ad.anchor.fade_in_beats,
                        FadeEdge::Out => ad.anchor.fade_out_beats,
                    };
                    let delta = ClipFadeDelta {
                        key: ad.key,
                        edge,
                        prev_beats,
                        next_beats,
                    };
                    ui.push_edit({ let v_d = [delta]; Edit::mutate(move |app: &mut AppData| { let entries: Vec<(ClipRef, FadeEdgeKind, f64)> = v_d.iter().filter_map(|d| clip_key_to_ref(app, d.key).map(|t| { let edge = match d.edge { FadeEdge::In => FadeEdgeKind::In, FadeEdge::Out => FadeEdgeKind::Out }; (t, edge, d.next_beats) })).collect(); if !entries.is_empty() { app.handle_event(AppEvent::SetClipFadeBeatsBatch(entries)); } }) });
                }
                AudioDragOutcome::FadeCurve { edge, next_curve } => {
                    let delta = ClipFadeCurveDelta { key: ad.key, edge, next_curve };
                    ui.push_edit({ let v_d = [delta]; Edit::mutate(move |app: &mut AppData| { let entries: Vec<(ClipRef, FadeEdgeKind, common::model::FadeCurve)> = v_d.iter().filter_map(|d| clip_key_to_ref(app, d.key).map(|t| { let edge = match d.edge { FadeEdge::In => FadeEdgeKind::In, FadeEdge::Out => FadeEdgeKind::Out }; let curve = model_curve_from_widget(d.next_curve); (t, edge, curve) })).collect(); if !entries.is_empty() { app.handle_event(AppEvent::SetClipFadeCurveBatch(entries)); } }) });
                }
            }
        }

        // ---- M14 Phase 63n-2 / 63n-8 (#028 / #033): automation_point_drag release ----
        // 旧 Phase 63n-2: 4px jitter 閾値で短 click → no-op (selection 変化なし)。
        // M14 Phase 63n-8 (#033): 短 click は **`SelectAutomationPoints`** に化け、 long drag (>=4px)
        // は selection に含まれていれば **全 selected 点を batch move**、 含まれなければ単独 move。
        //
        // 短 click 仕様 (#033 §C):
        //   - 修飾なし + drag<4px → `next = vec![pressed]` (single select、 旧 selection 破棄)
        //   - Shift / Ctrl + drag<4px → `next = prev XOR vec![pressed]` (toggle)
        //   - Alt + click は既に上の press block で `DeleteAutomationPoints` 即時発火済 (= ここに来ない)
        //
        // long drag 仕様 (#033 §E):
        //   - pressed point が `selected_automation_points` に含まれる → 全 selected の `MoveAutomationPointDelta`
        //     を 1 vec で発行 (各 delta の prev は **release 時点の caller データ** から再 lookup、 next は
        //     pressed point の anchor 位置を round して算出した adjusted_dt を適用)
        //   - 含まれない → 単独 move (旧挙動互換、 selection は変化しない)
        //
        // **absolute 位置 snap** (CLAUDE.md「drag 系 widget の snap」 と同 idiom): anchor の絶対 beat
        // (`clip_start + anchor_time` ) に raw_dt を足して `snap_beat` で round、 差分 `adjusted_dt`
        // を全 anchor に適用。 これで (a) 単一 / 多重で grid 吸着挙動が一致、 (b) anchor が grid 外でも
        // 最終位置 grid に着地。 alt は session の `last_alt` を真値 (race 回避)。
        if let Some(ad) = point_drag_release {
            let dx = ad.last_mouse.0 - ad.anchor_mouse.0;
            let dy = ad.last_mouse.1 - ad.anchor_mouse.1;
            let dist = dx.abs() + dy.abs();
            if dist >= 4.0 {
                // body_rect / clip_rect は anchor 固定 (drag 中の view scroll / lane 順序変化に強い)。
                let beat_to_px =
                    f64::from(ad.body_rect_anchor.w) / view.len_beats.max(1e-6);
                let raw_dt = f64::from(dx) / beat_to_px;
                let raw_abs = ad.clip_start_beat + ad.anchor_time_beat + raw_dt;
                let snapped_abs =
                    view.snap.snap_beat(raw_abs, ad.last_alt, zoom_x_px_per_beat);
                let adjusted_dt = snapped_abs - (ad.clip_start_beat + ad.anchor_time_beat);
                let dv = -dy / ad.clip_rect_anchor.h.max(1.0);
                // pressed が selection に含まれていれば multi、 そうでなければ single
                let drag_set: Vec<AutomationPointKey> =
                    if selected_automation_points.contains(&ad.point) {
                        selected_automation_points.to_vec()
                    } else {
                        vec![ad.point]
                    };
                let mut deltas: Vec<MoveAutomationPointDelta> = Vec::new();
                for key in &drag_set {
                    // release 時の caller データから anchor を再 lookup (drag 中は Edit 流れないので
                    // model 不変、 visible_tracks がそのまま使える)。
                    if let Some((t_b, v_n, _c_start, c_len)) =
                        find_automation_point_data(visible_tracks, *key)
                    {
                        let next_t = (t_b + adjusted_dt).clamp(0.0, c_len.max(0.0));
                        let next_v = (v_n + dv).clamp(0.0, 1.0);
                        if (next_t - t_b).abs() > 1e-9 || (next_v - v_n).abs() > 1e-6 {
                            deltas.push(MoveAutomationPointDelta {
                                point: *key,
                                prev_time_beat: t_b,
                                prev_value_norm: v_n,
                                next_time_beat: next_t,
                                next_value_norm: next_v,
                            });
                        }
                    }
                }
                if !deltas.is_empty() {
                    ui.push_edit({ let v_d = deltas; Edit::mutate(move |app: &mut AppData| { let entries: Vec<MoveAutomationPointEntry> = v_d.into_iter().map(|d| MoveAutomationPointEntry { key: AutomationPointKeyRef { track_id: d.point.clip.track, lane_id: d.point.clip.lane, clip_id: d.point.clip.clip, point_idx: d.point.point_idx }, prev_time_beat: d.prev_time_beat, prev_value_norm: d.prev_value_norm, next_time_beat: d.next_time_beat, next_value_norm: d.next_value_norm }).collect(); if !entries.is_empty() { app.handle_event(AppEvent::MoveAutomationPoints { deltas: entries }); } }) });
                }
            } else if !ad.last_alt {
                // 短 click (drag < 4px) → SelectAutomationPoints。 Alt は上 press block で delete 済なので
                // ここで Alt 真値の path は来ない前提だが、 防衛的に `!ad.last_alt` で除外する。
                let press_shift = ad.start_modifiers.shift;
                let press_ctrl = ad.start_modifiers.ctrl;
                let prev = selected_automation_points.to_vec();
                let next: Vec<AutomationPointKey> = if press_shift || press_ctrl {
                    // toggle: prev XOR {pressed}
                    toggle_selection(&prev, ad.point)
                } else {
                    // replace: vec![pressed] (pressed が既に唯一の selection なら同値 → no-op)
                    vec![ad.point]
                };
                // 修飾なし単一クリック (= 置き換え選択) では競合する clip 選択も解除して
                // automation 面を 1 つだけにする (点とクリップが同時に光って混乱するのを
                // 防ぐ。 lasso / Shift / Ctrl は両面選択を温存するので除外)。
                if !press_shift && !press_ctrl && !selected_automation_clips.is_empty() {
                    ui.push_edit({ let v_prev = selected_automation_clips.to_vec(); let v_next = Vec::new(); Edit::mutate(move |app: &mut AppData| { let prev_model: Vec<common::model::AutomationClipKey> = v_prev.into_iter().map(widget_to_model_clip_key).collect(); let next_model: Vec<common::model::AutomationClipKey> = v_next.into_iter().map(widget_to_model_clip_key).collect(); app.handle_event(AppEvent::SelectAutomationClips { prev: prev_model, next: next_model }); }) });
                    response.selection_changed = true;
                }
                if next != prev {
                    ui.push_edit({ let v_prev = prev; let v_next = next; Edit::mutate(move |app: &mut AppData| { let prev_model: Vec<AutomationPointKeyRef> = v_prev.into_iter().map(|k| AutomationPointKeyRef { track_id: k.clip.track, lane_id: k.clip.lane, clip_id: k.clip.clip, point_idx: k.point_idx }).collect(); let next_model: Vec<AutomationPointKeyRef> = v_next.into_iter().map(|k| AutomationPointKeyRef { track_id: k.clip.track, lane_id: k.clip.lane, clip_id: k.clip.clip, point_idx: k.point_idx }).collect(); app.handle_event(AppEvent::SelectAutomationPoints { prev: prev_model, next: next_model }); }) });
                    response.selection_changed = true;
                }
            }
        }

        // ---- M14 Phase 63n-9 (#033): automation_curve_param_drag release → SetAutomationCurveParam ----
        // anchor と preview の差分が 1e-4 未満なら no-op (= handle を click したけど drag しなかったケース、
        // = user 意図的に値を変えていない)。 そうでなければ `SetAutomationCurveParam { point, kind, prev, next }`
        // を 1 件発行 (caller の AppEvent は kind で `BezierTension` / `ExponentialBend` を分岐して
        // `clip.points[idx].curve = Bezier { tension: next }` or `Exponential { bend: next }` で commit)。
        if let Some(cd) = automation_curve_param_release
            && (cd.preview_value - cd.anchor_value).abs() > 1e-4
        {
            ui.push_edit({ let v_pt = cd.point; let v_kind = cd.kind; let v_pv = cd.anchor_value; let v_nv = cd.preview_value; Edit::mutate(move |app: &mut AppData| { let track_id = v_pt.clip.track; let lane_id = v_pt.clip.lane; let clip_id = v_pt.clip.clip; let point_idx = v_pt.point_idx; match v_kind { SetAutomationCurveParamKind::BezierTension => { app.handle_event(AppEvent::SetAutomationCurveBezierTension { track_id, lane_id, clip_id, point_idx, prev: v_pv, next: v_nv }); } SetAutomationCurveParamKind::ExponentialBend => { app.handle_event(AppEvent::SetAutomationCurveExponentialBend { track_id, lane_id, clip_id, point_idx, prev: v_pv, next: v_nv }); } } }) });
        }

        // ---- M14 Phase 63n-8 (#033): automation_lasso_drag release → SelectAutomationPoints ----
        // 空き lane zone で press → drag → release で発火。 next 計算は **press 時 modifier** で分岐:
        // - 修飾なし → replace (next = lasso 内 points、 旧 selection 破棄)
        // - Shift   → union  (next = prev ∪ lasso 内 points)
        // - Ctrl    → XOR    (next = prev XOR lasso 内 points = toggle inclusion)
        //
        // **dist < 4px の空き click 短 click 化**:
        // - 修飾なし → `next = vec![]` (clear、 空き click = selection clear、 既存 MIDI lanes_click と同 UX)
        // - Shift / Ctrl → no-op (selection 維持、 = 誤クリック保護)
        //
        // 「lasso rect 内に point の **中心** が含まれる」 を hit 判定 (#033 §C 仕様)。 visible_tracks
        // ベースで collapsed / invisible lane は対象外 (= 既存 `automation_point_at` の visible scope と整合)。
        if let Some(ls) = automation_lasso_release {
            let abs_w = (ls.last_mouse.0 - ls.anchor.0).abs();
            let abs_h = (ls.last_mouse.1 - ls.anchor.1).abs();
            let dist_lasso = abs_w + abs_h;
            let lasso_rect = Rect {
                x: ls.anchor.0.min(ls.last_mouse.0),
                y: ls.anchor.1.min(ls.last_mouse.1),
                w: abs_w,
                h: abs_h,
            };
            let prev = selected_automation_points.to_vec();
            let next: Vec<AutomationPointKey> = if dist_lasso < 4.0 {
                // 空き短 click — 修飾なしで clear、 Shift / Ctrl は no-op
                if ls.start_modifiers.shift || ls.start_modifiers.ctrl {
                    prev.clone()
                } else {
                    Vec::new()
                }
            } else {
                // lasso の点中心 hit 判定 (visible_tracks + visible lane scope)
                let inside = collect_points_in_rect(
                    visible_tracks,
                    press_tops,
                    view,
                    lanes,
                    lasso_rect,
                    style,
                );
                if ls.start_modifiers.shift {
                    // union (prev order を保持 + lasso 由来の新規だけ append)
                    let mut out = prev.clone();
                    for k in inside {
                        if !out.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else if ls.start_modifiers.ctrl {
                    // XOR (toggle inclusion): prev に在って lasso にも在る点を除く + prev に無くて lasso に在る点を追加
                    let mut out: Vec<AutomationPointKey> =
                        prev.iter().copied().filter(|k| !inside.contains(k)).collect();
                    for k in inside {
                        if !prev.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else {
                    inside // replace
                }
            };
            if next != prev {
                ui.push_edit({ let v_prev = prev; let v_next = next; Edit::mutate(move |app: &mut AppData| { let prev_model: Vec<AutomationPointKeyRef> = v_prev.into_iter().map(|k| AutomationPointKeyRef { track_id: k.clip.track, lane_id: k.clip.lane, clip_id: k.clip.clip, point_idx: k.point_idx }).collect(); let next_model: Vec<AutomationPointKeyRef> = v_next.into_iter().map(|k| AutomationPointKeyRef { track_id: k.clip.track, lane_id: k.clip.lane, clip_id: k.clip.clip, point_idx: k.point_idx }).collect(); app.handle_event(AppEvent::SelectAutomationPoints { prev: prev_model, next: next_model }); }) });
                response.selection_changed = true;
            }

            // daw_01 #071 (option 1): 同じ四角ドラッグで automation **clip** も選択する (点とクリップを
            // 同時に拾う = 何も失わず clip の範囲選択を上乗せ)。 修飾セマンティクスは点と完全対称
            // (修飾なし=replace / Shift=union / Ctrl=XOR、 空き短 click は修飾なしで clear・Shift/Ctrl で no-op)。
            // clip は rect 交差で hit (点は中心 hit)、 = MIDI clip marquee と同 `rects_intersect` 判定。
            let clip_prev = selected_automation_clips.to_vec();
            let clip_next: Vec<AutomationClipKey> = if dist_lasso < 4.0 {
                if ls.start_modifiers.shift || ls.start_modifiers.ctrl {
                    clip_prev.clone()
                } else {
                    Vec::new()
                }
            } else {
                let inside = collect_clips_in_rect(
                    visible_tracks,
                    press_tops,
                    view.track_row_h,
                    view,
                    header_pane.x,
                    header_pane.w,
                    lanes,
                    style,
                    lasso_rect,
                );
                if ls.start_modifiers.shift {
                    let mut out = clip_prev.clone();
                    for k in inside {
                        if !out.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else if ls.start_modifiers.ctrl {
                    let mut out: Vec<AutomationClipKey> =
                        clip_prev.iter().copied().filter(|k| !inside.contains(k)).collect();
                    for k in inside {
                        if !clip_prev.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else {
                    inside
                }
            };
            if clip_next != clip_prev {
                ui.push_edit({ let v_prev = clip_prev; let v_next = clip_next; Edit::mutate(move |app: &mut AppData| { let prev_model: Vec<common::model::AutomationClipKey> = v_prev.into_iter().map(widget_to_model_clip_key).collect(); let next_model: Vec<common::model::AutomationClipKey> = v_next.into_iter().map(widget_to_model_clip_key).collect(); app.handle_event(AppEvent::SelectAutomationClips { prev: prev_model, next: next_model }); }) });
                response.selection_changed = true;
            }
        }

        // ---- M14 Phase 63n-5 (#030): automation_lane_resize_drag release → SetLaneHeight ----
        // drag 中は per-frame emit で live update 済 (lane_default_drag と同 pattern)。
        // release frame は 1 度だけ最終値を `SetLaneHeight { prev: anchor, next: end }` で発行。
        // anchor と同値なら no-op (= ユーザが splitter を click したけど drag しなかったケース)。
        if let Some(rd) = lane_resize_drag_release {
            let dy = rd.last_mouse_y - rd.anchor_mouse_y;
            let raw = f32::from(rd.anchor_height_px) + dy;
            // M14 Phase 63n-6 (#031): release も runtime clamp (style.max ∧ lanes.h)。
            let end = clamp_height_px(
                raw,
                style.automation_lane_min_height_px,
                effective_lane_max_height(style, lanes),
            );
            if end != rd.anchor_height_px {
                ui.push_edit({ let v_lane = rd.lane; let v_prev = rd.anchor_height_px; let v_next = end; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetLaneHeight { track_id: v_lane.track, lane_id: v_lane.lane, prev_px: v_prev, next_px: v_next }); }) });
            }
        }

        // ---- M14 Phase 63n-3 (#028): automation_clip_drag release ----
        // commit-by-release: 短 click (Move + !Alt + dist < 4px) は **`SelectAutomationClips` に demote**
        // (= 既存 MIDI clip の `clip_short_click_pos` 経路と同 idiom、 lane body 上 click は automation
        // 選択に振る)。 それ以外は MoveAutomationClips / CloneAutomationClipsLinked /
        // CloneAutomationClipsIndependent / ResizeAutomationClips を発行。 modifier は session の
        // `last_*` を真値とし pointer.modifiers を直接見ない (race 回避、 ClipDragSession と同 pattern)。
        // beat_to_px は anchor 固定 `body_rect_anchor` から計算 (view scroll 耐性)、 absolute snap で
        // grid 吸着、 cross-lane Move は release y から `automation_lane_key_at_y` で drop lane 解決。
        if let Some(acd) = automation_clip_drag_release {
            let release_alt = acd.last_alt;
            let dx = acd.last_mouse.0 - acd.anchor_mouse.0;
            let dy = acd.last_mouse.1 - acd.anchor_mouse.1;
            let dist = dx.abs() + dy.abs();
            let demote =
                matches!(acd.kind, ClipDragKind::Move) && !release_alt && dist < 4.0;
            if demote {
                // short click on automation clip → 修飾で分岐 (#071): 修飾なし = 単一置換、
                // Shift / Ctrl = 選択足し引き (= 既に居れば外す、 居なければ足す toggle)。 MIDI clip
                // 同様 anchor 順は問わず、 掴んだ clip (= primary) を対象にする。
                let prev = selected_automation_clips.to_vec();
                let key = acd.primary;
                let next: Vec<AutomationClipKey> = if acd.last_shift || acd.last_ctrl {
                    if prev.contains(&key) {
                        prev.iter().copied().filter(|k| *k != key).collect()
                    } else {
                        let mut out = prev.clone();
                        out.push(key);
                        out
                    }
                } else {
                    vec![key]
                };
                // 修飾なし単一クリック (= 置き換え選択) では競合する point 選択も解除して
                // automation 面を 1 つだけにする (点とクリップが同時に光って混乱するのを
                // 防ぐ。 lasso / Shift / Ctrl は両面選択を温存するので除外)。
                if !acd.last_shift
                    && !acd.last_ctrl
                    && !selected_automation_points.is_empty()
                {
                    ui.push_edit({ let v_prev = selected_automation_points.to_vec(); let v_next: Vec<AutomationPointKey> = Vec::new(); Edit::mutate(move |app: &mut AppData| { let prev_model: Vec<AutomationPointKeyRef> = v_prev.into_iter().map(|k| AutomationPointKeyRef { track_id: k.clip.track, lane_id: k.clip.lane, clip_id: k.clip.clip, point_idx: k.point_idx }).collect(); let next_model: Vec<AutomationPointKeyRef> = v_next.into_iter().map(|k| AutomationPointKeyRef { track_id: k.clip.track, lane_id: k.clip.lane, clip_id: k.clip.clip, point_idx: k.point_idx }).collect(); app.handle_event(AppEvent::SelectAutomationPoints { prev: prev_model, next: next_model }); }) });
                    response.selection_changed = true;
                }
                if prev != next {
                    ui.push_edit({ let v_prev = prev; let v_next = next; Edit::mutate(move |app: &mut AppData| { let prev_model: Vec<common::model::AutomationClipKey> = v_prev.into_iter().map(widget_to_model_clip_key).collect(); let next_model: Vec<common::model::AutomationClipKey> = v_next.into_iter().map(widget_to_model_clip_key).collect(); app.handle_event(AppEvent::SelectAutomationClips { prev: prev_model, next: next_model }); }) });
                    response.selection_changed = true;
                }
            } else {
                // beat_to_px は現在フレームの lanes.w から算出 (全 lane body は幅 lanes.w で同一)。
                // press 時の anchor 幅でなく現幅を使うことで drag 中の resize に追従する。
                let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
                let raw_beat_delta = if beat_to_px > 1e-9 {
                    f64::from(dx) / beat_to_px
                } else {
                    0.0
                };
                // snap pivot = anchors[0] (= 掴んだ clip)、 overlay ghost と同 SSoT。
                let beat_delta = compute_automation_clip_drag_beat_delta(
                    &acd,
                    raw_beat_delta,
                    &view.snap,
                    zoom_x_px_per_beat,
                );
                let min_len = if view.snap.is_active(release_alt) {
                    view.snap
                        .beat_unit(zoom_x_px_per_beat)
                        .map_or(0.05, |u| u.max(0.05))
                } else {
                    0.05
                };
                // #071: cross-lane drop は単一選択 drag のみ (cursor 解決)。 複数選択一括は宛先 lane が
                // 一意でない (異種・可変高 lane) ため各 anchor は自 lane 維持の horizontal time-shift。
                let single = acd.anchors.len() == 1;
                match acd.kind {
                    ClipDragKind::Move => {
                        let cursor_lane = if single {
                            automation_lane_key_at_y(
                                visible_tracks,
                                press_tops,
                                view.track_row_h,
                                header_pane.x,
                                header_pane.w,
                                lanes.x,
                                lanes.w,
                                style,
                                acd.last_mouse.1,
                            )
                        } else {
                            None
                        };
                        let mut deltas: Vec<MoveAutomationClipDelta> = Vec::new();
                        for a in &acd.anchors {
                            let new_start = (a.start_beat + beat_delta).max(0.0);
                            let to_lane = cursor_lane.map_or(a.lane, |(lk, _body)| lk);
                            let moved = (new_start - a.start_beat).abs() > 1e-6 || to_lane != a.lane;
                            if moved {
                                deltas.push(MoveAutomationClipDelta {
                                    from: a.key,
                                    to_lane,
                                    prev_start_beat: a.start_beat,
                                    next_start_beat: new_start,
                                });
                            }
                        }
                        if !deltas.is_empty() {
                            let req = if acd.last_ctrl && acd.last_shift {
                                Edit::mutate(move |app: &mut AppData| {
                                    let entries = deltas
                                        .into_iter()
                                        .map(widget_to_model_clip_delta)
                                        .collect::<Vec<_>>();
                                    if !entries.is_empty() {
                                        app.handle_event(AppEvent::CloneAutomationClipsIndependent {
                                            deltas: entries,
                                        });
                                    }
                                })
                            } else if acd.last_ctrl {
                                Edit::mutate(move |app: &mut AppData| {
                                    let entries = deltas
                                        .into_iter()
                                        .map(widget_to_model_clip_delta)
                                        .collect::<Vec<_>>();
                                    if !entries.is_empty() {
                                        app.handle_event(AppEvent::CloneAutomationClipsLinked {
                                            deltas: entries,
                                        });
                                    }
                                })
                            } else {
                                Edit::mutate(move |app: &mut AppData| {
                                    let entries = deltas
                                        .into_iter()
                                        .map(widget_to_model_clip_delta)
                                        .collect::<Vec<_>>();
                                    if !entries.is_empty() {
                                        app.handle_event(AppEvent::MoveAutomationClips {
                                            deltas: entries,
                                        });
                                    }
                                })
                            };
                            ui.push_edit(req);
                        }
                    }
                    ClipDragKind::ResizeRight => {
                        let mut deltas: Vec<ResizeAutomationClipDelta> = Vec::new();
                        for a in &acd.anchors {
                            let new_len = (a.len_beats + beat_delta).max(min_len);
                            if (new_len - a.len_beats).abs() > 1e-6 {
                                deltas.push(ResizeAutomationClipDelta {
                                    key: a.key,
                                    prev_start: a.start_beat,
                                    prev_len: a.len_beats,
                                    next_start: a.start_beat,
                                    next_len: new_len,
                                });
                            }
                        }
                        if !deltas.is_empty() {
                            ui.push_edit({ let v_d = deltas; Edit::mutate(move |app: &mut AppData| { let entries = v_d.into_iter().map(|d| ResizeAutomationClipEntry { key: widget_to_model_clip_key(d.key), prev_start: d.prev_start, prev_len: d.prev_len, next_start: d.next_start, next_len: d.next_len }).collect::<Vec<_>>(); if !entries.is_empty() { app.handle_event(AppEvent::ResizeAutomationClips { deltas: entries }); } }) });
                        }
                    }
                    ClipDragKind::ResizeLeft => {
                        let mut deltas: Vec<ResizeAutomationClipDelta> = Vec::new();
                        for a in &acd.anchors {
                            let max_start = a.start_beat + a.len_beats - min_len;
                            let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                            let actual = new_start - a.start_beat;
                            let new_len = (a.len_beats - actual).max(min_len);
                            if (new_start - a.start_beat).abs() > 1e-6
                                || (new_len - a.len_beats).abs() > 1e-6
                            {
                                deltas.push(ResizeAutomationClipDelta {
                                    key: a.key,
                                    prev_start: a.start_beat,
                                    prev_len: a.len_beats,
                                    next_start: new_start,
                                    next_len: new_len,
                                });
                            }
                        }
                        if !deltas.is_empty() {
                            ui.push_edit({ let v_d = deltas; Edit::mutate(move |app: &mut AppData| { let entries = v_d.into_iter().map(|d| ResizeAutomationClipEntry { key: widget_to_model_clip_key(d.key), prev_start: d.prev_start, prev_len: d.prev_len, next_start: d.next_start, next_len: d.next_len }).collect::<Vec<_>>(); if !entries.is_empty() { app.handle_event(AppEvent::ResizeAutomationClips { deltas: entries }); } }) });
                        }
                    }
                }
            }
        }

        // ---- short click on lanes (drag<16px) → SelectClips ----
        if let Some(((cx, cy), click_ctrl, click_shift)) = clip_short_click_pos
            && lanes.contains(cx, cy)
        {
            let prev = selected_clips.to_vec();
            let next: Vec<ClipKey> =
                if let Some((hit_key, _)) = clip_hit(visible_tracks, press_tops, view, lanes, cx, cy, style.resize_handle_px) {
                    // daw_01 #93: clip click も **modifier-aware** に (track header の SelectTrack /
                    // 9.x 系 marquee と同 idiom)。旧実装は常に `vec![hit_key]` (Single 置換) で、
                    // Ctrl+click で複数クリップを選べなかった (multi-clip piano roll の前提が崩れていた)。
                    //   - Ctrl  = Toggle (XOR: 選択に居れば外し、 居なければ末尾 = anchor として足す)
                    //   - Shift = Union  (足すだけ、 既存ぶんは末尾へ繰り上げて anchor に)
                    //   - 無修飾 = Single (置換)
                    // anchor は常に末尾 (caller の `SetClipSelection` が `next.last()` を anchor にする)。
                    // modifier は session の careful-update 値 (release frame の生読みは race)。
                    if click_ctrl {
                        let mut n = prev.clone();
                        if let Some(pos) = n.iter().position(|k| *k == hit_key) {
                            n.remove(pos);
                        } else {
                            n.push(hit_key);
                        }
                        n
                    } else if click_shift {
                        let mut n = prev.clone();
                        n.retain(|k| *k != hit_key);
                        n.push(hit_key);
                        n
                    } else {
                        vec![hit_key]
                    }
                } else {
                    Vec::new()
                };
            if prev != next {
                ui.push_edit({ let v_next = next; Edit::mutate(move |app: &mut AppData| { let next_refs: Vec<ClipRef> = v_next.iter().filter_map(|key| clip_key_to_ref(app, *key)).collect(); app.handle_event(AppEvent::SetClipSelection(next_refs)); }) });
                response.selection_changed = true;
            }
            if let Some(idx) = track_index_from_y(cy, lanes.y, press_tops)
                && let Some(t) = visible_tracks.get(idx)
            {
                let beat = px_to_beat(cx, lanes.x, lanes.w, view);
                response.clicked_at_track_beat = Some((t.id, beat));
            }
        }

        // ---- M14 Phase 125 (#102) + daw_01 #75: drag marquee gate ----
        // 旧設計は rect-select 起動に Shift 必須だったが、 標準 DAW (REAPER/Live/Bitwig) に倣い
        // **空き zone を無修飾 drag → 範囲選択** にする。 修飾は release 時の next 計算で
        // plain=REPLACE / Shift=UNION / Ctrl=XOR に分岐し、 press 時 modifier は
        // `take_drag_rect_in_rect` が `DragRect.modifiers` に snapshot する (下の commit block で読む)。
        // gate を **clear の前** で評価して `marquee_active` を作り、 同フレーム二重 emit を防ぐ。
        //
        // #75: clip の **上から** でも範囲選択を開始できるようにする。 起動 zone を `marquee_zone_ok`
        // で判定する:
        //   - clip 無し (空き zone)              → 任意修飾で marquee (従来どおり)。
        //   - clip の **Move zone** + Shift+!Ctrl → marquee (NEW)。 plain Move / Ctrl(+Shift) clone /
        //                                           Shift+resize time-stretch とは排他。
        //   - clip の resize handle / その他      → marquee 不可 (time-stretch・clone・move に譲る)。
        // この zone 判定は press 側 clip_drag gate (#021 の `(!shift||ctrl)` / resize) と
        // 鏡像で、 marquee に入る press は press 側で clip_drag session を **起動しない** ものに限られる。
        // 二重防御として下の no-session ガード (全 session None) でも弾く。 automation lane は lasso が
        // 所有するため `!press_in_automation_lane` で除外 (no_session は `automation_lasso_drag` を
        // 含まないのでこの zone 除外が必須)。 splitter / 他 drag も no-session で除外。
        let drag_rect_wid = wid.child(b"rect_select");
        let shift_rect_active = {
            let state: &mut daw_ui_core::widgets::drag_rect::DragRectState =
                ui.widget_state(drag_rect_wid);
            state.drag_start.is_some()
        };
        let press_in_automation_lane = pointer.primary_just_pressed
            && pointer.pos.is_some_and(|(_, py)| {
                automation_lane_at(
                    visible_tracks,
                    press_tops,
                    view.track_row_h,
                    header_pane.x,
                    header_pane.w,
                    lanes.x,
                    lanes.w,
                    style,
                    py,
                )
                .is_some()
            });
        let marquee_zone_ok = pointer.primary_just_pressed
            && !pointer.modifiers.alt
            && !press_in_automation_lane
            && pointer.pos.is_some_and(|(px, py)| {
                lanes.contains(px, py)
                    && match clip_hit(
                        visible_tracks,
                        press_tops,
                        view,
                        lanes,
                        px,
                        py,
                        style.resize_handle_px,
                    ) {
                        None => true,
                        // #75: clip 本体 (Move zone) は Shift(Ctrl なし) のときだけ marquee 起動。
                        Some((_, ClipDragKind::Move)) => {
                            pointer.modifiers.shift && !pointer.modifiers.ctrl
                        }
                        // resize handle 上は time-stretch (#61) / resize に譲る。
                        Some(_) => false,
                    }
            });
        let marquee_press = if marquee_zone_ok {
            let s: &ArrangementState = ui.widget_state(wid);
            s.track_volume_drag.is_none()
                && s.track_reorder.is_none()
                && s.audio_drag.is_none()
                && s.clip_drag.is_none()
                && s.automation_point_drag.is_none()
                && s.automation_clip_drag.is_none()
                && s.automation_lane_resize_drag.is_none()
                && s.track_row_resize_drag.is_none()
                && s.playhead_drag.is_none()
                && s.loop_drag.is_none()
                && s.automation_curve_param_drag.is_none()
        } else {
            false
        };
        let marquee_active = marquee_press || shift_rect_active;

        // ---- pure release on empty lanes (no drag started) → SelectClips clear ----
        // clip_drag_session が無い + 空白 release + Shift なし。 #102: marquee がこの空き zone press を
        // 所有する frame (`marquee_active`) は下の commit が zero-rect REPLACE で clear するため、 ここでは
        // push しない (= 同フレーム二重 emit / undo 二重を防ぐ。 daw_01 #102「二重 emit 抑制」)。
        if pointer.primary_just_released
            && clip_short_click_pos.is_none()
            && !clip_drag_release_was_some
            && !pointer.modifiers.shift
            && !marquee_active
            && let Some((cx, cy)) = pointer.pos
            && lanes.contains(cx, cy)
            && clip_hit(visible_tracks, press_tops, view, lanes, cx, cy, style.resize_handle_px).is_none()
            && !selected_clips.is_empty()
        {
            ui.push_edit({ let v_next = Vec::new(); Edit::mutate(move |app: &mut AppData| { let next_refs: Vec<ClipRef> = v_next.iter().filter_map(|key| clip_key_to_ref(app, *key)).collect(); app.handle_event(AppEvent::SetClipSelection(next_refs)); }) });
            response.selection_changed = true;
        }

        // ---- loop drag release → SetLoopRange ----
        // M14 Phase 63j (#024): snap 適用済 endpoints を overlay と共通の helper で計算。
        // alt は `ld.last_alt` を真値とし、 release frame の `pointer.modifiers.alt` を直接見ない
        // (clip_drag と同じ理由 — OS event 順序で false 化する race を回避)。
        if let Some(ld) = loop_drag_release {
            let cur_beat = px_to_beat(ld.last_mouse_x, ruler.x, ruler.w, view);
            let (start, end) =
                compute_loop_drag_endpoints(&ld, cur_beat, &view.snap, zoom_x_px_per_beat);
            ui.push_edit({ let v_s = start; let v_e = end; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetLoopRange { start: v_s, end: v_e }); }) });
        }

        // ---- M14 Phase 63c (#016): track header drag release → SetTrackParent ----
        // dist < 16px → click 格下げ (modifier-aware SelectTrack に任せる、 後続 loop の clicked_track 経路)
        // dist >= 16px → 上で計算した `pending_drop` を SetTrackParent として 1 度発行。
        // 旧 ReorderTracks 経由の sibling reorder も同 variant に統合済 (parent 不変 + anchor_after 指定)。
        if let Some((src_tracks, parent, anchor_after)) = pending_drop {
            ui.push_edit({ let v_tracks = src_tracks; let v_parent = parent; let v_anchor = anchor_after; Edit::mutate(move |app: &mut AppData| { if !v_tracks.iter().any(|id| app.song_doc.song().tracks.iter().any(|t| t.id == *id)) { return; } app.edit_song(|song| { let mut moved: Vec<common::model::Track> = v_tracks.iter().filter_map(|id| { let pos = song.tracks.iter().position(|t| t.id == *id)?; Some(song.tracks.remove(pos)) }).collect(); if moved.is_empty() { return; } for t in &mut moved { t.parent_group_id = v_parent; } let insert_at = match v_anchor { None => 0, Some(after_id) => song.tracks.iter().position(|t| t.id == after_id).map(|i| i + 1).unwrap_or(song.tracks.len()) }; for (offset, t) in moved.into_iter().enumerate() { song.tracks.insert(insert_at + offset, t); } }); }) });
        }

        // ---- M10 Phase 47b+49: track volume drag release → 最終値を 1 度 commit ----
        // drag 中は per-frame Mutate で live update 済 (mixer fader と挙動同期)。
        // S4a: release frame は forward の Mutate を 1 度発行 (undo はアプリ層 SongDoc)。
        // (このファイルは S4b で daw_gui/src/widgets/ へ移設 + 全面書き換え予定。ここは Undoable
        //  variant 削除で compile を通すための最小変換。)
        if let Some(tv) = track_volume_release {
            let end = volume_from_mouse_x(tv.last_mouse_x, tv.band_rect.x, tv.band_rect.w);
            if (end - tv.anchor_volume).abs() > 1e-4 {
                ui.push_edit({ let v_track = tv.track_id; let v_next = end; Edit::mutate(move |app: &mut AppData| { let amp = MeterScale::default().frac_to_amp(v_next.clamp(0.0, 1.0)); app.handle_event(AppEvent::SetTrackVolume { track: v_track, amp }); }) });
            }
        }

        // ---- M14 Phase 125 (#102): marquee commit (modifier 分岐: plain=REPLACE / Shift=UNION / Ctrl=XOR) ----
        // gate `marquee_active` と `drag_rect_wid` は上の clear ガード手前で計算済 (空き zone press のみ所有)。
        // `take_drag_rect_in_rect` は呼ぶだけで cyan overlay を自動描画し、 press 時 modifier を
        // `DragRect.modifiers` に snapshot する。 release frame (`drag.finished`) に inside を計算して修飾で
        // next を分岐。 REPLACE は inside そのまま (zero-rect → 空 → 選択 clear)。 `prev != next` ガードで
        // no-op を抑制 (automation lasso #033 と同 idiom)。 Ctrl+Shift clone は clip HIT 時のみ (gate の
        // `clip_hit().is_none()`) なので、 ここに来る press は必ず空き zone = clone と競合しない。
        if marquee_active
            && let Some(drag) = ui.take_drag_rect_in_rect(drag_rect_wid, lanes)
        {
            response.rect_select_active = true;
            if drag.finished {
                let drag_rect = drag.rect();
                let mut inside: Vec<ClipKey> = Vec::new();
                for (i, t) in visible_tracks.iter().enumerate() {
                    let row_top = press_tops[i];
                    // 描画と同じ per-track 実効行高で交差判定する。
                    let row_h = effective_track_row_h(t, view.track_row_h);
                    for c in &t.clips {
                        let r = clip_to_rect(row_top, row_h, c, view, lanes);
                        if rects_intersect(r, drag_rect) {
                            inside.push(ClipKey { track: t.id, clip: c.id });
                        }
                    }
                }
                let prev: Vec<ClipKey> = selected_clips.to_vec();
                let next: Vec<ClipKey> = if drag.modifiers.shift {
                    // UNION: prev 順を保持しつつ inside の新規だけ append。
                    let mut out = prev.clone();
                    for k in inside {
                        if !out.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else if drag.modifiers.ctrl {
                    // XOR: prev に在って inside にも在る key を除き、 inside の新規を追加。
                    let mut out: Vec<ClipKey> =
                        prev.iter().copied().filter(|k| !inside.contains(k)).collect();
                    for k in inside {
                        if !prev.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else {
                    inside // REPLACE (zero-rect なら空 = clear)
                };
                if next != prev {
                    ui.push_edit({ let v_next = next; Edit::mutate(move |app: &mut AppData| { let next_refs: Vec<ClipRef> = v_next.iter().filter_map(|key| clip_key_to_ref(app, *key)).collect(); app.handle_event(AppEvent::SetClipSelection(next_refs)); }) });
                    response.selection_changed = true;
                }
            }
        }

        // ---- wheel: Ctrl=zoom_x / Alt=zoom_y (row_h) / Shift=scroll_x / plain=track_top ----
        // M14 Phase 104 (daw_01 #075): wheel を **ruler 下の content 全域** (`header_pane` + `lanes`) で
        // 取得する。 左の track header 列 (master row header / automation lane header を含む) の上でも
        // 縦操作 (plain=scroll / Alt=zoom_y) が効く。 横操作 (Ctrl=zoom_x / Shift=scroll_x) は beat anchor
        // (`mx - lanes.x`) が header 上 (`mx < lanes.x`) では意味を成さないため header 上では無視する
        // (= `over_lanes` で gate)。 lanes 上の 4 操作はすべて従来どおり (header_w==0 なら content 全域 ==
        // lanes、 over_lanes は常に true で旧挙動と byte 互換)。
        let content_below_ruler = Rect {
            x: header_pane.x,
            y: header_pane.y,
            w: header_pane.w + lanes.w,
            h: lanes_h,
        };
        let scroll = ui.take_scroll_in_rect(content_below_ruler);
        if scroll.1.abs() > 0.0 || scroll.0.abs() > 0.0 {
            let dy = scroll.1;
            // header pane 上 (`mx < lanes.x`) では横軸操作 (Ctrl / Shift) を無視。 pointer.pos は
            // take_scroll_in_rect が `content_below_ruler.contains` を満たして Some を保証済。
            let over_lanes = pointer.pos.is_some_and(|(mx, _)| mx >= lanes.x);
            if pointer.modifiers.ctrl && over_lanes {
                // M14 Phase 61a (#011): wheel up = zoom in (符号反転)、 1 ノッチで ~20% 変化
                // (係数 0.005 → 0.0015、 Cubase/Live 同等)、 SetZoomX を絶対値送信に統一
                // (旧設計は factor 0.55..1.82 を直送りで daw_01 の clamp(2, 400) で必ず 2 に
                // 張り付き ruler 1〜100 圧縮を起こしていた)。 SetTrackRowH と同パターン。
                // M14 Phase 61a follow-up: マウス位置を anchor に zoom (Cubase/Live 標準)、
                // SetScrollX を同 frame で発行して beat_at_mouse を維持。
                let factor = (dy * 0.0015).exp();
                let new_zoom = (zoom_x_px_per_beat * factor).clamp(0.1, 10000.0);
                if let Some((mx, _)) = pointer.pos {
                    let beat_at_mouse =
                        view.start_beat + f64::from(mx - lanes.x) * beat_per_px;
                    let new_beat_per_px = 1.0 / f64::from(new_zoom);
                    let new_start = beat_at_mouse - f64::from(mx - lanes.x) * new_beat_per_px;
                    ui.push_edit({ let v_b = new_start.max(0.0); Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetArrangeScroll(v_b as f32)); }) });
                }
                ui.push_edit({ let v_z = new_zoom; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetArrangeZoom(v_z)); }) });
            } else if pointer.modifiers.alt {
                // M10 Phase 48 / M14 Phase 61a (#011): Alt+wheel で row_h 縦ズーム (exp curve、
                // wheel up = zoom in)、 マウス y 位置を anchor に SetTrackTop で画面位置維持。
                // M14 Phase 63n-6 (#031): 加えて **automation lane の height_px も同 factor で scale** —
                // user feedback「Alt+wheel で MIDI track と automation lane が同時に変わってほしい」 を
                // 反映。 visible track の visible lane に `SetLaneHeight` を 1 件ずつ発行 (= caller が
                // 各 lane を update、 lane.height_px は per-track row_h と独立に持つので並列で OK)。
                let factor = (dy * 0.0015).exp();
                let new_h = view.track_row_h * factor;
                if let Some((_, my)) = pointer.pos
                    && view.track_row_h > 0.0
                {
                    let abs_pos = (f64::from(my - lanes.y) + f64::from(view.track_top))
                        / f64::from(view.track_row_h);
                    #[allow(clippy::cast_possible_truncation)]
                    let new_top =
                        (abs_pos * f64::from(new_h) - f64::from(my - lanes.y)).max(0.0) as f32;
                    ui.push_edit({ let v_t = new_top; Edit::mutate(move |app: &mut AppData| { app.ui_prefs.arrange_track_top = v_t.max(0.0); }) });
                }
                ui.push_edit({ let v_h = new_h; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetArrangeTrackRowH(v_h)); }) });

                // M14 Phase 63n-6 (#031): visible lane / per-track row override も同 factor で scale。
                // user feedback「Track 4 を drag で大きくした後 Alt+wheel で縮めても override が
                // 残ったまま」 → 各 override を factor 倍する。 個別差は scale 中保持 (lane1=100,
                // lane2=60 → lane1=70, lane2=42)、 enough wheel で min に収束 (= 個別差は残るが、
                // ユーザは引き続き wheel で全体を縮められる)。
                let lane_min = style.automation_lane_min_height_px;
                let lane_max = effective_lane_max_height(style, lanes);
                for t in visible_tracks {
                    // per-track row 高さ override (= `t.row_h.is_some()`) も factor 倍。 None
                    // (= view default 追従) は SetTrackRowH 経由で既に追従するので scale 不要。
                    if let Some(row_h) = t.row_h {
                        let scaled = f32::from(row_h) * factor;
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let new_t_h = scaled.round().clamp(1.0, f32::from(u16::MAX)) as u16;
                        if new_t_h != row_h {
                            ui.push_edit({ let v_track = t.id; let v_prev = row_h; let v_next = new_t_h; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetSingleTrackRowH { track_id: v_track, prev_px: v_prev, next_px: v_next }); }) });
                        }
                    }
                    if t.automation_lanes_collapsed {
                        continue;
                    }
                    for lane in &t.automation_lanes {
                        if !lane.visible || lane.height_px == 0 {
                            continue;
                        }
                        let scaled = f32::from(lane.height_px) * factor;
                        let new_lane_h = clamp_height_px(scaled, lane_min, lane_max);
                        if new_lane_h != lane.height_px {
                            ui.push_edit({ let v_lane = AutomationLaneKey {
                                    track: t.id,
                                    lane: lane.id,
                                }; let v_prev = lane.height_px; let v_next = new_lane_h; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetLaneHeight { track_id: v_lane.track, lane_id: v_lane.lane, prev_px: v_prev, next_px: v_next }); }) });
                        }
                    }
                }
            } else if pointer.modifiers.shift && over_lanes {
                let delta = -f64::from(dy) * beat_per_px * 4.0;
                ui.push_edit({ let v_b = view.start_beat + delta; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetArrangeScroll(v_b as f32)); }) });
            } else if !pointer.modifiers.ctrl && !pointer.modifiers.shift {
                // plain wheel (= 縦 scroll)。 header / lanes どちらの上でも同一挙動。 `!ctrl && !shift`
                // guard は header 上で横操作キーが押されているときに plain scroll へ落ちないため
                // (lanes 上では ctrl は上の分岐、 shift は直上の分岐で既に消費されここへ来ない)。
                // M14 Phase 115 (daw_01 #088): dy は入力層で px 化済 (LINE_HEIGHT_PX=40/line)。
                // 旧実装の追加 ×8 は二重スケール (1 ノッチ 320px ≈ 8 行) だったので撤去し、 scroll_area
                // と同じ「入力層の px delta をそのまま使う」 に揃える (1 ノッチ ≈ 40px ≈ 1 行)。
                let new_top = (view.track_top - dy).max(0.0);
                ui.push_edit({ let v_t = new_top; Edit::mutate(move |app: &mut AppData| { app.ui_prefs.arrange_track_top = v_t.max(0.0); }) });
            }
        }

        // ---- M14 Phase 127 (daw_01 #105): Arranger section drag release dispatch ----
        // overlay (preview) と同じ `compute_section_drag_beat_delta` で確定値を計算 (release で grid に
        // 飛ぶ不整合を構造的に回避)。 alt は session の `last_alt` を真値とする (clip_drag と同 pattern)。
        if let Some(sd) = section_drag_release {
            let raw_px_delta = sd.last_mouse.0 - sd.anchor_mouse.0;
            let raw_beat_delta = f64::from(raw_px_delta) * beat_per_px;
            let dist = raw_px_delta.abs();
            let delta =
                compute_section_drag_beat_delta(&sd, raw_beat_delta, &view.snap, zoom_x_px_per_beat);
            match sd.kind {
                SectionGesture::Create => {
                    // 範囲 drag のみ作成 (単純 click は dblclick が 1 bar 作成を担当)。
                    if dist >= 4.0 {
                        let other = (sd.anchor_press_beat + delta).max(0.0);
                        let start = sd.anchor_press_beat.min(other);
                        let len = (sd.anchor_press_beat - other).abs();
                        if len >= SECTION_MIN_LEN_BEATS {
                            ui.push_edit({ let v_start = start; let v_len = len; Edit::mutate(move |app: &mut AppData| { app.apply_create_section(v_start, v_len); }) });
                        }
                    }
                }
                SectionGesture::Move => {
                    if dist < 4.0 {
                        // M14 Phase 128 (#106): 短 click (jitter 未満) = 選択 + 帯ジャンプを併発。 drag して
                        // いないので Ctrl は Toggle-select (Duplicate は dist>=4 の Ctrl+drag のみ)。 modifier は
                        // Shift=RangeFromAnchor / Ctrl=Toggle / 無=Single (`SelectTrack` と同 idiom)。
                        let modifier = if sd.last_shift {
                            SelectModifier::RangeFromAnchor
                        } else if sd.last_ctrl {
                            SelectModifier::Toggle
                        } else {
                            SelectModifier::Single
                        };
                        ui.push_edit({ let v_id = sd.section_id; let v_mod = modifier; Edit::mutate(move |app: &mut AppData| { app.apply_select_section(v_id, v_mod); }) });
                        ui.push_edit({ let v_beat = sd.anchor_start.max(0.0); Edit::mutate(move |app: &mut AppData| { app.seek_playhead_to(v_beat); }) });
                    } else if sd.last_ctrl {
                        let next_start = (sd.anchor_start + delta).max(0.0);
                        ui.push_edit({ let v_id = sd.section_id; let v_dest = next_start; Edit::mutate(move |app: &mut AppData| { app.apply_duplicate_section(v_id, v_dest); }) });
                    } else {
                        let next_start = (sd.anchor_start + delta).max(0.0);
                        ui.push_edit({ let v_id = sd.section_id; let v_ns = next_start; Edit::mutate(move |app: &mut AppData| { app.apply_move_section(v_id, v_ns); }) });
                    }
                }
                SectionGesture::ResizeLeft => {
                    // 左端 drag: start/len 両方変化。 start は 0 以上 & 右端 - 最小長 を越えない sanity floor。
                    let right = sd.anchor_start + sd.anchor_len;
                    let next_start = (sd.anchor_start + delta)
                        .clamp(0.0, (right - SECTION_MIN_LEN_BEATS).max(0.0));
                    let next_len = (right - next_start).max(SECTION_MIN_LEN_BEATS);
                    ui.push_edit({ let v_id = sd.section_id; let v_ns = next_start; let v_nl = next_len; Edit::mutate(move |app: &mut AppData| { app.apply_resize_section(v_id, v_ns, v_nl); }) });
                }
                SectionGesture::ResizeRight => {
                    // 右端 drag: len のみ変化 (start 固定)。
                    let next_len = (sd.anchor_len + delta).max(SECTION_MIN_LEN_BEATS);
                    ui.push_edit({ let v_id = sd.section_id; let v_ns = sd.anchor_start; let v_nl = next_len; Edit::mutate(move |app: &mut AppData| { app.apply_resize_section(v_id, v_ns, v_nl); }) });
                }
            }
        }

        // ---- M14 Phase 127 (daw_01 #105): Arranger レーンの double-click ----
        //  - section 帯上 (in-rect) → `BeginRenameSection` (帯名 dblclick で改名開始、 `BeginRenameTrack` と同 idiom)
        //  - 空きレーン (帯の外、 隣接する resize ハンドル拡張部も含む) → `CreateSection` (既定長 1 bar)
        // rename 判定は `section_hit` (resize ハンドルを ±px 外側拡張) でなく `section_at_inrect`
        // (帯内のみ) を使う。 拡張ハンドルは drag の掴みやすさ用で、 「帯のすぐ隣の空白」 の dblclick を
        // 隣 section の rename に化けさせていた。 帯外の dblclick は空きレーン扱いで CreateSection に回る。
        if arranger_lane_h > 0.0
            && let Some((cx, cy)) = ui.take_double_click_in_rect(arranger_rect)
        {
            if let Some(sid) = section_at_inrect(sections, arranger_rect, view, cx, cy) {
                ui.push_edit({ let v_id = sid; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::BeginRenameSection(v_id)); }) });
            } else {
                let raw_beat = px_to_beat(cx, arranger_rect.x, arranger_rect.w, view);
                let start = view
                    .snap
                    .snap_beat(raw_beat, pointer.modifiers.alt, zoom_x_px_per_beat)
                    .max(0.0);
                ui.push_edit({ let v_start = start; let v_len = beats_per_bar(view.time_sig); Edit::mutate(move |app: &mut AppData| { app.apply_create_section(v_start, v_len); }) });
            }
        }

        // ---- M14 Phase 127 (daw_01 #105): Arranger レーンの secondary (右) click ----
        // section 帯上 (in-rect) のみ `SecondaryClickSection { id, pos }` を発火 (caller が `pos` に
        // コンテキストメニューを開く、 `SecondaryClickEmpty` と同 idiom)。 空きレーン上の右クリックは no-op。
        // dblclick rename と同じく point gesture なので `section_at_inrect` (帯内のみ) を使う。
        // resize ハンドル拡張 (`section_hit`) だと帯のすぐ隣の空白の右クリックで隣 section のメニューが出る。
        if arranger_lane_h > 0.0
            && let Some((cx, cy)) = ui.take_secondary_press_in_rect(arranger_rect)
            && let Some(sid) = section_at_inrect(sections, arranger_rect, view, cx, cy)
        {
            ui.push_edit({ let v_id = sid; let v_pos = (cx, cy); Edit::mutate(move |app: &mut AppData| { app.ui_ephemeral.section_menu = Some((v_id, v_pos)); app.ui_ephemeral.section_menu_open = true; }) });
        }

        // ---- double-click (lanes 内で clip / lane body / 空白 track row) ----
        // M14 Phase 63n-2 (#028) + Phase 63n-4 (#029): priority 順:
        //  1. clip hit (track row 内 clip rect) → DoubleClickClip
        //  2. lane body 内 clip 内 (curve 描画域) → AddAutomationPoint (snap 適用)
        //  3. lane body 内 clip ギャップ (= cursor の絶対 beat が既存 clip の x 範囲に重ならない) →
        //     CreateAutomationClip (snap 適用、 default len は style.automation_clip_default_len_beats)
        //  4. track row の空き → DoubleClickEmpty
        //  lane padding 内 (clip と x overlap するが clip の縦 padding zone) は no-op (= ユーザの意図が
        //  add-point か create-clip か判別できないため、 既存挙動を維持して何も発行しない)。
        if let Some((cx, cy)) = ui.take_double_click_in_rect(lanes) {
            if let Some((hit_key, _)) =
                clip_hit(visible_tracks, press_tops, view, lanes, cx, cy, style.resize_handle_px)
            {
                ui.push_edit({ let v_key = hit_key; Edit::mutate(move |app: &mut AppData| { if let Some(target) = clip_key_to_ref(app, v_key) { app.handle_event(AppEvent::SelectClip { target, additive: false }); if app.is_audio_clip(target) { app.handle_event(AppEvent::OpenAudioEditor(target)); } else { app.handle_event(AppEvent::CloseAudioEditor); app.handle_event(AppEvent::SelectBottomPanel(1)); } } }) });
            } else if let Some((pt_key, _)) = automation_point_at(
                visible_tracks,
                press_tops,
                view.track_row_h,
                view,
                header_pane.x,
                header_pane.w,
                lanes,
                cx,
                cy,
                style,
            ) {
                // 既存 point の上での dblclick → 値の数値入力を開始 (新規点追加より優先)。
                // caller (daw_01) が automation_point_rects で rect を引いて inline 数値入力 overlay を出す。
                ui.push_edit({ let v_key = pt_key; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::BeginEditAutomationPointValue { key: AutomationPointKeyRef { track_id: v_key.clip.track, lane_id: v_key.clip.lane, clip_id: v_key.clip.clip, point_idx: v_key.point_idx } }); }) });
            } else if let Some((t_idx, lane_idx, _h_rect, body_rect)) = automation_lane_at(
                visible_tracks,
                press_tops,
                view.track_row_h,
                header_pane.x,
                header_pane.w,
                lanes.x,
                lanes.w,
                style,
                cy,
            ) {
                let track_id = visible_tracks[t_idx].id;
                let lane = &visible_tracks[t_idx].automation_lanes[lane_idx];
                if let Some((clip_key, time_local, value_norm)) =
                    automation_clip_at(track_id, lane, body_rect, view, style, cx, cy)
                {
                    // (2) lane body 内 clip 内 → AddAutomationPoint
                    let clip_ref = lane.clips.iter().find(|c| c.id == clip_key.clip);
                    let clip_start = clip_ref.map_or(0.0, |c| c.start_beat);
                    let clip_len = clip_ref.map_or(0.0, |c| c.len_beats);
                    let raw_abs = clip_start + time_local;
                    let snapped_abs = view.snap.snap_beat(
                        raw_abs,
                        pointer.modifiers.alt,
                        zoom_x_px_per_beat,
                    );
                    let snapped_local =
                        (snapped_abs - clip_start).clamp(0.0, clip_len.max(0.0));
                    ui.push_edit({ let v_clip = clip_key; let v_tb = snapped_local; let v_vn = value_norm; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::AddAutomationPoint { track_id: v_clip.track, lane_id: v_clip.lane, clip_id: v_clip.clip, time_beat: v_tb, value_norm: v_vn }); }) });
                } else if cx >= body_rect.x && cx < body_rect.x + body_rect.w {
                    // (3) lane body 内 clip ギャップ → CreateAutomationClip。
                    // beat-domain で「cursor の絶対 beat が既存 clip と重なるか」 を判定し、
                    // 重ならない場合のみ発行する (= clip の縦 padding zone でも x が clip と重なって
                    // いれば抑止、 ユーザの意図が「padding を狙った add-point」 なのか「new clip」 なのか
                    // 判別できないため安全側 = no-op)。 cursor が clip の縦 padding 外で、 かつ x が
                    // 任意の clip と重ならない場合のみ「真の empty」 と判定。
                    let cursor_beat = px_to_beat(cx, lanes.x, lanes.w, view);
                    let on_existing_clip = lane.clips.iter().any(|c| {
                        cursor_beat >= c.start_beat && cursor_beat < c.start_beat + c.len_beats
                    });
                    if !on_existing_clip {
                        let snapped_start = view.snap.snap_beat(
                            cursor_beat,
                            pointer.modifiers.alt,
                            zoom_x_px_per_beat,
                        );
                        let lane_key = AutomationLaneKey {
                            track: track_id,
                            lane: lane.id,
                        };
                        ui.push_edit({ let v_lane = lane_key; let v_sb = snapped_start; let v_lb = style.automation_clip_default_len_beats; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::CreateAutomationClip { lane: common::model::AutomationLaneKey { track: v_lane.track, lane: v_lane.lane }, start_beat: v_sb, len_beats: v_lb }); }) });
                    }
                }
            } else if let Some(idx) = track_index_from_y(cy, lanes.y, press_tops)
                && let Some(t) = visible_tracks.get(idx)
                // M14 Phase 63n-10 (#034): master row 上で MIDI clip 作成 (`DoubleClickEmpty`) を発火しない
                // (= daw_01 #034 §G 確認、 master row の body 部は automation lane の clip dblclick のみ
                // 受け付け、 main row body は clip 概念を持たない)。
                && t.id != MASTER_TRACK_ID
            {
                // track row の空き dblclick (lane row では automation_lane_at が Some を返して
                // 上の分岐で吸収済 → ここに来るのは track row のみ)。
                let track_row_top = press_tops[idx];
                if cy < track_row_top + effective_track_row_h(t, view.track_row_h) {
                    let raw_beat = px_to_beat(cx, lanes.x, lanes.w, view);
                    // M9 Phase 45f: dblclick beat も widget 内 snap (#010 [Replied])。 daw_01 側で
                    // `beat.floor()` を消せるようになる。 single frame の click なので drag state は
                    // 関与せず、 直接 `pointer.modifiers.alt` を読んでよい。
                    let beat = view.snap.snap_beat(
                        raw_beat,
                        pointer.modifiers.alt,
                        zoom_x_px_per_beat,
                    );
                    ui.push_edit({ let v_track = t.id; let v_start = (beat).max(0.0); Edit::mutate(move |app: &mut AppData| { if let Some(t_idx) = app.song_doc.song().tracks.iter().position(|t| t.id == v_track) { app.handle_event(AppEvent::CreateClip { track: t_idx as u32, start_beat: v_start }); app.handle_event(AppEvent::SelectBottomPanel(1)); } }) });
                }
            }
        }

        // ---- secondary (右) click in 空きレーン → SecondaryClickEmpty (daw_01 #071) ----
        // `DoubleClickEmpty` と対になる secondary 版。 clip_hit / automation_lane_at に吸収
        // されない「真の空き track row」 上の右クリックのみ発火する (= 上の dblclick 経路の
        // 空き track row branch と同じ exclusion)。 clip / automation lane 上の右クリックは
        // caller (daw_01) の clip context menu 用に握りつぶさず素通しする (= take はするが
        // consume しない `take_secondary_press_in_rect` の設計)。 beat は widget 内で snap 済み、
        // pos は menu anchor 用の右クリック viewport 座標。
        if let Some((cx, cy)) = ui.take_secondary_press_in_rect(lanes) {
            let on_clip =
                clip_hit(visible_tracks, press_tops, view, lanes, cx, cy, style.resize_handle_px)
                    .is_some();
            let on_lane = automation_lane_at(
                visible_tracks,
                press_tops,
                view.track_row_h,
                header_pane.x,
                header_pane.w,
                lanes.x,
                lanes.w,
                style,
                cy,
            )
            .is_some();
            if !on_clip
                && !on_lane
                && let Some(idx) = track_index_from_y(cy, lanes.y, press_tops)
                && let Some(t) = visible_tracks.get(idx)
                // master row は clip 概念を持たないため発火しない (DoubleClickEmpty と同じ)。
                && t.id != MASTER_TRACK_ID
            {
                let track_row_top = press_tops[idx];
                if cy < track_row_top + effective_track_row_h(t, view.track_row_h) {
                    let raw_beat = px_to_beat(cx, lanes.x, lanes.w, view);
                    // dblclick と同じく widget 内 snap。 single frame の press なので drag state は
                    // 関与せず直接 `pointer.modifiers.alt` を読んでよい。
                    let beat =
                        view.snap.snap_beat(raw_beat, pointer.modifiers.alt, zoom_x_px_per_beat);
                    ui.push_edit({ let v_track = t.id; let v_beat = (beat).max(0.0); let v_pos = (cx, cy); Edit::mutate(move |app: &mut AppData| { app.ui_ephemeral.clip_create_menu = Some((v_track, v_beat, v_pos)); app.ui_ephemeral.clip_create_menu_open = true; }) });
                }
            }
        }
}
