//! drag session の「overlay 用スナップショット」 と「release フレームの take」 を 1 か所に集約する。
//!
//! 旧実装はこの 2 つを session ごとに交互に 14 回書いており、 どの session が overlay を持ち
//! どれが release commit を持つのかが読み取れなかった。

use super::*;

/// このフレームに生きている session (overlay / hover / cursor / heavy / header / rects が読む)。
///
/// **14 session のうち 4 つを意図的に入れていない。 理由は 2 種類ある**:
///
/// - `automation_lane_resize_drag` / `track_row_resize_drag` / `header_resize_drag` —
///   cursor がこの 3 つを読むのは release take の **後** なので (`cursor::apply` の
///   `resize_active` / `header_resize_active`)、 live snapshot に入れると release フレームで
///   `Some` に化けてカーソル形状が 1 フレーム変わる。
///   `cursor::apply` 内で `ui.widget_state` を読む形を維持する。
/// - `playhead_drag` — **live snapshot を読む消費者が 1 つも無い**。 widget 内の
///   `playhead_drag` 参照は press の起動 / `PressClaim` の 11 列挙 / drag continuation /
///   per-frame emit / release take の discard、 および `release.rs` の marquee ゲートと
///   `mod.rs` の端スクロール軸判定だけで、 後者 2 つはどちらも `&ArrangementState` を
///   直接読む。 最終値は per-frame emit (`drag::emit_playhead`) が出しているので release
///   commit も無く、 take して捨てるだけ。
///   **「読む人がいないから入れない」であって「入れ忘れ」ではない。**
///   復活させたくなったら、 まず消費者を 1 つ挙げること。
#[derive(Default)]
pub(super) struct LiveSessions {
    pub clip_drag: Option<ClipDragSession>,
    pub loop_drag: Option<LoopDragSession>,
    pub section_drag: Option<SectionDragSession>,
    pub track_reorder: Option<TrackReorderSession>,
    pub track_volume: Option<TrackVolumeDragSession>,
    pub audio_drag: Option<AudioDragSession>,
    pub point_drag: Option<AutomationPointDragSession>,
    pub automation_clip_drag: Option<AutomationClipDragSession>,
    pub automation_lasso: Option<AutomationLassoSession>,
    /// r.md #73: 旧 `automation_curve_param` (中央ハンドル drag) の差し替え。
    pub automation_segment_bend: Option<AutomationSegmentBendSession>,
}

/// release フレームで `take()` した session。 **`release::commit_releases` だけが読む。**
#[derive(Default)]
pub(super) struct ReleasedSessions {
    pub clip_drag: Option<ClipDragSession>,
    /// 短クリックに格下げされた clip drag の `(last_mouse, last_ctrl, last_shift)`。
    pub clip_short_click_pos: Option<((f32, f32), bool, bool)>,
    pub audio_drag: Option<AudioDragSession>,
    pub point_drag: Option<AutomationPointDragSession>,
    pub automation_clip_drag: Option<AutomationClipDragSession>,
    /// r.md #73: 旧 `automation_curve_param` (中央ハンドル drag) の差し替え。
    pub automation_segment_bend: Option<AutomationSegmentBendSession>,
    pub automation_lasso: Option<AutomationLassoSession>,
    pub lane_resize: Option<AutomationLaneResizeDragSession>,
    pub section_drag: Option<SectionDragSession>,
    pub loop_drag: Option<LoopDragSession>,
    pub track_volume: Option<TrackVolumeDragSession>,
    /// `(source_track_ids, parent, anchor_after)`。 `resolve_track_drop` で解決済。
    pub pending_drop: Option<(Vec<u32>, Option<u32>, Option<u32>)>,
    /// `pending_drop` の hash。 release フレームの optimistic preview で cache miss を強制するため
    /// `viewport_key` に混ぜる。
    pub pending_reorder_hash: u64,
}

/// session の clone (overlay 用) と release take を 1 度に行う。
/// `response.automation_lasso_active` はここで立てる。
///
/// **`playhead_drag` / `track_row_resize_drag` / `header_resize_drag` は release フレームで
/// `take()` して捨てるだけ** (per-frame emit で最終値が出ているので release commit は不要)。
pub(super) fn take(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    response: &mut ArrangementResponse,
) -> (LiveSessions, ReleasedSessions) {
    let wid = f.wid;
    let pointer = f.pointer;
    let mut live = LiveSessions::default();
    let mut released = ReleasedSessions::default();

    // 2) drag overlay 計算用に clone を取る (last_mouse を更新した後)。
    live.clip_drag = {
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
            released.clip_short_click_pos = Some((nd.last_mouse, nd.last_ctrl, nd.last_shift));
        } else {
            released.clip_drag = Some(nd);
        }
    }

    live.loop_drag = {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.loop_drag
    };
    released.loop_drag = if pointer.primary_just_released {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.loop_drag.take()
    } else {
        None
    };

    // M14 Phase 127 (daw_01 #105): section drag の overlay 用 copy (SectionDragSession は Copy) と
    // release 取り出し (`loop_drag` と同 idiom)。
    live.section_drag = {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.section_drag
    };
    released.section_drag = if pointer.primary_just_released {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.section_drag.take()
    } else {
        None
    };

    // M10 Phase 46: track reorder session の overlay 用 clone と release 取り出し。
    // M14 Phase 63c (#016): TrackReorderSession は Vec<u32> を持つため Copy 不可。 ここで clone。
    live.track_reorder = {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.track_reorder.clone()
    };
    let track_reorder_release_raw: Option<TrackReorderSession> = if pointer.primary_just_released {
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
    released.pending_drop = track_reorder_release_raw.as_ref().and_then(|tr| {
        let dx = tr.last_mouse_x - tr.anchor_mouse_x;
        let dy = tr.last_mouse_y - tr.anchor_mouse_y;
        if (dx * dx + dy * dy).sqrt() < REORDER_DRAG_THRESHOLD_PX {
            return None;
        }
        let drop = resolve_track_drop(
            f.tracks,
            &f.visible_tracks,
            &f.tops,
            &f.is_group_set,
            &tr.source_track_ids,
            f.style.indent_px,
            tr.last_mouse_y,
            tr.last_mouse_x,
            tr.anchor_mouse_x,
        );
        Some((tr.source_track_ids.clone(), drop.parent, drop.anchor_after))
    });
    released.pending_reorder_hash = released.pending_drop.as_ref().map_or(0_u64, |(ts, p, a)| {
        let mut h = u64::from(p.unwrap_or(u32::MAX));
        h = h.wrapping_mul(31).wrapping_add(u64::from(a.unwrap_or(u32::MAX)));
        for t in ts {
            h = h.wrapping_mul(31).wrapping_add(u64::from(*t));
        }
        h.wrapping_mul(0x100_0000_01B3)
    });

    // M10 Phase 47b: track volume drag session の overlay 用 clone と release 取り出し。
    live.track_volume = {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.track_volume_drag
    };
    released.track_volume = if pointer.primary_just_released {
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
    live.audio_drag = {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.audio_drag
    };
    released.audio_drag = if pointer.primary_just_released {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.audio_drag.take()
    } else {
        None
    };

    // M14 Phase 63n-2 (#028): automation_point_drag overlay clone + release take。
    live.point_drag = {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.automation_point_drag
    };
    released.point_drag = if pointer.primary_just_released {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.automation_point_drag.take()
    } else {
        None
    };

    // M14 Phase 63n-5 (#030): automation_lane_resize_drag release take (overlay は不要 — caller が
    // per-frame 受信した SetLaneHeight で `lane.height_px` を update することで lane が伸び縮みする
    // 様子が cached 描画に直接反映される)。 release frame で session を take し、 final height を発行。
    released.lane_resize = if pointer.primary_just_released {
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
    live.automation_clip_drag = {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.automation_clip_drag.clone()
    };
    released.automation_clip_drag = if pointer.primary_just_released {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.automation_clip_drag.take()
    } else {
        None
    };

    // M14 Phase 63n-8 (#033): automation_lasso_drag overlay clone + release take。
    // overlay は drag 中の lasso rect を cached 外で描画 (style.automation_lasso_fill / border)、
    // release で 1 度だけ `SelectAutomationPoints` を発行 (next 計算は anchor 時の modifier で
    // replace / union / XOR 分岐)。
    live.automation_lasso = {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.automation_lasso_drag
    };
    released.automation_lasso = if pointer.primary_just_released {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.automation_lasso_drag.take()
    } else {
        None
    };
    if live.automation_lasso.is_some() {
        response.automation_lasso_active = true;
    }

    // r.md #73: automation_segment_bend overlay clone + release take。
    // overlay は drag 中の preview curve を cached 外で描画 (`preview_curve` で再 flatten した
    // polyline を `automation_curve_bend_preview_color` で描く。 掴んでいる区間の base curve は
    // cached 側が描かないので、 これが唯一の線)、 release で 1 度だけ
    // `SetAutomationCurve { .., point_id, next }` を発行 (anchor と同値なら no-op)。
    //
    // **`live` の clone は `released` の take より前**。 release frame でも overlay が Some で
    // 残るので、 「base は既に skip、 preview はもう無い」 の 1 frame 抜けが起きない。
    live.automation_segment_bend = {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.automation_segment_bend
    };
    released.automation_segment_bend = if pointer.primary_just_released {
        let state: &mut ArrangementState = ui.widget_state(wid);
        state.automation_segment_bend.take()
    } else {
        None
    };

    (live, released)
}

/// heavy 描画が重ねる overlay 群。
#[derive(Default)]
pub(super) struct Overlays {
    /// `(session, beat_delta, track_delta)`。 press 直後 (delta=0) から出す — 閾値ゲートを
    /// 張ると mouse down のハイライトが消える (r.md #24)。
    pub clip: Option<(ClipDragSession, f64, i32)>,
    /// Resize ゴーストの最小長 (snap unit と `MIN_CLIP_LEN_BEATS` の大きい方)。
    /// alt の真値は session の `last_alt` (r.md #68: preview ≠ commit を防ぐ)。
    pub clip_min_len: f64,
    pub audio: Option<AudioDragSession>,
    pub point: Option<AutomationPointDragSession>,
    pub automation_clip: Option<AutomationClipDragSession>,
    /// r.md #73: 旧 `curve_param` (中央ハンドル) の差し替え。
    pub segment_bend: Option<AutomationSegmentBendSession>,
    pub lasso: Option<AutomationLassoSession>,
    pub section: Option<SectionDragSession>,
    pub reorder: Option<ReorderOverlay>,
    /// snap 適用済の loop preview 範囲 (commit と同一値)。
    pub loop_preview: Option<(f64, f64)>,
    /// `released.pending_reorder_hash` の写し (`viewport_key` の材料)。
    pub reorder_hash: u64,
}

/// **呼び出し位置が `cursor` より前に動くことの正当性**: ここが含む
/// `clip_min_len` / `section` / `reorder` の 3 つは旧実装では cursor ブロックの **後**に
/// あったが、 いずれも**純粋**である。 入力は `f.*` (地形、 誰も書かない) / `live.*`
/// (この時点で確定済の session clone) / `view.snap` / `style` だけで、 出力は `Overlays` の
/// フィールドのみ (`ArrangementState` にも `ArrangementResponse` にも書かない)。
/// したがって `cursor::hover` / `cursor::apply` より前に評価しても、 両者の入力・出力とも
/// 変わらない。
pub(super) fn overlays(
    f: &ArrangementFrame<'_>,
    live: &LiveSessions,
    released: &ReleasedSessions,
) -> Overlays {
    // drag overlay delta (last_mouse ベース、release と一貫)。
    // r.md #24: overlay は press 直後 (delta=0) から出す (= mouse down で掴んだ clip が
    // 選択枠でハイライトされる)。 press 中に中身 (名前 / 波形 / MIDI) が消えないのは
    // `draw_drag_preview` が **中身入りの半透明コピー** を描くようにしたため (旧: 中身の無い
    // 不透明 ghost が元 clip を覆い隠していた = #24 の主因)。 閾値ゲートは張らない
    // (張ると mouse down のハイライトが消える)。
    let clip: Option<(ClipDragSession, f64, i32)> = live.clip_drag.as_ref().map(|nd| {
        let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
        let raw = f64::from(dx) * f.beat_per_px;
        // **絶対位置 snap** (= Cubase / Live と同じ「nearest grid alignment」 動作):
        // anchor 0 の編集対象端 (Move=start / ResizeRight=end / ResizeLeft=start) の絶対位置を
        // grid に round → その差分 (`adjusted_delta`) を全 anchor に同じだけ適用する。
        // delta-snap (= raw_delta だけを round) だと anchor が grid 外に既にずれていた場合
        // (例: 前回 Alt+drag で +0.078 拍ずらした) に release してもずれが永久残る。
        // 絶対 snap なら anchor 0 が必ず grid 上に着地し、 複数選択は相対関係を維持。
        // alt は drag state の `last_alt` を真値とし、 `pointer.modifiers.alt` を直接見ない。
        let beat_delta =
            compute_clip_drag_beat_delta(nd, raw, &f.view.snap, f.zoom_x_px_per_beat);
        // track 方向は y→visible 行 index 解決の差 (per-track 行高 / lane 展開対応)。
        let track_delta = compute_clip_drag_track_delta(nd, &f.tops);
        (nd.clone(), beat_delta, track_delta)
    });

    // M14 Phase 63j (#024): overlay の preview range も `compute_loop_drag_endpoints` で
    // snap 適用済 (commit と同一値で確定、 release 時の「カクッ」 ずれを回避)。 alt は session の
    // `last_alt` を真値とし、 `pointer.modifiers.alt` を直接見ない (clip_drag と同じ pattern)。
    let loop_preview: Option<(f64, f64)> = live.loop_drag.map(|ld| {
        let cur_beat = px_to_beat(ld.last_mouse_x, f.ruler.x, f.ruler.w, f.view);
        compute_loop_drag_endpoints(&ld, cur_beat, &f.view.snap, f.zoom_x_px_per_beat)
    });

    // M9 Phase 45f: drag overlay の Resize min_len は snap unit。 下限は model の
    // `MIN_CLIP_LEN_BEATS` (= `resize_clip` の clamp と同じ 1/16)。 r.md #68: ここが
    // 0.05 だったので、 snap off (Alt) で 1/16 未満までゴーストが縮み、 release で
    // 1/16 に戻る = preview ≠ commit だった。
    // release 側 min_len と一貫させるため、 alt 真値は drag session の `last_alt` を使う
    // (overlay と release commit が必ず同一 unit で確定する)。 overlay 不在時 (drag していない)
    // は min_len 自体使われないので、 alt = false で適当な値で初期化しておけばよい。
    const MIN_CLIP_LEN: f64 = common::model::MIN_CLIP_LEN_BEATS;
    let overlay_alt = clip.as_ref().is_some_and(|(nd, _, _)| nd.last_alt);
    let clip_min_len: f64 = if f.view.snap.is_active(overlay_alt) {
        f.view
            .snap
            .beat_unit(f.zoom_x_px_per_beat)
            .map_or(MIN_CLIP_LEN, |u| u.max(MIN_CLIP_LEN))
    } else {
        MIN_CLIP_LEN
    };

    // M10 Phase 46 → 101 (daw_01 #072): track reorder の drag preview geometry。
    // dist >= 閾値 のときのみ overlay 描画 (短 click 中は静止 = button click と区別がつかないため
    // UI ノイズ)。 **commit (`pending_drop`) と同じ `resolve_track_drop`** を通すので indicator が
    // 指す位置 = 実際に着地する位置 が必ず一致する (旧 `compute_reorder_target_index` は parent /
    // 深さを描けず blank-drop で実結果とズレていた)。
    let reorder: Option<ReorderOverlay> = live
        .track_reorder
        .as_ref()
        .filter(|tr| {
            let dx = tr.last_mouse_x - tr.anchor_mouse_x;
            let dy = tr.last_mouse_y - tr.anchor_mouse_y;
            (dx * dx + dy * dy).sqrt() >= REORDER_DRAG_THRESHOLD_PX
        })
        .map(|tr| {
            let drop = resolve_track_drop(
                f.tracks,
                &f.visible_tracks,
                &f.tops,
                &f.is_group_set,
                &tr.source_track_ids,
                f.style.indent_px,
                tr.last_mouse_y,
                tr.last_mouse_x,
                tr.anchor_mouse_x,
            );
            let indicator_y = f
                .tops
                .get(drop.gap)
                .copied()
                .or_else(|| f.tops.last().copied())
                .unwrap_or(f.header_pane.y);
            let indent_x = f.header_pane.x + f32::from(drop.depth) * f.style.indent_px;
            // parent が group のとき header 行を hilight。 parent が collapsed で不可視なら
            // (visible に居ない → position None →) hilight しない (不可視 UI を光らせない意図の
            // None。 reparent 構造自体は commit と同一 resolver なので一致する)。
            let highlight_row = drop.parent.and_then(|pid| {
                f.visible_tracks.iter().position(|t| t.id == pid).map(|vi| {
                    let y = f.tops.get(vi).copied().unwrap_or(f.header_pane.y);
                    let h = effective_track_row_h(&f.visible_tracks[vi], f.view.track_row_h);
                    Rect { x: f.content_below_ruler.x, y, w: f.content_below_ruler.w, h }
                })
            });
            ReorderOverlay { indicator_y, indent_x, drag_center_y: tr.last_mouse_y, highlight_row }
        });

    Overlays {
        clip,
        clip_min_len,
        // M14 Phase 63k (#025): audio_drag overlay (ghost = drag 中の preview line / fade
        // envelope / label) は cached 外で描画する。
        audio: live.audio_drag,
        // M14 Phase 63n-2 (#028): point ghost は drag 中の preview を新位置に上書き
        // (cached 内描画は anchor 値のまま)。
        point: live.point_drag,
        // M14 Phase 63n-3 (#028): ghost rect は drag 中の preview (新位置 / 新長さ、
        // cross-lane drop なら新 lane の body 内) を cached 外で重ねる。 base 描画 (cached 内) も
        // 同 frame 表示されるが、 ghost が上に重なる。
        automation_clip: live.automation_clip_drag.clone(),
        // r.md #73: 区間 bend は preview curve を cached 外で描画
        // (drag 中のみ逆算した `preview_curve` で live update)。
        segment_bend: live.automation_segment_bend,
        // M14 Phase 63n-8 (#033): lasso rect を cached 外で描画。
        lasso: live.automation_lasso,
        // M14 Phase 127 (daw_01 #105): Arranger レーン overlay 用 capture。
        section: live.section_drag,
        reorder,
        loop_preview,
        reorder_hash: released.pending_reorder_hash,
    }
}
