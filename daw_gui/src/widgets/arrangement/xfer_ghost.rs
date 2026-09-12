//! `docs/plan_project_tabs.md` §5.6: **別のタブから運んできている** クリップ / セル /
//! トラックの着地プレビュー (落とし先タブのアレンジに描く)。
//!
//! 昇格 (`xfer.rs`) した payload は daw-ui の drag payload に居て、元タブの session は
//! もう無いので、内部ドラッグのゴースト (`draw::draw_drag_preview`) では描けない。
//! ここは payload の写し (`ClipCopy` / `LauncherCellCopy` / `TracksCopy`) から、落とす側
//! (`view::capture_drop`) と同じ規則で着地位置を求めて描く: レーンならクリップの矩形
//! (独立コピーの枠色)、帯ならセルのスロット、トラックなら挿入線。

use std::sync::Arc;

use super::*;
use crate::app_types::ProjectTransferPayload;
use crate::clipboard::{ClipCopy, ClipboardPayload, clips_from_cells};

/// heavy 描画の中 (内部ドラッグのゴーストの直後) から呼ぶ。
pub(super) fn draw(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    p: &ProjectTransferPayload,
) {
    let Some(pos) = f.pointer.pos else { return };
    // **帯 (セッション) の上は描かない。** ランチャー帯は `run.rs` でアレンジの heavy 描画の
    // **後** に描かれる (行の減光がクリップの上に乗るため) ので、ここで積んだゴーストは
    // 帯の背景とセルに上書きされて見えない。帯側の着地プレビューは
    // `launcher::draw::xfer_overlays` が帯自身の描画パスで描く。
    if f.launcher.pane.w > 0.0 && f.launcher.pane.contains(pos.0, pos.1) {
        return;
    }
    // ---- レーン / ヘッダ列の上 ----
    if !f.content_below_ruler.contains(pos.0, pos.1) {
        return;
    }
    match &p.envelope.payload {
        ClipboardPayload::Clips(clips) => {
            draw_clip_ghosts(hctx, f, pos, clips, p.grab_beat_offset, p.grab_track_offset);
        }
        ClipboardPayload::LauncherCells(cells) => {
            let clips = clips_from_cells(cells);
            draw_clip_ghosts(hctx, f, pos, &clips, 0.0, 0);
        }
        ClipboardPayload::Tracks(_) => {
            // 落とした行の直上に入る (余白なら末尾) — その位置に挿入線。
            let y = track_index_from_y(pos.1, f.lanes.y, &f.tops)
                .and_then(|i| f.tops.get(i).copied())
                .or_else(|| f.tops.last().copied())
                .unwrap_or(f.lanes.y);
            let area = f.content_below_ruler;
            hctx.with_clip_rect(area, |hctx| {
                push_filled_rect(
                    hctx,
                    Rect {
                        x: area.x,
                        y: y - f.style.reorder_drop_indicator_h * 0.5,
                        w: area.w,
                        h: f.style.reorder_drop_indicator_h,
                    },
                    f.style.reorder_drop_indicator,
                );
            });
        }
        _ => {}
    }
}

/// 行の無い余白の上でのプレビュー: 一番下の行の下に、作られる本数ぶんの行を
/// 仮に並べてクリップを描く (`capture_drop::drop_into_new_tracks` と同じ数え方)。
fn draw_new_track_ghosts(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    pos: (f32, f32),
    clips: &[ClipCopy],
    grab_beat_offset: f64,
    last_bottom: Option<f32>,
) {
    let lanes = f.lanes;
    let row_h = f.view.track_row_h;
    let top0 = last_bottom.unwrap_or(lanes.y);
    let raw = px_to_beat(pos.0, lanes.x, lanes.w, f.view) - grab_beat_offset;
    let at = f.view.snap.snap_beat(raw.max(0.0), false, f.zoom_x_px_per_beat);
    let fill = f.style.clip_clone_indep_fill.with_alpha(DRAG_PREVIEW_FILL_ALPHA);
    let ranks = crate::clipboard::dense_row_ranks(clips);
    hctx.with_clip_rect(lanes, |hctx| {
        for (cc, rank) in clips.iter().zip(&ranks) {
            let row_top = top0 + *rank as f32 * row_h;
            if row_top > lanes.y + lanes.h {
                continue;
            }
            let mut preview = ghost_clip_view(cc, fill);
            preview.start_beat = at + cc.start_beat;
            let r = clip_to_rect(row_top, row_h, &preview, f.view, lanes);
            if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                continue;
            }
            // 新しいトラックの行そのものも薄く示す (落とすと行が増えることが分かる)。
            push_filled_rect(
                hctx,
                Rect { x: lanes.x, y: row_top, w: lanes.w, h: row_h - 1.0 },
                f.style.clip_clone_indep_fill.with_alpha(draw::NEW_TRACK_ROW_ALPHA),
            );
            draw_clip(hctx, r, &preview, f.style, lanes, TrackKind::default(), f.view);
            hctx.push_rect(RectCommand {
                rect: r,
                fill: Color::TRANSPARENT,
                border: f.style.clip_clone_indep_border,
                border_width: f.style.clip_selected_border_w,
                radius: [f.style.clip_radius; 4],
                clip_rect: Some(lanes),
            });
        }
    });
}

/// 運んできた写し 1 個の描画用ビュー (レーンでも新トラックでも同じ見た目)。
fn ghost_clip_view(cc: &ClipCopy, fill: Color) -> ClipView {
    ClipView {
        id: 0,
        start_beat: cc.start_beat,
        len_beats: cc.length_beats,
        content_offset_beats: cc.content_offset_beats,
        name: Arc::from(cc.name.as_deref().unwrap_or("")),
        color: Some(cc.color.map_or(fill, |c| {
            Color { r: c[0], g: c[1], b: c[2], a: 1.0 }.with_alpha(DRAG_PREVIEW_FILL_ALPHA)
        })),
        share_group_color: None,
        fades: Vec::new(),
        thumbnail: None,
        in_active_group: false,
        muted: false,
    }
}

/// `capture_drop::take_project_transfer_drop` と同じ着地規則: 行 = ポインタの行 −
/// 掴んだ行差 + 相対行、拍 = snap(ポインタの拍 − 掴んだ拍差) + 相対拍。
fn draw_clip_ghosts(
    hctx: &mut HeavyCtx<'_, '_, AppData>,
    f: &ArrangementFrame<'_>,
    pos: (f32, f32),
    clips: &[ClipCopy],
    grab_beat_offset: f64,
    grab_track_offset: usize,
) {
    let lanes = f.lanes;
    let style = f.style;
    // 一番下の行の下端 (= ここから下は「行の無い余白」)。
    // **`tops` の末尾がその下端そのもの** (`visible_track_row_tops` は行数 + 1 個を返し、
    // 最後の要素は最終行の下端)。行高を足すと 1 行ぶん下へずれる。
    let last_bottom = f.tops.last().copied();
    let hovered_row = track_index_from_y(pos.1, lanes.y, &f.tops);
    // **余白の上では新しいトラックの行としてプレビューする** (`capture_drop` の
    // `drop_into_new_tracks` と同じ着地規則 — Ableton Live と同じく、落とすと
    // 要る本数だけトラックが増える)。
    if hovered_row.is_none() && last_bottom.is_none_or(|b| pos.1 >= b) {
        draw_new_track_ghosts(hctx, f, pos, clips, grab_beat_offset, last_bottom);
        return;
    }
    let hovered = hovered_row.unwrap_or(f.visible_tracks.len().saturating_sub(1));
    // master 行はクリップを持てない (落とす側も `song.tracks` に居ないので弾く)。
    // 下限を最初の実トラック行に上げて、ゴーストと着地先を一致させる。
    let min_row = usize::from(f.visible_tracks.first().is_some_and(|t| t.id == MASTER_TRACK_ID));
    let base_row = hovered.saturating_sub(grab_track_offset).max(min_row);
    let raw = px_to_beat(pos.0, lanes.x, lanes.w, f.view) - grab_beat_offset;
    let at = f.view.snap.snap_beat(raw.max(0.0), false, f.zoom_x_px_per_beat);
    let fill = style.clip_clone_indep_fill.with_alpha(DRAG_PREVIEW_FILL_ALPHA);
    hctx.with_clip_rect(lanes, |hctx| {
        for cc in clips {
            let Some(idx) = usize::try_from(base_row as i64 + cc.track_offset).ok() else { continue };
            let (Some(row_top), Some(track)) = (f.tops.get(idx).copied(), f.visible_tracks.get(idx))
            else {
                continue;
            };
            let row_h = effective_track_row_h(track, f.view.track_row_h);
            let mut preview = ghost_clip_view(cc, fill);
            preview.start_beat = at + cc.start_beat;
            let r = clip_to_rect(row_top, row_h, &preview, f.view, lanes);
            if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                continue;
            }
            draw_clip(hctx, r, &preview, style, lanes, track.kind, f.view);
            // 独立コピーの枠 (内部ドラッグの Ctrl+Shift と同じ色)。
            hctx.push_rect(RectCommand {
                rect: r,
                fill: Color::TRANSPARENT,
                border: style.clip_clone_indep_border,
                border_width: style.clip_selected_border_w,
                radius: [style.clip_radius; 4],
                clip_rect: Some(lanes),
            });
        }
    });
}
