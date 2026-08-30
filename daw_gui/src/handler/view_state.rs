//! handler::view_state — `ViewState` (保存される「見方の都合」) の snapshot / restore。
//!
//! r.md #87 でランチャー帯の 4 値が加わり `selection_view.rs` が実コード 1,000 行 budget
//! (不変条件 9) の天井を越えたので、**足す前に**この 1 対を切り出した。
//! ここが「保存する表示状態」の唯一の口で、書く側 (`snapshot_view_state`) と読む側
//! (`restore_view_state`) が必ず隣り合う — 片方に足してもう片方を忘れる事故を防ぐ。
//!
//! **`Song` (曲の中身) はここを通らない。** ここに置くのは「変えても `*` が立たない」
//! ものだけ (memory `project_dirty_flag_rule`)。

use crate::app_types::*;
use crate::state::*;

impl AppData {
    /// 現在の表示状態 (ズーム / スクロール / 行高 / スナップ等 + per-clip view) を
    /// `ViewState` にスナップショットする。 save / autosave 時に呼ぶ。 per-clip map は
    /// 現存しないクリップの orphan entry を GC して書き出す。
    pub fn snapshot_view_state(&self) -> common::model::ViewState {
        let mut expanded: Vec<u32> = self.ui_prefs.expanded_automation_tracks.iter().copied().collect();
        expanded.sort_unstable();
        let mut piano_roll_views: Vec<(common::model::ClipKey, common::model::PianoRollViewState)> =
            self.ui_prefs.piano_roll_views
                .iter()
                .filter(|(k, _)| self.live_clip_key(**k).is_some())
                .map(|(k, v)| (*k, *v))
                .collect();
        piano_roll_views.sort_by_key(|(k, _)| (k.track_id, k.clip_id));
        let mut audio_editor_views: Vec<(
            common::model::ClipKey,
            common::model::AudioEditorViewState,
        )> = self
            .ui_prefs.audio_editor_views
            .iter()
            .filter(|(k, _)| self.live_clip_key(**k).is_some())
            .map(|(k, v)| (*k, *v))
            .collect();
        audio_editor_views.sort_by_key(|(k, _)| (k.track_id, k.clip_id));
        // Fit / `Z` 縦ズームが張った lane 行高。**現存するレーンの分だけ**書き出し
        // (消えたレーンの orphan を溜めない)、キー順で並べて save 差分を安定させる。
        let mut lane_row_overrides: Vec<(common::model::AutomationLaneKey, u16)> = self
            .ui_prefs
            .automation_lane_row_overrides
            .iter()
            .filter(|(k, _)| {
                self.song_doc.song().automation_lane_by_key(k.track, k.lane).is_some()
            })
            .map(|(k, v)| (*k, *v))
            .collect();
        lane_row_overrides.sort_unstable_by_key(|(k, _)| (k.track, k.lane));
        // r.md #65: エディタ窓のジオメトリ。per-clip view と同じく **現存する
        // device の分だけ**を書き出し (削除済み device の orphan を溜めない)、
        // device_id 昇順で並べて save 差分を安定させる。
        let mut plugin_editor_windows: Vec<(u64, common::model::EditorWindowGeometry)> = self
            .ui_prefs
            .plugin_editor_windows
            .iter()
            .filter(|(id, _)| find_device_by_id(self.song_doc.song(), **id).is_some())
            .map(|(id, g)| (*id, *g))
            .collect();
        plugin_editor_windows.sort_unstable_by_key(|(id, _)| *id);
        common::model::ViewState {
            arrange_zoom_x: self.ui_prefs.arrange_zoom_x,
            arrangement_split_ratio: self.ui_prefs.arrangement_split_ratio,
            arrange_scroll_beat: self.ui_prefs.arrange_scroll_beat,
            arrange_follow: self.ui_prefs.arrange_follow,
            // 再生ループ (ON/OFF + 範囲) は transport が live SSoT。 ここへ書くことで
            // 「dirty は立てないが保存される」 (= ズーム / スクロールと同じ扱い)。
            loop_region: self.transport.loop_region,
            arrange_track_top: self.ui_prefs.arrange_track_top,
            arrange_track_row_h: self.ui_prefs.arrange_track_row_h,
            arrange_header_w: self.ui_prefs.arrange_header_w,
            track_row_overrides: self.ui_prefs.track_row_overrides.clone(),
            expanded_automation_tracks: expanded,
            automation_lane_row_overrides: lane_row_overrides,
            master_row_automation_expanded: self.ui_prefs.master_row_automation_expanded,
            arrange_snap_enabled: self.ui_prefs.arrange_snap_enabled,
            arrange_snap_choice: self.ui_prefs.arrange_snap_choice,
            pianoroll_snap_enabled: self.ui_prefs.pianoroll_snap_enabled,
            pianoroll_snap_choice: self.ui_prefs.pianoroll_snap_choice,
            piano_roll_fold: self.ui_prefs.piano_roll_fold,
            snap_on_draw: self.ui_prefs.snap_on_draw,
            snap_live_input: self.recording.snap_live_input,
            bottom_panel: self.ui_prefs.bottom_panel,
            piano_roll_views,
            audio_editor_views,
            plugin_editor_windows,
            // r.md #87: ランチャー帯の見せ方 / 幅 / 列幅 / 横スクロール。
            // 「見方の都合」なので保存はするが dirty は立てない。
            launcher_layout: self.ui_prefs.launcher_layout,
            launcher_width: self.ui_prefs.launcher_width,
            launcher_scene_col_w: self.ui_prefs.launcher_scene_col_w,
            launcher_scroll_scene: self.ui_prefs.launcher_scroll_scene,
        }
    }

    /// load 時に `ViewState` を AppData へ流し込む。 別プロジェクトの per-clip
    /// view が漏れないよう **必ず先に map をクリア**。 `view = None` (旧ファイル /
    /// view 未保存) なら globals は現状維持 (= 従来の fit-to-content / 既定値挙動)。
    /// 全値を有効域へ clamp して壊れた / 古い保存値を吸収する。
    ///
    /// `loop_region` は `view` と**別引数**で受ける: v28 以前の `.daw` は `ViewState`
    /// を持たないのにループ範囲は持つので、 loader
    /// ([`common::project::LoadedProject::loop_region`]) が両方を解決した値を渡す。
    /// ここで `view` から取ると、 旧ファイル用に `ViewState::default()` を合成する
    /// 羽目になり globals が既定値へ潰れる。 engine への `SetLoop` 送出も込みで
    /// [`AppData::set_loop_region`] を通す (= 復元忘れの故障モードを作らない)。
    pub fn restore_view_state(
        &mut self,
        view: Option<common::model::ViewState>,
        loop_region: common::model::LoopRegion,
    ) {
        self.ui_prefs.piano_roll_views.clear();
        self.ui_prefs.audio_editor_views.clear();
        // r.md #65: 別プロジェクトの窓位置が漏れないよう per-clip view と同様に先にクリア。
        self.ui_prefs.plugin_editor_windows.clear();
        self.set_loop_region(loop_region);
        let Some(v) = view else { return };
        let max_choice = (crate::view::snap::SNAP_LABELS.len() as u8).saturating_sub(1);
        self.ui_prefs.arrange_zoom_x = v.arrange_zoom_x.clamp(2.0, 400.0);
        // `0.0` / NaN は「未設定」 (旧ファイル)。view 側が既定比率へ倒すので
        // ここで 0.05 に clamp しない (するとアレンジが潰れて開く)。
        self.ui_prefs.arrangement_split_ratio = if v.arrangement_split_ratio.is_finite() {
            v.arrangement_split_ratio.clamp(0.0, 0.95)
        } else {
            0.0
        };
        self.ui_prefs.arrange_scroll_beat = v.arrange_scroll_beat.max(0.0);
        self.ui_prefs.arrange_follow = v.arrange_follow;
        self.ui_prefs.arrange_track_top = v.arrange_track_top.max(0.0);
        self.ui_prefs.arrange_track_row_h =
            v.arrange_track_row_h.clamp(MIN_ARRANGE_ROW_H, MAX_ARRANGE_ROW_H);
        self.ui_prefs.arrange_header_w = v.arrange_header_w.clamp(80.0, 480.0);
        self.ui_prefs.track_row_overrides = v
            .track_row_overrides
            .into_iter()
            .map(|(k, h)| (k, h.max(16)))
            .collect();
        self.ui_prefs.expanded_automation_tracks = v.expanded_automation_tracks.into_iter().collect();
        // レーン行高は `after_song_replaced` が前 project ぶんを消した後にここで入れ直す
        // (消えたレーンのキーは捨てる)。下限だけ効かせるのは `track_row_overrides` と同じ。
        self.ui_prefs.automation_lane_row_overrides = v
            .automation_lane_row_overrides
            .into_iter()
            .filter(|(k, _)| {
                self.song_doc.song().automation_lane_by_key(k.track, k.lane).is_some()
            })
            .map(|(k, h)| (k, h.max(16)))
            .collect();
        self.ui_prefs.master_row_automation_expanded = v.master_row_automation_expanded;
        self.ui_prefs.arrange_snap_enabled = v.arrange_snap_enabled;
        self.ui_prefs.arrange_snap_choice = v.arrange_snap_choice.min(max_choice);
        self.ui_prefs.pianoroll_snap_enabled = v.pianoroll_snap_enabled;
        self.ui_prefs.pianoroll_snap_choice = v.pianoroll_snap_choice.min(max_choice);
        self.ui_prefs.piano_roll_fold = v.piano_roll_fold;
        self.ui_prefs.snap_on_draw = v.snap_on_draw;
        self.recording.snap_live_input = v.snap_live_input;
        self.ui_prefs.bottom_panel = v.bottom_panel;
        // r.md #87: ランチャー帯。負の値 / NaN は「未設定」へ潰し、widget の既定幅に任せる
        // (壊れた保存値で帯が消える / 画面外へ飛ぶのを防ぐ)。
        self.ui_prefs.launcher_layout = v.launcher_layout;
        self.ui_prefs.launcher_width = sanitize_launcher_px(v.launcher_width);
        self.ui_prefs.launcher_scene_col_w = sanitize_launcher_px(v.launcher_scene_col_w);
        self.ui_prefs.launcher_scroll_scene =
            if v.launcher_scroll_scene.is_finite() { v.launcher_scroll_scene.max(0.0) } else { 0.0 };
        // 選択 (時間範囲) は session-only なので復元しない
        // (`docs/plan_range_selection.md` §2.3)。
        for (k, mut pv) in v.piano_roll_views {
            pv.zoom_x = pv.zoom_x.clamp(8.0, 400.0);
            pv.zoom_y = pv.zoom_y.clamp(6.0, 40.0);
            pv.top_pitch = pv.top_pitch.clamp(11, 127);
            pv.scroll_beat = pv.scroll_beat.max(0.0);
            self.ui_prefs.piano_roll_views.insert(k, pv);
        }
        for (k, mut av) in v.audio_editor_views {
            // r.md #44: start_beat は content-local 軸で、左端を外へ伸ばした clip では
            // 負にもなる (= 0 で clamp しない)。描画側が窓で clamp し直す。
            if !av.start_beat.is_finite() {
                av.start_beat = 0.0;
            }
            av.len_beats = av.len_beats.max(0.0);
            self.ui_prefs.audio_editor_views.insert(k, av);
        }
        // r.md #65: エディタ窓のジオメトリ。現存しない device の stale entry は捨てる。
        // 位置 (`x`/`y`) は **clamp しない** — マルチモニタでは負値が正当で、
        // 画面外かどうかの判定はモニタ構成を知る plugin-host 側が open 時に行う。
        // サイズ 0 の entry は **1 へ clamp せず捨てる**: 0 は「最小化中に採られた
        // 縮退値」を意味し、1 に昇格させると「有効な 1×1 の窓サイズ」に化けて
        // 次回 open で 1×1 のエディタが出る。上限だけ健全域へ丸める。
        for (device_id, mut g) in v.plugin_editor_windows {
            if g.width == 0
                || g.height == 0
                || find_device_by_id(self.song_doc.song(), device_id).is_none()
            {
                continue;
            }
            g.width = g.width.min(16_384);
            g.height = g.height.min(16_384);
            self.ui_prefs.plugin_editor_windows.insert(device_id, g);
        }
    }
}


/// r.md #87: 保存された px 値を「未設定 (`0.0`)」か「正の px」に正す。
/// NaN / 負値が入ると帯が消えたり `Rect` が破綻するので、load の 1 か所で潰す。
fn sanitize_launcher_px(v: f32) -> f32 {
    if v.is_finite() && v > 0.0 { v } else { 0.0 }
}
