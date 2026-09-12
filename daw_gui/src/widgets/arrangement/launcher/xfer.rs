//! `docs/plan_project_tabs.md` §5.6: **別のタブから運んできている** クリップ / セル /
//! トラックの、**帯 (セッションビュー) の中での**着地プレビュー。
//!
//! アレンジ側 (`widgets::arrangement::xfer_ghost`) は帯に描けない — 帯は `run.rs` で
//! アレンジの heavy 描画の **後** に描かれる (ランチャー主導の行の減光がクリップの上に
//! 乗るため) ので、アレンジのパスで積んだゴーストは帯の背景とセルに上書きされる。
//! だから帯の分はこのモジュールが帯自身の描画パスから描く。
//!
//! 着地先の解き方は落とす側 (`view::capture_drop`) と同じ規則。

use super::draw::{ghost_style, push_rounded, push_slot_ghosts};
use super::*;
use crate::app_types::ProjectTransferPayload;
use crate::clipboard::{ClipboardPayload, LauncherCellCopy, cells_from_clips};

/// 帯の描画パスから毎フレーム呼ぶ (payload が無ければ何もしない)。
pub(super) fn overlays(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    p: &ProjectTransferPayload,
) {
    let Some(pos) = f.pointer.pos else { return };
    if !f.launcher.pane.contains(pos.0, pos.1) {
        return;
    }
    // トラックは帯の行そのものを増やす (セルのスロットが無い) ので、行の挿入線だけ出す。
    if matches!(p.envelope.payload, ClipboardPayload::Tracks(_)) {
        track_insert_line(hctx, f, pos);
        return;
    }
    // payload をセルの並びに正規化する (クリップ → セル、セル → そのまま)。
    let cells: Vec<LauncherCellCopy> = match &p.envelope.payload {
        ClipboardPayload::Clips(clips) => cells_from_clips(clips),
        ClipboardPayload::LauncherCells(cells) => cells.clone(),
        _ => return,
    };
    let style = ghost_style(f, ClipCopyMode::CloneIndependent);
    // 格子の上なら着地スロット、外なら (停止列 / 見出し / つかみ代) カーソル追従ゴースト。
    let slots = target_slots(f, pos, &cells);
    if slots.is_empty() {
        cursor_ghost(hctx, f, pos, style);
    } else {
        push_slot_ghosts(hctx, f, &slots, style);
    }
}

/// ポインタの (行, 列) を原点に、各セルの相対位置を足した着地スロット群。
fn target_slots(
    f: &ArrangementFrame<'_>,
    pos: (f32, f32),
    cells: &[LauncherCellCopy],
) -> Vec<(ArrangementRowKey, u32)> {
    let Some((row, col)) = super::xfer_slot_at(f, pos) else {
        return Vec::new();
    };
    let Some(row_i) = f.rows.iter().position(|r| r.key == row) else {
        return Vec::new();
    };
    cells
        .iter()
        .filter_map(|c| {
            let ri = usize::try_from(row_i as i64 + c.row_offset).ok()?;
            let key = f.rows.get(ri)?.key;
            // 落とす側 (`cell_drop_target` → `row_accepts_cells`) が拒否する行には出さない
            // (マスター行 / グループ行 / セルを持てないレーン行)。
            if !layout::row_takes_cells(f, key) {
                return None;
            }
            let ci = u32::try_from(i64::from(col) + c.scene_offset).ok()?;
            Some((key, ci))
        })
        .collect()
}

/// 落ちる先が無い (格子の外) ときにカーソルへ付くゴースト。
fn cursor_ghost(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    pos: (f32, f32),
    (fill, border): (Color, Color),
) {
    let w = (f.launcher.col_w - 2.0).max(8.0);
    let h = f.view.track_row_h.max(8.0) - 4.0;
    let ghost = Rect { x: pos.0 - w * 0.5, y: pos.1 - h * 0.5, w, h };
    let pane = f.launcher.pane;
    hctx.with_clip_rect(pane, |hctx| {
        push_rounded(hctx, ghost, fill, border, CELL_RADIUS);
    });
}

/// トラックを運んでいるときの行の挿入線 (その行の直上に入る)。
fn track_insert_line(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    pos: (f32, f32),
) {
    let Some(row) = layout::row_at_y(f, pos.1) else { return };
    let top = layout::row_screen_top(f, &row);
    let pane = f.launcher.pane;
    let h = f.style.reorder_drop_indicator_h;
    hctx.with_clip_rect(pane, |hctx| {
        push_filled_rect(
            hctx,
            Rect { x: pane.x, y: top - h * 0.5, w: pane.w, h },
            f.style.reorder_drop_indicator,
        );
    });
}
