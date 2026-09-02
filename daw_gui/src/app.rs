//! Bitwig-style DAW GUI state.
//!
//! 状態は 3 つに分けて持つ:
//!   1. **song** — `Track → Clip → Note` のツリー。あらゆる編集で mutate し、
//!      Play / clip-edit のたびに plugin_host へ push する。
//!   2. **selection** — 選択中の track / clip / notes。inspector・piano roll・
//!      lyric panel の入力源。
//!   3. **view state** — zoom / scroll / playhead / peak meter。
//!
//! gui_01 (daw-ui) は immediate-mode + `Edit<M>` クロージャ方式:
//! - 状態は plain mutable field
//! - 派生は method (`pub fn track_headers(&self) -> Vec<TrackHeader>` 等)
//! - background thread → UI event は `EventLoopProxy<AppEvent>` 経由

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::model::Song;
use common::plugin_db::PluginDatabase;
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::UnboundedSender;

use crate::audio_source_cache::AudioSourceCache;
use crate::dispatcher::{BackgroundDispatcher, JobDispatcher};





























































pub use crate::app_types::*;
pub use crate::event::{
    AppEvent, AudioEventTrimSide, DiscreteClipEdit, FadeEdgeKind, QuantizePitchTarget,
};

pub use crate::state::{
    AppData, DeviceParamKey, EditScope, IpcState, MediaState, RecordingState, ScrubGesture,
    SelectionState, SongDoc, StreamGesture, TransportState, UiEphemeral, UiPrefs, VoicevoxState,
};






impl AppData {
    // DI composition root: 全ての外部依存 (IPC sender / dispatcher / job /
    // plugin DB / supervisor / app_dirs) を注入する。 依存数が clippy の
    // 7-arg 閾値を超えるが、 composition root の性質上自然なので allow。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        audio_tx: UnboundedSender<AudioCommand>,
        plugin_tx: UnboundedSender<PluginCommand>,
        // 将来的な auto-select 用に予約。現在は song に反映していない。
        _clap_plugin_path: Option<PathBuf>,
        plugin_db: Option<Arc<PluginDatabase>>,
        event_proxy: Arc<dyn BackgroundDispatcher>,
        voicevox_job: Arc<dyn JobDispatcher>,
        supervisor: Option<Arc<crate::bootstrap::ChildSupervisor>>,
        app_dirs: Option<common::app_dirs::AppDirs>,
        // (A1 r.md #8) 解決済みデバイス実サンプルレート (= bootstrap.sample_rate)。
        sample_rate: u32,
    ) -> Self {
        let mut song = Song::default();
        // **id は必ず allocator から採る。** `Track::default()` の `id` は
        // 「未採番」の sentinel (0) で、0 を実トラックの住所として使うと
        // `RowKey::packed()` が 0 になり、`audio_bridge` の「空きスロット =
        // row_key 0」規約と衝突する (= ランチャーの走行状態が 1 行も GUI へ
        // 届かず、セルの進捗が永久に出ない)。起動直後の 1 本目でそれを踏んでいた。
        let first_track_id = song.alloc_track_id();
        song.tracks.push(track_with(|t| {
            t.id = first_track_id;
            t.name = "Track 1".into();
        }));
        // 起動時の初期プロジェクトにも安定 project_id を採番する
        // (clipboard の同一プロジェクト判定用)。
        song.ensure_project_id();
        let initial_peak_display = vec![(0.0, 0.0, 0.0); song.tracks.len()];
        let initial_bpm = song.bpm;
        let initial_time_sig_num = song.time_sig.0;
        let recovery_candidates = app_dirs
            .as_ref()
            .map(|d| common::recovery::scan_recovery_files(&d.recovery_dir()))
            .unwrap_or_default();
        let recent_files =
            load_recent_list(app_dirs.as_ref().map(|d| d.recent()));
        let recent_saved =
            load_recent_list(app_dirs.as_ref().map(|d| d.recent_saved()));
        let show_recovery_modal = !recovery_candidates.is_empty();
        if show_recovery_modal {
            tracing::info!(
                count = recovery_candidates.len(),
                "recovery candidates found at startup"
            );
        }
        let plugin_picker_entries = plugin_db
            .as_ref()
            .map(|db| {
                let mut v: Vec<PluginPickEntry> =
                    db.entries.iter().map(PluginPickEntry::from_db_entry).collect();
                v.sort_by_key(|e| e.name.to_lowercase());
                v
            })
            .unwrap_or_default();

        // プロジェクト非依存のアプリ設定は **1 回だけ** 読む (旧実装はフィールドごとに
        // 同じ JSON を 3 回 load していた)。 `app_dirs == None` (テスト) では既定値。
        let app_config = app_dirs
            .as_ref()
            .map(|d| crate::app_config::load(d.app_config()))
            .unwrap_or_default();
        // r.md #48: 保存された id からテーマを解決する。ファイルが消えていても
        // `resolve` が既定テーマにフォールバックするので起動できる。
        let theme = crate::theme::resolve(
            app_dirs.as_ref().map(common::app_dirs::AppDirs::themes_dir).as_deref(),
            &app_config.theme,
        );

        let app = Self {
            theme,
            // r.md #54: 解析はセッション限りなので既定 (Idle / レポート無し)。
            loudness: crate::state::LoudnessState::default(),
            song_doc: SongDoc::new(song),
            transport: TransportState {
                metronome_enabled: false,
                is_playing: false,
                preroll_remaining: 0,
                loop_region: common::model::LoopRegion::default(),
                playhead_beat: None,
                playback_origin_beat: None,
                panic_reinit_due: None,
                panic_release_pending: false,
                master_meter: crate::master_meter::MasterMeterSnapshot::default(),
                track_peak_display: initial_peak_display,
                master_strip_gr: (0.0, 0.0),
                mod_plane: common::mod_plane::ModPlane::default(),
                pending_play: false,
                pending_play_record: None,
                export_stage: None,
                export_progress_at: None,
                export_cancel: None,
                pending_video_export: None,
                export_temp_wav: None,
                pending_video_export_range: None,
                pending_video_export_dims: None,
                pending_export: None,
            },
            selection: SelectionState {
                selected_track_ids: Vec::new(),
                selected_section_ids: Vec::new(),
                selected_scene_ids: Vec::new(),
                selected_automation_clips: Vec::new(),
                last_edit_select: None,
                selected_automation_points: Vec::new(),
                time: None,
                range_anchor: None,
                selected_launcher_cells: Vec::new(),
                launcher_cell_anchor: None,
                selected_device_ids: Vec::new(),
                track_anchor: None,
                section_anchor: None,
                automation_point_anchor: None,
                automation_clip_anchor: None,
                device_anchor: None,
                scene_anchor: None,
            },
            ipc: IpcState {
                sample_rate,
                ara_doc_cache: std::collections::HashMap::new(),
                ara_pcm_materialized: std::collections::HashMap::new(),
                plugin_param_values: std::collections::HashMap::new(),
                plugin_params: std::collections::HashMap::new(),
                slot_has_gui: std::collections::HashMap::new(),
                loaded_devices: std::collections::HashMap::new(),
                metrics: common::metrics_bridge::ResourceMetrics::default(),
                metrics_bridge: None,
                plugin_db,
                pending_state_queue: VecDeque::new(),
                state_request_sent_at: None,
                audio_tx: Some(audio_tx),
                plugin_tx: Some(plugin_tx),
                pending_clip_fx_bounce: None,
                pending_glue_bake: None,
                pending_vocal_synth_bounce: None,
                pending_vocal_synth_export: std::collections::HashSet::new(),
                open_plugin_guis: std::collections::HashSet::new(),
                pending_plugin_loads: std::collections::HashMap::new(),
                next_plugin_load_generation: 0,
                failed_plugin_loads: std::collections::HashMap::new(),
                pending_added_plugin_finalize: std::collections::HashMap::new(),
                gui_open_requests: Vec::new(),
                rescan_result: Arc::new(Mutex::new(None)),
                supervisor,
                child_disconnect_log: Vec::new(),
                is_rescanning: false,
                last_synced_epoch: 0,
                event_proxy,
            },
            voicevox: VoicevoxState::new(voicevox_job),
            media: MediaState {
                audio_source_cache: AudioSourceCache::new(),
                video_thumbnail_rgba: std::collections::HashMap::new(),
                pending_thumbnail_uploads: Vec::new(),
                image_source_bgra: std::collections::HashMap::new(),
                pending_image_uploads: Vec::new(),
                asset_decode: None,
                load_progress: None,
                load_progress_label: "",
            },
            recording: RecordingState {
                recording_mode: common::model::RecordingMode::default(),
                requested: false,
                live: false,
                count_in_bars: 0,
                midi_recording_active_notes: std::collections::HashMap::new(),
                monitor_notes: std::collections::HashSet::new(),
                metronome_enabled_pre_recording: None,
                midi_learn_target: None,
                active_param_gestures: std::collections::HashSet::new(),
                latched_param_gestures: std::collections::HashSet::new(),
                recording_last_beat: std::collections::HashMap::new(),
                last_sent_recording_lanes: std::collections::HashSet::new(),
                preview_note: None,
                nudge_audition: None,
                midi_input_label: String::new(),
                step_cursor_beat: 0.0,
                step_size_beats: DEFAULT_NOTE_DURATION,
                snap_live_input: false,
            },
            ui_prefs: UiPrefs {
                strip_comp_open: false,
                strip_eq_open: false,
                preview_window_visible: false,
                // 既定 ON: クリップを動かしたら automation も付いてくる方が期待に近い。
                // アプリ設定 (`AppConfig`) から復元する。
                automation_follows_clips: app_config.automation_follows_clips,
                collapsed_groups: std::collections::HashSet::new(),
                expanded_automation_tracks: std::collections::HashSet::new(),
                master_row_automation_expanded: false,
                track_row_overrides: std::collections::HashMap::new(),
                automation_lane_row_overrides: std::collections::HashMap::new(),
                bottom_panel: 0,
                audio_editor_views: std::collections::HashMap::new(),
                audio_editor_vertical_gain: 1.0,
                arrange_zoom_x: ARRANGE_PX_PER_BEAT,
                // 0.0 = 未設定 (view が既定比率へ倒す)。
                arrangement_split_ratio: 0.0,
                arrange_scroll_beat: 0.0,
                arrange_follow: common::model::FollowMode::default(),
                arrange_track_top: 0.0,
                arrange_track_row_h: ARRANGE_TRACK_HEIGHT,
                arrange_header_w: 160.0,
                // r.md #87: 0 = 未設定 → widget が既定幅を使う。
                launcher_layout: common::model::LauncherLayout::default(),
                launcher_width: 0.0,
                launcher_scene_col_w: 0.0,
                launcher_scroll_scene: 0.0,
                piano_roll_views: std::collections::HashMap::new(),
                plugin_editor_windows: std::collections::HashMap::new(),
                multi_clip_view: common::model::PianoRollViewState::default(),
                multi_clip_view_key: Vec::new(),
                locked_pr_tracks: std::collections::HashSet::new(),
                last_note_duration_beats: DEFAULT_NOTE_DURATION,
                pianoroll_snap_enabled: true,
                pianoroll_snap_choice: crate::view::snap::CHOICE_PIANOROLL_DEFAULT,
                arrange_snap_enabled: true,
                arrange_snap_choice: crate::view::snap::CHOICE_ARRANGE_DEFAULT,
                resource_monitor_enabled: app_config.resource_monitor_enabled,
                // r.md #29: 編集履歴 window の開閉/位置/サイズを app_config から復元。
                undo_history_open: app_config.undo_history_open,
                undo_history_rect: app_config
                    .undo_history_rect
                    .map(|[x, y, w, h]| daw_ui_renderer::Rect { x, y, w, h }),
                // r.md #48: 設定 window の開閉/位置/サイズ。
                settings_open: app_config.settings_open,
                settings_rect: app_config
                    .settings_rect
                    .map(|[x, y, w, h]| daw_ui_renderer::Rect { x, y, w, h }),
                // r.md #54: ラウドネスレポート window の開閉/位置/サイズ。
                loudness_report_open: app_config.loudness_report_open,
                loudness_report_rect: app_config
                    .loudness_report_rect
                    .map(|[x, y, w, h]| daw_ui_renderer::Rect { x, y, w, h }),
                // r.md #50: マスターパネルの開閉 / 幅 / セクション配分 / メーター設定。
                master_panel_open: app_config.master_panel_open,
                master_panel_w: app_config.master_panel_w,
                master_panel_sections: app_config.master_panel_sections,
                meter_settings: app_config.meter,
                // r.md #75: VOICEVOX 合成の塊の長さ (秒)。load 側でクランプ済。
                voicevox_chunk_secs: app_config.voicevox_chunk_secs,
                is_help_open: false,
                is_about_open: false,
                app_dirs,
                recent_files,
                recent_saved,
                // 初期 label cache は new() の末尾でまとめて初期化する。 Self
                // literal の途中で他 field を参照できないので、 一旦 empty で
                // 構築し、 caller 側で `init_recent_labels` を呼ばずに済むよう
                // make_app! / fn new の末尾で埋める (= line ~810 付近の
                // 「app.init_recent_labels()」 を見よ)。
                recent_files_labels: Vec::new(),
                recent_saved_labels: Vec::new(),
                snap_on_draw: false,
                piano_roll_fold: false,
            },
            ui_ephemeral: UiEphemeral {
                arr_label_cache: std::cell::RefCell::default(),
                tempo_map_cache: std::cell::RefCell::default(),
                // r.md #48: 設定 window を開いたときに `refresh_available_themes` が埋める。
                // 起動時に settings_open が復元されるケースは `new()` 末尾で埋める。
                available_themes: Vec::new(),
                loaded_project_id: 0,
                project_generation: 0,
                video_texture_cache: std::collections::HashMap::new(),
                image_texture_cache: std::collections::HashMap::new(),
                pending_texture_destroys: Vec::new(),
                arrangement_hover_beat: None,
                arrangement_hover_beat_raw: None,
                arrangement_hover_clip: None,
                arrange_hovered_track: None,
                mixer_hovered_track: None,
                mixer_hovered_strip_section: None,
                master_hovered_section: None,
                master_gain_dragging: false,
                voicevox_chunk_editing: false,
                pianoroll_hover_beat: None,
                pianoroll_hover_beat_song_raw: None,
                pianoroll_hover_note: None,
                pending_clipboard_write: None,
                editing_automation_point: None,
                last_touched_param: None,
                preview_secs_memo: std::cell::Cell::new(None),
                home_toggle_at_first: false,
                arrange_zoom_history: Vec::new(),
                arrange_zoom_anchor: None,
                zoom_lane_fill: None,
                arrange_hover_content: None,
                arrange_dragging_track_volume: None,
                arrange_hovered_automation_lane: None,
                piano_roll_lyric_editing: false,
                pianoroll_viewport: None,
                audio_editor_clip: None,
                pianoroll_focus_clip: None,
                audio_editor_hover_beat_in_clip: None,
                inspector_body_h: 800.0,
                inspector_device_panel_h: 0.0,
                last_pianoroll_grid_size: (0.0, 0.0),
                pending_pianoroll_fit: false,
                last_arrange_lanes_size: (0.0, 0.0),
                last_arrange_rows: Vec::new(),
                resource_panel_open: false,
                undo_history_follow_pos: 0,
                plugin_picker_entries,
                plugin_picker_visible: Vec::new(),
                plugin_picker_query: String::new(),
                is_plugin_picker_open: false,
                plugin_picker_cursor: 0,
                font_picker_families: Vec::new(),
                font_picker_visible: Vec::new(),
                font_picker_query: String::new(),
                font_picker_cursor: 0,
                is_font_picker_open: false,
                font_picker_loading: false,
                font_picker_target: None,
                font_picker_restore: String::new(),
                send_picker: None,
                open_video_fx_params: None,
                open_plugin_params: None,
                anim_epoch: std::time::Instant::now(),
                frame_now: std::time::Instant::now(),
                status_message: String::new(),
                pending_shortcut_injections: Vec::new(),
                track_rename_id: None,
                color_picker_target: None,
                color_picker_anchor: None,
                color_picker_session_dirty: false,
                clip_create_menu: None,
                clip_create_menu_open: false,
                section_menu: None,
                section_menu_open: false,
                section_rename_id: None,
                section_rename_text: String::new(),
                track_rename_text: String::new(),
                clip_rename: None,
                clip_rename_text: String::new(),
                bpm_edit_text: format!("{initial_bpm:.1}"),
                time_sig_num_edit_text: initial_time_sig_num.to_string(),
                clip_edit_buffer_target: None,
                clip_text_content_edit_text: String::new(),
                clip_text_font_family_edit_text: String::new(),
                scrub_gesture: None,
                scrub_gesture_seen: false,
                mod_follower_scrub_active: false,
                armed_mod_source: None,
                expanded_mod_sources: std::collections::HashSet::new(),
                export_range_picker: None,
                recovery_candidates,
                show_recovery_modal,
                dirty_guard: None,
                guard_after_save: None,
                guard_pending_action: None,
                export_dialog_open: false,
                save_as_dialog_open: false,
                #[cfg(windows)]
                main_window_hwnd: None,
            },
            // r.md #49: 起動直後は「アクティブ」から始める。winit は起動時の
            // `Focused(true)` を必ずしも送らないので、false 始まりだと最初の
            // クリックまで画面が描かれない。
            activity: crate::state::ActivityState {
                main_focused: true,
                ..Default::default()
            },
            // r.md #87: ランチャーの一時状態 (フォーカス / hover / MIDI bind)。
            launcher: crate::state::LauncherUiState::default(),
            // r.md #61: 起動直後は `Running`。終了要求で `Draining` に入る。
            shutdown: crate::shutdown::ShutdownState::default(),
            // r.md #50: メーター設定の初期値は app_config から。`active` は
            // 「パネルが描かれているか」で、view が毎フレーム同期する。
            meter_control: std::sync::Arc::new(std::sync::Mutex::new(
                crate::master_meter::settings::MeterControl {
                    settings: app_config.meter,
                    loudness_reset_epoch: 0,
                    peak_reset_epoch: 0,
                    active: app_config.master_panel_open,
                },
            )),
        };
        // recent_files / recent_saved の path 列から filename label cache を
        // 1 回構築。 push_recent / push_recent_saved 経由の更新でも自動的に
        // 同期されるので、 初回のみここで初期化する。
        let mut app = app;
        app.init_recent_labels();
        // r.md #48: 設定 window が開いた状態で復元されたときは、最初のフレームまでに
        // テーマ一覧が要る (描画側は毎フレーム作り直さない)。
        if app.ui_prefs.settings_open {
            app.refresh_available_themes();
        }
        // cache が旧 port-probe 版 (PluginEntry の 3 bool 未取得) なら、
        // 起動時に 1 回だけ自動で再 probe (rescan) して port 構成を埋める。 production
        // (app_dirs=Some) のみ — test は app_dirs=None なので実システム scan を避ける。
        if app.ui_prefs.app_dirs.is_some()
            && app
                .ipc.plugin_db
                .as_ref()
                .is_some_and(|db| db.needs_port_probe())
        {
            tracing::info!("plugin cache predates port-probe; auto-rescanning to fill port info");
            app.begin_rescan();
        }
        // 起動直後は clean (SongDoc::new が saved_epoch = edit_epoch で構築)。
        app
    }
}






















impl AppData {
    /// AppEvent dispatcher。view から `Edit::mutate` 経由で、background thread
    /// から `EventLoopProxy<AppEvent>` 経由で呼ばれる。
    pub fn handle_event(&mut self, event: AppEvent) {
        // (r.md #61) 終了シーケンス中は **全 event を捨てる**。
        //
        // `Draining` は「子プロセスの teardown を待つ間もイベントループが回り
        // 続ける」という新しい窓で、旧実装 (`should_quit` を立てた同じフレームで
        // `exit()`) には存在しなかった。ここを開けたままにすると、
        //   - 「終了処理中…」の下に残った picker のクリックが通る
        //     (= 畳ませた plugin host へ `SetSlotPlugin` が飛ぶ)
        //   - 30 秒周期の `AutosaveTick` が recovery ファイルを書き直す
        // といった「もう終わると決めた後の副作用」が起きる。
        //
        // export gate と違って **allow-list ではなく全遮断**にできるのは、
        // 終了が必ず `DRAIN_TIMEOUT` で終端するから — 「落としすぎて永久ロック」
        // という export gate の失敗モードが原理的に存在しない。完了判定
        // (`poll_shutdown`) は event ではなく `try_wait` で回っている。
        if self.shutdown.is_shutting_down() {
            tracing::debug!(?event, "event dropped during shutdown");
            return;
        }
        // この event の ambient undo scope を確定する (1 event 内の複数 edit_song は
        // 1 undo step に squash、 Begin*/End* gesture 中は drag 全体で 1 step)。
        // 同時に、 この event が snapshot を積んだときの履歴リスト用ラベル
        // (r.md #29) を event 種から確定して渡す。
        self.song_doc.begin_event(event.undo_label());
        // Export gate (positive-default + block-list)。
        //
        // 旧構造は negative-default の allow-list だった (export 中は列挙した少数
        // event 以外を全 drop)。 これは「新 variant を allow に入れ忘れ →
        // export 中に GUI 永久ロック」 class の温床で、 事故が 3 件記録された。
        // これを反転する: export 中でも **原則すべて流す**。 明示 block-list に
        // 載る event だけ drop する。
        //
        // song 編集自体は `SongDoc::edit` (edit_song) の export_lock が拒否する
        // ので、 ここで二重に止める必要はない。 transport (Play 等) は各 handler が
        // `export_stage` を見て自前で抑止する。 従って block すべきは
        // **走行中の plugin instance を再構成する host round-trip のみ** — export は
        // instance を流用するため、 これらが LoadSong / RemoveSlotPlugin /
        // OpenPluginShmem を host へ送ると render が壊れる:
        //   - `PluginEvent::SlotPluginLoaded`   … plugin load 完了適用
        //   - `PluginEvent::AllPluginStates`    … Deferred edit 適用 (reconcile→LoadSong)
        //   - `AudioEvent::BounceClipFxComplete`… bounce 完了適用 (engine song 復元)
        // 宙吊りになった round-trip は #64 watchdog (state round-trip) が export 後に
        // 回収する。 判断に迷う event は block しない (安全側 = 流す) ので、
        // 「新 variant の分類し忘れ = deadlock」 という故障モードが構造的に消える。
        // r.md #54: 範囲ラウドネス解析も同じ freewheel 経路 (engine の
        // `export_running` を共有) なので、同じ block-list を適用する。
        // **書き出し / 解析だけ**を見る (bounce / Glue の焼き込みは含めない) —
        // それらは自分の `BounceClipFxComplete` で engine song を復元するので、
        // 広い述語で捨てると自分の完了を握り潰して永久に終わらない。
        if self.export_or_analysis_busy() {
            let block = matches!(
                event,
                AppEvent::Plugin(common::protocol::PluginEvent::SlotPluginLoaded { .. })
                    | AppEvent::Plugin(common::protocol::PluginEvent::AllPluginStates { .. })
                    | AppEvent::Audio(common::protocol::AudioEvent::BounceClipFxComplete { .. })
            );
            if block {
                tracing::debug!(?event, "event blocked during export (host-reconfig round-trip)");
                return;
            }
        }

        match event {
            // 子プロセス (daw_audio / daw_plugin_host) からの protocol event は
            // direct-wrap で丸ごと届く。 variant ごとの既存処理接続は
            // handler::ipc の dispatch_* が担う (旧 1:1 bridge / *FromChild を置換)。
            AppEvent::Audio(ev) => self.dispatch_audio_event(ev),
            AppEvent::Plugin(ev) => self.dispatch_plugin_event(ev),
            // r.md #87: ランチャー操作は 1 arm で受けて専用 dispatcher へ。
            AppEvent::Launcher(ev) => self.handle_launcher_event(ev),
            // New / Open は現在のプロジェクトを破棄するので、 dirty なら
            // 先に保存確認ダイアログを挟む (clean なら即実行)。
            // r.md #61: 全終了経路の合流点。
            AppEvent::Quit(req) => self.request_quit(req),
            AppEvent::New => self.request_guarded_action(DirtyGuardAction::New),
            AppEvent::Open => self.request_guarded_action(DirtyGuardAction::Open),
            AppEvent::Save => {
                // ガード確認中 / 保存後アクション待ち中 / queue drain 待ち中は
                // 手動保存を無視する。 この間に別経路の保存を走らせると pending_state_queue
                // に余分な Save が積まれ、 続く New/Open が project を破壊しうる (guard_save が
                // 発行する保存に一本化する)。 finish_save の再保存ループは begin_save 直呼び
                // なのでこの gate を通らない。
                if self.ui_ephemeral.dirty_guard.is_none()
                    && self.ui_ephemeral.guard_after_save.is_none()
                    && self.ui_ephemeral.guard_pending_action.is_none()
                {
                    self.action_save();
                }
            }
            AppEvent::SaveAs => {
                if self.ui_ephemeral.dirty_guard.is_none()
                    && self.ui_ephemeral.guard_after_save.is_none()
                    && self.ui_ephemeral.guard_pending_action.is_none()
                {
                    self.action_save_as();
                }
            }
            AppEvent::DirtyGuardSave => self.guard_save(),
            AppEvent::DirtyGuardDiscard => {
                if let Some(action) = self.ui_ephemeral.dirty_guard.take() {
                    // 「保存せず続行/終了」 = 現プロジェクトの未保存変更を破棄する。
                    // その変更を写した autosave (sidecar / session recovery file) を
                    // 消してから操作を実行する。 残すと、 同じ file を開き直したとき /
                    // 次回起動時に recovery 機構が「破棄したはずの変更を復元しますか？」
                    // と聞いてしまう (実機検証で発覚)。
                    self.discard_current_autosave();
                    self.perform_guard_action(action);
                }
            }
            AppEvent::DirtyGuardCancel => {
                self.ui_ephemeral.dirty_guard = None;
            }
            AppEvent::Play => {
                self.play();
            }
            AppEvent::Stop => {
                self.stop();
            }
            AppEvent::PlayToggle => {
                if self.transport.is_playing {
                    self.stop();
                } else {
                    self.play();
                }
            }
            AppEvent::Panic => {
                self.panic();
            }
            AppEvent::PlayFromCursor { beat } => {
                self.action_play_from_cursor(beat);
            }
            AppEvent::GotoTimelineHome => {
                self.goto_timeline_home();
            }
            AppEvent::GotoTimelineEnd => {
                self.goto_timeline_end();
            }
            AppEvent::ToggleLoop => {
                self.toggle_loop();
            }
            AppEvent::PreviewPitchChanged { track_idx, pitch } => {
                // gui_01 #055: 押下 pitch を track id 付き held-value に解決し、
                // 前回値と差分して note-on/off を音源トラックへ送る。 track id は
                // reorder race-free な addressing (audio 側で index に再解決)。
                // 対象 track が存在しない / pitch=None なら next=None (= 発音停止)。
                let next = pitch
                    .and_then(|p| self.song_doc.song().tracks.get(track_idx as usize).map(|t| (t.id, p)));
                for action in diff_preview(self.recording.preview_note, next) {
                    match action {
                        PreviewAction::NoteOff { track_id, pitch } => {
                            self.send_audio(AudioCommand::PreviewNoteOff { track_id, pitch });
                        }
                        PreviewAction::NoteOn { track_id, pitch } => {
                            self.send_audio(AudioCommand::PreviewNoteOn {
                                track_id,
                                pitch,
                                velocity: PREVIEW_VELOCITY,
                            });
                        }
                    }
                }
                self.recording.preview_note = next;
            }
            AppEvent::LoopSelectedClipToggle { automation } => {
                self.loop_selected_clip_toggle(automation);
            }
            AppEvent::BpmEditChanged(s) => {
                self.ui_ephemeral.bpm_edit_text = s;
            }
            AppEvent::CommitBpmEdit => {
                self.commit_bpm_edit();
            }
            AppEvent::SetSongBpmFromScrub(next) => {
                let clamped = next.clamp(1.0, 400.0);
                if (self.song_doc.song().bpm - clamped).abs() > f32::EPSILON {
                    let old_bpm = self.song_doc.song().bpm;
                    // scrub の連続 commit は stream gesture で 1 undo step に
                    // squash する (dirty / autosave は epoch bump が担う)。
                    let scope = self.song_doc.stream_scope(StreamGesture::BpmScrub);
                    self.song_doc.edit(scope, |song| song.bpm = clamped);
                    self.ui_ephemeral.bpm_edit_text = format!("{:.1}", clamped);
                    // Raw audio clip を秒固定スケール (r.md #7)。Raw clip があれば
                    // LoadSong (decode 再利用で軽量) で再生 window を追従させ、
                    // 無ければ従来の軽量 SetSongBpm で済ます。
                    if !self.rescale_raw_clips_for_bpm_change(old_bpm, clamped) {
                        self.send_audio(AudioCommand::SetSongBpm { bpm: clamped });
                    }
                }
            }
            AppEvent::SetSongTimeSigNumFromScrub(next) => {
                let clamped = next.clamp(1, 32);
                if self.song_doc.song().time_sig.0 != clamped {
                    let scope = self.song_doc.stream_scope(StreamGesture::TimeSigScrub);
                    self.song_doc.edit(scope, |song| song.time_sig.0 = clamped);
                    self.ui_ephemeral.time_sig_num_edit_text = clamped.to_string();
                    self.send_audio(AudioCommand::SetSongTimeSigNumerator { num: clamped });
                }
            }
            AppEvent::TimeSigNumEditChanged(s) => {
                self.ui_ephemeral.time_sig_num_edit_text = s;
            }
            AppEvent::CommitTimeSigNumEdit => {
                self.commit_time_sig_num_edit();
            }
            AppEvent::SetSongTimeSigDenominator(den) => {
                self.set_song_time_sig_denominator(den);
            }
            AppEvent::Undo => self.undo(),
            AppEvent::Redo => self.redo(),
            // r.md #29: 履歴 window の開閉トグル (View メニュー / ✕ / Esc / Ctrl+Alt+Z)。
            // 開閉状態は app_config に永続 (再起動を跨いで復元)。
            AppEvent::ToggleUndoHistory => {
                self.ui_prefs.undo_history_open = !self.ui_prefs.undo_history_open;
                self.persist_app_config();
            }
            // r.md #48: 設定 window の開閉トグル (Edit メニュー / ✕ / Esc)。
            AppEvent::ToggleSettings => {
                self.ui_prefs.settings_open = !self.ui_prefs.settings_open;
                if self.ui_prefs.settings_open {
                    // テーマ一覧の実体は `themes/` の read_dir + JSON パース。
                    // **開いたときに 1 回だけ**取る (描画ループでディスクを叩かない)。
                    // 開き直せば新しく置いたファイルが出るので再起動は要らない。
                    self.refresh_available_themes();
                }
                self.persist_app_config();
            }
            // r.md #50: 画面右端のマスターパネルの開閉 (View メニュー / Ctrl+Alt+M)。
            AppEvent::ToggleMasterPanel => {
                self.ui_prefs.master_panel_open = !self.ui_prefs.master_panel_open;
                self.sync_meter_control();
                self.persist_app_config();
            }
            // ドラッグ中は状態だけ更新し、release でまとめてディスクへ書く
            // (毎フレーム app_config.json を同期書き込みしない — 設定 window /
            // 編集履歴 window と同じ commit-on-release の流儀)。
            AppEvent::SetMasterPanelWidth { w, commit } => {
                use crate::handler::master_panel::{MASTER_PANEL_MAX_W, MASTER_PANEL_MIN_W};
                let next = w.clamp(MASTER_PANEL_MIN_W, MASTER_PANEL_MAX_W);
                let changed = (next - self.ui_prefs.master_panel_w).abs() >= 0.5;
                if changed {
                    self.ui_prefs.master_panel_w = next;
                }
                if commit {
                    self.persist_app_config();
                }
            }
            AppEvent::SetMasterPanelSectionRatios { ratios, commit } => {
                let sum: f32 = ratios.iter().sum();
                if sum <= 0.0 {
                    return;
                }
                let next = [
                    ratios[0] / sum,
                    ratios[1] / sum,
                    ratios[2] / sum,
                    ratios[3] / sum,
                ];
                let changed = !next
                    .iter()
                    .zip(self.ui_prefs.master_panel_sections.iter())
                    .all(|(a, b)| (a - b).abs() < 1e-4);
                if changed {
                    self.ui_prefs.master_panel_sections = next;
                }
                if commit {
                    self.persist_app_config();
                }
            }
            AppEvent::SetMeterSettings(settings) => {
                self.ui_prefs.meter_settings = *settings;
                self.sync_meter_control();
                self.persist_app_config();
            }
            // EBU Tech 3341 §2.2: 積算値は「同時に」リセットできること。
            AppEvent::ResetLoudness => self.reset_master_loudness(),
            AppEvent::ResetMasterPeakHold => self.reset_master_peak_hold(),
            // r.md #48: テーマ切替。 id からパレットを解決して差し替えるだけで、
            // 実際に画面へ反映するのは runner (`UiHost::set_palette` + 描画キャッシュ破棄)。
            AppEvent::SetTheme(id) => {
                let dirs = self.ui_prefs.app_dirs.as_ref().map(common::app_dirs::AppDirs::themes_dir);
                self.theme = crate::theme::resolve(dirs.as_deref(), &id);
                self.persist_app_config();
            }
            AppEvent::SetVoicevoxChunkSecs { secs, commit } => {
                self.set_voicevox_chunk_secs(secs, commit);
            }
            // r.md #29: 履歴リストの行 click → その state へ一発 Undo/Redo。
            AppEvent::JumpHistory(index) => self.jump_history_to(index),
            AppEvent::QuantizeSelectedNotes(div) => {
                self.quantize_selected_notes(div);
            }
            AppEvent::SetNoteVelocity { note, velocity } => {
                self.set_note_velocity(note, velocity);
            }
            AppEvent::SetNoteVelocities(updates) => {
                self.set_note_velocities(&updates);
            }
            AppEvent::AddInstrumentTrack => self.action_add_instrument_track(),
            // 前面化は runner の user_event が window へ直接行うため、
            // ここには届かない。 match 網羅のための no-op。
            AppEvent::RaiseMainWindow => {}
            AppEvent::GroupSelectedTracks { track_ids } => {
                self.action_group_selected_tracks(&track_ids);
            }
            AppEvent::ToggleGroupCollapsed { track_id } => {
                // r.md #74: arrangement / mixer 両方の group disclosure が
                // ここに合流する (`collapsed_groups` が 2 ビュー共通の SSoT)。
                if !self.ui_prefs.collapsed_groups.insert(track_id) {
                    self.ui_prefs.collapsed_groups.remove(&track_id);
                }
            }
            AppEvent::ToggleTrackAutomationCollapsed { track_id } => {
                // gui_01 #034 (Phase 63n-10): master row の expansion は
                // 通常 track の set とは別 SSoT。
                if track_id == common::model::MASTER_TRACK_ID {
                    self.ui_prefs.master_row_automation_expanded =
                        !self.ui_prefs.master_row_automation_expanded;
                } else if !self.ui_prefs.expanded_automation_tracks.insert(track_id) {
                    self.ui_prefs.expanded_automation_tracks.remove(&track_id);
                }
            }
            AppEvent::SetLaneEnabled {
                track_id,
                lane_id,
                enabled,
            } => self.set_lane_enabled(track_id, lane_id, enabled),
            AppEvent::SetLaneVisible {
                track_id,
                lane_id,
                visible,
            } => self.set_lane_visible(track_id, lane_id, visible),
            AppEvent::SetLaneDefault {
                track_id,
                lane_id,
                prev_norm: _,
                next_norm,
            } => self.set_lane_default(track_id, lane_id, next_norm),
            AppEvent::DeleteLane { track_id, lane_id } => {
                self.delete_lane(track_id, lane_id)
            }
            AppEvent::SetLaneHeight {
                track_id,
                lane_id,
                prev_px: _,
                next_px,
            } => self.set_lane_height(track_id, lane_id, next_px),
            AppEvent::SetSingleTrackRowH {
                track_id,
                prev_px: _,
                next_px,
            } => {
                self.ui_prefs.track_row_overrides.insert(track_id, next_px);
            }
            AppEvent::AddAutomationPoint {
                track_id,
                lane_id,
                clip_id,
                time_beat,
                value_norm,
            } => self.add_automation_point(track_id, lane_id, clip_id, time_beat, value_norm),
            AppEvent::MoveAutomationPoints { deltas } => {
                self.move_automation_points(&deltas)
            }
            AppEvent::DeleteAutomationPoints { points } => {
                self.delete_automation_points(&points)
            }
            AppEvent::BeginEditAutomationPointValue { key } => {
                // session-only: 該当 point が存在するときだけ編集開始 (race で
                // 既に消えていれば no-op)。
                if self.automation_point_value(&key).is_some() {
                    self.ui_ephemeral.editing_automation_point = Some(key);
                }
            }
            AppEvent::SetAutomationPointValue { key, value } => {
                self.set_automation_point_value(&key, value);
                self.ui_ephemeral.editing_automation_point = None;
            }
            AppEvent::SetAutomationCurve { track_id, lane_id, clip_id, point_id, next } => {
                self.set_automation_curve(track_id, lane_id, clip_id, point_id, next);
            }
            AppEvent::MoveAutomationClips { deltas } => {
                self.move_automation_clips(&deltas)
            }
            AppEvent::CloneAutomationClipsLinked { deltas } => {
                self.clone_automation_clips_linked(&deltas)
            }
            AppEvent::CloneAutomationClipsIndependent { deltas } => {
                self.clone_automation_clips_independent(&deltas)
            }
            AppEvent::DuplicateAutomationClipsShared(keys) => {
                self.duplicate_automation_clips_shared(&keys);
            }
            AppEvent::DuplicateAutomationClipsUnique(keys) => {
                self.duplicate_automation_clips_unique(&keys);
            }
            AppEvent::ResizeAutomationClips { deltas } => {
                self.resize_automation_clips(&deltas)
            }
            AppEvent::DeleteAutomationClips { keys } => {
                self.delete_automation_clips(&keys)
            }
            AppEvent::SelectAutomationClips { prev: _, next } => {
                // 選択の SSoT は範囲 1 本なので、**範囲もそのクリップ群へ張り直す**。
                // これを飛ばすと、前に選んでいた MIDI クリップが選択表示のまま残り、
                // `Z` が「選んで見えているクリップ」ではない方へズームする (実機で報告)。
                self.select_automation_clip_range(&next);
                // 直近に選択した編集面を記録 (= 共存選択されたときの
                // copy/cut/delete 対象を「最後に選んだ面」 に決める last-wins)。
                if !next.is_empty() {
                    self.selection.last_edit_select = Some(EditSurface::AutomationClips);
                }
                self.selection.selected_automation_clips = next;
            }
            AppEvent::SelectAutomationPoints { prev: _, next } => {
                if !next.is_empty() {
                    self.selection.last_edit_select = Some(EditSurface::AutomationPoints);
                }
                self.selection.selected_automation_points = next;
            }
            AppEvent::QuantizeSelectedAutomationPoints(div) => {
                self.quantize_selected_automation_points(div);
            }
            AppEvent::MakeAutomationClipUnique(key) => {
                self.make_automation_clip_unique(key);
            }
            AppEvent::TouchParam {
                track_id,
                target,
                display_name,
            } => {
                self.ui_ephemeral.last_touched_param = Some(TouchedParam {
                    track_id,
                    target,
                    display_name,
                    touched_at: std::time::Instant::now(),
                });
            }
            AppEvent::AddAutomationFromLastTouched => {
                self.add_automation_from_last_touched();
            }
            AppEvent::AddImageAutomationLane { field } => {
                self.add_image_automation_lane(field);
            }
            AppEvent::RemoveImageAutomationLane { field } => {
                self.remove_image_automation_lane(field);
            }
            AppEvent::AddTextAutomationLane { field } => {
                self.add_text_automation_lane(field);
            }
            AppEvent::RemoveTextAutomationLane { field } => {
                self.remove_text_automation_lane(field);
            }
            AppEvent::AddGroupAutomationLane { param } => {
                self.add_group_automation_lane(param);
            }
            AppEvent::RemoveGroupAutomationLane { param } => {
                self.remove_group_automation_lane(param);
            }
            AppEvent::BeginGroupTransformDrag => {
                // r.md #28: group transform の scrub / preview drag 全体を 1 undo step に
                // bracket する (= ParamGestureBegin と同 idiom)。これが無いと per-frame の
                // `SetGroupTransformField` が各々 fresh な event_scope で snapshot を積み、
                // 1 回の drag が undo 履歴を大量の step で埋める。group lane recording は未対応。
                self.song_doc.begin_gesture();
            }
            AppEvent::SetGroupTransformField { track_id, param, value } => {
                // scrubable_number / preview drag からの live 設定。inspector は
                // track.group_transform を毎フレーム直接読むので resync 不要。
                self.set_group_transform_field(track_id, param, value);
            }
            AppEvent::EndGroupTransformDrag => {
                self.song_doc.end_gesture();
            }
            // r.md #28: inspector scrubable_number の drag / text 編集 stroke を 1 undo step に
            // bracket する。arch refactor で `is_undoable` whitelist を撤去した際、この Begin/End
            // が no-op のまま残り、per-frame の Set* 編集が各々 undo step を積んでいた (= 1 drag で
            // 履歴が溢れる)。ParamGestureBegin/End と同じ begin_gesture/end_gesture で塞ぐ。
            AppEvent::BeginInspectorScrub => {
                self.song_doc.begin_gesture();
            }
            AppEvent::EndInspectorScrub => {
                self.song_doc.end_gesture();
            }
            AppEvent::BeginImagePiPDrag => {
                // r.md #28: preview canvas 上の image PiP drag 全体を 1 undo step に bracket
                // する (per-frame の `SetClipImageX/Y/W/H/Rotation` が各々 snapshot を積んで
                // undo 履歴を溢れさせない = group transform / inspector scrub と同 idiom)。
                self.song_doc.begin_gesture();
                // lane recording seed: selected_clip が指す image track に対し、lane を持つ
                // field を `active_param_gestures` に登録する。record_automation_points_for
                // _tick が再生中に 1/64 beat 刻みで point を打ち続ける。drag end (= MouseInput
                // Released) で `image_drag_release` 経路から End を発火してクリアする。
                self.begin_image_pip_drag_recording();
            }
            AppEvent::EndImagePiPDrag => {
                self.end_image_pip_drag_recording();
                self.song_doc.end_gesture();
            }
            AppEvent::SetRecordingMode(mode) => {
                self.recording.recording_mode = mode;
                self.sync_recording_lanes_with_audio();
            }
            AppEvent::SetMetronomeEnabled(enabled) => {
                // Phase 7 B3 (2026-05-13): metronome on/off。 audio thread は
                // 次 buffer から `render_metronome` の有無を切り替える (= 無効
                // 時は mix step 自体 skip = CPU 0)。 GUI 側は transport bar の
                // toggle UI 更新のみ。
                self.transport.metronome_enabled = enabled;
                self.send_audio(AudioCommand::SetMetronomeEnabled(enabled));
            }
            AppEvent::ToggleMidiRecording => {
                self.toggle_midi_recording();
            }
            AppEvent::SetCountInBars(bars) => {
                self.recording.count_in_bars = bars.min(2);
            }
            AppEvent::ParamGestureBegin {
                track_id,
                target,
                display_name,
            } => {
                // built-in トラックコントロール (Volume / Pan / SendGain)
                // の drag は gesture 先頭で 1 回だけ Song snapshot を取り、 「1 drag =
                // 1 undo step」 にする (`BeginInspectorScrub` と同 idiom)。 per-frame に
                // 発火する `SetTrackVolume` / `SetTrackPan` / `SetSendGain` 自体は
                // 非 undoable のまま (連続発火で履歴が溢れるため)。 これが無いと
                // フェーダー操作が undo スタックに積まれず、 Undo が直前のクリップ移動
                // 等まで巻き戻してしまう。 `ParamGestureBegin` は gesture 立ち上がりで
                // 1 度だけ発火する (`push_param_gesture_edges` の edge 検知) ので二重に
                // ならない。 `PluginParam` は値が Song snapshot に入らない (plugin 内部
                // 状態) ので除外、 `SongTempo` / `TimeSig` は transport 側の commit
                // ベース undo に委ねる。
                // fader/knob drag 全体を 1 undo step に bracket する (最初の
                // song 編集が snapshot を積む。 PluginParam のように song を
                // 変えない gesture では undo step は増えない)。
                self.song_doc.begin_gesture();
                self.recording.active_param_gestures.insert((track_id, target.clone()));
                // Phase 4 Step C: Latch / Write mode で 再生中の gesture begin は
                // latched_param_gestures にも入れる。 stop まで「触れた事実」 を
                // 保持し、 release 後も curve 上書きを継続する。 Touch mode では
                // latched は使わない (= release で recording 完全停止)。
                if matches!(
                    self.recording.recording_mode,
                    common::model::RecordingMode::Latch | common::model::RecordingMode::Write
                ) && self.transport.is_playing
                {
                    self.recording.latched_param_gestures.insert((track_id, target.clone()));
                }
                // `TouchParam` を発火し続けるより、 gesture begin で `last_touched_param`
                // を更新する idiom に統一する。 (= drag 開始の瞬間が touch、 drag 中
                // の値変化は touch を再発火しない)
                self.ui_ephemeral.last_touched_param = Some(TouchedParam {
                    track_id,
                    target,
                    display_name,
                    touched_at: std::time::Instant::now(),
                });
                self.sync_recording_lanes_with_audio();
            }
            AppEvent::ParamGestureEnd { track_id, target } => {
                self.recording.active_param_gestures.remove(&(track_id, target.clone()));
                if self.recording.active_param_gestures.is_empty() {
                    self.song_doc.end_gesture();
                }
                // BPM scrub は毎 tick edit_song で epoch を bump するので、 plugin
                // host 側の BPM 消費者 (VOICEVOX metadata / ARA placement / lipsync)
                // は runner の frame flush (flush_song_sync) が構造的に追従する
                // (旧 pending_host_sync 予約は epoch 一本化で不要になった)。
                // Phase 4 Step C: Touch mode の場合、 release で recording 完全停止 →
                // recording_last_beat からも 該当 entry を消す (= 次の gesture begin
                // で改めて throttle 開始)。 Latch / Write は stop まで latched 継続
                // なので last_beat も保持する (= 連続 record)。
                if self.recording.recording_mode == common::model::RecordingMode::Touch {
                    self.recording.recording_last_beat.remove(&(track_id, target));
                }
                self.sync_recording_lanes_with_audio();
            }
            AppEvent::CreateAutomationClip {
                lane,
                start_beat,
                len_beats,
            } => self.create_automation_clip(lane, start_beat, len_beats),
            AppEvent::UngroupTracks { track_ids } => {
                self.action_ungroup_tracks(&track_ids);
            }
            AppEvent::SetTrackParent { track_id, parent_id } => {
                self.action_set_track_parent(track_id, parent_id);
            }
            AppEvent::RemoveLastTrack => self.action_remove_last_track(),
            AppEvent::DeleteTracks(track_ids) => self.delete_tracks(track_ids),
            AppEvent::DuplicateTracksShared(track_ids) => self.duplicate_tracks(track_ids, true),
            AppEvent::DuplicateTracksUnique(track_ids) => self.duplicate_tracks(track_ids, false),
            AppEvent::MoveTrackUp(idx) => self.swap_tracks(idx, idx.saturating_sub(1)),
            AppEvent::MoveTrackDown(idx) => self.swap_tracks(idx, idx + 1),
            AppEvent::ReorderTracks(order) => self.reorder_tracks(&order),
            AppEvent::BeginRenameTrack(track_id) => {
                self.begin_rename_track(track_id);
            }
            AppEvent::RenameTrackChanged(text) => {
                self.ui_ephemeral.track_rename_text = text;
            }
            AppEvent::CommitRenameTrack => self.commit_rename_track(),
            AppEvent::CancelRenameTrack => {
                self.ui_ephemeral.track_rename_id = None;
                self.ui_ephemeral.track_rename_text.clear();
            }
            AppEvent::BeginRenameSection(id) => self.begin_rename_section(id),
            AppEvent::RenameSectionChanged(text) => self.ui_ephemeral.section_rename_text = text,
            AppEvent::CommitRenameSection => self.commit_rename_section(),
            AppEvent::CancelRenameSection => {
                self.ui_ephemeral.section_rename_id = None;
                self.ui_ephemeral.section_rename_text.clear();
            }
            AppEvent::SetSectionColor { id, color } => {
                self.edit_song(|song| {
                    if let Some(s) = song.sections.iter_mut().find(|s| s.id == id) {
                        s.color = color;
                    }
                });
            }
            AppEvent::BeginRenameClip(target) => self.begin_rename_clip(target),
            AppEvent::RenameClipChanged(text) => {
                self.ui_ephemeral.clip_rename_text = text;
            }
            AppEvent::CommitRenameClip => self.commit_rename_clip(),
            AppEvent::CancelRenameClip => {
                self.ui_ephemeral.clip_rename = None;
                self.ui_ephemeral.clip_rename_text.clear();
            }
            AppEvent::ToggleHelp => {
                self.ui_prefs.is_help_open = !self.ui_prefs.is_help_open;
            }
            AppEvent::CloseHelp => {
                self.ui_prefs.is_help_open = false;
            }
            AppEvent::ToggleAbout => {
                self.ui_prefs.is_about_open = !self.ui_prefs.is_about_open;
            }
            AppEvent::CloseAbout => {
                self.ui_prefs.is_about_open = false;
            }
            AppEvent::OpenRecent(path) => {
                // Open Recent も「プロジェクトを開く」 = 現プロジェクト破棄
                // なので dirty なら保存確認を挟む。
                self.request_guarded_action(DirtyGuardAction::OpenPath(path));
            }
            AppEvent::AutosaveTick => {
                self.maybe_autosave();
            }
            AppEvent::RecoveryRestore(path) => {
                self.restore_recovery(path);
            }
            AppEvent::RecoveryDiscard(path) => {
                self.discard_recovery(path);
            }
            AppEvent::RecoveryDismiss => {
                self.ui_ephemeral.show_recovery_modal = false;
            }
            AppEvent::MidiNoteOn { channel, pitch, velocity } => {
                self.handle_midi_note_on(channel, pitch, velocity);
            }
            AppEvent::MidiNoteOff { channel, pitch } => {
                // Phase 7 B4 Step D (2026-05-13): 録音中は note_off で
                // length_beats を確定。 step-input mode は note end を追跡
                // しないので無視。
                self.handle_midi_note_off(channel, pitch);
            }
            AppEvent::MidiControlChange { channel, controller, value } => {
                // Phase 7 B1-M Step 2 (2026-05-13): Learn mode なら binding 追加、
                // 通常モードなら既存 binding lookup で target に値送信。
                self.handle_midi_control_change(channel, controller, value);
            }
            AppEvent::StartMidiLearn(target) => {
                self.recording.midi_learn_target = Some(target);
                self.ui_ephemeral.status_message =
                    "MIDI Learn: 次の CC を bind します...".to_string();
            }
            AppEvent::CancelMidiLearn => {
                self.recording.midi_learn_target = None;
                self.ui_ephemeral.status_message = "MIDI Learn cancel".to_string();
            }
            AppEvent::RemoveMidiBinding(idx) => {
                if idx < self.song_doc.song().midi_bindings.len() {
                    self.edit_song(|song| song.midi_bindings.remove(idx));
                }
            }
            AppEvent::MidiInputOpened(name) => {
                let label = name.clone().unwrap_or_default();
                self.recording.midi_input_label = label.clone();
                if name.is_some() {
                    self.ui_ephemeral.status_message = format!("MIDI 入力: {label}");
                }
            }
            AppEvent::SelectBottomPanel(p) => {
                self.ui_prefs.bottom_panel = p;
            }
            AppEvent::SelectClip { target, additive } => {
                self.select_clip(target, additive);
            }
            AppEvent::SetClipSelection(targets) => {
                self.set_clip_selection(targets);
            }
            AppEvent::SetAutomationFollowsClips(on) => {
                self.ui_prefs.automation_follows_clips = on;
                // 「この人の作業のしかた」なのでアプリ設定側へ永続する。
                self.persist_app_config();
            }
            AppEvent::SetTimeSelection { start_beat, end_beat, lanes } => {
                let next = common::model::TimeSelection::new(start_beat, end_beat, lanes);
                self.set_time_selection(next);
                self.selection.range_anchor =
                    self.selection.time.as_ref().map(|t| t.start_beat);
                // 範囲を引いたら、ピアノロールは**その範囲**を映す (掛かったクリップ全体
                // ではない)。 ビューが曲頭のままだとノートが画面外で空に見えるので、
                // 範囲を張り直すたびに合わせ直す。
                self.fit_piano_roll_to_range();
            }
            AppEvent::SelectAllClips => {
                self.select_all_clips();
            }
            AppEvent::ClearSelection => {
                self.selection.time = None;
                self.selection.selected_launcher_cells.clear();
                // 選択を捨てたら Shift+click 範囲選択の基点も捨てる。
                self.selection.range_anchor = None;
                self.selection.launcher_cell_anchor = None;
            }
            AppEvent::ResizeClip {
                target,
                start_beat,
                length,
                stretch,
            } => {
                self.resize_clip(target, start_beat, length, stretch);
            }
            AppEvent::SetClipPositions(entries) => {
                self.set_clip_positions(&entries);
            }
            AppEvent::CreateClip { track, start_beat } => {
                self.create_clip(track, start_beat);
            }
            AppEvent::DeleteSelectedClip => self.delete_selected_clip(),
            AppEvent::DeleteTimeSelection => self.apply_delete_time_selection(),
            AppEvent::SelectNote { note, additive } => {
                self.select_note(note, additive);
            }
            AppEvent::ClearNoteSelection => self.clear_note_selection(),
            AppEvent::AddNote { key, start_beat, duration, pitch } => {
                self.add_note(key, start_beat, duration, pitch);
            }
            AppEvent::ResizeNote { key, note, duration } => {
                self.resize_note(key, note, duration);
            }
            AppEvent::SetNotePositions(entries) => {
                self.set_note_positions(&entries);
            }
            AppEvent::ResizeNotes(entries) => {
                self.resize_notes(&entries);
            }
            AppEvent::SetNoteSelection(targets) => {
                self.set_note_selection(&(targets));
                if !self.selected_note_ids().is_empty() {
                    self.selection.last_edit_select = Some(EditSurface::Notes);
                }
                // last (anchor) は packed note id。所属クリップを decode し、
                // (1) **そのクリップを対象 (target) に切替** — 非対象クリップのノートを掴むと編集対象が
                //     そちらへ移る (plan §D/E。selected_clips は不変なので slot/selected_notes は維持)、
                // (2) 既定 note 長をそのクリップから引く。
                if let Some(&last) = self.selected_note_ids().last()
                    && let Some((r, local)) = self.decode_note_id(last)
                {
                    if let Some(key) = self.live_clip_key(r) {
                        self.set_pianoroll_target_clip(key);
                    }
                    if let Some(dur) = self
                        .song_doc.song()
                        .track_by_id(r.track_id)
                        .and_then(|t| t.clip_by_id(r.clip_id))
                        .and_then(|c| self.song_doc.song().clip_notes(c).get(local).map(|n| n.duration_beats))
                    {
                        self.ui_prefs.last_note_duration_beats =
                            dur.max(common::model::MIN_NOTE_LEN_BEATS);
                    }
                }
            }
            AppEvent::DeleteSelectedNotes => self.delete_selected_notes(),
            AppEvent::DuplicateSelectedNotes => self.duplicate_selected_notes(),
            AppEvent::CopyNotes(entries) => self.copy_notes(&entries),
            AppEvent::SetNoteLyrics { clip_ref, lyrics } => {
                self.set_note_lyrics(clip_ref, &lyrics);
            }
            AppEvent::SetPianoRollTargetClip(key) => {
                self.set_pianoroll_target_clip(key);
            }
            AppEvent::TogglePianoRollTrackLock(track_id) => {
                self.toggle_pianoroll_track_lock(track_id);
            }
            AppEvent::NudgeSelectedNoteTime { step, steps } => {
                self.nudge_selected_notes_time(step, steps);
            }
            AppEvent::NudgeSelectedNoteLength { step, steps } => {
                self.nudge_selected_notes_length(step, steps);
            }
            AppEvent::NudgeSelectedNotePitch { octave, steps } => {
                self.nudge_selected_notes_pitch(octave, steps);
            }
            AppEvent::OpenPluginPicker => {
                self.ui_ephemeral.plugin_picker_query.clear();
                self.refresh_picker_visible();
                self.ui_ephemeral.is_plugin_picker_open = true;
            }
            AppEvent::ClosePluginPicker => {
                self.ui_ephemeral.is_plugin_picker_open = false;
                self.ui_ephemeral.plugin_picker_query.clear();
            }
            AppEvent::SetPluginPickerQuery(query) => {
                self.ui_ephemeral.plugin_picker_query = query;
                self.refresh_picker_visible();
            }
            AppEvent::MovePluginPickerCursor(delta) => {
                let len = self.ui_ephemeral.plugin_picker_visible.len();
                if len > 0 {
                    let new = (self.ui_ephemeral.plugin_picker_cursor as i32 + delta)
                        .clamp(0, len as i32 - 1) as usize;
                    self.ui_ephemeral.plugin_picker_cursor = new;
                }
            }
            AppEvent::OpenFontPicker => self.open_font_picker(),
            AppEvent::CloseFontPicker => self.close_font_picker(),
            AppEvent::SetFontPickerQuery(query) => {
                self.ui_ephemeral.font_picker_query = query;
                self.refresh_font_picker_visible();
            }
            AppEvent::MoveFontPickerCursor(delta) => self.move_font_picker_cursor(delta),
            AppEvent::HoverFontInPicker(idx) => self.hover_font_in_picker(idx),
            AppEvent::CommitFontFromPicker(family) => self.commit_font_from_picker(family),
            AppEvent::FontFamiliesLoaded(families) => self.on_font_families_loaded(families),
            AppEvent::AssetDecodeTick => self.on_asset_decode_tick(),
            AppEvent::RescanProgress { done, total } => {
                self.media.load_progress = Some((done, total));
                self.media.load_progress_label = "プラグインを走査中";
            }
            AppEvent::RescanPluginDb => {
                self.begin_rescan();
            }
            AppEvent::PluginDbRescanCompleted => {
                self.finish_rescan();
            }
            AppEvent::SetArrangeScroll(scroll) => {
                self.ui_prefs.arrange_scroll_beat = scroll.max(0.0);
                // 再生中の手動横スクロールは追従を解除する (ユーザー選択の挙動)。
                self.cancel_follow_on_manual_view_change();
            }
            AppEvent::CycleArrangeFollow => self.cycle_arrange_follow(),
            AppEvent::SetArrangeTrackRowH(h) => {
                // 上限は viewport 高に近いところまで広げる (1 トラックを画面いっぱいに
                // 表示できるようにする)。 viewport 高はここでは未知なので大きめに取り、
                // 実描画時は lanes 高さと min を取って絶対に visible 数 0 にならない構造で
                // 描画側 (`view_build` の `tracks_visible`) が吸収する。
                self.ui_prefs.arrange_track_row_h =
                    h.clamp(MIN_ARRANGE_ROW_H, MAX_ARRANGE_ROW_H);
            }
            AppEvent::SetArrangeHeaderW(w) => {
                // track 名が読める下限と lanes を潰さない上限で clamp。 widget は
                // 毎フレーム `view.header_w` としてこの値を読むので即反映される。
                self.ui_prefs.arrange_header_w = w.clamp(80.0, 480.0);
            }
            AppEvent::SetArrangeZoom(zoom) => {
                self.ui_prefs.arrange_zoom_x = zoom.clamp(2.0, 400.0);
                // 再生中の手動ズームは追従を解除する (ユーザー選択の挙動)。
                self.cancel_follow_on_manual_view_change();
            }
            AppEvent::SetPianoRollScrollX(scroll) => {
                if let Some(v) = self.piano_roll_view_entry() {
                    v.scroll_beat = scroll.max(0.0);
                }
            }
            AppEvent::SetPianoRollTopPitch(p) => {
                if let Some(v) = self.piano_roll_view_entry() {
                    v.top_pitch = p.clamp(11, 127);
                }
            }
            AppEvent::SetPianoRollZoomX(zoom) => {
                if let Some(v) = self.piano_roll_view_entry() {
                    v.zoom_x = zoom.clamp(8.0, 400.0);
                }
            }
            AppEvent::SetPianoRollZoomY(zoom) => {
                if let Some(v) = self.piano_roll_view_entry() {
                    v.zoom_y = zoom.clamp(6.0, 40.0);
                }
            }
            AppEvent::SetLoopRange { start, end } => {
                self.set_loop_range(start, end);
            }
            AppEvent::SelectPluginFromDb { id, keep_open, open_gui } => {
                self.select_plugin_from_db(id, keep_open, open_gui);
            }
            AppEvent::ToggleSlotGui { device_id } => {
                self.toggle_slot_gui(device_id);
            }
            // r.md #55: 閉じた 1 枚ごとに `SlotGuiClosed` が返ってくるので、
            // `ipc.open_plugin_guis` の掃除は ✕ を押したときと同じ経路 (on_gui_closed)
            // に任せる。ここで先回りして帳簿を clear しない (二重管理を作らない)。
            AppEvent::CloseAllPluginEditors => {
                self.send_plugin(PluginCommand::CloseAllSlotGuis);
            }
            AppEvent::SetVideoFxParam { device_id, param_id, value_real } => {
                self.set_video_fx_param(device_id, param_id, value_real);
            }
            AppEvent::SetPluginParam { device_id, param_id, value_real } => {
                self.set_plugin_param(device_id, param_id, value_real);
            }
            AppEvent::RemoveDevices { device_ids } => {
                self.remove_devices(device_ids);
            }
            AppEvent::RelocateDevices(req) => {
                self.relocate_devices(req);
            }
            AppEvent::SelectDevice { device_id, modifier } => {
                self.apply_select_device(device_id, modifier);
            }
            AppEvent::ReloadDevice { device_id } => {
                self.reload_device(device_id);
            }
            AppEvent::ExplodeParallelOut { device_id } => {
                self.explode_parallel_out(device_id);
            }
            AppEvent::SetParallelOutputRoute {
                device_id,
                port,
                dest,
            } => {
                self.set_parallel_output_route(device_id, port, dest);
            }
            AppEvent::SetSidechainSource {
                device_id,
                port,
                source,
            } => {
                self.set_sidechain_source(device_id, port, source);
            }
            AppEvent::SetPluginSendAllKeys { device_id, enabled } => {
                self.set_plugin_send_all_keys(device_id, enabled);
            }
            AppEvent::AddModSource { kind } => self.add_mod_source(kind),
            AppEvent::EditModSource { id, edit } => self.edit_mod_source(id, edit),
            AppEvent::RemoveModSource { id } => self.remove_mod_source(id),
            AppEvent::AddModRouting {
                track_id,
                target,
                source_id,
            } => {
                // 戻り値 (実際に足したか) は per-control ドラッグ経路では不要
                // (毎フレーム呼ばれ、 2 回目以降は no-op)。
                let _ = self.add_mod_routing(track_id, target, source_id);
            }
            AppEvent::RemoveModRouting {
                track_id,
                target,
                source_id,
            } => self.remove_mod_routing(track_id, target, source_id),
            AppEvent::SetModRoutingDepth {
                track_id,
                target,
                source_id,
                depth,
            } => self.set_mod_routing_depth(track_id, target, source_id, depth),
            AppEvent::SetModRoutingPolarity {
                track_id,
                target,
                source_id,
                bipolar,
            } => self.set_mod_routing_polarity(track_id, target, source_id, bipolar),
            AppEvent::SetModSourceTrack { id, source_track } => {
                self.set_mod_source_track(id, source_track)
            }
            AppEvent::SetModSourceAttack { id, ms } => self.set_mod_source_attack(id, ms),
            AppEvent::SetModSourceRelease { id, ms } => self.set_mod_source_release(id, ms),
            AppEvent::SetModSourceGain { id, gain } => self.set_mod_source_gain(id, gain),
            AppEvent::SetModSourceMode { id, mode } => self.set_mod_source_mode(id, mode),
            AppEvent::SetModSourceRectify { id, rectify } => {
                self.set_mod_source_rectify(id, rectify)
            }
            AppEvent::SetModSourceBand { id, band } => self.set_mod_source_band(id, band),
            AppEvent::SetModFollowerScrubbing(active) => self.set_mod_follower_scrubbing(active),
            AppEvent::SetModSourceTapPoint { id, tap_point } => {
                self.set_mod_source_tap_point(id, tap_point)
            }
            AppEvent::SetArmedModSource(id) => self.ui_ephemeral.armed_mod_source = id,
            AppEvent::SetAuxInputTapPoint {
                device_id,
                port,
                tap_point,
            } => self.set_aux_input_tap_point(device_id, port, tap_point),
            AppEvent::ReorderInspectorChain(order) => {
                self.reorder_inspector_chain(&order);
            }
            AppEvent::SetMasterGain(amp) => {
                self.set_master_gain(amp);
            }
            // マスターフェーダーの drag を 1 undo step に束ねる。これが無いと
            // per-frame の `SetMasterGain` が各々 snapshot を積み、1 回の drag で
            // undo 履歴が埋まる (group transform / inspector scrub と同じ罠)。
            AppEvent::BeginMasterGainDrag => {
                self.ui_ephemeral.master_gain_dragging = true;
                self.song_doc.begin_gesture();
            }
            AppEvent::EndMasterGainDrag => {
                self.ui_ephemeral.master_gain_dragging = false;
                self.song_doc.end_gesture();
            }
            AppEvent::Tick {
                samples,
                preroll,
                playing,
                recording_live,
            } => {
                // r.md #51: engine が所有する状態をここで観測する。
                // **`transport.is_playing` / `recording.live` を書くのはここだけ**
                // (他所で立てると engine の実状態と食い違い、Rec 単独録音で
                // プレイヘッド凍結・オートメーション未記録・曲末で止まらない、が
                // 一度に起きていた)。
                self.transport.preroll_remaining = preroll;
                self.recording.live = recording_live;
                let stopped = self.transport.is_playing && !playing;
                self.transport.is_playing = playing;
                self.on_tick(samples);
                if stopped {
                    // 手動停止・曲末 auto-stop・書き出し・パニックのどれで止まっても
                    // ここへ収束する (停止ホームへの復帰と録音セッションのクローズ)。
                    self.on_transport_stopped();
                }
            }
            // r.md #50: マスターメーターの表示状態は解析器が丸ごと作るので、
            // ここは差し替えるだけ (GUI 側で弾道を二重に掛けない)。
            AppEvent::MasterMeterTick(snapshot) => {
                self.transport.master_meter = *snapshot;
            }
            AppEvent::SetTrackVolume { track, amp } => {
                self.set_track_volume(track, amp);
            }
            AppEvent::SetTrackPan { track, pan } => {
                self.set_track_pan(track, pan);
            }
            AppEvent::SetTrackColor { track, color } => {
                self.edit_song(|song| {
                    if let Some(t) = song.tracks.iter_mut().find(|t| t.id == track) {
                        t.color = color;
                    }
                });
            }
            AppEvent::ResetTrackClipColors { track } => {
                // 全 clip の上書きを外す (= track 色継承)。undo は is_undoable で取得済。
                self.edit_song(|song| {
                    if let Some(t) = song.tracks.iter_mut().find(|t| t.id == track) {
                        for clip in &mut t.clips {
                            clip.color = None;
                        }
                    }
                });
            }
            AppEvent::ToggleTrackMute(track) => {
                self.toggle_track_mute(track);
            }
            AppEvent::ToggleTrackSolo(track) => {
                self.toggle_track_solo(track);
            }
            AppEvent::ToggleTrackArmed(track) => {
                self.toggle_track_armed(track);
            }
            AppEvent::StripEdit { track, edit } => {
                self.apply_strip_edit(track, &edit);
            }
            AppEvent::MasterStripEdit { param, value } => {
                self.apply_master_strip_edit(param, value);
            }
            AppEvent::ToggleStripSection(section) => {
                self.toggle_strip_section(section);
            }
            AppEvent::TrackPeaksTick { tracks, master_gr } => {
                self.on_track_peaks_tick(&tracks, master_gr);
            }
            AppEvent::LauncherRowsTick(rows) => {
                self.on_launcher_rows_tick(rows);
            }
            AppEvent::MetricsTick {
                dsp_load_peak,
                dsp_load_avg,
                xrun_count,
                buffer_frames,
                sample_rate,
            } => {
                self.ipc.metrics.dsp_load_peak = dsp_load_peak;
                self.ipc.metrics.dsp_load_avg = dsp_load_avg;
                self.ipc.metrics.xrun_count = xrun_count;
                self.ipc.metrics.buffer_frames = buffer_frames;
                self.ipc.metrics.sample_rate = sample_rate;
            }
            AppEvent::SystemMetricsTick { cpu, mem_mb } => {
                self.ipc.metrics.system_cpu = cpu;
                self.ipc.metrics.memory_mb = mem_mb;
            }
            AppEvent::ToggleResourceMonitor => {
                self.ui_prefs.resource_monitor_enabled = !self.ui_prefs.resource_monitor_enabled;
                // app_config.json に永続化 (プロジェクト非依存の UI 設定)。
                self.persist_app_config();
            }
            AppEvent::ToggleResourcePanel => {
                self.ui_ephemeral.resource_panel_open = !self.ui_ephemeral.resource_panel_open;
            }
            AppEvent::ModScalarsTick(plane) => {
                // docs/plan_modulation.md §4.2: snapshot the latest modulator
                // values (already attack/release-smoothed by the engine — no
                // extra GUI smoothing). Zero-copy: move the polled plane in。
                self.transport.mod_plane = plane;
            }
            AppEvent::AddReturnTrack => {
                self.action_add_return_track();
            }
            AppEvent::AddSend { src_track_id, dest_track_id } => {
                self.add_send(src_track_id, dest_track_id);
            }
            AppEvent::RemoveSend { track_id, send_idx } => {
                self.remove_send(track_id, send_idx);
            }
            AppEvent::SetSendMode { track_id, send_idx, mode } => {
                self.set_send_mode(track_id, send_idx, mode);
            }
            AppEvent::SetSendGain { track_id, send_idx, gain } => {
                self.set_send_gain(track_id, send_idx, gain);
            }
            AppEvent::SetSendEnabled { track_id, send_idx, enabled } => {
                self.set_send_enabled(track_id, send_idx, enabled);
            }
            AppEvent::OpenSendPicker { src_track_id } => {
                self.ui_ephemeral.send_picker = Some(SendPickerState { src_track_id });
            }
            AppEvent::CloseSendPicker => {
                self.ui_ephemeral.send_picker = None;
            }
            AppEvent::ExportWav => {
                self.open_export_range_picker(ExportRangeKind::Wav);
            }
            AppEvent::SetExportRangeStart(beat) => {
                if let Some(p) = self.ui_ephemeral.export_range_picker.as_mut() {
                    // start は [0, end) に clamp。 end と等しくなる入力は拒否
                    // (end より僅かに手前へ)。
                    p.start_beat = beat.clamp(0.0, (p.end_beat - MIN_EXPORT_RANGE_BEATS).max(0.0));
                }
            }
            AppEvent::SetExportRangeEnd(beat) => {
                if let Some(p) = self.ui_ephemeral.export_range_picker.as_mut() {
                    let max = self.song_doc.song().length_beats.max(p.start_beat + MIN_EXPORT_RANGE_BEATS);
                    p.end_beat = beat.clamp(p.start_beat + MIN_EXPORT_RANGE_BEATS, max);
                }
            }
            AppEvent::ResetExportRange => {
                if let Some(p) = self.ui_ephemeral.export_range_picker.as_mut() {
                    p.start_beat = 0.0;
                    p.end_beat = self.song_doc.song().length_beats.max(MIN_EXPORT_RANGE_BEATS);
                }
            }
            // r.md #54: 範囲プリセット。対象が無いときは何も変えず理由を出す
            // (ボタンを押しても黙って無反応、を作らない)。
            AppEvent::SetExportRangeSource(source) => {
                match self.export_range_from_source(source) {
                    Some((start, end)) => {
                        let len = self.song_doc.song().length_beats;
                        if let Some(p) = self.ui_ephemeral.export_range_picker.as_mut() {
                            p.start_beat = start.max(0.0);
                            p.end_beat = end.max(p.start_beat + MIN_EXPORT_RANGE_BEATS);
                            // 曲末より後ろは掴めない (end 側の clamp と同じ規則)。
                            let max = len.max(p.start_beat + MIN_EXPORT_RANGE_BEATS);
                            p.end_beat = p.end_beat.min(max);
                        }
                    }
                    None => {
                        self.ui_ephemeral.status_message =
                            format!("{}が無いので範囲を取れません", source.label());
                    }
                }
            }
            AppEvent::SetExportResolution(w, h) => {
                // dropdown はプリセット (全て偶数・正値) しか出さないが、
                // 念のため 0 を弾く (encoder は w/h != 0 を要求)。
                if let Some(p) = self.ui_ephemeral.export_range_picker.as_mut()
                    && w > 0
                    && h > 0
                {
                    p.resolution = (w, h);
                }
            }
            AppEvent::SetExportFramerate(fps) => {
                if let Some(p) = self.ui_ephemeral.export_range_picker.as_mut()
                    && fps > 0.0
                {
                    p.framerate = fps;
                }
            }
            AppEvent::ConfirmExportRange => {
                self.confirm_export_range();
            }
            AppEvent::CancelExportRange => {
                let was_loudness = self
                    .ui_ephemeral
                    .export_range_picker
                    .is_some_and(|p| matches!(p.kind, ExportRangeKind::Loudness));
                self.ui_ephemeral.export_range_picker = None;
                self.ui_ephemeral.status_message = if was_loudness {
                    "ラウドネス解析をキャンセルしました".into()
                } else {
                    "Export をキャンセルしました".into()
                };
            }
            // -------- ラウドネス解析 (r.md #54) --------
            AppEvent::AnalyzeLoudness => {
                self.open_loudness_range_picker();
            }
            AppEvent::ToggleLoudnessReport => {
                self.toggle_loudness_report();
            }
            AppEvent::RerunLoudnessAnalysis => {
                if let Some(r) = self.loudness.report.as_ref() {
                    let range = Some((r.range_start_beat, r.range_end_beat));
                    self.begin_loudness_analysis(range);
                }
            }
            AppEvent::CancelLoudnessAnalysis => {
                self.cancel_loudness_analysis();
            }
            AppEvent::SetLoudnessTarget { lufs, ceiling_dbtp } => {
                self.set_loudness_target(lufs, ceiling_dbtp);
            }
            AppEvent::SeekToLoudnessPosition(secs) => {
                self.seek_to_loudness_position(secs);
            }
            AppEvent::ExportMidi => {
                self.action_export_midi();
            }
            AppEvent::ImportAudio { paths, target, target_beat } => {
                self.action_import_audio(paths, target, target_beat);
            }
            AppEvent::OpenImportAudioDialog => {
                self.action_open_import_audio_dialog();
            }
            AppEvent::ImportVideo { paths, target_beat } => {
                #[cfg(windows)]
                self.action_import_video(paths, target_beat);
                #[cfg(not(windows))]
                {
                    let _ = (paths, target_beat);
                    self.ui_ephemeral.status_message =
                        "Video import は Windows 専用 (WMF 経由) です".into();
                }
            }
            AppEvent::OpenImportVideoDialog => {
                #[cfg(windows)]
                self.action_open_import_video_dialog();
                #[cfg(not(windows))]
                {
                    self.ui_ephemeral.status_message =
                        "Video import は Windows 専用 (WMF 経由) です".into();
                }
            }
            AppEvent::ImportImage { paths, target, target_beat } => {
                self.action_import_image(paths, target, target_beat);
            }
            AppEvent::OpenImportImageDialog => {
                self.action_open_import_image_dialog();
            }
            AppEvent::ImportMidi { paths, target, target_beat } => {
                self.action_import_midi(paths, target, target_beat);
            }
            AppEvent::OpenImportMidiDialog => {
                self.action_open_import_midi_dialog();
            }
            AppEvent::AddTextClipAt { track, start_beat } => {
                self.add_text_clip_to_track(track, start_beat);
            }
            AppEvent::TogglePreviewWindow => {
                self.ui_prefs.preview_window_visible = !self.ui_prefs.preview_window_visible;
                self.ui_ephemeral.status_message = if self.ui_prefs.preview_window_visible {
                    "Video preview: 表示".into()
                } else {
                    "Video preview: 非表示".into()
                };
            }
            AppEvent::OpenExportMp4Dialog => {
                #[cfg(windows)]
                self.open_export_range_picker(ExportRangeKind::Mp4);
                #[cfg(not(windows))]
                {
                    self.ui_ephemeral.status_message =
                        "Video export は Windows 専用 (WMF 経由) です".into();
                }
            }
            AppEvent::FileDialogResult { kind, paths } => {
                self.handle_file_dialog_result(kind, paths);
            }
            AppEvent::SaveAsResolved { path } => {
                self.ui_ephemeral.save_as_dialog_open = false;
                let Some(path) = path else {
                    // Save As キャンセル → 「保存して続行」 の保留操作を取り消し、
                    // アプリに留まる (旧同期フローの「何もしない」 と同義)。
                    self.ui_ephemeral.guard_after_save = None;
                    return;
                };
                if let Some(dir) = path.parent()
                    && let Err(e) = std::fs::create_dir_all(dir)
                {
                    self.ui_ephemeral.status_message = format!(
                        "プロジェクトフォルダの作成に失敗: {} ({e})",
                        dir.display()
                    );
                    // 保存できないなら操作を実行しない (データ損失回避)。
                    self.ui_ephemeral.guard_after_save = None;
                    return;
                }
                self.begin_save(path);
                // 「保存して続行」 由来の保留操作があるとき: plugin 無しは begin_save が
                // 同期保存して is_dirty が下りるので即実行。 plugin 有りは
                // has_pending_save が立ち、 on_all_states 完了ハンドラ (既存) が実行
                // する。
                if self.ui_ephemeral.guard_after_save.is_some()
                    && !self.song_doc.is_dirty()
                    && !self.has_pending_save()
                    && let Some(action) = self.ui_ephemeral.guard_after_save.take()
                {
                    self.perform_guard_action(action);
                }
            }
            AppEvent::ExportMp4 { output_path, audio_wav, range_beats, dims } => {
                #[cfg(windows)]
                self.action_export_mp4(output_path, audio_wav, range_beats, dims);
                #[cfg(not(windows))]
                {
                    let _ = (output_path, audio_wav, range_beats, dims);
                    self.ui_ephemeral.status_message =
                        "Video export は Windows 専用 (WMF 経由) です".into();
                }
            }
            AppEvent::ExportProgress { done, total } => {
                self.transport.export_stage = Some(ExportStage::VideoRender { done, total });
            }
            AppEvent::ExportFinished { result } => {
                self.transport.export_stage = None;
                self.transport.export_cancel = None;
                // 自動レンダリングした音声 temp 一式 (WAV + sidecar) を削除。
                self.remove_export_temp_wav();
                match result {
                    Ok(path) => {
                        self.ui_ephemeral.status_message =
                            format!("Video export 完了: {}", path.display());
                    }
                    Err(e) if e == "export cancelled" => {
                        self.ui_ephemeral.status_message =
                            "Video export をキャンセルしました".into();
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "export failed");
                        self.ui_ephemeral.status_message = format!("Video export 失敗: {e}");
                    }
                }
            }
            AppEvent::CancelExport => match self.transport.export_stage {
                // 映像フェーズは daw_gui プロセス内の render thread。in-process の
                // atomic flag で次フレーム中断させる。
                Some(ExportStage::VideoRender { .. }) => {
                    if let Some(flag) = &self.transport.export_cancel {
                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        self.ui_ephemeral.status_message = "Video export をキャンセル中...".into();
                    }
                }
                // 音声 freewheel は daw_audio プロセス。IPC で cancel を送り、
                // freewheel ループが次 buffer で中断 → `ExportWavComplete
                // { error: None, cancelled: true }` が返る (cancel は typed flag で
                // 伝わる)。標準 WAV export / video 前段のどちらでも有効。
                Some(ExportStage::AudioRender { .. }) => self.cancel_audio_render(),
                None => {}
            },
            AppEvent::SetClipReversed { target, reversed } => {
                self.set_clip_audio_event_reversed(target, reversed);
            }
            AppEvent::SetClipColor { target, color } => {
                self.edit_song(|song| propagate_clip_color(&mut song.tracks, target, color));
            }
            AppEvent::SetClipMuted { target, muted } => {
                // clip-level mute の SSoT (`Clip.muted`)。 content type を問わない。
                self.set_clip_muted(target, muted);
            }
            AppEvent::SetClipsMuted { targets, muted } => {
                // `q` で選択 clip / カーソル直下 clip を一括 toggle した結果。
                let _ = self.edit_song(|song| {
                    let mut changed = false;
                    for target in targets {
                        if let Some(track) = song.track_by_id_mut(target.track_id)
                            && let Some(clip) = track.clip_by_id_mut(target.clip_id)
                            && clip.muted != muted
                        {
                            clip.muted = muted;
                            changed = true;
                        }
                    }
                    changed
                });
            }
            AppEvent::SetNotesMuted { notes, muted } => {
                // `q` で選択 note / カーソル直下 note (packed id) を一括 toggle。
                self.set_notes_muted(&notes, muted);
            }
            AppEvent::SetClipStretchMode { target, mode } => {
                self.set_clip_audio_event_stretch_mode(target, mode);
            }
            AppEvent::ResyncClipEditBuffers(target) => {
                // 数値 buffer は撤去済み。 text section と共有する
                // `clip_edit_buffer_target` を target に向ける純 sync。
                self.ui_ephemeral.clip_edit_buffer_target = Some(target);
            }
            AppEvent::SetClipGainDb { target, gain_db } => {
                self.set_clip_audio_event_gain_db(target, gain_db);
            }
            AppEvent::SetClipPan { target, pan } => {
                self.set_clip_audio_event_pan(target, pan);
            }
            AppEvent::SetClipPitchSemitones { target, semitones } => {
                self.set_clip_audio_event_pitch_semitones(target, semitones);
            }
            AppEvent::SetClipFormantSemitones { target, semitones } => {
                self.set_clip_audio_event_formant_semitones(target, semitones);
            }
            AppEvent::SetClipFadeInBeats { target, beats } => {
                if self.is_image_clip(target) {
                    self.set_clip_image_event_fade_in_beats(target, beats);
                } else {
                    self.set_clip_audio_event_fade_in_beats(target, beats);
                }
            }
            AppEvent::SetClipFadeOutBeats { target, beats } => {
                if self.is_image_clip(target) {
                    self.set_clip_image_event_fade_out_beats(target, beats);
                } else {
                    self.set_clip_audio_event_fade_out_beats(target, beats);
                }
            }
            AppEvent::SetClipFadeInCurve { target, curve } => {
                if self.is_image_clip(target) {
                    self.set_clip_image_event_fade_in_curve(target, curve);
                } else {
                    self.set_clip_audio_event_fade_in_curve(target, curve);
                }
            }
            AppEvent::SetClipFadeOutCurve { target, curve } => {
                if self.is_image_clip(target) {
                    self.set_clip_image_event_fade_out_curve(target, curve);
                } else {
                    self.set_clip_audio_event_fade_out_curve(target, curve);
                }
            }
            AppEvent::SetClipImageX { target, value } => {
                self.set_clip_image_event_x(target, value);
            }
            AppEvent::SetClipImageY { target, value } => {
                self.set_clip_image_event_y(target, value);
            }
            AppEvent::SetClipImageW { target, value } => {
                self.set_clip_image_event_w(target, value);
            }
            AppEvent::SetClipImageH { target, value } => {
                self.set_clip_image_event_h(target, value);
            }
            AppEvent::SetClipImageOpacity { target, value } => {
                self.set_clip_image_event_opacity(target, value);
            }
            AppEvent::SetClipImageRotation { target, value } => {
                self.set_clip_image_event_rotation_radians(target, value);
            }
            AppEvent::SetClipTextX { target, value } => {
                self.set_clip_text_event_x(target, value);
            }
            AppEvent::SetClipTextY { target, value } => {
                self.set_clip_text_event_y(target, value);
            }
            AppEvent::SetClipTextW { target, value } => {
                self.set_clip_text_event_w(target, value);
            }
            AppEvent::SetClipTextH { target, value } => {
                self.set_clip_text_event_h(target, value);
            }
            AppEvent::SetClipTextRotation { target, value } => {
                self.set_clip_text_event_rotation_radians(target, value);
            }
            AppEvent::BeginTextPiPDrag => {
                // r.md #28: preview canvas 上の text PiP drag 全体を 1 undo step に bracket
                // する (image PiP と同 idiom)。
                self.song_doc.begin_gesture();
                self.begin_text_pip_drag_recording();
            }
            AppEvent::EndTextPiPDrag => {
                self.end_text_pip_drag_recording();
                self.song_doc.end_gesture();
            }
            AppEvent::SetClipTextMuted { target, muted } => {
                // 字幕 clip mute も clip-level `Clip.muted` に一本化。
                self.set_clip_muted(target, muted);
            }
            AppEvent::SetClipTextContent { target, value } => {
                self.set_clip_text_event_content(target, value);
            }
            AppEvent::SetClipTextFontFamily { target, value } => {
                self.set_clip_text_event_font_family(target, value);
            }
            AppEvent::SetClipTextAlign { target, value } => {
                self.set_clip_text_event_align(target, value);
            }
            AppEvent::SetClipTextFadeInCurve { target, curve } => {
                self.set_clip_text_event_fade_in_curve(target, curve);
            }
            AppEvent::SetClipTextFadeOutCurve { target, curve } => {
                self.set_clip_text_event_fade_out_curve(target, curve);
            }
            AppEvent::SetClipTextNumField { target, field, value } => {
                self.set_clip_text_num_field(target, field, value);
            }
            AppEvent::ClipTextContentEditChanged(s) => {
                self.ui_ephemeral.clip_text_content_edit_text = s;
            }
            AppEvent::ClipTextFontFamilyEditChanged(s) => {
                self.ui_ephemeral.clip_text_font_family_edit_text = s;
            }
            AppEvent::CommitClipTextContentEdit => {
                self.commit_clip_text_content_edit();
            }
            AppEvent::CommitClipTextFontFamilyEdit => {
                self.commit_clip_text_font_family_edit();
            }
            AppEvent::ResyncClipTextEditBuffers(target) => {
                self.resync_clip_text_event_edit_buffers(target);
            }
            AppEvent::AutoFadeSelectedClips => {
                self.auto_fade_selected_clips();
            }
            AppEvent::AutoCrossfadeSelectedClips => {
                self.auto_crossfade_selected_clips();
            }
            AppEvent::OpenAudioEditor(target) => {
                self.open_audio_editor(target);
            }
            AppEvent::CloseAudioEditor => {
                self.close_audio_editor();
            }
            AppEvent::SetAudioEditorScroll(start) => {
                self.set_audio_editor_scroll(start);
            }
            AppEvent::SetAudioEditorZoom { view_start_beat, view_len_beats } => {
                self.set_audio_editor_zoom(view_start_beat, view_len_beats);
            }
            AppEvent::SelectAudioEditorEvent(idx) => {
                self.set_audio_event_selection(&idx.into_iter().collect::<Vec<usize>>());
                if !self.selected_audio_event_indices().is_empty() {
                    self.selection.last_edit_select = Some(EditSurface::AudioEvents);
                }
            }
            AppEvent::SetAudioEditorEventSelection(indices) => {
                self.set_audio_editor_event_selection(indices);
            }
            AppEvent::DeleteAudioEditorSelection => {
                self.delete_audio_editor_selection();
            }
            AppEvent::DuplicateAudioEditorEvent => {
                self.duplicate_audio_editor_event();
            }
            AppEvent::AutoWarpSelectedClip => {
                if let Some(target) = self.selected_clip_ref() {
                    self.auto_warp_clip(target);
                }
            }
            AppEvent::MoveWarpMarker { event_idx, marker_idx, new_locked_beat } => {
                self.mutate_warp_markers(event_idx, |m| {
                    common::audio_render::move_warp_marker(m, marker_idx, new_locked_beat);
                });
            }
            AppEvent::AddWarpMarker { event_idx, source_frame, locked_beat } => {
                self.mutate_warp_markers(event_idx, |m| {
                    common::audio_render::add_warp_marker(m, source_frame, locked_beat);
                });
            }
            AppEvent::DeleteWarpMarker { event_idx, marker_idx } => {
                self.mutate_warp_markers(event_idx, |m| {
                    common::audio_render::delete_warp_marker(m, marker_idx);
                });
            }
            AppEvent::SetAudioEventStart { clip, event_idx, new_start_beats } => {
                self.set_audio_event_start(clip, event_idx, new_start_beats);
            }
            AppEvent::SetAudioEventTrim { clip, event_idx, side, delta_beats } => {
                self.set_audio_event_trim(clip, event_idx, side, delta_beats);
            }
            AppEvent::AddAudioEventFromFile { clip, path, position_in_clip_beats } => {
                self.add_audio_event_from_file(clip, path, position_in_clip_beats);
            }
            AppEvent::ToggleClipReversed(target) => {
                let cur = self.is_clip_audio_event_reversed(target);
                self.set_clip_audio_event_reversed(target, !cur);
            }
            AppEvent::BounceClipInPlace(target) => {
                self.bounce_clip_in_place(target);
            }
            AppEvent::BounceClipWithFx(target) => {
                self.bounce_clip_with_fx(target);
            }
            AppEvent::SetClipFadeBeatsBatch(entries) => {
                for (target, edge, beats) in &entries {
                    let beats = *beats;
                    let edge = *edge;
                    self.set_clip_event_fade(*target, move |mut f| {
                        // 上限は event 長 (音 / 映像 / 画像 / 字幕が全部 event 長基準で
                        // fade を適用するため。 r.md #38)。
                        let v = beats.clamp(0.0, f.len_beats.max(0.0));
                        match edge {
                            FadeEdgeKind::In => f.fade_in_beats = v,
                            FadeEdgeKind::Out => f.fade_out_beats = v,
                        }
                        f
                    });
                }
            }
            AppEvent::SetClipFadeCurveBatch(entries) => {
                for (target, edge, curve) in &entries {
                    let curve = *curve;
                    let edge = *edge;
                    self.set_clip_event_fade(*target, move |mut f| {
                        match edge {
                            FadeEdgeKind::In => f.fade_in_curve = curve,
                            FadeEdgeKind::Out => f.fade_out_curve = curve,
                        }
                        f
                    });
                }
            }
            AppEvent::BroadcastDiscreteClipEdit { targets, edit } => {
                // discrete トグル/ドロップダウンを選択全クリップへ一括適用。
                // 1 イベント = 1 undo snapshot (is_undoable)、 ここで per-clip setter を
                // ループする。 各 setter は variant-safe なので種別違いは no-op。
                for &t in &targets {
                    match edit {
                        DiscreteClipEdit::Reversed(v) => self.set_clip_audio_event_reversed(t, v),
                        // inspector の "Mute" トグルも clip-level `Clip.muted` に一本化。
                        DiscreteClipEdit::Muted(v) => self.set_clip_muted(t, v),
                        DiscreteClipEdit::StretchMode(m) => {
                            self.set_clip_audio_event_stretch_mode(t, m);
                        }
                        DiscreteClipEdit::FadeCurve(edge, c) => match edge {
                            FadeEdgeKind::In => {
                                if self.is_image_clip(t) {
                                    self.set_clip_image_event_fade_in_curve(t, c);
                                } else {
                                    self.set_clip_audio_event_fade_in_curve(t, c);
                                }
                            }
                            FadeEdgeKind::Out => {
                                if self.is_image_clip(t) {
                                    self.set_clip_image_event_fade_out_curve(t, c);
                                } else {
                                    self.set_clip_audio_event_fade_out_curve(t, c);
                                }
                            }
                        },
                        // 字幕 inspector の "Mute" も clip-level `Clip.muted` に一本化。
                        DiscreteClipEdit::TextMuted(v) => self.set_clip_muted(t, v),
                        DiscreteClipEdit::TextAlign(a) => self.set_clip_text_event_align(t, a),
                        DiscreteClipEdit::TextFadeCurve(edge, c) => match edge {
                            FadeEdgeKind::In => self.set_clip_text_event_fade_in_curve(t, c),
                            FadeEdgeKind::Out => self.set_clip_text_event_fade_out_curve(t, c),
                        },
                    }
                }
            }
            AppEvent::SplitClipAtPlayhead { snap } => {
                self.action_split_clips_at_cursor(snap);
            }
            AppEvent::GlueSelectedClips => {
                self.action_glue_selected_clips();
            }
            // PR-V4: SynthesizeVocal / VocalSynthCompleted は削除済。
            // vocal track は builtin VOICEVOX plugin が自動 synth する
            // (= sync_vocal_metadata 経由で歌詞 / note を flush →
            // background thread で HTTP synth)。 user の explicit
            // synth トリガは不要。
            AppEvent::SingersLoaded(singers) => {
                tracing::info!(
                    count = singers.len(),
                    "VOICEVOX singers loaded"
                );
                self.voicevox.singers = singers;
                // Clip Inspector の 2 段 dropdown は `singers` を
                // 直接読む (キャラ→style の階層が要るので flat cache は持たない)。
            }
            AppEvent::LipsyncGenerated { vocal_track_id, target_track_id, bpm, clips, generation } => {
                // 成功/失敗/空に関わらず in-flight 解除 (= スピナーを止める)。
                // generation が古くても (project 切替後でも) 必ず外す。
                self.voicevox.lipsync_inflight.remove(&target_track_id);
                // spawn 後に project が切り替わった (reset_saved_baseline
                // が gen を bump した) 古い結果は捨てる。 適用すると別 project の口
                // track を作り直して spurious dirty になる。 debounce leg と同 idiom。
                if generation == self.voicevox.lipsync_gen && !clips.is_empty() {
                    self.apply_lipsync_generated(vocal_track_id, bpm, clips);
                } else if generation == self.voicevox.lipsync_gen {
                    // 空 = 全 query 失敗 (engine 起動中等。 ソース無しは spawn 前に
                    // return 済み)。 発注時に記録した fingerprint を rollback して、
                    // 次の debounce で自動リトライさせる (残すと「最新」扱いになり
                    // 入力を変えるまで口パクが欠けたまま再生成されない)。
                    self.voicevox.lipsync_fingerprints.remove(&target_track_id);
                }
            }
            AppEvent::SetLipsyncTarget { track, target } => {
                self.set_lipsync_target(track, target);
            }
            AppEvent::SetMouthMapSlot { track, shape, source_id } => {
                self.set_mouth_map_slot(track, shape, source_id);
            }
            AppEvent::LipsyncDebounceFired(generation) => {
                if generation == self.voicevox.lipsync_gen {
                    // (talk) regen は target 中心 (= その口 track を出力先にする全ソースを
                    // まとめて再生成) なので、口 track ごとに 1 回だけ呼べば足りる。同じ
                    // target を複数ソースぶん呼ぶと全ソース regen が重複するため、出力先
                    // track 単位で dedup し代表ソースを 1 つ渡す。
                    let mut targets: Vec<u32> = self
                        .song_doc.song()
                        .tracks
                        .iter()
                        .filter_map(|t| t.lipsync_target_track)
                        .collect();
                    targets.sort_unstable();
                    targets.dedup();
                    for target in targets {
                        // 入力 (notes / 歌詞 / bpm / mouth_map / binding / clip 位置) が
                        // 前回の再生成時から変わっていなければスキップ。track rename / 色 /
                        // mute / volume 等の非入力編集による無駄な再生成を防ぐ。
                        let fp = Self::lipsync_input_fingerprint(self.song_doc.song(), target);
                        if self.voicevox.lipsync_fingerprints.get(&target) == Some(&fp) {
                            continue;
                        }
                        if let Some(src_id) = self
                            .song_doc.song()
                            .tracks
                            .iter()
                            .find(|t| t.lipsync_target_track == Some(target))
                            .map(|t| t.id)
                        {
                            self.regenerate_lipsync_for_track(src_id);
                        }
                    }
                }
            }
            AppEvent::SetClipVoice { clip, speaker_id, singer_name, style_name } => {
                self.set_clip_voice(clip, speaker_id, singer_name, style_name);
            }
            AppEvent::RefetchSingers => {
                self.spawn_fetch_singers();
            }
            AppEvent::SpeakersLoaded(speakers) => {
                tracing::info!(count = speakers.len(), "VOICEVOX talk speakers loaded");
                self.voicevox.talk_speakers = speakers;
            }
            AppEvent::RefetchSpeakers => {
                self.spawn_fetch_speakers();
            }
            AppEvent::SetClipTalkParam { clip, param, value } => {
                self.set_clip_talk_param(clip, param, value);
            }
            AppEvent::SetPianoRollSnapEnabled(b) => {
                self.ui_prefs.pianoroll_snap_enabled = b;
            }
            AppEvent::SetPianoRollSnapChoice(c) => {
                self.ui_prefs.pianoroll_snap_choice = clamp_snap_choice(c);
            }
            AppEvent::SetArrangeSnapEnabled(b) => {
                self.ui_prefs.arrange_snap_enabled = b;
            }
            AppEvent::SetArrangeSnapChoice(c) => {
                self.ui_prefs.arrange_snap_choice = clamp_snap_choice(c);
            }
            AppEvent::TogglePianoRollSnap => {
                self.ui_prefs.pianoroll_snap_enabled = !self.ui_prefs.pianoroll_snap_enabled;
            }
            AppEvent::ToggleArrangeSnap => {
                self.ui_prefs.arrange_snap_enabled = !self.ui_prefs.arrange_snap_enabled;
            }
            AppEvent::NarrowPianoRollGrid => {
                self.ui_prefs.pianoroll_snap_choice =
                    crate::view::snap::narrow_choice(self.ui_prefs.pianoroll_snap_choice);
            }
            AppEvent::NarrowArrangeGrid => {
                self.ui_prefs.arrange_snap_choice =
                    crate::view::snap::narrow_choice(self.ui_prefs.arrange_snap_choice);
            }
            AppEvent::WidenPianoRollGrid => {
                self.ui_prefs.pianoroll_snap_choice =
                    crate::view::snap::widen_choice(self.ui_prefs.pianoroll_snap_choice);
            }
            AppEvent::WidenArrangeGrid => {
                self.ui_prefs.arrange_snap_choice =
                    crate::view::snap::widen_choice(self.ui_prefs.arrange_snap_choice);
            }
            AppEvent::TogglePianoRollTriplet => {
                self.ui_prefs.pianoroll_snap_choice =
                    crate::view::snap::toggle_triplet_choice(self.ui_prefs.pianoroll_snap_choice);
            }
            AppEvent::ToggleArrangeTriplet => {
                self.ui_prefs.arrange_snap_choice =
                    crate::view::snap::toggle_triplet_choice(self.ui_prefs.arrange_snap_choice);
            }
            AppEvent::FitPianoRollToClip => {
                self.fit_piano_roll_to_clip();
            }
            AppEvent::FitArrangeToContent => {
                self.fit_arrange_to_content();
            }
            AppEvent::ZoomArrangeToSelectedClip { automation } => {
                self.zoom_arrange_to_selected_clip(automation);
            }
            AppEvent::ArrangeZoomBack => {
                self.arrange_zoom_back();
            }
            AppEvent::CloneClipsLinked(entries) => {
                self.clone_clips_linked(&entries);
            }
            AppEvent::CloneClipsIndependent(entries) => {
                self.clone_clips_independent(&entries);
            }
            AppEvent::MakeClipUnique(target) => {
                self.make_clip_unique(target);
            }
            AppEvent::SetScaleAtPlayhead { root, scale } => {
                self.set_scale_at_playhead(root, scale);
            }
            AppEvent::ClearScaleChanges => {
                self.edit_song(|song| song.scale_changes.clear());
            }
            AppEvent::QuantizePitchesToScale(target) => {
                self.quantize_pitches_to_scale(target);
            }
            AppEvent::ToggleSnapOnDraw => {
                self.ui_prefs.snap_on_draw = !self.ui_prefs.snap_on_draw;
            }
            AppEvent::ToggleSnapLiveInput => {
                self.recording.snap_live_input = !self.recording.snap_live_input;
            }
            AppEvent::ToggleFoldToScale => {
                self.ui_prefs.piano_roll_fold = !self.ui_prefs.piano_roll_fold;
            }
        }
        // edit_song が export 中拒否を予約していたら status に表示する
        // (song 凍結の単一保証点は SongDoc::edit、 旧 allow-list gate の置換)。
        if let Some(msg) = self.song_doc.take_rejection() {
            self.ui_ephemeral.status_message = msg.into();
        }
    }
}



// ---------------------------------------------------------------------------
// Free standing helpers
// ---------------------------------------------------------------------------



































