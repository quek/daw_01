//! Global Sampler / MIDI Capture の範囲をアレンジ / ランチャーのセルへ落とす受け口
//! (`docs/plan_global_sampler.md` Q4)。
//!
//! 下部タブが `Ui::begin_drag` で持ち出した payload を、**ファイル drop と同じ着地解決**
//! ([`arrangement_view::file_drop_target`] + 同じ pixel→beat + snap) で受ける。
//! レーン / ランチャー帯の上で離したときだけ消費し、それ以外は host が frame 末に
//! 捨てる (= キャンセル)。`arrangement_view.rs` の外に置くのはファイル budget
//! (不変条件 9) のため。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::app_types::{PROJECT_XFER_DRAG_KIND, ProjectTransferPayload};
use crate::clipboard::{ClipboardPayload, cells_from_clips, clips_from_cells};
use crate::event_launcher::LauncherRow;
use crate::state::LauncherFocus;
use crate::event_sampler::SamplerEvent;
use crate::state::midi_capture::{MIDI_CAPTURE_DRAG_KIND, MidiCaptureDragPayload};
use crate::state::sampler::{SAMPLER_DRAG_KIND, SamplerDragPayload};
use crate::view::arrangement_view::file_drop_target;
use crate::widgets::arrangement::{ArrangementResponse, ArrangementRowKey};

pub(crate) fn take_capture_drops(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    resp: &ArrangementResponse,
    canvas_area: Rect,
    scroll_beat: f64,
    zoom: f32,
    arr_snap: &common::snap::SnapConfig,
) {
    let pointer = ui.pointer();
    if !pointer.primary_just_released {
        return;
    }
    let Some(pos) = pointer.pos else { return };
    let in_launcher = resp.launcher.pane_rect.w > 0.0 && resp.launcher.pane_rect.contains(pos.0, pos.1);
    let kind = ui.dragging_kind();
    if kind == Some(PROJECT_XFER_DRAG_KIND) {
        // §5.6: トラックは **ヘッダ列** に落とすのが自然 (アレンジ内の並べ替えと同じ) なので、
        // レーンだけでなくヘッダ列 (行の無い下の余白も含む) でも受ける。
        let header_col = resp
            .track_header_rects
            .first()
            .map(|(_, r)| Rect { x: r.x, y: canvas_area.y, w: r.w, h: canvas_area.h });
        let in_headers = header_col.is_some_and(|r| r.contains(pos.0, pos.1));
        if !in_launcher && !in_headers && !canvas_area.contains(pos.0, pos.1) {
            tracing::info!(?pos, "project transfer released outside the arrangement; dropped");
            return;
        }
        take_project_transfer_drop(app, ui, resp, canvas_area, scroll_beat, zoom, arr_snap, pos, in_launcher);
        return;
    }
    if !in_launcher && !canvas_area.contains(pos.0, pos.1) {
        return;
    }
    if kind != Some(SAMPLER_DRAG_KIND) && kind != Some(MIDI_CAPTURE_DRAG_KIND) {
        return;
    }
    let Some(target) = file_drop_target(app, resp, pos) else {
        // 帯の停止列 / 見出しなど、置けない場所: 消費して何もしない (ファイルと同じ契約)。
        ui.cancel_drag();
        return;
    };
    let raw = scroll_beat + ((pos.0 - canvas_area.x) as f64 / zoom as f64);
    let target_beat = Some(arr_snap.snap_beat(raw.max(0.0), /* alt: */ false, zoom));
    if let Some(p) = ui.take_drag_payload::<SamplerDragPayload>(SAMPLER_DRAG_KIND) {
        let (start_frame, end_frame) = (p.start_frame, p.end_frame);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Sampler(SamplerEvent::Drop {
                start_frame,
                end_frame,
                target,
                target_beat,
            }));
        }));
    } else if let Some(p) = ui.take_drag_payload::<MidiCaptureDragPayload>(MIDI_CAPTURE_DRAG_KIND) {
        let (start_ns, end_ns) = (p.start_ns, p.end_ns);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Sampler(SamplerEvent::MidiDrop {
                start_ns,
                end_ns,
                target,
                target_beat,
            }));
        }));
    }
}

/// `docs/plan_project_tabs.md` §5.6: 別のタブから運んできたクリップ / トラックを落とす。
/// 着地は **既存の cross-project paste** (`paste_clips_at` / `paste_tracks_at`、
/// `source_project_id` 不一致 → 独立コピー) そのもの。`grab_*_offset` を引いて掴んだ
/// 位置関係を保つ。ランチャー帯の上は受け止めない (キャンセル)。
#[allow(clippy::too_many_arguments)]
fn take_project_transfer_drop(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    resp: &ArrangementResponse,
    canvas_area: Rect,
    scroll_beat: f64,
    zoom: f32,
    arr_snap: &common::snap::SnapConfig,
    pos: (f32, f32),
    in_launcher: bool,
) {
    if in_launcher {
        take_project_transfer_drop_on_launcher(app, ui, resp, pos);
        return;
    }
    let Some(p) = ui.take_drag_payload::<ProjectTransferPayload>(PROJECT_XFER_DRAG_KIND) else {
        return;
    };
    // **行は「見えている行」で解く** (`docs/plan_project_tabs.md` §5.6)。ゴーストも
    // 表示行で描いているので、ここだけ `song.tracks` の index で解くと、折り畳んだ
    // グループ / master 行 / レーン行のぶんだけ「見えている場所」と落ちる場所がずれる。
    let bands = track_bands(app, resp);
    let rows = &bands[..];
    let hovered_row = visible_row_at_y(rows, pos.1);
    // **一番下の行より下の余白**: Ableton Live と同じく、新しいトラックを作って
    // そこへ落とす (一番下のトラックへ寄せない)。ファイル drop の `NewTrackBottom` /
    // 帯の `LauncherNewTrack` と同じ約束を、タブ間の持ち込みにも通す。
    let below_all_rows = rows.last().is_none_or(|(_, r)| pos.1 >= r.y + r.h);
    tracing::info!(?pos, ?hovered_row, project = app.pk().0, "project transfer dropped");
    let src_pid = p.envelope.source_project_id;
    let media = p.envelope.media.clone();
    match &p.envelope.payload {
        crate::clipboard::ClipboardPayload::Clips(clips) => {
            let raw = scroll_beat + ((pos.0 - canvas_area.x) as f64 / zoom as f64) - p.grab_beat_offset;
            let at = arr_snap.snap_beat(raw.max(0.0), /* alt: */ false, zoom);
            if hovered_row.is_none() && below_all_rows {
                drop_into_new_tracks(ui, clips, src_pid, &media, at);
                return;
            }
            let base = hovered_row
                .unwrap_or(rows.len().saturating_sub(1))
                .saturating_sub(p.grab_track_offset);
            let Some((clips, track_id)) = retarget_clips(app, rows, base, clips) else {
                return;
            };
            let clips = crate::clipboard::sanitize_clips(clips);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                let n = app.paste_clips_at(clips, src_pid, track_id, at, &media);
                app.ui_ephemeral.status_message = format!("別のタブからクリップを {n} 個コピーしました");
            }));
        }
        // セルをアレンジのレーンへ: 行 = 相対行、拍 = 落とした拍 + 同じ行のセルを長さぶん右へ。
        ClipboardPayload::LauncherCells(cells) => {
            let raw = scroll_beat + ((pos.0 - canvas_area.x) as f64 / zoom as f64);
            let at = arr_snap.snap_beat(raw.max(0.0), /* alt: */ false, zoom);
            let cell_clips = clips_from_cells(cells);
            if hovered_row.is_none() && below_all_rows {
                drop_into_new_tracks(ui, &cell_clips, src_pid, &media, at);
                return;
            }
            let base = hovered_row.unwrap_or(rows.len().saturating_sub(1));
            let Some((clips, track_id)) = retarget_clips(app, rows, base, &cell_clips) else {
                return;
            };
            let clips = crate::clipboard::sanitize_clips(clips);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                let n = app.paste_clips_at(clips, src_pid, track_id, at, &media);
                app.ui_ephemeral.status_message = format!("別のタブからクリップを {n} 個コピーしました");
            }));
        }
        crate::clipboard::ClipboardPayload::Tracks(payload) => {
            // 落とした行の直上へ。余白なら末尾 (`paste_tracks_at` は未知の id を末尾扱いする)。
            let above = hovered_row.and_then(|i| rows.get(i)).map_or(u32::MAX, |(id, _)| *id);
            let payload = crate::clipboard::sanitize_tracks(payload.clone());
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                let n = app.paste_tracks_at(payload, src_pid, above, &media);
                app.ui_ephemeral.status_message = format!("別のタブからトラックを {n} 本コピーしました");
            }));
        }
        _ => {}
    }
}

/// y にある **表示行** の index (ゴーストと同じ単位 = [`track_bands`])。
fn visible_row_at_y(rows: &[(u32, Rect)], y: f32) -> Option<usize> {
    rows.iter().position(|(_, r)| y >= r.y && y < r.y + r.h)
}

/// ゴーストと同じ **トラックの帯** を widget の行 (`ArrangementResponse::rows`) から組む。
///
/// 1 本の帯 = 「トラック行 + その下に展開しているオートメーションレーン行」
/// (= `ArrangementFrame::tops` の `tops[i]..tops[i+1]` と同じ区切り)。ゴースト
/// (`widgets::arrangement::xfer_ghost`) はこの単位で行を引くので、着地もこれで引く。
///
/// **`track_header_rects` は使えない** — 高さがトラック本体だけ (レーン行は別の
/// rect 群) で、しかも画面外の行が落ちている (culling)。使うと、レーンを展開した行の
/// 上でゴーストと別の行に着地したり、最下段のレーンの上で「行の無い余白」と誤判定して
/// 勝手にトラックが増えたりする。ここは culling 前の `resp.rows` から組むので、
/// 画面外の行が着地先になったクリップも落ちない。
fn track_bands(app: &AppData, resp: &ArrangementResponse) -> Vec<(u32, Rect)> {
    let origin = resp.lanes_rect.y - app.cur.view.arrange_track_top;
    let mut out: Vec<(u32, Rect)> = Vec::new();
    for row in &resp.rows {
        match row.key {
            ArrangementRowKey::Track(id) => out.push((
                id,
                Rect {
                    x: resp.lanes_rect.x,
                    y: origin + row.content_top,
                    w: resp.lanes_rect.w,
                    h: row.height,
                },
            )),
            // レーン行は親トラックの帯へ併合する。
            ArrangementRowKey::Lane(_) => {
                if let Some((_, r)) = out.last_mut() {
                    r.h += row.height;
                }
            }
        }
    }
    out
}

/// 表示行で解いた着地先を `paste_clips_at` が使う **`song.tracks` の相対 index** へ翻訳する。
///
/// payload の `track_offset` は「元タブの表示行の差」(`widgets::arrangement::xfer`)。
/// `base` (= ポインタの行 − 掴んだ行差) からの表示行で各クリップの着地行を決め、その行の
/// トラック id を `song.tracks` の index に直して、アンカーからの差へ書き換える。
/// クリップを置けない行 (master 行 / レーン行 / 行の外) に当たったクリップは落とす。
/// 戻り値は `(書き換えた写し, アンカーのトラック id)`。置ける行が 1 つも無ければ `None`。
fn retarget_clips(
    app: &AppData,
    rows: &[(u32, Rect)],
    base: usize,
    clips: &[crate::clipboard::ClipCopy],
) -> Option<(Vec<crate::clipboard::ClipCopy>, u32)> {
    let song = app.cur.song_doc.song();
    let song_index_of_row = |row: i64| -> Option<usize> {
        let idx = usize::try_from(row).ok()?;
        let (id, _) = rows.get(idx)?;
        song.track_index_of(*id)
    };
    // **base ごと** 下へ寄せる (master 行などクリップを置けない行に当たったとき)。
    // ゴースト `xfer_ghost::draw_clip_ghosts` の `.max(min_row)` と同じ規則 —
    // アンカーだけ寄せて各クリップを素の base で解くと、master 行に当たった 1 個が
    // 黙って消えて残りが 1 行ずれる (単独なら何も貼られない)。
    let base = (base as i64..rows.len() as i64).find(|r| song_index_of_row(*r).is_some())?;
    let anchor_song = song_index_of_row(base)?;
    let anchor_id = song.tracks.get(anchor_song)?.id;
    let mut out = Vec::with_capacity(clips.len());
    for cc in clips {
        let Some(song_idx) = song_index_of_row(base + cc.track_offset) else {
            continue;
        };
        let mut cc = cc.clone();
        cc.track_offset = song_idx as i64 - anchor_song as i64;
        out.push(cc);
    }
    (!out.is_empty()).then_some((out, anchor_id))
}

/// 行の無い余白へ落とした: **新しいトラックを要る本数だけ作って**そこへ置く
/// (`docs/plan_project_tabs.md` §5.6 — Ableton Live と同じ)。行の並びは
/// [`crate::clipboard::dense_row_ranks`] で詰める (ゴーストと同じ数え方)。
fn drop_into_new_tracks(
    ui: &mut Ui<'_, AppData>,
    clips: &[crate::clipboard::ClipCopy],
    src_pid: u64,
    media: &common::model::MediaManifest,
    at: f64,
) {
    let ranks = crate::clipboard::dense_row_ranks(clips);
    let Some(n_tracks) = ranks.iter().max().map(|m| m + 1) else {
        return;
    };
    let clips: Vec<_> = clips
        .iter()
        .zip(&ranks)
        .map(|(c, rank)| {
            let mut c = c.clone();
            c.track_offset = *rank as i64;
            c
        })
        .collect();
    let clips = crate::clipboard::sanitize_clips(clips);
    let media = media.clone();
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        // トラックを足す編集と貼る編集を **1 undo 手** に束ねる (ユーザーの操作は
        // 1 回の drop なので、undo も 1 回で元に戻るべき)。
        let save = app.cur.song_doc.enter_own_gesture();
        if let Some(anchor) = app.append_empty_tracks(n_tracks) {
            let n = app.paste_clips_at(clips, src_pid, anchor, at, &media);
            app.ui_ephemeral.status_message =
                format!("別のタブからクリップを {n} 個、新しいトラックにコピーしました");
        }
        app.cur.song_doc.leave_own_gesture(save);
    }));
}

/// §5.6: ランチャー帯 (セッション) へ落とす。クリップは **その行 × 列のセル** に
/// (同じトラックの複数クリップは開始拍順に右の列へ)、トラックはその行の直上に貼る。
/// 格子の外 (停止列 / 見出し / つかみ代) はファイル drop と同じく何も置かない。
/// 行の無い下の余白なら一番下に新しいトラックを作ってそのセルに置く
/// (`ImportTrackTarget::LauncherNewTrack`、ファイル drop と同じ約束)。
fn take_project_transfer_drop_on_launcher(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    resp: &ArrangementResponse,
    pos: (f32, f32),
) {
    // 行と列は **帯が返した行の y 帯 / 列幅** で解く (ゴースト `launcher::xfer` と同じ
    // 1 本 = `launcher_bridge::cell_slot_at`)。`file_drop_target` と違って
    // 「セルを置けない行 (マスター / グループ) の上」でも落とし自体は成立させる —
    // 複数行を運んでいるとき、その行より下の行にはゴーストが出ているので、ここで
    // 弾くと「プレビューが出ているのに何も貼られない」になる。
    let Some((row, scene_index)) = crate::view::launcher_bridge::cell_slot_at(resp, pos) else {
        tracing::info!(?pos, "project transfer released on the launcher band outside the grid; dropped");
        ui.cancel_drag();
        return;
    };
    let Some(p) = ui.take_drag_payload::<ProjectTransferPayload>(PROJECT_XFER_DRAG_KIND) else {
        return;
    };
    let src_pid = p.envelope.source_project_id;
    let media = p.envelope.media.clone();
    tracing::info!(?pos, ?row, scene_index, project = app.pk().0, "project transfer dropped on launcher");
    let cells = match &p.envelope.payload {
        // セル → セル (同じ相対配置で)。クリップはセルの並びに写してから同じ経路へ。
        ClipboardPayload::LauncherCells(cells) => cells.clone(),
        ClipboardPayload::Clips(clips) => cells_from_clips(clips),
        ClipboardPayload::Tracks(payload) => {
            // 行の直上へ (行の無い余白なら末尾)。レーン行はその親トラックの上。
            let above = row.map_or(u32::MAX, |k| match k {
                crate::widgets::arrangement::ArrangementRowKey::Track(id) => id,
                crate::widgets::arrangement::ArrangementRowKey::Lane(l) => l.track,
            });
            let payload = crate::clipboard::sanitize_tracks(payload.clone());
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                let n = app.paste_tracks_at(payload, src_pid, above, &media);
                app.ui_ephemeral.status_message = format!("別のタブからトラックを {n} 本コピーしました");
            }));
            return;
        }
        _ => return,
    };
    let cells = crate::clipboard::sanitize_launcher_cells(cells);
    let Some(row) = row else {
        // 行が 1 つも無い下の余白: 一番下に新しいトラックを作り、その行に載る
        // セル (行差 0) だけ置く (下に行が無いので他は行き先が無い)。
        let cells: Vec<_> = cells.into_iter().filter(|c| c.row_offset == 0).collect();
        if cells.is_empty() {
            return;
        }
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            // 上と同じ: トラックを足す編集と貼る編集で 1 undo 手。
            let save = app.cur.song_doc.enter_own_gesture();
            if let Some(track_id) = app.append_empty_track() {
                let dest = LauncherFocus {
                    row: LauncherRow::Track(track_id),
                    scene_index: scene_index as usize,
                };
                let n = app.paste_launcher_cells(cells, src_pid, dest, &media);
                app.ui_ephemeral.status_message = format!("別のタブからセルを {n} 個コピーしました");
            }
            app.cur.song_doc.leave_own_gesture(save);
        }));
        return;
    };
    let Some((cells, dest_row)) = retarget_cells(app, resp, row, cells) else {
        return;
    };
    let dest = LauncherFocus { row: dest_row, scene_index: scene_index as usize };
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        let n = app.paste_launcher_cells(cells, src_pid, dest, &media);
        app.ui_ephemeral.status_message = format!("別のタブからセルを {n} 個コピーしました");
    }));
}

/// 帯の **表示行**で解いた着地先を、`paste_launcher_cells` が使う
/// **曲の全行 (`all_launcher_rows`) の相対 index** へ翻訳する。
///
/// payload の `row_offset` は「元タブの表示行の差」(`widgets::arrangement::xfer`)。
/// ポインタの行から表示行で各セルの着地行を決め (ゴースト `launcher::xfer::target_slots`
/// と同じ規則)、その行を全行の並びで数え直す。行き先の行が画面に無い / セルを置けない
/// セルは落とす。戻り値は `(書き換えた写し, 貼り付けの基準行)`。
fn retarget_cells(
    app: &AppData,
    resp: &ArrangementResponse,
    hovered: crate::widgets::arrangement::ArrangementRowKey,
    cells: Vec<crate::clipboard::LauncherCellCopy>,
) -> Option<(Vec<crate::clipboard::LauncherCellCopy>, LauncherRow)> {
    let bands = &resp.launcher.row_bands;
    let base = bands.iter().position(|(k, _)| *k == hovered)?;
    let song = app.cur.song_doc.song();
    let all = app.all_launcher_rows();
    // 表示行 → 全行の index (セルを置けない行は `None`)。
    let all_index_of = |visible: i64| -> Option<usize> {
        let idx = usize::try_from(visible).ok()?;
        let row = crate::view::launcher_bridge::row_of(bands.get(idx)?.0);
        if !crate::handler::launcher_cells::row_accepts_cells(song, row) {
            return None;
        }
        all.iter().position(|r| *r == row)
    };
    // 基準行 = 一番上の着地行 (ポインタの行がセルを置けないこともあるので、置ける
    // 行の中から採る — ゴーストもその行にだけスロットを描いている)。
    let placed: Vec<(usize, crate::clipboard::LauncherCellCopy)> = cells
        .into_iter()
        .filter_map(|c| all_index_of(base as i64 + c.row_offset).map(|i| (i, c)))
        .collect();
    let anchor = placed.iter().map(|(i, _)| *i).min()?;
    let out = placed
        .into_iter()
        .map(|(i, mut c)| {
            c.row_offset = i as i64 - anchor as i64;
            c
        })
        .collect();
    Some((out, *all.get(anchor)?))
}

