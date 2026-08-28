//! lane 領域 (clip 描画域 = `f.lanes`) の press 振り分け。 ゾーンごとに排他なチェーンで、
//! 優先順位は audio grip > clip > automation point > **区間 bend** > automation clip > lasso。
//! 各分岐は消費したことを `PressClaim` に立てて後続を止める。
//!
//! r.md #73: 旧 `curve_handle` (選択済 point の中央ハンドル) と `alt_resize`
//! (Alt+drag = レーン / 行の高さ変更) の 2 本を撤去した。 前者はハンドルが原理的に
//! 動かない (Bezier の t=0.5 は tension に依らず常に中点) 実装で、 後者が Alt を
//! 占有していたせいで「線を直接掴んで曲げる」を置く場所が無かった。 高さ変更は
//! Alt+ホイール (`release.rs`、 ヘッダ列でも効く) と下端スプリッタ (`press::splitter`)
//! が引き続き担う。

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

/// automation 系 4 本 + lasso。
/// 内部で `point` → `segment_bend` → `automation_clip` → `lasso` を
/// **この順**で呼ぶ (優先順位そのもの)。
pub(super) fn automation(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
    actions: &mut PressActions,
) {
    point(ui, f, hit, claim, actions);
    segment_bend(ui, f, hit, claim);
    automation_clip(ui, f, hit, claim);
    lasso(ui, f, hit, claim, actions);
}

/// M14 Phase 63n-8 (#033): point press は **Shift / Ctrl 修飾も accept** (release 時 短 click
/// 化で toggle / replace を判定する)。 旧 Phase 63n-2 は `!shift && !ctrl` で除外していたが、
/// それだと Shift+click on point が何の session も起動せず toggle が発火しない bug を持っていた。
/// Shift+click on point は drag>=4px なら通常 move (= MoveAutomationPoints、 modifier 無視で
/// pressed が selection に含まれていれば multi)、 短 click なら toggle。 Ctrl 同様。
///
/// r.md #73: **点に当たった時点で `claim.point` を立てる** (旧実装は drag session を
/// 起動したときだけ立てていた)。 Alt+クリック (削除) は drag session を張らないので、
/// 旧述語では「点の削除」と後続の press (区間 bend / automation clip) が同フレームで
/// 両方走ってしまう。 seed は据え置きで立てる条件だけを広げるので、 ゲートは単調に
/// 強くなる方向にしか動かない (r.md #77 の等価性の根拠を壊さない)。
fn point(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
    actions: &mut PressActions,
) {
    let (px, py) = (hit.px, hit.py);
    if claim.splitter || !hit.in_lanes {
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
    // 当たった時点で「point 層がこの押下を消費した」。 以降の分岐 (区間 bend /
    // automation clip / lasso) はこれを読む。
    claim.point = true;
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

/// r.md #73: Alt + レーン本体の線 → 区間の曲げ (bend) session。
///
/// 優先順位は point (半径 2 倍) の **後**、automation clip の **前**。 point が先に効くので
/// Alt+クリック (点の削除) と共存する。 Hold / Linear の区間は「曲線」(= `Exponential`)
/// へ自動変換してから量を付ける。 commit は release で 1 回だけ (undo 1 段)。
///
/// `preview_curve` の初期値は **`anchor_curve`** (= press 直後は今の見た目のまま)。
/// 最初の continuation で `start_curve` を基準に解いた結果へ切り替わる
/// (= Hold 区間は最初の 1px 動かした瞬間に直線化してから曲がる。 連続解が無いので仕様)。
fn segment_bend(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
) {
    let (px, py) = (hit.px, hit.py);
    if !(!claim.splitter
        && !claim.point
        && !claim.session
        && hit.in_lanes
        && f.pointer.modifiers.alt
        && !hit.shift
        && !hit.ctrl)
    {
        return;
    }
    let Some(seg) = automation_segment_at(
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
    let start_curve = match seg.curve {
        common::model::AutomationCurve::Hold | common::model::AutomationCurve::Linear => {
            common::model::AutomationCurve::Exponential { bend: 0.0 }
        }
        other => other,
    };
    // lane が引けなければ session を起動しない。 `automation_segment_at` が当てた lane と
    // 同じものなので実際には必ず引けるが、 **握りつぶして 0.0 を anchor にしない** —
    // それをやると最初の 1px で線が窓の下端へ飛ぶ。
    let Some((lane, _clip)) = find_lane_clip(&f.visible_tracks, seg.point.clip) else {
        return;
    };
    let map = curve::LaneValueMap::from_lane(lane, seg.clip_rect);
    let anchor_value_norm =
        curve::eval_norm(map, seg.a_plain, seg.b_plain, seg.grab_u, start_curve);
    let state: &mut ArrangementState = ui.widget_state(f.wid);
    state.automation_segment_bend = Some(AutomationSegmentBendSession {
        point: seg.point,
        grab_u: seg.grab_u,
        a_plain: seg.a_plain,
        b_plain: seg.b_plain,
        anchor_curve: seg.curve,
        start_curve,
        anchor_value_norm,
        clip_rect_anchor: seg.clip_rect,
        anchor_mouse_y: py,
        last_mouse_y: py,
        preview_curve: seg.curve,
    });
    claim.session = true;
}

/// M14 Phase 63n-3 (#028) / daw_01 #071: lane body 内 automation clip の press 振り分け。
/// priority: **point hit / 区間 bend より低い** (= 上の 2 ブロックが消費していたら skip)。
/// #071 で Shift / Ctrl 修飾でも起動する (= MIDI clip drag と完全対称、 release で短 click
/// を modifier 別 (plain=単一置換 / Shift・Ctrl=選択足し引き) に demote)。 automation lane では
/// marquee (`!press_in_automation_lane`) は走らないので Shift を温存する必要はない。
/// 掴んだ clip が選択集合に含まれていれば選択中の全 clip を grabbed-first で `anchors` に
/// 積み一括 move / resize する (MIDI clip と同 idiom)。
///
/// r.md #73: **`!alt` ゲートを外した。** 旧実装は「Alt 修飾は lane Alt+drag for resize に
/// 予約する」ために Alt+press を弾いており、 その代償として automation clip の
/// Alt-snap-off を失っていた。 #73 で Alt+drag resize を撤去したので予約の根拠が消え、
/// 残すと「Alt を押してドラッグすると何も起きない」死角が新しく生まれる。 外したことで
/// **Alt = スナップ無効が復活し、 MIDI / audio clip と対称になる**。 線の上の Alt+drag は
/// 1 段上の `segment_bend` が先勝する (`!claim.session`)。
fn automation_clip(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
) {
    let (px, py) = (hit.px, hit.py);
    if !claim.splitter
        && !claim.point
        && !claim.session
        && hit.in_lanes
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

/// M14 Phase 63n-8 (#033): automation point の lasso press — **空き automation lane zone**
/// (= lane body && !clip && !point && !lane resize splitter) の drag で起動。 Q2=A の zone 排他
/// 設計: clip / point / splitter 上は既存 drag (move / move-points / resize) を最優先で起動済、
/// ここはそれら全てが起動しなかった場合の lane body fallback。 既存 MIDI clip rect_select は
/// automation lane 内では起動しない (= 後段の rect_select block で `!in_automation_lane` で
/// guard)、 automation lane では空き zone drag が **修飾なしで lasso** (= Shift / Ctrl は
/// release 時 next 計算で union / XOR 分岐)、 #033 Q2 回答 A と整合。
///
/// r.md #73: **`!alt` ゲートを外した。** 旧コメントは「Alt は lane resize に予約済
/// (上の Alt+drag fallback で先勝) なので `!alt` で除外」と書いていたが、 その
/// Alt+drag resize を撤去したので予約が無効になった。 残すと空き lane zone の
/// Alt+drag が何も起こさない死角になる。 lasso に snap の概念は無いので Alt に別の
/// 意味は無く、 線の上の Alt+drag は `segment_bend` が先に `claim.session` を立てて
/// ここへ来ない (`no_session` 相当のゲートは下の `claim.session` チェック)。
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
    if !(!claim.splitter && !claim.point && hit.in_lanes) {
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
