//! S4b Phase D: arrangement widget の heavy (cached + overlay) 描画パス。
//! `arrangement()` の `ui.heavy(...)` closure body を抽出。 大量の per-frame capture を
//! 明示引数で受ける (immediate-mode の描画状態は 1 フレーム限りの値渡し)。

#![allow(clippy::too_many_arguments)]

use super::*;

pub(super) fn render_arrangement_heavy(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    tracks_owned: Arc<[ArrangementTrack]>,
    view_copy: ArrangementView,
    style_copy: ArrangementStyle,
    lanes: Rect,
    ruler: Rect,
    header_pane: Rect,
    header_pane_copy: Rect,
    arranger_rect_copy: Rect,
    arranger_header_rect_copy: Rect,
    arranger_lane_h_copy: f32,
    beat_per_px: f64,
    zoom_x_px_per_beat: f32,
    id_for_inner: u64,
    viewport_key_hash: u64,
    clip_content: HashMap<ClipKey, ClipContentDraw>,
    selected_set: HashSet<ClipKey>,
    selected_tracks_for_heavy: Vec<u32>,
    selected_automation_clips_set_for_heavy: HashSet<AutomationClipKey>,
    selected_automation_points_for_heavy: HashSet<AutomationPointKey>,
    mapping: TimeMapping,
    sample_viewport: ViewportState1D,
    grid_style: BarBeatGridStyle,
    ruler_style: TimeRulerStyle,
    drag_overlay_clone: Option<(ClipDragSession, f64, i32)>,
    drag_overlay_min_len: f64,
    audio_drag_overlay: Option<AudioDragSession>,
    point_drag_overlay: Option<AutomationPointDragSession>,
    automation_clip_drag_overlay: Option<AutomationClipDragSession>,
    curve_param_overlay: Option<AutomationCurveParamDragSession>,
    lasso_overlay: Option<AutomationLassoSession>,
    section_drag_overlay: Option<SectionDragSession>,
    sections_for_draw: Vec<SectionView>,
    reorder_overlay: Option<ReorderOverlay>,
    loop_preview_clone: Option<(f64, f64)>,
) {
            // M14 Phase 63n-1 (#028): heavy closure 内でも prefix sum tops を計算 ('static borrow が
            // 必要なため caller scope の tops_for_draw は持ち込めない、 同一 visibility を再計算)。
            let tops_owned_for_heavy = visible_track_row_tops(
                &tracks_owned,
                lanes.y,
                view_copy.track_top,
                view_copy.track_row_h,
            );
            // M14 Phase 77 (daw_01 #048): track_top に依存する draw を scope 単位で scissor。
            // `below_ruler` は ruler 下の領域 (= header_pane ∪ lanes)、 automation lane / reorder
            // overlay 等 header と lanes をまたぐ draw 用。 ruler / loop_band / playhead は
            // track_top に依存しない static draw なので scope 外に置いて既存挙動維持。
            let below_ruler = Rect {
                x: header_pane_copy.x.min(lanes.x),
                y: header_pane_copy.y,
                w: header_pane_copy.w + lanes.w,
                h: lanes.h,
            };
            // === cached: viewport_key 一致時 skip ===
            hctx.cached(viewport_key_hash, |hctx| {
                push_filled_rect(hctx, header_pane, style_copy.header_bg);
                // M14 Phase 77 (daw_01 #048): lanes scope (track row 系の y 依存 draw)。
                hctx.with_clip_rect(lanes, |hctx| {
                    draw_lanes_bg(
                        hctx,
                        lanes,
                        &tracks_owned,
                        &tops_owned_for_heavy,
                        view_copy,
                        &selected_tracks_for_heavy,
                        &style_copy,
                    );
                    hctx.ui_mut().bar_beat_grid(
                        ("arr_grid", id_for_inner),
                        lanes,
                        mapping,
                        sample_viewport,
                        grid_style,
                        // M14 Phase 124 (#100): subdivision はピアノロール限定なので arrangement は None。
                        None,
                    );
                    draw_clips(hctx, &tracks_owned, &tops_owned_for_heavy, view_copy, lanes, &style_copy);
                    // M14 Phase 63k (#025): audio_edit が Some の clip に dB handle line + fade envelope を重ねる。
                    // 描画は draw_clips 後 (clip rect の上に重なる)、 selection overlay より前 (selection の
                    // 黄色 fill が上書きしない、 selection 中も dB / fade が見える)。
                    let view_end_for_audio = view_copy.start_beat + view_copy.len_beats;
                    for (i, t) in tracks_owned.iter().enumerate() {
                        let row_top = tops_owned_for_heavy[i];
                        // draw_clips と同じ per-track 実効行高 (culling / rect の両方)。
                        let row_h = effective_track_row_h(t, view_copy.track_row_h);
                        if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
                            continue;
                        }
                        for c in &t.clips {
                            let Some(audio) = c.audio_edit else {
                                continue;
                            };
                            let end = c.start_beat + c.len_beats;
                            if end < view_copy.start_beat || c.start_beat > view_end_for_audio {
                                continue;
                            }
                            let r = clip_to_rect(row_top, row_h, c, view_copy, lanes);
                            if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                                continue;
                            }
                            if r.w < style_copy.audio_min_clip_w_for_handles_px {
                                continue;
                            }
                            draw_clip_audio_overlay(hctx, r, &audio, c.len_beats, &style_copy);
                        }
                    }
                });
                // M14 Phase 77 (daw_01 #048): ruler scope (static、 track_top に依存しない static
                // primitive だが defensive で wrap)。
                if view_copy.ruler_h > 0.0 {
                    hctx.with_clip_rect(ruler, |hctx| {
                        hctx.ui_mut().time_ruler(
                            ("arr_ruler", id_for_inner),
                            ruler,
                            mapping,
                            sample_viewport,
                            ruler_style,
                        );
                    });
                }

                // M14 Phase 63n-1 (#028): automation lane 行群の描画 (track 行の下、 expand されたもののみ)。
                // 各 visible track の `automation_lanes_collapsed = false` のとき、 visible lane を上から
                // 順に積む (header = lane 左端 / body = lane 右端 = clip 描画域と同 x)。 lane の y 範囲は
                // `tops[i] + track_row_h` から `tops[i+1]` (= 次 track 上端) の間。
                // 描画は cached 内: viewport_key に lane 関連 hash が入る前提 (fold_arrangement_clip_hash
                // を後ほど lane も含むように拡張する)。 現状は clip hash で大方の変化を検出可能。
                //
                // M14 Phase 77 (daw_01 #048): below_ruler scope (header_pane + lanes を跨ぐ draw 用)。
                // automation lane の背景 fill (line 4004-4013) は header_rect.x から body_rect 終端まで
                // span するので、 単独 lanes / header_pane scope では片側が切られる。
                hctx.with_clip_rect(below_ruler, |hctx| {
                    for (i, t) in tracks_owned.iter().enumerate() {
                        if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
                            continue;
                        }
                        let track_row_top = tops_owned_for_heavy[i];
                        // viewport culling: track 領域全体 (track + lanes) が viewport 外なら skip
                        let track_total_bottom = tops_owned_for_heavy[i + 1];
                        if track_total_bottom < lanes.y || track_row_top > lanes.y + lanes.h {
                            continue;
                        }
                        let mut lane_y = track_row_top + effective_track_row_h(t, view_copy.track_row_h);
                        // M14 Phase 63n-1 (#028) follow-up: lane 行 header は親 track と同じ indent に揃える
                        // (= 親 track の depth * indent_px)。 group 配下の track の lane が「どの track の
                        // lane か」 を視覚的に追えるようにするため (#028 user 指摘 1)。
                        let header_indent = f32::from(t.depth) * style_copy.indent_px;
                        for lane in &t.automation_lanes {
                            if !lane.visible {
                                continue;
                            }
                            let lh = f32::from(lane.height_px);
                            // lane 行 viewport culling
                            if lane_y + lh < lanes.y || lane_y > lanes.y + lanes.h {
                                lane_y += lh;
                                continue;
                            }
                            let header_rect = Rect {
                                x: header_pane_copy.x + header_indent,
                                y: lane_y,
                                w: (header_pane_copy.w - header_indent).max(2.0),
                                h: lh,
                            };
                            let body_rect = Rect {
                                x: lanes.x,
                                y: lane_y,
                                w: lanes.w,
                                h: lh,
                            };
                            draw_automation_lane(
                                hctx,
                                t.id,
                                lane,
                                header_rect,
                                body_rect,
                                view_copy,
                                &style_copy,
                                lanes,
                                &selected_automation_clips_set_for_heavy,
                            );
                            lane_y += lh;
                        }
                    }
                });
            });

            // === cached 外: clip content (波形 / MIDI) → selection / drag preview / playhead / loop band ===
            // S4b Phase C: clip の中身 (audio 波形 / MIDI ノート) を clip クロームの直上・selection
            // overlay の直下に、 name 帯と共有する `clip_content_inset_top` で描く (旧 app 側 rect +
            // hardcode inset 重ね描きを撤去、 inset SSoT 化)。 decode 完了で毎フレーム反映されるよう
            // cached の外で描く。
            hctx.with_clip_rect(lanes, |hctx| {
                let inset = clip_content_inset_top(&style_copy);
                let view_end = view_copy.start_beat + view_copy.len_beats;
                for (i, t) in tracks_owned.iter().enumerate() {
                    let row_top = tops_owned_for_heavy[i];
                    let row_h = effective_track_row_h(t, view_copy.track_row_h);
                    if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
                        continue;
                    }
                    for c in &t.clips {
                        let key = ClipKey { track: t.id, clip: c.id };
                        let Some(content) = clip_content.get(&key) else {
                            continue;
                        };
                        let end = c.start_beat + c.len_beats;
                        if end < view_copy.start_beat || c.start_beat > view_end {
                            continue;
                        }
                        let r = clip_to_rect(row_top, row_h, c, view_copy, lanes);
                        if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                            continue;
                        }
                        let is_selected = selected_set.contains(&key);
                        match content {
                            ClipContentDraw::Audio {
                                buffer,
                                start_frames,
                                end_frames,
                                source_id,
                            } => {
                                draw_clip_waveform_inner(
                                    hctx,
                                    key,
                                    r,
                                    buffer,
                                    *start_frames,
                                    *end_frames,
                                    *source_id,
                                    is_selected,
                                    lanes.x,
                                    inset,
                                    &style_copy,
                                );
                            }
                            ClipContentDraw::Midi { notes, len_beats } => {
                                let clip_bg = if is_selected {
                                    style_copy.clip_selected_fill
                                } else {
                                    c.color.unwrap_or(style_copy.clip_default_fill)
                                };
                                draw_clip_midi_inner(
                                    hctx,
                                    r,
                                    notes,
                                    *len_beats,
                                    clip_bg,
                                    &style_copy,
                                    lanes.x,
                                    inset,
                                );
                            }
                        }
                    }
                }
            });

            // M14 Phase 77 (daw_01 #048): track_top に依存する overlay 群を below_ruler scope で
            // wrap。 loop_band / playhead は static (ruler / spans ruler+lanes) なので scope 外。
            hctx.with_clip_rect(below_ruler, |hctx| {
            // M14 Phase 96 (daw_01 #068): 連動ハイライトは selection overlay の **前** に描画
            // (選択中 member は黄塗りが上書き優先、 非選択の同グループ member が hue 強調の主役)。
            draw_active_group_overlay(
                hctx,
                &tracks_owned,
                &tops_owned_for_heavy,
                view_copy,
                lanes,
                &style_copy,
            );
            draw_selection_overlay(
                hctx,
                &tracks_owned,
                &tops_owned_for_heavy,
                &selected_set,
                view_copy,
                lanes,
                &style_copy,
            );
            if let Some((nd, bd, td)) = drag_overlay_clone {
                draw_drag_preview(
                    hctx,
                    &nd,
                    &tracks_owned,
                    &tops_owned_for_heavy,
                    view_copy,
                    lanes,
                    &style_copy,
                    tracks_owned.len(),
                    bd,
                    td,
                    drag_overlay_min_len,
                );
            }
            // M14 Phase 63k (#025): audio_drag ghost overlay (drag 中の dB / fade preview + label)。
            // commit-by-release のため clip_rect_anchor + 計算済 outcome から preview rect / line を
            // 描き直す。 cached 外なので 1 frame 1 描画 (drag 中のみ)、 release frame で session が
            // take されてから次 frame は ghost 消滅。 base 描画 (cached 内) も同 frame 表示されるが、
            // ghost が上に重なって最新値を user に見せる。
            if let Some(ad) = audio_drag_overlay {
                draw_audio_drag_ghost(hctx, &ad, beat_per_px, &style_copy);
            }
            // M14 Phase 63n-2 (#028): automation_point_drag ghost (新位置の point dot を半透明で重ねる)。
            // anchor 固定の `body_rect_anchor` / `clip_rect_anchor` で beat_to_px / y 軸を計算 (drag
            // 中の view scroll 耐性)。 release commit と同じ式で next position を出すため SSoT を
            // 共有 (commit と overlay が同一値で確定)。 alt は session の `last_alt` を真値とする。
            if let Some(pd) = point_drag_overlay {
                let dx = pd.last_mouse.0 - pd.anchor_mouse.0;
                let dy = pd.last_mouse.1 - pd.anchor_mouse.1;
                let beat_to_px =
                    f64::from(pd.body_rect_anchor.w) / view_copy.len_beats.max(1e-6);
                let raw_dt = f64::from(dx) / beat_to_px;
                let raw_abs = pd.clip_start_beat + pd.anchor_time_beat + raw_dt;
                let snapped_abs =
                    view_copy.snap.snap_beat(raw_abs, pd.last_alt, zoom_x_px_per_beat);
                let next_local =
                    (snapped_abs - pd.clip_start_beat).clamp(0.0, pd.clip_len_beats.max(0.0));
                let next_value = (pd.anchor_value_norm
                    - dy / pd.clip_rect_anchor.h.max(1.0))
                .clamp(0.0, 1.0);
                let abs_beat = pd.clip_start_beat + next_local;
                #[allow(clippy::cast_possible_truncation)]
                let px = pd.body_rect_anchor.x
                    + ((abs_beat - view_copy.start_beat) * beat_to_px) as f32;
                let py = pd.clip_rect_anchor.y + (1.0 - next_value) * pd.clip_rect_anchor.h;
                let r = style_copy.automation_point_radius_px;
                hctx.push_rect(RectCommand {
                    rect: Rect { x: px - r, y: py - r, w: r * 2.0, h: r * 2.0 },
                    fill: style_copy.clip_selected_fill,
                    border: theme::TEXT,
                    border_width: 1.5,
                    radius: [r; 4],
                    clip_rect: Some(pd.body_rect_anchor),
                });
            }
            // M14 Phase 63n-3 (#028): automation_clip_drag ghost (drag 中の preview rect、 cross-lane
            // drop 解決込み)。 fill / border / badge は MIDI clip drag preview と完全対称。
            if let Some(acd) = automation_clip_drag_overlay {
                let is_move_clone =
                    matches!(acd.kind, ClipDragKind::Move) && acd.last_ctrl;
                let (fill, border, badge_glyph) = if is_move_clone {
                    if acd.last_shift {
                        (
                            style_copy.clip_clone_indep_fill,
                            style_copy.clip_clone_indep_border,
                            Some('+'),
                        )
                    } else {
                        (
                            style_copy.clip_clone_linked_fill,
                            style_copy.clip_clone_linked_border,
                            Some('⇌'),
                        )
                    }
                } else {
                    (
                        style_copy.clip_selected_fill,
                        style_copy.clip_selected_border,
                        None,
                    )
                };
                // beat_to_px は現在フレームの lanes.w から算出 (全 lane body は幅 lanes.w で同一、
                // for_each_visible_lane 参照)。 press 時の anchor 幅でなく現幅を使うことで drag 中の
                // window / header resize に追従する。
                let beat_to_px = f64::from(lanes.w) / view_copy.len_beats.max(1e-6);
                let raw_beat_delta = if beat_to_px > 1e-9 {
                    f64::from(acd.last_mouse.0 - acd.anchor_mouse.0) / beat_to_px
                } else {
                    0.0
                };
                // snap pivot = anchors[0] (= 掴んだ clip)、 release commit と同 SSoT。
                let beat_delta = compute_automation_clip_drag_beat_delta(
                    &acd,
                    raw_beat_delta,
                    &view_copy.snap,
                    zoom_x_px_per_beat,
                );
                let min_len = if view_copy.snap.is_active(acd.last_alt) {
                    view_copy
                        .snap
                        .beat_unit(zoom_x_px_per_beat)
                        .map_or(0.05, |u| u.max(0.05))
                } else {
                    0.05
                };
                // #071: 単一選択は cursor で cross-lane drop を preview、 複数選択は各 anchor の自 lane に
                // 留め horizontal time-shift を preview (release commit の cross-lane policy と一致)。
                let single = acd.anchors.len() == 1;
                let pad = style_copy.automation_clip_v_pad_px;
                for a in &acd.anchors {
                    let (g_start, g_len) = match acd.kind {
                        ClipDragKind::Move => ((a.start_beat + beat_delta).max(0.0), a.len_beats),
                        ClipDragKind::ResizeRight => {
                            (a.start_beat, (a.len_beats + beat_delta).max(min_len))
                        }
                        ClipDragKind::ResizeLeft => {
                            let max_start = a.start_beat + a.len_beats - min_len;
                            let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                            let actual = new_start - a.start_beat;
                            (new_start, (a.len_beats - actual).max(min_len))
                        }
                    };
                    let target_body = if single && matches!(acd.kind, ClipDragKind::Move) {
                        automation_lane_key_at_y(
                            &tracks_owned,
                            &tops_owned_for_heavy,
                            view_copy.track_row_h,
                            header_pane_copy.x,
                            header_pane_copy.w,
                            lanes.x,
                            lanes.w,
                            &style_copy,
                            acd.last_mouse.1,
                        )
                        .map_or(a.body_rect, |(_, body)| body)
                    } else {
                        a.body_rect
                    };
                    let g_clip_y = target_body.y + pad;
                    let g_clip_h = (target_body.h - pad * 2.0).max(2.0);
                    #[allow(clippy::cast_possible_truncation)]
                    let g_x =
                        target_body.x + ((g_start - view_copy.start_beat) * beat_to_px) as f32;
                    #[allow(clippy::cast_possible_truncation)]
                    let g_w = ((g_len * beat_to_px) as f32).max(2.0);
                    let ghost_rect = Rect { x: g_x, y: g_clip_y, w: g_w, h: g_clip_h };
                    if ghost_rect.x + ghost_rect.w >= lanes.x
                        && ghost_rect.x <= lanes.x + lanes.w
                    {
                        hctx.push_rect(RectCommand {
                            rect: ghost_rect,
                            fill,
                            border,
                            border_width: style_copy.clip_selected_border_w,
                            radius: [style_copy.clip_radius; 4],
                            clip_rect: Some(lanes),
                        });
                        if let Some(g) = badge_glyph
                            && ghost_rect.w > style_copy.clip_clone_badge_size + 4.0
                            && ghost_rect.h > style_copy.clip_clone_badge_size + 2.0
                        {
                            hctx.push_text(GlyphArea {
                                text: Arc::from(g.to_string()),
                                left: ghost_rect.x + 4.0,
                                top: ghost_rect.y + 2.0,
                                font_size: style_copy.clip_clone_badge_size,
                                line_height: style_copy.clip_clone_badge_size * 1.2,
                                color: style_copy.clip_clone_badge_color,
                                clip_rect: Some(ghost_rect),
                                ..GlyphArea::default()
                            });
                        }
                    }
                }
            }
            // M14 Phase 63n-8 (#033): selected automation points overlay (cached 外、 selection 変化のみで
            // 全 lane 再キャッシュは走らない設計)。 base draw (cached 内) は selection 不問の通常 dot を
            // 描く、 ここで selected な点だけを白色 + 大 dot で上書き (= base dot を完全に覆って差し替え)。
            // 描画式は `draw_automation_lane` の point dot と同 SSoT (`body_origin_x + abs_beat * beat_to_px`、
            // `clip_y + (1 - value_norm) * clip_h`)、 collapsed track / invisible lane は skip。
            if !selected_automation_points_for_heavy.is_empty() {
                let beat_to_px = f64::from(lanes.w) / view_copy.len_beats.max(1e-6);
                let r_sel = style_copy.automation_point_radius_selected_px;
                let pad = style_copy.automation_clip_v_pad_px;
                for (i, t) in tracks_owned.iter().enumerate() {
                    if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
                        continue;
                    }
                    let row_top = tops_owned_for_heavy[i];
                    let row_total_bottom = tops_owned_for_heavy[i + 1];
                    if row_total_bottom < lanes.y || row_top > lanes.y + lanes.h {
                        continue;
                    }
                    let mut lane_y =
                        row_top + effective_track_row_h(t, view_copy.track_row_h);
                    for lane in &t.automation_lanes {
                        if !lane.visible {
                            continue;
                        }
                        let lh = f32::from(lane.height_px);
                        if lane_y + lh < lanes.y || lane_y > lanes.y + lanes.h {
                            lane_y += lh;
                            continue;
                        }
                        let clip_y = lane_y + pad;
                        let clip_h = (lh - pad * 2.0).max(2.0);
                        for c in &lane.clips {
                            for (p_idx, p) in c.points.iter().enumerate() {
                                #[allow(clippy::cast_possible_truncation)]
                                let key = AutomationPointKey {
                                    clip: AutomationClipKey {
                                        track: t.id,
                                        lane: lane.id,
                                        clip: c.id,
                                    },
                                    point_idx: p_idx as u32,
                                };
                                if !selected_automation_points_for_heavy.contains(&key) {
                                    continue;
                                }
                                let abs_beat = c.start_beat + p.time_beat;
                                #[allow(clippy::cast_possible_truncation)]
                                let px = lanes.x
                                    + ((abs_beat - view_copy.start_beat) * beat_to_px) as f32;
                                let py =
                                    clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                                hctx.push_rect(RectCommand {
                                    rect: Rect {
                                        x: px - r_sel,
                                        y: py - r_sel,
                                        w: r_sel * 2.0,
                                        h: r_sel * 2.0,
                                    },
                                    fill: style_copy.automation_point_selected_fill,
                                    border: style_copy.automation_point_selected_border,
                                    border_width: 1.5,
                                    radius: [r_sel; 4],
                                    clip_rect: Some(Rect {
                                        x: lanes.x,
                                        y: lane_y,
                                        w: lanes.w,
                                        h: lh,
                                    }),
                                });
                            }
                        }
                        lane_y += lh;
                    }
                }
            }
            // M14 Phase 63n-9 (#033): selected point の Bezier / Exponential 入射 segment に handle を描画。
            // 描画式は `compute_curve_handle_pos` で SSoT、 drag 中は preview_value で handle 位置 + curve
            // segment を上書き (= cached layer の base curve を `automation_curve_param_preview_color` の
            // thicker line で覆って live preview)、 release frame で session take 済なら drag 終了。
            if !selected_automation_points_for_heavy.is_empty() {
                let beat_to_px = f64::from(lanes.w) / view_copy.len_beats.max(1e-6);
                let pad = style_copy.automation_clip_v_pad_px;
                let handle_r = style_copy.automation_curve_param_handle_radius_px;
                let handle_offset = style_copy.automation_curve_param_handle_offset_px;
                for (i, t) in tracks_owned.iter().enumerate() {
                    if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
                        continue;
                    }
                    let row_top = tops_owned_for_heavy[i];
                    let row_total_bottom = tops_owned_for_heavy[i + 1];
                    if row_total_bottom < lanes.y || row_top > lanes.y + lanes.h {
                        continue;
                    }
                    let mut lane_y =
                        row_top + effective_track_row_h(t, view_copy.track_row_h);
                    for lane in &t.automation_lanes {
                        if !lane.visible {
                            continue;
                        }
                        let lh = f32::from(lane.height_px);
                        if lane_y + lh < lanes.y || lane_y > lanes.y + lanes.h {
                            lane_y += lh;
                            continue;
                        }
                        let clip_y = lane_y + pad;
                        let clip_h = (lh - pad * 2.0).max(2.0);
                        let lane_clip = Rect {
                            x: lanes.x,
                            y: lane_y,
                            w: lanes.w,
                            h: lh,
                        };
                        for c in &lane.clips {
                            for p_idx in 1..c.points.len() {
                                #[allow(clippy::cast_possible_truncation)]
                                let key = AutomationPointKey {
                                    clip: AutomationClipKey {
                                        track: t.id,
                                        lane: lane.id,
                                        clip: c.id,
                                    },
                                    point_idx: p_idx as u32,
                                };
                                if !selected_automation_points_for_heavy.contains(&key) {
                                    continue;
                                }
                                let p = &c.points[p_idx];
                                let (kind, base_value) = match p.curve {
                                    ArrangementCurveKind::Bezier { tension } => (
                                        SetAutomationCurveParamKind::BezierTension,
                                        tension,
                                    ),
                                    ArrangementCurveKind::Exponential { bend } => (
                                        SetAutomationCurveParamKind::ExponentialBend,
                                        bend,
                                    ),
                                    _ => continue,
                                };
                                // drag 中 (= curve_param_overlay の point == 当該 key) なら preview_value、
                                // そうでなければ point の現在値 (= base_value)。 drag 中の handle のみが
                                // 動く (他の selected の handle は静止)。
                                let value = curve_param_overlay
                                    .as_ref()
                                    .filter(|cd| cd.point == key && cd.kind == kind)
                                    .map_or(base_value, |cd| cd.preview_value);
                                let prev = &c.points[p_idx - 1];
                                let prev_abs = c.start_beat + prev.time_beat;
                                let cur_abs = c.start_beat + p.time_beat;
                                #[allow(clippy::cast_possible_truncation)]
                                let prev_x = lanes.x
                                    + ((prev_abs - view_copy.start_beat) * beat_to_px) as f32;
                                #[allow(clippy::cast_possible_truncation)]
                                let cur_x = lanes.x
                                    + ((cur_abs - view_copy.start_beat) * beat_to_px) as f32;
                                let prev_y =
                                    clip_y + (1.0 - prev.value_norm.clamp(0.0, 1.0)) * clip_h;
                                let cur_y =
                                    clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                                // drag 中の preview curve segment を上書き描画 (= cached の base curve を
                                // 視覚的に置換、 line_width を +50% にして元線を覆う)。 drag 中の selected
                                // のみ 1 件描画 (他 selected の base curve は cached のまま)。
                                if curve_param_overlay
                                    .as_ref()
                                    .is_some_and(|cd| cd.point == key)
                                {
                                    let preview_kind_value = match kind {
                                        SetAutomationCurveParamKind::BezierTension => {
                                            ArrangementCurveKind::Bezier { tension: value }
                                        }
                                        SetAutomationCurveParamKind::ExponentialBend => {
                                            ArrangementCurveKind::Exponential { bend: value }
                                        }
                                    };
                                    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(32);
                                    pts.push((prev_x, prev_y));
                                    flatten_lane_segment(
                                        (prev_x, prev_y),
                                        (prev_x, prev_y),
                                        (cur_x, cur_y),
                                        (cur_x, cur_y),
                                        preview_kind_value,
                                        2.0,
                                        &mut pts,
                                    );
                                    let segs: Vec<daw_ui_renderer::LineSegment> = pts
                                        .windows(2)
                                        .map(|w| daw_ui_renderer::LineSegment {
                                            a: [w[0].0, w[0].1],
                                            b: [w[1].0, w[1].1],
                                            color: style_copy
                                                .automation_curve_param_preview_color,
                                        })
                                        .collect();
                                    hctx.push_lines(daw_ui_renderer::LineBatch {
                                        segments: segs.into(),
                                        line_width_px: style_copy
                                            .automation_curve_line_width_px
                                            * 1.5,
                                        clip_rect: Some(lane_clip),
                                    });
                                }
                                // handle dot 描画 (compute_curve_handle_pos と同 SSoT)。
                                let (hx, hy) = compute_curve_handle_pos(
                                    prev_x,
                                    prev_y,
                                    cur_x,
                                    cur_y,
                                    kind,
                                    value,
                                    handle_offset,
                                );
                                hctx.push_rect(RectCommand {
                                    rect: Rect {
                                        x: hx - handle_r,
                                        y: hy - handle_r,
                                        w: handle_r * 2.0,
                                        h: handle_r * 2.0,
                                    },
                                    fill: style_copy.automation_curve_param_handle_fill,
                                    border: style_copy.automation_curve_param_handle_border,
                                    border_width: 1.5,
                                    radius: [handle_r; 4],
                                    clip_rect: Some(lane_clip),
                                });
                            }
                        }
                        lane_y += lh;
                    }
                }
            }
            // M14 Phase 63n-8 (#033): lasso 矩形 overlay (drag 中のみ、 cached 外で半透明 cyan 系を描画)。
            // anchor から last_mouse の bounding rect を style.automation_lasso_fill / border で 1 度描画。
            // press と release が同 frame で起きる超短 click の場合、 session は release frame で take 済
            // = `lasso_overlay = None` で overlay 不描画 (= 即時消失、 user 視点で「click だけ」 と認識される)。
            if let Some(ls) = lasso_overlay {
                let rect = Rect {
                    x: ls.anchor.0.min(ls.last_mouse.0),
                    y: ls.anchor.1.min(ls.last_mouse.1),
                    w: (ls.anchor.0 - ls.last_mouse.0).abs(),
                    h: (ls.anchor.1 - ls.last_mouse.1).abs(),
                };
                hctx.push_rect(RectCommand {
                    rect,
                    fill: style_copy.automation_lasso_fill,
                    border: style_copy.automation_lasso_border,
                    border_width: 1.0,
                    radius: [0.0; 4],
                    clip_rect: Some(lanes),
                });
            }
            }); // end with_clip_rect(below_ruler)  for selection / drag / lasso overlays
            // loop band: drag preview がある場合は preview を描く、無ければ view.loop_range
            // M14 Phase 77: loop_band は ruler 領域、 track_top 不変なので scope 外で defensive wrap。
            if let Some(range) = loop_preview_clone.or(view_copy.loop_range) {
                draw_loop_band(
                    hctx,
                    range,
                    view_copy.start_beat,
                    view_copy.len_beats,
                    ruler,
                    style_copy.loop_band,
                    style_copy.loop_handle,
                    style_copy.loop_handle_w,
                );
            }
            // M14 Phase 127 (daw_01 #105): Arranger レーン (背景 + section 帯 + drag preview)。 loop band と
            // 同じく cached 外・track scroll 非依存なので below_ruler scope の外で描画 (ruler と lanes の間)。
            if arranger_lane_h_copy > 0.0 {
                draw_sections_lane(
                    hctx,
                    &sections_for_draw,
                    section_drag_overlay,
                    view_copy,
                    arranger_rect_copy,
                    arranger_header_rect_copy,
                    &view_copy.snap,
                    zoom_x_px_per_beat,
                    &style_copy,
                );
            }
            if let Some(b) = view_copy.playhead_beat
                && b >= view_copy.start_beat
                && b <= view_copy.start_beat + view_copy.len_beats
            {
                let beat_to_px = f64::from(lanes.w) / view_copy.len_beats.max(1e-6);
                #[allow(clippy::cast_possible_truncation)]
                let x = lanes.x + ((b - view_copy.start_beat) * beat_to_px) as f32;
                draw_playhead_line(
                    hctx,
                    x,
                    ruler.y,
                    lanes.y + lanes.h,
                    style_copy.playhead_color,
                    style_copy.playhead_width_px,
                );
            }

            // === M10 Phase 46 → 101 (daw_01 #072): track reorder drop indicator + preview ===
            // M14 Phase 77 (daw_01 #048): reorder overlay は header_pane + lanes を跨ぐ (横 1 行帯)
            // ので below_ruler scope で wrap (= ruler / toolbar への leak 防止)。
            //
            // M14 Phase 101 (daw_01 #072): 深さを可視化する。 (1) 着地先 group があれば header 行を
            // hilight (Cubase の緑矢印に相当)、 (2) indicator 横線の **左端を解決済み深さの indent 列に
            // 合わせる** (flush-left = top-level / 1 段右 = group の子)。 これらは `resolve_track_drop` の
            // 結果から事前計算済 (= commit と同じ着地位置を描く)。
            if let Some(ov) = reorder_overlay {
                hctx.with_clip_rect(below_ruler, |hctx| {
                    // (1) group-header hilight (nest 先の肯定フィードバック)。
                    if let Some(hl) = ov.highlight_row {
                        push_filled_rect(hctx, hl, style_copy.reorder_group_highlight);
                    }
                    // (2) 深さ連動 drop indicator 横線。 左端 = indent 列、 右端 = header + lanes。
                    let line_right = header_pane_copy.x + header_pane_copy.w + lanes.w;
                    let line_x = ov.indent_x.min(line_right - 1.0);
                    push_filled_rect(
                        hctx,
                        Rect {
                            x: line_x,
                            y: ov.indicator_y - style_copy.reorder_drop_indicator_h * 0.5,
                            w: (line_right - line_x).max(1.0),
                            h: style_copy.reorder_drop_indicator_h,
                        },
                        style_copy.reorder_drop_indicator,
                    );
                    // (3) dragging row 半透明複製 (header_pane 領域、last_mouse_y 中心)。
                    let row_h = view_copy.track_row_h;
                    let drag_y = (ov.drag_center_y - row_h * 0.5)
                        .clamp(header_pane_copy.y, header_pane_copy.y + header_pane_copy.h - row_h);
                    let alpha = style_copy.reorder_drag_alpha.clamp(0.0, 1.0);
                    let base_rgb = style_copy.track_selected_bg;
                    push_filled_rect(
                        hctx,
                        Rect { x: header_pane_copy.x, y: drag_y, w: header_pane_copy.w, h: row_h },
                        Color::rgba(base_rgb.r, base_rgb.g, base_rgb.b, alpha),
                    );
                });
            }
}
