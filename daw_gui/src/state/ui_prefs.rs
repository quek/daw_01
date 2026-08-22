// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! S3b-1: AppData state group (UiPrefs)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

pub struct UiPrefs {
    /// docs/plan_video.md P4: video preview window の表示フラグ。 menu
    /// "View → Video Preview" / shortcut で toggle、 runner が毎フレーム
    /// この値を見て第二 winit::Window を create / destroy する。 false で
    /// 起動するので video import 前は preview は出ない (= MV 開始時は
    /// preview 不要、 user が import 後に明示的に開く)。
    pub preview_window_visible: bool,
    /// 折り畳み中の group track id 集合。 group 自身が `kind == Group`
    /// (= 子を持つ) かつこの set に含まれていれば子孫の row を hide。
    pub collapsed_groups: std::collections::HashSet<u32>,
    /// gui_01 #028 (M14 Phase 63n-1): automation lane 群を **展開中** の
    /// track id 集合 (Bitwig 流: 既定は折り畳み)。 含まれない track の
    /// `automation_lanes_collapsed = true` を widget へ渡す。 ▶/▼ click
    /// で `ToggleTrackAutomationCollapsed` イベント経由に insert/remove。
    /// プロジェクト保存対象ではない (= session-only): UI 状態は再起動で
    /// 既定 (全 collapsed) に戻る。
    pub expanded_automation_tracks: std::collections::HashSet<u32>,
    /// gui_01 #034 (Phase 63n-10): master row の automation 展開状態。
    /// `expanded_automation_tracks` と直交した 1 bool で持つ (= track id
    /// 集合に MASTER_TRACK_ID を入れる方式は sentinel が混ざって SSoT が
    /// 曖昧、 master 専用 field の方が intent が明瞭)。 起動時 false、
    /// `ToggleTrackAutomationCollapsed { track_id: MASTER_TRACK_ID }` で flip。
    /// session-only / Undo / save 対象外。
    pub master_row_automation_expanded: bool,
    /// gui_01 #031 (M14 Phase 63n-6): track ごとの row 高さ override。
    /// `Some(px)` で個別 track 高さ、`None` (= map に entry なし) で
    /// global default `arrange_track_row_h` を使う。 widget の Alt+drag
    /// or 下端 splitter drag で `SetSingleTrackRowH` 発火 → ここに反映。
    /// Alt+wheel は引き続き global を変える (`SetTrackRowH`)。
    /// session-only (= save / Undo 対象外、 必要になったら `Track.row_h`
    /// として model 化する)。
    pub track_row_overrides: std::collections::HashMap<u32, u16>,

    // -------- View state --------
    pub bottom_panel: u8,
    /// (B13 r.md #8) Audio Editor の波形 縦 gain (振幅表示の拡大率)。 Alt+wheel で
    /// 増減。 session-only (描画スケールのみ、 model / 音声には非影響)。
    pub audio_editor_vertical_gain: f32,
    /// Audio Editor の表示状態を **クリップごと** (`ClipKey`) に記憶する
    /// (Ableton Live / Bitwig 流)。 entry が無い (= 初回) クリップは
    /// `open_audio_editor` がクリップ全長を見せる初期 view を入れる。 値域は
    /// `audio_editor_view_state` / `set_audio_editor_*` 経由で読み書きし、 描画前に
    /// clip 長で clamp する。 `ViewState.audio_editor_views` として永続化される。
    pub audio_editor_views:
        std::collections::HashMap<common::model::ClipKey, common::model::AudioEditorViewState>,
    pub arrange_zoom_x: f32,
    pub arrange_scroll_beat: f32,
    /// 再生中プレイヘッド追従スクロールの方式 (Off / Scroll / Page、 `Alt+F` で循環)。
    /// `ViewState` でプロジェクト単位に保存 (snap 設定と同じ idiom)。 再生中に
    /// ユーザーが手動で横スクロール / ズームすると `Off` に落ちる (ユーザー選択)。
    /// `on_tick` が再生中のみこの mode に応じて `arrange_scroll_beat` を更新する。
    pub arrange_follow: common::model::FollowMode,
    /// arrangement の縦 scroll offset (px、 smooth)。 `0.0` で first track
    /// が lanes 上端、 wheel scroll で増減。 widget 側で `SetTrackTop` を
    /// 発火するので handler がここに書き込む。 overscroll (lanes 領域
    /// 外への描画) の scissor は widget 側 (gui_01 #048) の責務。
    pub arrange_track_top: f32,
    /// arrangement の 1 track row 高さ (px)。Alt+wheel で 16..96 に縦ズーム。
    /// default は `ARRANGE_TRACK_HEIGHT`。
    pub arrange_track_row_h: f32,
    /// automation lane の行高 session override (= `Z` 縦ズームで選択
    /// automation clip のレーンを画面いっぱいに拡大した一時値。 model の
    /// `AutomationLane.height_px` は保存対象なので汚さず、 これで上書き表示する)。
    /// 該当 lane の splitter resize (`set_lane_height`) と `X` (zoom back) で解除。
    /// `track_row_overrides` の lane 版。 session-only (save / Undo 対象外)。
    pub automation_lane_row_overrides:
        std::collections::HashMap<common::model::AutomationLaneKey, u16>,
    /// arrangement の track header 幅 (px、 default 160.0)。 header と
    /// lanes の境界 (右端 splitter) drag で gui_01 arrangement widget が
    /// `SetHeaderW` を発火 → `SetArrangeHeaderW` 経由でここを更新する。 widget は
    /// 毎フレーム `view.header_w` としてこの値を読む。 session-only (= save /
    /// Undo 対象外、 `arrange_track_row_h` と同じ扱い)。
    pub arrange_header_w: f32,
    /// ピアノロールの表示状態を **クリップごと** (`ClipKey`) に記憶する
    /// (Ableton Live / Bitwig 流)。 旧来のフラットな `pianoroll_zoom_x` 等は撤去し、
    /// `selected_clip` (= 現在ピアノロールで開いているクリップの `ClipKey`) で引く
    /// accessor (`pianoroll_zoom_x()` / `pianoroll_zoom_y()` / `pianoroll_top_pitch()` /
    /// `pianoroll_scroll_beat()`) に一本化した (= 重複所有を作らない、 SSoT)。 entry が
    /// 無い (= 初回選択) クリップは `select_clip` が `fit_piano_roll_to_clip` で埋める。
    /// `ViewState.piano_roll_views` として永続化される。
    pub piano_roll_views:
        std::collections::HashMap<common::model::ClipKey, common::model::PianoRollViewState>,
    /// **複数クリップ同時表示**中の共有 viewport (song-absolute scroll)。
    /// 単一表示は per-clip 永続 `piano_roll_views` を使うが、複数表示は表示クリップ集合に
    /// 依存する 1 つの transient viewport を使う (非永続)。`multi_clip_view_key` と組で持ち、
    /// 表示クリップ集合が変わったら union bbox に再 fit する。
    pub multi_clip_view: common::model::PianoRollViewState,
    /// `multi_clip_view` がどの表示クリップ集合 (`shown_pianoroll_clips` の
    /// `ClipKey` 列) に対して fit 済かを記録。draw でこれと現在の集合が違えば再 fit。
    pub multi_clip_view_key: Vec<common::model::ClipKey>,
    /// ピアノロールで「ロック (参照専用)」にした **トラック** (track id)。lock された
    /// トラックの (表示中) note は淡色ゴーストで描画され、hit-test / 選択 / 編集から除外される。
    /// 凡例がトラック単位なのでロックもトラック単位 (そのトラックの表示クリップ全部に効く)。
    /// session 内 transient (非永続)。legend のロックトグルで増減。
    pub locked_pr_tracks: std::collections::HashSet<u32>,
    /// FL Studio の smart length 互換: 直近に作成 / リサイズ / クリック選択した
    /// ノートの長さ (拍)。次の新規追加時のデフォルト長として使う。session 内
    /// in-memory のみ、永続化はしない。`add_note` / `resize_notes` /
    /// `SetNoteSelection` ハンドラで更新。
    pub last_note_duration_beats: f64,

    // -------- Grid snap state --------
    /// piano_roll の Snap on/off (Snap toggle / `G` キー)。
    pub pianoroll_snap_enabled: bool,
    /// `view::snap::SNAP_LABELS` の index。`view::snap::choice_to_mode` で SnapMode に変換。
    pub pianoroll_snap_choice: u8,
    pub arrange_snap_enabled: bool,
    pub arrange_snap_choice: u8,
    /// status bar の常駐メーター表示 on/off (app_config.json で永続化)。
    pub resource_monitor_enabled: bool,

    /// r.md #29: 編集履歴 window が開いているか (app_config で永続、 再起動を跨いで
    /// 復元)。 View メニュー / Ctrl+Alt+Z / Esc / ✕ で toggle。
    pub undo_history_open: bool,
    /// r.md #29: 編集履歴 window の位置・サイズ (app_config で永続)。 `None` =
    /// 未配置 (初回は既定の右上)。 drag / resize 確定時に更新して保存する。
    pub undo_history_rect: Option<daw_ui_renderer::Rect>,

    /// r.md #54: ラウドネスレポート window が開いているか (app_config で永続)。
    /// 解析メニュー / `Ctrl+L` / Esc / ✕ で toggle。
    pub loudness_report_open: bool,
    /// r.md #54: ラウドネスレポート window の位置・サイズ (app_config で永続)。
    pub loudness_report_rect: Option<daw_ui_renderer::Rect>,

    /// r.md #48: 設定 window が開いているか (app_config で永続)。
    /// Edit メニュー「設定...」 / Esc / ✕ で toggle。
    pub settings_open: bool,
    /// r.md #48: 設定 window の位置・サイズ (app_config で永続)。 `None` = 未配置。
    pub settings_rect: Option<daw_ui_renderer::Rect>,

    /// r.md #50: 画面右端のマスターパネルを出すか (app_config で永続)。
    /// View メニュー / `Ctrl+Alt+M` で toggle。
    pub master_panel_open: bool,
    /// マスターパネルの幅 (px、app_config で永続)。左端ドラッグで変わる。
    pub master_panel_w: f32,
    /// マスターパネルのセクション高さ配分 (MASTER / スペクトラム / オシロ / ゴニオ)。
    /// 合計 1.0 に正規化されている。境界ドラッグで変わる。
    pub master_panel_sections: [f32; 4],
    /// 各メーターの設定 (右クリックメニューで変える、app_config で永続)。
    /// テレメトリスレッドの解析器へは `AppData::meter_control` 経由で渡る。
    pub meter_settings: crate::master_meter::settings::MeterSettings,

    pub is_help_open: bool,

    /// r.md #60: ヘルプ > バージョン情報 (About) が開いているか。
    /// GPLv3 §0 の Appropriate Legal Notices を表示する画面。 `is_help_open` と同じく
    /// セッション内だけの状態で、 app_config には永続しない (起動のたびに開いても困る)。
    pub is_about_open: bool,

    /// per-user データディレクトリ (recent / recent_saved / recovery /
    /// window_state の永続化先) の **Single Source of Truth**。 production は
    /// `AppDirs::production()` (= `%LOCALAPPDATA%/daw_01/`)、 test は
    /// `AppDirs::under(tempdir)` か `None`。 `None` は「永続化しない」 を
    /// 意味し、 実ユーザー状態を汚染しない (= dispatcher と同じ DI パターン)。
    pub app_dirs: Option<common::app_dirs::AppDirs>,
    /// 「最近開いたファイル」 (= Open ダイアログ / OpenRecent 経由で読み込んだ
    /// .daw)。 File メニュー「Open Recent ►」 に表示。 永続化先は
    /// `app_dirs.recent()` (= `%LOCALAPPDATA%/daw_01/recent.json`)。
    pub recent_files: crate::recent::RecentFiles,
    /// 「最近保存したファイル」 (= Save / Save As で書き込んだ先)。 File
    /// メニュー「Recently Saved ►」 に表示。 永続化先は
    /// `app_dirs.recent_saved()` (= `%LOCALAPPDATA%/daw_01/recent_saved.json`)。
    /// 開いた履歴と分離して「保存先だけ覚えておく」 UX を提供する。
    pub recent_saved: crate::recent::RecentFiles,
    /// `recent_files` の filename だけ抽出したキャッシュ。 gui_01 `menu_bar`
    /// API が label に `&'a str` を要求し、 'a が `Ui` の borrow 寿命
    /// (= `&AppData` の寿命) と一致するため、 label 文字列も AppData 内に
    /// 持っておく必要がある。 frame 内で `&app.recent_files_labels[i]` を
    /// 渡せば lifetime が解決する。 `push_recent` / load 時に更新。
    pub recent_files_labels: Vec<String>,
    /// `recent_saved` の filename キャッシュ。 同じ理由。
    pub recent_saved_labels: Vec<String>,

    /// Phase 7 B5 (`docs/plan_scale.html` §5.1): Snap on Draw toggle。 ON のとき
    /// piano_roll で note 追加時の pitch を `Song.scale_at(beat).snap(pitch)` で
    /// in-scale に寄せる。 piano_roll header の toggle で切替、 session-only
    /// state (project save しない)。 Highlight mode が前提 (Fold mode は
    /// widget 側で既に in-scale pitch を push する)。
    pub snap_on_draw: bool,
    /// Phase 7 B5 (`docs/plan_scale.html` §4.4): piano_roll が Fold mode か。
    /// `true` で out-of-scale 行を非表示 (Ableton K キー Fold to Scale 相当)、
    /// `false` で Highlight mode (root 行強調 + in-scale 通常 + out 行 dim)。
    /// piano_roll snap toolbar の「Fold」 toggle で切替、 session-only state。
    /// `Song.scale_changes` が空のときは `view.scale = None` で機能 OFF。
    pub piano_roll_fold: bool,
}
