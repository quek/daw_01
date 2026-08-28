//! lane 領域 (clip 描画域 = `f.lanes`) の press 振り分け。 ゾーンごとに排他な 7 本のチェーンで、
//! 優先順位は audio grip > clip > curve handle > automation point > automation clip >
//! Alt+drag resize > lasso。 各分岐は消費したことを `PressClaim` に立てて後続を止める。

use super::*;

/// audio grip (gain band / fade corner) → MIDI/Audio clip の Move/Resize。
/// audio grip が先勝したら clip drag は起動しない (`else if` の排他をそのまま維持)。
///
/// M14 Phase 63k (#025): audio gesture (gain handle / fade corner) を最優先で振り分ける。
/// audio grip にヒットしたら clip_drag (Move/Resize) は起動しない (排他) — `audio_grip_hit_in_lanes`
/// が先勝で priority 判定する。 modifier (Shift / Ctrl) は audio gesture では無視 (Bitwig spec
/// §3.5/§3.6 と整合、 modifier-free な直感的操作)。 audio_edit が None の clip ではこの
/// ブロックは即 None を返すため、 既存挙動 (MIDI / Vocal clip) は影響を受けない。
pub(super) fn clip_zone(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
) {
    let (px, py) = (hit.px, hit.py);
    let audio_press = if !claim.splitter && hit.in_lanes && !hit.shift && !hit.ctrl {
        audio_grip_hit_in_lanes(
            &f.visible_tracks,
            &f.tops,
            f.view,
            f.lanes,
            px,
            py,
            f.style,
        )
    } else {
        None
    };
    if let Some((hit_key, grip)) = audio_press {
        if let Some((t_idx, t)) =
            f.visible_tracks.iter().enumerate().find(|(_, t)| t.id == hit_key.track)
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
                f.tops[t_idx],
                effective_track_row_h(t, f.view.track_row_h),
                c,
                f.view,
                f.lanes,
            );
            // Gain は常に vertical lock 確定 (横 drag は無視)、 Fade は press 時 `None` で
            // sticky direction 待ち (continuation で閾値超えた方向に lock)。
            let locked_horizontal = match kind {
                AudioDragKind::Gain => Some(false),
                _ => None,
            };
            let session = AudioDragSession {
                key: hit_key,
                kind,
                anchor_gain_db: c.audio_edit.map_or(0.0, |a| a.gain_db),
                anchor_fade,
                clip_rect_anchor: r_anchor,
                content_map_anchor: content_map(c, f.view, f.lanes),
                clip_bg_anchor: draw::clip_effective_fill(c, t.kind, f.style),
                anchor_mouse: (px, py),
                last_mouse: (px, py),
                locked_horizontal,
            };
            let state: &mut ArrangementState = ui.widget_state(f.wid);
            state.audio_drag = Some(session);
            claim.session = true;
        }
    } else if !claim.splitter
        && hit.in_lanes
        && let Some((hit_key, kind)) = clip_hit(
            &f.visible_tracks,
            &f.tops,
            f.view,
            f.lanes,
            px,
            py,
            f.style.resize_handle_px,
        )
    {
        // r.md #35: 旧実装はここに `(!shift || ctrl || resize)` gate があり、 Shift+press を
        // clip_drag から弾いて marquee (#75) に渡していた。 その marquee は 0 サイズ矩形では
        // 何も拾わないため **Shift+click が完全に無反応** になっていた。 gate を外して
        // Shift+press でも drag session を張り、 release の短 click 格下げ経路
        // (`clip_short_click_pos` が `(ctrl, shift)` を持ち回る) で範囲選択に解決する。
        // Shift+Move ドラッグは通常の移動、 Shift+resize は従来どおり time-stretch (#61)。
        let drag_keys: Vec<ClipKey> = if f.selected_clips.contains(&hit_key) {
            f.selected_clips.to_vec()
        } else {
            vec![hit_key]
        };
        let mut anchors: Vec<ClipDragAnchor> = Vec::new();
        for k in &drag_keys {
            // visible_tracks の visible-idx を anchor.track_index に保存 (release frame の
            // delta 計算 + draw_drag_preview の new_idx も同じ visible-idx で動く)。
            if let Some((t_idx, t)) =
                f.visible_tracks.iter().enumerate().find(|(_, t)| t.id == k.track)
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
            let press_alt = f.pointer.modifiers.alt;
            let press_ctrl = f.pointer.modifiers.ctrl;
            let press_shift = f.pointer.modifiers.shift;
            let state: &mut ArrangementState = ui.widget_state(f.wid);
            state.clip_drag = Some(ClipDragSession {
                kind,
                anchor_mouse: (px, py),
                last_mouse: (px, py),
                last_alt: press_alt,
                last_ctrl: press_ctrl,
                last_shift: press_shift,
                anchors,
            });
            claim.session = true;
        }
    }
}

/// automation 系 5 本 + Alt+drag フォールバック + lasso。
/// 内部で `curve_handle` → `point` → `automation_clip` → `alt_resize` → `lasso` を
/// **この順**で呼ぶ (優先順位そのもの)。
pub(super) fn automation(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
    actions: &mut PressActions,
) {
    curve_handle(ui, f, hit, claim);
    point(ui, f, hit, claim, actions);
    automation_clip(ui, f, hit, claim);
    alt_resize(ui, f, hit, claim, actions);
    lasso(ui, f, hit, claim, actions);
}

/// M14 Phase 63n-9 (#033): tension/bend handle press 検出 — **point press より先勝** で
/// selected point の Bezier / Exponential 入射 segment 中央 handle に当たった場合、 curve
/// param drag を起動。 handle は curve から 10px 上方向 offset で描画されるので point dot
/// 位置とは交差しないが、 priority 上 handle > point > lasso にする (= curve param 編集が
/// 最も狙った操作のため)。 modifier (Shift / Ctrl / Alt) は handle press では無視 (= Alt
/// は drag continuation で × 0.2 sensitivity に使う、 Shift/Ctrl は将来 multi-handle 編集に
/// 予約) — handle 上 click は **常に curve param drag 起動**。
fn curve_handle(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
) {
    if !claim.splitter
        && hit.in_lanes
        && let Some((handle_point, handle_kind, handle_value, lane_h)) =
            find_curve_param_handle_at(
                &f.visible_tracks,
                &f.tops,
                f.view,
                f.lanes,
                f.selected_automation_points,
                f.style,
                hit.px,
                hit.py,
            )
    {
        let effective_h = f32::from(lane_h.max(40));
        let last_alt = f.pointer.modifiers.alt;
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.automation_curve_param_drag = Some(AutomationCurveParamDragSession {
            point: handle_point,
            kind: handle_kind,
            anchor_value: handle_value,
            anchor_mouse_y: hit.py,
            last_mouse_y: hit.py,
            last_alt,
            effective_lane_height_px: effective_h,
            preview_value: handle_value,
        });
        claim.curve_handle = true;
        claim.session = true;
    }
}

/// M14 Phase 63n-8 (#033): point press は **Shift / Ctrl 修飾も accept** (release 時 短 click
/// 化で toggle / replace を判定する)。 旧 Phase 63n-2 は `!shift && !ctrl` で除外していたが、
/// それだと Shift+click on point が何の session も起動せず toggle が発火しない bug を持っていた。
/// Shift+click on point は drag>=4px なら通常 move (= MoveAutomationPoints、 modifier 無視で
/// pressed が selection に含まれていれば multi)、 短 click なら toggle。 Ctrl 同様。
///
/// M14 Phase 63n-9 (#033): handle press が先勝した場合 (= `claim.curve_handle`) は
/// point press を skip (= 同 frame で 2 session が起動するのを回避)。
fn point(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
    actions: &mut PressActions,
) {
    let (px, py) = (hit.px, hit.py);
    if !(!claim.splitter && !claim.curve_handle && hit.in_lanes) {
        return;
    }
    let Some((point_key, _r)) = automation_point_at(
        &f.visible_tracks,
        &f.tops,
        f.view.track_row_h,
        f.view,
        f.header_pane.x,
        f.header_pane.w,
        f.lanes,
        px,
        py,
        f.style,
    ) else {
        return;
    };
    if f.pointer.modifiers.alt {
        // Alt + click on point → 即時 DeleteAutomationPoints (commit-by-release なし)
        let v_k = vec![point_key];
        actions.delete_point = Some(Edit::mutate(move |app: &mut AppData| {
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
    } else if let Some((lane, clip_in)) = find_lane_clip(&f.visible_tracks, point_key.clip) {
        // 通常 click on point → drag session 起動 (release で MoveAutomationPoints)
        let p_idx = point_key.point_idx as usize;
        if let Some(p) = lane
            .clips
            .iter()
            .find(|c| c.id == point_key.clip.clip)
            .and_then(|c| c.points.get(p_idx))
            && let Some((_t_idx, _l_idx, _h_rect, body_rect)) = automation_lane_at(
                &f.visible_tracks,
                &f.tops,
                f.view.track_row_h,
                f.header_pane.x,
                f.header_pane.w,
                f.lanes.x,
                f.lanes.w,
                f.style,
                py,
            )
        {
            let beat_to_px = f64::from(f.lanes.w) / f.view.len_beats.max(1e-6);
            let pad = f.style.automation_clip_v_pad_px;
            let clip_y = body_rect.y + pad;
            let clip_h = (body_rect.h - pad * 2.0).max(2.0);
            #[allow(clippy::cast_possible_truncation)]
            let cx_clip =
                body_rect.x + ((clip_in.start_beat - f.view.start_beat) * beat_to_px) as f32;
            #[allow(clippy::cast_possible_truncation)]
            let cw = ((clip_in.len_beats * beat_to_px) as f32).max(2.0);
            let clip_rect_anchor = Rect { x: cx_clip, y: clip_y, w: cw, h: clip_h };
            let press_alt = f.pointer.modifiers.alt;
            let press_modifiers = f.pointer.modifiers;
            let session = AutomationPointDragSession {
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
            };
            let state: &mut ArrangementState = ui.widget_state(f.wid);
            state.automation_point_drag = Some(session);
            claim.point = true;
            claim.session = true;
        }
    }
}

/// M14 Phase 63n-3 (#028) / daw_01 #071: lane body 内 automation clip の press 振り分け。
/// priority: **point hit より低い** (= 上の point block で point drag / Alt+delete が起動済なら
/// skip)。 #071 で Shift / Ctrl 修飾でも起動する (= MIDI clip drag と完全対称、 release で短 click
/// を modifier 別 (plain=単一置換 / Shift・Ctrl=選択足し引き) に demote)。 automation lane では
/// marquee (`!press_in_automation_lane`) は走らないので Shift を温存する必要はない。 Alt のみ
/// lane resize に予約 (下の Alt+drag fallback)。 掴んだ clip が選択集合に含まれていれば選択中の
/// 全 clip を grabbed-first で `anchors` に積み一括 move / resize する (MIDI clip と同 idiom)。
///
/// M14 Phase 63n-6 (#031 follow-up): Alt 修飾は **lane Alt+drag for resize に予約** する
/// ため、 Alt+press on automation clip は session を起動しない。 これによって lane body 内の
/// 任意位置 (clip 上を含む) で Alt+drag → lane resize が動作する (= user expectation 1:1)。
/// 既存 automation clip Alt-snap-off 機能は失われるが、 automation 編集で sub-grid 位置を
/// 細かく調整する用途は稀で、 lane resize の優先度の方が高いと判断 (= user feedback 反映)。
/// MIDI / audio clip の Alt-snap-off (= clip_drag press) は **track row のみ** に作用するため
/// この変更の影響を受けない。
///
/// M14 Phase 63n-9 (#033): handle press (curve param drag) が先勝した場合 clip drag も skip。
fn automation_clip(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
) {
    let (px, py) = (hit.px, hit.py);
    if !claim.splitter
        && !claim.point
        && !claim.curve_handle
        && hit.in_lanes
        && !f.pointer.modifiers.alt
        && let Some((clip_key, kind, _clip_rect, _body_rect_anchor)) = automation_clip_zone_at(
            &f.visible_tracks,
            &f.tops,
            f.view.track_row_h,
            f.view,
            f.header_pane.x,
            f.header_pane.w,
            f.lanes,
            f.style,
            px,
            py,
            f.style.resize_handle_px,
        )
    {
        let press_alt = f.pointer.modifiers.alt;
        let press_ctrl = f.pointer.modifiers.ctrl;
        let press_shift = f.pointer.modifiers.shift;
        // #071: 掴んだ clip が選択集合に含まれていれば選択中の全 clip を一括 drag。 grabbed-first
        // 順 (snap pivot = anchors[0] = 掴んだ clip)。 MIDI clip の `selected_clips.contains(&hit)`
        // idiom を 1:1 ミラー。
        let mut keys: Vec<AutomationClipKey> = vec![clip_key];
        if f.selected_automation_clips.contains(&clip_key) {
            keys.extend(
                f.selected_automation_clips.iter().copied().filter(|k| *k != clip_key),
            );
        }
        let anchors = collect_automation_clip_anchors(
            &f.visible_tracks,
            &f.tops,
            f.view.track_row_h,
            f.header_pane.x,
            f.header_pane.w,
            f.lanes.x,
            f.lanes.w,
            f.style,
            &keys,
        );
        if !anchors.is_empty() {
            let state: &mut ArrangementState = ui.widget_state(f.wid);
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
            claim.session = true;
        }
    }
}

/// M14 Phase 63n-6 (#031): Alt+drag detection — splitter / 既存 press logic で session が
/// 起動しなかった場合のみ動作 (Alt+click on point / Alt+drag on clip 等は既に上で処理済 →
/// 該当 session が立っていれば skip する)。 lane body hit なら lane resize、 そうでなく
/// track row body hit なら row resize。 cursor が lanes 領域 (= clip 描画域) でも
/// header_pane (= lane label 列) でも動く — lane label 上 Alt+drag を「lane を伸ばす」 と
/// 期待する user 直感に合わせる (= 「lane の上で Alt+drag」 = lane resize)。
fn alt_resize(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
    actions: &PressActions,
) {
    let (px, py) = (hit.px, hit.py);
    let in_arr = hit.in_lanes || (f.header_w > 0.0 && f.header_pane.contains(px, py));
    if !(f.pointer.modifiers.alt && !hit.shift && !hit.ctrl && !claim.splitter && in_arr) {
        return;
    }
    if claim.session || actions.any() {
        return;
    }
    let lane_at = automation_lane_at(
        &f.visible_tracks,
        &f.tops,
        f.view.track_row_h,
        f.header_pane.x,
        f.header_pane.w,
        f.lanes.x,
        f.lanes.w,
        f.style,
        py,
    );
    if let Some((t_idx, l_idx, _h_rect, _b_rect)) = lane_at {
        let lane = &f.visible_tracks[t_idx].automation_lanes[l_idx];
        let lane_key = AutomationLaneKey { track: f.visible_tracks[t_idx].id, lane: lane.id };
        let anchor_h = lane.height_px;
        if anchor_h > 0 {
            let state: &mut ArrangementState = ui.widget_state(f.wid);
            state.automation_lane_resize_drag = Some(AutomationLaneResizeDragSession {
                lane: lane_key,
                anchor_height_px: anchor_h,
                anchor_mouse_y: py,
                last_mouse_y: py,
                last_emitted_height: anchor_h,
            });
            claim.session = true;
        }
    } else if let Some(t_idx) = track_index_from_y(py, f.lanes.y, &f.tops)
        && t_idx + 1 < f.tops.len()
    {
        // lane が無い (or collapsed) で track row body の中の Alt+drag → per-track row resize。
        // row body 範囲 = `[tops[t_idx], tops[t_idx] + effective_row_h(t))`、 それ以遠は
        // lane 領域 (= `lane_at` で既に拾われる前提) — y check は collapsed track / 末尾
        // track の「lane 無し領域」 まで含めて row body と認定するための明示判定。
        let t = &f.visible_tracks[t_idx];
        let row_top = f.tops[t_idx];
        let anchor_row_h = effective_track_row_h(t, f.view.track_row_h);
        let row_bottom = row_top + anchor_row_h;
        if py >= row_top && py < row_bottom && anchor_row_h > 0.0 {
            let track = t.id;
            let state: &mut ArrangementState = ui.widget_state(f.wid);
            state.track_row_resize_drag = Some(TrackRowResizeDragSession {
                track,
                anchor_row_h,
                anchor_mouse_y: py,
                last_mouse_y: py,
                last_emitted_height: anchor_row_h,
            });
            claim.session = true;
        }
    }
}

/// M14 Phase 63n-8 (#033): automation point の lasso press — **空き automation lane zone**
/// (= lane body && !clip && !point && !lane resize splitter) の drag で起動。 Q2=A の zone 排他
/// 設計: clip / point / splitter 上は既存 drag (move / move-points / resize) を最優先で起動済、
/// ここはそれら全てが起動しなかった場合の lane body fallback。 既存 MIDI clip rect_select は
/// automation lane 内では起動しない (= 後段の rect_select block で `!in_automation_lane` で
/// guard)、 automation lane では空き zone drag が **修飾なしで lasso** (= Shift / Ctrl は
/// release 時 next 計算で union / XOR 分岐)、 #033 Q2 回答 A と整合。 Alt は lane resize に
/// 予約済 (上の Alt+drag fallback で先勝) なので `!alt` で除外。
///
/// **`automation_lasso_drag` は 11 列挙外なので `claim` は立てない。**
///
/// 旧実装はここで `primary_just_pressed` / `pointer.pos` を再テストして `px`/`py` を
/// 同値 shadow していたが、 `dispatch` 冒頭と同一条件の再評価なので落とした。
fn lasso(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &PressClaim,
    actions: &PressActions,
) {
    let (px, py) = (hit.px, hit.py);
    if !(!f.pointer.modifiers.alt && !claim.splitter && hit.in_lanes) {
        return;
    }
    if claim.session || actions.any() {
        return;
    }
    let lane_at = automation_lane_at(
        &f.visible_tracks,
        &f.tops,
        f.view.track_row_h,
        f.header_pane.x,
        f.header_pane.w,
        f.lanes.x,
        f.lanes.w,
        f.style,
        py,
    );
    if let Some((_t_idx, _l_idx, _h_rect, body_rect)) = lane_at
        && px >= body_rect.x
        && px < body_rect.x + body_rect.w
    {
        // body x range 内 (= lane header 外)、 clip / point / splitter は上で先勝で
        // 既に session 起動 (claim.session で除外済) なので、 lane body の **真の空き zone**
        // で press したことが確定。 lasso session 起動。
        let start_modifiers = f.pointer.modifiers;
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.automation_lasso_drag =
            Some(AutomationLassoSession { anchor: (px, py), last_mouse: (px, py), start_modifiers });
    }
}
