//! S4b Phase D: arrangement widget の本体 (`arrangement()` fn — press/drag/release state
//! machine + heavy 描画 dispatch)。 helper・型は `use super::*` で親から継承する。

use super::*;

    #[allow(clippy::too_many_lines)]
pub fn arrangement(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) -> ArrangementResponse {
        // S4b: 入力ビューを AppData から直接構築 (旧 mirror 型 + make_edit 翻訳層を撤去)。
        let built = view_build::build(app, area);
        let tracks: &[ArrangementTrack] = &built.tracks;
        let sections: &[SectionView] = &built.sections;
        let view = built.view;
        let selected_clips: &[ClipKey] = &built.selected_clips;
        let selected_tracks: &[u32] = &built.selected_tracks;
        let selected_automation_clips: &[AutomationClipKey] = &built.selected_automation_clips;
        let selected_automation_points: &[AutomationPointKey] = &built.selected_automation_points;
        let style: &ArrangementStyle = &built.style;
        let master_row: Option<&ArrangementMasterRow> = Some(&built.master_row);
        let rect = area;
        let id = "arrangement";
        // auto-fit (X キー / Fit ボタン) 用に現フレームの canvas (lanes) サイズを記録 (旧 draw() 冒頭)。
        let canvas_size = (
            (area.w - app.ui_prefs.arrange_header_w).max(0.0),
            (area.h - view_build::RULER_H).max(0.0),
        );
        if app.ui_ephemeral.last_arrange_canvas_size != canvas_size {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.last_arrange_canvas_size = canvas_size;
            }));
        }
        let wid = WidgetId::ROOT.child((b"arrangement_widget", &id));
        let pointer = ui.pointer();

        // ---- rect 分割 ----
        let header_w = view.header_w.max(0.0);
        let ruler_h = view.ruler_h.max(0.0);
        // M14 Phase 127 (daw_01 #105): Arranger レーンを ruler の直下・track lanes の上に確保する。
        // `arranger_lane_h == 0.0` で従来レイアウトと完全一致 (レーン無し)。 track lanes / header_pane の
        // y 原点を arranger 分だけ下げることで track row (header / lanes 双方) が自動的に下にずれる
        // (`header_pane.y == lanes.y` の不変条件は維持 = press_tops を header / lanes で共有する前提)。
        let arranger_lane_h = view.arranger_lane_h.max(0.0);
        let lanes_h = (rect.h - ruler_h - arranger_lane_h).max(1.0);
        let lanes_w = (rect.w - header_w).max(1.0);
        let header_pane = Rect {
            x: rect.x,
            y: rect.y + ruler_h + arranger_lane_h,
            w: header_w,
            h: lanes_h,
        };
        let ruler =
            Rect { x: rect.x + header_w, y: rect.y, w: lanes_w, h: ruler_h };
        // Arranger レーン本体 (lanes 幅、 ruler 直下) と header 側の見出し領域 ("Arranger" ラベル用)。
        let arranger_rect =
            Rect { x: rect.x + header_w, y: rect.y + ruler_h, w: lanes_w, h: arranger_lane_h };
        let arranger_header_rect =
            Rect { x: rect.x, y: rect.y + ruler_h, w: header_w, h: arranger_lane_h };
        let lanes = Rect {
            x: rect.x + header_w,
            y: rect.y + ruler_h + arranger_lane_h,
            w: lanes_w,
            h: lanes_h,
        };

        // M9 Phase 45f / M14 Phase 63j (#024): snap 用 zoom = lanes.w / view.len_beats。
        // press 振り分け (ruler の playhead seek) でも snap 計算に必要なため、 後の overlay 計算と
        // 共有する目的で関数の頭で 1 度計算する。
        let beat_per_px = view.len_beats / f64::from(lanes.w.max(1.0));
        #[allow(clippy::cast_possible_truncation)]
        let zoom_x_px_per_beat: f32 = (1.0 / beat_per_px) as f32;

        // ---- response 初期 ----
        let mut response = ArrangementResponse {
            ruler_rect: ruler,
            ..Default::default()
        };

        // ---- M14 Phase 63c (#016): visible 領域 (collapsed 親の subtree skip) を pre-compute ----
        // press / drag / release / draw すべてが visible-domain の row index で動くように、
        // `tracks` (caller's 全 list) を visible-only に絞った Vec を作って以降で共有する。
        // `clip_to_rect` / `track_index_from_y` の `track_index` 引数は visible-idx と解釈される。
        // tracks_for_draw (heavy() / 描画用、 後述 optimistic reorder 適用版) も同じ visibility 集合。
        let visible_indices_press: Vec<usize> = compute_visible_indices(tracks);
        // M14 Phase 63n-10 (#034): master_row を synthetic `ArrangementTrack` (id = `MASTER_TRACK_ID`、
        // clips 空、 mute/solo false、 automation_lanes は master_row から複製) として `visible_tracks[0]`
        // に prepend。 既存 hit-test / 描画コードを **そのまま reuse** できる (= clips が空なので
        // MIDI/Audio clip drag は自然に no-op、 automation_lanes は通常 track と同 schema)。 「Master」
        // ラベル描画 / mute/solo button 非表示 / clip 系 EditRequest 抑制は描画 / 押下 path で
        // `t.id == MASTER_TRACK_ID` 分岐を入れて対処。
        //
        // `visible_indices_press` は **caller's tracks の index 列**で master の caller index は無いため
        // この Vec は変更しない (= 後段の clone source は `tracks` だが master 経路は別ロジック)。
        let mut visible_tracks: Vec<ArrangementTrack> = visible_indices_press
            .iter()
            .map(|&i| tracks[i].clone())
            .collect();
        if let Some(master) = master_row {
            visible_tracks.insert(0, synthesize_master_track(master));
        }
        // M14 Phase 63n-1 (#028): visible track の prefix-sum row tops。 lane 0 個 (= 既存挙動)
        // では `tops[i] = lanes.y - track_top + i * track_row_h` と等価。 expand 中の lane 群が
        // ある track 以降は次 track 以降の row top が下にずれる (= 描画 / hit-test SSoT)。
        // M14 Phase 63n-10 (#034): `visible_tracks[0]` に master_row が prepend されていれば、 master の
        // 高さ + lanes 高さ込みの prefix sum が自動で組まれる (= 通常 track と同じ helper を再利用)。
        let press_tops =
            visible_track_row_tops(&visible_tracks, lanes.y, view.track_top, view.track_row_h);
        // M14 Phase 63c (#016): collapsed 後でも「Group A は子を持つ track」 と判定するため、
        // **caller の full `tracks`** から「他 track の parent_id として参照されている id 集合」 を 1 度計算。
        // `is_group_track(id, visible_tracks)` だと collapsed で children が filter outされ false 化する罠を回避。
        let is_group_set: HashSet<u32> =
            tracks.iter().filter_map(|t| t.parent_id).collect();

        // ---- press 振り分け: audio_drag / clip_drag / loop_drag / playhead_drag を state に積む ----
        // M14 Phase 63j (#024): ruler の plain click は press frame で `SetPlayheadBeat` を 1 度
        // 発火する (continuation は後段の per-frame block 経由)。 press block 内では state borrow が
        // 走るため `push_edit` は呼べず、 `press_seek_beat` に貯めて press block を抜けてから 1 度発行。
        let mut press_seek_beat: Option<f64> = None;
        // M14 Phase 63n-1 (#028): track 行右端の lane disclosure click 検出。 press block 終了後に
        // `ToggleTrackAutomationCollapsed { track }` を 1 度発行 (`press_seek_beat` と同パターン)。
        let mut press_lane_toggle: Option<u32> = None;
        // M14 Phase 63n-2 (#028): lane header の icon click action。 press block 終了後に push_edit。
        // None で何も起きず、 Some で 1 度発行 (重複 click は最初の lane を優先 = early break)。
        let mut press_lane_button: Option<Edit<AppData>> = None;
        // M14 Phase 63n-2 (#028): Alt+click on point → DeleteAutomationPoints の引数。
        let mut press_delete_point: Option<Edit<AppData>> = None;
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
        {
            let in_lanes = lanes.contains(px, py);
            let in_ruler = ruler.contains(px, py);
            let shift = pointer.modifiers.shift;
            let ctrl = pointer.modifiers.ctrl;
            // release frame で確定する click 系 (track header のトラック選択) 用の
            // press 時 modifier snapshot (`press_modifiers` doc 参照)。
            {
                let state: &mut ArrangementState = ui.widget_state(wid);
                state.press_modifiers = pointer.modifiers;
            }

            // M14 Phase 63n-5 (#030): lane 下端 splitter hit (= body x range × lane bottom edge ±handle)
            // を **最優先** で判定。 hit したら resize drag session を起動して以降の press logic を skip
            // (= audio grip / clip drag / point hit / track header と排他)。 modifier 無視 (Shift+drag /
            // Ctrl+drag でも resize は同じ意味で、 既存 modifier semantics と衝突する余地が無い)。
            let splitter_lane = automation_lane_resize_splitter_at(
                &visible_tracks,
                &press_tops,
                view.track_row_h,
                header_pane.x,
                header_pane.w,
                lanes.x,
                lanes.w,
                style,
                px,
                py,
            );
            let splitter_press = if let Some(lane_key) = splitter_lane {
                let anchor_h = visible_tracks
                    .iter()
                    .find(|t| t.id == lane_key.track)
                    .and_then(|t| t.automation_lanes.iter().find(|l| l.id == lane_key.lane))
                    .map_or(0_u16, |l| l.height_px);
                if anchor_h > 0 {
                    let state: &mut ArrangementState = ui.widget_state(wid);
                    state.automation_lane_resize_drag =
                        Some(AutomationLaneResizeDragSession {
                            lane: lane_key,
                            anchor_height_px: anchor_h,
                            anchor_mouse_y: py,
                            last_mouse_y: py,
                            last_emitted_height: anchor_h,
                        });
                    true
                } else {
                    false
                }
            } else if let Some(row_idx) = track_row_resize_splitter_at(
                &visible_tracks,
                &press_tops,
                view.track_row_h,
                lanes.x,
                lanes.w,
                style,
                px,
                py,
            ) {
                // M14 Phase 63n-6 (#031): track row 下端 splitter hit (lane splitter 不在の場合のみ)
                // → **per-track** row resize session 起動 (= splitter で hit した track のみが伸び縮み)。
                let t = &visible_tracks[row_idx];
                let anchor_row_h = effective_track_row_h(t, view.track_row_h);
                if anchor_row_h > 0.0 {
                    let state: &mut ArrangementState = ui.widget_state(wid);
                    state.track_row_resize_drag = Some(TrackRowResizeDragSession {
                        track: t.id,
                        anchor_row_h,
                        anchor_mouse_y: py,
                        last_mouse_y: py,
                        last_emitted_height: anchor_row_h,
                    });
                    true
                } else {
                    false
                }
            } else if header_resize_splitter_at(rect, header_w, style, px, py) {
                // M14 Phase 117 (daw_01 #091): header / lanes 境界 splitter hit (lane/row splitter 不在の
                // 場合のみ = lanes 左端 4px の角は lane/row resize を優先)。 → header 幅 resize session 起動。
                // 境界は arrangement 全高に張るので clip drag (in_lanes) / ruler seek (in_ruler) より優先
                // させる (両者は後段で `!splitter_press` gate 済)。
                let state: &mut ArrangementState = ui.widget_state(wid);
                state.header_resize_drag = Some(HeaderResizeDragSession {
                    anchor_header_w: header_w,
                    anchor_mouse_x: px,
                    last_mouse_x: px,
                    last_emitted_w: header_w,
                });
                true
            } else {
                false
            };

            // M14 Phase 63k (#025): audio gesture (gain handle / fade corner) を最優先で振り分ける。
            // audio grip にヒットしたら clip_drag (Move/Resize) は起動しない (排他) — `audio_grip_hit_in_lanes`
            // が先勝で priority 判定する。 modifier (Shift / Ctrl) は audio gesture では無視 (Bitwig spec
            // §3.5/§3.6 と整合、 modifier-free な直感的操作)。 audio_edit が None の clip ではこの
            // ブロックは即 None を返すため、 既存挙動 (MIDI / Vocal clip) は影響を受けない。
            let audio_press = if !splitter_press && in_lanes && !shift && !ctrl {
                audio_grip_hit_in_lanes(&visible_tracks, &press_tops, view, lanes, px, py, style)
            } else {
                None
            };
            if let Some((hit_key, grip)) = audio_press {
                if let Some((t_idx, t)) =
                    visible_tracks.iter().enumerate().find(|(_, t)| t.id == hit_key.track)
                    && let Some(c) = t.clips.iter().find(|c| c.id == hit_key.clip)
                {
                    // r.md #38: fade は掴んだ **その event** だけを対象にする。
                    let (kind, anchor_fade) = match grip {
                        AudioGripHit::GainHandleBand => (AudioDragKind::Gain, None),
                        AudioGripHit::FadeCornerIn { event_index } => (
                            AudioDragKind::FadeIn,
                            c.fades.iter().find(|f| f.event_index == event_index).copied(),
                        ),
                        AudioGripHit::FadeCornerOut { event_index } => (
                            AudioDragKind::FadeOut,
                            c.fades.iter().find(|f| f.event_index == event_index).copied(),
                        ),
                    };
                    let r_anchor = clip_to_rect(
                        press_tops[t_idx],
                        effective_track_row_h(t, view.track_row_h),
                        c,
                        view,
                        lanes,
                    );
                    // Gain は常に vertical lock 確定 (横 drag は無視)、 Fade は press 時 `None` で
                    // sticky direction 待ち (continuation で閾値超えた方向に lock)。
                    let locked_horizontal = match kind {
                        AudioDragKind::Gain => Some(false),
                        _ => None,
                    };
                    let state: &mut ArrangementState = ui.widget_state(wid);
                    state.audio_drag = Some(AudioDragSession {
                        key: hit_key,
                        kind,
                        anchor_gain_db: c.audio_edit.map_or(0.0, |a| a.gain_db),
                        anchor_fade,
                        clip_rect_anchor: r_anchor,
                        content_map_anchor: content_map(c, view, lanes),
                        clip_bg_anchor: draw::clip_effective_fill(c, t.kind, style),
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        locked_horizontal,
                    });
                }
            } else if !splitter_press
                && in_lanes
                && let Some((hit_key, kind)) =
                    clip_hit(&visible_tracks, &press_tops, view, lanes, px, py, style.resize_handle_px)
            {
                // r.md #35: 旧実装はここに `(!shift || ctrl || resize)` gate があり、 Shift+press を
                // clip_drag から弾いて marquee (#75) に渡していた。 その marquee は 0 サイズ矩形では
                // 何も拾わないため **Shift+click が完全に無反応** になっていた。 gate を外して
                // Shift+press でも drag session を張り、 release の短 click 格下げ経路
                // (`clip_short_click_pos` が `(ctrl, shift)` を持ち回る) で範囲選択に解決する。
                // Shift+Move ドラッグは通常の移動、 Shift+resize は従来どおり time-stretch (#61)。
                let drag_keys: Vec<ClipKey> = if selected_clips.contains(&hit_key) {
                    selected_clips.to_vec()
                } else {
                    vec![hit_key]
                };
                let mut anchors: Vec<ClipDragAnchor> = Vec::new();
                for k in &drag_keys {
                    // visible_tracks の visible-idx を anchor.track_index に保存 (release frame の
                    // delta 計算 + draw_drag_preview の new_idx も同じ visible-idx で動く)。
                    if let Some((t_idx, t)) =
                        visible_tracks.iter().enumerate().find(|(_, t)| t.id == k.track)
                        && let Some(c) = t.clips.iter().find(|c| c.id == k.clip)
                    {
                        anchors.push(ClipDragAnchor {
                            key: *k,
                            start_beat: c.start_beat,
                            len_beats: c.len_beats,
                            track_index: t_idx,
                        });
                    }
                }
                if !anchors.is_empty() {
                    let press_alt = pointer.modifiers.alt;
                    let press_ctrl = pointer.modifiers.ctrl;
                    let press_shift = pointer.modifiers.shift;
                    let state: &mut ArrangementState = ui.widget_state(wid);
                    state.clip_drag = Some(ClipDragSession {
                        kind,
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        last_alt: press_alt,
                        last_ctrl: press_ctrl,
                        last_shift: press_shift,
                        anchors,
                    });
                }
            }
            // M14 Phase 127 (daw_01 #105): Arranger レーン press 振り分け。 arranger_rect は ruler /
            // lanes / header_pane と y 領域が排他なので独立 block で扱う。 header 幅 splitter
            // (全高に張る) との競合のみ `!splitter_press` で回避 (clip / ruler と同 gate)。
            if !splitter_press && arranger_lane_h > 0.0 && arranger_rect.contains(px, py) {
                let press_alt = pointer.modifiers.alt;
                let press_ctrl = pointer.modifiers.ctrl;
                let press_shift = pointer.modifiers.shift;
                if let Some((sid, kind)) =
                    section_hit(sections, arranger_rect, view, px, py, style.resize_handle_px)
                    && let Some(s) = sections.iter().find(|s| s.id == sid)
                {
                    // 既存 section 上 → Move / Resize session (Ctrl は release で Duplicate に分岐)。
                    let gesture = match kind {
                        ClipDragKind::Move => SectionGesture::Move,
                        ClipDragKind::ResizeLeft => SectionGesture::ResizeLeft,
                        ClipDragKind::ResizeRight => SectionGesture::ResizeRight,
                    };
                    let state: &mut ArrangementState = ui.widget_state(wid);
                    state.section_drag = Some(SectionDragSession {
                        kind: gesture,
                        section_id: sid,
                        anchor_start: s.start_beat,
                        anchor_len: s.len_beats,
                        anchor_press_beat: px_to_beat(px, arranger_rect.x, arranger_rect.w, view),
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        last_alt: press_alt,
                        last_ctrl: press_ctrl,
                        last_shift: press_shift,
                    });
                } else {
                    // 空きレーン → 範囲 drag による新規作成 session (press 端を snap で grid に着地)。
                    // 単純 click (drag 距離 < 4px) は release で no-op、 新規作成は dblclick が担当する。
                    let raw = px_to_beat(px, arranger_rect.x, arranger_rect.w, view);
                    let anchor = view.snap.snap_beat(raw, press_alt, zoom_x_px_per_beat).max(0.0);
                    let state: &mut ArrangementState = ui.widget_state(wid);
                    state.section_drag = Some(SectionDragSession {
                        kind: SectionGesture::Create,
                        section_id: 0,
                        anchor_start: anchor,
                        anchor_len: 0.0,
                        anchor_press_beat: anchor,
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        last_alt: press_alt,
                        last_ctrl: press_ctrl,
                        last_shift: press_shift,
                    });
                }
            }
            if in_ruler && !splitter_press {
                // M14 Phase 117 (daw_01 #091): header splitter は arrangement 全高に張るので ruler 行の
                // 左端 (boundary ±handle/2) で splitter_press が立つ。 その frame は header 幅 resize を
                // 優先し playhead seek / loop edit は起動しない。
                let press_beat = px_to_beat(px, ruler.x, ruler.w, view);
                let press_alt = pointer.modifiers.alt;
                // M14 Phase 63j (#024): plain (= Shift 非保持) ruler 操作は **playhead seek**
                // (Bitwig / Reaper / Ableton 流)、 Shift 修飾で従来の loop range edit に振り分け。
                //
                // 旧設計 (M9 Phase 45e〜): plain ruler drag = loop NewRange / Middle drag、 loop 端
                // ハンドル drag = Start/End。 これでは「再生中の任意位置で split したい」 UX で
                // ruler 上に playhead を置く手段が無く、 daw_01 #024 で「ユーザビリティが壊滅的」 と報告。
                //
                // 新設計:
                //   - **plain ruler click/drag** → `SetPlayheadBeat` 連続発火 (snap 適用 + clamp ≥ 0)
                //   - **Shift + ruler drag** → loop edit (NewRange / 既存 loop の Start/End/Middle)
                // multi-track 系 widget で Shift は加算選択用なので潰さない設計判断、 ruler は
                // 単一軸で Shift の他用途が無いので loop ops 専用 modifier として再利用する。
                if shift {
                    let kind = if let Some(range) = view.loop_range {
                        match loop_band_hit_kind(range, view.start_beat, view.len_beats, ruler, px, 4.0) {
                            Some(LoopBandHit::Start) => LoopDragKind::Start,
                            Some(LoopBandHit::End) => LoopDragKind::End,
                            Some(LoopBandHit::Middle) => LoopDragKind::Middle,
                            None => LoopDragKind::NewRange,
                        }
                    } else {
                        LoopDragKind::NewRange
                    };
                    // M14 Phase 63j (#024): NewRange の anchor 端点は press 時 snap で
                    // grid に着地させる (caller 側 boilerplate を強要しない設計、 release 端点も
                    // `compute_loop_drag_endpoints` で snap される)。 既存 loop の Start/End/Middle
                    // drag は anchor が `view.loop_range` 由来 (= 既に commit 済 = grid 上前提) なので
                    // press 時 snap 不要、 raw `press_beat` を保持して Middle の delta 計算に使う。
                    let anchor_press_beat_for_session = match kind {
                        LoopDragKind::NewRange => view
                            .snap
                            .snap_beat(press_beat, press_alt, zoom_x_px_per_beat),
                        _ => press_beat,
                    };
                    let anchor_loop = view.loop_range.unwrap_or((
                        anchor_press_beat_for_session,
                        anchor_press_beat_for_session,
                    ));
                    let state: &mut ArrangementState = ui.widget_state(wid);
                    state.loop_drag = Some(LoopDragSession {
                        kind,
                        anchor_loop,
                        anchor_press_beat: anchor_press_beat_for_session,
                        anchor_mouse_x: px,
                        last_mouse_x: px,
                        last_alt: press_alt,
                    });
                } else {
                    // playhead seek session 開始 + press frame で 1 度発火 (continuation 発火は
                    // 後段の per-frame block が担当)。 snap は `MoveClips` と同 policy: alt 押下で
                    // 一時 OFF、 zoom_x_px_per_beat に対する Adaptive grid。
                    let snapped =
                        view.snap.snap_beat(press_beat, press_alt, zoom_x_px_per_beat).max(0.0);
                    let state: &mut ArrangementState = ui.widget_state(wid);
                    state.playhead_drag = Some(PlayheadDragSession {
                        last_mouse_x: px,
                        last_emitted_beat: snapped,
                    });
                    press_seek_beat = Some(snapped);
                }
            }
            // M10 Phase 46+47b: track header press 振り分け
            //  - volume band 内 → TrackVolumeDragSession (priority 最高)
            //  - 上記以外 + Name button area を含む row + M/S/Up/Dn/Del button rect 非 hit → reorder
            //  - 16px 未満 drag は release で click 格下げ (track header click のトラック選択が代替)
            // M14 Phase 63n-2 (#028): track 行 と lane 行 で分岐。 lane 行 (= track 行下、 expanded のみ)
            // では lane header button (★/👁/✕) と default band drag を扱う。
            // r.md #43 review: popup (右クリックメニュー) が開いている frame は header の
            // press 経路を丸ごと止める。 context menu は `capture_input == false` で背景
            // pointer を mask しないので、 menu item の press が背後の行に届き
            // **volume band drag が起動して離した位置の音量に飛ぶ** / reorder session が
            // 始まる (release 側の選択ガードだけでは塞げない press 側の同件)。
            if header_w > 0.0
                && !ui.has_open_popups()
                && header_pane.contains(px, py)
                && let Some(idx) = track_index_from_y(py, header_pane.y, &press_tops)
                && let Some(t) = visible_tracks.get(idx)
            {
                // header_pane.y と lanes.y は同じ値 (rect 分割で y 軸 origin 共通) なので press_tops を共有可。
                let row_top = press_tops[idx];
                // M14 Phase 63n-6 (#031): per-track row 高さで track row 範囲を判定。
                let row_h_eff = effective_track_row_h(t, view.track_row_h);
                let track_row_bottom = row_top + row_h_eff;
                if py < track_row_bottom {
                    // === track row press (既存ロジック) ===
                    // M14 Phase 118 follow-up (#092 review): press 側の row も draw 側 `row_for_layout`
                    // (Phase 63c #016 で導入) と **同じ indent** を適用する。 これまで press は非 indent の
                    // header_pane 幅で volume band / M·S·R / disclosure / lane disclosure を hit-test して
                    // いたため、 nested track (depth>0) で「描画位置 (indent 済) と press 判定がズレる」
                    // pre-existing バグがあった (深ネスト group の indent 空白を click すると volume drag が
                    // 起動する / 描画済ボタンの click が reorder に化ける 等)。 draw と同 indent にして
                    // press↔draw を SSoT 化 (depth==0 は indent=0 で byte 完全互換)。
                    let indent = f32::from(t.depth) * style.indent_px;
                    let row = Rect {
                        x: header_pane.x + indent,
                        y: row_top,
                        w: (header_pane.w - indent).max(2.0),
                        h: view.track_row_h,
                    };
                    let band_h = if matches!(t.kind, TrackKind::Video) || t.id == MASTER_TRACK_ID
                    {
                        // M14 Phase 72 (#044): video track では volume slider band を非表示
                        // (volume / pan は video には意味を持たない、 instrument / fx_chain と同様)。
                        // master row も描画側 (`header_row_layout(row, 0.0)`) と揃えて band 無し —
                        // 揃えないと不可視の volume drag が起動して
                        // `SetTrackVolume{track:MASTER}` を emit + カーソルが EwResize 化 (review)。
                        0.0
                    } else {
                        style.track_volume_band_h
                    };
                    let layout = header_row_layout(row, band_h);
                    if let Some(band) = layout.volume_band
                        && band.contains(px, py)
                    {
                        // band は frac 空間 (0..1 = MeterScale 上の位置)。 stored amp を
                        // frac に写して anchor にすることで、 release の frac 比較 /
                        // 描画と整合する (r.md #11。 旧 `amp.clamp(0,1)` は amp を frac と
                        // 誤用し、 +6dB 側で頭打ち + release で fill が飛んでいた)。
                        let av = MeterScale::default().amp_to_frac(t.volume);
                        let state: &mut ArrangementState = ui.widget_state(wid);
                        state.track_volume_drag = Some(TrackVolumeDragSession {
                            track_id: t.id,
                            anchor_volume: av,
                            band_rect: band,
                            anchor_mouse_x: px,
                            last_mouse_x: px,
                            last_emitted_volume: av,
                        });
                    } else {
                        let in_small_button = layout.buttons.iter().any(|b| b.contains(px, py));
                        // M14 Phase 63c (#016): disclosure rect の click は track_reorder セッションを
                        // 起動しない (折り畳み toggle のみ、 release frame 別経路で Edit 発行)。
                        let in_disclosure = is_group_set.contains(&t.id)
                            && disclosure_rect_for(layout.name_rect, style, t.depth)
                                .contains(px, py);
                        // M14 Phase 63n-1 (#028) + 63n-2 修正: lane disclosure hit zone は
                        // **`layout.lane_disc_rect`** を使う (S button の **右**、 button と非 overlap)。
                        // 旧 `lane_disclosure_rect_for(row, style)` (= track 行の右端内側) は S button
                        // と完全 overlap して描画後勝ちで `+`/`-` が覆われる bug 持ちだった (#028 user
                        // feedback で「`+`/`-` が見えない」)。 layout SSoT に統一して描画と hit-test
                        // が同 rect を参照する。
                        let in_lane_disclosure = !t.automation_lanes.is_empty()
                            && layout.lane_disc_rect.contains(px, py);
                        if in_lane_disclosure {
                            press_lane_toggle = Some(t.id);
                        } else if !in_small_button && !in_disclosure && t.id != MASTER_TRACK_ID {
                            // M14 Phase 63c (#016): multi-select 中の drag は selected_tracks をまとめて
                            // 移動するため、 source_track_ids に selected を全部入れる (clicked が selected
                            // に含まれていなければ単独 drag = `vec![clicked]`)。
                            // M14 Phase 63n-10 (#034): master row は reorder 対象外 (= 上端固定、 daw_01
                            // #034 §A 仕様)。 anchor_track_id に MASTER_TRACK_ID が入ると `arr_tracks` に
                            // 該当 id が存在しない → caller の reorder 実装が空振りする (= 結果 no-op だが
                            // session 立ち上げ自体が無駄、 明示的に skip)。
                            let source_ids: Vec<u32> = if selected_tracks.contains(&t.id) {
                                selected_tracks.to_vec()
                            } else {
                                vec![t.id]
                            };
                            let state: &mut ArrangementState = ui.widget_state(wid);
                            state.track_reorder = Some(TrackReorderSession {
                                anchor_track_id: t.id,
                                source_track_ids: source_ids,
                                anchor_mouse_y: py,
                                last_mouse_y: py,
                                anchor_mouse_x: px,
                                last_mouse_x: px,
                            });
                        }
                    }
                } else if !t.automation_lanes_collapsed && !t.automation_lanes.is_empty() {
                    // === lane header press (新規 Phase 63n-2) ===
                    // lane 群を上から積んで cursor py が当たる lane を見つけ、 button rect / default
                    // band rect を判定する。 invisible lane は積まない。
                    let header_indent = f32::from(t.depth) * style.indent_px;
                    let mut lane_y = track_row_bottom;
                    for lane in &t.automation_lanes {
                        if !lane.visible {
                            continue;
                        }
                        let lh = f32::from(lane.height_px);
                        if py >= lane_y && py < lane_y + lh {
                            let lane_key = AutomationLaneKey { track: t.id, lane: lane.id };
                            let header_rect = Rect {
                                x: header_pane.x + header_indent,
                                y: lane_y,
                                w: (header_pane.w - header_indent).max(2.0),
                                h: lh,
                            };
                            if let Some(layout) = automation_lane_header_layout(header_rect, style)
                            {
                                if layout.enabled_icon_rect.contains(px, py) {
                                    let v_lane = lane_key;
                                    let v_en = !lane.enabled;
                                    press_lane_button = Some(Edit::mutate(move |app: &mut AppData| {
                                        app.handle_event(AppEvent::SetLaneEnabled {
                                            track_id: v_lane.track,
                                            lane_id: v_lane.lane,
                                            enabled: v_en,
                                        });
                                    }));
                                } else if layout.visible_icon_rect.contains(px, py) {
                                    let v_lane = lane_key;
                                    let v_vis = !lane.visible;
                                    press_lane_button = Some(Edit::mutate(move |app: &mut AppData| {
                                        app.handle_event(AppEvent::SetLaneVisible {
                                            track_id: v_lane.track,
                                            lane_id: v_lane.lane,
                                            visible: v_vis,
                                        });
                                    }));
                                } else if layout.delete_icon_rect.contains(px, py) {
                                    let v_lane = lane_key;
                                    press_lane_button = Some(Edit::mutate(move |app: &mut AppData| {
                                        app.handle_event(AppEvent::DeleteLane {
                                            track_id: v_lane.track,
                                            lane_id: v_lane.lane,
                                        });
                                    }));
                                }
                                // default value フィールドの press は caller の
                                // scrubable_number_at overlay が直接処理する (widget 内 band drag は廃止)。
                            }
                            break;
                        }
                        lane_y += lh;
                    }
                }
            }

            // M14 Phase 63n-2 (#028): lane body (= clip 描画域、 lanes rect 内) の press 振り分け。
            // priority: point hit (Alt → 即時 delete / 通常 → drag session)。 single click on empty は
            // selection clear / 空き選択用に確保 (Bitwig / Live と同 UX)。 AddAutomationPoint は
            // **double click** 経由で発火 (既存 `take_double_click_in_rect` block で分岐)。
            // audio_grip / clip_drag (上で MIDI/Audio 行を既に処理済) は track row の y range 内のみ
            // 作動するため lane body と排他。
            // M14 Phase 63n-9 (#033): tension/bend handle press 検出 — **point press より先勝** で
            // selected point の Bezier / Exponential 入射 segment 中央 handle に当たった場合、 curve
            // param drag を起動。 handle は curve から 10px 上方向 offset で描画されるので point dot
            // 位置とは交差しないが、 priority 上 handle > point > lasso にする (= curve param 編集が
            // 最も狙った操作のため)。 modifier (Shift / Ctrl / Alt) は handle press では無視 (= Alt
            // は drag continuation で × 0.2 sensitivity に使う、 Shift/Ctrl は将来 multi-handle 編集に
            // 予約) — handle 上 click は **常に curve param drag 起動**。
            let mut handle_press_started = false;
            if !splitter_press
                && in_lanes
                && let Some((handle_point, handle_kind, handle_value, lane_h)) =
                    find_curve_param_handle_at(
                        &visible_tracks,
                        &press_tops,
                        view,
                        lanes,
                        selected_automation_points,
                        style,
                        px,
                        py,
                    )
            {
                let effective_h = f32::from(lane_h.max(40));
                let state: &mut ArrangementState = ui.widget_state(wid);
                state.automation_curve_param_drag = Some(AutomationCurveParamDragSession {
                    point: handle_point,
                    kind: handle_kind,
                    anchor_value: handle_value,
                    anchor_mouse_y: py,
                    last_mouse_y: py,
                    last_alt: pointer.modifiers.alt,
                    effective_lane_height_px: effective_h,
                    preview_value: handle_value,
                });
                handle_press_started = true;
            }

            // M14 Phase 63n-8 (#033): point press は **Shift / Ctrl 修飾も accept** (release 時 短 click
            // 化で toggle / replace を判定する)。 旧 Phase 63n-2 は `!shift && !ctrl` で除外していたが、
            // それだと Shift+click on point が何の session も起動せず toggle が発火しない bug を持っていた。
            // Shift+click on point は drag>=4px なら通常 move (= MoveAutomationPoints、 modifier 無視で
            // pressed が selection に含まれていれば multi)、 短 click なら toggle。 Ctrl 同様。
            // M14 Phase 63n-9 (#033): handle press が先勝した場合 (= `handle_press_started=true`) は
            // point press を skip (= 同 frame で 2 session が起動するのを回避)。
            if !splitter_press
                && !handle_press_started
                && in_lanes
                && let Some((point_key, _r)) = automation_point_at(
                    &visible_tracks,
                    &press_tops,
                    view.track_row_h,
                    view,
                    header_pane.x,
                    header_pane.w,
                    lanes,
                    px,
                    py,
                    style,
                )
            {
                if pointer.modifiers.alt {
                    // Alt + click on point → 即時 DeleteAutomationPoints (commit-by-release なし)
                    let v_k = vec![point_key];
                    press_delete_point = Some(Edit::mutate(move |app: &mut AppData| {
                        let refs: Vec<AutomationPointKeyRef> = v_k
                            .into_iter()
                            .map(|k| AutomationPointKeyRef {
                                track_id: k.clip.track,
                                lane_id: k.clip.lane,
                                clip_id: k.clip.clip,
                                point_idx: k.point_idx,
                            })
                            .collect();
                        if !refs.is_empty() {
                            app.handle_event(AppEvent::DeleteAutomationPoints { points: refs });
                        }
                    }));
                } else if let Some((lane, clip_in)) =
                    find_lane_clip(&visible_tracks, point_key.clip)
                {
                    // 通常 click on point → drag session 起動 (release で MoveAutomationPoints)
                    let p_idx = point_key.point_idx as usize;
                    if let Some(p) = lane
                        .clips
                        .iter()
                        .find(|c| c.id == point_key.clip.clip)
                        .and_then(|c| c.points.get(p_idx))
                        && let Some((_t_idx, _l_idx, _h_rect, body_rect)) = automation_lane_at(
                            &visible_tracks,
                            &press_tops,
                            view.track_row_h,
                            header_pane.x,
                            header_pane.w,
                            lanes.x,
                            lanes.w,
                            style,
                            py,
                        )
                    {
                        let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
                        let pad = style.automation_clip_v_pad_px;
                        let clip_y = body_rect.y + pad;
                        let clip_h = (body_rect.h - pad * 2.0).max(2.0);
                        #[allow(clippy::cast_possible_truncation)]
                        let cx_clip = body_rect.x
                            + ((clip_in.start_beat - view.start_beat) * beat_to_px) as f32;
                        #[allow(clippy::cast_possible_truncation)]
                        let cw = ((clip_in.len_beats * beat_to_px) as f32).max(2.0);
                        let clip_rect_anchor =
                            Rect { x: cx_clip, y: clip_y, w: cw, h: clip_h };
                        let press_alt = pointer.modifiers.alt;
                        let press_modifiers = pointer.modifiers;
                        let state: &mut ArrangementState = ui.widget_state(wid);
                        state.automation_point_drag = Some(AutomationPointDragSession {
                            point: point_key,
                            anchor_time_beat: p.time_beat,
                            anchor_value_norm: p.value_norm,
                            clip_rect_anchor,
                            body_rect_anchor: body_rect,
                            clip_start_beat: clip_in.start_beat,
                            clip_len_beats: clip_in.len_beats,
                            anchor_mouse: (px, py),
                            last_mouse: (px, py),
                            last_alt: press_alt,
                            start_modifiers: press_modifiers,
                        });
                    }
                }
            }

            // M14 Phase 63n-3 (#028) / daw_01 #071: lane body 内 automation clip の press 振り分け。
            // priority: **point hit より低い** (= 上の point block で point drag / Alt+delete が起動済なら
            // skip)。 #071 で Shift / Ctrl 修飾でも起動する (= MIDI clip drag と完全対称、 release で短 click
            // を modifier 別 (plain=単一置換 / Shift・Ctrl=選択足し引き) に demote)。 automation lane では
            // marquee (`!press_in_automation_lane`) は走らないので Shift を温存する必要はない。 Alt のみ
            // lane resize に予約 (下の Alt+drag fallback)。 掴んだ clip が選択集合に含まれていれば選択中の
            // 全 clip を grabbed-first で `anchors` に積み一括 move / resize する (MIDI clip と同 idiom)。
            let already_taken_by_point = {
                let state: &ArrangementState = ui.widget_state(wid);
                state.automation_point_drag.is_some()
            };
            // M14 Phase 63n-6 (#031 follow-up): Alt 修飾は **lane Alt+drag for resize に予約** する
            // ため、 Alt+press on automation clip は session を起動しない。 これによって lane body 内の
            // 任意位置 (clip 上を含む) で Alt+drag → lane resize が動作する (= user expectation 1:1)。
            // 既存 automation clip Alt-snap-off 機能は失われるが、 automation 編集で sub-grid 位置を
            // 細かく調整する用途は稀で、 lane resize の優先度の方が高いと判断 (= user feedback 反映)。
            // MIDI / audio clip の Alt-snap-off (= clip_drag press) は **track row のみ** に作用するため
            // この変更の影響を受けない (track row は別 priority でこの後 row Alt+drag fallback と排他)。
            // M14 Phase 63n-9 (#033): handle press (curve param drag) が先勝した場合 clip drag も skip。
            if !splitter_press
                && !already_taken_by_point
                && !handle_press_started
                && in_lanes
                && !pointer.modifiers.alt
                && let Some((clip_key, kind, _clip_rect, _body_rect_anchor)) =
                    automation_clip_zone_at(
                        &visible_tracks,
                        &press_tops,
                        view.track_row_h,
                        view,
                        header_pane.x,
                        header_pane.w,
                        lanes,
                        style,
                        px,
                        py,
                        style.resize_handle_px,
                    )
            {
                let press_alt = pointer.modifiers.alt;
                let press_ctrl = pointer.modifiers.ctrl;
                let press_shift = pointer.modifiers.shift;
                // #071: 掴んだ clip が選択集合に含まれていれば選択中の全 clip を一括 drag。 grabbed-first
                // 順 (snap pivot = anchors[0] = 掴んだ clip)。 MIDI clip の `selected_clips.contains(&hit)`
                // idiom を 1:1 ミラー。
                let mut keys: Vec<AutomationClipKey> = vec![clip_key];
                if selected_automation_clips.contains(&clip_key) {
                    keys.extend(
                        selected_automation_clips
                            .iter()
                            .copied()
                            .filter(|k| *k != clip_key),
                    );
                }
                let anchors = collect_automation_clip_anchors(
                    &visible_tracks,
                    &press_tops,
                    view.track_row_h,
                    header_pane.x,
                    header_pane.w,
                    lanes.x,
                    lanes.w,
                    style,
                    &keys,
                );
                if !anchors.is_empty() {
                    let state: &mut ArrangementState = ui.widget_state(wid);
                    state.automation_clip_drag = Some(AutomationClipDragSession {
                        kind,
                        primary: clip_key,
                        anchors,
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        last_alt: press_alt,
                        last_ctrl: press_ctrl,
                        last_shift: press_shift,
                    });
                }
            }

            // M14 Phase 63n-6 (#031): Alt+drag detection — splitter / 既存 press logic で session が
            // 起動しなかった場合のみ動作 (Alt+click on point / Alt+drag on clip 等は既に上で処理済 →
            // 該当 session が立っていれば skip する)。 lane body hit なら lane resize、 そうでなく
            // track row body hit なら row resize。 cursor が lanes 領域 (= clip 描画域) でも
            // header_pane (= lane label 列) でも動く — lane label 上 Alt+drag を「lane を伸ばす」 と
            // 期待する user 直感に合わせる (= 「lane の上で Alt+drag」 = lane resize)。
            let in_arr = in_lanes || (header_w > 0.0 && header_pane.contains(px, py));
            if pointer.modifiers.alt
                && !shift
                && !ctrl
                && !splitter_press
                && in_arr
            {
                let no_session = {
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
                };
                let no_press_action = press_seek_beat.is_none()
                    && press_lane_toggle.is_none()
                    && press_lane_button.is_none()
                    && press_delete_point.is_none();
                if no_session && no_press_action {
                    let lane_at = automation_lane_at(
                        &visible_tracks,
                        &press_tops,
                        view.track_row_h,
                        header_pane.x,
                        header_pane.w,
                        lanes.x,
                        lanes.w,
                        style,
                        py,
                    );
                    if let Some((t_idx, l_idx, _h_rect, _b_rect)) = lane_at {
                        let lane = &visible_tracks[t_idx].automation_lanes[l_idx];
                        let lane_key = AutomationLaneKey {
                            track: visible_tracks[t_idx].id,
                            lane: lane.id,
                        };
                        let anchor_h = lane.height_px;
                        if anchor_h > 0 {
                            let state: &mut ArrangementState = ui.widget_state(wid);
                            state.automation_lane_resize_drag =
                                Some(AutomationLaneResizeDragSession {
                                    lane: lane_key,
                                    anchor_height_px: anchor_h,
                                    anchor_mouse_y: py,
                                    last_mouse_y: py,
                                    last_emitted_height: anchor_h,
                                });
                        }
                    } else if let Some(t_idx) = track_index_from_y(py, lanes.y, &press_tops)
                        && t_idx + 1 < press_tops.len()
                    {
                        // lane が無い (or collapsed) で track row body の中の Alt+drag → per-track row resize。
                        // row body 範囲 = `[tops[t_idx], tops[t_idx] + effective_row_h(t))`、 それ以遠は
                        // lane 領域 (= `lane_at` で既に拾われる前提) — y check は collapsed track / 末尾
                        // track の「lane 無し領域」 まで含めて row body と認定するための明示判定。
                        let t = &visible_tracks[t_idx];
                        let row_top = press_tops[t_idx];
                        let anchor_row_h = effective_track_row_h(t, view.track_row_h);
                        let row_bottom = row_top + anchor_row_h;
                        if py >= row_top && py < row_bottom && anchor_row_h > 0.0 {
                            let state: &mut ArrangementState = ui.widget_state(wid);
                            state.track_row_resize_drag = Some(TrackRowResizeDragSession {
                                track: t.id,
                                anchor_row_h,
                                anchor_mouse_y: py,
                                last_mouse_y: py,
                                last_emitted_height: anchor_row_h,
                            });
                        }
                    }
                }
            }

            // M14 Phase 63n-8 (#033): automation point の lasso press — **空き automation lane zone**
            // (= lane body && !clip && !point && !lane resize splitter) の drag で起動。 Q2=A の zone 排他
            // 設計: clip / point / splitter 上は既存 drag (move / move-points / resize) を最優先で起動済、
            // ここはそれら全てが起動しなかった場合の lane body fallback。 既存 MIDI clip rect_select は
            // automation lane 内では起動しない (= 後段の rect_select block で `!in_automation_lane` で
            // guard)、 automation lane では空き zone drag が **修飾なしで lasso** (= Shift / Ctrl は
            // release 時 next 計算で union / XOR 分岐)、 #033 Q2 回答 A と整合。 Alt は lane resize に
            // 予約済 (上の Alt+drag fallback で先勝) なので `!pointer.modifiers.alt` で除外。
            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && !pointer.modifiers.alt
                && !splitter_press
                && in_lanes
            {
                let no_session = {
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
                };
                let no_press_action = press_seek_beat.is_none()
                    && press_lane_toggle.is_none()
                    && press_lane_button.is_none()
                    && press_delete_point.is_none();
                if no_session && no_press_action {
                    let lane_at = automation_lane_at(
                        &visible_tracks,
                        &press_tops,
                        view.track_row_h,
                        header_pane.x,
                        header_pane.w,
                        lanes.x,
                        lanes.w,
                        style,
                        py,
                    );
                    if let Some((_t_idx, _l_idx, _h_rect, body_rect)) = lane_at
                        && px >= body_rect.x
                        && px < body_rect.x + body_rect.w
                    {
                        // body x range 内 (= lane header 外)、 clip / point / splitter は上で先勝で
                        // 既に session 起動 (no_session で除外済) なので、 lane body の **真の空き zone**
                        // で press したことが確定。 lasso session 起動。
                        let state: &mut ArrangementState = ui.widget_state(wid);
                        state.automation_lasso_drag = Some(AutomationLassoSession {
                            anchor: (px, py),
                            last_mouse: (px, py),
                            start_modifiers: pointer.modifiers,
                        });
                    }
                }
            }
        }

        // M14 Phase 63j (#024): press block で貯めた playhead seek を 1 度発行 (state borrow 終了後)。
        if let Some(beat) = press_seek_beat {
            ui.push_edit({ let v_beat = beat; Edit::mutate(move |app: &mut AppData| { app.seek_playhead_to(v_beat); }) });
        }
        // M14 Phase 63n-1 (#028): track 行右端の lane disclosure click を 1 度発行 (同上)。
        if let Some(track) = press_lane_toggle {
            ui.push_edit({ let v_track = track; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::ToggleTrackAutomationCollapsed { track_id: v_track }); }) });
        }
        // M14 Phase 63n-2 (#028): lane header button (★/👁/✕) の click を 1 度発行。
        if let Some(req) = press_lane_button {
            ui.push_edit(req);
        }
        // M14 Phase 63n-2 (#028): Alt+click on point → DeleteAutomationPoints を 1 度発行 (即時)。
        if let Some(req) = press_delete_point {
            ui.push_edit(req);
        }

        // M14 Phase 63n-2 (#028): 右クリック on point の context menu は **caller 責務**。
        // widget は `response.automation_point_rects: Vec<(AutomationPointKey, Rect)>` を毎 frame
        // 返し (clip_rects と同 idiom)、 caller は loop で `context_menu_for(*rect, &["Hold",
        // "Linear", "Bezier"], ...)` を呼ぶ。 widget 内で secondary press を消費する旧設計は popup の
        // anchor_rect が **右クリック frame だけ Some** で次 frame 以降 caller が context_menu_for を
        // 呼ばないため popup state が消える bug を持っていた (= 一瞬で popup が閉じる)。 #028 §11.4
        // で確定した「caller が anchor を毎 frame 呼ぶ」 idiom に統一。

        // ---- drag continue / release 検出 ----
        // drag 中なら continuation frame で `last_mouse` / `last_alt` (および各 drag の last_*) を
        // update。 **release frame の `last_alt` は update しない** — 同 frame に
        // ModifiersChanged(alt=false) が先行する現象 (alt が一瞬 false に化ける) を回避するため、
        // release 直前 frame の値を保持する。 **release frame の `last_mouse` は pointer.pos が
        // anchor と異なる場合のみ update** — winit は release frame で `pointer.pos` を press 位置
        // に戻すことがあり、 そのまま上書きすると delta = 0 で commit not pushed (drag が「元に戻る」
        // ように見える)。 pointer.pos == anchor のときは continuation 由来の last_mouse を保持し、
        // そうでないときは pointer.pos が真値 (= 通常 release pos、 OR press → 1 frame で release した
        // short drag の release pos) として update する。
        if let Some((px, py)) = pointer.pos {
            let alt_now = pointer.modifiers.alt;
            let ctrl_now = pointer.modifiers.ctrl;
            let shift_now = pointer.modifiers.shift;
            let is_release = pointer.primary_just_released;
            let state: &mut ArrangementState = ui.widget_state(wid);
            if let Some(ref mut nd) = state.clip_drag {
                if !is_release {
                    nd.last_mouse = (px, py);
                    nd.last_alt = alt_now;
                    // M14 Phase 63e (#019): ctrl / shift も同じ仕組みで update。 release frame は
                    // ModifiersChanged が MouseInput より先に届いて false 化するリスクがあるので skip。
                    nd.last_ctrl = ctrl_now;
                    nd.last_shift = shift_now;
                } else if (px, py) != nd.anchor_mouse {
                    nd.last_mouse = (px, py);
                }
            }
            // M14 Phase 127 (daw_01 #105): section drag continuation。 clip_drag と同じく continuation で
            // last_mouse / last_alt / last_ctrl を update、 release frame は巻き戻し検知時のみ update。
            if let Some(ref mut sd) = state.section_drag {
                if !is_release {
                    sd.last_mouse = (px, py);
                    sd.last_alt = alt_now;
                    sd.last_ctrl = ctrl_now;
                    sd.last_shift = shift_now;
                } else if (px - sd.anchor_mouse.0).abs() > f32::EPSILON {
                    sd.last_mouse = (px, py);
                }
            }
            if let Some(ref mut ld) = state.loop_drag {
                if !is_release {
                    ld.last_mouse_x = px;
                    // M14 Phase 63j (#024): last_alt は continuation で update、 release は
                    // skip (clip_drag と同じ pattern、 OS event 順序による false 化 race を回避)。
                    ld.last_alt = alt_now;
                } else if (px - ld.anchor_mouse_x).abs() > f32::EPSILON {
                    // release frame で pointer.pos が press 位置と異なる = winit が press 位置に
                    // 巻き戻していない → 真値として update (clip_drag と同 pattern)。
                    ld.last_mouse_x = px;
                }
            }
            if let Some(ref mut tr) = state.track_reorder {
                // continuation は常に update。 release 時は winit 巻き戻し検知のため
                // anchor と differ する場合のみ update (clip_drag と同 pattern)。
                // M14 Phase 101 (daw_01 #072): y / x を独立に判定して update (片軸だけ巻き戻る
                // ケースでも他軸の真値を保持)。
                if !is_release || (py - tr.anchor_mouse_y).abs() > f32::EPSILON {
                    tr.last_mouse_y = py;
                }
                if !is_release || (px - tr.anchor_mouse_x).abs() > f32::EPSILON {
                    tr.last_mouse_x = px;
                }
            }
            if let Some(ref mut tv) = state.track_volume_drag
                && (!is_release || (px - tv.anchor_mouse_x).abs() > f32::EPSILON)
            {
                tv.last_mouse_x = px;
            }
            // M14 Phase 63j (#024): playhead_drag continuation で last_mouse_x を track。
            // release frame は session を後段で `take()` するため update 不要。
            if let Some(ref mut pd) = state.playhead_drag
                && !is_release
            {
                pd.last_mouse_x = px;
            }
            // M14 Phase 63k (#025): audio_drag continuation で last_mouse + sticky direction lock を update。
            // - last_mouse: continuation で常に update、 release frame は pointer.pos == anchor_mouse の
            //   ときのみ skip (winit が release で press 位置に戻すケースを回避、 clip_drag と同 pattern)。
            // - locked_horizontal: 未確定 (`None`) のとき、 累積 |dx| / |dy| のうちどちらかが
            //   `audio_fade_sticky_threshold_px` を超えたら方向 lock。 一度 lock されたら release まで
            //   切替不可 (要望文 §3.2: sticky direction)。
            if let Some(ref mut ad) = state.audio_drag {
                // continuation は常に update。 release frame は pointer.pos == anchor_mouse のときだけ skip
                // (winit が release で press 位置に戻すケースを回避、 clip_drag と同 pattern)。
                if !is_release || (px, py) != ad.anchor_mouse {
                    ad.last_mouse = (px, py);
                }
                if ad.locked_horizontal.is_none() {
                    let dx = (ad.last_mouse.0 - ad.anchor_mouse.0).abs();
                    let dy = (ad.last_mouse.1 - ad.anchor_mouse.1).abs();
                    let threshold = style.audio_fade_sticky_threshold_px;
                    if dx >= threshold || dy >= threshold {
                        ad.locked_horizontal = Some(dx >= dy);
                    }
                }
            }
            // M14 Phase 63n-2 (#028): automation_point_drag continuation で last_mouse + last_alt を update。
            // release frame は last_mouse は pointer.pos != anchor_mouse のときのみ update (clip_drag と
            // 同 pattern: winit が release で press 位置に戻すケースを回避)、 last_alt は release では
            // 保持 (ModifiersChanged が MouseInput より先に届く race を回避)。
            if let Some(ref mut ad) = state.automation_point_drag {
                if !is_release {
                    ad.last_mouse = (px, py);
                    ad.last_alt = alt_now;
                } else if (px, py) != ad.anchor_mouse {
                    ad.last_mouse = (px, py);
                }
            }
            // M14 Phase 63n-5 (#030): automation_lane_resize_drag continuation で last_mouse_y を update
            // (lane_default_drag と同 pattern、 release frame は release block で処理)。
            if let Some(ref mut rd) = state.automation_lane_resize_drag
                && !is_release
            {
                rd.last_mouse_y = py;
            }
            // M14 Phase 63n-6 (#031): track_row_resize_drag continuation で last_mouse_y を update
            // (lane_resize_drag と同 pattern、 release frame は per-frame 内で final 済 + take 廃棄)。
            if let Some(ref mut rd) = state.track_row_resize_drag
                && !is_release
            {
                rd.last_mouse_y = py;
            }
            // M14 Phase 117 (daw_01 #091): header_resize_drag continuation で last_mouse_x を update
            // (track_row_resize_drag の横軸版、 release frame は per-frame 内で final 済 + take 廃棄)。
            if let Some(ref mut hd) = state.header_resize_drag
                && !is_release
            {
                hd.last_mouse_x = px;
            }
            // M14 Phase 63n-3 (#028): automation_clip_drag continuation で last_mouse +
            // last_alt / last_ctrl / last_shift を update (`ClipDragSession` と同 pattern)。
            // release frame の `last_mouse` は pointer.pos != anchor のときのみ update、 modifier は
            // release では保持 (ModifiersChanged が MouseInput より先に届く race を回避)。
            if let Some(ref mut acd) = state.automation_clip_drag {
                if !is_release {
                    acd.last_mouse = (px, py);
                    acd.last_alt = alt_now;
                    acd.last_ctrl = ctrl_now;
                    acd.last_shift = shift_now;
                } else if (px, py) != acd.anchor_mouse {
                    acd.last_mouse = (px, py);
                }
            }
            // M14 Phase 63n-8 (#033): automation_lasso_drag continuation で last_mouse を update。
            // `start_modifiers` は press 時固定 (= 「lasso 開始時に Shift だったが drag 中に離した」 でも
            // union 動作、 既存 `DragRectState.start_modifiers` と同 idiom)。 release frame の last_mouse
            // は release pos が anchor と異なる場合のみ update (clip_drag と同 pattern)。
            if let Some(ref mut ls) = state.automation_lasso_drag
                && (!is_release || (px, py) != ls.anchor)
            {
                ls.last_mouse = (px, py);
            }
            // M14 Phase 63n-9 (#033): automation_curve_param_drag continuation で last_mouse_y / last_alt /
            // preview_value を update。 release frame は last_alt update を skip (= 既存 OS event 順序
            // race 回避 pattern、 ModifiersChanged が MouseInput より先に届く現象への対応)、 last_mouse_y は
            // release pos が anchor と異なる場合のみ update。 preview_value は anchor + sensitivity 計算で
            // 毎 frame 算出 (= live preview の SSoT、 release で final 値として使用)。
            if let Some(ref mut cd) = state.automation_curve_param_drag {
                if !is_release {
                    cd.last_mouse_y = py;
                    cd.last_alt = alt_now;
                } else if (py - cd.anchor_mouse_y).abs() > f32::EPSILON {
                    cd.last_mouse_y = py;
                }
                let dy = cd.last_mouse_y - cd.anchor_mouse_y;
                let delta =
                    curve_param_delta_from_dy(dy, cd.effective_lane_height_px, cd.last_alt);
                cd.preview_value = (cd.anchor_value + delta).clamp(-1.0, 1.0);
            }
        }

        // ---- ドラッグ端オートスクロール ----
        // drag 中、pointer が lanes 端の hot-zone に入ったら view を自動スクロールし、掴んでいる対象が
        // カーソルに追従し続ける (実 DAW 標準)。横 (beat) と縦 (track_top) の両軸。relative-delta で
        // 位置を決める session (clip / section / automation point/clip / lasso / clip marquee) は実
        // スクロール px ぶん anchor を逆方向に shift して追従させる (= content space delta)。track 並べ
        // 替え (live 行 top 再解決) と ruler の loop/playhead (絶対 px→beat 再解決) は anchor shift 不要。
        // カーソルを端で止めたままでもスクロール継続するよう `request_redraw` で次フレームを確保する。
        if pointer.primary_pressed && !pointer.primary_just_released {
            // 移動量ゲート: press からの移動が ACTIVATE_PX 以上のときのみ端スクロールを許可
            // (click-and-hold で view が飛ぶのを防ぐ)。press frame で press 位置を記録。
            let moved_enough = {
                let state: &mut ArrangementState = ui.widget_state(wid);
                if pointer.primary_just_pressed {
                    state.edge_scroll_press = pointer.pos;
                }
                let gate = daw_ui_core::widgets::edge_scroll::ACTIVATE_PX;
                matches!((state.edge_scroll_press, pointer.pos),
                    (Some(p), Some(c)) if (c.0 - p.0).powi(2) + (c.1 - p.1).powi(2) >= gate * gate)
            };
            let axes = if moved_enough {
                let state: &mut ArrangementState = ui.widget_state(wid);
                arrangement_edge_scroll_axes(state)
            } else {
                None
            };
            let drag_rect_wid = wid.child(b"rect_select");
            let marquee_active = moved_enough
                && axes.is_none()
                && {
                    let st: &mut daw_ui_core::widgets::drag_rect::DragRectState =
                        ui.widget_state(drag_rect_wid);
                    st.drag_start.is_some()
                };
            // clip marquee (空き lanes の rect-select) は両軸。
            if let Some((ax, ay)) = axes.or_else(|| marquee_active.then_some((true, true))) {
                let cfg = daw_ui_core::widgets::edge_scroll::EdgeScrollCfg::default();
                let (dx, dy) = daw_ui_core::widgets::edge_scroll::edge_scroll_delta(
                    pointer.pos,
                    lanes,
                    cfg,
                    ax,
                    ay,
                );
                if dx != 0.0 || dy != 0.0 {
                    // 実際に適用された scroll 量 (px) を求め、その分だけ anchor を逆 shift する。
                    let mut applied_beat_px = 0.0_f32;
                    let mut applied_track_px = 0.0_f32;
                    if dx != 0.0 && beat_per_px > 1e-6 {
                        // r.md #53: 端自動スクロールは 1 frame あたり 0〜18px の連続量なので、
                        // スナップ済の表示原点に足すと zone 入口 (< 0.5px/frame) で端数が
                        // 毎フレーム捨てられて一切進まなくなる。 基準は連続値のモデル側。
                        let new_start =
                            (view.scroll_beat_raw + f64::from(dx) * beat_per_px).max(0.0);
                        #[allow(clippy::cast_possible_truncation)]
                        let adx = ((new_start - view.scroll_beat_raw) / beat_per_px) as f32;
                        if adx != 0.0 {
                            applied_beat_px = adx;
                            ui.push_edit({ let v_b = new_start; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetArrangeScroll(v_b as f32)); }) });
                        }
                    }
                    if dy != 0.0 {
                        // 縦 scroll は既存 SetTrackTop と同じく下限 0 のみ (上限 clamp は handler 非対象、
                        // wheel 挙動と互換)。
                        let new_top = (view.track_top + dy).max(0.0);
                        let ady = new_top - view.track_top;
                        if ady != 0.0 {
                            applied_track_px = ady;
                            ui.push_edit({ let v_t = new_top; Edit::mutate(move |app: &mut AppData| { app.ui_prefs.arrange_track_top = v_t.max(0.0); }) });
                        }
                    }
                    if applied_beat_px != 0.0 || applied_track_px != 0.0 {
                        if marquee_active {
                            let st: &mut daw_ui_core::widgets::drag_rect::DragRectState =
                                ui.widget_state(drag_rect_wid);
                            if let Some(s) = st.drag_start.as_mut() {
                                s.0 -= applied_beat_px;
                                s.1 -= applied_track_px;
                            }
                        } else {
                            let st: &mut ArrangementState = ui.widget_state(wid);
                            arrangement_compensate_anchor(st, applied_beat_px, applied_track_px);
                        }
                        ui.request_redraw();
                    }
                }
            }
        }

        // default value の per-frame 編集は caller の scrubable_number_at overlay が担う
        // (旧 band drag の per-frame SetLaneDefault emit は廃止)。

        // M14 Phase 63n-5 (#030): automation_lane_resize_drag の per-frame live update。
        // drag 中は user に「lane が伸び縮みする様子」 を見せたいので、 height 変化を毎 frame 発行する
        // (lane_default_drag と同 pattern)。 release frame は release block で最終値を発行するためここでは skip。
        if let Some((_px, py)) = pointer.pos
            && !pointer.primary_just_released
        {
            // M14 Phase 63n-6 (#031): max は `min(style.max, lanes.h)` で runtime clamp。
            // style 値は絶対 cap、 lanes.h は描画 pane の現在縦サイズ (= 「画面いっぱい」)。
            let max_h = effective_lane_max_height(style, lanes);
            let min_h = style.automation_lane_min_height_px;
            let mut emit: Option<(AutomationLaneKey, u16, u16)> = None;
            {
                let state: &mut ArrangementState = ui.widget_state(wid);
                if let Some(ref mut rd) = state.automation_lane_resize_drag {
                    let dy = py - rd.anchor_mouse_y;
                    let raw = f32::from(rd.anchor_height_px) + dy;
                    let next = clamp_height_px(raw, min_h, max_h);
                    if next != rd.last_emitted_height {
                        emit = Some((rd.lane, rd.anchor_height_px, next));
                        rd.last_emitted_height = next;
                    }
                }
            }
            if let Some((lane, prev, next)) = emit {
                ui.push_edit({ let v_lane = lane; let v_prev = prev; let v_next = next; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetLaneHeight { track_id: v_lane.track, lane_id: v_lane.lane, prev_px: v_prev, next_px: v_next }); }) });
            }
        }

        // M14 Phase 63n-6 (#031): track_row_resize_drag の per-frame live update。
        // drag 中は **対象 track の `t.row_h`** が変わる度に caller が `SetSingleTrackRowH` を mutate
        // する (= per-track override 化、 Bitwig per-track zoom と同 idiom)。 widget は floor 1 px の
        // u16 で発火 (caller-side で `[min, max]` clamp)、 同値抑制 0.5 px 閾値で u16 quantization 込み。
        if let Some((_px, py)) = pointer.pos
            && !pointer.primary_just_released
        {
            let mut row_emit: Option<(u32, u16, u16)> = None;
            {
                let state: &mut ArrangementState = ui.widget_state(wid);
                if let Some(ref mut rd) = state.track_row_resize_drag {
                    let dy = py - rd.anchor_mouse_y;
                    let next_f = (rd.anchor_row_h + dy).max(1.0);
                    if (next_f - rd.last_emitted_height).abs() >= 0.5 {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let next = next_f.round().clamp(1.0, f32::from(u16::MAX)) as u16;
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let prev =
                            rd.anchor_row_h.round().clamp(1.0, f32::from(u16::MAX)) as u16;
                        row_emit = Some((rd.track, prev, next));
                        rd.last_emitted_height = next_f;
                    }
                }
            }
            if let Some((track, prev, next)) = row_emit {
                ui.push_edit({ let v_track = track; let v_prev = prev; let v_next = next; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetSingleTrackRowH { track_id: v_track, prev_px: v_prev, next_px: v_next }); }) });
            }
        }

        // M14 Phase 117 (daw_01 #091): header_resize_drag の per-frame live update。 drag 中は
        // header 幅変化を毎 frame `SetHeaderW { prev: anchor, next }` で発行する (caller が
        // `view.header_w` を更新 → 次 frame に header / lanes が連動伸縮)。 `next` は raw px
        // (NaN/負値防止の `max(0.0)` floor のみ、 実用 clamp は caller)、 同値抑制 0.5 px。
        if let Some((px, _py)) = pointer.pos
            && !pointer.primary_just_released
        {
            let mut header_emit: Option<(f32, f32)> = None;
            {
                let state: &mut ArrangementState = ui.widget_state(wid);
                if let Some(ref mut hd) = state.header_resize_drag {
                    let dx = px - hd.anchor_mouse_x;
                    let next = (hd.anchor_header_w + dx).max(0.0);
                    if (next - hd.last_emitted_w).abs() >= 0.5 {
                        header_emit = Some((hd.anchor_header_w, next));
                        hd.last_emitted_w = next;
                    }
                }
            }
            if let Some((_prev, next)) = header_emit {
                ui.push_edit({ let v_next = next; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::SetArrangeHeaderW(v_next)); }) });
            }
        }

        // M14 Phase 63j (#024): playhead drag continuation の per-frame live update。
        // press frame は press block 内で発火済 (`press_seek_beat`)、 ここは continuation のみ。
        // release frame は emit せず session を後段で take して discard する (commit-by-release 無し)。
        // `last_emitted_beat` で同値発火を抑制 (1e-6 拍 = ~10μs @ 120BPM 以下は ignore)。
        if let Some((px, _py)) = pointer.pos
            && !pointer.primary_just_pressed
            && !pointer.primary_just_released
        {
            let alt = pointer.modifiers.alt;
            let mut emit_beat: Option<f64> = None;
            {
                let state: &mut ArrangementState = ui.widget_state(wid);
                if let Some(ref mut pd) = state.playhead_drag {
                    let raw = px_to_beat(px, ruler.x, ruler.w, view);
                    let next = view.snap.snap_beat(raw, alt, zoom_x_px_per_beat).max(0.0);
                    if (next - pd.last_emitted_beat).abs() > 1e-6 {
                        emit_beat = Some(next);
                        pd.last_emitted_beat = next;
                    }
                }
            }
            if let Some(beat) = emit_beat {
                ui.push_edit({ let v_beat = beat; Edit::mutate(move |app: &mut AppData| { app.seek_playhead_to(v_beat); }) });
            }
        }

        // M10 Phase 49: track volume drag 中の per-frame live update。
        // release frame は Mutate 発火を抑制し、release ブロックの Undoable Edit に任せる
        // (= fader_at の `suppress_mutate_on_release` と同パターン)。
        // 同値発火を抑えるため `last_emitted_volume` と差分比較。
        if let Some((px, _py)) = pointer.pos
            && !pointer.primary_just_released
        {
            let mut volume_emit: Option<(u32, f32, f32)> = None;
            {
                let state: &mut ArrangementState = ui.widget_state(wid);
                if let Some(ref mut tv) = state.track_volume_drag {
                    let next = volume_from_mouse_x(px, tv.band_rect.x, tv.band_rect.w);
                    if (next - tv.last_emitted_volume).abs() > 1e-4 {
                        volume_emit = Some((tv.track_id, tv.anchor_volume, next));
                        tv.last_emitted_volume = next;
                    }
                }
            }
            if let Some((track, _prev, next)) = volume_emit {
                ui.push_edit({ let v_track = track; let v_next = next; Edit::mutate(move |app: &mut AppData| { let amp = MeterScale::default().frac_to_amp(v_next.clamp(0.0, 1.0)); app.handle_event(AppEvent::SetTrackVolume { track: v_track, amp }); }) });
            }
        }
        // 2) drag overlay 計算用に clone を取る (last_mouse を更新した後)。
        let clip_drag_session: Option<ClipDragSession> = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.clip_drag.clone()
        };
        let clip_drag_release_raw: Option<ClipDragSession> = if pointer.primary_just_released {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.clip_drag.take()
        } else {
            None
        };
        // 短 click 化時は session の careful-update modifier (`last_ctrl` / `last_shift`)
        // も一緒に持ち回す — release frame の `pointer.modifiers` 生読みは
        // 「ModifiersChanged が Released より先に届く」 race で Ctrl/Shift+click が
        // Single に化ける (automation clip の demote と同 pattern、 review)。
        #[allow(clippy::type_complexity)]
        let (clip_drag_release, clip_short_click_pos): (
            Option<ClipDragSession>,
            Option<((f32, f32), bool, bool)>,
        ) =
            if let Some(nd) = clip_drag_release_raw {
                let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
                let dy = nd.last_mouse.1 - nd.anchor_mouse.1;
                let dist = dx.abs() + dy.abs();
                // 短 click 化 (drag → click 格下げ) の閾値は **mouse jitter を ignore する程度**
                // (`CLIP_CLICK_DRAG_SLOP_PX`) に抑える。 旧実装の 16px 閾値は過剰で、 user が「ちょっと
                // ずらす」 操作も吸収してしまい release で元位置 (= 通常 grid 上) に戻る → 「grid に飛ぶ」
                // symptom の主因。
                // 適用条件:
                //   - **Resize (Left/Right)** は閾値関係なく常に commit (resize handle 上の click は
                //     意味がない、 短 drag でも長さ変更を反映すべき)。
                //   - **Move** で **Alt なし** のときのみ jitter 閾値で短 click 化。 click vs drag の
                //     区別が必要なのは Move のみ (click = selection 切替、 drag = 移動)。
                //   - **Alt 押下中** は Move でも閾値 skip (Alt は raw 微調整の明示意図)。
                let is_move = matches!(nd.kind, ClipDragKind::Move);
                let demote = is_move && !nd.last_alt && dist < CLIP_CLICK_DRAG_SLOP_PX;
                if demote {
                    (None, Some((nd.last_mouse, nd.last_ctrl, nd.last_shift)))
                } else {
                    (Some(nd), None)
                }
            } else {
                (None, None)
            };

        let loop_drag_session: Option<LoopDragSession> = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.loop_drag
        };
        let loop_drag_release: Option<LoopDragSession> = if pointer.primary_just_released {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.loop_drag.take()
        } else {
            None
        };

        // M14 Phase 127 (daw_01 #105): section drag の overlay 用 copy (SectionDragSession は Copy) と
        // release 取り出し (`loop_drag` と同 idiom)。
        let section_drag_session: Option<SectionDragSession> = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.section_drag
        };
        let section_drag_release: Option<SectionDragSession> = if pointer.primary_just_released {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.section_drag.take()
        } else {
            None
        };

        // M10 Phase 46: track reorder session の overlay 用 clone と release 取り出し。
        // M14 Phase 63c (#016): TrackReorderSession は Vec<u32> を持つため Copy 不可。 ここで clone。
        let track_reorder_session: Option<TrackReorderSession> = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.track_reorder.clone()
        };
        let track_reorder_release_raw: Option<TrackReorderSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = ui.widget_state(wid);
                state.track_reorder.take()
            } else {
                None
            };

        // M14 Phase 63c (#016) → 101 (daw_01 #072): track header drag release の **drop action**。
        // `SetTrackParent { tracks, parent, anchor_after }` を 1 つ発行。 caller は (1) source を
        // arr_tracks から remove (2) parent_id を `parent` に更新 (3) `anchor_after` の直後
        // (None で先頭) に挿入、 という再構築をする。
        //
        // M14 Phase 101 (daw_01 #072): drop 解決を `resolve_track_drop` に一本化。 Y で gap、 X で
        // ネスト深さを決め、 (parent, anchor_after) を導出する (旧 Y-only ヒューリスティックは「一番下へ」
        // drop が最下段 group の内側に吸い込まれるバグを持っていた)。 **overlay (描画プレビュー) と
        // 完全に同じ pure 関数**を通すので preview = commit が構造的に保証される。 gate は drag 距離
        // (dx/dy 合成) で、 click (≒静止) を reorder に昇格させない。
        let pending_drop: Option<(Vec<u32>, Option<u32>, Option<u32>)> =
            track_reorder_release_raw.as_ref().and_then(|tr| {
                let dx = tr.last_mouse_x - tr.anchor_mouse_x;
                let dy = tr.last_mouse_y - tr.anchor_mouse_y;
                if (dx * dx + dy * dy).sqrt() < REORDER_DRAG_THRESHOLD_PX {
                    return None;
                }
                let drop = resolve_track_drop(
                    tracks,
                    &visible_tracks,
                    &press_tops,
                    &is_group_set,
                    &tr.source_track_ids,
                    style.indent_px,
                    tr.last_mouse_y,
                    tr.last_mouse_x,
                    tr.anchor_mouse_x,
                );
                Some((tr.source_track_ids.clone(), drop.parent, drop.anchor_after))
            });
        let pending_reorder_hash: u64 = pending_drop.as_ref().map_or(0_u64, |(ts, p, a)| {
            let mut h = u64::from(p.unwrap_or(u32::MAX));
            h = h.wrapping_mul(31).wrapping_add(u64::from(a.unwrap_or(u32::MAX)));
            for t in ts {
                h = h.wrapping_mul(31).wrapping_add(u64::from(*t));
            }
            h.wrapping_mul(0x100_0000_01B3)
        });

        // M10 Phase 47b: track volume drag session の overlay 用 clone と release 取り出し。
        let track_volume_session: Option<TrackVolumeDragSession> = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.track_volume_drag
        };
        let track_volume_release: Option<TrackVolumeDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = ui.widget_state(wid);
                state.track_volume_drag.take()
            } else {
                None
            };

        // M14 Phase 63j (#024): playhead_drag は release frame で take して discard。
        // continuous emit は per-frame block で完了済、 release 専用 commit は不要。
        if pointer.primary_just_released {
            let state: &mut ArrangementState = ui.widget_state(wid);
            let _ = state.playhead_drag.take();
        }

        // M14 Phase 63k (#025): audio_drag overlay 用 clone と release 取り出し。
        let audio_drag_session: Option<AudioDragSession> = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.audio_drag
        };
        let audio_drag_release: Option<AudioDragSession> = if pointer.primary_just_released {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.audio_drag.take()
        } else {
            None
        };

        // M14 Phase 63n-2 (#028): automation_point_drag overlay clone + release take。
        let point_drag_session: Option<AutomationPointDragSession> = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.automation_point_drag
        };
        let point_drag_release: Option<AutomationPointDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = ui.widget_state(wid);
                state.automation_point_drag.take()
            } else {
                None
            };

        // M14 Phase 63n-5 (#030): automation_lane_resize_drag release take (overlay は不要 — caller が
        // per-frame 受信した SetLaneHeight で `lane.height_px` を update することで lane が伸び縮みする
        // 様子が cached 描画に直接反映される)。 release frame で session を take し、 final height を発行。
        let lane_resize_drag_release: Option<AutomationLaneResizeDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = ui.widget_state(wid);
                state.automation_lane_resize_drag.take()
            } else {
                None
            };

        // M14 Phase 63n-6 (#031): track_row_resize_drag release take + discard。 per-frame emit で
        // 既に最終値が発火済 (= `last_emitted_height`)、 release で追加 emit は不要 (lane と異なる)。
        // session を `take()` して廃棄 (cursor 形状 / hover 判定が release 後すぐ解除される)。
        if pointer.primary_just_released {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.track_row_resize_drag.take();
        }

        // M14 Phase 117 (daw_01 #091): header_resize_drag release take + discard (row resize と同 idiom、
        // per-frame で final 済)。
        if pointer.primary_just_released {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.header_resize_drag.take();
        }

        // M14 Phase 63n-3 (#028): automation_clip_drag overlay clone + release take。
        // overlay は ghost clip rect を cached 外で重ねる、 release で 1 度だけ
        // `MoveAutomationClips` / `CloneAutomationClipsLinked` / `CloneAutomationClipsIndependent` /
        // `ResizeAutomationClips` / (短 click 時) `SelectAutomationClips` のいずれかを発行。
        let automation_clip_drag_session: Option<AutomationClipDragSession> = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.automation_clip_drag.clone()
        };
        let automation_clip_drag_release: Option<AutomationClipDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = ui.widget_state(wid);
                state.automation_clip_drag.take()
            } else {
                None
            };

        // M14 Phase 63n-8 (#033): automation_lasso_drag overlay clone + release take。
        // overlay は drag 中の lasso rect を cached 外で描画 (style.automation_lasso_fill / border)、
        // release で 1 度だけ `SelectAutomationPoints` を発行 (next 計算は anchor 時の modifier で
        // replace / union / XOR 分岐)。 `response.automation_lasso_active = session.is_some()` を後で set。
        let automation_lasso_session: Option<AutomationLassoSession> = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.automation_lasso_drag
        };
        let automation_lasso_release: Option<AutomationLassoSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = ui.widget_state(wid);
                state.automation_lasso_drag.take()
            } else {
                None
            };
        if automation_lasso_session.is_some() {
            response.automation_lasso_active = true;
        }

        // M14 Phase 63n-9 (#033): automation_curve_param_drag overlay clone + release take。
        // overlay は drag 中 handle + preview curve segment を cached 外で描画 (handle 位置は preview_value
        // 由来、 curve は preview_value で再 flatten した polyline を `automation_curve_param_preview_color`
        // で重ねる)、 release で 1 度だけ `SetAutomationCurveParam { point, kind, prev_value, next_value }`
        // を発行 (anchor == preview なら 1e-4 閾値で no-op)。
        let automation_curve_param_session: Option<AutomationCurveParamDragSession> = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.automation_curve_param_drag
        };
        let automation_curve_param_release: Option<AutomationCurveParamDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = ui.widget_state(wid);
                state.automation_curve_param_drag.take()
            } else {
                None
            };

        // drag overlay delta (last_mouse ベース、release と一貫)。
        // M14 Phase 63j (#024): `beat_per_px` / `zoom_x_px_per_beat` は関数頭で計算済 (press 振り分けの
        // playhead seek snap でも使うため)。 ここでは shadow せず再利用する。
        // r.md #24: overlay は press 直後 (delta=0) から出す (= mouse down で掴んだ clip が
        // 選択枠でハイライトされる)。 press 中に中身 (名前 / 波形 / MIDI) が消えないのは
        // `draw_drag_preview` が **中身入りの半透明コピー** を描くようにしたため (旧: 中身の無い
        // 不透明 ghost が元 clip を覆い隠していた = #24 の主因)。 閾値ゲートは張らない
        // (張ると mouse down のハイライトが消える)。
        let clip_drag_overlay: Option<(ClipDragSession, f64, i32)> = clip_drag_session
            .as_ref()
            .map(|nd| {
                let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
                let raw = f64::from(dx) * beat_per_px;
                // **絶対位置 snap** (= Cubase / Live と同じ「nearest grid alignment」 動作):
                // anchor 0 の編集対象端 (Move=start / ResizeRight=end / ResizeLeft=start) の絶対位置を
                // grid に round → その差分 (`adjusted_delta`) を全 anchor に同じだけ適用する。
                // delta-snap (= raw_delta だけを round) だと anchor が grid 外に既にずれていた場合
                // (例: 前回 Alt+drag で +0.078 拍ずらした) に release してもずれが永久残る。
                // 絶対 snap なら anchor 0 が必ず grid 上に着地し、 複数選択は相対関係を維持。
                // alt は drag state の `last_alt` を真値とし、 `pointer.modifiers.alt` を直接見ない。
                let beat_delta = compute_clip_drag_beat_delta(
                    nd,
                    raw,
                    &view.snap,
                    zoom_x_px_per_beat,
                );
                // track 方向は y→visible 行 index 解決の差 (per-track 行高 / lane 展開対応)。
                let track_delta = compute_clip_drag_track_delta(nd, &press_tops);
                (nd.clone(), beat_delta, track_delta)
            });

        // M14 Phase 63j (#024): overlay の preview range も `compute_loop_drag_endpoints` で
        // snap 適用済 (commit と同一値で確定、 release 時の「カクッ」 ずれを回避)。 alt は session の
        // `last_alt` を真値とし、 `pointer.modifiers.alt` を直接見ない (clip_drag と同じ pattern)。
        let loop_drag_preview_range: Option<(f64, f64)> = loop_drag_session.map(|ld| {
            let cur_beat = px_to_beat(ld.last_mouse_x, ruler.x, ruler.w, view);
            compute_loop_drag_endpoints(&ld, cur_beat, &view.snap, zoom_x_px_per_beat)
        });

        // ---- hover 計算 ----
        if let Some((cx, cy)) = pointer.pos
            && lanes.contains(cx, cy)
        {
            response.hovered_track = track_index_from_y(cy, lanes.y, &press_tops)
                .and_then(|idx| visible_tracks.get(idx).map(|t| t.id));
            if let Some((hit_key, hit_kind)) =
                clip_hit(&visible_tracks, &press_tops, view, lanes, cx, cy, style.resize_handle_px)
            {
                response.hovered_clip = Some(hit_key);
                response.hovered_zone = Some(hit_kind);
            } else {
                // M14 Phase 116 (daw_01 #090): clip-first first-hit。 clip に当たらなかったときだけ
                // ポインタ下の automation lane body を公開する (`hovered_clip` と排他)。 `cx` は既に
                // `lanes.contains(cx, cy)` で lanes pane 内と確定済 (= header 帯ではなく body)。
                response.hovered_automation_lane = automation_lane_key_at_y(
                    &visible_tracks,
                    &press_tops,
                    view.track_row_h,
                    header_pane.x,
                    header_pane.w,
                    lanes.x,
                    lanes.w,
                    style,
                    cy,
                )
                .map(|(key, _body_rect)| key);
            }
        }
        // M14 Phase 127 (daw_01 #105): Arranger section hover (arranger_rect 内、 clip / lane と y 排他)。
        if let Some((cx, cy)) = pointer.pos
            && arranger_lane_h > 0.0
            && arranger_rect.contains(cx, cy)
        {
            // hover の zone (Move/Resize) も保持して cursor を駆動する (id だけ捨てない)。
            if let Some((id, kind)) =
                section_hit(sections, arranger_rect, view, cx, cy, style.resize_handle_px)
            {
                response.hovered_section = Some(id);
                response.hovered_section_zone = Some(kind);
            }
        }
        // visible section の rect を response に積む (clip_rects と同 semantics、 caller の context_menu_for
        // 用)。 完全 off-screen (arranger_rect と x 交差しない) は除外。
        if arranger_lane_h > 0.0 {
            for s in sections {
                let r = section_to_rect(s, view, arranger_rect);
                if r.x + r.w >= arranger_rect.x && r.x <= arranger_rect.x + arranger_rect.w {
                    response.section_rects.push((s.id, r));
                }
            }
        }
        response.dragging = clip_drag_session.as_ref().map(|nd| nd.kind);
        response.reordering = track_reorder_session.as_ref().map(|tr| tr.anchor_track_id);
        response.dragging_track_volume = track_volume_session.map(|tv| tv.track_id);
        // 既存 section の Move/Resize drag のみ報告 (Create 範囲 drag は transient creation なので None)。
        response.dragging_section = section_drag_session.and_then(|sd| match sd.kind {
            SectionGesture::Move => Some(ClipDragKind::Move),
            SectionGesture::ResizeLeft => Some(ClipDragKind::ResizeLeft),
            SectionGesture::ResizeRight => Some(ClipDragKind::ResizeRight),
            SectionGesture::Create => None,
        });

        // ---- cursor ----
        // drag 中 / hover 中の clip 上 / それ以外で arrangement 内なら明示的に Default
        // にリセット (`set_cursor` を呼ばないと OS 側に前フレームの形が残る、winit は state-full)。
        // M14 Phase 63n-3 (#028): automation clip drag 中も MIDI と同じ cursor 形状 (排他で `Some` 判定)。
        // M14 Phase 63n-5 (#030): lane resize drag 中は NsResize (cursor 移動の縦軸を強調)、 hover 時も
        // splitter hot zone なら NsResize にして discoverability を確保。 lane resize > clip drag > hover の
        // priority (= 同時に成立しないが、 万一重なっても resize を優先)。
        // M14 Phase 63n-6 (#031): row resize drag 中も NsResize (lane resize と同じ)。 lane / row の
        // 両 session を同 priority で扱い、 同時に立たない (press 時に一方しか起動しない)。
        let resize_active = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.automation_lane_resize_drag.is_some() || state.track_row_resize_drag.is_some()
        };
        // M14 Phase 117 (daw_01 #091): header 幅 resize drag 中 / hover 中は EwResize (横軸)。
        // active は最優先 (NsResize / clip drag より上)、 hover は lane/row splitter NsResize の後に評価。
        let header_resize_active = {
            let state: &mut ArrangementState = ui.widget_state(wid);
            state.header_resize_drag.is_some()
        };
        let dragging_kind = response
            .dragging
            .or(automation_clip_drag_session.as_ref().map(|acd| acd.kind))
            // section の Move/Resize drag 中も clip と同じ cursor (Move / EwResize)。
            // clip drag と section drag は y 領域排他なので同時に Some にならない。
            .or(response.dragging_section);
        if header_resize_active {
            ui.set_cursor(CursorIcon::EwResize);
        } else if resize_active {
            ui.set_cursor(CursorIcon::NsResize);
        } else if let Some(kind) = dragging_kind {
            ui.set_cursor(drag_kind_cursor(kind));
        } else if response.reordering.is_some() {
            ui.set_cursor(CursorIcon::Move);
        } else if response.dragging_track_volume.is_some() {
            ui.set_cursor(CursorIcon::EwResize);
        } else if let Some(zone) = response.hovered_zone {
            ui.set_cursor(drag_kind_cursor(zone));
        } else if let Some(zone) = response.hovered_section_zone {
            // section 帯の hover も clip と同 idiom — 端 (Resize zone) で EwResize、
            // 中央 (Move zone) で Move。 帯端を掴んでリサイズできることを ↔ カーソルで示す。
            ui.set_cursor(drag_kind_cursor(zone));
        } else if let Some((cx, cy)) = pointer.pos
            && (automation_lane_resize_splitter_at(
                &visible_tracks,
                &press_tops,
                view.track_row_h,
                header_pane.x,
                header_pane.w,
                lanes.x,
                lanes.w,
                style,
                cx,
                cy,
            )
            .is_some()
                || track_row_resize_splitter_at(
                    &visible_tracks,
                    &press_tops,
                    view.track_row_h,
                    lanes.x,
                    lanes.w,
                    style,
                    cx,
                    cy,
                )
                .is_some())
        {
            ui.set_cursor(CursorIcon::NsResize);
        } else if let Some((cx, cy)) = pointer.pos
            && header_resize_splitter_at(rect, header_w, style, cx, cy)
        {
            // M14 Phase 117 (daw_01 #091): header / lanes 境界 hover で EwResize (discoverability)。
            // lane/row splitter (NsResize) を上で先に判定済なので角の競合は NsResize 優先。
            ui.set_cursor(CursorIcon::EwResize);
        } else if let Some((cx, cy)) = pointer.pos
            && let Some((_key, kind, _clip_rect, _body_rect)) = automation_clip_zone_at(
                &visible_tracks,
                &press_tops,
                view.track_row_h,
                view,
                header_pane.x,
                header_pane.w,
                lanes,
                style,
                cx,
                cy,
                style.resize_handle_px,
            )
        {
            // automation clip も MIDI clip と同様に端で EwResize / 本体で Move を出す。
            // press 側は `automation_clip_zone_at` で resize/move を既に判定して clip drag を起動して
            // いるが、 hover cursor だけ未配線で「端でカーソルが左右矢印にならない」 状態だった。
            // lane/row/header splitter の resize hover はこの上で先に判定済なので、 角の競合は
            // それらが優先される (= press 側の splitter 優先順位と一致)。
            let cur = match kind {
                ClipDragKind::Move => CursorIcon::Move,
                ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => CursorIcon::EwResize,
            };
            ui.set_cursor(cur);
        }
        // 「自分の矩形なら Default」の分岐はもう要らない (daw_01 r.md #50):
        // `Ui` が per-frame セマンティクスになり、誰も要求しなかったフレームは
        // 自動で Default に戻る。

        // ---- 描画 (heavy + cached + 動的 overlay) ----
        // M10 Phase 50: pending_reorder_hash を viewport_key に入れて、release frame の optimistic
        // preview で cache miss を強制 (新順序での再描画を 1 frame 遅延なく行う)。
        // tuple Hash 実装は 12 要素まで → nested tuple で 13 要素分を表現。
        // M13 Phase 55: bpm / time_sig を 3 つ目の nested tuple で追加し v2 に bump。
        // M14 Phase 61b (#011): clip 個別の (id, start_beat, len_beats) 変化を widget 側で hash
        // して 4 つ目の outer 要素 internal_clip_hash として viewport_key に追加 + v3 に bump。
        // M14 Phase 63c (#016): selected_tracks を fold して selection 変化での cache miss を保証
        // (旧 `selected_track.unwrap_or(u32::MAX)` の単一 u32 に対し、 multi-select は集合 hash)。
        // 加えて parent_id / depth / collapsed の構成変化は data_generation で caller 責務 (group
        // 構成変化は track 構成変化と同義、 caller が data_generation を bump する前提)。
        // (review) fold は caller slice でなく **visible_tracks** (= collapsed subtree
        // 除外 + synthetic master prepend 済み) を対象にする。 旧実装は master row
        // (song_lanes) が hash 対象外で、 テンポ等の master automation 編集・折り畳み・
        // 高さ変更が cached 層で stale になっていた (cached は master 行も描く)。
        let internal_clip_hash = fold_arrangement_clip_hash(&visible_tracks);
        let selected_tracks_hash: u64 = selected_tracks.iter().fold(0xCBF2_9CE4_8422_2325_u64, |a, &x| {
            a.wrapping_mul(0x100_0000_01B3).wrapping_add(u64::from(x))
        });
        // M14 Phase 63n-3 (#028): selected_automation_clips を fold して cache 再構築を保証 (= 選択
        // 変化時に lane の clip rect 描画が selected_fill / selected_border に切り替わる)。
        let selected_automation_clips_hash: u64 = selected_automation_clips.iter().fold(
            0xCBF2_9CE4_8422_2325_u64,
            |a, k| {
                a.wrapping_mul(0x100_0000_01B3)
                    .wrapping_add(u64::from(k.track))
                    .wrapping_mul(0x100_0000_01B3)
                    .wrapping_add(u64::from(k.lane))
                    .wrapping_mul(0x100_0000_01B3)
                    .wrapping_add(u64::from(k.clip))
            },
        );
        let viewport_key = (
            (
                b"arrangement_widget_v7" as &[u8],
                rect.w.to_bits(),
                rect.h.to_bits(),
                view.start_beat.to_bits(),
                view.len_beats.to_bits(),
                view.track_top.to_bits(),
                view.track_row_h.to_bits(),
                view.tracks_visible.to_bits(),
                view.header_w.to_bits(),
                view.ruler_h.to_bits(),
                view.data_generation,
                selected_tracks_hash,
            ),
            pending_reorder_hash,
            (view.bpm.to_bits(), u32::from(view.time_sig.0), u32::from(view.time_sig.1)),
            internal_clip_hash,
            selected_automation_clips_hash,
            // (review) cached primitives は絶対座標で再生されるため、 widget の
            // 位置 (rect.x/y) と arranger lane 高さ (lanes.y のオフセット成分) も
            // key に含める — 「サイズ不変で位置 / lane 高さだけ変わる」 layout
            // 変化で旧座標に描かれるのを correct-by-construction で防ぐ。
            (rect.x.to_bits(), rect.y.to_bits(), view.arranger_lane_h.to_bits()),
        );

        // M14 Phase 63c (#016): SetTrackParent に統合した結果、 release frame の optimistic
        // preview (旧 ReorderTracks の new_order を frame 末尾 deferred apply の代わりに同 frame
        // で見せる) は廃止。 caller の SetTrackParent arm は「source remove → parent_id update →
        // anchor_after 後に insert」 を行うが、 widget は次 frame で更新後の `tracks` を再受信して
        // 描画する (= 1 frame の表示遅延)。 user が drag release で「カクッ」 と動く挙動になるが、
        // 構造変化を伴う drop は反映までの遅延が許容範囲 (sibling reorder を SetTrackParent と
        // 統一した代償としては妥当)。 必要なら別 PR で optimistic preview を再導入可能。
        //
        // tracks_for_draw は draw / track headers loop / clip 計算で使う visible-only Arc。
        // 入力 `tracks` (caller's slice、 順序込み) を visible filter かけたコピーを保持。
        let tracks_for_draw: Arc<[ArrangementTrack]> = Arc::from(visible_tracks.clone());
        let tracks_owned: Arc<[ArrangementTrack]> = Arc::clone(&tracks_for_draw);
        let style_copy = *style;
        let view_copy = view;
        // M14 Phase 63n-1 (#028): cached / cached-外 の prefix sum tops は heavy closure 内で
        // 再計算する (`'static` 制約で外側 borrow を持ち込めないため)。 caller scope では持たない。
        let selected_set: HashSet<ClipKey> = selected_clips.iter().copied().collect();
        // M14 Phase 63n-3 (#028): automation clip selection set (heavy closure 用)。
        let selected_automation_clips_set_for_heavy: HashSet<AutomationClipKey> =
            selected_automation_clips.iter().copied().collect();
        // M14 Phase 63c (#016): heavy closure は `'static` 要求なので owned Vec<u32> で渡す
        // (selected_set と同パターン)。 loop 側の hit-test では `selected_tracks` slice (borrowed)
        // を直接 contains で参照するため、 ここで cloned heavy 用 vector を別に持って move 衝突を回避。
        let selected_tracks_for_heavy: Vec<u32> = selected_tracks.to_vec();
        // M14 Phase 113 (daw_01 #085): group track 背景 tint 撤去に伴い、 lanes 背景描画
        // (`draw_lanes_bg`) は group 判定を使わなくなったため heavy closure 用の is_group_set clone は不要。
        // group の hit-test / disclosure / drag drop 判定は loop 側の borrowed `is_group_set` を直接使う。
        let drag_overlay_clone = clip_drag_overlay.clone();
        // M14 Phase 63k (#025): audio_drag overlay 用 clone (heavy closure に move)。
        // ghost (drag 中の preview line / fade envelope / label) は cached 外で描画する。
        let audio_drag_overlay = audio_drag_session;
        // M14 Phase 63n-2 (#028): point_drag の overlay 用 clone (heavy closure に move)。
        // point ghost は drag 中の preview を新位置に上書き (cached 内描画は anchor 値のまま)。
        let point_drag_overlay = point_drag_session;
        // M14 Phase 63n-3 (#028): automation_clip_drag overlay (heavy closure に move)。
        // ghost rect は drag 中の preview (新位置 / 新長さ、 cross-lane drop なら新 lane の body 内) を
        // cached 外で重ねる。 base 描画 (cached 内) も同 frame 表示されるが、 ghost が上に重なる。
        // #071: session は multi-anchor (non-Copy) になったため overlay 用に clone し、 原本は後段の
        // `dragging_automation_clip` (9528 付近) で kind を取り出すまで生かす。
        let automation_clip_drag_overlay = automation_clip_drag_session.clone();
        // M14 Phase 63n-8 (#033): selected automation points (overlay 描画用、 cached 外で selected 点だけ
        // 白色 + 大 dot で上書き)。 cached layer の base draw は selection 不問の grey dot を描く、 overlay は
        // selection の差分のみを上書きする (= `data_generation` bump なしで selection 変化が反映される、
        // piano_roll の selection overlay と同 idiom)。
        let selected_automation_points_for_heavy: HashSet<AutomationPointKey> =
            selected_automation_points.iter().copied().collect();
        // M14 Phase 63n-8 (#033): lasso session の overlay clone (cached 外で lasso rect を描画)。
        let lasso_overlay = automation_lasso_session;
        // M14 Phase 63n-9 (#033): curve param drag session の overlay clone (cached 外で handle + preview
        // curve segment を描画、 drag 中のみ true value で live update)。
        let curve_param_overlay = automation_curve_param_session;
        // M9 Phase 45f: drag overlay の Resize min_len は snap unit。 下限は model の
        // `MIN_CLIP_LEN_BEATS` (= `resize_clip` の clamp と同じ 1/16)。 r.md #68: ここが
        // 0.05 だったので、 snap off (Alt) で 1/16 未満までゴーストが縮み、 release で
        // 1/16 に戻る = preview ≠ commit だった。
        // release 側 min_len と一貫させるため、 alt 真値は drag session の `last_alt` を使う
        // (overlay と release commit が必ず同一 unit で確定する)。 overlay 不在時 (drag していない)
        // は min_len 自体使われないので、 alt = false で適当な値で初期化しておけばよい。
        const MIN_CLIP_LEN: f64 = common::model::MIN_CLIP_LEN_BEATS;
        let drag_overlay_alt =
            clip_drag_overlay.as_ref().is_some_and(|(nd, _, _)| nd.last_alt);
        let drag_overlay_min_len: f64 = if view.snap.is_active(drag_overlay_alt) {
            view.snap.beat_unit(zoom_x_px_per_beat).map_or(MIN_CLIP_LEN, |u| u.max(MIN_CLIP_LEN))
        } else {
            MIN_CLIP_LEN
        };
        let loop_preview_clone = loop_drag_preview_range;
        let header_pane_copy = header_pane;
        // M14 Phase 127 (daw_01 #105): Arranger レーン overlay 用 capture (heavy closure に move)。
        // section データは borrow を closure に持ち込めないので owned Vec に clone (SectionView は
        // Arc<str> name の安価 clone)。 drag session / rect / lane 高さは Copy。
        let sections_for_draw: Vec<SectionView> = sections.to_vec();
        let section_drag_overlay = section_drag_session;
        let arranger_rect_copy = arranger_rect;
        let arranger_header_rect_copy = arranger_header_rect;
        let arranger_lane_h_copy = arranger_lane_h;
        // M10 Phase 46 → 101 (daw_01 #072): track reorder の drag preview geometry。
        // dist >= 閾値 のときのみ overlay 描画 (短 click 中は静止 = button click と区別がつかないため
        // UI ノイズ)。 **commit (`pending_drop`) と同じ `resolve_track_drop`** を通すので indicator が
        // 指す位置 = 実際に着地する位置 が必ず一致する (旧 `compute_reorder_target_index` は parent /
        // 深さを描けず blank-drop で実結果とズレていた)。
        let reorder_overlay: Option<ReorderOverlay> = track_reorder_session
            .as_ref()
            .filter(|tr| {
                let dx = tr.last_mouse_x - tr.anchor_mouse_x;
                let dy = tr.last_mouse_y - tr.anchor_mouse_y;
                (dx * dx + dy * dy).sqrt() >= REORDER_DRAG_THRESHOLD_PX
            })
            .map(|tr| {
                let drop = resolve_track_drop(
                    tracks,
                    &visible_tracks,
                    &press_tops,
                    &is_group_set,
                    &tr.source_track_ids,
                    style.indent_px,
                    tr.last_mouse_y,
                    tr.last_mouse_x,
                    tr.anchor_mouse_x,
                );
                let indicator_y = press_tops
                    .get(drop.gap)
                    .copied()
                    .or_else(|| press_tops.last().copied())
                    .unwrap_or(header_pane.y);
                let indent_x = header_pane.x + f32::from(drop.depth) * style.indent_px;
                // parent が group のとき header 行を hilight。 parent が collapsed で不可視なら
                // (visible に居ない → position None →) hilight しない (不可視 UI を光らせない意図の
                // None。 reparent 構造自体は commit と同一 resolver なので一致する)。
                let highlight_row = drop.parent.and_then(|pid| {
                    visible_tracks.iter().position(|t| t.id == pid).map(|vi| {
                        let y = press_tops.get(vi).copied().unwrap_or(header_pane.y);
                        let h = effective_track_row_h(&visible_tracks[vi], view.track_row_h);
                        Rect { x: header_pane.x, y, w: header_pane.w + lanes.w, h }
                    })
                });
                ReorderOverlay {
                    indicator_y,
                    indent_x,
                    drag_center_y: tr.last_mouse_y,
                    highlight_row,
                }
            });

        // M13 Phase 55: ruler / lanes grid を library `time_ruler` / `bar_beat_grid` に統合。
        // beat 単位の view を sample 単位の `ViewportState1D` に変換 (sample_rate = 48k で
        // 比例定数は打ち消されるので BarBeat 表示には影響しない)。
        let mapping = TimeMapping {
            sample_rate: 48_000.0,
            tempo_bpm: f64::from(view.bpm.max(1.0)),
            time_sig: (view.time_sig.0.max(1), view.time_sig.1.max(1)),
            display: TimeDisplay::BarBeat,
        };
        let spb = mapping.samples_per_beat();
        let sample_viewport =
            ViewportState1D::new(view.start_beat * spb, view.len_beats.max(1e-6) * spb);
        // r.md #48: 汎用 widget の style はパレットから組む (`Default` は「いまどのテーマで
        // 描いているか」 を知れないので廃止された)。色は arrangement の style が SSoT なので
        // 上書きし、間引き閾値だけパレット既定を引き継ぐ。
        let palette = ui.palette();
        let grid_style = BarBeatGridStyle {
            bar_color: style.bar_line,
            beat_color: style.beat_line,
            bar_line_width: style.bar_line_width_px,
            beat_line_width: style.beat_line_width_px,
            // M14 Phase 63m (daw_01 #027): zoom 連動の beat 線間引き (default 4px)。
            ..BarBeatGridStyle::from_palette(palette)
        };
        let ruler_style = TimeRulerStyle {
            bg: style.ruler_bg,
            tick_color: style.bar_line,
            label_color: style.ruler_label_color,
            bar_tick_height: 12.0,
            beat_tick_height: 5.0,
            // M14 Phase 63m (daw_01 #027): zoom 連動の label / beat tick 間引き (default 60 / 4 px)。
            ..TimeRulerStyle::from_palette(palette)
        };
        // heavy() closure は `'static` 要求なので id を hash 化して move capture。
        let id_for_inner: u64 = hash_inputs(id);

        // S4b Phase C / r.md #68: 波形 / MIDI プレビューの中身を model + audio cache から
        // 1 フレーム分だけ集めて closure に move する (`Arc<AudioSourceBuffer>` は refcount
        // clone で安価)。 visible clip のみ。 座標は **content-local 拍** で、 画面 x への
        // 換算は widget 側の `content_map` (content 原点 + ビューのズーム) が 1 本で行う。
        // SongTempo automation を持つ曲だけ曲線評価になる (無ければ定数 = 従来と同コスト)。
        // base とゴーストで **同じ写像** を使う (engine と同じ `event_wave_spans` の入力)。
        let tempo_map = common::audio_render::TempoMap::from_song(app.song_doc.song());
        let clip_content = content_build::build_clip_content(app, &tempo_map, &visible_tracks);
        // r.md #68: Shift + 端 drag (= time-stretch) のときだけ、 ゴーストの中身を
        // commit と同じ `stretch_remap` + `event_wave_spans` で組み直す (Slice 配置 /
        // Raw→Stretch 昇格まで含めてプレビュー = 確定結果)。 トリム / 移動では確定後の
        // 中身が base と同一なので空 = 描画側が `clip_content` をそのまま使う。
        let stretch_ghost_content = content_build::build_stretch_ghost_content(
            app,
            &tempo_map,
            &visible_tracks,
            clip_drag_overlay.as_ref(),
            drag_overlay_min_len,
        );

        let viewport_key_hash: u64 = hash_inputs(viewport_key);
        // r.md #58: フェードの掴む正方形を出す clip。 `response.hovered_clip` は上の
        // 「hover 計算」 で **このフレーム中に** 確定済みなので、 caller 側ミラー
        // (`app.ui_ephemeral.arrangement_hover_clip`、 1 フレーム遅れ) ではなくこちらを使う。
        // `viewport_key` にも `fold_arrangement_clip_hash` にも入れないこと。
        let hovered_clip_for_heavy: Option<ClipKey> = response.hovered_clip;
        ui.heavy(("arrangement_inner", &id), move |hctx| {
            render::render_arrangement_heavy(hctx, tracks_owned, view_copy, style_copy, lanes, ruler, header_pane, header_pane_copy, arranger_rect_copy, arranger_header_rect_copy, arranger_lane_h_copy, beat_per_px, zoom_x_px_per_beat, id_for_inner, viewport_key_hash, hovered_clip_for_heavy, clip_content, stretch_ghost_content, selected_set, selected_tracks_for_heavy, selected_automation_clips_set_for_heavy, selected_automation_points_for_heavy, mapping, sample_viewport, grid_style, ruler_style, drag_overlay_clone, drag_overlay_min_len, audio_drag_overlay, point_drag_overlay, automation_clip_drag_overlay, curve_param_overlay, lasso_overlay, section_drag_overlay, sections_for_draw, reorder_overlay, loop_preview_clone);
        });

        release::commit_releases(ui, wid, &mut response, pointer, view, style, master_row, sections, selected_clips, selected_automation_clips, selected_automation_points, &visible_tracks, &press_tops, lanes, ruler, header_pane, arranger_rect, lanes_h, arranger_lane_h, beat_per_px, zoom_x_px_per_beat, clip_drag_release, clip_short_click_pos, audio_drag_release, point_drag_release, automation_clip_drag_release, automation_curve_param_release, automation_lasso_release, lane_resize_drag_release, section_drag_release, loop_drag_release, track_volume_release, pending_drop);

        // ---- track headers (button_at × 4 + toggle_button_at × 2) + トラック選択トリガ ----
        // M10 Phase 50: tracks_for_draw を使う (release frame の optimistic preview と同順序)。
        // M14 Phase 63c (#016): visible_indices を pre-compute して collapsed 親配下を skip、
        // visible_i (描画上の row index) を row_y に使う。 各 track header に depth * indent_px の
        // 左 indent + group track には disclosure ▼/▶ アイコン。 selection は selected_tracks_set で判定。
        // 修飾 (Shift / Ctrl) で Single / RangeFromAnchor / Toggle を decode して渡す。
        // 1 frame 内で最初に click された track id を `clicked_track` に蓄え、 loop 後に
        // `apply_select_tracks` を 1 度呼ぶ (loop 内で複数発行しないため)。
        //
        // M14 Phase 77 (daw_01 #048): header row 描画 push_* 群を `header_pane` で auto-scissor
        // する。 closure 化すると `ui.xxx` の大量 rename を要するため、 `with_clip_rect` と
        // 同等の `current_clip` push/pop を open-code で実施 (`Ui::with_clip_rect` 実装と同 idiom、
        // `pub(crate)` 経由)。
        let prev_clip_for_headers = ui.current_clip_rect();
        ui.set_current_clip_rect(Some(
            daw_ui_core::ui::merge_clip(prev_clip_for_headers, Some(header_pane)).unwrap_or(header_pane),
        ));
        let visible_idx_for_headers = compute_visible_indices(&tracks_for_draw);
        // M14 Phase 63n-1 (#028): track headers loop 用 prefix sum tops (immediate mode、 cached 外)。
        // `tracks_for_draw` は既に visible-only な Vec を Arc 化したものなので、 そのまま slice 経由で
        // 渡せば clone 不要 (header_pane.y == lanes.y は rect 分割 origin 共通)。
        let header_tops = visible_track_row_tops(
            &tracks_for_draw,
            header_pane.y,
            view.track_top,
            view.track_row_h,
        );
        let mut clicked_track_for_select: Option<u32> = None;
        let mut disclosure_clicked: Option<u32> = None;
        if header_w > 0.0 {
            for (visible_i, &i) in visible_idx_for_headers.iter().enumerate() {
                let t = &tracks_for_draw[i];
                let row_y = header_tops[visible_i];
                let row =
                    Rect { x: header_pane.x, y: row_y, w: header_pane.w, h: view.track_row_h };
                if row.y + row.h < header_pane.y || row.y > header_pane.y + header_pane.h {
                    continue;
                }

                // M14 Phase 63n-10 (#034): master row 専用 header 描画。 mute/solo button / volume band /
                // group disclosure / row click → トラック選択の全 path を skip し、 neutral gray 背景 +
                // "Master" label + lane disclosure (`▶`/`▼`) のみを描画する (daw_01 #034 §B 仕様)。
                // 通常 track 経路の `selected_tracks` / `is_group_set` 判定とは独立 (master は selection
                // 対象外、 group でもない、 = 「特殊な行」 として描画分岐)。
                if t.id == MASTER_TRACK_ID {
                    // M14 Phase 90 (daw_01 #061): master 行も選択可能。 selected なら通常 track と同じ
                    // `track_selected_bg`、 非選択は従来の `master_row_color`。 "Master" label / lane
                    // disclosure はこの背景の上に重畳描画 (色は据え置き)。
                    let master_bg = if selected_tracks.contains(&t.id) {
                        style.track_selected_bg
                    } else {
                        style.master_row_color
                    };
                    ui.panel(("arr_master_thbg", 0_u32), row, master_bg, 0.0);
                    let indent = f32::from(t.depth) * style.indent_px; // 0 固定だが既存 idiom 維持
                    let row_for_layout = Rect {
                        x: row.x + indent,
                        y: row.y,
                        w: (row.w - indent).max(2.0),
                        h: row.h,
                    };
                    let layout = header_row_layout(row_for_layout, 0.0); // volume band 無し
                    // "Master" label を name_rect に push_text (button にはしない = click は selection 経路に
                    // 流さない)。 font_size は style.master_row_label_size、 色は master_row_label_color。
                    let label_rect = layout.name_rect;
                    ui.push_text(GlyphArea {
                        text: Arc::from("Master"),
                        left: label_rect.x + 4.0,
                        top: label_rect.y
                            + (label_rect.h - style.master_row_label_size * 1.2) * 0.5,
                        font_size: style.master_row_label_size,
                        line_height: style.master_row_label_size * 1.2,
                        color: style.master_row_label_color,
                        clip_rect: Some(label_rect),
                        ..GlyphArea::default()
                    });
                    // M14 Phase 63n-10 (#034): lane disclosure (`+` / `-`) を master row でも描画 (= 通常
                    // track と同 idiom)。 click 検出は press block 経由で `press_lane_toggle = Some(t.id)`
                    // (= `MASTER_TRACK_ID`) が立ち、 loop 後に `ToggleTrackAutomationCollapsed
                    // { track: MASTER_TRACK_ID }` が発火する SSoT。
                    if !t.automation_lanes.is_empty() {
                        let lane_disc = layout.lane_disc_rect;
                        let label = if t.automation_lanes_collapsed { "+" } else { "-" };
                        ui.push_text(GlyphArea {
                            text: label.into(),
                            left: lane_disc.x,
                            top: lane_disc.y
                                + (lane_disc.h - style.automation_disclosure_size * 1.2) * 0.5,
                            font_size: style.automation_disclosure_size,
                            line_height: style.automation_disclosure_size * 1.2,
                            color: style.disclosure_color,
                            clip_rect: Some(lane_disc),
                            ..GlyphArea::default()
                        });
                    }
                    // Response.track_header_rects に積む (caller が master row の rect 領域を識別可能に)。
                    response.track_header_rects.push((t.id, row));
                    // M14 Phase 90 (daw_01 #061): master 行の header click もトラック選択。 通常 track と
                    // 同じ `clicked_track_for_select` 経路を再利用し、 loop 後の modifier-aware 発行に乗せる
                    // (Single なら next=[MASTER_TRACK_ID])。 lane disclosure (`+`/`-`) rect 内 release は
                    // automation collapse トグルが priority なので除外する (disclosure > row-select)。
                    // master には mute/solo/volume band が無いので row 全体 (disclosure 除く) が対象。
                    if pointer.primary_just_released
                        && let Some((rx, ry)) = pointer.pos
                        && row.contains(rx, ry)
                        && (t.automation_lanes.is_empty() || !layout.lane_disc_rect.contains(rx, ry))
                        && !ui.has_open_popups()
                    {
                        clicked_track_for_select = Some(t.id);
                    }
                    continue;
                }

                // 背景 (selection > 通常)。 M14 Phase 113 (daw_01 #085): group track 専用の
                // 背景 tint は撤去 (group は indent / disclosure ▶▼ で識別、 背景は他 track と同じ
                // neutral header_bg)。 `is_group_set` は依然 disclosure 描画 / hit-test で使う。
                if selected_tracks.contains(&t.id) {
                    ui.panel(("arr_thsel", t.id), row, style.track_selected_bg, 0.0);
                } else {
                    ui.panel(("arr_thbg", t.id), row, style.header_bg, 0.0);
                }

                // M14 Phase 63c (#016): depth * indent_px の左 indent。 layout 計算は indent 反映後の
                // row_inner で実行する (= row.x + indent、 row.w - indent)。 #069 で color strip も
                // この indent に追従させるため、 strip 描画の前に indent を確定する。
                let indent = f32::from(t.depth) * style.indent_px;

                // M14 Phase 87 (daw_01 #059): track color strip。 Some(c) のとき header の (indent 後の)
                // 左端に縦ストライプを背景の上から描く (selected/group/video 背景と色衝突しない)。
                // None は strip 非描画 = 既存挙動完全互換。
                // M14 Phase 97 (daw_01 #069): x を row.x + indent にして名前と同じだけ右にインデント
                // (子トラックで色ストライプが名前と一緒にネスト)。 depth==0 は indent=0 で従来と pixel 一致。
                if let Some(c) = t.color
                    && style.track_color_strip_w > 0.0
                {
                    ui.push_rect(RectCommand {
                        rect: Rect {
                            x: row.x + indent,
                            y: row.y,
                            w: style.track_color_strip_w,
                            h: row.h,
                        },
                        fill: c,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: Some(row),
                    });
                }

                let row_for_layout = Rect {
                    x: row.x + indent,
                    y: row.y,
                    w: (row.w - indent).max(2.0),
                    h: row.h,
                };
                let band_h = if matches!(t.kind, TrackKind::Video) {
                    // M14 Phase 72 (#044): video track は volume band を非描画。
                    0.0
                } else {
                    style.track_volume_band_h
                };
                let layout = header_row_layout(row_for_layout, band_h);
                let name_rect = layout.name_rect;
                let [m_rect, s_rect, r_rect] = layout.buttons;

                // M10 Phase 47b: track volume band 描画。
                // drag 中の track はその drag session の last_mouse_x で preview volume を計算 (リアルタイム feedback)。
                if let Some(band) = layout.volume_band {
                    let dragging_this = track_volume_session
                        .as_ref()
                        .filter(|tv| tv.track_id == t.id);
                    let display_v = if let Some(tv) = dragging_this {
                        volume_from_mouse_x(tv.last_mouse_x, tv.band_rect.x, tv.band_rect.w)
                    } else {
                        // stored amp → frac (band と同じ MeterScale 空間)。 +6dB 側も
                        // 正しい fill 位置で描く (r.md #11)。
                        MeterScale::default().amp_to_frac(t.volume)
                    };
                    ui.panel(
                        ("arr_tvol_track", t.id),
                        band,
                        style.track_volume_band_track,
                        0.0,
                    );
                    let fill_w = band.w * display_v;
                    if fill_w > 0.0 {
                        ui.panel(
                            ("arr_tvol_fill", t.id),
                            Rect { x: band.x, y: band.y, w: fill_w, h: band.h },
                            style.track_volume_band_fill,
                            0.0,
                        );
                    }
                }

                // M14 Phase 63c (#016): disclosure ▼/▶ — group track のみ描画 + click で
                // ToggleGroupCollapsed Edit 発行 (loop 後に発火、 トラック選択より priority 高)。
                let is_group = is_group_set.contains(&t.id);
                let disclosure_rect = disclosure_rect_for(name_rect, style, t.depth);
                if is_group {
                    let label = if t.collapsed { "▶" } else { "▼" };
                    ui.push_text(GlyphArea {
                        text: label.into(),
                        left: disclosure_rect.x + disclosure_rect.w * 0.2,
                        top: disclosure_rect.y + (disclosure_rect.h - style.track_text_size * 1.2) * 0.5,
                        font_size: style.track_text_size,
                        line_height: style.track_text_size * 1.2,
                        color: style.disclosure_color,
                        clip_rect: Some(disclosure_rect),
                        ..GlyphArea::default()
                    });
                    if pointer.primary_just_released
                        && let Some((rx, ry)) = pointer.pos
                        && disclosure_rect.contains(rx, ry)
                    {
                        disclosure_clicked = Some(t.id);
                    }
                }
                // M14 Phase 63n-1 (#028) + 63n-2 修正: track 行 header の lane disclosure (S button の
                // 右、 `layout.lane_disc_rect`) を **`+` / `-`** で描画。 旧 `▽`/`▷` (U+25BD/U+25B7) は
                // font 不在で不可視 click target になる、 旧 `▼`/`▶` は group disclosure と同 glyph で
                // user が混同する両方の問題を解消した最終形 (#028 follow-up user feedback で確定)。
                // `automation_lanes.is_empty()` の track は描画しない (= layout 上は rect 確保するが
                // visual には何も出ない、 click もメッセージにならない)。 click 検出は press block で
                // 同じ `layout.lane_disc_rect` を使うので描画と hit-test の SSoT が完全一致。
                if !t.automation_lanes.is_empty() {
                    let lane_disc = layout.lane_disc_rect;
                    let label = if t.automation_lanes_collapsed { "+" } else { "-" };
                    ui.push_text(GlyphArea {
                        text: label.into(),
                        left: lane_disc.x,
                        top: lane_disc.y + (lane_disc.h - style.automation_disclosure_size * 1.2) * 0.5,
                        font_size: style.automation_disclosure_size,
                        line_height: style.automation_disclosure_size * 1.2,
                        color: style.disclosure_color,
                        clip_rect: Some(lane_disc),
                        ..GlyphArea::default()
                    });
                }
                // disclosure を除いた name 領域 (group の場合は disclosure 分削る)
                let name_rect_visible = if is_group {
                    Rect {
                        x: disclosure_rect.x + disclosure_rect.w,
                        y: name_rect.y,
                        w: (name_rect.w - disclosure_rect.w).max(2.0),
                        h: name_rect.h,
                    }
                } else {
                    name_rect
                };
                let button_zones: [Rect; 4] = [name_rect_visible, m_rect, s_rect, r_rect];

                let id_name = ("arr_tname", t.id);
                let id_mute = ("arr_tmute", t.id);
                let id_solo = ("arr_tsolo", t.id);
                let id_armed = ("arr_tarmed", t.id);

                let track_id = t.id;
                let muted = t.muted;
                let solo = t.solo;
                let armed = t.armed;

                let name_text = t.name.clone();
                // M14 Phase 63c (#016): name 領域 click は modifier-aware なトラック選択を loop 後に
                // 発行する形に変更。 button_at_clicked で click 検知のみ行い、 内部で Edit は emit
                // しない (旧設計は button_at の closure 内で選択更新を emit していた)。
                // M14 Phase 105 (#076): track 名 font は `style.track_text_size` に追従させる
                // (汎用 button の 16px 固定では daw_01 が名前サイズを下げられないため)。
                // M14 Phase 107 (#079): track 名は常に左寄せ (Reaper / Cubase / Live と同じ。
                // 先頭が識別に最重要、 省略時の左寄せとも一致)。 M/S/R toggle は中央寄せのまま。
                if ui.button_at_clicked_sized_aligned(
                    id_name,
                    &name_text,
                    name_rect_visible,
                    style.track_text_size,
                    daw_ui_core::widgets::button::ButtonTextAlign::Left,
                ) && !ui.has_open_popups()
                {
                    // 名前ボタン経路も popup ガード対象 (release catch-all / master 行と同件)。
                    // daw-ui の button 自体も popup mask を見るようにしたが、 ここは
                    // 「どの行が click されたか」 を蓄える daw_gui 側の状態遷移なので明示する。
                    clicked_track_for_select = Some(t.id);
                }
                // M14 Phase 118 (daw_01 #092): group track 名 double-click rename の信頼性。 深くネストした
                // group track は indent で name_rect が 20px floor まで潰れ、 さらに disclosure 分を引くと
                // `name_rect_visible` が 2〜4px になり double-click が当たらなかった。 group track のみ
                // **header row 全体** を rename hit zone にし、 single-click で別意味を持つ sub-zone
                // (M·S·R / lane disclosure / volume band drag / header splitter) を除外する。 これで
                // indent 空白 + 名前帯のどこを double-click しても rename が始まる (REAPER の TCP 名 dblclick
                // 流)。 通常 track は `name_rect_visible` のまま (名前帯が潰れないので挙動完全不変、 sub-zone
                // 除外も常に no-op)。
                //
                // M14 Phase 119 (daw_01 #092 follow-up): **group disclosure (`▶`/`▼`) も rename zone に含める**。
                // depth-0 (top-level) group は disclosure が name 帯の左端 (= indent 空白が無く x∈[pad, pad+
                // indent_px]) に張り付くため、 旧実装は disclosure を sub-zone 除外していた結果「最上段 / 子持ち
                // group の名前左側を double-click しても rename されない」 症状になっていた (master row の有無は
                // 無関係 = 最上段が top-level group になりがちなだけの相関、 と pixel/hit-test 検証で確定)。 disclosure
                // の **single-click** 折り畳みは別経路 (`disclosure_clicked`) で従来どおり (回帰なし)。 **double-click**
                // は明確に rename 意図なので disclosure 上でも rename を起こす。 double-click が disclosure を踏むと
                // 2 release で折り畳みが 2 回 toggle するが、 daw_01 の `collapsed_groups` (HashSet) を直接 flip する
                // 非 undoable な view-state edit なので net-zero (= fold 状態保存、 undo 履歴も汚さない)。 M·S·R /
                // lane disclosure は name 帯の **右**で名前と無関係なので除外を維持 (button の double-toggle を rename に
                // 化けさせない)、 volume band も名前帯の下の独立 drag 控除なので維持。
                let rename_hit = if is_group { row } else { name_rect_visible };
                if let Some((dcx, dcy)) = ui.take_double_click_in_rect(rename_hit) {
                    let in_subzone = m_rect.contains(dcx, dcy)
                        || s_rect.contains(dcx, dcy)
                        || r_rect.contains(dcx, dcy)
                        || (!t.automation_lanes.is_empty()
                            && layout.lane_disc_rect.contains(dcx, dcy))
                        || layout.volume_band.is_some_and(|b| b.contains(dcx, dcy))
                        // M14 Phase 118 follow-up: group の broad zone は header / lanes 境界まで届くので、
                        // header 幅 splitter (#091) の hot zone も除外して rename と resize を分離する。
                        || header_resize_splitter_at(rect, header_w, style, dcx, dcy);
                    if !in_subzone {
                        ui.push_edit({ let v_id = track_id; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::BeginRenameTrack(v_id)); }) });
                    }
                }
                ui.toggle_button_at(id_mute, "M", m_rect, muted, &style.mute_button, |_| {
                    { let v_id = track_id; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::ToggleTrackMute(v_id)); }) }
                });
                ui.toggle_button_at(id_solo, "S", s_rect, solo, &style.solo_button, |_| {
                    { let v_id = track_id; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::ToggleTrackSolo(v_id)); }) }
                });
                // M14 Phase 68 (#040): R button (Record-arm)。 mute / solo と完全同 idiom、
                // armed track のみが audio engine の録音入力対象 (caller 仕様)。
                ui.toggle_button_at(id_armed, "R", r_rect, armed, &style.armed_button, |_| {
                    { let v_id = track_id; Edit::mutate(move |app: &mut AppData| { app.handle_event(AppEvent::ToggleTrackArmed(v_id)); }) }
                });
                // Phase 47c: ↑/↓/× button は削除 (drag&drop reorder + Delete shortcut で代替)。
                // `MoveTrackUp/Down` は context menu / keyboard 用に残す (削除は root の arbiter)。

                // Response.track_header_rects に積む
                response.track_header_rects.push((t.id, row));

                // トラック選択トリガ: row 内 release + button_zones / disclosure いずれにも非 hit。
                // catch-all は modifier-aware な選択更新の元データを蓄えるだけ (発行は loop 後)。
                // lane disclosure (`+`/`-`) と volume band も除外する — 除外しないと
                // toggle / volume drag の release が選択更新を併発し、 multi-select が
                // 単一選択に潰れる (master 分岐は除外済みで通常 track だけ漏れていた、 review)。
                // popup (右クリックメニュー) が開いている frame も除外する: context menu は
                // `capture_input == false` で背景 pointer を mask しないので、 menu item への
                // click がこの row にも届き multi-select が単一に潰れる → 「選択トラックを
                // まとめて Delete / 複製」 が右クリック 1 本にしか効かなくなる
                // (clip 短 click の r.md #14 ガードと同 class、 r.md #43 で同件対処)。
                if pointer.primary_just_released
                    && let Some((rx, ry)) = pointer.pos
                    && row.contains(rx, ry)
                    && !button_zones.iter().any(|b| b.contains(rx, ry))
                    && !(is_group && disclosure_rect.contains(rx, ry))
                    && (t.automation_lanes.is_empty()
                        || !layout.lane_disc_rect.contains(rx, ry))
                    && !layout.volume_band.is_some_and(|b| b.contains(rx, ry))
                    && !ui.has_open_popups()
                {
                    clicked_track_for_select = Some(t.id);
                }
            }
        }
        // M14 Phase 77 (daw_01 #048): header_pane scope を復元 (= track header push_* 群が終了)。
        ui.set_current_clip_rect(prev_clip_for_headers);

        // M14 Phase 63c (#016): disclosure click → ToggleGroupCollapsed (priority 高、 トラック選択は
        // この frame では skip = group の collapsed toggle 動作のみで selection は変えない、
        // Reaper / Live と同じ UX)。
        if let Some(tid) = disclosure_clicked {
            ui.push_edit({ let v_id = tid; Edit::mutate(move |app: &mut AppData| { if app.ui_prefs.collapsed_groups.contains(&v_id) { app.ui_prefs.collapsed_groups.remove(&v_id); } else { app.ui_prefs.collapsed_groups.insert(v_id); } }) });
            clicked_track_for_select = None;
        }

        // M14 Phase 63c (#016): clicked_track があれば modifier-aware なトラック選択を
        // 1 度だけ発行する。 Single → next = [tid]、 anchor 更新。 RangeFromAnchor (Shift)
        // → anchor から visible 列の連続範囲 (anchor が None なら Single 同等)。
        // Toggle (Ctrl) → tid を selected に対して toggle。
        if let Some(tid) = clicked_track_for_select {
            // press 時 snapshot を真値にする (release frame の生読みは
            // ModifiersChanged 先行 race で Ctrl/Shift+click が Single に化ける、
            // clip 短クリックの `last_ctrl`/`last_shift` と同 class)。
            let (shift, ctrl) = {
                let state: &ArrangementState = ui.widget_state(wid);
                (state.press_modifiers.shift, state.press_modifiers.ctrl)
            };
            let modifier = SelectModifier::from_modifiers(shift, ctrl);
            // r.md #35: アンカーは `SelectionState.track_anchor` が所有する (旧: widget state の
            // `ArrangementState.selection_anchor`)。 全選択面で同じ場所・同じ更新規則にするため
            // (`docs/plan_selection_modifiers.md` §4.3)。 更新規則も **Single / Toggle で更新、
            // Range は据え置き** に直した (旧実装は Single / Range で更新していたので、 Shift+click
            // を繰り返すと基点が歩いて範囲を伸縮できなかった。 Explorer / Finder / REAPER は据え置き)。
            // r.md #43: 選択遷移の解決自体は `AppData::apply_select_tracks` に集約した
            // (mixer strip の click と同じ 1 実装 + last-wins タグを Tracks に倒す口)。
            // view が渡すのは「この view の可視順」 だけ。
            let visible_ids: Vec<u32> = visible_idx_for_headers
                .iter()
                .map(|&i| tracks_for_draw[i].id)
                .collect();
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.apply_select_tracks(tid, modifier, &visible_ids);
            }));
            response.selection_changed = true;
        }

        // ---- M14 Phase 63f (#020): clip_rects を visible-tracks 順 (= 描画順) で積む ----
        // draw_clips と同じ culling: row が lanes 外 / clip が view beat 範囲外なら除外。
        // 部分カリングは full rect を返す (caller の context_menu_for は popup_rect_clamped_at で
        // 画面外はみ出しを吸収するため、 視野内に少しでも見えていれば十分操作可能)。
        let view_end = view.start_beat + view.len_beats;
        for (i, t) in visible_tracks.iter().enumerate() {
            let row_top = press_tops[i];
            let row_h = effective_track_row_h(t, view.track_row_h);
            if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
                continue;
            }
            for c in &t.clips {
                let end = c.start_beat + c.len_beats;
                if end < view.start_beat || c.start_beat > view_end {
                    continue;
                }
                let r = clip_to_rect(row_top, row_h, c, view, lanes);
                response.clip_rects.push((ClipKey { track: t.id, clip: c.id }, r));
            }
        }

        // ---- automation_lane_default_rects を毎 frame 積む ----
        // 各 visible lane header の default value 数値入力フィールド rect (= caller が
        // scrubable_number_at を overlay する位置)。 master row (synthetic track) の lane も
        // `visible_tracks[t_idx].id == MASTER_TRACK_ID` で含まれる。 行高不足で field rect が
        // 無い (= layout.default_field_rect == None) lane は除外。
        for_each_visible_lane(
            &visible_tracks,
            &press_tops,
            view.track_row_h,
            header_pane.x,
            header_pane.w,
            lanes.x,
            lanes.w,
            style,
            |t_idx, _l_idx, lane, h_rect, _body_rect| {
                if h_rect.y + h_rect.h < lanes.y || h_rect.y > lanes.y + lanes.h {
                    return;
                }
                if let Some(layout) = automation_lane_header_layout(h_rect, style)
                    && let Some(field) = layout.default_field_rect
                {
                    let key = AutomationLaneKey {
                        track: visible_tracks[t_idx].id,
                        lane: lane.id,
                    };
                    response.automation_lane_default_rects.push((key, field));
                }
            },
        );

        // ---- point drag 中の live 値を response に乗せる ----
        // overlay ghost (上の cached 外描画) と同じ式で next_value / cursor を算出し、 caller が
        // カーソル近傍に現値を人間可読単位で表示できるようにする。 release frame は session が
        // take 済 (None) になるので、 ここは drag 継続中のみ Some。
        if !pointer.primary_just_released
            && let Some(pd) = point_drag_session
        {
            let dx = pd.last_mouse.0 - pd.anchor_mouse.0;
            let dy = pd.last_mouse.1 - pd.anchor_mouse.1;
            let beat_to_px = f64::from(pd.body_rect_anchor.w) / view.len_beats.max(1e-6);
            let raw_dt = f64::from(dx) / beat_to_px;
            let raw_abs = pd.clip_start_beat + pd.anchor_time_beat + raw_dt;
            let snapped_abs = view.snap.snap_beat(raw_abs, pd.last_alt, zoom_x_px_per_beat);
            let next_local =
                (snapped_abs - pd.clip_start_beat).clamp(0.0, pd.clip_len_beats.max(0.0));
            let next_value =
                (pd.anchor_value_norm - dy / pd.clip_rect_anchor.h.max(1.0)).clamp(0.0, 1.0);
            let abs_beat = pd.clip_start_beat + next_local;
            #[allow(clippy::cast_possible_truncation)]
            let px = pd.body_rect_anchor.x + ((abs_beat - view.start_beat) * beat_to_px) as f32;
            let py = pd.clip_rect_anchor.y + (1.0 - next_value) * pd.clip_rect_anchor.h;
            response.automation_point_drag = Some(AutomationPointDragInfo {
                key: pd.point,
                value_norm: next_value,
                cursor: (px, py),
            });
        }

        // ---- M14 Phase 63n-2 (#028): automation_point_rects を毎 frame 積む ----
        // for_each_visible_lane で SSoT を共有し、 各 visible point を screen 座標に変換した
        // 半径 8px 正方形 rect を返す (= caller の context_menu_for で右クリック anchor として使う)。
        // collapsed group 内 / collapsed lane / invisible lane / view beat 範囲外の point は除外。
        let radius = style.automation_point_radius_px.max(2.0);
        for_each_visible_lane(
            &visible_tracks,
            &press_tops,
            view.track_row_h,
            header_pane.x,
            header_pane.w,
            lanes.x,
            lanes.w,
            style,
            |t_idx, _l_idx, lane, _h_rect, body_rect| {
                if body_rect.y + body_rect.h < lanes.y || body_rect.y > lanes.y + lanes.h {
                    return;
                }
                let track_id = visible_tracks[t_idx].id;
                let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
                let pad = style.automation_clip_v_pad_px;
                let clip_y = body_rect.y + pad;
                let clip_h = (body_rect.h - pad * 2.0).max(2.0);
                for clip_in in &lane.clips {
                    let end = clip_in.start_beat + clip_in.len_beats;
                    if end < view.start_beat || clip_in.start_beat > view_end {
                        continue;
                    }
                    for (p_idx, p) in clip_in.points.iter().enumerate() {
                        let abs_beat = clip_in.start_beat + p.time_beat;
                        if abs_beat < view.start_beat - 1e-6
                            || abs_beat > view.start_beat + view.len_beats + 1e-6
                        {
                            continue;
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        let px = body_rect.x
                            + ((abs_beat - view.start_beat) * beat_to_px) as f32;
                        let py = clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                        let key = AutomationPointKey {
                            clip: AutomationClipKey {
                                track: track_id,
                                lane: lane.id,
                                clip: clip_in.id,
                            },
                            #[allow(clippy::cast_possible_truncation)]
                            point_idx: p_idx as u32,
                        };
                        let r = Rect {
                            x: px - radius,
                            y: py - radius,
                            w: radius * 2.0,
                            h: radius * 2.0,
                        };
                        response.automation_point_rects.push((key, r));
                    }
                }
            },
        );

        // ---- M14 Phase 63n-3 (#028): automation_clip_rects を毎 frame 積む ----
        // for_each_visible_lane で SSoT を共有 (= 描画 / hit-test と同じ式)、 visible automation
        // clip の lane body 内 rect (縦 padding 適用済) を返す。 collapsed group / hidden lane / view
        // beat 範囲外の clip は除外。 caller は右クリック context menu (Make Unique / Delete) の
        // anchor として使う想定。
        for_each_visible_lane(
            &visible_tracks,
            &press_tops,
            view.track_row_h,
            header_pane.x,
            header_pane.w,
            lanes.x,
            lanes.w,
            style,
            |t_idx, _l_idx, lane, _h_rect, body_rect| {
                if body_rect.y + body_rect.h < lanes.y || body_rect.y > lanes.y + lanes.h {
                    return;
                }
                let track_id = visible_tracks[t_idx].id;
                // daw_01 #086: lane の実行 rect (= body_rect そのもの) を毎 frame 返す。
                // Z 縦ズームがレイアウトを複製せず lane の実 y を引けるようにする。
                response.automation_lane_rects.push((
                    AutomationLaneKey { track: track_id, lane: lane.id },
                    body_rect,
                ));
                let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
                let pad = style.automation_clip_v_pad_px;
                let clip_y = body_rect.y + pad;
                let clip_h = (body_rect.h - pad * 2.0).max(2.0);
                for clip_in in &lane.clips {
                    let end = clip_in.start_beat + clip_in.len_beats;
                    if end < view.start_beat || clip_in.start_beat > view_end {
                        continue;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    let cx_clip = body_rect.x
                        + ((clip_in.start_beat - view.start_beat) * beat_to_px) as f32;
                    #[allow(clippy::cast_possible_truncation)]
                    let cw = ((clip_in.len_beats * beat_to_px) as f32).max(2.0);
                    let key = AutomationClipKey {
                        track: track_id,
                        lane: lane.id,
                        clip: clip_in.id,
                    };
                    response.automation_clip_rects.push((
                        key,
                        Rect { x: cx_clip, y: clip_y, w: cw, h: clip_h },
                    ));
                }
            },
        );

        // M14 Phase 63n-3 (#028): drag 中の automation clip kind を response に反映 (cursor /
        // status indicator 用)。 既存 `dragging` (MIDI clip 用) と直交。
        response.dragging_automation_clip = automation_clip_drag_session.map(|acd| acd.kind);

        response
    }
