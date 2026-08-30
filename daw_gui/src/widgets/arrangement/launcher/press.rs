//! ランチャー帯の press 振り分けと、帯の上のカーソル形状。
//!
//! `arrangement::press::dispatch` の `splitter` の**直後**に呼ばれる。帯はヘッダ /
//! ルーラー / アレンジのレーンと x が排他なので、既存の分岐と競合するのは
//! ヘッダ境界スプリッタ (arrangement 全高に張る) だけで、それは `claim.splitter` が
//! 先に立って弾いてくれる。
//!
//! **ここでは `Song` も engine も触らない。** 起きたことは
//! [`LauncherIntent`] として widget state に積み、`release::commit` が
//! `ArrangementResponse` へ移す (`PressActions` と同じ「借用が閉じてから出す」idiom)。

use super::*;

/// 帯の中でポインタが乗っているもの。press と cursor が共有する 1 本。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Zone {
    /// 帯の右端 (帯幅を変える)。
    PaneSplitter,
    /// シーン見出しの列境界 (列幅を変える)。掴んだ列の表示 index。
    ColSplitter(usize),
    /// 見出し行の停止列 (= 全行停止)。
    GlobalStop,
    /// 見出し行の返す列 (= 全行をアレンジへ)。
    GlobalReturn,
    /// シーン見出しの ▶。
    SceneLaunch(u32),
    /// シーン見出しの本体 (ドラッグで並べ替え)。
    SceneBody { scene_id: u32, index: u32 },
    /// 行の停止列。
    RowStop(ArrangementRowKey),
    /// 行の返す列。
    RowReturn(ArrangementRowKey),
    /// セルの ▶。
    CellLaunch(LauncherCellKey),
    /// セルの本体 (選択 + ドラッグ)。
    CellBody(LauncherCellKey),
}

/// 帯の当たり判定。**描画と同じ rect helper (`layout`) を通す**ので、押せる場所と
/// 見えている場所が構造的に一致する。
#[must_use]
pub(super) fn zone_at(f: &ArrangementFrame<'_>, x: f32, y: f32) -> Option<Zone> {
    if f.launcher.pane.w <= 0.0 {
        return None;
    }
    if layout::pane_splitter_at(f, x, y) {
        return Some(Zone::PaneSplitter);
    }
    if !f.launcher.pane.contains(x, y) || f.launcher.collapsed {
        return None;
    }
    if let Some(i) = layout::col_splitter_at(f, x, y) {
        return Some(Zone::ColSplitter(i));
    }
    let l = &f.launcher;
    let in_head = y < l.head.y + l.head.h;
    if in_head {
        if l.stop_col.contains(x, y) {
            return Some(Zone::GlobalStop);
        }
        if l.return_col.contains(x, y) {
            return Some(Zone::GlobalReturn);
        }
        let i = l.col_index_at_x(x)?;
        // 実体の無い列 (= 空きプレースホルダ) は `scene_id = 0`。撃つと全行停止、
        // 並べ替えの対象にはならない (並び順を持たないため)。
        let scene_id = f.launcher_view.scenes.get(i).map_or(0, |s| s.id);
        let head_cell = Rect {
            x: l.col_x(i) + 1.0,
            y: l.scene_head.y + 1.0,
            w: (l.col_w - 2.0).max(2.0),
            h: (l.scene_head.h - 2.0).max(2.0),
        };
        let btn = layout::launch_button_rect(Rect {
            x: head_cell.x + 3.0,
            w: (head_cell.w - 3.0).max(2.0),
            ..head_cell
        });
        // **押せる場所は列の実体の有無で変わらない** — プレースホルダ列も実体のある
        // 列と見た目がほぼ同じ (名前も ▶ も出る) ので、当たり判定だけ違うと
        // 「同じに見えるのに押した結果が違う」になる。
        //
        // 以前はプレースホルダの本体ぜんぶを `SceneLaunch(0)` (= 全行停止) に倒して
        // いた。列の空き部分をクリックしただけで全行が止まり、しかも `launch_scene` が
        // transport を回すので「止めるつもりで再生が始まる」という形で出ていた。
        if btn.contains(x, y) {
            return Some(Zone::SceneLaunch(scene_id));
        }
        if scene_id == 0 {
            // 実体の無い列は並び順も id も持たないので、掴めず選べない。
            // 列を足したいときは右クリックメニュー (「シーンを追加」) が担う。
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        return Some(Zone::SceneBody { scene_id, index: i as u32 });
    }
    let row = layout::row_at_y(f, y)?;
    // マスター行はクリップを持たないので、停止 / 返す / セルのどれも押せない
    // (描画側も同じ条件で何も出さない)。
    if row.key == ArrangementRowKey::Track(MASTER_TRACK_ID) {
        return None;
    }
    if l.stop_col.contains(x, y) {
        return Some(Zone::RowStop(row.key));
    }
    if l.return_col.contains(x, y) {
        return Some(Zone::RowReturn(row.key));
    }
    let (key, rect) = layout::cell_at(f, x, y)?;
    // グループ行の「まとめセル」は本体ぜんぶが発火ボタン。グループトラックは自分の
    // クリップを鳴らさない (`process_track_owned` が `track_has_children` で pass 1 を
    // 抜ける) ので、選んだり運んだりする中身が無い。
    if f.launcher_view.rows.get(&row.key).is_some_and(|r| r.group) {
        return Some(Zone::CellLaunch(key));
    }
    // 空セルは **記号 (■ / ●) の上だけ**がボタン。中身のあるセルと同じ分割で、
    // 記号の外は「焦点を移すだけ」の本体になる (Live / Bitwig の空スロットと同じ)。
    //
    // 以前は空セルの本体ぜんぶをボタンにしていたが、空スロットのボタンは
    // 「その行を止める」なので、**列の空きを掴もうとしただけで再生が止まる**。
    // 押せる場所を記号に限れば、止めたいときだけ止まる。
    if key.is_empty() {
        return Some(if layout::launch_button_rect(rect).contains(x, y) {
            Zone::CellLaunch(key)
        } else {
            Zone::CellBody(key)
        });
    }
    if layout::launch_button_rect(rect).contains(x, y) {
        Some(Zone::CellLaunch(key))
    } else {
        Some(Zone::CellBody(key))
    }
}

/// グループ行のまとめセルを **子行のセル**へ展開する。グループ以外はそのまま 1 件。
///
/// 空セルの子も含めて返すので、「子が全部空の列を撃つ = 子を全部止める」が
/// そのまま成立する (計画書 Q11 の空セル = 停止)。
#[must_use]
pub(super) fn expand_group_cells(
    tracks: &[ArrangementTrack],
    view: &LauncherView,
    cell: LauncherCellKey,
) -> Vec<LauncherCellKey> {
    let ArrangementRowKey::Track(group_id) = cell.row else {
        return vec![cell];
    };
    if !view.rows.get(&cell.row).is_some_and(|r| r.group) {
        return vec![cell];
    }
    let col = cell.scene_index as usize;
    tracks
        .iter()
        .filter(|t| is_group_descendant(tracks, t.id, group_id))
        .map(|t| layout::cell_key(view, ArrangementRowKey::Track(t.id), col))
        .collect()
}

/// `id` の祖先に `ancestor` が居るか (collapsed でも親子関係は残るので full list で辿る)。
#[must_use]
pub(super) fn is_group_descendant(
    tracks: &[ArrangementTrack],
    id: u32,
    ancestor: u32,
) -> bool {
    let mut cur = tracks.iter().find(|t| t.id == id).and_then(|t| t.parent_id);
    for _ in 0..64 {
        let Some(pid) = cur else { return false };
        if pid == ancestor {
            return true;
        }
        cur = tracks.iter().find(|t| t.id == pid).and_then(|t| t.parent_id);
    }
    false
}

/// press 振り分け本体。`claim.splitter` を立てたフレームは、以降の
/// clip / ruler / header / automation 分岐が丸ごと止まる (既存のゲートをそのまま使う)。
pub(crate) fn dispatch(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
) {
    if claim.splitter {
        return;
    }
    let Some(zone) = zone_at(f, hit.px, hit.py) else {
        return;
    };
    let (px, py) = (hit.px, hit.py);
    let ctrl = hit.ctrl;
    let shift = hit.shift;
    let alt = f.pointer.modifiers.alt;
    let launch_cells = match zone {
        Zone::CellLaunch(cell) => expand_group_cells(f.tracks, f.launcher_view, cell),
        _ => Vec::new(),
    };
    let pane_w = f.launcher.pane.w;
    let col_w = f.launcher.col_w;
    let selected = selected_cells(f);
    let state: &mut ArrangementState = ui.widget_state(f.wid);
    let s = &mut state.launcher;
    match zone {
        Zone::PaneSplitter => {
            s.pane_width_drag = Some(PaneWidthDragSession {
                anchor_pane_w: pane_w,
                anchor_mouse_x: px,
                last_emitted_w: pane_w,
            });
            claim.splitter = true;
        }
        Zone::ColSplitter(_) => {
            s.col_width_drag = Some(ColWidthDragSession {
                anchor_col_w: col_w,
                anchor_mouse_x: px,
                last_emitted_w: col_w,
            });
            claim.splitter = true;
        }
        Zone::GlobalStop => {
            s.pending_intents.push(LauncherIntent::StopAllRows);
            claim.session = true;
        }
        Zone::GlobalReturn => {
            s.pending_intents.push(LauncherIntent::SwitchAllToArranger);
            claim.session = true;
        }
        Zone::RowStop(row) => {
            s.pending_intents.push(LauncherIntent::StopRow(row));
            claim.session = true;
        }
        Zone::RowReturn(row) => {
            s.pending_intents.push(LauncherIntent::SwitchRowToArranger(row));
            claim.session = true;
        }
        Zone::SceneLaunch(scene_id) => {
            s.pending_intents.push(LauncherIntent::LaunchScene { scene_id, pressed: true });
            s.held_button = Some(LauncherButton::Scene(scene_id));
            claim.session = true;
        }
        Zone::SceneBody { scene_id, index } => {
            s.scene_reorder = Some(SceneReorderSession {
                scene_id,
                anchor_index: index,
                anchor_mouse: (px, py),
                last_mouse: (px, py),
                last_ctrl: ctrl,
                last_shift: shift,
            });
            claim.session = true;
        }
        Zone::CellLaunch(cell) => {
            // 空セルは掴む実体が無いので選択には入らないが、**キーボードの起点
            // (焦点) は動かす** — でないと空セルを押しても矢印 / `Enter` の対象が
            // 前のままで、「押した場所と動く場所が違う」になる
            // (`launcher_bridge` が空セルの `SelectCell` を `FocusCell` に倒す)。
            if cell.is_empty() {
                s.pending_intents.push(LauncherIntent::SelectCell {
                    cell,
                    additive: false,
                    range: false,
                });
            }
            for c in launch_cells {
                s.pending_intents.push(LauncherIntent::Launch { cell: c, pressed: true });
            }
            s.held_button = Some(LauncherButton::Cell(cell));
            claim.session = true;
        }
        Zone::CellBody(cell) if cell.is_empty() => {
            // 空セルの本体には運ぶ実体が無いので drag は始めない。**焦点だけ移す**
            // (`launcher_bridge` が空セルの `SelectCell` を `FocusCell` に倒す) ので、
            // 矢印 / `Enter` の起点は押した場所に付いてくる。新規作成はダブルクリック。
            s.pending_intents.push(LauncherIntent::SelectCell {
                cell,
                additive: false,
                range: false,
            });
            claim.session = true;
        }
        Zone::CellBody(cell) => {
            // 掴んだセルが選択に含まれていれば選択全部を運ぶ (クリップの drag と同 idiom)。
            let cells = if selected.contains(&cell) { selected } else { vec![cell] };
            s.cell_drag = Some(CellDragSession {
                primary: cell,
                cells,
                anchor_mouse: (px, py),
                last_mouse: (px, py),
                last_ctrl: ctrl,
                last_shift: shift,
                last_alt: alt,
            });
            claim.session = true;
        }
    }
}

/// いま選択されているセル (このフレームの格子の上に居るものだけ)。
///
/// 選択 SSoT は arrangement と同じ `selected_clips` / `selected_automation_clips` で、
/// セルのクリップ id はアレンジのクリップと同じ id 空間なので**そのまま照合できる**。
fn selected_cells(f: &ArrangementFrame<'_>) -> Vec<LauncherCellKey> {
    let mut out = Vec::new();
    for (row_key, row) in &f.launcher_view.rows {
        for (scene_id, cell) in &row.cells {
            let Some(index) = f.launcher_view.scenes.iter().position(|s| s.id == *scene_id) else {
                continue;
            };
            #[allow(clippy::cast_possible_truncation)]
            let key = LauncherCellKey {
                row: *row_key,
                scene_index: index as u32,
                scene_id: *scene_id,
                clip_id: cell.clip_id,
            };
            let hit = key.clip_key().is_some_and(|k| f.selected_clips.contains(&k))
                || key
                    .automation_clip_key()
                    .is_some_and(|k| f.selected_automation_clips.contains(&k));
            if hit {
                out.push(key);
            }
        }
    }
    out
}

/// 帯の上のカーソル形状。`cursor::apply` の **後**に呼び、帯の上にいるときだけ上書きする
/// (アレンジ側の判定を壊さない)。
pub(crate) fn cursor(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let dragging = {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.launcher.pane_width_drag.is_some() || state.launcher.col_width_drag.is_some()
    };
    if dragging {
        ui.set_cursor(CursorIcon::EwResize);
        return;
    }
    let Some((x, y)) = f.pointer.pos else { return };
    match zone_at(f, x, y) {
        Some(Zone::PaneSplitter | Zone::ColSplitter(_)) => ui.set_cursor(CursorIcon::EwResize),
        // 空セルの本体は運ぶ実体が無い (焦点が動くだけ) ので Move を出さない。
        Some(Zone::CellBody(c)) if c.is_empty() => {}
        Some(Zone::SceneBody { .. } | Zone::CellBody(_)) => ui.set_cursor(CursorIcon::Move),
        _ => {}
    }
}
