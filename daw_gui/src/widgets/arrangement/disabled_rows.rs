//! r.md #131: 実効的に無効なトラックの行を丸ごと沈める overlay (`docs/plan_rmd_131_track_disable.md` Q2)。
//!
//! 沈めるのはトラック行とその展開中のオートメーションレーン行の **レーン帯とランチャーのセル**
//! (ヘッダは `header::draw_rows` が自分の行に重ねる)。ランチャー主導の行の減光
//! (`launcher::draw::dim_launcher_rows`) と同じトークン `Palette::row_dim_ink` を使い、行の縦位置は
//! ランチャー帯と同じ `f.rows` 1 本から読む (行ズレが構造的に起きない)。クリップ / セルの描画より後
//! (= `run.rs` でランチャー帯の後) に重ねるので、クリップもグリッドも等しく沈む。

use super::launcher::layout::row_screen_top;
use super::*;

/// 無効トラックの行に `row_dim_ink` を重ねる。無効トラックが 1 本も見えていなければ何も積まない。
pub(super) fn draw(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    let disabled: HashSet<u32> = f.visible_tracks.iter().filter(|t| t.disabled).map(|t| t.id).collect();
    if disabled.is_empty() {
        return;
    }
    let track_of = |key: ArrangementRowKey| match key {
        ArrangementRowKey::Track(id) => id,
        ArrangementRowKey::Lane(k) => k.track,
    };
    // ランチャー帯が畳まれている (セルを描かない) / 無い (幅 0) ときは帯側を沈めない。
    let panes = [Some(f.lanes), (!f.launcher.collapsed).then_some(f.launcher.grid)];
    ui.heavy(("arrangement_disabled_rows", &f.id), |hctx| {
        let ink = hctx.palette().row_dim_ink;
        for pane in panes.into_iter().flatten().filter(|p| p.w > 0.0 && p.h > 0.0) {
            hctx.with_clip_rect(pane, |hctx| {
                for row in f.rows.iter().filter(|r| disabled.contains(&track_of(r.key))) {
                    let top = row_screen_top(f, row);
                    if top + row.height < pane.y || top > pane.y + pane.h {
                        continue;
                    }
                    push_filled_rect(hctx, Rect { x: pane.x, y: top, w: pane.w, h: row.height }, ink);
                }
            });
        }
    });
}
