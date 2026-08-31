//! ランチャー帯の drag 継続 (`advance`) と session の take。
//!
//! **`last_mouse` は release フレームでは更新しない。** winit は release で
//! `pointer.pos` を press 位置へ巻き戻すことがあり、そのまま上書きすると移動量が 0 に
//! なって「元に戻る」ように見える (アレンジ側の全 session と同じ規約)。
//! `last_ctrl` / `last_shift` も同じ理由で release フレームは据え置く
//! (`ModifiersChanged` が `MouseInput(Released)` より先に届く race)。

use super::*;

/// drag 継続 + 帯の view 状態 (幅 / 列幅 / 横スクロール) の per-frame 反映。
pub(crate) fn advance(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    update_sessions(ui, f);
    emit_pane_width(ui, f);
    emit_col_width(ui, f);
    scroll_scenes(ui, f);
}

fn update_sessions(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let is_release = f.pointer.primary_just_released;
    let Some(pos) = f.pointer.pos else { return };
    let m = f.pointer.modifiers;
    let state: &mut ArrangementState = ui.widget_state(f.wid);
    let s = &mut state.launcher;
    if is_release {
        return;
    }
    if let Some(d) = s.cell_drag.as_mut() {
        d.last_mouse = pos;
        d.last_ctrl = m.ctrl;
        d.last_shift = m.shift;
        d.last_alt = m.alt;
    }
    if let Some(d) = s.scene_reorder.as_mut() {
        d.last_mouse = pos;
        d.last_ctrl = m.ctrl;
        d.last_shift = m.shift;
    }
}

/// 帯幅 drag の per-frame 反映。**端まで寄せたらレイアウトそのものを切り替える**
/// (左端 = アレンジのみ / 右端 = ランチャーのみ、計画書 Q5)。
///
/// **「表示に使う幅」と「『両方』として覚える幅」を分ける。** ポインタは掴み代
/// ([`layout::pane_w_bounds`]) まで連続に追従するが、`ui_prefs.launcher_width` へ書くのは
/// 両側が実用幅で収まる範囲 ([`layout::both_pane_w_bounds`]) に居るあいだだけ。
/// 分けないと、端まで引く途中の「格子が 1 列も入らない幅」で最後の書き込みが起き、
/// `Tab` で「両方」へ戻したとき掴み代だけの帯が出る (計画書 Q5-b の
/// 「『両方』の比率は覚えている」が成り立たない)。
fn emit_pane_width(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let Some((px, _)) = f.pointer.pos else { return };
    if f.pointer.primary_just_released {
        return;
    }
    let avail = (f.rect.w - f.header_w).max(1.0);
    let (lo, hi) = layout::pane_w_bounds(avail);
    let mut emit: Option<f32> = None;
    {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        if let Some(d) = state.launcher.pane_width_drag.as_mut() {
            let next = (d.anchor_pane_w + (px - d.anchor_mouse_x)).clamp(lo, hi);
            if (next - d.last_emitted_w).abs() >= 0.5 {
                d.last_emitted_w = next;
                emit = Some(next);
            }
        }
    }
    let Some(next) = emit else { return };
    let (both_lo, both_hi) = layout::both_pane_w_bounds(avail);
    // 「両方」として成立しない幅まで引いたら、その時点で端へ吸着する
    // (`|| next <= lo + 1.0` は窓が狭くて両側の最低幅を取れないときの退化ケース)。
    let layout = if next < both_lo || next <= lo + 1.0 {
        LauncherLayout::ArrangerOnly
    } else if next > both_hi || next >= hi - 1.0 {
        LauncherLayout::LauncherOnly
    } else {
        LauncherLayout::Both
    };
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.ui_prefs.launcher_layout = layout;
        if layout == LauncherLayout::Both {
            app.ui_prefs.launcher_width = next;
        }
    }));
}

/// 列幅 drag の per-frame 反映 (全列共通)。
fn emit_col_width(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let Some((px, _)) = f.pointer.pos else { return };
    if f.pointer.primary_just_released {
        return;
    }
    let mut emit: Option<f32> = None;
    {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        if let Some(d) = state.launcher.col_width_drag.as_mut() {
            let next =
                (d.anchor_col_w + (px - d.anchor_mouse_x)).clamp(MIN_COL_W, MAX_COL_W);
            if (next - d.last_emitted_w).abs() >= 0.5 {
                d.last_emitted_w = next;
                emit = Some(next);
            }
        }
    }
    let Some(next) = emit else { return };
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.ui_prefs.launcher_scene_col_w = next;
    }));
}

/// `Shift` + ホイールで列を横スクロールする。
///
/// **`Shift` を押していないホイールは消費しない** — 素のホイールは行の縦スクロール
/// (`release::commit_releases` の wheel ブロックが `header_pane ∪ 帯 ∪ lanes` で拾う) で、
/// 帯の上でも行が動くのが正しい。アレンジ側の `Shift` = 横スクロールと同じ語彙。
fn scroll_scenes(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    if !f.pointer.modifiers.shift || f.launcher.collapsed || f.launcher.col_w <= 0.0 {
        return;
    }
    let Some((px, py)) = f.pointer.pos else { return };
    if !f.launcher.pane.contains(px, py) {
        return;
    }
    let (dx, dy) = ui.take_scroll_in_rect(f.launcher.pane);
    // 横成分があればそれを、無ければ縦成分を横へ倒す (ホイールしか無いマウス用)。
    let delta = if dx.abs() > 0.0 { dx } else { -dy };
    if delta.abs() <= 0.0 {
        return;
    }
    let cur = f.launcher.scroll_scene;
    // 実シーンが 0 でも右にはプレースホルダ列が並んでいるので、最低 1 列ぶんは
    // スクロールできる (0 だと「空の曲では横スクロールが一切効かない」)。
    #[allow(clippy::cast_precision_loss)]
    let max = (f.launcher_view.scenes.len() as f32).max(1.0);
    let next = (cur - delta / f.launcher.col_w).clamp(0.0, max);
    if (next - cur).abs() < 1e-4 {
        return;
    }
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.ui_prefs.launcher_scroll_scene = next;
    }));
}

/// シーン並べ替えの落とし先 (表示 index)。**overlay の指標線と commit が同じこの 1 本を通る**
/// ので、線が指す位置と着地位置が必ず一致する。
#[must_use]
pub(super) fn drop_scene_index(f: &ArrangementFrame<'_>, x: f32) -> usize {
    let n = f.launcher_view.scenes.len();
    if n == 0 {
        return 0;
    }
    let l = &f.launcher;
    // 列の**中心**を境に着地先を決める (中心より右なら 1 つ右の列)。
    // `Song::move_scene` は remove → insert なので、指標線を「列の左端」に出す
    // だけだと右方向のドラッグで 1 列ずれて着地する。
    let rel = ((x - l.grid.x) / l.col_w.max(1.0) + l.scroll_scene).max(0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let raw = (rel + 0.5).floor() as usize;
    raw.min(n.saturating_sub(1))
}

/// session の overlay 用スナップショットと release take。
pub(crate) fn take(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) -> LauncherSessions {
    let released = f.pointer.primary_just_released;
    let state: &mut ArrangementState = ui.widget_state(f.wid);
    let s = &mut state.launcher;
    let mut out = LauncherSessions {
        live_cell_drag: s.cell_drag.clone(),
        live_scene_reorder: s.scene_reorder,
        // 押下中の見た目に使うので **take せず覗くだけ** (take は release 時のみ)。
        live_held_button: s.held_button,
        ..LauncherSessions::default()
    };
    if released {
        out.released_cell_drag = s.cell_drag.take();
        out.released_scene_reorder = s.scene_reorder.take();
        out.released_button = s.held_button.take();
        // 幅 / 列幅は per-frame で最終値まで書き終わっているので take して捨てるだけ
        // (アレンジの `header_resize_drag` / `track_row_resize_drag` と同 idiom)。
        s.pane_width_drag = None;
        s.col_width_drag = None;
    }
    out
}
