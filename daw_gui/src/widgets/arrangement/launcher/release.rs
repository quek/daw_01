//! ランチャー帯の release 確定 — drag の結果 / ダブルクリック / 押しっぱなしの離しを
//! [`LauncherIntent`] にして `ArrangementResponse` へ移す。
//!
//! **ここでも `Song` は書かない。** 「何が起きたか」だけを返し、`AppEvent` への翻訳と
//! `AudioCommand` の送信は caller (束 D) が行う (計画書 §3 の分担)。

use super::*;

/// `run.rs` の最後の方で呼ぶ。press が積んだ意図の回収も含めてここが唯一の出口。
pub(crate) fn commit(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    sessions: LauncherSessions,
    response: &mut ArrangementResponse,
) {
    // press ブロックが貯めた意図 (発火 / 停止 / 返す) を先に出す。
    {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        let pending = std::mem::take(&mut state.launcher.pending_intents);
        response.launcher.intents.extend(pending);
    }
    // 押しっぱなしのボタンを離した = `LaunchMode::Gate` の停止契機。
    if let Some(btn) = sessions.released_button {
        match btn {
            // 押下と **同じ展開**を通す (グループ行は子行へ)。片方だけ展開すると
            // `LaunchMode::Gate` で「撃ったが止まらない子」が残る。
            LauncherButton::Cell(cell) => {
                for c in press::expand_group_cells(f.tracks, f.launcher_view, cell) {
                    response
                        .launcher
                        .intents
                        .push(LauncherIntent::Launch { cell: c, pressed: false });
                }
            }
            LauncherButton::Scene(scene_id) => {
                response
                    .launcher
                    .intents
                    .push(LauncherIntent::LaunchScene { scene_id, pressed: false });
            }
        }
    }
    if let Some(sr) = sessions.released_scene_reorder {
        commit_scene_reorder(f, &sr, response);
    }
    if let Some(cd) = sessions.released_cell_drag {
        commit_cell_drag(f, &cd, response);
    }
    hovered_cell(f, response);
    double_click(ui, f, response);
}

/// ポインタ下のセル (`hovered_clip` と同じ毎フレーム算出の hover state)。
fn hovered_cell(f: &ArrangementFrame<'_>, response: &mut ArrangementResponse) {
    if let Some((x, y)) = f.pointer.pos {
        response.launcher.hovered_cell = layout::cell_at(f, x, y).map(|(k, _)| k);
    }
}

/// 空セルのダブルクリック = 空クリップ作成 / クリップ有りのダブルクリック = エディタを開く。
fn double_click(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    response: &mut ArrangementResponse,
) {
    // popup が開いているフレームは背景の入力を拾わない (`press::dispatch` と同じ理由 —
    // context menu は anchor の外で背景の pointer をマスクしない)。項目の連打が
    // 「空セルのダブルクリック」に化けると、押した覚えのないセルが作られる。
    if f.launcher.collapsed || ui.has_open_popups() {
        return;
    }
    let Some((cx, cy)) = ui.take_double_click_in_rect(f.launcher.grid) else {
        return;
    };
    let Some((key, rect)) = layout::cell_at(f, cx, cy) else {
        return;
    };
    // ▶ (発火ボタン) の上は「撃つ」だけの場所。**press と同じ 1 本の分割規則**
    // (`launch_button_rect`) を通す — 通さないと、クリップを撃ち直す 2 連打が
    // そのまま `OpenCellEditor` になり、空セルの ■ を 2 度叩いて行を止める操作が
    // `CreateCell` になる (止める / 撃ち直すつもりの操作が曲の中身を変える)。
    if layout::launch_button_rect(rect).contains(cx, cy) {
        return;
    }
    // セルを所有できない行 (マスター行 / グループ行) には作らない・開かない。
    // 落とし先 (`drop_cell_at`) と caller へ返す rect が通るのと同じ 1 本。
    if !layout::row_takes_cells(f, key.row) {
        return;
    }
    if key.is_empty() {
        response
            .launcher
            .intents
            .push(LauncherIntent::CreateCell { row: key.row, scene_index: key.scene_index });
    } else {
        response.launcher.intents.push(LauncherIntent::OpenCellEditor(key));
    }
}

// ============================================================
// シーン列の並べ替え
// ============================================================

fn commit_scene_reorder(
    f: &ArrangementFrame<'_>,
    sr: &SceneReorderSession,
    response: &mut ArrangementResponse,
) {
    let dx = sr.last_mouse.0 - sr.anchor_mouse.0;
    let dy = sr.last_mouse.1 - sr.anchor_mouse.1;
    if (dx * dx + dy * dy).sqrt() < REORDER_DRAG_THRESHOLD_PX {
        // 短クリックは **列の選択**に格下げ (セル本体の drag と同じ demote)。
        // 以前はここで黙って捨てていたので、シーン見出しは押しても何も起きず、
        // 列を選ぶ手段が存在しなかった (= 列のフォローアクションは「その列に
        // セルを持つ行を選ぶ」経由でしか触れず、空の列は設定できなかった)。
        response.launcher.intents.push(LauncherIntent::SelectScene {
            scene_id: sr.scene_id,
            additive: sr.last_ctrl && !sr.last_shift,
            range: sr.last_shift,
        });
        return;
    }
    // overlay の指標線と **同じ 1 本** を通す (指した位置 = 着地位置)。
    let to = drag::drop_scene_index(f, sr.last_mouse.0);
    #[allow(clippy::cast_possible_truncation)]
    let to_index = to as u32;
    if to_index == sr.anchor_index {
        return;
    }
    response
        .launcher
        .intents
        .push(LauncherIntent::ReorderScene { scene_id: sr.scene_id, to_index });
}

// ============================================================
// セルのドラッグ
// ============================================================

fn commit_cell_drag(
    f: &ArrangementFrame<'_>,
    cd: &CellDragSession,
    response: &mut ArrangementResponse,
) {
    let (mx, my) = cd.last_mouse;
    let dx = mx - cd.anchor_mouse.0;
    let dy = my - cd.anchor_mouse.1;
    if dx.abs() + dy.abs() < CELL_DRAG_SLOP_PX {
        // 短クリックは選択に格下げ (クリップの drag と同じ demote)。
        response.launcher.intents.push(LauncherIntent::SelectCell {
            cell: cd.primary,
            additive: cd.last_ctrl && !cd.last_shift,
            range: cd.last_shift,
        });
        return;
    }
    let mode = ClipCopyMode::from_modifiers(cd.last_ctrl, cd.last_shift);
    if f.lanes.contains(mx, my) {
        drop_to_arranger(f, cd, mode, response);
        return;
    }
    if !f.launcher.collapsed && f.launcher.grid.contains(mx, my) {
        drop_to_cells(f, cd, mode, response);
    }
}

/// セルをアレンジのレーンへ運んだ。
fn drop_to_arranger(
    f: &ArrangementFrame<'_>,
    cd: &CellDragSession,
    mode: ClipCopyMode,
    response: &mut ArrangementResponse,
) {
    let (mx, my) = cd.last_mouse;
    let Some(base_row) = arrangement_row_at_y(f, my) else {
        return;
    };
    let Some(base_idx) = row_index(f, base_row) else {
        return;
    };
    let Some(anchor_idx) = row_index(f, cd.primary.row) else {
        return;
    };
    let raw = px_to_beat(mx, f.lanes.x, f.lanes.w, f.view);
    let start = f.view.snap.snap_beat(raw, cd.last_alt, f.zoom_x_px_per_beat).max(0.0);
    let drops: Vec<CellToClipDrop> = cd
        .cells
        .iter()
        .filter_map(|cell| {
            let idx = row_index(f, cell.row)?;
            let to = shift_row(f, idx, base_idx, anchor_idx)?;
            Some(CellToClipDrop { from: *cell, to_row: to, to_start_beat: start })
        })
        .collect();
    if !drops.is_empty() {
        response.launcher.intents.push(LauncherIntent::DropCellsToArranger { drops, mode });
    }
}

/// セルを別のセルへ運んだ (帯の中の移動 / 複製)。
fn drop_to_cells(
    f: &ArrangementFrame<'_>,
    cd: &CellDragSession,
    mode: ClipCopyMode,
    response: &mut ArrangementResponse,
) {
    let moves = plan_cell_moves(f, cd);
    if !moves.is_empty() {
        response.launcher.intents.push(LauncherIntent::MoveCells { moves, mode });
    }
}

/// 帯の中で運んでいるセルの **着地先**。
///
/// **`draw::drag_overlays` のプレビューと `drop_to_cells` の確定が同じこの 1 本を
/// 通る**ので、ゴーストが乗っているスロットと実際に落ちるスロットが構造的に
/// 一致する (別々に解くと、掴んだセルと落とし先の相対位置の計算がどこかでずれて
/// 「見えている場所と違うところに落ちる」になる)。
#[must_use]
pub(super) fn plan_cell_moves(
    f: &ArrangementFrame<'_>,
    cd: &CellDragSession,
) -> Vec<LauncherCellMove> {
    let (mx, my) = cd.last_mouse;
    let Some(target) = layout::drop_cell_at(f, mx, my) else {
        return Vec::new();
    };
    let Some(base_idx) = row_index(f, target.row) else {
        return Vec::new();
    };
    let Some(anchor_idx) = row_index(f, cd.primary.row) else {
        return Vec::new();
    };
    let col_delta = i64::from(target.scene_index) - i64::from(cd.primary.scene_index);
    cd.cells
        .iter()
        .filter_map(|cell| {
            let idx = row_index(f, cell.row)?;
            let to_row = shift_row(f, idx, base_idx, anchor_idx)?;
            let col = (i64::from(cell.scene_index) + col_delta).max(0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let to_scene_index = col as u32;
            Some(LauncherCellMove { from: *cell, to_row, to_scene_index })
        })
        .filter(|m| m.to_row != m.from.row || m.to_scene_index != m.from.scene_index)
        .collect()
}

// ============================================================
// アレンジのクリップを帯へ落とす
// ============================================================

/// **アレンジのクリップドラッグがランチャー帯の上で離された**ときに、アレンジ側の
/// `MoveClips` を出さずセルへの drop 意図に振り替える。
///
/// `release::commit_releases` の clip drag ブロックの手前で呼ばれ、`true` を返した
/// フレームは `clip_drag_release` を落とす (= アレンジ側の移動は起きない)。
pub(crate) fn take_arrangement_drop(
    f: &ArrangementFrame<'_>,
    nd: &ClipDragSession,
    response: &mut ArrangementResponse,
) -> bool {
    take_drop(f, nd.kind, nd.last_mouse, (nd.last_ctrl, nd.last_shift), response, || {
        plan_clip_drops(f, nd)
    })
}

/// [`take_arrangement_drop`] のオートメーションクリップ版 (レーン行 → レーン行のセル)。
/// `release::commit_releases` の automation_clip_drag ブロックの手前で呼ばれる。
pub(crate) fn take_automation_drop(
    f: &ArrangementFrame<'_>,
    acd: &AutomationClipDragSession,
    response: &mut ArrangementResponse,
) -> bool {
    take_drop(f, acd.kind, acd.last_mouse, (acd.last_ctrl, acd.last_shift), response, || {
        plan_automation_clip_drops(f, acd)
    })
}

/// 2 種のドラッグで共通の「帯の上で離したか」判定と意図の積み込み。
fn take_drop(
    f: &ArrangementFrame<'_>,
    kind: ClipDragKind,
    last_mouse: (f32, f32),
    (ctrl, shift): (bool, bool),
    response: &mut ArrangementResponse,
    plan: impl FnOnce() -> Vec<ClipToCellDrop>,
) -> bool {
    if f.launcher.collapsed || !matches!(kind, ClipDragKind::Move) {
        return false;
    }
    let (mx, my) = last_mouse;

    // **帯の上で離したら必ずここで受け止める。** 格子の外 (停止列 / 返す列 /
    // 見出し行 / つかみ代) はセルにできないので「移動をキャンセル」として吸収する
    // — 素通りさせるとアレンジ側の move が走り、掴んだクリップが帯まで戻した
    // ぶんだけ左へ飛んで **拍 0 にワープする**。
    if !f.launcher.pane.contains(mx, my) {
        return false;
    }
    let drops = plan();
    let mode = ClipCopyMode::from_modifiers(ctrl, shift);
    if !drops.is_empty() {
        response.launcher.intents.push(LauncherIntent::DropClipsToCells { drops, mode });
    }
    true
}

/// 帯へ運んでいるクリップ 1 つ (行 / 開始拍 / 何を掴んだか)。2 種のドラッグ session を
/// 同じ写像に通すための共通形。
struct DropSource {
    row: ArrangementRowKey,
    start_beat: f64,
    from: ArrangementClipRef,
}

/// アレンジから帯へ運んでいる (MIDI / オーディオ) クリップの **着地先**。
///
/// `plan_cell_moves` と同じ理由でプレビューと確定が共有する 1 本
/// (`draw::drag_overlays` がこれを rect に直してゴーストを置く)。格子の外
/// (停止列 / 返す列 / 見出し行) を指しているときは空 = 落ちる先が無いので
/// ゴーストも出ない。
#[must_use]
pub(super) fn plan_clip_drops(
    f: &ArrangementFrame<'_>,
    nd: &ClipDragSession,
) -> Vec<ClipToCellDrop> {
    let mut sources: Vec<DropSource> = nd
        .anchors
        .iter()
        .map(|a| DropSource {
            row: ArrangementRowKey::Track(a.key.track_id),
            start_beat: a.start_beat,
            from: ArrangementClipRef::Track(a.key),
        })
        .collect();
    // トラック行のクリップが行けるのはトラック行だけ (handler の `drop_one_clip_to_cell`
    // と同じ規約)。レーン行へ写った分は落とせないのでゴーストも出さない。
    plan_drops(f, nd.kind, nd.last_mouse, &mut sources, |row| {
        matches!(row, ArrangementRowKey::Track(_))
    })
}

/// [`plan_clip_drops`] のオートメーションクリップ版。行き先はレーン行だけ。
#[must_use]
pub(super) fn plan_automation_clip_drops(
    f: &ArrangementFrame<'_>,
    acd: &AutomationClipDragSession,
) -> Vec<ClipToCellDrop> {
    let mut sources: Vec<DropSource> = acd
        .anchors
        .iter()
        .map(|a| DropSource {
            // anchor の lane key は widget 内の mirror 型、行キーは common の型。
            row: ArrangementRowKey::Lane(common::model::AutomationLaneKey {
                track: a.lane.track,
                lane: a.lane.lane,
            }),
            start_beat: a.start_beat,
            from: ArrangementClipRef::Lane(a.key),
        })
        .collect();
    plan_drops(f, acd.kind, acd.last_mouse, &mut sources, |row| {
        matches!(row, ArrangementRowKey::Lane(_))
    })
}

/// 掴んだクリップ群を、指している格子のセルを起点に行 / 列へ写す。
///
/// 同じ行の複数クリップは、開始拍の順に隣の列へ並べる (Live と同じ「時間軸を列に
/// 開く」写像)。ランチャーには時間軸が無いので、これ以外に複数クリップを 1 列へ
/// 落とすと必ずどれかが上書きになる。行は「掴んだ行 → 落とした行」のずれぶん
/// 全体を平行移動し、`accepts` を満たさない行 (種別違い) へ写ったものは捨てる。
fn plan_drops(
    f: &ArrangementFrame<'_>,
    kind: ClipDragKind,
    last_mouse: (f32, f32),
    sources: &mut [DropSource],
    accepts: impl Fn(ArrangementRowKey) -> bool,
) -> Vec<ClipToCellDrop> {
    // 端を掴んだ resize は帯へ落とせない (`take_drop` と同じゲート)。
    // ここを緩めると「ゴーストは出るのに離しても何も起きない」になる。
    if f.launcher.collapsed || !matches!(kind, ClipDragKind::Move) {
        return Vec::new();
    }
    let (mx, my) = last_mouse;
    let Some(target) = layout::drop_cell_at(f, mx, my) else {
        return Vec::new();
    };
    let Some(base_idx) = row_index(f, target.row) else {
        return Vec::new();
    };
    let Some(anchor_idx) = sources.first().and_then(|s| row_index(f, s.row)) else {
        return Vec::new();
    };
    sources.sort_by(|a, b| {
        row_index(f, a.row)
            .cmp(&row_index(f, b.row))
            .then(a.start_beat.total_cmp(&b.start_beat))
    });
    let mut rank_row: Option<ArrangementRowKey> = None;
    let mut rank = 0u32;
    let mut drops: Vec<ClipToCellDrop> = Vec::new();
    for s in sources.iter() {
        if rank_row != Some(s.row) {
            rank_row = Some(s.row);
            rank = 0;
        }
        let Some(idx) = row_index(f, s.row) else {
            continue;
        };
        let Some(to_row) = shift_row(f, idx, base_idx, anchor_idx) else {
            continue;
        };
        if !accepts(to_row) {
            continue;
        }
        let to_scene_index = target.scene_index.saturating_add(rank);
        drops.push(ClipToCellDrop { from: s.from, to_row, to_scene_index });
        rank += 1;
    }
    drops
}

// ============================================================
// 行 index の写像
// ============================================================

/// 行キー → `ArrangementFrame::rows` の index。
fn row_index(f: &ArrangementFrame<'_>, key: ArrangementRowKey) -> Option<usize> {
    f.rows.iter().position(|r| r.key == key)
}

/// `idx` の行を「掴んだ行 → 落とした行」のぶんだけずらした先の行キー。
/// 範囲外へ出たら `None` (= そのセルは運ばない)。
fn shift_row(
    f: &ArrangementFrame<'_>,
    idx: usize,
    base_idx: usize,
    anchor_idx: usize,
) -> Option<ArrangementRowKey> {
    let delta = base_idx as i64 - anchor_idx as i64;
    let next = idx as i64 + delta;
    if next < 0 {
        return None;
    }
    #[allow(clippy::cast_sign_loss)]
    f.rows.get(next as usize).map(|r| r.key)
}

/// アレンジのレーン側の y にある行 (帯の格子と同じ行モデルで引く)。
fn arrangement_row_at_y(f: &ArrangementFrame<'_>, y: f32) -> Option<ArrangementRowKey> {
    if y < f.lanes.y || y >= f.lanes.y + f.lanes.h {
        return None;
    }
    f.rows
        .iter()
        .find(|r| {
            let top = layout::row_screen_top(f, r);
            y >= top && y < top + r.height
        })
        .map(|r| r.key)
}
