//! drag 継続フェーズ: 生きている session の `last_*` 更新 → 端オートスクロール →
//! per-frame の live 値発火。 **この 3 つの順序は現行と同一に保つ** (端スクロールが
//! anchor を逆補正した結果を per-frame 発火が読む)。

use super::*;

pub(super) fn advance(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    update_sessions(ui, f);
    edge_autoscroll(ui, f);
    emit_lane_height(ui, f);
    emit_row_height(ui, f);
    emit_header_w(ui, f);
    emit_playhead(ui, f);
    emit_track_volume(ui, f);
}

/// release フレームで winit が pointer を press 位置へ巻き戻す現象への対処。
/// **判定軸は対象ごとに違うので引数で受ける** (現行の 4 通りをそのまま表現する。
/// 規則そのものの統一は「離した瞬間の最後のわずかな動きが反映されるか」 が対象ごとに
/// 変わる = ユーザーに見える挙動の変更なので、 別件として扱う)。
#[derive(Clone, Copy)]
pub(super) enum RewindAxes {
    /// タプル完全一致で「巻き戻っていない」 と判定する。
    BothExact { cur: (f32, f32), anchor: (f32, f32) },
    /// x のみ `f32::EPSILON` 比較。
    X { cur: f32, anchor: f32 },
    /// y のみ `f32::EPSILON` 比較。
    Y { cur: f32, anchor: f32 },
}

/// `last_*` を `cur` で更新してよいか。
/// - 継続フレーム (`!is_release`) は常に true。
/// - release フレームは pointer が anchor から動いている (= 巻き戻っていない) ときだけ true。
pub(super) fn accept_release_pos(is_release: bool, axes: RewindAxes) -> bool {
    if !is_release {
        return true;
    }
    match axes {
        RewindAxes::BothExact { cur, anchor } => cur != anchor,
        RewindAxes::X { cur, anchor } | RewindAxes::Y { cur, anchor } => {
            (cur - anchor).abs() > f32::EPSILON
        }
    }
}

/// 14 個の session の `last_mouse` / `last_alt` / `last_ctrl` / `last_shift` を更新する。
///
/// drag 中なら continuation frame で `last_mouse` / `last_alt` (および各 drag の last_*) を
/// update。 **release frame の `last_alt` は update しない** — 同 frame に
/// ModifiersChanged(alt=false) が先行する現象 (alt が一瞬 false に化ける) を回避するため、
/// release 直前 frame の値を保持する。 **release frame の `last_mouse` は pointer.pos が
/// anchor と異なる場合のみ update** — winit は release frame で `pointer.pos` を press 位置
/// に戻すことがあり、 そのまま上書きすると delta = 0 で commit not pushed (drag が「元に戻る」
/// ように見える)。 その判定 (`accept_release_pos`) の **軸は対象ごとに 4 通りに割れている**
/// ので、 `RewindAxes` で受ける。 規則自体は 1 つも変えていない。
///
/// `ui.widget_state(f.wid)` を 1 度だけ取り、 その `&mut ArrangementState` で 14 個を順に
/// 処理する。 **`push_edit` をこのブロック内で呼ばない** (borrow が閉じていない)。
#[allow(clippy::too_many_lines)]
fn update_sessions(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let Some((px, py)) = f.pointer.pos else { return };
    let alt_now = f.pointer.modifiers.alt;
    let ctrl_now = f.pointer.modifiers.ctrl;
    let shift_now = f.pointer.modifiers.shift;
    let is_release = f.pointer.primary_just_released;
    let sticky_threshold = f.style.audio_fade_sticky_threshold_px;
    let state: &mut ArrangementState = ui.widget_state(f.wid);
    if let Some(ref mut nd) = state.clip_drag {
        if accept_release_pos(
            is_release,
            RewindAxes::BothExact { cur: (px, py), anchor: nd.anchor_mouse },
        ) {
            nd.last_mouse = (px, py);
        }
        if !is_release {
            nd.last_alt = alt_now;
            // M14 Phase 63e (#019): ctrl / shift も同じ仕組みで update。 release frame は
            // ModifiersChanged が MouseInput より先に届いて false 化するリスクがあるので skip。
            nd.last_ctrl = ctrl_now;
            nd.last_shift = shift_now;
        }
    }
    // M14 Phase 127 (daw_01 #105): section drag continuation。 clip_drag と同じく continuation で
    // last_mouse / last_alt / last_ctrl を update、 release frame は巻き戻し検知時のみ update。
    if let Some(ref mut sd) = state.section_drag {
        if accept_release_pos(is_release, RewindAxes::X { cur: px, anchor: sd.anchor_mouse.0 }) {
            sd.last_mouse = (px, py);
        }
        if !is_release {
            sd.last_alt = alt_now;
            sd.last_ctrl = ctrl_now;
            sd.last_shift = shift_now;
        }
    }
    if let Some(ref mut ld) = state.loop_drag {
        // release frame で pointer.pos が press 位置と異なる = winit が press 位置に
        // 巻き戻していない → 真値として update (clip_drag と同 pattern)。
        if accept_release_pos(is_release, RewindAxes::X { cur: px, anchor: ld.anchor_mouse_x }) {
            ld.last_mouse_x = px;
        }
        if !is_release {
            // M14 Phase 63j (#024): last_alt は continuation で update、 release は
            // skip (clip_drag と同じ pattern、 OS event 順序による false 化 race を回避)。
            ld.last_alt = alt_now;
        }
    }
    if let Some(ref mut tr) = state.track_reorder {
        // continuation は常に update。 release 時は winit 巻き戻し検知のため
        // anchor と differ する場合のみ update (clip_drag と同 pattern)。
        // M14 Phase 101 (daw_01 #072): y / x を独立に判定して update (片軸だけ巻き戻る
        // ケースでも他軸の真値を保持)。
        if accept_release_pos(is_release, RewindAxes::Y { cur: py, anchor: tr.anchor_mouse_y }) {
            tr.last_mouse_y = py;
        }
        if accept_release_pos(is_release, RewindAxes::X { cur: px, anchor: tr.anchor_mouse_x }) {
            tr.last_mouse_x = px;
        }
    }
    if let Some(ref mut tv) = state.track_volume_drag
        && accept_release_pos(is_release, RewindAxes::X { cur: px, anchor: tv.anchor_mouse_x })
    {
        tv.last_mouse_x = px;
    }
    // M14 Phase 63j (#024): playhead_drag continuation で last_mouse_x を track。
    // release frame は session を後段で `take()` するため update 不要
    // (= **巻き戻し判定を通さない**。 通すと挙動が変わる)。
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
    //   **巻き戻し判定の外** (session が生きていれば毎フレーム走る)。
    if let Some(ref mut ad) = state.audio_drag {
        if accept_release_pos(
            is_release,
            RewindAxes::BothExact { cur: (px, py), anchor: ad.anchor_mouse },
        ) {
            ad.last_mouse = (px, py);
        }
        if ad.locked_horizontal.is_none() {
            let dx = (ad.last_mouse.0 - ad.anchor_mouse.0).abs();
            let dy = (ad.last_mouse.1 - ad.anchor_mouse.1).abs();
            if dx >= sticky_threshold || dy >= sticky_threshold {
                ad.locked_horizontal = Some(dx >= dy);
            }
        }
    }
    // M14 Phase 63n-2 (#028): automation_point_drag continuation で last_mouse + last_alt を update。
    // release frame は last_mouse は pointer.pos != anchor_mouse のときのみ update (clip_drag と
    // 同 pattern: winit が release で press 位置に戻すケースを回避)、 last_alt は release では
    // 保持 (ModifiersChanged が MouseInput より先に届く race を回避)。
    if let Some(ref mut ad) = state.automation_point_drag {
        if accept_release_pos(
            is_release,
            RewindAxes::BothExact { cur: (px, py), anchor: ad.anchor_mouse },
        ) {
            ad.last_mouse = (px, py);
        }
        if !is_release {
            ad.last_alt = alt_now;
        }
    }
    // M14 Phase 63n-5 (#030): automation_lane_resize_drag continuation で last_mouse_y を update
    // (release frame は release block で処理 = **巻き戻し判定なし**)。
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
        if accept_release_pos(
            is_release,
            RewindAxes::BothExact { cur: (px, py), anchor: acd.anchor_mouse },
        ) {
            acd.last_mouse = (px, py);
        }
        if !is_release {
            acd.last_alt = alt_now;
            acd.last_ctrl = ctrl_now;
            acd.last_shift = shift_now;
        }
    }
    // M14 Phase 63n-8 (#033): automation_lasso_drag continuation で last_mouse を update。
    // `start_modifiers` は press 時固定 (= 「lasso 開始時に Shift だったが drag 中に離した」 でも
    // union 動作、 既存 `DragRectState.start_modifiers` と同 idiom)。 release frame の last_mouse
    // は release pos が anchor と異なる場合のみ update (clip_drag と同 pattern)。
    if let Some(ref mut ls) = state.automation_lasso_drag
        && accept_release_pos(
            is_release,
            RewindAxes::BothExact { cur: (px, py), anchor: ls.anchor },
        )
    {
        ls.last_mouse = (px, py);
    }
    // r.md #73: 区間 bend の continuation。 release frame は last_mouse_y を anchor と
    // 異なるときだけ更新する (既存 OS event 順序 race 回避 pattern)。
    // `preview_curve` は毎 frame **逆算** で作り直す (= live preview の SSoT、 release で
    // final 値として使う。 **巻き戻し判定の外**)。 解けない frame は直前値を維持する。
    //
    // 感度定数は使わない — 「掴んだ場所が指に付いてくる」を成立させるため、 指の移動 px
    // ぶんだけ動かした目標値から curve を解く。 逆算は区間の符号付き高さ `(b - a)` で割るので
    // 上り / 下りの符号は構造的に正しくなる (定数 `dir` を掛ける小細工は不要)。
    if let Some(ref mut bd) = state.automation_segment_bend {
        if accept_release_pos(is_release, RewindAxes::Y { cur: py, anchor: bd.anchor_mouse_y }) {
            bd.last_mouse_y = py;
        }
        // **まだ 1px も動いていないフレームでは preview を触らない。**
        // 触ると Alt+クリックしただけ (= 動かしていない) で curve が書き換わる:
        // Hold / Linear 区間の `start_curve` は `Exponential { bend: 0.0 }` なので、
        // dy = 0 で解いても `Exponential { bend: 0.0 }` が返り、 `anchor_curve`
        // (= Linear) と異なるため release の no-op 判定をすり抜けてしまう。
        // 既に Exponential / Bezier の区間でも、 逆算の丸めで最下位ビットがずれて
        // 同じことが起きる。 「最初の 1px で直線化してから曲がる」 は仕様どおり。
        let moved = (bd.last_mouse_y - bd.anchor_mouse_y).abs() > f32::EPSILON;
        // lane は毎 frame 引き直す (session に target を持たせない = `Copy` を保つ)。
        if moved
            && let Some((lane, _clip)) = find_lane_clip(&f.visible_tracks, bd.point.clip)
        {
            let map = curve::LaneValueMap::from_lane(lane, bd.clip_rect_anchor);
            let dy = bd.last_mouse_y - bd.anchor_mouse_y;
            let target_norm =
                (bd.anchor_value_norm - dy / bd.clip_rect_anchor.h.max(1.0)).clamp(0.0, 1.0);
            if let Some(next) = curve::solve_bend(
                map,
                bd.a_plain,
                bd.b_plain,
                bd.grab_u,
                bd.start_curve,
                target_norm,
            ) {
                bd.preview_curve = next;
            }
        }
    }
}

/// ドラッグ端オートスクロール。
///
/// drag 中、pointer が lanes 端の hot-zone に入ったら view を自動スクロールし、掴んでいる対象が
/// カーソルに追従し続ける (実 DAW 標準)。横 (beat) と縦 (track_top) の両軸。relative-delta で
/// 位置を決める session (clip / section / automation point/clip / lasso / clip marquee) は実
/// スクロール px ぶん anchor を逆方向に shift して追従させる (= content space delta)。track 並べ
/// 替え (live 行 top 再解決) と ruler の loop/playhead (絶対 px→beat 再解決) は anchor shift 不要。
/// カーソルを端で止めたままでもスクロール継続するよう `request_redraw` で次フレームを確保する。
///
/// `state.edge_scroll_press` はこのブロックが唯一の所有者。
fn edge_autoscroll(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let pointer = f.pointer;
    if !pointer.primary_pressed || pointer.primary_just_released {
        return;
    }
    // 移動量ゲート: press からの移動が ACTIVATE_PX 以上のときのみ端スクロールを許可
    // (click-and-hold で view が飛ぶのを防ぐ)。press frame で press 位置を記録。
    let moved_enough = {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        if pointer.primary_just_pressed {
            state.edge_scroll_press = pointer.pos;
        }
        let gate = daw_ui_core::widgets::edge_scroll::ACTIVATE_PX;
        matches!((state.edge_scroll_press, pointer.pos),
            (Some(p), Some(c)) if (c.0 - p.0).powi(2) + (c.1 - p.1).powi(2) >= gate * gate)
    };
    let axes = if moved_enough {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        arrangement_edge_scroll_axes(state)
    } else {
        None
    };
    let drag_rect_wid = f.wid.child(b"rect_select");
    let marquee_active = moved_enough && axes.is_none() && {
        let st: &mut daw_ui_core::widgets::drag_rect::DragRectState =
            ui.widget_state(drag_rect_wid);
        st.drag_start.is_some()
    };
    // clip marquee (空き lanes の rect-select) は両軸。
    let Some((ax, ay)) = axes.or_else(|| marquee_active.then_some((true, true))) else {
        return;
    };
    let cfg = daw_ui_core::widgets::edge_scroll::EdgeScrollCfg::default();
    let (dx, dy) = daw_ui_core::widgets::edge_scroll::edge_scroll_delta(
        pointer.pos,
        f.lanes,
        cfg,
        ax,
        ay,
    );
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    // 実際に適用された scroll 量 (px) を求め、その分だけ anchor を逆 shift する。
    let mut applied_beat_px = 0.0_f32;
    let mut applied_track_px = 0.0_f32;
    if dx != 0.0 && f.beat_per_px > 1e-6 {
        // r.md #53: 端自動スクロールは 1 frame あたり 0〜18px の連続量なので、
        // スナップ済の表示原点に足すと zone 入口 (< 0.5px/frame) で端数が
        // 毎フレーム捨てられて一切進まなくなる。 基準は連続値のモデル側。
        let new_start = (f.view.scroll_beat_raw + f64::from(dx) * f.beat_per_px).max(0.0);
        #[allow(clippy::cast_possible_truncation)]
        let adx = ((new_start - f.view.scroll_beat_raw) / f.beat_per_px) as f32;
        if adx != 0.0 {
            applied_beat_px = adx;
            ui.push_edit({
                let v_b = new_start;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetArrangeScroll(v_b as f32));
                })
            });
        }
    }
    if dy != 0.0 {
        // 縦 scroll は既存 SetTrackTop と同じく下限 0 のみ (上限 clamp は handler 非対象、
        // wheel 挙動と互換)。
        let new_top = (f.view.track_top + dy).max(0.0);
        let ady = new_top - f.view.track_top;
        if ady != 0.0 {
            applied_track_px = ady;
            ui.push_edit({
                let v_t = new_top;
                Edit::mutate(move |app: &mut AppData| {
                    app.ui_prefs.arrange_track_top = v_t.max(0.0);
                })
            });
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
            let st: &mut ArrangementState = ui.widget_state(f.wid);
            arrangement_compensate_anchor(st, applied_beat_px, applied_track_px);
        }
        ui.request_redraw();
    }
}

// default value の per-frame 編集は caller の scrubable_number_at overlay が担う
// (旧 band drag の per-frame SetLaneDefault emit は廃止)。

/// M14 Phase 63n-5 (#030): automation_lane_resize_drag の per-frame live update。
/// drag 中は user に「lane が伸び縮みする様子」 を見せたいので、 height 変化を毎 frame 発行する。
/// release frame は release block で最終値を発行するためここでは skip。
fn emit_lane_height(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let Some((_px, py)) = f.pointer.pos else { return };
    if f.pointer.primary_just_released {
        return;
    }
    // M14 Phase 63n-6 (#031): max は `min(style.max, lanes.h)` で runtime clamp。
    // style 値は絶対 cap、 lanes.h は描画 pane の現在縦サイズ (= 「画面いっぱい」)。
    let max_h = effective_lane_max_height(f.style, f.lanes);
    let min_h = f.style.automation_lane_min_height_px;
    let mut emit: Option<(AutomationLaneKey, u16, u16)> = None;
    {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
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
        ui.push_edit({
            let v_lane = lane;
            let v_prev = prev;
            let v_next = next;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLaneHeight {
                    track_id: v_lane.track,
                    lane_id: v_lane.lane,
                    prev_px: v_prev,
                    next_px: v_next,
                });
            })
        });
    }
}

/// M14 Phase 63n-6 (#031): track_row_resize_drag の per-frame live update。
/// drag 中は **対象 track の `t.row_h`** が変わる度に caller が `SetSingleTrackRowH` を mutate
/// する (= per-track override 化、 Bitwig per-track zoom と同 idiom)。 widget は floor 1 px の
/// u16 で発火 (caller-side で `[min, max]` clamp)、 同値抑制 0.5 px 閾値で u16 quantization 込み。
fn emit_row_height(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let Some((_px, py)) = f.pointer.pos else { return };
    if f.pointer.primary_just_released {
        return;
    }
    let mut row_emit: Option<(u32, u16, u16)> = None;
    {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        if let Some(ref mut rd) = state.track_row_resize_drag {
            let dy = py - rd.anchor_mouse_y;
            let next_f = (rd.anchor_row_h + dy).max(1.0);
            if (next_f - rd.last_emitted_height).abs() >= 0.5 {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let next = next_f.round().clamp(1.0, f32::from(u16::MAX)) as u16;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let prev = rd.anchor_row_h.round().clamp(1.0, f32::from(u16::MAX)) as u16;
                row_emit = Some((rd.track, prev, next));
                rd.last_emitted_height = next_f;
            }
        }
    }
    if let Some((track, prev, next)) = row_emit {
        ui.push_edit({
            let v_track = track;
            let v_prev = prev;
            let v_next = next;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSingleTrackRowH {
                    track_id: v_track,
                    prev_px: v_prev,
                    next_px: v_next,
                });
            })
        });
    }
}

/// M14 Phase 117 (daw_01 #091): header_resize_drag の per-frame live update。 drag 中は
/// header 幅変化を毎 frame `SetHeaderW { prev: anchor, next }` で発行する (caller が
/// `view.header_w` を更新 → 次 frame に header / lanes が連動伸縮)。 `next` は raw px
/// (NaN/負値防止の `max(0.0)` floor のみ、 実用 clamp は caller)、 同値抑制 0.5 px。
fn emit_header_w(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let Some((px, _py)) = f.pointer.pos else { return };
    if f.pointer.primary_just_released {
        return;
    }
    let mut header_emit: Option<(f32, f32)> = None;
    {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
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
        ui.push_edit({
            let v_next = next;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetArrangeHeaderW(v_next));
            })
        });
    }
}

/// M14 Phase 63j (#024): playhead drag continuation の per-frame live update。
/// press frame は press block 内で発火済 (`actions.seek_beat`)、 ここは continuation のみ。
/// release frame は emit せず session を `sessions::take` が take して discard する
/// (commit-by-release 無し)。 `last_emitted_beat` で同値発火を抑制
/// (1e-6 拍 = ~10μs @ 120BPM 以下は ignore)。
fn emit_playhead(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let Some((px, _py)) = f.pointer.pos else { return };
    if f.pointer.primary_just_pressed || f.pointer.primary_just_released {
        return;
    }
    let alt = f.pointer.modifiers.alt;
    let mut emit_beat: Option<f64> = None;
    {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        if let Some(ref mut pd) = state.playhead_drag {
            let raw = px_to_beat(px, f.ruler.x, f.ruler.w, f.view);
            let next = f.view.snap.snap_beat(raw, alt, f.zoom_x_px_per_beat).max(0.0);
            if (next - pd.last_emitted_beat).abs() > 1e-6 {
                emit_beat = Some(next);
                pd.last_emitted_beat = next;
            }
        }
    }
    if let Some(beat) = emit_beat {
        ui.push_edit({
            let v_beat = beat;
            Edit::mutate(move |app: &mut AppData| {
                app.seek_playhead_to(v_beat);
            })
        });
    }
}

/// M10 Phase 49: track volume drag 中の per-frame live update。
/// release frame は Mutate 発火を抑制し、release ブロックの Undoable Edit に任せる
/// (= fader_at の `suppress_mutate_on_release` と同パターン)。
/// 同値発火を抑えるため `last_emitted_volume` と差分比較。
fn emit_track_volume(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let Some((px, _py)) = f.pointer.pos else { return };
    if f.pointer.primary_just_released {
        return;
    }
    let mut volume_emit: Option<(u32, f32, f32)> = None;
    {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        if let Some(ref mut tv) = state.track_volume_drag {
            let next = volume_from_mouse_x(px, tv.band_rect.x, tv.band_rect.w);
            if (next - tv.last_emitted_volume).abs() > 1e-4 {
                volume_emit = Some((tv.track_id, tv.anchor_volume, next));
                tv.last_emitted_volume = next;
            }
        }
    }
    if let Some((track, _prev, next)) = volume_emit {
        ui.push_edit({
            let v_track = track;
            let v_next = next;
            Edit::mutate(move |app: &mut AppData| {
                let amp = MeterScale::default().frac_to_amp(v_next.clamp(0.0, 1.0));
                app.handle_event(AppEvent::SetTrackVolume { track: v_track, amp });
            })
        });
    }
}
