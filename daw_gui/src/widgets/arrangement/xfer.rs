//! `docs/plan_project_tabs.md` §5.6: アレンジ内部のドラッグ (クリップ Move / トラック
//! ヘッダの並べ替え) を **別のタブへの持ち込み** に昇格させる。
//!
//! 昇格の瞬間 = (a) ポインタがタブ帯のタブ (アクティブ以外) の上に
//! [`tab_strip::SPRING_LOAD`] 留まった、または (b) ドラッグ中に Ctrl+Tab /
//! Ctrl+Shift+Tab が押された (`ui_ephemeral.pending_tab_switch`)。どちらも:
//! 運んでいる選択を copy と同じ `ClipboardEnvelope` に写して `Ui::begin_drag` に載せ、
//! 元タブの session を捨て (Song は無変更)、タブを切り替える。落とす側は
//! `view::capture_drop` (cross-project paste = 独立コピー)。

use std::time::Instant;

use common::protocol::ProjectKey;

use super::*;
use crate::app_types::{PROJECT_XFER_DRAG_KIND, ProjectTransferPayload};
use crate::event_launcher::LauncherCellKey;
use crate::event_tabs::TabEvent;
use crate::view::tab_strip;

/// `drag::advance` の前に呼ぶ。昇格したフレームは session が消えているので、以降の
/// continuation / release は何もしない (= 元タブのクリップは動かない)。
pub(super) fn promote(app: &AppData, ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let now = app.ui_ephemeral.frame_now;
    // 昇格できる session: クリップの Move / トラックヘッダの並べ替え / ランチャーのセル。
    let (clip_move, track_ids, cells) = promotable(ui, f);
    let pending = app.ui_ephemeral.pending_tab_switch;
    if clip_move.is_none() && track_ids.is_none() && cells.is_none() {
        if let Some(k) = pending {
            // 昇格できないドラッグ (範囲 / ループ / ...) 中の Ctrl+Tab: ただ切り替える。
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.pending_tab_switch = None;
                app.handle_event(AppEvent::Tab(TabEvent::Switch(k)));
            }));
        }
        return;
    }
    let target = pending.or_else(|| spring_target(app, ui, f, now));
    let Some(target) = target else { return };
    if target == app.pk() {
        return;
    }
    let payload = match (clip_move, track_ids, cells) {
        (Some((anchor_mouse, keys, range)), _, _) => {
            clip_payload(app, f, anchor_mouse, &keys, range)
        }
        (None, Some(ids), _) => track_payload(app, &ids),
        (None, None, Some(cells)) => cell_payload(app, f, &cells),
        (None, None, None) => None,
    };
    let Some(payload) = payload else {
        if pending.is_some() {
            ui.push_edit(Edit::mutate(|app: &mut AppData| app.ui_ephemeral.pending_tab_switch = None));
        }
        return;
    };
    {
        let st: &mut ArrangementState = ui.widget_state(f.wid);
        st.clip_drag = None;
        st.track_reorder = None;
        st.launcher.cell_drag = None;
        st.tab_hover = None;
    }
    tracing::info!(target = target.0, from = app.pk().0, "arrangement drag promoted to project transfer");
    ui.begin_drag(PROJECT_XFER_DRAG_KIND, payload);
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.ui_ephemeral.pending_tab_switch = None;
        app.handle_event(AppEvent::Tab(TabEvent::Switch(target)));
    }));
}

/// いま昇格できるドラッグ (クリップの Move / トラックの並べ替え / ランチャーのセル)。
/// どれも無ければ spring-loaded の計時もリセットする。
type ClipMove = ((f32, f32), Vec<ClipKey>, Option<(f64, f64)>);
type Promotable = (Option<ClipMove>, Option<Vec<u32>>, Option<Vec<LauncherCellKey>>);
fn promotable(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) -> Promotable {
    let st: &mut ArrangementState = ui.widget_state(f.wid);
    let clip_move = st.clip_drag.as_ref().filter(|d| d.kind == ClipDragKind::Move).map(|d| {
        (
            d.anchor_mouse,
            d.anchors.iter().map(|a| a.key).collect::<Vec<_>>(),
            // press で確定した「時間範囲を掴んでいるか」。掴んだのが範囲でなければ
            // クリップ丸ごとを運ぶ (範囲が別の場所に残っているだけの状態で範囲側を
            // 運ぶと、掴んだクリップが 1 つも運ばれない)。
            d.range_move(),
        )
    });
    let track_ids = st.track_reorder.as_ref().map(|t| t.source_track_ids.clone());
    let cells = st.launcher.cell_drag.as_ref().map(|d| widget_cells_to_handler(&d.cells));
    if clip_move.is_none() && track_ids.is_none() && cells.is_none() {
        st.tab_hover = None;
    }
    (clip_move, track_ids, cells)
}

/// widget のセルキー → handler のセルキー (空セルは運ばない)。
fn widget_cells_to_handler(
    cells: &[super::launcher::LauncherCellKey],
) -> Vec<LauncherCellKey> {
    cells
        .iter()
        .filter_map(|c| {
            if let Some(k) = c.clip_key() {
                return Some(LauncherCellKey::Track(k));
            }
            c.automation_clip_key().map(|k| {
                LauncherCellKey::Lane(common::model::AutomationClipKey {
                    track: k.track,
                    lane: k.lane,
                    clip: k.clip,
                })
            })
        })
        .collect()
}

/// タブ帯の (アクティブ以外の) タブの上に `SPRING_LOAD` 留まったらそのタブ。
fn spring_target(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    now: Instant,
) -> Option<ProjectKey> {
    let strip = Rect {
        x: 0.0,
        y: crate::view::root::MENU_H,
        w: ui.screen().width as f32,
        h: tab_strip::height(app),
    };
    let over = f.pointer.pos.and_then(|(px, py)| {
        tab_strip::layout(app, strip)
            .into_iter()
            .find(|(_, r)| r.contains(px, py))
            .map(|(k, _)| k)
    });
    let st: &mut ArrangementState = ui.widget_state(f.wid);
    match (over, st.tab_hover) {
        (Some(k), Some((hk, since))) if hk == k => {
            if k != app.pk() && now.duration_since(since) >= tab_strip::SPRING_LOAD {
                return Some(k);
            }
        }
        (Some(k), _) => st.tab_hover = Some((k, now)),
        (None, _) => st.tab_hover = None,
    }
    None
}

/// 運んでいるクリップ群を envelope に写す。`grab_*_offset` は「掴んだ点 − 先頭クリップ」。
fn clip_payload(
    app: &AppData,
    f: &ArrangementFrame<'_>,
    anchor_mouse: (f32, f32),
    keys: &[ClipKey],
    range_move: Option<(f64, f64)>,
) -> Option<ProjectTransferPayload> {
    let mut refs: Vec<ClipKey> = keys.to_vec();
    refs.sort_by_key(|k| (k.track_id, k.clip_id));
    refs.dedup();
    // **時間範囲を掴んで動かしているときだけ** 範囲で切った写しを運ぶ (Ctrl+C と同じ形:
    // 範囲の先頭が原点、はみ出したクリップは窓を詰める)。
    let (mut envelope, base_beat, min_track) =
        match range_move.and_then(|_| app.time_selection_clips_copy()) {
            Some((clips, origin, min_track)) => (
                app.envelope_with_media(crate::clipboard::ClipboardPayload::Clips(clips)),
                origin,
                min_track,
            ),
            None => app.clips_copy_envelope(&refs)?,
        };
    // **行の単位を「見えている行」に揃える** (`docs/plan_project_tabs.md` §5.6)。
    // envelope の `track_offset` は元タブの `song.tracks` index の差だが、落とし先で
    // そのまま使うと、折り畳んだグループ / master 行 / レーン行のぶんだけゴーストの行と
    // 着地の行がずれる (畳まれて見えない子トラックにクリップが落ちる)。ユーザーが見て
    // 操作しているのは表示行なので、運ぶ前に表示行の差へ翻訳しておく。
    let song = app.cur.song_doc.song();
    let visible_of = |song_idx: usize| -> Option<usize> {
        let id = song.tracks.get(song_idx)?.id;
        f.visible_tracks.iter().position(|t| t.id == id)
    };
    // 見えない行 (畳んだグループの子など) のクリップは **捨てる** — 着地側も置けない
    // 行の品目を捨てるので向きを揃える。`?` で中断すると、選択に畳んだ行のクリップが
    // 1 個混ざっているだけで**ドラッグそのものが無反応**になる。
    let base_visible = {
        let crate::clipboard::ClipboardPayload::Clips(clips) = &mut envelope.payload else {
            return None;
        };
        let mut kept: Vec<(crate::clipboard::ClipCopy, usize)> = Vec::with_capacity(clips.len());
        for cc in clips.iter() {
            let Ok(song_idx) = usize::try_from(min_track as i64 + cc.track_offset) else {
                continue;
            };
            if let Some(visible) = visible_of(song_idx) {
                kept.push((cc.clone(), visible));
            }
        }
        let base = kept.iter().map(|(_, v)| *v).min()?;
        *clips = kept
            .into_iter()
            .map(|(mut cc, v)| {
                cc.track_offset = v as i64 - base as i64;
                cc
            })
            .collect();
        base
    };
    // 落とし先では常に独立コピー (§5.6)。**元タブへ戻して落としても独立コピー** —
    // 一致する id を載せたままだと、同じタブへ戻したときだけリンクした写しになり、
    // 「運んだ先で元を編集したら中身が変わった」が起きる (track / cell と同じ扱い)。
    envelope.source_project_id = 0;
    let grab_beat = px_to_beat(anchor_mouse.0, f.lanes.x, f.lanes.w, f.view);
    let grab_visible =
        track_index_from_y(anchor_mouse.1, f.lanes.y, &f.tops).unwrap_or(base_visible);
    Some(ProjectTransferPayload {
        envelope,
        grab_beat_offset: (grab_beat - base_beat).max(0.0),
        grab_track_offset: grab_visible.saturating_sub(base_visible),
    })
}

/// 運んでいるランチャーのセル群を envelope に写す (copy と同じ写し)。
///
/// **行差だけは表示行 (`f.rows`) の差に直して運ぶ。** `launcher_cells_copy` の行差は
/// 曲の全行 (`all_launcher_rows`) の index 差 (Ctrl+C / Ctrl+V が畳み方に依らず
/// 同じ行へ貼るための基準) だが、持ち込みのゴーストも着地も **見えている行**で解く
/// (`docs/plan_project_tabs.md` §5.6)。直さないと、畳んだグループや展開したレーンの
/// ぶんだけプレビューと落ちる行がずれる。
fn cell_payload(
    app: &AppData,
    f: &ArrangementFrame<'_>,
    cells: &[LauncherCellKey],
) -> Option<ProjectTransferPayload> {
    let mut copy = app.launcher_cells_copy(cells)?;
    let all = app.all_launcher_rows();
    let visible_of = |row: crate::event_launcher::LauncherRow| -> Option<usize> {
        let key = match row {
            crate::event_launcher::LauncherRow::Track(id) => ArrangementRowKey::Track(id),
            crate::event_launcher::LauncherRow::Lane(k) => ArrangementRowKey::Lane(k),
        };
        f.rows.iter().position(|r| r.key == key)
    };
    // 原点 = 掴んだ群の一番上の行 (`launcher_cells_copy` の row_offset の基準)。
    let min_all = cells.iter().filter_map(|c| all.iter().position(|r| *r == c.row())).min()?;
    let base_visible = visible_of(*all.get(min_all)?)?;
    for c in &mut copy {
        let all_i = usize::try_from(min_all as i64 + c.row_offset).ok()?;
        c.row_offset = visible_of(*all.get(all_i)?)? as i64 - base_visible as i64;
    }
    let mut envelope =
        app.envelope_with_media(crate::clipboard::ClipboardPayload::LauncherCells(copy));
    envelope.source_project_id = 0;
    Some(ProjectTransferPayload { envelope, grab_beat_offset: 0.0, grab_track_offset: 0 })
}

/// 運んでいるトラック群を envelope に写す (plugin state は Song に保存済みのもの)。
/// グループを掴んだら **子トラックごと** 運ぶ (アレンジ内の並べ替えと同じ)。
fn track_payload(app: &AppData, ids: &[u32]) -> Option<ProjectTransferPayload> {
    let mut with_children: Vec<u32> = Vec::new();
    for id in ids {
        for t in app.collect_track_subtree_ids(*id) {
            if !with_children.contains(&t) {
                with_children.push(t);
            }
        }
    }
    let copy = app.collect_track_copies(&with_children);
    if copy.tracks.is_empty() {
        return None;
    }
    // 落とし先では常に独立コピー (§5.6): 元 project と一致しない id (0) を載せる。
    let mut envelope = app.envelope_with_media(crate::clipboard::ClipboardPayload::Tracks(copy));
    envelope.source_project_id = 0;
    Some(ProjectTransferPayload { envelope, grab_beat_offset: 0.0, grab_track_offset: 0 })
}
