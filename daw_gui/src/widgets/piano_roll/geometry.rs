// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! S4c Phase B-E: piano_roll widget の幾何 / hit-test / drag geometry helper 群。
//! 型・session・`RowGeometry` は `use super::*` で親 (mod.rs) から継承する
//! (privacy: 子モジュールは親の private item / struct field を参照できる)。

#![allow(clippy::too_many_arguments)]

use super::*;

/// (M14 Phase 70 / daw_01 #042) drag dy (px) から pitch delta (i32) を計算する mode-aware helper。
///
/// - Linear / Highlight: 旧式 `dy * (pitch_visible / grid.h)` = 半音単位 delta。
/// - Fold: `dy / row_h` = scale degree 単位 delta (= 可視 in-scale 行の数で割る)。
///
/// 返り値は `apply_pitch_drag_delta` で anchor pitch に適用される。
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub(super) fn compute_pitch_drag_delta(view: PianoRollView, grid: Rect, dy: f32) -> i32 {
    if matches!(view.scale.map(|s| s.mode), Some(PianoRollScaleMode::Fold)) {
        let geom = RowGeometry::compute(view, grid);
        return (-(dy / geom.row_h.max(1.0))).round() as i32;
    }
    let pitch_per_px = view.pitch_visible / grid.h.max(1.0);
    (-(dy * pitch_per_px)).round() as i32
}

/// (M14 Phase 70 / daw_01 #042 + 70b follow-up) anchor pitch に drag delta を適用、 新 pitch を
/// 返す mode-aware helper。
///
/// - **Fold mode**: anchor を scale degree に変換 → delta 加算 → in-scale pitch に逆変換 (= 必ず
///   in-scale 出力、 `last_alt` 関係なし、 元々 scale degree 単位の drag なので Alt で raw 化する
///   意味がない)。
/// - **Linear (None / Highlight, `snap_pitch_during_drag = false`)**: `anchor + delta` を 0..=127
///   に clamp して u8 化 (= 旧挙動)。
/// - **Linear + Highlight + `snap_pitch_during_drag = true` + `!last_alt`**: clamp 後、
///   `snap_to_nearest_in_scale` で最寄り in-scale に吸着 (= Bitwig / Cubase 流の drag preview snap)。
/// - **Linear + `last_alt = true`**: `snap_pitch_during_drag` 無視で raw clamp (= Alt で snap 一時無効)。
///
/// out-of-scale anchor (Fold 中に既存 out note を drag) は「直下 in-scale」 の degree を基点に。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn apply_pitch_drag_delta(
    anchor_pitch: u8,
    delta: i32,
    view: PianoRollView,
    last_alt: bool,
) -> u8 {
    if let Some(sc) = view.scale
        && matches!(sc.mode, PianoRollScaleMode::Fold)
    {
        let d = sc.pitch_to_scale_degree(anchor_pitch);
        return sc.scale_degree_to_pitch(d + delta);
    }
    let raw = (i32::from(anchor_pitch) + delta).clamp(0, 127) as u8;
    if let Some(sc) = view.scale
        && matches!(sc.mode, PianoRollScaleMode::Highlight)
        && view.snap_pitch_during_drag
        && !last_alt
    {
        return snap_to_nearest_in_scale(raw, sc);
    }
    raw
}

/// (M14 Phase 70b / daw_01 #042 follow-up) `pitch` を最寄り in-scale pitch に snap する。
///
/// `pitch` が既に in-scale ならそのまま返す。 そうでなければ、 上下に向かって最短距離の in-scale
/// pitch を探し、 距離 tie は **上を優先** (Cubase 流の tie-breaker、 daw_01 一次情報リンクと一致)。
///
/// 必ず in-scale pitch (0..=127) を返す。 `in_scale_mask = 0` (= 全 out-of-scale) の degenerate
/// caller には input `pitch` をそのまま返す (= 上下 12 半音以内に in-scale が無いケース)。
#[must_use]
pub(super) fn snap_to_nearest_in_scale(pitch: u8, scale: PianoRollScale) -> u8 {
    if scale.is_in_scale(pitch) {
        return pitch;
    }
    // 半音単位で上下を同時探索、 in-scale を見つけたら距離記録。 全 12 半音 範囲 (= 1 octave 以内
    // に必ず in-scale がある、 mask が 0 で無い限り) なら必ず見つかる。
    let mut above: Option<(u8, u8)> = None; // (pitch, distance)
    let mut below: Option<(u8, u8)> = None;
    for d in 1_u8..=12 {
        if above.is_none() {
            let p_up_i = i32::from(pitch) + i32::from(d);
            if p_up_i <= 127 {
                let p_up = p_up_i as u8;
                if scale.is_in_scale(p_up) {
                    above = Some((p_up, d));
                }
            }
        }
        if below.is_none() {
            let p_dn_i = i32::from(pitch) - i32::from(d);
            if p_dn_i >= 0 {
                let p_dn = p_dn_i as u8;
                if scale.is_in_scale(p_dn) {
                    below = Some((p_dn, d));
                }
            }
        }
        if above.is_some() && below.is_some() {
            break;
        }
    }
    match (above, below) {
        (Some((a_p, a_d)), Some((b_p, b_d))) => {
            if a_d <= b_d { a_p } else { b_p }
        }
        (Some((a_p, _)), None) => a_p,
        (None, Some((b_p, _))) => b_p,
        (None, None) => pitch, // degenerate: mask=0、 input をそのまま
    }
}

/// 内部 helper: note geometry から rect を計算。`note_to_rect` と drag preview から呼ばれる。
/// 拍は f64、最終的な pixel 座標は f32 にcast (描画用)。
pub(super) fn note_geometry_to_rect(
    start_beat: f64,
    len_beats: f64,
    pitch: u8,
    view: PianoRollView,
    grid: Rect,
) -> Rect {
    let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
    let x = grid.x + ((start_beat - view.start_beat) * beat_to_px) as f32;
    let w = ((len_beats * beat_to_px) as f32).max(1.5);
    // (M14 Phase 70 / daw_01 #042) Fold mode は y↔pitch 写像が in-scale 行 only に圧縮される。
    // RowGeometry::compute で linear / fold どちらの mode でも統一的に y 座標を返せる。
    let geom = RowGeometry::compute(view, grid);
    let (y, h) = geom.pitch_to_y_and_h(pitch);
    Rect { x, y, w, h }
}

/// 内部 helper: cursor 位置がこの note のどの zone (Move / ResizeLeft / ResizeRight)
/// に該当するかを返す。`note_hit` / `note_hover_cursor` から共通で呼ばれる。
///
/// 判定範囲 (x 方向): note rect の左右 edge から **内外** ±`edge` px (= 8px 幅のハンドル帯)。
/// y 方向は note rect 内のみ (拡張なし、隣接 pitch との衝突回避)。
///
/// 短 note (`r.w <= edge * 2.0`) は rect 内では Move 強制 (左右 edge 領域が重なって
/// 判別不能なため)、rect 外側のみ ResizeLeft / ResizeRight として扱う。
pub(super) fn note_zone_at(
    note: &Note,
    view: PianoRollView,
    grid: Rect,
    cx: f32,
    cy: f32,
    edge: f32,
) -> Option<NoteDragKind> {
    let r = note_to_rect(note, view, grid);
    // y は note rect 内のみ (Rect::contains の半開区間と整合)
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
    let short_note = r.w <= edge * 2.0;

    Some(if short_note && in_rect {
        NoteDragKind::Move
    } else if !in_rect {
        // rect 外 (外側拡張ハンドル) は「rect のどちら側か」で決める。 near_left
        // 先行評価だと幅 < edge の極短 note で右外側帯 [r.x+r.w, r.x+edge) が
        // ResizeLeft に化ける (review — doc の「外側は左右それぞれの端」 と整合)。
        if cx < r.x {
            NoteDragKind::ResizeLeft
        } else {
            NoteDragKind::ResizeRight
        }
    } else if near_left {
        NoteDragKind::ResizeLeft
    } else if near_right {
        NoteDragKind::ResizeRight
    } else {
        NoteDragKind::Move
    })
}

/// visible note slice に対する hit-test 本体。`note_hit` (visible 絞り込み後) と
/// `note_hover_cursor` が共有し、「drag で掴む note」と「hover カーソルが指す note」を
/// 構造的に一致させる (SSoT)。
///
/// 隣接 note (A.right == B.left) では両者の resize ハンドル帯が共有境界付近で重なる。
/// このとき **cursor が rect 内部に在る note (in-rect) を、外側拡張ハンドル
/// (outer-extension) しか当たらない note より無条件で優先**する。これにより A の右端を
/// 掴みたいのに B の左端 resize に奪われる問題 (daw_01 #053) を解消。同 tier
/// (両方 in-rect = overlap、または両方 outer = 微小 gap) は resize edge への水平距離が
/// 近い方を採用し、同距離なら後勝ち (描画順で前面) を踏襲する。
pub(super) fn note_hit_in(
    visible: &[Note],
    view: PianoRollView,
    grid: Rect,
    cx: f32,
    cy: f32,
    edge: f32,
) -> Option<(NoteId, NoteDragKind)> {
    let mut hit: Option<(NoteId, NoteDragKind)> = None;
    let mut hit_inside = false;
    let mut hit_edge_dist = f32::INFINITY;
    for note in visible {
        // lock されたクリップの note は参照専用ゴースト = hit-test から
        // 除外して掴めなくする (描画はされる)。hover カーソルも note_hit_in 共有なので一致する。
        if note.style.locked {
            continue;
        }
        let Some(kind) = note_zone_at(note, view, grid, cx, cy, edge) else {
            continue;
        };
        let r = note_to_rect(note, view, grid);
        let inside = cx >= r.x && cx < r.x + r.w;
        // resize edge への水平距離 (Move は当該 cursor 位置 = 距離 0 扱い)。
        let edge_x = match kind {
            NoteDragKind::ResizeLeft => r.x,
            NoteDragKind::ResizeRight => r.x + r.w,
            NoteDragKind::Move => cx,
        };
        let dist = (cx - edge_x).abs();
        // in-rect は outer に無条件で勝つ。同 tier は近い edge 優先 (同距離は後勝ち)。
        let better = if inside == hit_inside {
            dist <= hit_edge_dist
        } else {
            inside
        };
        if better {
            hit = Some((note.id, kind));
            hit_inside = inside;
            hit_edge_dist = dist;
        }
    }
    hit
}

/// (M14 Phase 64 / daw_01 #018) `pointer.y` から絶対 velocity (0..=127) を計算。
///
/// `vel_area.y` (lane top) = 127、 `vel_area.y + vel_area.h` (lane bottom) = 0 として
/// 線形 map。 範囲外は clamp (lane の上を超えて drag したら 127、 下を超えたら 0)。
/// `vel_area.h <= 0` (= disabled) なら 0 を返す (defensive)。
pub(super) fn velocity_from_y(py: f32, vel_area: Rect) -> u8 {
    if vel_area.h <= 0.0 {
        return 0;
    }
    let t = (1.0 - (py - vel_area.y) / vel_area.h).clamp(0.0, 1.0);
    (t * 127.0).round() as u8
}

/// (M14 Phase 64 / daw_01 #018) velocity lane 内の hit-test。
///
/// `cx` 位置にある note の velocity bar に hit するかを判定。 各 note の bar 中央 x は
/// `vel_area.x + (n.start_beat - view.start_beat) * beat_to_px`。 hit zone は **bar 中央から
/// 左右 ± `(velocity_bar_width_px / 2 + tolerance)` px**。
///
/// **選択優先 (daw_01 #33)**: velocity lane は note を pitch を無視して start_beat の x に集約
/// するため、 同じ拍に複数 note (ハーモニー / 密集 / tolerance 重なり) があると 1 本の x 列に
/// 複数の bar が重なる。 このとき「その x に選択中 note があれば選択中を優先」する
/// (= `is_selected(id)` が真の note を、 無ければ最後の note を返す)。 これで選択の近くを
/// 掴めば必ず選択 note が hit し、 caller が選択集合全体を編集対象にできる (選択外の最前面
/// note が握られて「選択したのに一部しか変わらない」事故を防ぐ)。 選択が無い / その x に
/// 選択 note が無いときは従来どおり後勝ち (visible 順で前面) の 1 本。
///
/// `cy` が `vel_area` 内かは caller 側で判定済み前提 (この関数は x 方向のみ判定)。
/// 戻り値 `None` は「この cx に bar 無し」 (lane 余白のクリック)。
pub(super) fn velocity_bar_hit(
    visible: &[Note],
    view: PianoRollView,
    vel_area: Rect,
    cx: f32,
    bar_width: f32,
    tolerance: f32,
    is_selected: impl Fn(NoteId) -> bool,
) -> Option<NoteId> {
    let beat_to_px = f64::from(vel_area.w) / view.len_beats.max(1e-6);
    let half_w = bar_width * 0.5 + tolerance;
    let mut hit: Option<NoteId> = None;
    let mut hit_selected: Option<NoteId> = None;
    for n in visible {
        let nx = vel_area.x + ((n.start_beat - view.start_beat) * beat_to_px) as f32;
        if (cx - nx).abs() <= half_w {
            hit = Some(n.id);
            if is_selected(n.id) {
                hit_selected = Some(n.id);
            }
        }
    }
    hit_selected.or(hit)
}

/// 絶対位置 snap で計算した note drag の beat delta (overlay と release commit で共有)。
/// anchor 0 の編集対象端 (Move=start / ResizeRight=end / ResizeLeft=start) の絶対位置を
/// snap → その差分を全 anchor に適用 (相対関係維持 + anchor 0 が grid に着地)。 anchors が
/// 空のときは raw を返す (defensive)。
pub(super) fn compute_note_drag_beat_delta(
    nd: &NoteDragSession,
    raw_beat_delta: f64,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
) -> f64 {
    let Some(a0) = nd.anchors.first() else {
        return raw_beat_delta;
    };
    let pivot = match nd.kind {
        NoteDragKind::Move | NoteDragKind::ResizeLeft => a0.start_beat,
        NoteDragKind::ResizeRight => a0.start_beat + a0.len_beats,
    };
    let snapped_pivot =
        snap.snap_beat(pivot + raw_beat_delta, nd.last_alt, zoom_x_px_per_beat);
    snapped_pivot - pivot
}

/// drag preview の shifted note geometry を計算 (drag 中の表示用、元 Note は不変)。
/// kind に応じて start_beat / pitch / len_beats を delta で更新した tuple を返す
/// (Note を返さないのは Note が `Arc<str>` lyric を持つので Copy できないため、
/// drag preview で必要な geometry 3 つだけ返す)。
pub(super) fn drag_preview_geometry(
    anchor: NoteDragAnchor,
    kind: NoteDragKind,
    beat_delta: f64,
    pitch_delta: i32,
    min_len: f64,
    view: PianoRollView,
    last_alt: bool,
) -> (f64, f64, u8) {
    match kind {
        NoteDragKind::Move => (
            (anchor.start_beat + beat_delta).max(0.0),
            anchor.len_beats,
            apply_pitch_drag_delta(anchor.pitch, pitch_delta, view, last_alt),
        ),
        NoteDragKind::ResizeRight => (
            anchor.start_beat,
            (anchor.len_beats + beat_delta).max(min_len),
            anchor.pitch,
        ),
        NoteDragKind::ResizeLeft => {
            let max_start = anchor.start_beat + anchor.len_beats - min_len;
            let new_start = (anchor.start_beat + beat_delta).clamp(0.0, max_start);
            let actual_delta = new_start - anchor.start_beat;
            (new_start, (anchor.len_beats - actual_delta).max(min_len), anchor.pitch)
        }
    }
}

/// note_create session の現在の `(start_beat, len_beats, pitch)` を計算
/// (drag preview と release commit で共有 = 描画と確定が必ず一致)。
///
/// **モデル: 既定長ノートの「右端」を掴んで動かす相対 resize** (Ableton Live / ダブルクリックで
/// カーソルが右端へ warp し、 そこを掴んでいる感覚)。 `anchor_mouse` は warp 先 (= 右端 screen x)
/// なので、 ドラッグ開始時 (cursor == anchor) の `raw_delta == 0` で長さは既定長のまま (= 最短へ
/// 飛ばない)。 そこからの移動量ぶんだけ右端が動く (cursor がそのまま右端に追従)。
///
/// - `dragged == false` (即放し / jitter 以下 / warp 未着地): 長さ = `view.default_note_len_beats`
///   (= caller の `last_note_duration_beats`)。 `0.0625` (1/16) 下限。
/// - `dragged == true` (warp 着地後に左右いずれかへ閾値ぶん drag した): 右端 pivot = `start + default`、
///   warp 先からの移動量 `raw_delta = (last_mouse.x − anchor_mouse.x) × beat_per_px` を pivot に足して
///   **絶対位置 snap** (`snap(pivot + raw_delta)`、 ui/CLAUDE.md の delta-snap NG ガイドライン /
///   note_drag ResizeRight と同方式。 anchor が右端なので実効的に `snap(cursor 位置)` = 右端が
///   cursor に一致)。 長さ = `max(min_len, snapped_right − start)`。 右ドラッグで伸長、 左ドラッグで
///   右端から短縮 (min_len まで)。 alt は session の `last_alt` を真値とし `pointer.modifiers.alt` を
///   直接見ない (overlay と commit の一致)。
pub(super) fn note_create_geometry(
    nc: &NoteCreateSession,
    view: PianoRollView,
    beat_per_px: f64,
    zoom_x_px_per_beat: f32,
) -> (f64, f64, u8) {
    let default_len = view.default_note_len_beats.max(0.0625);
    let len = if nc.dragged {
        let raw_delta = f64::from(nc.last_mouse.0 - nc.anchor_mouse.0) * beat_per_px;
        let pivot = nc.start_beat + default_len;
        let right = view.snap.snap_beat(pivot + raw_delta, nc.last_alt, zoom_x_px_per_beat);
        let min_len = if view.snap.is_active(nc.last_alt) {
            view.snap
                .beat_unit(zoom_x_px_per_beat)
                .map_or(NOTE_CREATE_MIN_LEN, |u| u.max(NOTE_CREATE_MIN_LEN))
        } else {
            NOTE_CREATE_MIN_LEN
        };
        (right - nc.start_beat).max(min_len)
    } else {
        default_len
    };
    (nc.start_beat, len, nc.pitch)
}
