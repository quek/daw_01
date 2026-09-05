//! 共有グループ「連動ハイライト」 (gui_01 #068 / daw_01 #086 / r.md #91)。
//!
//! アレンジのクリップ・automation lane の clip・ランチャー帯のセルが **同じ印** で光る。
//! 印そのもの ([`push_active_group_overlay`]) と、アレンジ側の走査 ([`draw_active_group_overlay`])
//! をここに置く。ランチャー帯は `launcher::draw` が同じ `push_active_group_overlay` を呼ぶ。
//! `draw.rs` がサイズ budget (不変条件 9) の天井に居るので、同じ責務でまとめて切り出した。

use super::*;

/// M14 Phase 96 (daw_01 #068): 共有グループ「連動ハイライト」overlay。
/// `clip.in_active_group == true` かつ `share_group_color.is_some()` の clip に、 selection
/// (黄塗り) とは **別レイヤ** の強調 (glow wash + bright thick border) を重ねる。
/// M14 Phase 114 (daw_01 #086): 強調色は **identity-neutral** な `share_group_active_color` に変更
/// (旧: グループ hue を流用)。 #086 で clip fill が user 指定色になったため、 hue wash だと user の色と
/// 喧嘩する。 hover 中は 1 グループしか強調しないので色でグループを区別する必要は無い。
///
/// - **`in_active_group == false` / `share_group_color == None` の clip は一切描画しない**
///   (= 既存挙動と pixel 完全一致、 常に false で渡せば移行安全、 非 share clip は強調しない defensive)。
/// - **selection overlay より前** に呼ぶ: 選択中の同グループ member は黄塗りが上書き優先され
///   (#068 の「黄塗り優先で OK」)、 非選択 member が neutral 強調の主役になる。
/// - **cached 外で毎フレーム描画**: active group は hover / 選択で毎フレーム変わるため
///   viewport_key (heavy cache key) には含めない (hover 由来の変化で heavy cache を無効化しない =
///   selection overlay と同 idiom)。 描画は `draw_clips` / `draw_selection_overlay` と同じ culling。
pub(super) fn draw_active_group_overlay<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
) {
    let view_end = view.start_beat + view.len_beats;
    for (i, t) in visible_tracks.iter().enumerate() {
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        // 行 (clip 帯) と lane 群は縦に別領域なので culling も別々 — 行だけ画面上に
        // 出ていて lane が下に隠れている / その逆、 のどちらも起きる。
        let row_visible = row_top + row_h >= lanes.y && row_top <= lanes.y + lanes.h;
        for c in &t.clips {
            if !row_visible || !c.in_active_group {
                continue;
            }
            // share group member (= `share_group_color.is_some()`) でなければ強調しない
            // (video clip 等は share_group_color = None、 defensive)。 M14 Phase 114 (#086) で hue 値は
            // 強調色に使わなくなったが、 「リンクされた clip だけ」 を強調する guard は維持する。
            if c.share_group_color.is_none() {
                continue;
            }
            let end = c.start_beat + c.len_beats;
            if end < view.start_beat || c.start_beat > view_end {
                continue;
            }
            let r = clip_to_rect(row_top, row_h, c, view, lanes);
            if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                continue;
            }
            push_active_group_overlay(hctx, r, style, Some(lanes));
        }
        // r.md #91: automation lane の clip も同じ集合・同じ印で光らせる (lane 走査は
        // `draw_automation_selection_overlay` と同じ = 描画位置の SSoT `automation_clip_rect`)。
        if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
            continue;
        }
        let mut lane_y = row_top + row_h;
        for lane in &t.automation_lanes {
            if !lane.visible {
                continue;
            }
            let lh = f32::from(lane.height_px);
            let body_rect = Rect { x: lanes.x, y: lane_y, w: lanes.w, h: lh };
            lane_y += lh;
            if body_rect.y + lh < lanes.y || body_rect.y > lanes.y + lanes.h {
                continue;
            }
            for c in &lane.clips {
                if !c.in_active_group || c.share_group_color.is_none() {
                    continue;
                }
                let r = automation_clip_rect(body_rect, view, c.start_beat, c.len_beats, style);
                if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                    continue;
                }
                push_active_group_overlay(hctx, r, style, Some(lanes));
            }
        }
    }
}

/// 共有グループ「連動ハイライト」の印 1 つ分 (glow wash + 明るい中立枠)。
///
/// アレンジのクリップ / automation clip / ランチャー帯のセルが **同じこの 1 本** を呼ぶ
/// (r.md #91): 面が違っても「同じ content を共有している」印は 1 つで、 style トークン
/// (`share_group_active_*`) の解釈を 2 か所に持たない。
///
/// M14 Phase 114 (daw_01 #086): 強調色は **identity-neutral** な `share_group_active_color`
/// (bright 中立色)。 #086 で clip fill が user 指定色になったため、 旧 hue wash だと
/// ユーザの選んだ色と喧嘩する (hover 中は 1 グループしか強調しない = どのグループかを色で
/// 区別する必要が無い)。 selection の黄塗りとは別レイヤの「明度上げ + 明るい中立枠」。
pub(super) fn push_active_group_overlay<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    style: &ArrangementStyle,
    clip_rect: Option<Rect>,
) {
    // (1) glow wash: neutral color を低 alpha で clip 全体に敷いて「明るくする」。 alpha=0 なら
    //     no-op (= ring のみの強調)。 透明 fill push を避けるため alpha>0 の時だけ積む。
    if style.share_group_active_glow_alpha > 0.0 {
        let ac = style.share_group_active_color;
        let glow = Color { r: ac.r, g: ac.g, b: ac.b, a: style.share_group_active_glow_alpha };
        hctx.push_rect(RectCommand {
            rect: r,
            fill: glow,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [style.clip_radius; 4],
            clip_rect,
        });
    }
    // (2) bright thick border: 同 neutral color を太枠で outline。 透明 fill なので
    //     clip 名 / 既存 fill は隠さず、 枠だけ強調 (= 「束ねられている」 印象)。
    if style.share_group_active_border_w > 0.0 {
        hctx.push_rect(RectCommand {
            rect: r,
            fill: Color::TRANSPARENT,
            border: style.share_group_active_color,
            border_width: style.share_group_active_border_w,
            radius: [style.clip_radius; 4],
            clip_rect,
        });
    }
}
