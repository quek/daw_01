//! hover 判定と cursor 決定。 **`apply` は `hover` が書いた `response` を読む**ので、
//! この 2 つの呼び出し順を入れ替えないこと。

use super::*;

/// `hovered_track` / `hovered_clip` / `hovered_zone` / `hovered_automation_lane` /
/// `hovered_section` / `hovered_section_zone` / `section_rects` / `dragging*` を埋める。
///
/// `response.hovered_clip` は **このフレーム中に**確定し、 heavy (`render::dispatch`) が
/// フェードの掴む正方形を出す clip の判定に使う (r.md #58)。
/// **`viewport_key` にも `fold_arrangement_clip_hash` にも入れないこと** —
/// 入れるとマウスを動かすたびにアレンジ全体が再構築される。
pub(super) fn hover(
    f: &ArrangementFrame<'_>,
    live: &LiveSessions,
    response: &mut ArrangementResponse,
) {
    if let Some((cx, cy)) = f.pointer.pos
        && f.lanes.contains(cx, cy)
    {
        response.hovered_track = track_index_from_y(cy, f.lanes.y, &f.tops)
            .and_then(|idx| f.visible_tracks.get(idx).map(|t| t.id));
        if let Some((hit_key, hit_kind)) = clip_hit(
            &f.visible_tracks,
            &f.tops,
            f.view,
            f.lanes,
            cx,
            cy,
            f.style.resize_handle_px,
        ) {
            response.hovered_clip = Some(hit_key);
            response.hovered_zone = Some(hit_kind);
        } else {
            // M14 Phase 116 (daw_01 #090): clip-first first-hit。 clip に当たらなかったときだけ
            // ポインタ下の automation lane body を公開する (`hovered_clip` と排他)。 `cx` は既に
            // `lanes.contains(cx, cy)` で lanes pane 内と確定済 (= header 帯ではなく body)。
            response.hovered_automation_lane = automation_lane_key_at_y(
                &f.visible_tracks,
                &f.tops,
                f.view.track_row_h,
                f.header_pane.x,
                f.header_pane.w,
                f.lanes.x,
                f.lanes.w,
                f.style,
                cy,
            )
            .map(|(key, _body_rect)| key);
        }
    }
    // M14 Phase 127 (daw_01 #105): Arranger section hover (arranger_rect 内、 clip / lane と y 排他)。
    if let Some((cx, cy)) = f.pointer.pos
        && f.arranger_lane_h > 0.0
        && f.arranger_rect.contains(cx, cy)
    {
        // hover の zone (Move/Resize) も保持して cursor を駆動する (id だけ捨てない)。
        if let Some((id, kind)) =
            section_hit(f.sections, f.arranger_rect, f.view, cx, cy, f.style.resize_handle_px)
        {
            response.hovered_section = Some(id);
            response.hovered_section_zone = Some(kind);
        }
    }
    // visible section の rect を response に積む (clip_rects と同 semantics、 caller の context_menu_for
    // 用)。 完全 off-screen (arranger_rect と x 交差しない) は除外。
    if f.arranger_lane_h > 0.0 {
        for s in f.sections {
            let r = section_to_rect(s, f.view, f.arranger_rect);
            if r.x + r.w >= f.arranger_rect.x && r.x <= f.arranger_rect.x + f.arranger_rect.w {
                response.section_rects.push((s.id, r));
            }
        }
    }
    response.dragging = live.clip_drag.as_ref().map(|nd| nd.kind);
    response.reordering = live.track_reorder.as_ref().map(|tr| tr.anchor_track_id);
    response.dragging_track_volume = live.track_volume.map(|tv| tv.track_id);
    // 既存 section の Move/Resize drag のみ報告 (Create 範囲 drag は transient creation なので None)。
    response.dragging_section = live.section_drag.and_then(|sd| match sd.kind {
        SectionGesture::Move => Some(ClipDragKind::Move),
        SectionGesture::ResizeLeft => Some(ClipDragKind::ResizeLeft),
        SectionGesture::ResizeRight => Some(ClipDragKind::ResizeRight),
        SectionGesture::Create => None,
    });
}

/// cursor 形状の決定。 優先順位は
/// header resize > lane/row resize > drag 種別 > reorder > volume > hover zone >
/// hover section zone > splitter hover (Ns) > header splitter hover (Ew) > automation clip zone。
///
/// **`automation_lane_resize_drag` / `track_row_resize_drag` / `header_resize_drag` は
/// `ui.widget_state` から直接読む** — この読みは `sessions::take` の release take より後に
/// 位置する必要があるため (release フレームでは None になるのが現行挙動)。 したがって
/// `LiveSessions` はこの 3 つを持たない。
pub(super) fn apply(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    live: &LiveSessions,
    response: &ArrangementResponse,
) {
    // drag 中 / hover 中の clip 上 / それ以外で arrangement 内なら明示的に Default
    // にリセット (`set_cursor` を呼ばないと OS 側に前フレームの形が残る、winit は state-full)。
    // M14 Phase 63n-3 (#028): automation clip drag 中も MIDI と同じ cursor 形状 (排他で `Some` 判定)。
    // M14 Phase 63n-5 (#030): lane resize drag 中は NsResize (cursor 移動の縦軸を強調)、 hover 時も
    // splitter hot zone なら NsResize にして discoverability を確保。 lane resize > clip drag > hover の
    // priority (= 同時に成立しないが、 万一重なっても resize を優先)。
    // M14 Phase 63n-6 (#031): row resize drag 中も NsResize (lane resize と同じ)。 lane / row の
    // 両 session を同 priority で扱い、 同時に立たない (press 時に一方しか起動しない)。
    let resize_active = {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.automation_lane_resize_drag.is_some() || state.track_row_resize_drag.is_some()
    };
    // M14 Phase 117 (daw_01 #091): header 幅 resize drag 中 / hover 中は EwResize (横軸)。
    // active は最優先 (NsResize / clip drag より上)、 hover は lane/row splitter NsResize の後に評価。
    let header_resize_active = {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.header_resize_drag.is_some()
    };
    let dragging_kind = response
        .dragging
        .or(live.automation_clip_drag.as_ref().map(|acd| acd.kind))
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
    } else if let Some((cx, cy)) = f.pointer.pos
        && (automation_lane_resize_splitter_at(
            &f.visible_tracks,
            &f.tops,
            f.view.track_row_h,
            f.header_pane.x,
            f.header_pane.w,
            f.lanes.x,
            f.lanes.w,
            f.style,
            cx,
            cy,
        )
        .is_some()
            || track_row_resize_splitter_at(
                &f.visible_tracks,
                &f.tops,
                f.view.track_row_h,
                f.lanes.x,
                f.lanes.w,
                f.style,
                cx,
                cy,
            )
            .is_some())
    {
        ui.set_cursor(CursorIcon::NsResize);
    } else if let Some((cx, cy)) = f.pointer.pos
        && header_resize_splitter_at(f.rect, f.header_w, f.style, cx, cy)
    {
        // M14 Phase 117 (daw_01 #091): header / lanes 境界 hover で EwResize (discoverability)。
        // lane/row splitter (NsResize) を上で先に判定済なので角の競合は NsResize 優先。
        ui.set_cursor(CursorIcon::EwResize);
    } else if let Some((cx, cy)) = f.pointer.pos
        && let Some((_key, kind, _clip_rect, _body_rect)) = automation_clip_zone_at(
            &f.visible_tracks,
            &f.tops,
            f.view.track_row_h,
            f.view,
            f.header_pane.x,
            f.header_pane.w,
            f.lanes,
            f.style,
            cx,
            cy,
            f.style.resize_handle_px,
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
}
