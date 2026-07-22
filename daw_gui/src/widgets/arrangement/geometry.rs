//! S4b Phase D: arrangement widget の幾何 / hit-test helper 群 (レイアウト・座標変換・
//! automation lane hit-test)。 型・session は `use super::*` で親から継承する。

use super::*;

/// M14 Phase 63n-1 (#028): visible track 群の prefix sum row top (`tops.len() == visible_tracks.len() + 1`)。
/// `tops[i]` = i 番目 track 上端 = (i-1) 番目 track 下端、 `tops[i+1] - tops[i]` で i 番目の expanded
/// 高さ (= `track_row_height(visible_tracks[i], track_row_h)`)。 lane 0 個 = `tops[i] = lanes_y -
/// track_top + i * track_row_h` と等価 (= 既存挙動完全互換)。 描画 / hit-test 全箇所が共有する SSoT。
#[must_use]
pub fn visible_track_row_tops(
    visible_tracks: &[ArrangementTrack],
    lanes_y: f32,
    track_top: f32,
    track_row_h: f32,
) -> Vec<f32> {
    let mut tops = Vec::with_capacity(visible_tracks.len() + 1);
    let mut y = lanes_y - track_top;
    tops.push(y);
    for t in visible_tracks {
        y += track_row_height(t, track_row_h);
        tops.push(y);
    }
    tops
}

/// (track_row_top, track_row_h, clip) → screen rect (lanes 範囲、horizontal clip 形状)。
/// M14 Phase 63n-1 (#028): row_top は caller が `tops[visible_idx]` で渡す前提
/// (lane 込みの prefix sum)。 `track_row_h` は **MIDI/Audio clip 行の高さのみ** (= `view.track_row_h`、
/// lane 高さは含まない)。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn clip_to_rect(
    track_row_top: f32,
    track_row_h: f32,
    clip: &ClipView,
    view: ArrangementView,
    lanes: Rect,
) -> Rect {
    let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
    let x = lanes.x + ((clip.start_beat - view.start_beat) * beat_to_px) as f32;
    let w = ((clip.len_beats * beat_to_px) as f32).max(2.0);
    let h = (track_row_h - 4.0).max(2.0);
    Rect { x, y: track_row_top + 2.0, w, h }
}

/// M14 Phase 63k (#025): audio_edit が Some の clip 上の audio gesture grip ヒット種別。
/// 公開 `ClipDragKind` には足さず内部 enum で扱う (caller の hover/drag 報告は既存 3 variant
/// のまま維持、 audio gesture は widget 内で完結)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioGripHit {
    /// clip 上端の左角 (12×12 px)。 fade_in length / curve drag の起点。
    FadeCornerIn,
    /// clip 上端の右角 (12×12 px)。 fade_out length / curve drag の起点。
    FadeCornerOut,
    /// clip 中央 horizontal 帯 (handle line ±4 px、 端から x_margin 内側)。 gain dB drag の起点。
    GainHandleBand,
}

/// M14 Phase 63k (#025): 単一 clip の audio_edit grip ヒット (priority: gain > fade corner)。
/// `audio_edit` が None の clip ではヒット無し、 `r.w < min_w` の短 clip でも無効化。
/// fade 角は resize handle (4 px) より priority 高 (= clip 内側の上端 12×12 を fade に振る)、
/// resize は fade 角の外側 (clip rect の外側 ±4 px) で活きる。
#[allow(clippy::too_many_arguments)]
pub(super) fn audio_grip_hit(
    track_row_top: f32,
    track_row_h: f32,
    clip: &ClipView,
    view: ArrangementView,
    lanes: Rect,
    cx: f32,
    cy: f32,
    style: &ArrangementStyle,
) -> Option<AudioGripHit> {
    clip.audio_edit?;
    let r = clip_to_rect(track_row_top, track_row_h, clip, view, lanes);
    if r.w < style.audio_min_clip_w_for_handles_px {
        return None;
    }
    if cy < r.y || cy >= r.y + r.h {
        return None;
    }
    let corner = style.audio_fade_corner_size_px;
    // priority 1: gain handle band — clip 中央 y ±half_band、 端から x_margin 内側のみ
    let center_y = r.y + r.h * 0.5;
    let half_band = style.audio_db_handle_band_h * 0.5;
    let margin = style.audio_db_handle_x_margin;
    if cx >= r.x + margin
        && cx < r.x + r.w - margin
        && cy >= center_y - half_band
        && cy < center_y + half_band
    {
        return Some(AudioGripHit::GainHandleBand);
    }
    // priority 2: fade in 角 (top-left 12×12)
    if cx >= r.x && cx < r.x + corner && cy >= r.y && cy < r.y + corner {
        return Some(AudioGripHit::FadeCornerIn);
    }
    // priority 3: fade out 角 (top-right 12×12)
    if cx >= r.x + r.w - corner && cx < r.x + r.w && cy >= r.y && cy < r.y + corner {
        return Some(AudioGripHit::FadeCornerOut);
    }
    None
}

/// M14 Phase 63k (#025): lanes 内 cursor 位置から hit する `(ClipKey, AudioGripHit)` を返す
/// (clip の `audio_edit = Some` のものだけが対象、 後勝ち)。 `clip_hit` の audio gesture 版。
#[must_use]
pub(super) fn audio_grip_hit_in_lanes(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    cx: f32,
    cy: f32,
    style: &ArrangementStyle,
) -> Option<(ClipKey, AudioGripHit)> {
    if !lanes.contains(cx, cy) {
        return None;
    }
    let visible_idx = track_index_from_y(cy, lanes.y, tops)?;
    let track = visible_tracks.get(visible_idx)?;
    let row_top = tops[visible_idx];
    // 描画 (draw_clip_audio_overlay) と同じ per-track 実効行高で判定する。
    let row_h = effective_track_row_h(track, view.track_row_h);
    let mut hit: Option<(ClipKey, AudioGripHit)> = None;
    for clip in &track.clips {
        if let Some(zone) = audio_grip_hit(row_top, row_h, clip, view, lanes, cx, cy, style) {
            hit = Some((ClipKey { track: track.id, clip: clip.id }, zone));
        }
    }
    hit
}

/// 内部 helper: cursor 位置がこの clip のどの zone (Move / ResizeLeft / ResizeRight)
/// に該当するかを返す。`clip_hit` から呼ばれる。
///
/// 判定範囲 (x 方向): clip rect の左右 edge から **内外** ±`edge` px (= 8px 幅のハンドル帯)。
/// y 方向は clip rect 内のみ (拡張なし、隣接 track row との衝突回避)。
///
/// 短 clip (`r.w <= edge * 2.0`) は rect 内では Move 強制 (左右 edge 領域が重なって
/// 判別不能なため)、rect 外側のみ ResizeLeft / ResizeRight として扱う。
#[allow(clippy::too_many_arguments)]
pub(super) fn clip_zone_at(
    track_row_top: f32,
    track_row_h: f32,
    clip: &ClipView,
    view: ArrangementView,
    lanes: Rect,
    cx: f32,
    cy: f32,
    edge: f32,
) -> Option<ClipDragKind> {
    let r = clip_to_rect(track_row_top, track_row_h, clip, view, lanes);
    // y は clip rect 内のみ (Rect::contains の半開区間と整合)
    if cy < r.y || cy >= r.y + r.h {
        return None;
    }
    // x の拡張範囲 [r.x - edge, r.x + r.w + edge) 外は不参加
    if cx < r.x - edge || cx >= r.x + r.w + edge {
        return None;
    }
    let in_rect = cx >= r.x && cx < r.x + r.w;
    let near_left = cx < r.x + edge;
    let near_right = cx >= r.x + r.w - edge;
    let short_clip = r.w <= edge * 2.0;

    Some(if short_clip && in_rect {
        ClipDragKind::Move
    } else if near_left && (!in_rect || cx - r.x < edge) {
        ClipDragKind::ResizeLeft
    } else if near_right && (!in_rect || (r.x + r.w) - cx < edge) {
        ClipDragKind::ResizeRight
    } else {
        ClipDragKind::Move
    })
}

/// lanes 内 cursor 位置から hit する (ClipKey, ClipDragKind) を返す。
///
/// resize handle は clip rect の左右 edge から **内外** ±`resize_handle_px` の範囲
/// (= 8px 幅のハンドル帯)。短 clip (`r.w <= resize_handle_px * 2`) は rect 内は Move 強制、
/// rect 外側のみ resize 判定。
///
/// 隣接 clip (A.right == B.left) では両者の resize ハンドル帯が共有境界付近で重なる。
/// このとき **cursor が rect 内部に在る clip (in-rect) を、外側拡張ハンドル
/// (outer-extension) しか当たらない clip より無条件で優先**する。これにより A の右端を
/// 掴みたいのに B の左端 resize に奪われる問題 (#101) を解消。同 tier (両方 in-rect = overlap、
/// または両方 outer = 微小 gap) は resize edge への水平距離が近い方を採用し、同距離なら
/// 後勝ち (描画順で前面) を踏襲する。piano_roll の [`note_hit_in`](super::piano_roll) と構造同一。
#[must_use]
pub fn clip_hit(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    cx: f32,
    cy: f32,
    resize_handle_px: f32,
) -> Option<(ClipKey, ClipDragKind)> {
    if !lanes.contains(cx, cy) {
        return None;
    }
    let visible_idx = track_index_from_y(cy, lanes.y, tops)?;
    let track = visible_tracks.get(visible_idx)?;
    let row_top = tops[visible_idx];
    // 描画 (draw_clips) と同じ per-track 実効行高で判定する。 global
    // `view.track_row_h` のままだと、 行を太らせた track のクリップ下部が
    // 「描画されているのに掴めない」 (marquee 起動 / dblclick で重複生成) になる。
    let row_h = effective_track_row_h(track, view.track_row_h);
    let mut hit: Option<(ClipKey, ClipDragKind)> = None;
    let mut hit_inside = false;
    let mut hit_edge_dist = f32::INFINITY;
    for clip in &track.clips {
        let Some(kind) =
            clip_zone_at(row_top, row_h, clip, view, lanes, cx, cy, resize_handle_px)
        else {
            continue;
        };
        let r = clip_to_rect(row_top, row_h, clip, view, lanes);
        let inside = cx >= r.x && cx < r.x + r.w;
        // resize edge への水平距離 (Move は当該 cursor 位置 = 距離 0 扱い)。
        let edge_x = match kind {
            ClipDragKind::ResizeLeft => r.x,
            ClipDragKind::ResizeRight => r.x + r.w,
            ClipDragKind::Move => cx,
        };
        let dist = (cx - edge_x).abs();
        // in-rect は outer に無条件で勝つ。同 tier は近い edge 優先 (同距離は後勝ち)。
        let better = if inside == hit_inside {
            dist <= hit_edge_dist
        } else {
            inside
        };
        if better {
            hit = Some((ClipKey { track: track.id, clip: clip.id }, kind));
            hit_inside = inside;
            hit_edge_dist = dist;
        }
    }
    hit
}

/// M14 Phase 127 (daw_01 #105): section の Arranger レーン内 rect (`clip_to_rect` の section 版)。
/// レーンは track row のような縦分割を持たないので高さは arranger レーン全高。 時間→x は ruler /
/// clips と同じ `beat_to_px` mapping を共有 (ruler / playhead / loop band と縦に揃う)。
pub(super) fn section_to_rect(section: &SectionView, view: ArrangementView, arranger: Rect) -> Rect {
    section_rect_from(section.start_beat, section.len_beats, view, arranger)
}

/// M14 Phase 127 (#105): `(start_beat, len_beats)` から Arranger レーン内 rect を計算 (`section_to_rect`
/// と drag preview 描画が共有、 preview のために temp `SectionView` を作らずに済む)。
pub(super) fn section_rect_from(start_beat: f64, len_beats: f64, view: ArrangementView, arranger: Rect) -> Rect {
    let beat_to_px = f64::from(arranger.w) / view.len_beats.max(1e-6);
    let x = arranger.x + ((start_beat - view.start_beat) * beat_to_px) as f32;
    let w = ((len_beats * beat_to_px) as f32).max(2.0);
    Rect { x, y: arranger.y, w, h: arranger.h }
}

/// M14 Phase 127 (#105): section rect 上の cursor x がどの zone (Move / ResizeLeft / ResizeRight) かを返す。
/// `clip_zone_at` の x ロジックと同一 (resize handle は rect 左右 edge から内外 ±`edge`、 短 section は
/// rect 内 Move 強制 / 外側のみ resize)。 y は arranger レーン全高なので呼び出し側の `arranger.contains`
/// で既に保証され、 ここでは x のみ判定する。
pub(super) fn section_zone_at(r: Rect, cx: f32, edge: f32) -> Option<ClipDragKind> {
    if cx < r.x - edge || cx >= r.x + r.w + edge {
        return None;
    }
    let in_rect = cx >= r.x && cx < r.x + r.w;
    let near_left = cx < r.x + edge;
    let near_right = cx >= r.x + r.w - edge;
    let short = r.w <= edge * 2.0;
    Some(if short && in_rect {
        ClipDragKind::Move
    } else if near_left && (!in_rect || cx - r.x < edge) {
        ClipDragKind::ResizeLeft
    } else if near_right && (!in_rect || (r.x + r.w) - cx < edge) {
        ClipDragKind::ResizeRight
    } else {
        ClipDragKind::Move
    })
}

/// M14 Phase 127 (#105): Arranger レーン内 cursor 位置から hit する `(section id, ClipDragKind)` を返す。
/// `clip_hit` と同じ **2-tier in-rect 優先** (隣接 section の共有境界では内側 section を、 外側拡張ハンドル
/// しか当たらない section より無条件優先、 同 tier は resize edge への水平距離が近い方、 同距離は後勝ち)。
/// section は arranger レーン全高なので y は `arranger.contains` のみで判定する。
#[must_use]
pub(super) fn section_hit(
    sections: &[SectionView],
    arranger: Rect,
    view: ArrangementView,
    cx: f32,
    cy: f32,
    resize_handle_px: f32,
) -> Option<(u32, ClipDragKind)> {
    if arranger.h <= 0.0 || !arranger.contains(cx, cy) {
        return None;
    }
    let mut hit: Option<(u32, ClipDragKind)> = None;
    let mut hit_inside = false;
    let mut hit_edge_dist = f32::INFINITY;
    for s in sections {
        let r = section_to_rect(s, view, arranger);
        let Some(kind) = section_zone_at(r, cx, resize_handle_px) else {
            continue;
        };
        let inside = cx >= r.x && cx < r.x + r.w;
        let edge_x = match kind {
            ClipDragKind::ResizeLeft => r.x,
            ClipDragKind::ResizeRight => r.x + r.w,
            ClipDragKind::Move => cx,
        };
        let dist = (cx - edge_x).abs();
        let better = if inside == hit_inside {
            dist <= hit_edge_dist
        } else {
            inside
        };
        if better {
            hit = Some((s.id, kind));
            hit_inside = inside;
            hit_edge_dist = dist;
        }
    }
    hit
}

/// cursor が strictly どの section 帯の **内側** (in-rect) にあるかを返す。 `section_hit` と
/// 違い resize handle の外側拡張 (`±resize_handle_px`) を **一切含めない**。 dblclick rename / 右クリック
/// メニューは「帯そのもの」 を対象にする **point gesture** で、 帯の外側 (隣の空きレーン) で発火しては
/// いけない (帯のすぐ隣の空白を dblclick すると隣 section の rename になっていた bug)。 Move/Resize の
/// **drag** は掴みやすさのため引き続き `section_hit` の拡張ハンドルを使う。 section は昇順・非交差前提、
/// 共有境界 (`A.right == B.left`) は半開区間 `[x, x+w)` で右 section に属す (= 1 点に高々 1 section)。
#[must_use]
pub(super) fn section_at_inrect(
    sections: &[SectionView],
    arranger: Rect,
    view: ArrangementView,
    cx: f32,
    cy: f32,
) -> Option<u32> {
    if arranger.h <= 0.0 || !arranger.contains(cx, cy) {
        return None;
    }
    sections.iter().find_map(|s| {
        let r = section_to_rect(s, view, arranger);
        (cx >= r.x && cx < r.x + r.w).then_some(s.id)
    })
}

/// clip / section の drag zone (`ClipDragKind`) を cursor 形状へ写す共通マップ
/// (中央 Move → `Move`、 端 Resize → `EwResize`)。 clip drag / clip hover / section drag / section hover の
/// 4 経路が同じ写像を共有する (= 端を掴んでリサイズできることを ↔ カーソルで discoverable にする)。
pub(super) fn drag_kind_cursor(kind: ClipDragKind) -> CursorIcon {
    match kind {
        ClipDragKind::Move => CursorIcon::Move,
        ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => CursorIcon::EwResize,
    }
}

/// M14 Phase 127 (#105): 拍子から 1 bar の拍数を返す (`numerator * 4 / denominator`)。 4/4=4、 3/4=3、
/// 6/8=3。 0 除算 / 0 拍を避けるため numerator / denominator は 1 以上、 結果は `1.0` 以上に floor する。
pub(super) fn beats_per_bar(time_sig: (u8, u8)) -> f64 {
    let num = f64::from(time_sig.0.max(1));
    let den = f64::from(time_sig.1.max(1));
    (num * 4.0 / den).max(1.0)
}

/// y 座標から **visible track index** を計算 (M14 Phase 63n-1: prefix-sum 化)。
/// `tops` は `visible_track_row_tops` の戻り値 (= len = visible_tracks.len() + 1、 prefix sum
/// of expanded heights)。 lane 0 個 = `tops[i] = lanes_y - track_top + i * track_row_h` と等価で
/// 既存の `(local / track_row_h).floor()` と同じ index を返す (= 既存挙動完全互換)。
/// `tops.len() < 2` または y が範囲外なら `None`。
#[must_use]
pub fn track_index_from_y(y: f32, _lanes_y: f32, tops: &[f32]) -> Option<usize> {
    if tops.len() < 2 {
        return None;
    }
    if y < tops[0] {
        return None;
    }
    // tops は単調増加。 y が tops[i] <= y < tops[i+1] となる i を返す。
    // partition_point(|&t| t <= y) - 1 = i (binary search で O(log N))。
    let i = tops.partition_point(|&t| t <= y);
    if i == 0 || i > tops.len() - 1 {
        return None;
    }
    Some(i - 1)
}

// `LoopBandHit` / `loop_band_hit_kind` は M14 Phase 69 (#041) で
// `crate::widgets::ruler_ops` に extract (piano_roll と共有)。

#[inline]
pub(super) fn px_to_beat(px: f32, lanes_x: f32, lanes_w: f32, view: ArrangementView) -> f64 {
    let beat_per_px = view.len_beats / f64::from(lanes_w.max(1.0));
    view.start_beat + f64::from(px - lanes_x) * beat_per_px
}

/// M14 Phase 63n-5 (#030): lane height drag で raw px (= anchor_h + dy) を `[min, max]` に clamp して
/// `u16` に丸める。 round で整数化 (= drag 中の 0.5 px 揺れで height がカクつかないよう)、 min/max が
/// 逆転していたら max を min 以上に補正 (style 異常入力に対する safety、 panic しない)。
#[inline]
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn clamp_height_px(raw: f32, min: u16, max: u16) -> u16 {
    let lo = f32::from(min);
    let hi = f32::from(max).max(lo);
    raw.round().clamp(lo, hi) as u16
}

/// M14 Phase 63n-6 (#031): lane 高さ drag の **実効 max** = `min(style.max, lanes.h.round())`。
/// 「最大は画面いっぱいまで」 (= lane が描画 pane より高くならない) を runtime clamp で表現。
/// `lanes.h` が style.max を超えても style 値が absolute cap として作用 (= 異常入力 safety)。
/// `lanes.h` が極端に小さい (= overflow scroll 中で pane が 30 px 未満等) 場合は `min_height` 以上に
/// なるよう clamp_height_px 側で補正されるため、 ここでは `lanes.h.round() as u16` を返すだけで OK。
#[inline]
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn effective_lane_max_height(style: &ArrangementStyle, lanes: Rect) -> u16 {
    let style_cap = style.automation_lane_max_height_px;
    let pane_cap = lanes.h.round().max(0.0) as u32; // u16 overflow 防止に u32 経由で min 計算
    style_cap.min(u16::try_from(pane_cap).unwrap_or(u16::MAX))
}

/// M10 Phase 47b: `mouse_x` から band 内の volume 値 (`0.0..=1.0`) を計算。
/// `band_w <= 0` で `0.0` を返す (ガード)。
#[must_use]
pub fn volume_from_mouse_x(mouse_x: f32, band_x: f32, band_w: f32) -> f32 {
    if band_w <= 0.0 {
        return 0.0;
    }
    ((mouse_x - band_x) / band_w).clamp(0.0, 1.0)
}

/// M14 Phase 101 (daw_01 #072): track header drag を reorder に昇格させる最小移動量 (px)。
/// これ未満は click (= SelectTrack) 扱い。 pending_drop (commit) と reorder_overlay (描画) が
/// **同じ閾値**を使うことで preview と commit の発火条件が一致する。
pub(super) const REORDER_DRAG_THRESHOLD_PX: f32 = 16.0;

/// clip press を「click か drag か」で分ける jitter slop (px、 manhattan `|dx|+|dy|`)。 これ未満の
/// 移動で release すると Move は短 click (= 選択切替) に demote される (run.rs の `demote`)。 mouse
/// jitter を ignore しつつ「ちょっとずらす」 操作は drag として扱う程度の小ささ。 旧実装の 16px 閾値は
/// 過剰で、 release で元位置 (= grid 上) に戻る「grid に飛ぶ」 symptom の主因だった。
pub(super) const CLIP_CLICK_DRAG_SLOP_PX: f32 = 4.0;

/// M14 Phase 127 (daw_01 #105): section resize / create の **sanity floor** 拍 (= 異常入力で len が
/// 0 / 負にならない最小値)。 実用 clamp (隣接帯への食い込み防止 / 重複正規化) は caller の
/// `normalize_sections` が行うので、 widget はこの floor のみ (既存「widget は snap + sanity floor、
/// 実 clamp は caller」 規約どおり)。
pub(super) const SECTION_MIN_LEN_BEATS: f64 = 1.0 / 16.0;

/// M14 Phase 101 (daw_01 #072): track header drag&drop の **drop 解決結果**。
/// `pending_drop` (実適用 = `SetTrackParent` 発行) と `reorder_overlay` (描画プレビュー) が
/// **同一の** `resolve_track_drop` を通して得る単一真実源。 これにより「プレビューと実結果が
/// 食い違う」 (旧 blank-drop の症状) が構造的に起き得ない。
///
/// 設計 (daw_01 docs/plan_group_track.md §8.4 改訂版): **Y で gap (挿入行間)、 X でネスト深さ** を
/// 決める。 可視行 R(=above) と R+1(=below) の間の gap では合法深さが連続区間 `[min_d, max_d]` に
/// なり、 各深さ `d` が一意の `(parent, anchor_after)` に対応する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ReorderDrop {
    /// 挿入する gap (visible 行間) の index、 `0..=visible_tracks.len()`。 indicator の Y に使う
    /// (`tops[gap]`)。 gap g は visible 行 `g-1` (above) と `g` (below) の間。
    pub(super) gap: usize,
    /// 選択された nest 深さ (`[min_d, max_d]` に clamp 済)。 indicator 線の左 indent
    /// (`header_left + depth * indent_px`) に使う。
    pub(super) depth: u8,
    /// reparent 先 group の id (`None` = top-level)。 `SetTrackParent.parent`。 `depth > 0` のとき
    /// 必ず group container なので indicator の group-header hilight 対象でもある。
    pub(super) parent: Option<u32>,
    /// `SetTrackParent.anchor_after` — full `tracks` Vec 上で source 群を挿入する直前 track id
    /// (`None` = 先頭)。 **source 自身は除外** する (caller は source を remove してから anchor_after を
    /// 探すため、 anchor が source だと見つからず末尾 append してしまう罠を回避)。 gap の full-Vec
    /// 挿入位置 (= below の full index、 なければ末尾) の直前にある最初の非 source track。
    pub(super) anchor_after: Option<u32>,
}

/// M14 Phase 101 (daw_01 #072): reorder drag の描画プレビューに必要な geometry (すべて screen px、
/// `resolve_track_drop` の結果から事前計算)。 描画 closure はこれを読むだけ (press_tops 等を closure に
/// capture しないで済む)。 indicator 線・深さインデント・group header hilight が **commit と同じ
/// 解決結果**から導出されるので「プレビューと実結果がズレる」 ことが構造的に起きない。
#[derive(Clone, Copy, Debug)]
pub(super) struct ReorderOverlay {
    /// drop indicator 横線の Y (= gap の screen top、 `press_tops[gap]`)。
    pub(super) indicator_y: f32,
    /// indicator 線の **左端 X** (= `header_left + depth * indent_px`)。 線の indent 量が深さ
    /// プレビューそのもの (flush-left = top-level、 1 段右 = その group の子)。
    pub(super) indent_x: f32,
    /// drag 中の半透明 ghost row の中心 Y (= `last_mouse_y`)。
    pub(super) drag_center_y: f32,
    /// reparent 先 parent が group のとき、 hilight する group header の row rect (Cubase の
    /// 緑矢印に相当する肯定フィードバック)。 top-level drop (`parent == None`) では `None`。
    pub(super) highlight_row: Option<Rect>,
}

/// M14 Phase 101 (daw_01 #072): `mouse_y` を visible 行間の **gap index** (`0..=N`) に写像する。
/// `tops` は `visible_track_row_tops` の出力 (len = `N+1`、 単調増加、 lane 込みの prefix sum)。
/// row R 内では中央線より上で gap=R (R の前)、 下で gap=R+1 (R の後)。 最上端より上で 0、
/// 最下端 (`tops[N]`) 以下で N (= 末尾 = 「一番下へ」)。 可変行高 (lane 展開) に追従する。
pub(super) fn gap_from_y(tops: &[f32], mouse_y: f32) -> usize {
    let n = tops.len().saturating_sub(1); // 行数
    if n == 0 {
        return 0;
    }
    if mouse_y < tops[0] {
        return 0;
    }
    if mouse_y >= tops[n] {
        return n;
    }
    // tops[r] <= mouse_y < tops[r+1] となる行 r。 partition_point = 「<= の個数」 = r+1。
    let r = tops.partition_point(|&t| t <= mouse_y).saturating_sub(1).min(n - 1);
    let mid = (tops[r] + tops[r + 1]) * 0.5;
    if mouse_y < mid {
        r
    } else {
        r + 1
    }
}

/// M14 Phase 101 (daw_01 #072): `start` から `parent_id` chain を上へ辿り、 `depth == target_depth`
/// の祖先 id を返す。 `target_depth == start.depth` なら `start` 自身 (= group の最初の子として nest
/// するケース)。 `target_depth > start.depth` は `None` (上へ辿っても深くはなれない)。 hop 上限は
/// `depth: u8` の全域 (= 最大 255 段) を覆う 256 + cycle 防御 (循環参照は 256 hop で打ち切り None)。
pub(super) fn ancestor_at_depth(
    start: &ArrangementTrack,
    target_depth: u8,
    tracks: &[ArrangementTrack],
) -> Option<u32> {
    let mut cur = start;
    for _ in 0..256 {
        if cur.depth <= target_depth {
            return (cur.depth == target_depth).then_some(cur.id);
        }
        let pid = cur.parent_id?;
        cur = tracks.iter().find(|t| t.id == pid)?;
    }
    None
}

/// M14 Phase 101 (daw_01 #072): track header drag&drop の drop 解決 (純関数、 preview = commit の SSoT)。
///
/// - `tracks`: caller の **full** track Vec (master 含まず、 子は親直後の preorder 連続ブロック前提)。
/// - `visible_tracks`: collapsed 親配下を skip した可視列 (先頭に synthetic master があり得る)。
/// - `tops`: `visible_tracks` の prefix-sum row tops (len = visible+1)。
/// - `is_group_set`: 子を持つ track id 集合 (= group container)。
/// - `source`: drag 中の track id slice (anchor_after / parent 計算で除外)。 通常 1〜数件なので
///   slice の線形 `contains` で十分 (drag 中毎フレーム呼ぶため HashSet を alloc しない)。
/// - `indent_px`: 深さ 1 段の幅 (X→深さ写像の単位)。
/// - `mouse_y` / `mouse_x`: drag 中の最終 pointer。 `anchor_mouse_x`: 掴んだ瞬間の x (深さ基準列)。
///   深さは `mouse_x - anchor_mouse_x` の **相対** 列量で決める (絶対 x や header 左端には依存しない
///   = どこを掴んでも「右へ動かすと nest」 が成立する)。
///
/// 戻り値 `ReorderDrop` の `(parent, anchor_after)` をそのまま `SetTrackParent` に乗せれば、
/// 「Y で行・ X で深さ」 が確定する。 `gap` / `depth` は indicator 描画に使う。
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_track_drop(
    tracks: &[ArrangementTrack],
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    is_group_set: &HashSet<u32>,
    source: &[u32],
    indent_px: f32,
    mouse_y: f32,
    mouse_x: f32,
    anchor_mouse_x: f32,
) -> ReorderDrop {
    // gap_from_y は契約上 [0, n] を返すので追加 clamp は不要。
    let gap = gap_from_y(tops, mouse_y);
    let above: Option<&ArrangementTrack> =
        gap.checked_sub(1).and_then(|i| visible_tracks.get(i));
    let below: Option<&ArrangementTrack> = visible_tracks.get(gap);

    // 合法 nest 深さ区間 [min_d, max_d]:
    //  - max_d = depth(above) + (above が group なら 1) — above の子まで潜れる / above と sibling。
    //  - min_d = depth(below) — below の深さまで浅くできる (= 囲う group を抜ける)。 末尾は 0。
    // preorder 不変条件下で min_d <= max_d は保証されるが、 異常入力に備え min を max に clamp。
    let max_d = above.map_or(0, |a| {
        a.depth.saturating_add(u8::from(is_group_set.contains(&a.id)))
    });
    let min_d = below.map_or(0, |b| b.depth).min(max_d);

    // X → 深さ。 anchor 相対の列 offset を base=min_d (境界モデル default) に加算して区間 clamp。
    // 右へ動かすほど深く nest、 動かさなければ min_d (= メンバー間は内側 / 最終メンバー下は浅い側)。
    let indent_unit = indent_px.max(1.0);
    #[allow(clippy::cast_possible_truncation)]
    let col_offset = ((mouse_x - anchor_mouse_x) / indent_unit).round() as i32;
    let depth = (i32::from(min_d) + col_offset)
        .clamp(i32::from(min_d), i32::from(max_d))
        .max(0) as u8;

    // parent = above の depth-1 祖先 (depth==0 → top-level None)。
    let parent = if depth == 0 {
        None
    } else {
        above.and_then(|a| ancestor_at_depth(a, depth - 1, tracks))
    };
    // **parent が source 自身になる cycle を防ぐ** (= 自分を自分の子にする / multi-select で moving 中の
    // 祖先を親にする)。 例: expanded group G を G ヘッダ直下の gap へ drag すると above=G・唯一の合法深さ
    // depth(G)+1 で parent=G=source になる。 daw_01 の SetTrackParent 直接適用は cycle 検証を通らない
    // (parent_group_id を直書きする) ので widget 側で source を親にしない不変を保証する。 source に当たったら
    // 最近接の **非 source 祖先** へ繰り上げる (全祖先が source なら top-level)。
    let mut parent = parent;
    while let Some(pid) = parent {
        if source.contains(&pid) {
            parent = tracks.iter().find(|t| t.id == pid).and_then(|t| t.parent_id);
        } else {
            break;
        }
    }

    // anchor_after = gap の full-Vec 挿入位置 ins の直前にある最初の非 source track (None = 先頭)。
    // ins: below の full index (= below の直前に挿入)。 below 無し (末尾 gap) は tracks.len()。
    // below が master (= synthetic、 full Vec に居ない) のときは ins=0 (= 先頭、 master は song.tracks 外)。
    // 通常 track は必ず full Vec に在る (visible_tracks は tracks ∪ {master} から作る) ので、 position が
    // None になるのは master のみ。 master を明示分岐して「正常 track が欠落したら 0」 の曖昧さを排除する。
    let ins = match below {
        None => tracks.len(),
        Some(b) if b.id == MASTER_TRACK_ID => 0,
        Some(b) => tracks.iter().position(|t| t.id == b.id).unwrap_or(0),
    };
    let anchor_after = tracks[..ins.min(tracks.len())]
        .iter()
        .rev()
        .find(|t| !source.contains(&t.id))
        .map(|t| t.id);

    ReorderDrop { gap, depth, parent, anchor_after }
}

/// track header 1 行内のレイアウト (Name button + 3 small buttons + 任意の volume band + lane disclosure)。
/// `name_rect` (= drag start zone & text area)、`buttons` (= [M, S, R]、Phase 68 で R button = Record-arm 追加。
/// Phase 47c で ↑/↓/× は drag&drop + Delete shortcut に置換され削除済)、`volume_band` は inner 下部に band 用の
/// 余裕がある時のみ `Some` (Phase 47b)、`lane_disc_rect` は M14 Phase 63n-2 で R button の **右** に予約された
/// lane disclosure (`+`/`-` icon) 用の rect (track_row 全体の右端、 automation_lanes が空でも常に layout に
/// 含めて名前領域を一定にする)。
pub(super) struct HeaderRowLayout {
    pub(super) name_rect: Rect,
    pub(super) buttons: [Rect; 3],
    /// M10 Phase 47b: track volume band rect (`row_h` 余裕がある時のみ Some)。
    pub(super) volume_band: Option<Rect>,
    /// M14 Phase 63n-2 (#028): lane disclosure (`+`/`-` toggle) の hit zone + 描画 rect。
    /// `automation_lanes` が空の track でも layout 上の幅は確保 (= 名前領域が track 間で一定)、
    /// 空 lane の track では描画されないが click も反応しない (caller が `lanes.is_empty()` で判定)。
    pub(super) lane_disc_rect: Rect,
}

#[allow(clippy::similar_names)]
pub(super) fn header_row_layout(row: Rect, volume_band_h: f32) -> HeaderRowLayout {
    let pad = 4.0_f32;
    let inner = Rect {
        x: row.x + pad,
        y: row.y + pad,
        w: (row.w - pad * 2.0).max(2.0),
        h: (row.h - pad * 2.0).max(2.0),
    };
    // buttons は常に 20px max (band の有無で縮めない)。band は inner.h に余裕があるときだけ表示する。
    let btn_h = inner.h.min(20.0);
    let small = 22.0_f32;
    let gap = 2.0_f32;
    // Phase 68 (#040): M + S + R の 3 button (← Phase 47c の M + S 2 button 構成から R = Record-arm を追加)。
    // 並び順は業界標準の M / S / R (Bitwig / Live / Reaper と同じ、 左→右)。
    let n_btn = 3;
    // M14 Phase 63n-2 (#028): lane disclosure 用の幅を予約 (= disc_size + gap)。 R button の右に
    // 配置するため `total_right` に加算 → name_rect が縮む代わりに lane_disc が button と重ならない。
    let lane_disc_size = 12.0_f32;
    let lane_disc_extra = lane_disc_size + gap;
    #[allow(clippy::cast_precision_loss)]
    let total_right = small * n_btn as f32 + gap * n_btn as f32 + lane_disc_extra;
    let name_w = (inner.w - total_right).max(20.0);
    let name_rect = Rect { x: inner.x, y: inner.y, w: name_w, h: btn_h };
    let mut x_cursor = inner.x + name_w + gap;
    let mut buttons = [Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }; 3];
    for slot in &mut buttons {
        *slot = Rect { x: x_cursor, y: inner.y, w: small, h: btn_h };
        x_cursor += small + gap;
    }
    // S button の右に lane_disc rect (= ASCII `+`/`-` icon)。 行 vertical center に揃える。
    let lane_disc_rect = Rect {
        x: x_cursor,
        y: inner.y + (btn_h - lane_disc_size).max(0.0) * 0.5,
        w: lane_disc_size,
        h: lane_disc_size,
    };
    // band 表示条件: band_h > 0 && buttons の下に gap + band 分が収まる (progressive disclosure)。
    // default (`track_volume_band_h=4` / `gap=2`) なら inner.h >= 26 (= row_h >= 34) で表示。
    let band_h = volume_band_h.max(0.0);
    let band_gap = 2.0_f32;
    let volume_band = if band_h > 0.0 && btn_h + band_gap + band_h <= inner.h {
        Some(Rect {
            x: inner.x,
            y: inner.y + btn_h + band_gap,
            w: inner.w,
            h: band_h,
        })
    } else {
        None
    };
    HeaderRowLayout { name_rect, buttons, volume_band, lane_disc_rect }
}

/// M14 Phase 63c (#016): disclosure ▼ / ▶ アイコンの hit / 描画 rect。
/// `name_rect` の左端から `disclosure_w` 幅で切り出し、 indent 量 (`depth * indent_px`) は **既に
/// `name_rect.x` に反映されている前提** (caller 側の指定)。 group track でない場合は呼ばない (caller が判定)。
/// rect は `name_rect.h` を超えない正方形に近い (アイコン center 用)。
pub(super) fn disclosure_rect_for(name_rect: Rect, style: &ArrangementStyle, _depth: u8) -> Rect {
    // disclosure 幅は indent_px と同じ (= 1 段ぶんの幅)、 name_rect の左端から削り取る。
    let w = style.indent_px.max(8.0);
    let h = name_rect.h.min(w);
    Rect {
        x: name_rect.x,
        y: name_rect.y + (name_rect.h - h) * 0.5,
        w,
        h,
    }
}

/// M14 Phase 63n-2 (#028): lane header の icon / band の rect 一式 (描画 + hit-test の SSoT)。
#[derive(Clone, Copy, Debug)]
pub struct AutomationLaneHeaderLayout {
    /// `★`/`☆` icon (lane.enabled 切替用、 click で `SetLaneEnabled`)。
    pub enabled_icon_rect: Rect,
    /// `[V]` icon (lane.icon_glyph、 click 機能なし = visual only)。
    pub icon_glyph_rect: Rect,
    /// `👁` icon (lane.visible 切替用、 click で `SetLaneVisible`)。
    pub visible_icon_rect: Rect,
    /// `▣` icon (mute、 Phase 63n-2 では描画のみで click 機能なし)。
    pub mute_icon_rect: Rect,
    /// `✕` icon (lane 削除、 click で `DeleteLane`)。
    pub delete_icon_rect: Rect,
    /// default value の **数値入力フィールド** rect (旧 horizontal slider 帯を置換)。
    /// caller (daw_01) がここに `scrubable_number_at` を overlay して default 値を編集する。
    /// header 行高が icon 行 + フィールドを載せられない場合は `None` (= 極狭 lane では非表示)。
    pub default_field_rect: Option<Rect>,
}

/// M14 Phase 63n-2 (#028): lane header rect から icon / band の sub-rect 群を計算。
/// `draw_automation_lane` と完全同一の配置式 (描画と hit の SSoT)。 widget 内部 hit-test と
/// 外部 test の両方で使うため `pub`。
#[must_use]
pub fn automation_lane_header_layout(
    header_rect: Rect,
    style: &ArrangementStyle,
) -> Option<AutomationLaneHeaderLayout> {
    if header_rect.w < style.automation_lane_header_min_w_px {
        return None;
    }
    let pad = 4.0_f32;
    let icon_size = style.automation_lane_icon_size.max(4.0);
    let cx = header_rect.x + pad;
    let cy = header_rect.y + (header_rect.h - icon_size).max(0.0) * 0.5;
    let enabled_icon_rect = Rect { x: cx, y: cy, w: icon_size, h: icon_size };
    let icon_glyph_rect = Rect {
        x: cx + icon_size + pad,
        y: cy,
        w: icon_size,
        h: icon_size,
    };
    // 右寄せ: ✕ → ▣ → 👁 の順で右から左へ配置 (描画ループ `icons.iter().rev()` と同じ式)。
    let step = icon_size + pad * 0.5;
    let delete_x = header_rect.x + header_rect.w - pad - step;
    let mute_x = delete_x - step;
    let visible_x = mute_x - step;
    let visible_icon_rect = Rect { x: visible_x, y: cy, w: icon_size, h: icon_size };
    let mute_icon_rect = Rect { x: mute_x, y: cy, w: icon_size, h: icon_size };
    let delete_icon_rect = Rect { x: delete_x, y: cy, w: icon_size, h: icon_size };

    // default value 数値入力フィールド (旧スライダー帯を置換)。 caller が
    // scrubable_number_at を overlay できる読める高さ。 header 行下端から pad だけ上、
    // icon 行 (cy + icon_size) より下にフィールドが収まるなら Some。
    let field_h = style.automation_default_field_h;
    let field_y = header_rect.y + header_rect.h - field_h - pad;
    let field_x = cx;
    let field_w = (header_rect.w - pad * 2.0).max(0.0);
    let default_field_rect = if field_h > 0.0 && field_w > 0.0 && field_y >= cy + icon_size {
        Some(Rect { x: field_x, y: field_y, w: field_w, h: field_h })
    } else {
        None
    };

    Some(AutomationLaneHeaderLayout {
        enabled_icon_rect,
        icon_glyph_rect,
        visible_icon_rect,
        mute_icon_rect,
        delete_icon_rect,
        default_field_rect,
    })
}

/// M14 Phase 63n-2 (#028): visible track の expanded automation lane を順に visit する pure helper。
/// `header_pane_x` / `header_pane_w` は track header 領域の x 範囲 (= `view.header_w == 0` で
/// header 無し)、 `lanes_x` / `lanes_w` は clip 描画域。 callback には `(track_idx, lane_idx,
/// lane, header_rect, body_rect)` を渡す。 描画 / hit-test / drag press の SSoT (3 箇所が同じ式
/// で同じ lane y 範囲を計算するための共有)。
#[allow(clippy::too_many_arguments)]
pub(super) fn for_each_visible_lane<F>(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    mut f: F,
) where
    F: FnMut(usize, usize, &ArrangementAutomationLane, Rect, Rect),
{
    for (i, t) in visible_tracks.iter().enumerate() {
        if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
            continue;
        }
        let track_row_top = tops[i];
        // M14 Phase 63n-6 (#031): per-track row 高さ override 反映 (lane y 起点 = row_top + effective row_h)。
        let mut lane_y = track_row_top + effective_track_row_h(t, track_row_h);
        let header_indent = f32::from(t.depth) * style.indent_px;
        for (j, lane) in t.automation_lanes.iter().enumerate() {
            if !lane.visible {
                continue;
            }
            let lh = f32::from(lane.height_px);
            let header_rect = Rect {
                x: header_pane_x + header_indent,
                y: lane_y,
                w: (header_pane_w - header_indent).max(2.0),
                h: lh,
            };
            let body_rect = Rect { x: lanes_x, y: lane_y, w: lanes_w, h: lh };
            f(i, j, lane, header_rect, body_rect);
            lane_y += lh;
        }
    }
}

/// M14 Phase 63n-5 (#030): lane 下端 splitter hot zone (= lane bottom edge ±`handle_px` の y range
/// × body x range) に cursor が当たっているか判定。 当たった lane の `AutomationLaneKey` を返す
/// (= cursor 形状切替 + caller のテストで rect 中心 px を導出する用途)。 splitter は body x range のみ
/// — header 側は button / band と排他。 splitter hit > 他の hover priority (cursor は最優先で NsResize)。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn automation_lane_resize_splitter_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> Option<AutomationLaneKey> {
    let handle = style.automation_lane_resize_handle_px;
    if handle <= 0.0 || cx < lanes_x || cx >= lanes_x + lanes_w {
        return None;
    }
    let mut found: Option<AutomationLaneKey> = None;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes_x,
        lanes_w,
        style,
        |i, _j, lane, _h_rect, b_rect| {
            if found.is_some() {
                return;
            }
            let bottom = b_rect.y + b_rect.h;
            if cy >= bottom - handle && cy < bottom {
                found = Some(AutomationLaneKey {
                    track: visible_tracks[i].id,
                    lane: lane.id,
                });
            }
        },
    );
    found
}

/// M14 Phase 63n-6 (#031): track row 下端 splitter hot zone (= row body bottom edge ±`handle_px` の
/// y range × body x range) に cursor が当たっているか判定。 当たった visible track index を返す
/// (= cursor 形状切替 + caller のテストで rect 中心 px を導出する用途)。 row 高さは global なので
/// track index は意味的に「どの行で trigger したか」 のみ示す参考値で、 drag 自体は全 row 一斉。
/// splitter zone は **track row body の最下端 4 px** (= `tops[i] + track_row_h - handle .. + track_row_h`)
/// — 行の下に automation lane がある場合は「最初の lane の上端」 と一致するが、 lane splitter は
/// **lane bottom edge** を見るので排他 (= 別エッジで衝突しない)。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn track_row_resize_splitter_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> Option<usize> {
    let handle = style.automation_lane_resize_handle_px;
    if handle <= 0.0 || track_row_h <= 0.0 || cx < lanes_x || cx >= lanes_x + lanes_w {
        return None;
    }
    for i in 0..visible_tracks.len() {
        if i + 1 >= tops.len() {
            break;
        }
        let t = &visible_tracks[i];
        let row_top = tops[i];
        // M14 Phase 63n-6 (#031): per-track row 高さで row_bottom を計算 (override 済 track の splitter
        // zone がそのトラックの下端に追従)。
        let row_bottom = row_top + effective_track_row_h(t, track_row_h);
        if cy >= row_bottom - handle && cy < row_bottom {
            return Some(i);
        }
    }
    None
}

/// M14 Phase 117 (daw_01 #091): track header 列と lanes の境界 (`arrangement_rect.x + header_w` の縦線)
/// を中心とした header 幅 drag splitter の hot zone に cursor が当たっているか判定。 hot zone は境界
/// `±header_resize_handle_px/2` の横帯 × arrangement 全高 (ruler 行も含む縦線全長)。 `header_w <= 0`
/// (header 無し) / `handle <= 0` で常に `false`。 track header の M/S/R ボタン等とは衝突しない (header の
/// 右端 4px inner pad に splitter の header 側が収まる)。 press 振り分けで lane/row splitter の **後** に
/// 評価する (= 同時成立しうる lanes 左端の角は lane/row resize を優先) ので、 実質 cursor は header の
/// 4px pad 〜 lanes 左端 4px で `EwResize`。
#[must_use]
pub fn header_resize_splitter_at(
    arrangement_rect: Rect,
    header_w: f32,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> bool {
    let handle = style.header_resize_handle_px;
    if handle <= 0.0 || header_w <= 0.0 {
        return false;
    }
    let boundary = arrangement_rect.x + header_w;
    let half = handle * 0.5;
    cx >= boundary - half
        && cx < boundary + half
        && cy >= arrangement_rect.y
        && cy < arrangement_rect.y + arrangement_rect.h
}

/// M14 Phase 63n-2 (#028): lane body 内 cursor 位置から hit する point を返す (後勝ち、 描画順と整合)。
/// 戻り値の `Rect` は popup anchor 用 point dot rect (= `lane_disclosure_rect_for` 同様)。
/// hit zone は **point dot 半径の 2 倍** (= 8px @ default radius=4) で生成、 fingertip 操作の余裕を持たせる。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn automation_point_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    view: ArrangementView,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes: Rect,
    cx: f32,
    cy: f32,
    style: &ArrangementStyle,
) -> Option<(AutomationPointKey, Rect)> {
    if !lanes.contains(cx, cy) {
        return None;
    }
    let radius = style.automation_point_radius_px.max(2.0);
    let hit_r2 = (radius * 2.0).powi(2);
    let mut hit: Option<(AutomationPointKey, Rect)> = None;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes.x,
        lanes.w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            if cy < body_rect.y || cy >= body_rect.y + body_rect.h {
                return;
            }
            let track_id = visible_tracks[t_idx].id;
            let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
            let pad = style.automation_clip_v_pad_px;
            let clip_y = body_rect.y + pad;
            let clip_h = (body_rect.h - pad * 2.0).max(2.0);
            for clip_in in &lane.clips {
                for (p_idx, p) in clip_in.points.iter().enumerate() {
                    let abs_beat = clip_in.start_beat + p.time_beat;
                    #[allow(clippy::cast_possible_truncation)]
                    let px = body_rect.x + ((abs_beat - view.start_beat) * beat_to_px) as f32;
                    let py = clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                    let dx = cx - px;
                    let dy = cy - py;
                    if dx * dx + dy * dy <= hit_r2 {
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
                        hit = Some((key, r));
                    }
                }
            }
        },
    );
    hit
}

/// M14 Phase 63n-2 (#028): lane body 内 cursor から該当する `(track_idx, lane_idx, header_rect,
/// body_rect)` を返す。 `for_each_visible_lane` を 1 度走らせて y 範囲が合う最初の lane を採用
/// (lane 群は y で disjoint なので「最初」 = 「唯一」)。 cursor が lane 内にいるかどうかの判定で
/// header_rect / body_rect 共通の y 範囲だけを見る (x は header / body 跨ぎでも lane 1 つ)。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn automation_lane_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    cy: f32,
) -> Option<(usize, usize, Rect, Rect)> {
    let mut found: Option<(usize, usize, Rect, Rect)> = None;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes_x,
        lanes_w,
        style,
        |t_idx, l_idx, _lane, h_rect, b_rect| {
            if found.is_some() {
                return;
            }
            if cy >= h_rect.y && cy < h_rect.y + h_rect.h {
                found = Some((t_idx, l_idx, h_rect, b_rect));
            }
        },
    );
    found
}

/// M14 Phase 63n-2 (#028): visible track 群から `(track_id, lane_id, clip_id)` 三つ組で対応する
/// `(lane, clip)` 参照を取得する pure helper。 press / release で何度も lookup するため、 関数化
/// しないと if let 連鎖が press block を膨らませる。
#[must_use]
pub(super) fn find_lane_clip(
    visible_tracks: &[ArrangementTrack],
    key: AutomationClipKey,
) -> Option<(&ArrangementAutomationLane, &ArrangementAutomationClip)> {
    let track = visible_tracks.iter().find(|t| t.id == key.track)?;
    let lane = track.automation_lanes.iter().find(|l| l.id == key.lane)?;
    let clip = lane.clips.iter().find(|c| c.id == key.clip)?;
    Some((lane, clip))
}

/// M14 Phase 63n-9 (#033): S 字 cubic Bezier (制御点 x=(1/3, 2/3) 固定) の y(t) を評価。
/// `flatten_lane_segment` の Bezier 分岐と同 SSoT。 t=0 で a、 t=1 で b、 tension=0 で線形等価、
/// `tension=+1.0` で c1y=a, c2y=b (滑らかな S 字)、 `tension=-1.0` で c1y=b, c2y=a (overshoot 反転)。
#[must_use]
pub(super) fn evaluate_bezier_y(a: f32, b: f32, tension: f32, t: f32) -> f32 {
    let t_clamped = tension.clamp(-1.0, 1.0);
    let diag1 = a + (b - a) * (1.0 / 3.0);
    let diag2 = a + (b - a) * (2.0 / 3.0);
    let mix = t_clamped.abs();
    let (target1, target2) = if t_clamped >= 0.0 { (a, b) } else { (b, a) };
    let c1y = diag1 * (1.0 - mix) + target1 * mix;
    let c2y = diag2 * (1.0 - mix) + target2 * mix;
    let omt = 1.0 - t;
    omt.powi(3) * a + 3.0 * omt.powi(2) * t * c1y + 3.0 * omt * t.powi(2) * c2y + t.powi(3) * b
}

/// M14 Phase 63n-9 (#033): tension/bend handle の screen 座標を計算。
/// `(prev_x, prev_y)` と `(cur_x, cur_y)` は curve 端点の screen 座標、 `kind` + `param_value` で
/// segment 中央 (= t=0.5) の y を curve 評価値から算出。 handle は curve から上方向に `offset_px`
/// 飛び出させて click target を curve 線 (1.5px) と分離。 daw_01 #033 §B Q3=A 仕様。
#[must_use]
pub(super) fn compute_curve_handle_pos(
    prev_x: f32,
    prev_y: f32,
    cur_x: f32,
    cur_y: f32,
    kind: SetAutomationCurveParamKind,
    param_value: f32,
    offset_px: f32,
) -> (f32, f32) {
    let x = (prev_x + cur_x) * 0.5;
    let mid_y = match kind {
        SetAutomationCurveParamKind::BezierTension => {
            evaluate_bezier_y(prev_y, cur_y, param_value, 0.5)
        }
        SetAutomationCurveParamKind::ExponentialBend => {
            let exponent = 2.0_f32.powf(param_value.clamp(-1.0, 1.0));
            prev_y + (cur_y - prev_y) * (0.5_f32).powf(exponent)
        }
    };
    (x, mid_y - offset_px)
}

/// M14 Phase 63n-9 (#033): cursor 座標から hit する curve param handle を返す。 `selected_points`
/// に含まれる point の **Bezier / Exponential 入射 segment** にのみ handle が存在 (= Hold / Linear は
/// handle なし、 first point (= idx 0) も入射 segment なしで除外)。 hit zone は handle の **半径 2 倍**
/// (= 8px @ default radius=4)、 描画と同 SSoT (`compute_curve_handle_pos`)。
/// 戻り値: `(point_key, kind, current_value, lane_height_px)` — current_value は drag session の
/// `anchor_value`、 lane_height_px は sensitivity 計算用 (`effective_lane_height_px` = max(_, 40))。
#[must_use]
#[allow(clippy::too_many_arguments)]
pub(super) fn find_curve_param_handle_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    selected_points: &[AutomationPointKey],
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> Option<(AutomationPointKey, SetAutomationCurveParamKind, f32, u16)> {
    if selected_points.is_empty() {
        return None;
    }
    let handle_r = style.automation_curve_param_handle_radius_px.max(2.0);
    let hit_r_sq = (handle_r * 2.0).powi(2);
    let offset = style.automation_curve_param_handle_offset_px;
    let pad = style.automation_clip_v_pad_px;
    let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
    for (i, t) in visible_tracks.iter().enumerate() {
        if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
            continue;
        }
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        let mut lane_y = row_top + row_h;
        for lane in &t.automation_lanes {
            if !lane.visible {
                continue;
            }
            let lh = f32::from(lane.height_px);
            let clip_y = lane_y + pad;
            let clip_h = (lh - pad * 2.0).max(2.0);
            for c in &lane.clips {
                for p_idx in 1..c.points.len() {
                    let key = AutomationPointKey {
                        clip: AutomationClipKey {
                            track: t.id,
                            lane: lane.id,
                            clip: c.id,
                        },
                        #[allow(clippy::cast_possible_truncation)]
                        point_idx: p_idx as u32,
                    };
                    if !selected_points.contains(&key) {
                        continue;
                    }
                    let p = &c.points[p_idx];
                    let (kind, value) = match p.curve {
                        ArrangementCurveKind::Bezier { tension } => {
                            (SetAutomationCurveParamKind::BezierTension, tension)
                        }
                        ArrangementCurveKind::Exponential { bend } => {
                            (SetAutomationCurveParamKind::ExponentialBend, bend)
                        }
                        _ => continue, // Hold / Linear: handle なし
                    };
                    let prev = &c.points[p_idx - 1];
                    let prev_abs = c.start_beat + prev.time_beat;
                    let cur_abs = c.start_beat + p.time_beat;
                    #[allow(clippy::cast_possible_truncation)]
                    let prev_x = lanes.x + ((prev_abs - view.start_beat) * beat_to_px) as f32;
                    #[allow(clippy::cast_possible_truncation)]
                    let cur_x = lanes.x + ((cur_abs - view.start_beat) * beat_to_px) as f32;
                    let prev_y =
                        clip_y + (1.0 - prev.value_norm.clamp(0.0, 1.0)) * clip_h;
                    let cur_y =
                        clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                    let (hx, hy) = compute_curve_handle_pos(
                        prev_x, prev_y, cur_x, cur_y, kind, value, offset,
                    );
                    let dx = cx - hx;
                    let dy = cy - hy;
                    if dx * dx + dy * dy <= hit_r_sq {
                        return Some((key, kind, value, lane.height_px));
                    }
                }
            }
            lane_y += lh;
        }
    }
    None
}

/// M14 Phase 63n-9 (#033): handle drag の sensitivity 計算。 dy → value delta。
/// Q3=A 仕様: `effective_lane_height = max(lane_height_px, 40)` drag で full range (`-2.0`)、 つまり
/// `1 px = 2.0 / effective_h` の value delta。 Alt 押下で × 0.2 (= 5x 精細)。 y は screen 軸で上が
/// 負なので上 drag = + value (符号反転)。
#[must_use]
pub(super) fn curve_param_delta_from_dy(dy: f32, effective_h: f32, alt: bool) -> f32 {
    let raw = -dy * 2.0 / effective_h.max(1.0);
    if alt { raw * 0.2 } else { raw }
}

/// M14 Phase 63n-8 (#033): point key から `(time_beat, value_norm, clip_start, clip_len)` を取得。
/// multi-select drag の release commit で各 selected point の anchor を再 lookup するために使う
/// (drag 中は Edit が流れないので model 不変、 visible_tracks がそのまま使える前提)。
#[must_use]
pub(super) fn find_automation_point_data(
    visible_tracks: &[ArrangementTrack],
    key: AutomationPointKey,
) -> Option<(f64, f32, f64, f64)> {
    let track = visible_tracks.iter().find(|t| t.id == key.clip.track)?;
    let lane = track.automation_lanes.iter().find(|l| l.id == key.clip.lane)?;
    let clip = lane.clips.iter().find(|c| c.id == key.clip.clip)?;
    let p = clip.points.get(key.point_idx as usize)?;
    Some((p.time_beat, p.value_norm, clip.start_beat, clip.len_beats))
}

/// r.md #35: Shift+click 範囲選択用に、 `clip` が属する automation clip の全 point を
/// **時間順 (= `point_idx` 順)** に並べた key 列を返す。 automation point は 1 つの clip 内で
/// 時間順に一意なので、 clip / note のような 2 次元ブロックではなく 1 次元の順序範囲でよい
/// (値軸は時間で決まる)。 clip が見つからなければ空 vec。
#[must_use]
pub(super) fn automation_point_order(
    visible_tracks: &[ArrangementTrack],
    clip: AutomationClipKey,
) -> Vec<AutomationPointKey> {
    let Some(track) = visible_tracks.iter().find(|t| t.id == clip.track) else {
        return Vec::new();
    };
    let Some(lane) = track.automation_lanes.iter().find(|l| l.id == clip.lane) else {
        return Vec::new();
    };
    let Some(c) = lane.clips.iter().find(|c| c.id == clip.clip) else {
        return Vec::new();
    };
    (0..c.points.len())
        .map(|i| AutomationPointKey { clip, point_idx: i as u32 })
        .collect()
}

/// r.md #35: Shift+click 範囲選択用に、 可視 automation lane 上の全 automation clip を
/// 「行 = 可視 lane 通し番号 / 時間 = clip の開始〜終了拍」 として並べる。 並び順は描画順
/// (track → lane → clip)。 MIDI clip の `clip_range_items` と同じ考え方。
#[must_use]
pub(super) fn automation_clip_range_items(
    visible_tracks: &[ArrangementTrack],
) -> Vec<RangeItem<AutomationClipKey>> {
    let mut out = Vec::new();
    let mut row: i64 = 0;
    for t in visible_tracks {
        for l in &t.automation_lanes {
            for c in &l.clips {
                out.push(RangeItem {
                    key: AutomationClipKey { track: t.id, lane: l.id, clip: c.id },
                    row,
                    start: c.start_beat,
                    end: c.start_beat + c.len_beats,
                });
            }
            row += 1;
        }
    }
    out
}

/// M14 Phase 63n-8 (#033): lasso rect 内に **中心が含まれる** visible automation point を集める。
/// visible_tracks scope (collapsed track / `automation_lanes_collapsed=true` の lane 群 / `lane.visible=false`
/// の lane は除外)、 既存 `automation_point_at` の hit-test scope と整合。 点中心 (= `(px, py)`) は
/// 描画と同 SSoT (`body_origin_x + (abs_beat - view.start_beat) * beat_to_px`、 `clip_y + (1 - value) * clip_h`)。
#[must_use]
pub(super) fn collect_points_in_rect(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    rect: Rect,
    style: &ArrangementStyle,
) -> Vec<AutomationPointKey> {
    // 描画と同じ縦 padding (`draw_automation_lane` / `automation_point_at` と同じく
    // `style.automation_clip_v_pad_px` を参照。 定数 fork だと style 変更時に
    // lasso の点中心座標だけ描画とずれる、 review)。
    let pad: f32 = style.automation_clip_v_pad_px;
    let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
    let mut out: Vec<AutomationPointKey> = Vec::new();
    for (i, t) in visible_tracks.iter().enumerate() {
        if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
            continue;
        }
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        let mut lane_y = row_top + row_h;
        for lane in &t.automation_lanes {
            if !lane.visible {
                continue;
            }
            let lh = f32::from(lane.height_px);
            // 描画と同じ縦 padding 適用 (`draw_automation_lane` SSoT)。
            let clip_y = lane_y + pad;
            let clip_h = (lh - pad * 2.0).max(2.0);
            for c in &lane.clips {
                for (p_idx, p) in c.points.iter().enumerate() {
                    let abs_beat = c.start_beat + p.time_beat;
                    #[allow(clippy::cast_possible_truncation)]
                    let px = lanes.x + ((abs_beat - view.start_beat) * beat_to_px) as f32;
                    let py = clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                    if rect.contains(px, py) {
                        out.push(AutomationPointKey {
                            clip: AutomationClipKey {
                                track: t.id,
                                lane: lane.id,
                                clip: c.id,
                            },
                            point_idx: p_idx as u32,
                        });
                    }
                }
            }
            lane_y += lh;
        }
    }
    out
}

/// daw_01 #071: lasso rect と交差する automation clip を集める (`collect_points_in_rect` の clip 版)。
/// `for_each_visible_lane` で body_rect を取り、 描画 / hit-test と同じ clip rect 式 (縦 padding 適用済)
/// で `rects_intersect` 判定する。 collapsed / invisible lane は `for_each_visible_lane` が除外済。
#[allow(clippy::too_many_arguments)]
pub(super) fn collect_clips_in_rect(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    view: ArrangementView,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes: Rect,
    style: &ArrangementStyle,
    rect: Rect,
) -> Vec<AutomationClipKey> {
    let mut out: Vec<AutomationClipKey> = Vec::new();
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes.x,
        lanes.w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            let track_id = visible_tracks[t_idx].id;
            let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
            let pad = style.automation_clip_v_pad_px;
            let clip_y = body_rect.y + pad;
            let clip_h = (body_rect.h - pad * 2.0).max(2.0);
            for clip in &lane.clips {
                #[allow(clippy::cast_possible_truncation)]
                let cx_clip =
                    body_rect.x + ((clip.start_beat - view.start_beat) * beat_to_px) as f32;
                #[allow(clippy::cast_possible_truncation)]
                let cw = ((clip.len_beats * beat_to_px) as f32).max(2.0);
                let r = Rect { x: cx_clip, y: clip_y, w: cw, h: clip_h };
                if rects_intersect(r, rect) {
                    out.push(AutomationClipKey {
                        track: track_id,
                        lane: lane.id,
                        clip: clip.id,
                    });
                }
            }
        },
    );
    out
}

/// daw_01 #071: 指定 `keys` の automation clip 群を drag anchor に変換する (MIDI clip の anchor 構築の
/// automation 版)。 `for_each_visible_lane` で各 clip の lane body_rect を取り、 戻りは `keys` 順
/// (= grabbed-first を保つ)。 visible でない / 見つからない key は skip。
#[allow(clippy::too_many_arguments)]
pub(super) fn collect_automation_clip_anchors(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    keys: &[AutomationClipKey],
) -> Vec<AutomationClipDragAnchor> {
    let mut found: Vec<AutomationClipDragAnchor> = Vec::new();
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes_x,
        lanes_w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            let track_id = visible_tracks[t_idx].id;
            for clip in &lane.clips {
                let key = AutomationClipKey { track: track_id, lane: lane.id, clip: clip.id };
                if keys.contains(&key) {
                    found.push(AutomationClipDragAnchor {
                        key,
                        start_beat: clip.start_beat,
                        len_beats: clip.len_beats,
                        lane: key.lane_key(),
                        body_rect,
                    });
                }
            }
        },
    );
    keys.iter()
        .filter_map(|k| found.iter().find(|a| a.key == *k).copied())
        .collect()
}

/// M14 Phase 63n-2 (#028): lane body 内 cursor から hit する automation clip を返す。
/// 戻り値: `(clip_key, clip_local_time_beat, value_norm)` (clip_local_time_beat は clip start から
/// のオフセット拍、 value_norm は cy 座標から逆算した `0.0..=1.0`)。 cursor が clip ギャップ内なら
/// `None` (空き click では空気穴 = caller 側で `AddAutomationPoint` 発行しない)。
pub(super) fn automation_clip_at(
    track_id: u32,
    lane: &ArrangementAutomationLane,
    body_rect: Rect,
    view: ArrangementView,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> Option<(AutomationClipKey, f64, f32)> {
    let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
    let pad = style.automation_clip_v_pad_px;
    let clip_y = body_rect.y + pad;
    let clip_h = (body_rect.h - pad * 2.0).max(2.0);
    if cy < clip_y || cy >= clip_y + clip_h {
        return None;
    }
    for clip in &lane.clips {
        #[allow(clippy::cast_possible_truncation)]
        let cx_clip = body_rect.x + ((clip.start_beat - view.start_beat) * beat_to_px) as f32;
        #[allow(clippy::cast_possible_truncation)]
        let cw = ((clip.len_beats * beat_to_px) as f32).max(2.0);
        if cx >= cx_clip && cx < cx_clip + cw {
            let abs_beat = view.start_beat + f64::from(cx - body_rect.x) / beat_to_px;
            let local = (abs_beat - clip.start_beat).clamp(0.0, clip.len_beats);
            let value_norm = (1.0 - (cy - clip_y) / clip_h).clamp(0.0, 1.0);
            let key = AutomationClipKey {
                track: track_id,
                lane: lane.id,
                clip: clip.id,
            };
            return Some((key, local, value_norm));
        }
    }
    None
}

/// M14 Phase 63n-3 (#028): lane body 内の automation clip 上で hit する
/// `(AutomationClipKey, ClipDragKind, clip_rect, body_rect)` を返す。
/// `clip_zone_at` と完全同 仕様: clip rect 左右 edge から内外 ±`edge` px が Resize、 内側中央が Move、
/// 短 clip (`r.w <= edge * 2`) は rect 内全 Move (rect 外側のみ resize)。 隣接 clip の共有境界は
/// `clip_hit` / `section_hit` と同じ 2-tier in-rect 優先 (同 tier は edge 距離、 同距離は後勝ち)。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn automation_clip_zone_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    view: ArrangementView,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes: Rect,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
    edge: f32,
) -> Option<(AutomationClipKey, ClipDragKind, Rect, Rect)> {
    if !lanes.contains(cx, cy) {
        return None;
    }
    let mut hit: Option<(AutomationClipKey, ClipDragKind, Rect, Rect)> = None;
    // `clip_hit` / `section_hit` と同じ 2-tier in-rect 優先 (隣接 clip の共有境界で
    // 右端 resize が後続の外側拡張 ResizeLeft に奪われる既知バグクラスの同件、
    // review — ui/CLAUDE.md 「左右端 resize ハンドル widget は 2-tier 踏襲」)。
    let mut hit_inside = false;
    let mut hit_edge_dist = f32::INFINITY;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes.x,
        lanes.w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            if cy < body_rect.y || cy >= body_rect.y + body_rect.h {
                return;
            }
            let track_id = visible_tracks[t_idx].id;
            let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
            let pad = style.automation_clip_v_pad_px;
            let clip_y = body_rect.y + pad;
            let clip_h = (body_rect.h - pad * 2.0).max(2.0);
            if cy < clip_y || cy >= clip_y + clip_h {
                return;
            }
            for clip in &lane.clips {
                #[allow(clippy::cast_possible_truncation)]
                let cx_clip = body_rect.x
                    + ((clip.start_beat - view.start_beat) * beat_to_px) as f32;
                #[allow(clippy::cast_possible_truncation)]
                let cw = ((clip.len_beats * beat_to_px) as f32).max(2.0);
                let r = Rect { x: cx_clip, y: clip_y, w: cw, h: clip_h };
                if cx < r.x - edge || cx >= r.x + r.w + edge {
                    continue;
                }
                let in_rect = cx >= r.x && cx < r.x + r.w;
                let near_left = cx < r.x + edge;
                let near_right = cx >= r.x + r.w - edge;
                let short_clip = r.w <= edge * 2.0;
                let kind = if short_clip && in_rect {
                    ClipDragKind::Move
                } else if !in_rect {
                    // rect 外 (外側拡張ハンドル) は rect のどちら側かで決める
                    // (piano_roll `note_zone_in` と同修正 — 極短 clip の右外側帯が
                    // ResizeLeft に化けないように)。
                    if cx < r.x {
                        ClipDragKind::ResizeLeft
                    } else {
                        ClipDragKind::ResizeRight
                    }
                } else if near_left {
                    ClipDragKind::ResizeLeft
                } else if near_right {
                    ClipDragKind::ResizeRight
                } else {
                    ClipDragKind::Move
                };
                let edge_x = match kind {
                    ClipDragKind::ResizeLeft => r.x,
                    ClipDragKind::ResizeRight => r.x + r.w,
                    ClipDragKind::Move => cx,
                };
                let dist = (cx - edge_x).abs();
                // in-rect は outer に無条件で勝つ。 同 tier は近い edge 優先
                // (同距離は後勝ち = 描画順で前面)。
                let better = if in_rect == hit_inside {
                    dist <= hit_edge_dist
                } else {
                    in_rect
                };
                if !better {
                    continue;
                }
                let key = AutomationClipKey {
                    track: track_id,
                    lane: lane.id,
                    clip: clip.id,
                };
                hit = Some((key, kind, r, body_rect));
                hit_inside = in_rect;
                hit_edge_dist = dist;
            }
        },
    );
    hit
}

/// M14 Phase 63n-3 (#028): cursor y から該当する `(AutomationLaneKey, body_rect)` を返す
/// (`automation_lane_at` の lane_key 抽出版、 cross-lane drag の release frame で `last_mouse.1` から
/// drop 先 lane を確定する用途)。 cursor が lane 群の y 範囲外なら `None`。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn automation_lane_key_at_y(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    cy: f32,
) -> Option<(AutomationLaneKey, Rect)> {
    let mut found: Option<(AutomationLaneKey, Rect)> = None;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes_x,
        lanes_w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            if found.is_some() {
                return;
            }
            if cy >= body_rect.y && cy < body_rect.y + body_rect.h {
                let track_id = visible_tracks[t_idx].id;
                found = Some((
                    AutomationLaneKey { track: track_id, lane: lane.id },
                    body_rect,
                ));
            }
        },
    );
    found
}




#[must_use]
pub(super) fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}
