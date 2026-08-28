//! track header pane (`f.header_pane`) の press 振り分け。 track 行 (volume band /
//! M·S·R 除外 / group disclosure / lane disclosure / reorder) と lane 行 (★/👁/✕) に分岐する。
//!
//! **popup が開いているフレームは丸ごと止める** (`ui.has_open_popups()`)。 context menu は
//! `capture_input == false` で背景 pointer を mask しないので、 menu item の press が背後の行に
//! 届いて volume band drag / reorder session が起動する (r.md #43 の同件)。

use super::*;

/// M10 Phase 46+47b: track header press 振り分け
///  - volume band 内 → TrackVolumeDragSession (priority 最高)
///  - 上記以外 + Name button area を含む row + M/S/Up/Dn/Del button rect 非 hit → reorder
///  - 16px 未満 drag は release で click 格下げ (track header click のトラック選択が代替)
///
/// M14 Phase 63n-2 (#028): track 行 と lane 行 で分岐。 lane 行 (= track 行下、 expanded のみ)
/// では lane header button (★/👁/✕) を扱う。
///
/// r.md #43 review: popup (右クリックメニュー) が開いている frame は header の
/// press 経路を丸ごと止める。 menu item の press が背後の行に届き
/// **volume band drag が起動して離した位置の音量に飛ぶ** / reorder session が
/// 始まる (release 側の選択ガードだけでは塞げない press 側の同件)。
///
/// **`f.header_w > 0.0` を落とさない** — `header_w == 0` のときは header pane 幅が 0 で、
/// このフェーズは丸ごと no-op になるのが現行挙動。
pub(super) fn dispatch(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
    actions: &mut PressActions,
) {
    let (px, py) = (hit.px, hit.py);
    if !(f.header_w > 0.0 && !ui.has_open_popups() && f.header_pane.contains(px, py)) {
        return;
    }
    let Some(idx) = track_index_from_y(py, f.header_pane.y, &f.tops) else { return };
    let Some(t) = f.visible_tracks.get(idx) else { return };
    // header_pane.y と lanes.y は同じ値 (rect 分割で y 軸 origin 共通) なので tops を共有可。
    let row_top = f.tops[idx];
    // M14 Phase 63n-6 (#031): per-track row 高さで track row 範囲を判定。
    let row_h_eff = effective_track_row_h(t, f.view.track_row_h);
    let track_row_bottom = row_top + row_h_eff;
    if py < track_row_bottom {
        track_row(ui, f, hit, claim, actions, t, row_top);
    } else if !t.automation_lanes_collapsed && !t.automation_lanes.is_empty() {
        lane_header(f, hit, actions, t, track_row_bottom);
    }
}

/// === track row press ===
///
/// M14 Phase 118 follow-up (#092 review): press 側の row も draw 側 `row_for_layout`
/// (Phase 63c #016 で導入) と **同じ indent** を適用する。 これまで press は非 indent の
/// header_pane 幅で volume band / M·S·R / disclosure / lane disclosure を hit-test して
/// いたため、 nested track (depth>0) で「描画位置 (indent 済) と press 判定がズレる」
/// pre-existing バグがあった (深ネスト group の indent 空白を click すると volume drag が
/// 起動する / 描画済ボタンの click が reorder に化ける 等)。 draw と同 indent にして
/// press↔draw を SSoT 化 (depth==0 は indent=0 で byte 完全互換)。
///
/// `track_volume_drag` / `track_reorder` はどちらも 11 列挙内なので、 起動したら
/// `claim.session = true`。
fn track_row(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    claim: &mut PressClaim,
    actions: &mut PressActions,
    t: &ArrangementTrack,
    row_top: f32,
) {
    let (px, py) = (hit.px, hit.py);
    let style = f.style;
    let indent = f32::from(t.depth) * style.indent_px;
    let row = Rect {
        x: f.header_pane.x + indent,
        y: row_top,
        w: (f.header_pane.w - indent).max(2.0),
        h: f.view.track_row_h,
    };
    let band_h = if matches!(t.kind, TrackKind::Video) || t.id == MASTER_TRACK_ID {
        // M14 Phase 72 (#044): video track では volume slider band を非表示
        // (volume / pan は video には意味を持たない、 instrument / fx_chain と同様)。
        // master row も描画側 (`header_row_layout(row, 0.0)`) と揃えて band 無し —
        // 揃えないと不可視の volume drag が起動して
        // `SetTrackVolume{track:MASTER}` を emit + カーソルが EwResize 化 (review)。
        0.0
    } else {
        style.track_volume_band_h
    };
    let layout = header_row_layout(row, band_h);
    if let Some(band) = layout.volume_band
        && band.contains(px, py)
    {
        // band は frac 空間 (0..1 = MeterScale 上の位置)。 stored amp を
        // frac に写して anchor にすることで、 release の frac 比較 /
        // 描画と整合する (r.md #11。 旧 `amp.clamp(0,1)` は amp を frac と
        // 誤用し、 +6dB 側で頭打ち + release で fill が飛んでいた)。
        let av = MeterScale::default().amp_to_frac(t.volume);
        let track_id = t.id;
        let state: &mut ArrangementState = ui.widget_state(f.wid);
        state.track_volume_drag = Some(TrackVolumeDragSession {
            track_id,
            anchor_volume: av,
            band_rect: band,
            anchor_mouse_x: px,
            last_mouse_x: px,
            last_emitted_volume: av,
        });
        claim.session = true;
    } else {
        let in_small_button = layout.buttons.iter().any(|b| b.contains(px, py));
        // M14 Phase 63c (#016): disclosure rect の click は track_reorder セッションを
        // 起動しない (折り畳み toggle のみ、 release frame 別経路で Edit 発行)。
        let in_disclosure = f.is_group_set.contains(&t.id)
            && disclosure_rect_for(layout.name_rect, style, t.depth).contains(px, py);
        // M14 Phase 63n-1 (#028) + 63n-2 修正: lane disclosure hit zone は
        // **`layout.lane_disc_rect`** を使う (S button の **右**、 button と非 overlap)。
        // 旧 `lane_disclosure_rect_for(row, style)` (= track 行の右端内側) は S button
        // と完全 overlap して描画後勝ちで `+`/`-` が覆われる bug 持ちだった (#028 user
        // feedback で「`+`/`-` が見えない」)。 layout SSoT に統一して描画と hit-test
        // が同 rect を参照する。
        let in_lane_disclosure =
            !t.automation_lanes.is_empty() && layout.lane_disc_rect.contains(px, py);
        if in_lane_disclosure {
            actions.lane_toggle = Some(t.id);
        } else if !in_small_button && !in_disclosure && t.id != MASTER_TRACK_ID {
            // M14 Phase 63c (#016): multi-select 中の drag は selected_tracks をまとめて
            // 移動するため、 source_track_ids に selected を全部入れる (clicked が selected
            // に含まれていなければ単独 drag = `vec![clicked]`)。
            // M14 Phase 63n-10 (#034): master row は reorder 対象外 (= 上端固定、 daw_01
            // #034 §A 仕様)。 anchor_track_id に MASTER_TRACK_ID が入ると `arr_tracks` に
            // 該当 id が存在しない → caller の reorder 実装が空振りする (= 結果 no-op だが
            // session 立ち上げ自体が無駄、 明示的に skip)。
            let source_ids: Vec<u32> = if f.selected_tracks.contains(&t.id) {
                f.selected_tracks.to_vec()
            } else {
                vec![t.id]
            };
            let anchor_track_id = t.id;
            let state: &mut ArrangementState = ui.widget_state(f.wid);
            state.track_reorder = Some(TrackReorderSession {
                anchor_track_id,
                source_track_ids: source_ids,
                anchor_mouse_y: py,
                last_mouse_y: py,
                anchor_mouse_x: px,
                last_mouse_x: px,
            });
            claim.session = true;
        }
    }
}

/// === lane header press (Phase 63n-2) ===
///
/// lane 群を上から積んで cursor py が当たる lane を見つけ、 button rect を判定する。
/// invisible lane は積まない。
///
/// `ui` を受け取らない: このフェーズは session を 1 つも起動せず (= `claim` も立てない)、
/// `Edit` は `actions.lane_button` に貯めて `press::dispatch` が発行する。
fn lane_header(
    f: &ArrangementFrame<'_>,
    hit: &PressHit,
    actions: &mut PressActions,
    t: &ArrangementTrack,
    track_row_bottom: f32,
) {
    let (px, py) = (hit.px, hit.py);
    let style = f.style;
    let header_indent = f32::from(t.depth) * style.indent_px;
    let mut lane_y = track_row_bottom;
    for lane in &t.automation_lanes {
        if !lane.visible {
            continue;
        }
        let lh = f32::from(lane.height_px);
        if py >= lane_y && py < lane_y + lh {
            let lane_key = AutomationLaneKey { track: t.id, lane: lane.id };
            let header_rect = Rect {
                x: f.header_pane.x + header_indent,
                y: lane_y,
                w: (f.header_pane.w - header_indent).max(2.0),
                h: lh,
            };
            if let Some(layout) = automation_lane_header_layout(header_rect, style) {
                if layout.enabled_icon_rect.contains(px, py) {
                    let v_lane = lane_key;
                    let v_en = !lane.enabled;
                    actions.lane_button = Some(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetLaneEnabled {
                            track_id: v_lane.track,
                            lane_id: v_lane.lane,
                            enabled: v_en,
                        });
                    }));
                } else if layout.visible_icon_rect.contains(px, py) {
                    let v_lane = lane_key;
                    let v_vis = !lane.visible;
                    actions.lane_button = Some(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetLaneVisible {
                            track_id: v_lane.track,
                            lane_id: v_lane.lane,
                            visible: v_vis,
                        });
                    }));
                } else if layout.delete_icon_rect.contains(px, py) {
                    let v_lane = lane_key;
                    actions.lane_button = Some(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::DeleteLane {
                            track_id: v_lane.track,
                            lane_id: v_lane.lane,
                        });
                    }));
                }
                // default value フィールドの press は caller の
                // scrubable_number_at overlay が直接処理する (widget 内 band drag は廃止)。
            }
            break;
        }
        lane_y += lh;
    }
}
