//! r.md #127: **ボタンを押したまま Esc = ドラッグのキャンセル。**
//!
//! `Ui::drag_cancel_requested()` が立ったフレームに、生きている drag session を全部捨てる。
//! commit-by-release の session (クリップ移動 / 範囲 / 投げ縄 / セル運搬 …) は捨てるだけで
//! 何も起きない。**per-frame で値を流していた session** (音量スライダ / レーン高 / 行高 /
//! ヘッダ幅 / 帯幅 / 列幅 / プレイヘッド scrub) は press 時の値へ戻す — 戻さないと
//! 「Esc を押したのに途中の値が残る」 = キャンセルになっていない。
//!
//! 押しっぱなしのボタン (`launcher.held_button`) は触らない — Gate モードの「離すと停止」は
//! 通常の release 経路に任せる (キャンセルしたいのはドラッグであってボタンの押下ではない)。

use common::model::LauncherLayout;

use super::*;

/// `drag::advance` の前に呼ぶ。キャンセルしたフレームは以降の continuation / release が
/// session を見つけられないので、そのまま何も起きない。
pub(super) fn on_escape(ui: &mut Ui<'_, AppData>, f: &ArrangementFrame<'_>) {
    if !ui.drag_cancel_requested() {
        return;
    }
    let restores: Vec<Edit<AppData>> = {
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        let mut edits: Vec<Edit<AppData>> = Vec::new();

        // ---- per-frame emit していた session: anchor へ戻す ----
        if let Some(tv) = state.track_volume_drag.take() {
            let amp = MeterScale::default().frac_to_amp(tv.anchor_volume.clamp(0.0, 1.0));
            let track = tv.track_id;
            edits.push(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetTrackVolume { track, amp });
            }));
        }
        if let Some(rd) = state.automation_lane_resize_drag.take() {
            edits.push(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLaneHeight {
                    track_id: rd.lane.track,
                    lane_id: rd.lane.lane,
                    prev_px: rd.anchor_height_px,
                    next_px: rd.anchor_height_px,
                });
            }));
        }
        if let Some(rd) = state.track_row_resize_drag.take() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let px = rd.anchor_row_h.round().clamp(1.0, f32::from(u16::MAX)) as u16;
            edits.push(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSingleTrackRowH {
                    track_id: rd.track,
                    prev_px: px,
                    next_px: px,
                });
            }));
        }
        if let Some(hd) = state.header_resize_drag.take() {
            edits.push(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetArrangeHeaderW(hd.anchor_header_w));
            }));
        }
        if let Some(beat) = state.playhead_drag.take().and_then(|pd| pd.anchor_beat) {
            edits.push(Edit::mutate(move |app: &mut AppData| {
                app.seek_playhead_to(beat);
            }));
        }
        if let Some(d) = state.launcher.pane_width_drag.take() {
            edits.push(Edit::mutate(move |app: &mut AppData| {
                app.ui_prefs.launcher_layout = d.anchor_layout;
                if d.anchor_layout == LauncherLayout::Both {
                    app.ui_prefs.launcher_width = d.anchor_pane_w;
                }
            }));
        }
        if let Some(d) = state.launcher.col_width_drag.take() {
            edits.push(Edit::mutate(move |app: &mut AppData| {
                app.ui_prefs.launcher_scene_col_w = d.anchor_col_w;
            }));
        }

        // ---- commit-by-release の session: 捨てるだけ ----
        state.clip_drag = None;
        state.range_drag = None;
        state.loop_drag = None;
        state.track_reorder = None;
        state.audio_drag = None;
        state.automation_point_drag = None;
        state.automation_clip_drag = None;
        state.automation_lasso_drag = None;
        state.automation_segment_bend = None;
        state.section_drag = None;
        state.edge_scroll_press = None;
        state.launcher.scene_reorder = None;
        state.launcher.cell_drag = None;
        edits
    };
    for e in restores {
        ui.push_edit(e);
    }
}
