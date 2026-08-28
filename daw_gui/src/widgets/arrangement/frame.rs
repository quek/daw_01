//! arrangement widget の 1 フレームの「地形」 (`ArrangementFrame`) の構築と、
//! レイアウトの `AppData` へのミラー。
//!
//! `ArrangementFrame` は読み取り専用で、 全フェーズが `&ArrangementFrame` で受ける。
//! 可変な状態は `ArrangementState` (widget state) と `ArrangementResponse` が持つ。

use super::*;
// `BuiltArrangement` は `view_build.rs` の `pub(super)`。 `mod.rs` は
// `pub(crate) mod view_build;` を宣言するだけで re-export していないので、 `use super::*` では
// 型名が入らない (旧 `run.rs` は `let built = view_build::build(app, area);` と型名を書かずに
// 済ませていたので、 この問題は分割して初めて表面化する)。
use super::view_build::BuiltArrangement;

/// 1 フレーム分の「地形」。 `arrangement()` の各フェーズが共有する読み取り専用の束。
///
/// 旧実装ではこれが 20 個以上のローカル変数として 2,000 行を跨いで生存し、 フェーズを
/// 切り出すたびに引数が増えていた (`render.rs` 37 引数 / `release.rs` 33 引数)。
///
/// **可変な状態は入れない。** フェーズを跨ぐ可変状態は `ArrangementState` (widget state) と
/// `ArrangementResponse` が持つ。
pub(super) struct ArrangementFrame<'a> {
    // ---- 入力ビュー (`view_build::build` の結果からの借用) ----
    /// caller の全 track list (visible filter 前)。 `is_group_set` 生成と `resolve_track_drop` が使う。
    pub tracks: &'a [ArrangementTrack],
    pub sections: &'a [SectionView],
    pub view: ArrangementView,
    pub style: &'a ArrangementStyle,
    /// `Option` を落とさないこと — `commit_releases` が `Option<&ArrangementMasterRow>` で受ける。
    pub master_row: Option<&'a ArrangementMasterRow>,
    pub selected_clips: &'a [ClipKey],
    pub selected_tracks: &'a [u32],
    pub selected_automation_clips: &'a [AutomationClipKey],
    pub selected_automation_points: &'a [AutomationPointKey],

    // ---- rect 分割 ----
    pub rect: Rect,
    pub header_pane: Rect,
    pub ruler: Rect,
    pub arranger_rect: Rect,
    pub arranger_header_rect: Rect,
    pub lanes: Rect,
    pub header_w: f32,
    pub arranger_lane_h: f32,
    pub lanes_h: f32,

    // ---- 尺度 ----
    pub beat_per_px: f64,
    pub zoom_x_px_per_beat: f32,

    // ---- 行モデル ----
    /// collapsed 親配下を除外し、 先頭に synthetic master row を prepend した描画順の行。
    /// **描画 / hit-test / release / rect 収集がすべてこの 1 本を共有する** (旧 `visible_tracks` /
    /// `tracks_for_draw` / `tracks_owned` の 3 つ名は同一内容だった)。
    pub visible_tracks: Vec<ArrangementTrack>,
    /// `visible_tracks` の prefix-sum row top。 **`header_pane.y == lanes.y` は rect 分割から
    /// 自明に成り立つので、 header 側と lanes 側で同一。**
    /// 旧 `press_tops` / `header_tops` / `tops_owned_for_heavy` の 3 重計算をこの 1 本に統合。
    pub tops: Vec<f32>,
    /// 「他 track の parent_id として参照されている id」 の集合 (= group 判定)。
    /// **caller の full `tracks` から作る** — `visible_tracks` から作ると collapsed で子が
    /// filter され group 判定が false 化する。
    pub is_group_set: HashSet<u32>,

    // ---- widget identity / 入力 ----
    pub wid: WidgetId,
    pub id: &'static str,
    /// このフレームの pointer スナップショット (`PointerFrame` は Copy、 `ui` を借りない)。
    pub pointer: daw_ui_core::PointerFrame,
}

/// `BuiltArrangement` + caller の `area` + `ui` の pointer スナップショットから 1 フレームの
/// 地形を組む。
pub(super) fn build<'a>(
    built: &'a BuiltArrangement,
    area: Rect,
    ui: &Ui<'_, AppData>,
) -> ArrangementFrame<'a> {
    // S4b: 入力ビューを AppData から直接構築 (旧 mirror 型 + make_edit 翻訳層を撤去)。
    let tracks: &[ArrangementTrack] = &built.tracks;
    let sections: &[SectionView] = &built.sections;
    let view = built.view;
    let selected_clips: &[ClipKey] = &built.selected_clips;
    let selected_tracks: &[u32] = &built.selected_tracks;
    let selected_automation_clips: &[AutomationClipKey] = &built.selected_automation_clips;
    let selected_automation_points: &[AutomationPointKey] = &built.selected_automation_points;
    let style: &ArrangementStyle = &built.style;
    let master_row: Option<&ArrangementMasterRow> = Some(&built.master_row);
    let rect = area;
    let id = "arrangement";
    let wid = WidgetId::ROOT.child((b"arrangement_widget", &id));
    let pointer = ui.pointer();

    // ---- rect 分割 ----
    let header_w = view.header_w.max(0.0);
    let ruler_h = view.ruler_h.max(0.0);
    // M14 Phase 127 (daw_01 #105): Arranger レーンを ruler の直下・track lanes の上に確保する。
    // `arranger_lane_h == 0.0` で従来レイアウトと完全一致 (レーン無し)。 track lanes / header_pane の
    // y 原点を arranger 分だけ下げることで track row (header / lanes 双方) が自動的に下にずれる
    // (`header_pane.y == lanes.y` の不変条件は維持 = tops を header / lanes で共有する前提)。
    let arranger_lane_h = view.arranger_lane_h.max(0.0);
    let lanes_h = (rect.h - ruler_h - arranger_lane_h).max(1.0);
    let lanes_w = (rect.w - header_w).max(1.0);
    let header_pane =
        Rect { x: rect.x, y: rect.y + ruler_h + arranger_lane_h, w: header_w, h: lanes_h };
    let ruler = Rect { x: rect.x + header_w, y: rect.y, w: lanes_w, h: ruler_h };
    // Arranger レーン本体 (lanes 幅、 ruler 直下) と header 側の見出し領域 ("Arranger" ラベル用)。
    let arranger_rect =
        Rect { x: rect.x + header_w, y: rect.y + ruler_h, w: lanes_w, h: arranger_lane_h };
    let arranger_header_rect =
        Rect { x: rect.x, y: rect.y + ruler_h, w: header_w, h: arranger_lane_h };
    let lanes = Rect {
        x: rect.x + header_w,
        y: rect.y + ruler_h + arranger_lane_h,
        w: lanes_w,
        h: lanes_h,
    };

    // M9 Phase 45f / M14 Phase 63j (#024): snap 用 zoom = lanes.w / view.len_beats。
    // press 振り分け (ruler の playhead seek) でも snap 計算に必要なため、 後の overlay 計算と
    // 共有する目的で 1 度計算する。
    let beat_per_px = view.len_beats / f64::from(lanes.w.max(1.0));
    #[allow(clippy::cast_possible_truncation)]
    let zoom_x_px_per_beat: f32 = (1.0 / beat_per_px) as f32;

    // ---- M14 Phase 63c (#016): visible 領域 (collapsed 親の subtree skip) を pre-compute ----
    // press / drag / release / draw すべてが visible-domain の row index で動くように、
    // `tracks` (caller's 全 list) を visible-only に絞った Vec を作って以降で共有する。
    // `clip_to_rect` / `track_index_from_y` の `track_index` 引数は visible-idx と解釈される。
    let visible_indices: Vec<usize> = compute_visible_indices(tracks);
    // M14 Phase 63n-10 (#034): master_row を synthetic `ArrangementTrack` (id = `MASTER_TRACK_ID`、
    // clips 空、 mute/solo false、 automation_lanes は master_row から複製) として `visible_tracks[0]`
    // に prepend。 既存 hit-test / 描画コードを **そのまま reuse** できる (= clips が空なので
    // MIDI/Audio clip drag は自然に no-op、 automation_lanes は通常 track と同 schema)。 「Master」
    // ラベル描画 / mute/solo button 非表示 / clip 系 EditRequest 抑制は描画 / 押下 path で
    // `t.id == MASTER_TRACK_ID` 分岐を入れて対処。
    //
    // `visible_indices` は **caller's tracks の index 列**で master の caller index は無いため
    // この Vec は変更しない (= 後段の clone source は `tracks` だが master 経路は別ロジック)。
    let mut visible_tracks: Vec<ArrangementTrack> =
        visible_indices.iter().map(|&i| tracks[i].clone()).collect();
    if let Some(master) = master_row {
        visible_tracks.insert(0, synthesize_master_track(master));
    }
    // M14 Phase 63n-1 (#028): visible track の prefix-sum row tops。 lane 0 個 (= 既存挙動)
    // では `tops[i] = lanes.y - track_top + i * track_row_h` と等価。 expand 中の lane 群が
    // ある track 以降は次 track 以降の row top が下にずれる (= 描画 / hit-test SSoT)。
    // M14 Phase 63n-10 (#034): `visible_tracks[0]` に master_row が prepend されていれば、 master の
    // 高さ + lanes 高さ込みの prefix sum が自動で組まれる (= 通常 track と同じ helper を再利用)。
    let tops = visible_track_row_tops(&visible_tracks, lanes.y, view.track_top, view.track_row_h);
    // M14 Phase 63c (#016): collapsed 後でも「Group A は子を持つ track」 と判定するため、
    // **caller の full `tracks`** から「他 track の parent_id として参照されている id 集合」 を 1 度計算。
    // `is_group_track(id, visible_tracks)` だと collapsed で children が filter outされ false 化する罠を回避。
    let is_group_set: HashSet<u32> = tracks.iter().filter_map(|t| t.parent_id).collect();

    ArrangementFrame {
        tracks,
        sections,
        view,
        style,
        master_row,
        selected_clips,
        selected_tracks,
        selected_automation_clips,
        selected_automation_points,
        rect,
        header_pane,
        ruler,
        arranger_rect,
        arranger_header_rect,
        lanes,
        header_w,
        arranger_lane_h,
        lanes_h,
        beat_per_px,
        zoom_x_px_per_beat,
        visible_tracks,
        tops,
        is_group_set,
        wid,
        id,
        pointer,
    }
}

/// r.md #63: auto-fit (`X` / Fit ボタン) と縦ズーム (`Z`) 用に、 このフレームの **実レイアウト** を
/// `app.ui_ephemeral` にミラーし、 `response.arranger_rect` / `lanes_rect` / `rows` を埋める。
/// 差分があるときだけ `push_edit` する。
///
/// **lanes 高さを式で再導出しないこと** — `area.h - RULER_H` で再導出して Arranger 帯 18px を
/// 引き忘れたのが r.md #63 の症状 (`daw_gui/tests/arrange_fit_layout.rs` が回帰テスト)。
/// lanes サイズは rect 分割した `f.lanes` そのもの (= 描画 / hit-test が scissor に使う矩形と
/// 同一)。 行の一覧も同様にモデルから再導出せず、 widget が積んだ行そのものを渡す。
pub(super) fn mirror_layout(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    response: &mut ArrangementResponse,
) {
    let lanes_size = (f.lanes.w, f.lanes.h);
    if app.ui_ephemeral.last_arrange_lanes_size != lanes_size {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.last_arrange_lanes_size = lanes_size;
        }));
    }
    let rows = arrangement_row_layout(&f.visible_tracks, f.view.track_row_h);
    if app.ui_ephemeral.last_arrange_rows != rows {
        let next = rows.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.last_arrange_rows = next;
        }));
    }
    response.arranger_rect = f.arranger_rect;
    response.lanes_rect = f.lanes;
    response.rows = rows;
}
