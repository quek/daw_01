//! caller (`arrangement_view.rs`) が context menu / overlay の anchor に使う rect 群を
//! `ArrangementResponse` に積む。 積む順序は **描画順 (上から下、 左から右)** で、
//! caller はこの順序に依存している (`ArrangementResponse::clip_rects` の doc 参照)。
//!
//! M14 Phase 63n-2 (#028): 右クリック on point の context menu は **caller 責務**。
//! widget は `response.automation_point_rects: Vec<(AutomationPointKey, Rect)>` を毎 frame
//! 返し (clip_rects と同 idiom)、 caller は loop で `context_menu_for(*rect, &["Hold",
//! "Linear", "Bezier"], ...)` を呼ぶ。 widget 内で secondary press を消費する旧設計は popup の
//! anchor_rect が **右クリック frame だけ Some** で次 frame 以降 caller が context_menu_for を
//! 呼ばないため popup state が消える bug を持っていた (= 一瞬で popup が閉じる)。 #028 §11.4
//! で確定した「caller が anchor を毎 frame 呼ぶ」 idiom に統一。
//! (この段落は旧 `run.rs` で press フェーズと rect 収集フェーズの間に浮いていたが、
//!  説明しているのはこのモジュールの存在理由そのものなのでここへ移した。)

use super::*;

/// caller が **当たり判定 / メニューのアンカー**に使う rect を、それが乗っている面で
/// クリップする。面から出た部分が残ると `w <= 1.0` で捨てる。
///
/// **描画は scissor で切られるのに返す rect は切られない、を作らないための 1 本。**
/// r.md #87 でヘッダとレーンの間にランチャー帯が挟まったので、ビューの左外へ伸びた
/// クリップ / 点の rect は**帯やヘッダの上まで届く** — そこを右クリックすると、見えて
/// いないアレンジのクリップのメニューが、帯のセルのメニューと同時に開く
/// (`take_secondary_click_in_rect` は consume しない)。
///
/// **レイアウトのミラー (`automation_lane_rects` / `response.rows`) には使わない** —
/// あれは「行が実際にどこにあるか」を返す値で、切ると縦ズームの写像が狂う。
#[must_use]
fn hit_rect(r: Rect, surface: Rect) -> Option<Rect> {
    let hit = r.intersect(surface);
    (hit.w > 1.0 && hit.h > 1.0).then_some(hit)
}

pub(super) fn collect(
    f: &ArrangementFrame<'_>,
    live: &LiveSessions,
    response: &mut ArrangementResponse,
) {
    push_clip_rects(f, response);
    push_lane_default_rects(f, response);
    push_point_drag_live(f, live, response);
    push_point_rects(f, response);
    push_automation_clip_and_lane_rects(f, response);
    // M14 Phase 63n-3 (#028): drag 中の automation clip kind を response に反映 (cursor /
    // status indicator 用)。 既存 `dragging` (MIDI clip 用) と直交。
    // **rect 収集の後、 このフェーズの最後の文**という位置を動かさないこと
    // (`response` への書き込み順を旧実装と 1:1 に保つ)。
    response.dragging_automation_clip = live.automation_clip_drag.as_ref().map(|acd| acd.kind);
}

/// M14 Phase 63f (#020): clip_rects を visible-tracks 順 (= 描画順) で積む。
/// draw_clips と同じ culling: row が lanes 外 / clip が view beat 範囲外なら除外。
/// 部分的に見えている clip は **lanes で切った可視部分**を返す ([`hit_rect`]) —
/// full rect のままだと、ビューの左外へ伸びたクリップの当たり判定がランチャー帯や
/// トラックヘッダの上まで届く。caller の `context_menu_for` は可視部分をアンカーに
/// すれば十分で、はみ出しは `popup_rect_clamped_at` が吸収する。
fn push_clip_rects(f: &ArrangementFrame<'_>, response: &mut ArrangementResponse) {
    let view_end = f.view.start_beat + f.view.len_beats;
    for (i, t) in f.visible_tracks.iter().enumerate() {
        let row_top = f.tops[i];
        let row_h = effective_track_row_h(t, f.view.track_row_h);
        if row_top + row_h < f.lanes.y || row_top > f.lanes.y + f.lanes.h {
            continue;
        }
        for c in &t.clips {
            let end = c.start_beat + c.len_beats;
            if end < f.view.start_beat || c.start_beat > view_end {
                continue;
            }
            let r = clip_to_rect(row_top, row_h, c, f.view, f.lanes);
            let Some(hit) = hit_rect(r, f.lanes) else {
                continue;
            };
            response.clip_rects.push((ClipKey { track_id: t.id, clip_id: c.id }, hit));
        }
    }
}

/// 各 visible lane header の default value 数値入力フィールド rect (= caller が
/// scrubable_number_at を overlay する位置)。 master row (synthetic track) の lane も
/// `visible_tracks[t_idx].id == MASTER_TRACK_ID` で含まれる。 行高不足で field rect が
/// 無い (= layout.default_field_rect == None) lane は除外。
fn push_lane_default_rects(f: &ArrangementFrame<'_>, response: &mut ArrangementResponse) {
    for_each_visible_lane(
        &f.visible_tracks,
        &f.tops,
        f.view.track_row_h,
        f.header_pane.x,
        f.header_pane.w,
        f.lanes.x,
        f.lanes.w,
        f.style,
        |t_idx, _l_idx, lane, h_rect, _body_rect| {
            if h_rect.y + h_rect.h < f.lanes.y || h_rect.y > f.lanes.y + f.lanes.h {
                return;
            }
            // **ここは `hit_rect` を通さない。** caller は数値入力欄を「この rect に
            // 描く」ので、切ると欄そのものが潰れる (当たり判定だけの rect とは別の口)。
            if let Some(layout) = automation_lane_header_layout(h_rect, f.style)
                && let Some(field) = layout.default_field_rect
            {
                let key =
                    AutomationLaneKey { track: f.visible_tracks[t_idx].id, lane: lane.id };
                response.automation_lane_default_rects.push((key, field));
            }
        },
    );
}

/// point drag 中の live 値を response に乗せる。
/// overlay ghost (cached 外描画) と同じ式で next_value / cursor を算出し、 caller が
/// カーソル近傍に現値を人間可読単位で表示できるようにする。 release frame は session が
/// take 済 (None) になるので、 ここは drag 継続中のみ Some。
fn push_point_drag_live(
    f: &ArrangementFrame<'_>,
    live: &LiveSessions,
    response: &mut ArrangementResponse,
) {
    if !f.pointer.primary_just_released
        && let Some(pd) = live.point_drag
    {
        let dx = pd.last_mouse.0 - pd.anchor_mouse.0;
        let dy = pd.last_mouse.1 - pd.anchor_mouse.1;
        let beat_to_px = f64::from(pd.body_rect_anchor.w) / f.view.len_beats.max(1e-6);
        let raw_dt = f64::from(dx) / beat_to_px;
        let raw_abs = pd.clip_start_beat + pd.anchor_time_beat + raw_dt;
        let snapped_abs = f.view.snap.snap_beat(raw_abs, pd.last_alt, f.zoom_x_px_per_beat);
        let next_local =
            (snapped_abs - pd.clip_start_beat).clamp(0.0, pd.clip_len_beats.max(0.0));
        let next_value =
            (pd.anchor_value_norm - dy / pd.clip_rect_anchor.h.max(1.0)).clamp(0.0, 1.0);
        let abs_beat = pd.clip_start_beat + next_local;
        #[allow(clippy::cast_possible_truncation)]
        let px = pd.body_rect_anchor.x + ((abs_beat - f.view.start_beat) * beat_to_px) as f32;
        let py = pd.clip_rect_anchor.y + (1.0 - next_value) * pd.clip_rect_anchor.h;
        response.automation_point_drag = Some(AutomationPointDragInfo {
            key: pd.point,
            value_norm: next_value,
            cursor: (px, py),
        });
    }
}

/// M14 Phase 63n-2 (#028): automation_point_rects を毎 frame 積む。
/// for_each_visible_lane で SSoT を共有し、 各 visible point を screen 座標に変換した
/// 半径 8px 正方形 rect を返す (= caller の context_menu_for で右クリック anchor として使う)。
/// collapsed group 内 / collapsed lane / invisible lane / view beat 範囲外の point は除外。
fn push_point_rects(f: &ArrangementFrame<'_>, response: &mut ArrangementResponse) {
    let view_end = f.view.start_beat + f.view.len_beats;
    let radius = f.style.automation_point_radius_px.max(2.0);
    for_each_visible_lane(
        &f.visible_tracks,
        &f.tops,
        f.view.track_row_h,
        f.header_pane.x,
        f.header_pane.w,
        f.lanes.x,
        f.lanes.w,
        f.style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            if body_rect.y + body_rect.h < f.lanes.y || body_rect.y > f.lanes.y + f.lanes.h {
                return;
            }
            let track_id = f.visible_tracks[t_idx].id;
            let beat_to_px = f64::from(body_rect.w) / f.view.len_beats.max(1e-6);
            let pad = f.style.automation_clip_v_pad_px;
            let clip_y = body_rect.y + pad;
            let clip_h = (body_rect.h - pad * 2.0).max(2.0);
            for clip_in in &lane.clips {
                let end = clip_in.start_beat + clip_in.len_beats;
                if end < f.view.start_beat || clip_in.start_beat > view_end {
                    continue;
                }
                let key = AutomationClipKey { track: track_id, lane: lane.id, clip: clip_in.id };
                // 点を置く帯 (x / w は lane body、y / h は縦 padding 適用後)。
                let band = Rect { y: clip_y, h: clip_h, ..body_rect };
                push_clip_point_rects(f, response, key, clip_in, band, beat_to_px, radius);
            }
        },
    );
}

/// automation clip 1 本ぶんの point rect を積む ([`push_point_rects`] の内側)。
///
/// **切り出しの理由はネスト段数** — 呼び出し側は「レーン closure → クリップ」で既に
/// 段を使い切っていて、点のループを畳み込むと不変条件 9 のインデント上限を超える。
fn push_clip_point_rects(
    f: &ArrangementFrame<'_>,
    response: &mut ArrangementResponse,
    key: AutomationClipKey,
    clip: &ArrangementAutomationClip,
    band: Rect,
    beat_to_px: f64,
    radius: f32,
) {
    for (p_idx, p) in clip.points.iter().enumerate() {
        let abs_beat = clip.start_beat + p.time_beat;
        if abs_beat < f.view.start_beat - 1e-6
            || abs_beat > f.view.start_beat + f.view.len_beats + 1e-6
        {
            continue;
        }
        // **式は描画側 (`draw.rs` の point dot) と 1 文字も変えない。** 丸めの位置が
        // 違うだけで、拍が大きいところで点と当たり判定が 1px ずれる。
        #[allow(clippy::cast_possible_truncation)]
        let px = band.x + ((abs_beat - f.view.start_beat) * beat_to_px) as f32;
        let py = band.y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * band.h;
        let r = Rect { x: px - radius, y: py - radius, w: radius * 2.0, h: radius * 2.0 };
        // ビュー左端の点は正方形の半分が lanes の外 (= ランチャー帯 / ヘッダ) へ
        // はみ出す。当たり判定なので可視部分だけ返す ([`hit_rect`])。
        let Some(hit) = hit_rect(r, f.lanes) else {
            continue;
        };
        #[allow(clippy::cast_possible_truncation)]
        let point_key = AutomationPointKey { clip: key, point_idx: p_idx as u32 };
        response.automation_point_rects.push((point_key, hit));
    }
}

/// M14 Phase 63n-3 (#028): automation_clip_rects を毎 frame 積む。
/// for_each_visible_lane で SSoT を共有 (= 描画 / hit-test と同じ式)、 visible automation
/// clip の lane body 内 rect (縦 padding 適用済) を返す。 collapsed group / hidden lane / view
/// beat 範囲外の clip は除外。 caller は右クリック context menu (Make Unique / Delete) の
/// anchor として使う想定。
fn push_automation_clip_and_lane_rects(
    f: &ArrangementFrame<'_>,
    response: &mut ArrangementResponse,
) {
    let view_end = f.view.start_beat + f.view.len_beats;
    for_each_visible_lane(
        &f.visible_tracks,
        &f.tops,
        f.view.track_row_h,
        f.header_pane.x,
        f.header_pane.w,
        f.lanes.x,
        f.lanes.w,
        f.style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            if body_rect.y + body_rect.h < f.lanes.y || body_rect.y > f.lanes.y + f.lanes.h {
                return;
            }
            let track_id = f.visible_tracks[t_idx].id;
            // daw_01 #086: lane の実行 rect (= body_rect そのもの) を毎 frame 返す。
            // Z 縦ズームがレイアウトを複製せず lane の実 y を引けるようにする。
            response
                .automation_lane_rects
                .push((AutomationLaneKey { track: track_id, lane: lane.id }, body_rect));
            let beat_to_px = f64::from(body_rect.w) / f.view.len_beats.max(1e-6);
            let pad = f.style.automation_clip_v_pad_px;
            let clip_y = body_rect.y + pad;
            let clip_h = (body_rect.h - pad * 2.0).max(2.0);
            for clip_in in &lane.clips {
                let end = clip_in.start_beat + clip_in.len_beats;
                if end < f.view.start_beat || clip_in.start_beat > view_end {
                    continue;
                }
                #[allow(clippy::cast_possible_truncation)]
                let cx_clip =
                    body_rect.x + ((clip_in.start_beat - f.view.start_beat) * beat_to_px) as f32;
                #[allow(clippy::cast_possible_truncation)]
                let cw = ((clip_in.len_beats * beat_to_px) as f32).max(2.0);
                let key =
                    AutomationClipKey { track: track_id, lane: lane.id, clip: clip_in.id };
                // clip_rects と同じ理由で lanes で切る (右クリックメニューのアンカー)。
                let Some(hit) = hit_rect(Rect { x: cx_clip, y: clip_y, w: cw, h: clip_h }, f.lanes)
                else {
                    continue;
                };
                response.automation_clip_rects.push((key, hit));
            }
        },
    );
}
