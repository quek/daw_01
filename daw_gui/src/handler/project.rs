//! handler::project — New/Open/Save/recovery/autosave/dirty-guard + plugin reconcile + undo/redo
//!
//! app.rs から機械分割した `impl AppData` メソッド群 (挙動は元と同一)。
use crate::state::*;
use crate::app_types::*;
use crate::event::*;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use common::model::Song;
use common::plugin_format::PluginFormat;
use common::protocol::{AudioCommand, PluginCommand, SlotState};
use crate::import_audio;

impl AppData {
    /// **Song スコープ状態の破棄チョークポイント。**
    ///
    /// `Song` 内の id (`Track::id` / `PluginInstance::id` / `AudioSourceId` /
    /// `ImageSourceId` / `VideoSourceId` / `ModSource::id` / `ClipKey` …) も
    /// `(track_id, device_index)` のような位置キーも、**その Song の中でしか
    /// 意味を持たない名前**である (`IdAllocators` は project ごとに 1 から
    /// 再採番する)。したがって別プロジェクトを開いた瞬間、それらを key にした
    /// 派生キャッシュ・選択・UI 状態は **全部無効**になる。id 空間が重なるので、
    /// 放置すると「解決に成功してしまう」= 前 project の対象に対して操作が
    /// 走る (前の曲の波形が出る / 選んでいないクリップが消える / 掴めない
    /// ノートができる、等)。
    ///
    /// 個別に思い出して消す方式は破綻する (実際、`voicevox_metadata_sent` だけが
    /// 消され、他は消されずに残っていた)。**Song を差し替える経路はここを通す**
    /// こと。GPU テクスチャなど renderer を必要とするものは AppData から解放
    /// できないので、[`Self::project_generation`] を bump して runner 側に
    /// 解放させる。
    ///
    /// 破棄は 2 段。**(A)** 参照系 (選択 / アンカー / 開いているエディタの対象 /
    /// 子プロセス帳簿) は Song を差し替えた時点で常に無効 — 同じ project の
    /// 別スナップショット (保存版に戻す / recovery 復元) でも clip / point の
    /// id 構成は乖離しうる。**(B)** decode 済みメディア (音源 / 画像 / 動画 /
    /// GPU テクスチャ) は「id + 実体」で同一性が決まるので、`project_id` が
    /// 変わったときだけ捨てる (同 project の読み直しで再 decode しない)。
    pub(crate) fn reset_song_scoped_state(&mut self) {
        // r.md #51: 走っているものを畳む。 録音セッションを閉じないと、新しい曲で
        // Rec が点灯したまま (かつ engine 側は曲末 auto-stop が抑止されたまま) になる。
        // モニター音も、新 Song に無い track id 宛の note-off が残らないよう全部止める。
        //
        // ここは **Song 差し替えの後**に呼ばれる (`after_song_replaced`) ので、
        // 押しっぱなしノートの長さ確定は先に捨てる — 対象の Song はもう無く、
        // 同じ id が新 Song の別ノートに当たりうる。
        self.recording.midi_recording_active_notes.clear();
        self.stop();
        self.silence_monitor_notes();
        // r.md #67: カーソルキーの試聴音も止める (旧 Song の track id 宛の note-off が
        // 新 Song で宙に浮かないよう、モニター音と同じ扱いにする)。
        self.expire_nudge_audition(true);

        // ---- (A) Song を差し替えたら常に無効になるもの --------------------
        // 同じ project の別スナップショット (保存版に戻す / recovery 復元) でも
        // clip / point の id 構成は乖離しうるので、参照系は無条件に捨てる。
        //
        // -- 選択 / アンカー (ClipKey・track_id・lane_id・point index) -------
        // 解決できてしまうので、残すと Delete / Cut が非選択対象に当たる。
        self.selection.selected_track_ids.clear();
        self.selection.selected_section_ids.clear();
        self.selection.selected_scene_ids.clear();
        self.selection.selected_automation_clips.clear();
        self.selection.selected_automation_points.clear();
        self.selection.selected_clip = None;
        self.selection.selected_clips.clear();
        self.selection.selected_notes.clear();
        self.selection.audio_editor_selected_events.clear();
        // r.md #71 (プラグインのコピー / 移動): device 選択も project スコープ。
        self.selection.selected_device_ids.clear();
        self.selection.device_anchor = None;
        self.selection.clip_anchor = None;
        self.selection.note_anchor = None;
        self.selection.track_anchor = None;
        self.selection.section_anchor = None;
        self.selection.scene_anchor = None;
        self.selection.automation_point_anchor = None;
        self.selection.automation_clip_anchor = None;
        self.selection.audio_editor_anchor = None;
        self.selection.last_edit_select = None;

        // -- 開いているエディタ / インスペクタの対象 ------------------------
        // `audio_editor_clip` は positional `ClipKey` なので、開いたままだと
        // 新 project の track[i].clip[j] を編集対象にしてしまう。
        self.ui_ephemeral.audio_editor_clip = None;
        self.ui_ephemeral.armed_mod_source = None;
        self.ui_ephemeral.expanded_mod_sources.clear();
        self.ui_ephemeral.open_plugin_params = None;
        self.ui_ephemeral.open_video_fx_params = None;

        // -- track_id / ClipKey keyed の表示設定 ----------------------------
        // ViewState を持たない旧 .daw では `restore_view_state` が早期 return
        // するので、ここで消さないと前 project の値が適用され続ける。
        self.ui_prefs.locked_pr_tracks.clear();
        self.ui_prefs.expanded_automation_tracks.clear();
        self.ui_prefs.track_row_overrides.clear();
        self.ui_prefs.multi_clip_view_key.clear();
        self.ui_prefs.collapsed_groups.clear();

        // -- 子プロセスに関する帳簿 (すべて device_id keyed) ----------------
        // teardown_all_loaded_plugins が消し損ねる分をここで確実に落とす。
        self.ipc.plugin_param_values.clear();
        self.ipc.ara_doc_cache.clear();
        self.ipc.ara_pcm_materialized.clear();
        self.ipc.gui_open_requests.clear();
        // 進行中 bounce の完了通知を新 project に適用しない。
        self.ipc.pending_clip_fx_bounce = None;
        self.ipc.pending_vocal_synth_bounce = None;
        // r.md #54: 前の曲のラウドネスレポート (数値も「範囲 x – y」の拍も) を
        // 新しい曲のものとして見せない。走査中なら engine ごと畳む。
        self.abort_loudness_analysis("プロジェクトを切り替えたのでラウドネス解析を中止しました".into());
        self.loudness.report = None;
        self.loudness.error = None;

        // ---- (B) **別プロジェクト**のときだけ無効になるもの ----------------
        // decode 済みメディアは「id + 実体」で同一性が決まるので、同じ project
        // を読み直すだけなら有効なまま (再 decode は無駄)。project が変われば
        // id 空間ごと別物になるので全部捨てる。
        let project_id = self.song_doc.song().project_id;
        if project_id != 0 && project_id == self.ui_ephemeral.loaded_project_id {
            return;
        }
        self.ui_ephemeral.loaded_project_id = project_id;
        // renderer を持つ runner に「preview 側の GPU 状態も捨てろ」と伝える世代印
        // (`PreviewWindowState::clear_all` / `cached_rings`)。 handle として表現できる
        // main renderer のテクスチャは下の破棄予約で渡すので、 世代印は
        // **AppData から handle で表現できないものだけ** を担当する。
        self.ui_ephemeral.project_generation =
            self.ui_ephemeral.project_generation.wrapping_add(1);
        // r.md #42: main renderer 上のサムネイル / 画像テクスチャは破棄予約へ積む。
        // 参照を捨てるだけだと GPU 側 store に entry が残り、 プロジェクトを開き直す
        // たびに VRAM が単調増加する (サムネイルはネイティブ解像度で 4K なら 1 枚 33MB)。
        self.discard_gpu_derived_caches();
        self.media.audio_source_cache.retain(|_| false);
        self.media.image_source_bgra.clear();
        self.media.pending_image_uploads.clear();
        self.media.video_thumbnail_rgba.clear();
        self.media.pending_thumbnail_uploads.clear();
    }

    /// New / Open / Restore 時、 `SongDoc::replace_song` の直後に呼ぶ
    /// (履歴破棄 / clean 化 / epoch bump は replace_song 側が担う)。
    ///
    /// 派生データの再構築 (`begin_asset_decode`) は呼び出し側が **この直後** に行う
    /// (headless script は decode を起動しないので、ここには含めない)。
    /// 順序を逆にすると、前 project のキャッシュを見て「decode 済み」と判断した直後に
    /// そのキャッシュが捨てられ、job も無いまま波形 / サムネイルが永久に出ない。
    pub(crate) fn after_song_replaced(&mut self) {
        // Song スコープの派生状態を捨てる (この 1 行が漏れると id 衝突で
        // 前 project の対象に操作が走る)。
        self.reset_song_scoped_state();
        // load / new / recovery では直前に flush_song_sync が
        // 走り、 口パク binding を持つ project だと mark_lipsync_dirty が 400ms
        // debounce で自動再生成をスケジュールする。 保存ファイル内の口パク clip は
        // 既に authoritative なので、 ここで再生成すると mouth clip が新しい
        // clip id / content id で作り直され (apply_lipsync_generated)、
        // saved_song と差分が出て「開いただけで '*' が付く」。 derived データの
        // 再計算は source 編集時だけに限定したいので、 baseline 確定と同時に
        // pending の再生成を無効化する (= 既存 clip をそのまま温存)。
        self.cancel_pending_lipsync_regen();
        // r.md #27: 別 project の device 群への「送信済み metadata」を持ち越さない
        // (device_id は project ごとに再割当されるので stale entry が誤 hit すると
        // 新 project の vocal device に seed 合成が飛ばず無音になる)。各 device の
        // load 時にも個別 invalidate するが、ここで全消去して stale entry も掃う。
        self.reset_voicevox_sync_state();
        // 保存ファイル内の口パク clip は既に authoritative。 その clip を生成した
        // 入力 (notes / 歌詞 / bpm / mouth_map / binding) を fingerprint のベースライン
        // として記録し、開いた直後の非入力編集 (track rename 等) で口パクが再生成
        // されないようにする (= `LipsyncDebounceFired` が fingerprint 一致でスキップ)。
        self.seed_lipsync_fingerprints();
        // r.md #17/#18: 旧バージョンで生成した重なり口 clip / 隙間で口が消える clip を、
        // 現行の「1 本の連続 clip・隙間は閉じ口」不変条件へ決定論的に畳み直す。 既に
        // 目標形なら no-op (dirty 化しない) なので、 seed の直後 (= clean baseline 後) に
        // 呼んで、 実際に畳んだ legacy プロジェクトだけを dirty にする。
        self.normalize_lipsync_clips_on_load();
        // r.md #39: 保存済み口パクが **古い配置ルール** で作られていたら一度だけ
        // 再生成する。 上の fingerprint baseline は「入力が変わったか」しか見ないので、
        // 配置ルール自体を変えたときはこの世代チェックが唯一の再生成トリガになる
        // (合成 WAV 側の `CACHE_SCHEMA_VERSION` と対)。 現行世代なら何もしない。
        self.regenerate_outdated_lipsync_on_load();
        // `Z`/`X` のズーム履歴は旧 project の view / track id を指すので
        // 別 project に持ち越さない。 段階ズームのアンカーと lane 高
        // override も旧 project の lane key を指すので一緒に破棄。
        self.ui_ephemeral.arrange_zoom_history.clear();
        self.ui_ephemeral.arrange_zoom_anchor = None;
        self.ui_prefs.automation_lane_row_overrides.clear();
        // r.md #10: 別 project の `Home` 2 段トグル state を持ち越さない
        // (= 新 project 最初の Home は先頭クリップ位置から始める)。
        self.ui_ephemeral.home_toggle_at_first = false;
        // 編集面の last-wins タグ (r.md #43) を含む Song スコープの参照系は、
        // 冒頭の `reset_song_scoped_state` が一括で捨てている。 個別に消し直さない
        // (破棄の口を 2 つにすると、また片方だけ更新される)。
    }

    pub(crate) fn undo(&mut self) {
        // audio editor の対象は positional な `ClipKey` なので、 song を差し替える
        // **前** に安定 key を退避して `after_undo_redo` で貼り直す。
        let key = self.audio_editor_target_key();
        if !self.song_doc.undo() {
            return;
        }
        self.after_undo_redo(key);
    }

    pub(crate) fn redo(&mut self) {
        let key = self.audio_editor_target_key();
        if !self.song_doc.redo() {
            return;
        }
        self.after_undo_redo(key);
    }

    /// r.md #29: 履歴リストの行 click → `index` 番目の state へ一気に遡る /
    /// 進む。 undo/redo を必要段数ぶん繰り返すのと等価だが、 reconcile
    /// (`after_undo_redo`) は最終 state に対して 1 度だけ走らせる。
    pub(crate) fn jump_history_to(&mut self, index: usize) {
        let key = self.audio_editor_target_key();
        if !self.song_doc.jump_to(index) {
            return;
        }
        self.after_undo_redo(key);
    }

    /// プロジェクト非依存の UI 設定 (resource monitor on/off・編集履歴 window の
    /// 開閉/位置/サイズ) を app_config.json に永続化する。 `ui_prefs` が SSoT なので
    /// 保存対象を増やしても呼び出し側は本メソッド 1 つで済む (r.md #3 / #29)。
    /// r.md #48: 設定画面に出すテーマ一覧を取り直す (`themes/` の read_dir + JSON パース)。
    /// 設定 window を開いたときだけ呼ぶ — 描画ループから呼ぶとフレームごとにディスクを叩く。
    pub(crate) fn refresh_available_themes(&mut self) {
        let dirs = self.ui_prefs.app_dirs.as_ref().map(common::app_dirs::AppDirs::themes_dir);
        self.ui_ephemeral.available_themes = crate::theme::available_themes(dirs.as_deref());
    }

    pub(crate) fn persist_app_config(&self) {
        let Some(dirs) = &self.ui_prefs.app_dirs else {
            return;
        };
        // 組み立ては `AppConfig` の定義の隣 (= app_config.rs)。網羅 literal なので
        // field を 1 つ足すたびにここが太る (サイズ budget)、かつ「保存する / しない」の
        // 判断は AppConfig 側の関心。
        let cfg = crate::app_config::AppConfig::from_prefs(&self.ui_prefs, self.theme.id.clone());
        if let Err(e) = crate::app_config::save(dirs.app_config(), &cfg) {
            tracing::warn!(error = ?e, "failed to save app_config");
        }
    }

    /// `audio_editor_key` は song を差し替える **前** に退避した安定 `ClipKey`
    /// (`AppData::audio_editor_target_key`)。 audio editor の対象は positional な
    /// `ClipKey` なので、 これで貼り直さないと index が詰まって別クリップを指す。
    pub(crate) fn after_undo_redo(&mut self, audio_editor_key: Option<common::model::ClipKey>) {
        // (epoch bump / gesture chain 切断は SongDoc::undo/redo が実施済み。)
        // selected_clip が undo 後も存在するなら維持、消えていれば None。
        // (常に None にすると undo のたびにピアノロールがプレースホルダに戻ってしまう)
        // stable ClipKey 保持なので並べ替え / undo を跨いでも追従する。 clip が
        // 削除されて解決できない key のみ落とす。
        if let Some(k) = self.selection.selected_clip
            && self.clip_at(k).is_none()
        {
            self.selection.selected_clip = None;
        }
        let mut keys = std::mem::take(&mut self.selection.selected_clips);
        keys.retain(|k| self.clip_at(*k).is_some());
        self.selection.selected_clips = keys;
        // note の index は undo で容易にずれるため、安全側で clear する。
        self.selection.selected_notes.clear();
        // audio event の選択 index も同様に undo でずれるため clear。
        self.selection.audio_editor_selected_events.clear();
        // (review) automation point 選択 (`point_idx` positional) と inline 編集中
        // point も undo でずれるため clear (notes / audio events と同じ扱い)。
        self.selection.selected_automation_points.clear();
        self.ui_ephemeral.editing_automation_point = None;
        // audio_editor_clip は index ベース ClipRef。 undo で track/clip が詰まると
        // 別 clip を編集してしまうので、 **安定 key で貼り直す** (解決不能 / 非 audio
        // なら閉じる)。 旧実装は「その index に audio clip が居るか」 しか見ておらず、
        // 詰まった先に別の audio clip が居るケースを取りこぼしていた
        // (track 削除経路と共通のガード = `reanchor_audio_editor`)。
        self.reanchor_audio_editor(audio_editor_key);
        self.ui_ephemeral.track_rename_id = None;
        self.ui_ephemeral.track_rename_text.clear();
        self.ui_ephemeral.section_rename_id = None;
        self.ui_ephemeral.section_rename_text.clear();
        // 削除/undo で消えた section id を選択から除外。
        self.selection.selected_section_ids
            .retain(|id| self.song_doc.song().sections.iter().any(|s| s.id == *id));
        self.ui_ephemeral.clip_rename = None;
        self.ui_ephemeral.clip_rename_text.clear();
        // selected_track_ids: undo で track が消えていたら除外。 残りが
        // 空なら「最後の track をカーソル」 にフォールバック (UI が
        // 完全選択ゼロでフリーズしないため)。
        let live_ids: std::collections::HashSet<u32> =
            self.song_doc.song().tracks.iter().map(|t| t.id).collect();
        self.selection.selected_track_ids.retain(|id| live_ids.contains(id));
        if self.selection.selected_track_ids.is_empty()
            && let Some(last) = self.song_doc.song().tracks.last()
        {
            let id = last.id;
            self.selection.selected_track_ids.push(id);
            // r.md #43: このフォールバックは **ユーザーの選択ではない** (undo で選択が
            // 全部消えたときに任意の 1 本を当てているだけ)。 last-wins タグが Tracks の
            // まま残ると、 直後の Delete が「触ってもいないトラック」 を消すので降ろす。
            // 削除経路の自動再選択は「削除位置の隣」 = 操作の続きなのでタグを保つが、
            // こちらは位置の連続性が無いので保てない。
            if self.selection.last_edit_select == Some(EditSurface::Tracks) {
                self.selection.last_edit_select = None;
            }
        }
        // collapsed_groups も track が消えていたら除外。
        self.ui_prefs.collapsed_groups.retain(|id| live_ids.contains(id));
        // r.md #71 (プラグインのコピー / 移動): undo/redo で消えた device の id も
        // 落とす (正しさは読む側の `live_device_ids()` が担保する。 これは後始末)。
        self.prune_device_selection();
        self.resize_track_peak_display();
        // Undo / Redo は plugin_host / audio engine の plugin
        // load 状態に直接 IPC を発行しないので、 ここで Song と
        // `loaded_devices` を diff して同期させる。 さもなければ
        // 「Bass track 削除 → Undo で track は復活するが plugin は
        // load されない (= 音が出ない)」 となる。
        //
        // Risk E (plan_undo_reconcile_polish.md): 多段 Undo で reconcile
        // が毎 step 走る cost を測定するための timing log。 plan B 完了で
        // diff は slot 単位の最小 set に絞られているので、 plugin chain
        // が変わらない Undo は HashMap iter のみで終わる。 変わる場合の
        // RemoveSlot/SetSlot IPC コストを観測したい場合は
        // `daw_gui::app::undo_perf=trace` で見る。
        let reconcile_started = std::time::Instant::now();
        self.reconcile_plugins_with_song();
        let reconcile_elapsed = reconcile_started.elapsed();
        tracing::trace!(
            target: "daw_gui::app::undo_perf",
            elapsed_us = reconcile_elapsed.as_micros() as u64,
            "reconcile_plugins_with_song after Undo/Redo"
        );
        self.resync_song_edit_texts();    }

    // -------- File ----------------------------------------------------------

    pub(crate) fn action_new(&mut self) {
        // 別プロジェクト (空) に切り替えるので現プロジェクトの plugin / editor を破棄。
        self.teardown_all_loaded_plugins();
        let mut song = Song::default();
        // New プロジェクトに新しい project_id を採番 (clipboard の
        // 同一プロジェクト判定用、別 New 同士は別プロジェクト扱いになる)。
        song.ensure_project_id();
        Self::migrate_legacy_vocal_tracks(&mut song);
        self.song_doc.replace_song(song);
        self.song_doc.file_path = None;
        self.selection.selected_track_ids.clear();
        self.selection.selected_scene_ids.clear();
        self.ui_prefs.collapsed_groups.clear();
        self.selection.selected_clip = None;
        self.selection.selected_notes.clear();
        // 新規プロジェクトでは前プロジェクトの per-clip view を漏らさずクリア
        // (globals は現状維持 = 従来挙動)。`None` 経路 = action_open_path の旧ファイルと同じ。
        // ループは New で必ず初期化する (前プロジェクトの範囲を持ち越さない)。
        self.restore_view_state(None, common::model::LoopRegion::default());
        self.resize_track_peak_display();
        // sync 前に migrated vocal track の builtin VOICEVOX を SetSlotPlugin
        // で plugin host に load 要求する (= restore_plugin_from_song と同
        // 経路、 起動直後の Song::default のみ self を持つので clone 経由)。
        let song_snapshot = self.song_doc.song().clone();
        self.restore_plugin_from_song(&song_snapshot);
        self.resync_song_edit_texts();
        // 新規プロジェクトを clean (= '*' 無し) で開始し、 旧プロジェクトの
        // Undo/Redo 履歴を破棄する (直前の song 差し替え等で edit_epoch が進むので、
        // ここで saved_epoch を現 epoch に合わせて dirty を打ち消す)。
        self.after_song_replaced();
        tracing::info!("new project");
    }

    pub(crate) fn action_open(&mut self) {
        let dialog = rfd::FileDialog::new().add_filter("daw", &["daw"]);
        self.spawn_file_dialog(
            dialog,
            FileDialogMode::PickFile,
            FileDialogKind::OpenProject,
        );
    }

    /// Phase 6 review fix: project load 直後に `self.song_doc.song().audio_sources` 全件
    /// を WAV decode して `self.media.audio_source_cache` に詰める。 旧コードでは
    /// この path が欠落していて、 saved project を開いた audio clip の波形が
    /// 表示されなかった (= arrangement widget の波形 overlay で
    /// `audio_source_cache.get(event.source_id) → None`)。 import 経由 (=
    /// drag&drop / Open Import Audio) で session 中に追加した source は import_one
    /// が即 decode + cache 投入していたので、 そちらだけ波形が出るという
    /// intermittent な見え方になっていた。
    ///
    /// caller は `self.song_doc.file_path` と `self.song_doc.song()` をセット済の前提。
    /// ProjectRelative は file_path.parent() で resolve、 Generated は廃止
    /// 仕様で skip。 decode 失敗は warn ログのみ (= waveform が出ないだけで
    /// 他機能は動く defensive)。
    /// プロジェクトの audio / image source を **background スレッドで** decode
    /// し、 caches へ逐次取り込む (/ `docs/plan_progress_streaming.md`)。
    /// 旧 `decode_*_sources_into_cache` は GUI スレッドで同期 decode し UI を
    /// 固めていた。 本関数は構造の swap 後に呼ばれ、 work-list を作って 1 本の
    /// thread で順次 decode、 1 件ごとに `AssetDecodeTick` を発火して `on_asset_
    /// decode_tick` が cache へ流し込む (= 波形 / 画像が順次出る streaming load)。
    /// 完了まで `asset_decode` は `Some`。 再生 gate は **audio 件数だけ**を見る
    /// (`AssetDecodeStaging::audio_remaining`)。
    ///
    /// r.md #42: 動画クリップのサムネイルも同じ work-list に載せる。 これは
    /// (a) プロジェクトを開き直したときにサムネイルが出なかった既存の穴、
    /// (b) GPU 再初期化後にテクスチャを作り直す必要、
    /// の **両方が「ディスク上の動画からサムネイルを再生成する」 同一処理**なので、
    /// 経路を 2 つ持たない (「サムネイルはいつ出るのか」 が一意に決まる)。
    ///
    /// `label` は進捗 overlay の文言 (プロジェクト読込 / GPU 復旧 で変える)。
    ///
    /// 走行中の decode があっても **常に新しい staging へ差し替える**。
    /// `on_asset_decode_tick` は `media.asset_decode` が指す **現行の** staging しか
    /// 読まないので、 旧スレッドの成果は孤児 Arc に溜まってそのまま捨てられる
    /// (= 別 project を開いた直後に旧 project の decode 結果が混入しない)。
    /// 未完了だった分は新しい work-list に再び載る (未 cache / 未 staging が条件なので)。
    pub(crate) fn begin_asset_decode(&mut self, label: &'static str) {
        use common::model::{AudioSourcePath, ImageSourcePath};
        // file_path = None (= 未保存 project の sidecar 復元) の場合、
        // ProjectRelative は resolve できないので skip。
        let project_dir: Option<PathBuf> = self
            .song_doc.file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));

        // 未 cache の audio source だけ work-list に (idempotent)。
        // `AudioSourceId` は Song スコープの名前で project ごとに 1 から再採番
        // されるので、**id 一致だけで cache hit と見なしてはいけない** — 別
        // project を開くと前 project の波形がそのまま残り、しかも decode job も
        // 積まれないので恒久的に直らない。cache された buffer 自身が持つ
        // `origin` (decode 元の解決済み絶対パス) が一致したときだけ再利用し、
        // 食い違う entry はここで捨てて decode し直す。
        let mut audio_jobs: Vec<(common::model::AudioSourceId, PathBuf)> = Vec::new();
        let mut stale_audio: Vec<common::model::AudioSourceId> = Vec::new();
        for (&source_id, source) in &self.song_doc.song().media.audio_sources {
            let abs = match &source.path {
                AudioSourcePath::Absolute(abs) => abs.clone(),
                AudioSourcePath::ProjectRelative(rel) => match project_dir.as_ref() {
                    Some(dir) => dir.join(rel),
                    None => continue,
                },
                // PR-V4 で廃止 (builtin VOICEVOX plugin 経由)。
                AudioSourcePath::Generated { .. } => continue,
            };
            match self.media.audio_source_cache.get(source_id) {
                Some(buf) if buf.origin == abs => continue,
                Some(_) => stale_audio.push(source_id),
                None => {}
            }
            audio_jobs.push((source_id, abs));
        }
        // 別 project の同 id を掴んだ entry は、decode 完了を待たずに **即座に**
        // 落とす (待つ間に波形描画 / onset 検出が前 project の音を読むため)。
        for source_id in stale_audio {
            self.media.audio_source_cache.remove(source_id);
        }
        // 未 staging の image source。
        let mut image_jobs: Vec<(common::model::ImageSourceId, PathBuf)> = Vec::new();
        for (&source_id, source) in &self.song_doc.song().media.image_sources {
            if self.media.image_source_bgra.contains_key(&source_id) {
                continue;
            }
            let abs = match &source.path {
                ImageSourcePath::Absolute(abs) => abs.clone(),
                ImageSourcePath::ProjectRelative(rel) => match project_dir.as_ref() {
                    Some(dir) => dir.join(rel),
                    None => continue,
                },
            };
            image_jobs.push((source_id, abs));
        }
        // r.md #42: 未 staging の video source サムネイル。 GPU へ上げ済みの分は
        // `video_thumbnail_rgba` から drain 済なので、 プロジェクトを開いた直後 /
        // GPU 再初期化直後は全件が job になる (= どちらも「ディスクの動画から作り直す」)。
        // CPU staging / GPU texture のどちらかに既にあるものだけ skip する冪等な work-list。
        //
        // 動画 decode は libav 依存で Windows 限定 (`decode_video_thumbnail` 参照)。
        // 他プラットフォームでは job を積まない (= total にも数えない)。
        let mut video_jobs: Vec<(common::model::VideoSourceId, PathBuf)> = Vec::new();
        if cfg!(windows) {
            for (&source_id, source) in &self.song_doc.song().media.video_sources {
                if self.media.video_thumbnail_rgba.contains_key(&source_id)
                    || self.ui_ephemeral.video_texture_cache.contains_key(&source_id)
                {
                    continue;
                }
                let abs = match &source.path {
                    common::model::VideoSourcePath::Absolute(abs) => abs.clone(),
                    common::model::VideoSourcePath::ProjectRelative(rel) => {
                        match project_dir.as_ref() {
                            Some(dir) => dir.join(rel),
                            None => continue,
                        }
                    }
                };
                video_jobs.push((source_id, abs));
            }
        }

        let audio_remaining = audio_jobs.len();
        let total = audio_jobs.len() + image_jobs.len() + video_jobs.len();
        if total == 0 {
            // 走行中だった decode の marker も畳む (= その成果は孤児 Arc へ捨てる)。
            // `load_progress` も一緒に消さないと、 進捗 overlay が出たまま残る
            // (例: 重い project を読込中に「新規」 を選んだとき)。
            self.media.asset_decode = None;
            self.media.load_progress = None;
            return;
        }
        let staging = Arc::new(Mutex::new(AssetDecodeStaging {
            total,
            audio_remaining,
            ..Default::default()
        }));
        self.media.asset_decode = Some(Arc::clone(&staging));
        self.media.load_progress = Some((0, total));
        self.media.load_progress_label = label;
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            for (id, abs) in audio_jobs {
                let decoded = crate::import_audio::decode_audio(&abs)
                    .map_err(|e| {
                        tracing::warn!(path = %abs.display(), error = %e, "asset decode (audio) failed");
                    })
                    .ok()
                    .map(std::sync::Arc::new);
                if let Ok(mut g) = staging.lock() {
                    if let Some(buf) = decoded {
                        g.audio.push((id, buf));
                    }
                    g.done += 1;
                    // 成否に関わらず 1 件消化 (失敗した source を待ち続けない)。
                    g.audio_remaining = g.audio_remaining.saturating_sub(1);
                }
                proxy.send(AppEvent::AssetDecodeTick);
            }
            for (id, abs) in image_jobs {
                let decoded = decode_image_to_bgra(&abs);
                if let Ok(mut g) = staging.lock() {
                    if let Some(img) = decoded {
                        g.image.push((id, img));
                    }
                    g.done += 1;
                }
                proxy.send(AppEvent::AssetDecodeTick);
            }
            for (id, abs) in video_jobs {
                let decoded = decode_video_thumbnail(&abs);
                if let Ok(mut g) = staging.lock() {
                    if let Some(t) = decoded {
                        g.video_thumbnail.push((id, t));
                    }
                    g.done += 1;
                }
                proxy.send(AppEvent::AssetDecodeTick);
            }
        });
    }

    /// 未 decode の **audio** source が残っているか (= 再生を gate すべきか)。
    /// 画像 / 動画サムネイルの decode 中は `false` (音は揃っているので再生してよい)。
    pub(crate) fn audio_decode_pending(&self) -> bool {
        self.media
            .asset_decode
            .as_ref()
            .and_then(|s| s.lock().ok().map(|g| g.audio_remaining > 0))
            .unwrap_or(false)
    }

    /// background decode から 1 件 decode 完了するたびに発火。 staging に
    /// 溜まった結果を self caches へ流し込み (= 該当 clip の波形 / 画像が描画
    /// 開始)、 全件完了で gate を外して queue 中の Play を流す。
    pub(crate) fn on_asset_decode_tick(&mut self) {
        let Some(staging) = self.media.asset_decode.clone() else {
            return;
        };
        let (audio, image, video_thumbnail, done, total, audio_remaining) = {
            let Ok(mut g) = staging.lock() else {
                return;
            };
            (
                std::mem::take(&mut g.audio),
                std::mem::take(&mut g.image),
                std::mem::take(&mut g.video_thumbnail),
                g.done,
                g.total,
                g.audio_remaining,
            )
        };
        for (id, buf) in audio {
            self.media.audio_source_cache.insert(id, buf);
        }
        for (id, (w, h, bytes)) in image {
            self.media.image_source_bgra.insert(id, (w, h, bytes));
            self.media.pending_image_uploads.push(id);
        }
        for (id, (w, h, rgba)) in video_thumbnail {
            self.media.video_thumbnail_rgba.insert(id, (w, h, rgba));
            self.media.pending_thumbnail_uploads.push(id);
        }
        // 音が揃った時点で再生 gate を外す (画像 / サムネイルは待たない)。
        if audio_remaining == 0 && self.transport.pending_play {
            self.fire_pending_play();
        }
        if done >= total {
            tracing::info!(total, "asset decode complete");
            self.media.asset_decode = None;
            self.media.load_progress = None;
        } else {
            self.media.load_progress = Some((done, total));
        }
    }

    /// r.md #42: GPU 資産を作り直した直後に呼ぶ。 GPU 上にしか無かった派生データ
    /// (動画サムネイル / 画像テクスチャ) を **ディスク上の元ファイルから**再構築する。
    ///
    /// SSoT はディスク上のファイルであって GPU テクスチャではない。 復元のためだけに
    /// 展開済みビットマップを RAM に常駐させる (4K 画像 1 枚 ~33MB × 枚数) 案は採らない
    /// — 復帰直後の一瞬の空白より、 常時のメモリ増のほうが害が大きい。
    pub(crate) fn rebuild_gpu_derived_caches(&mut self) {
        // 旧世代の handle を破棄予約へ。 device ごと作り直された経路では新 store に
        // その id が無いので destroy は no-op (id 空間は単調なので新テクスチャを
        // 巻き添えにしない)。 **片方の renderer だけ生きていた** 場合はここで実際に
        // 解放され、 orphan が残らない。
        self.discard_gpu_derived_caches();
        // 「未 staging のものだけ」 を対象にする冪等な work-list 構築なので、
        // GPU へ上げ済みだった (= staging が drain 済みの) ものだけが再 decode される。
        self.begin_asset_decode("GPU を再初期化しています");
    }

    /// main renderer 上の派生テクスチャ (動画サムネイル / 画像) の参照を捨て、
    /// 実体の解放を runner (`drain_texture_destroys`) に予約する。
    fn discard_gpu_derived_caches(&mut self) {
        let destroys = &mut self.ui_ephemeral.pending_texture_destroys;
        destroys.extend(self.ui_ephemeral.video_texture_cache.drain().map(|(_, h)| h));
        destroys.extend(self.ui_ephemeral.image_texture_cache.drain().map(|(_, h)| h));
    }

    pub(crate) fn action_open_path(&mut self, path: PathBuf) {
        // Recursive open を防ぐ: autosave file を直接開いた場合は弾く
        // (RecoveryRestore で開くべきもの)。
        if common::recovery::is_autosave_file(&path) {
            self.ui_ephemeral.status_message = format!(
                "autosave ファイルは Recovery modal から復元してください: {}",
                path.display()
            );
            return;
        }
        match common::project::load_project(&path) {
            Ok(loaded) => {
                let (mut song, view, loop_region) = (loaded.song, loaded.view, loaded.loop_region);
                tracing::info!(path = %path.display(), "loaded project");
                song.ensure_ids();
                Self::migrate_legacy_vocal_tracks(&mut song);
                // 別プロジェクトを開くので、現プロジェクトの plugin と
                // **開いている editor window** を先に全て破棄する。単一チェーン移行で
                // project 切替時の teardown が漏れ、前プロジェクトの editor 窓が残って
                // いた回帰の修正 (load 成功後・新 plugin load 前に実行)。
                self.teardown_all_loaded_plugins();
                self.restore_plugin_from_song(&song);
                self.song_doc.replace_song(song);
                self.song_doc.file_path = Some(path.clone());
                // load した内容を新しい保存ベースラインに確定し、 前プロジェクトの
                // Undo/Redo 履歴と **Song スコープ状態** を破棄する
                // (reset_saved_baseline 内で is_dirty=false)。
                // `begin_asset_decode` / `restore_view_state` より **先** に呼ぶ:
                // 後だと、前 project のキャッシュを見て「decode 済み」と判断した
                // 直後にそのキャッシュが捨てられ、decode job も無いまま波形が
                // 永久に出ない状態になる。
                self.after_song_replaced();
                // audio / image / video サムネイルの decode は重いので background
                // スレッドへ。 構造は既に swap 済みなので即操作可、 波形 / 画像 /
                // サムネイルは streaming で順次出る (begin_asset_decode →
                // AssetDecodeTick)。
                self.begin_asset_decode("プロジェクトを読込中");
                // 保存済みの表示状態 (ズーム / スクロール / per-clip view / 選択
                // クリップ) を復元。`None` (旧ファイル / view 未保存) なら per-clip map をクリア
                // するだけで globals は現状維持 = 従来の fit-to-content 挙動。
                self.restore_view_state(view, loop_region);
                // 復元した選択クリップのトラックを追従選択 (= select_clip と同じ文脈復元)。
                if let Some(r) = self.selected_clip_ref() {
                    self.select_track(r.track_id);
                }
                self.resize_track_peak_display();
                self.resync_song_edit_texts();
                // sidecar 検出: 前回のセッションが正常終了せず、 同 file の
                // autosave が残っているなら recovery modal に追加。 ユーザーが
                // 「復元」 で sidecar に切り替えられる。
                let sidecar = common::recovery::sidecar_for(&path);
                if sidecar.exists() && !self.ui_ephemeral.recovery_candidates.contains(&sidecar) {
                    // sidecar が .daw より新しいときだけ復元候補に出す。 古い
                    // (= 保存後の消し損ね / unclean exit 残骸) は stale なので
                    // 提示せず掃除する (delete-on-save の取りこぼし救済)。
                    if Self::recovery_sidecar_is_newer(&sidecar, &path) {
                        tracing::info!(
                            sidecar = %sidecar.display(),
                            "sidecar autosave detected on open (newer than saved file)"
                        );
                        self.ui_ephemeral.recovery_candidates.push(sidecar);
                        self.ui_ephemeral.show_recovery_modal = true;
                    } else {
                        tracing::info!(
                            sidecar = %sidecar.display(),
                            "stale sidecar autosave (not newer than saved file); removing"
                        );
                        let _ = std::fs::remove_file(&sidecar);
                    }
                }
                self.push_recent(path);
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to load project");
                self.ui_ephemeral.status_message = format!("Open 失敗: {e:#}");
            }
        }
    }

    /// File メニュー「Open Recent」 / 「Recently Saved」 用の display label
    /// を再計算。 `RecentFiles.paths` (= PathBuf 列) から basename を抽出
    /// した String 列を返す。 lifetime 都合上、 menu widget の `&'a str`
    /// label として渡すために AppData field に持つ必要があるので、 push 時に
    /// 呼ぶ。 起動時 (= `new`) では `init_recent_labels` の方で 1 回呼ぶ。
    pub(crate) fn rebuild_recent_labels(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| p.display().to_string())
            })
            .collect()
    }

    /// 起動直後 / load 直後に label cache を 1 度更新する helper。
    /// `AppData::new` が `recent_files: load_recent_files()` で paths を
    /// 復元するため、 同時に label cache も復元したい。 caller (= bootstrap)
    /// が `app.init_recent_labels()` を 1 回呼ぶ。
    pub fn init_recent_labels(&mut self) {
        self.ui_prefs.recent_files_labels =
            Self::rebuild_recent_labels(&self.ui_prefs.recent_files.paths);
        self.ui_prefs.recent_saved_labels =
            Self::rebuild_recent_labels(&self.ui_prefs.recent_saved.paths);
    }

    /// 「最近開いたファイル」 履歴に追加。 `recent.json` に永続化。
    pub(crate) fn push_recent(&mut self, path: PathBuf) {
        self.ui_prefs.recent_files.push(path);
        self.ui_prefs.recent_files_labels =
            Self::rebuild_recent_labels(&self.ui_prefs.recent_files.paths);
        if let Some(disk) = self.ui_prefs.app_dirs.as_ref().map(|d| d.recent())
            && let Err(e) = crate::recent::save(&disk, &self.ui_prefs.recent_files)
        {
            tracing::warn!(
                error = ?e,
                path = %disk.display(),
                "failed to persist recent files"
            );
        }
    }

    /// 「最近保存したファイル」 履歴に追加。 `recent_saved.json` に永続化。
    /// 開いた履歴 (`recent_files`) と完全に独立。 Save / Save As の両 path
    /// で実 file 書き込み成功後に呼ぶ。
    pub(crate) fn push_recent_saved(&mut self, path: PathBuf) {
        self.ui_prefs.recent_saved.push(path);
        self.ui_prefs.recent_saved_labels =
            Self::rebuild_recent_labels(&self.ui_prefs.recent_saved.paths);
        if let Some(disk) = self.ui_prefs.app_dirs.as_ref().map(|d| d.recent_saved())
            && let Err(e) = crate::recent::save(&disk, &self.ui_prefs.recent_saved)
        {
            tracing::warn!(
                error = ?e,
                path = %disk.display(),
                "failed to persist recent saved files"
            );
        }
    }

    pub(crate) fn maybe_autosave(&mut self) {
        if !self.song_doc.is_dirty() {
            return;
        }
        if self.song_doc.last_autosave.elapsed() < std::time::Duration::from_secs(60) {
            return;
        }

        // 保存先決定: file_path Some なら sidecar、 None なら recovery_dir。
        let autosave_path = match self.song_doc.file_path.as_ref() {
            Some(orig) => common::recovery::sidecar_for(orig),
            None => {
                let Some(dir) =
                    self.ui_prefs.app_dirs.as_ref().map(|d| d.recovery_dir())
                else {
                    // 永続化先未設定 (= test 等)。 未保存 project の autosave は skip。
                    return;
                };
                if let Err(e) = common::recovery::ensure_recovery_dir(&dir) {
                    tracing::warn!(error = ?e, "failed to create recovery dir");
                    return;
                }
                common::recovery::recovery_path_for_session(
                    &dir,
                    &self.song_doc.recovery_session_id,
                )
            }
        };

        // autosave も表示状態を同梱する (= ダーティでなくても view が
        // 永続化される → スクロール/ズーム変更が `*` を立てずに次回 open で復元される)。
        let view = self.snapshot_view_state();
        match common::project::save_project(&autosave_path, self.song_doc.song(), Some(&view)) {
            Ok(()) => {
                tracing::info!(path = %autosave_path.display(), "autosaved");
                self.song_doc.last_autosave = std::time::Instant::now();
            }
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    path = %autosave_path.display(),
                    "autosave failed"
                );
            }
        }
    }

    /// 手動保存成功後に、 この project に紐づく autosave を削除する。
    /// `maybe_autosave` が書く 2 箇所 (sidecar / session recovery file) を両方
    /// 消し、 `recovery_candidates` からも除く。 これで save 直後に unclean
    /// exit (クラッシュ / 強制終了) しても、 次回起動の recovery modal に
    /// 「save より古い」 候補が出ず、 保存内容を巻き戻すリスクを断つ。
    pub(crate) fn clear_stale_autosave_after_save(&mut self, saved_path: &Path) {
        let mut stale: Vec<PathBuf> = vec![common::recovery::sidecar_for(saved_path)];
        if let Some(dir) = self.ui_prefs.app_dirs.as_ref().map(|d| d.recovery_dir()) {
            stale.push(common::recovery::recovery_path_for_session(
                &dir,
                &self.song_doc.recovery_session_id,
            ));
        }
        for p in stale {
            if p.exists() {
                match std::fs::remove_file(&p) {
                    Ok(()) => {
                        tracing::info!(path = %p.display(), "removed stale autosave after save")
                    }
                    Err(e) => tracing::warn!(
                        error = ?e,
                        path = %p.display(),
                        "failed to remove stale autosave after save"
                    ),
                }
            }
            self.ui_ephemeral.recovery_candidates.retain(|c| c != &p);
        }
        // 次の autosave までの 60s タイマーを reset (= save 直後に即書き戻さない)。
        self.song_doc.last_autosave = std::time::Instant::now();
    }

    /// ダーティーガードで「保存せず続行/終了」 (discard) を選んだとき、
    /// 破棄する **現プロジェクト** の autosave を消す。 `maybe_autosave` が書く 2 箇所
    /// (file_path Some なら sidecar、 加えて session recovery file) を両方消し、
    /// `recovery_candidates` からも除く。 これをしないと、 同じ file を開き直したとき
    /// (`action_open_path` の sidecar 検出) や次回起動時の recovery scan で、
    /// 「破棄したはずの未保存変更を復元しますか？」 という矛盾した modal が出る。
    /// `clear_stale_autosave_after_save` の discard 版 (save 成功でなく明示破棄が trigger、
    /// untitled = file_path None も session file だけ掃除する)。
    pub(crate) fn discard_current_autosave(&mut self) {
        let mut stale: Vec<PathBuf> = Vec::new();
        if let Some(orig) = self.song_doc.file_path.as_ref() {
            stale.push(common::recovery::sidecar_for(orig));
        }
        if let Some(dir) = self.ui_prefs.app_dirs.as_ref().map(|d| d.recovery_dir()) {
            stale.push(common::recovery::recovery_path_for_session(
                &dir,
                &self.song_doc.recovery_session_id,
            ));
        }
        for p in stale {
            if p.exists() {
                match std::fs::remove_file(&p) {
                    Ok(()) => tracing::info!(
                        path = %p.display(),
                        "removed autosave of discarded project"
                    ),
                    Err(e) => tracing::warn!(
                        error = ?e,
                        path = %p.display(),
                        "failed to remove autosave on discard"
                    ),
                }
            }
            self.ui_ephemeral.recovery_candidates.retain(|c| c != &p);
        }
    }

    /// sidecar autosave が元 `.daw` より新しい (= 前回 unclean exit 時の未保存
    /// 変更を表す) かを mtime で判定する。 どちらかの mtime が取れない場合は
    /// 安全側に倒して `true` (= 候補に出して user 判断に委ねる) を返す。
    pub(crate) fn recovery_sidecar_is_newer(sidecar: &Path, daw: &Path) -> bool {
        let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        match (mtime(sidecar), mtime(daw)) {
            (Some(s), Some(d)) => s > d,
            _ => true,
        }
    }

    /// Recovery modal で「復元」 を押した処理。 sidecar 形式 (`<x>.daw.autosave.daw`)
    /// なら元 `<x>.daw` を file_path にセット、 recovery_dir 内 (`<uuid>.autosave.daw`)
    /// なら file_path = None (新規プロジェクト扱い、 ユーザーが Save As)。
    pub(crate) fn restore_recovery(&mut self, autosave_path: PathBuf) {
        let Ok(loaded) = common::project::load_project(&autosave_path) else {
            tracing::error!(
                path = %autosave_path.display(),
                "failed to load recovery file"
            );
            self.ui_ephemeral.status_message =
                format!("復元失敗: {}", autosave_path.display());
            return;
        };
        let (mut song, view, loop_region) = (loaded.song, loaded.view, loaded.loop_region);
        song.ensure_ids();
        // 別プロジェクトへの丸ごと差し替えなので、現プロジェクトの plugin と
        // 開いている editor window を先に全て破棄する (action_open_path /
        // action_new と同じ teardown。 これが無いと「plugin 入り project を開いた
        // 直後の復元」 で旧 plugin 実体・editor 窓・GUI cache が残る)。
        self.teardown_all_loaded_plugins();
        self.restore_plugin_from_song(&song);
        self.song_doc.replace_song(song);
        self.song_doc.file_path = common::recovery::original_file_for_sidecar(&autosave_path);
        // 復元した内容を新しい保存ベースラインに確定し、 履歴と Song スコープ
        // 状態を破棄する (action_open_path と同じく decode / view 復元より先)。
        // sidecar は元 project と同じ `project_id` を持つので、同一 project の
        // 復元ではキャッシュは温存される。
        self.after_song_replaced();
        // recovery 復元も load path と同じく background streaming
        // decode へ。 file_path を先にセット済みなので ProjectRelative も解決可。
        self.begin_asset_decode("プロジェクトを読込中");
        // recovery も表示状態 + 選択クリップを復元 (autosave が view を書いている)。
        self.restore_view_state(view, loop_region);
        if let Some(r) = self.selected_clip_ref() {
            self.select_track(r.track_id);
        }
        self.resize_track_peak_display();
        self.resync_song_edit_texts();
        let _ = std::fs::remove_file(&autosave_path);
        self.ui_ephemeral.recovery_candidates.retain(|p| p != &autosave_path);
        if self.ui_ephemeral.recovery_candidates.is_empty() {
            self.ui_ephemeral.show_recovery_modal = false;
        }
        tracing::info!(
            recovered_to = ?self.song_doc.file_path,
            "recovery restored"
        );
    }

    /// Recovery modal で「破棄」 を押した処理。 file 削除 + candidates から外す。
    pub(crate) fn discard_recovery(&mut self, autosave_path: PathBuf) {
        if let Err(e) = std::fs::remove_file(&autosave_path) {
            tracing::warn!(
                error = ?e,
                path = %autosave_path.display(),
                "failed to remove recovery file"
            );
        }
        self.ui_ephemeral.recovery_candidates.retain(|p| p != &autosave_path);
        if self.ui_ephemeral.recovery_candidates.is_empty() {
            self.ui_ephemeral.show_recovery_modal = false;
        }
    }

    /// アプリ正常終了時 (`WindowEvent::CloseRequested`) に呼ぶ cleanup。
    /// 自セッションで作った recovery file (sidecar / recovery_dir 両方) を削除。
    /// recovery file が無ければ no-op。 削除失敗は warn でログのみ。
    pub fn on_shutdown(&self) {
        // 自セッションの recovery_dir file
        if let Some(dir) = self.ui_prefs.app_dirs.as_ref().map(|d| d.recovery_dir()) {
            let p = common::recovery::recovery_path_for_session(
                &dir,
                &self.song_doc.recovery_session_id,
            );
            if p.exists()
                && let Err(e) = std::fs::remove_file(&p)
            {
                tracing::warn!(
                    error = ?e,
                    path = %p.display(),
                    "failed to remove recovery file on shutdown"
                );
            }
        }
        // sidecar (file_path が Some なら)
        if let Some(orig) = self.song_doc.file_path.as_ref() {
            let side = common::recovery::sidecar_for(orig);
            if side.exists()
                && let Err(e) = std::fs::remove_file(&side)
            {
                tracing::warn!(
                    error = ?e,
                    path = %side.display(),
                    "failed to remove sidecar on shutdown"
                );
            }
        }
    }

    /// Undo / Redo 後に呼んで、 `Song.tracks` と plugin_host の load
    /// 状態を **slot 粒度で** diff し、 必要な IPC を発行して両者を
    /// 再同期する。
    ///
    /// Undo / Redo は `Song` の clone 入れ替えだけ行うので、 plugin_host
    /// と audio engine 側の load 状態は元に戻らない。 そのまま放置すると
    /// 「track 削除 → Undo で track 復活 → plugin が host に load されて
    /// いないので音が鳴らない」「FX 1 個追加 → Undo でも host にその FX
    /// が残り続ける」 等の UX バグになる。
    ///
    /// r.md #71 (プラグインのコピー / 移動): diff は **device 粒度の 1 段**。
    /// 旧 Phase A (「host にあるが Song に無い track」 を `RemoveTrack` で消す)
    /// は撤去した — track という単位は host 側に無く、 「Song に無い device」 の
    /// 判定に完全に吸収される。
    ///
    /// [`AppData::loaded_devices`] と Song の device 集合を突き合わせ、
    /// host にあるが Song に無い device は `RemoveSlotPlugin`、 Song にあるが
    /// host に無い / `plugin_id_str` が違う device は `SetSlotPlugin`。
    /// plugin_host の SetSlotPlugin handler は同 device に同 plugin_id を置く
    /// dedup logic を持つので、 一致 device に改めて送信しても no-op
    /// (`SlotPluginLoaded` を再 emit するだけ)。
    ///
    /// plugin の **state** は `Song.PluginInstance::state` を
    /// `initial_state` として渡す。 直前 commit で push_undo_snapshot 前に
    /// `RequestAllStates` で最新 state を Song に書き戻しているので、
    /// 削除直前の knob 値も Undo で復元される。
    pub(crate) fn reconcile_plugins_with_song(&mut self) {
        if self.ipc.plugin_db.is_none() {
            // plugin DB が未ロードなら SetSlotPlugin の組み立て不可。
            // RemoveSlotPlugin 単体は db 不要だが、 まとめて skip する
            // (= db ロード待ち)。
            if !self.song_doc.song().tracks.is_empty() {
                tracing::warn!("reconcile: plugin database not loaded; skipped");
            }
            return;
        }
        let actions =
            compute_slot_reconcile_actions(self.song_doc.song(), &self.ipc.loaded_devices);
        for action in actions {
            match action {
                SlotReconcileAction::RemoveDevice { device_id } => {
                    tracing::info!(device_id, "reconcile: removing extra host device");
                    // close the editor before removing (see
                    // remove_devices_inner for the ordering rationale).
                    self.cleanup_slot_gui(device_id);
                    // **`ClosePluginShmem` を `RemoveSlotPlugin` より先に送る**
                    // (順序は死守。 audio worker が unmapped shmem を踏むと
                    // silent terminate → `all_done` 永久 wait。 理由は
                    // `handler/grouping.rs` の `plan_track_removal_ipc` doc)。
                    // 旧 Phase A が track 単位でまとめて送っていた責務を、
                    // device 単位でここが引き取る。
                    self.send_audio(AudioCommand::ClosePluginShmem { device_id });
                    self.send_plugin(PluginCommand::RemoveSlotPlugin { device_id });
                    self.ipc.loaded_devices.remove(&device_id);
                    self.ipc.pending_plugin_loads.remove(&device_id);
                }
                SlotReconcileAction::LoadDevice {
                    device_id,
                    plugin_id_str,
                    initial_state,
                } => {
                    tracing::info!(
                        device_id,
                        plugin_id = %plugin_id_str,
                        "reconcile: loading device from song"
                    );
                    self.send_set_slot_plugin(device_id, &plugin_id_str, initial_state);
                }
            }
        }
    }

    /// PR-V3 後段: 旧 project file を読み込んだとき、 `track.source =
    /// Vocal` で `track.instrument` が空の track を「builtin VOICEVOX が
    /// instrument に load された状態」 に書き換える。 caller (= action_
    /// open_path / action_new) は本関数で `&mut song` を migrate してから
    /// `restore_plugin_from_song` に渡す → 通常の plugin restore と同じ
    /// 経路で daw_plugin_host 側に SetSlotPlugin が飛ぶ。
    ///
    /// 既に instrument が居る vocal track (= 既に PR-V3 前段で auto-load
    /// 済 or 手動で plugin を入れた) は touch しない。 idempotent。
    pub(crate) fn migrate_legacy_vocal_tracks(song: &mut Song) {
        // v29: 追加 device に Song allocator で安定 id を振るため、 tracks を
        // index で回す (`&mut song.tracks` の iter 中は `song.alloc_device_id`
        // の全体借用が取れない)。
        for ti in 0..song.tracks.len() {
            // 単一デバイスチェーン: 旧 `instrument.is_none()` は「チェーンに音源
            // が無い」と等価。音源 = MIDI から audio を生む device (note_in +
            // audio_out) を 1 つも持たないなら legacy vocal とみなす (役割判定はせず
            // port を直接見る)。
            let has_sound_source = song.tracks[ti]
                .devices
                .iter()
                .any(|p| p.ports.has_note_input && p.ports.has_audio_output);
            let is_legacy_vocal = matches!(
                song.tracks[ti].source,
                common::model::InstrumentSource::Vocal
            ) && !has_sound_source;
            if !is_legacy_vocal {
                continue;
            }
            let device_id = song.alloc_device_id();
            let track = &mut song.tracks[ti];
            // builtin VOICEVOX は純粋音源 (note_in + audio_out)。チェーン末尾に
            // 追加する (位置で音源として導出される)。
            track.devices.push(common::model::PluginInstance {
                id: device_id,
                ..common::model::PluginInstance::with_ports(
                    common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
                    PluginFormat::Builtin,
                    common::port_config::PortConfig {
                        has_note_input: true,
                        has_note_output: false,
                        has_audio_output: true,
                        // 音源 (audio を生成、加工はしない) なので audio 入力なし。
                        has_audio_input: false,
                        has_video_input: false,
                        has_video_output: false,
                    },
                )
            });
            tracing::info!(
                track_id = track.id,
                track_name = %track.name,
                "PR-V3: legacy vocal track migrated to builtin VOICEVOX"
            );
        }
    }

    /// 現在 host に load されている全 plugin を破棄する (project 切替時)。
    /// audio へ `ClosePluginShmem` を先送りしてから plugin_host へ
    /// `UnloadAllPlugins` を送る (use-after-free deadlock 防止の順序)。
    /// 最後に GUI 側 cache を全消去。
    pub(crate) fn teardown_all_loaded_plugins(&mut self) {
        // 列挙元は **在庫 (`loaded_devices`) と Song の和集合**。 片方だけだと
        // 「load 応答待ちの device」 (帳簿に居ない) か 「Song から消えたが host に
        // 残っている device」 (Song に居ない) のどちらかを取りこぼす。
        let mut ids: std::collections::HashSet<u64> =
            self.ipc.loaded_devices.keys().copied().collect();
        for t in &self.song_doc.song().tracks {
            ids.extend(t.devices.iter().map(|d| d.id));
        }
        ids.extend(self.song_doc.song().master_fx_chain.iter().map(|d| d.id));
        for device_id in ids {
            self.send_audio(AudioCommand::ClosePluginShmem { device_id });
        }
        // project 切替。`device_id` は Song スコープの名前なので、 前 project の
        // instance を「列挙して消す」ことが原理的にできない (新 Song は旧 id を
        // 知らず、旧 Song はもう無い)。 帳簿にも Song にも依存しない
        // 「全部捨てろ」でしか塞げない (protocol.rs の UnloadAllPlugins doc 参照)。
        self.send_plugin(PluginCommand::UnloadAllPlugins);
        // 計測 slot も device_id で引くので同じく project スコープ。解放は
        // これまでリソースモニタを描画しているフレームでしか走らず、モニタを
        // 開かずに project を開き続けると 512 slot が stale で埋まって以後
        // どの plugin も 0 μs になっていた。instance を全部落とした直後の
        // ここが、live 集合が空だと確実に言える唯一の地点。
        if let Some(bridge) = self.ipc.metrics_bridge.as_ref() {
            bridge.reclaim_plugin_metric_slots(&std::collections::HashSet::new());
        }
        self.ipc.loaded_devices.clear();
        self.ipc.open_plugin_guis.clear();
        self.ipc.plugin_params.clear();
        self.ipc.slot_has_gui.clear();
        self.ipc.plugin_param_values.clear();
        self.ipc.pending_plugin_loads.clear();
        self.ipc.pending_added_plugin_finalize.clear();
        self.ipc.gui_open_requests.clear();
        // 「未ロード」 表示も project スコープ (前 project の device_id を
        // 次 project が再利用するので、 残すと無関係な device が失敗表示になる)。
        self.ipc.failed_plugin_loads.clear();
    }

    /// plugin_host に `SetSlotPlugin` を送る唯一の口 (script mode の生 API を除く)。
    /// plugin DB で `plugin_id` → (format, path) を解決し、 要求世代を採番して
    /// `pending_plugin_loads` に積む。 送れたら `true`。
    ///
    /// project 復元 / paste 復元 / Undo reconcile / インスペクタの再読込 が
    /// 同じ組み立てを 4 箇所で重複させていたのを 1 本化したもの。
    pub(crate) fn send_set_slot_plugin(
        &mut self,
        device_id: u64,
        plugin_id: &str,
        initial_state: Option<Vec<u8>>,
    ) -> bool {
        let Some(db) = self.ipc.plugin_db.clone() else {
            tracing::warn!(%plugin_id, "plugin database not loaded; cannot resolve plugin id");
            return false;
        };
        let Some(entry) = db.find_by_id(plugin_id) else {
            tracing::error!(id = %plugin_id, device_id, "plugin id not in database");
            return false;
        };
        // v29: 安定 device id でアドレスする。 0 (未採番) は ensure_ids 前の
        // song が漏れてきた設計バグなので error に出して skip。
        if device_id == 0 {
            tracing::error!(id = %plugin_id, "device id unallocated; skipping SetSlotPlugin");
            return false;
        }
        let format = entry.format;
        let path = entry.path.clone();
        let resolved_id = entry.id.clone();
        let generation = self.track_pending_load(device_id);
        self.send_plugin(PluginCommand::SetSlotPlugin {
            device_id,
            format,
            path,
            plugin_id: resolved_id,
            initial_state,
            generation,
        });
        true
    }

    /// plugin_host にこの device を実体化させる **唯一の口**。
    /// 内蔵映像 FX (`ports.is_video()`) は plugin_host に載らない device なので
    /// skip し `false` を返す (engine は未登録 device を skip する = 音声素通り)。
    /// project 復元 / paste 復元 / device コピー (r.md #71) が全部ここを通る。
    pub(crate) fn restore_device(&mut self, inst: &common::model::PluginInstance) -> bool {
        if inst.ports.is_video() {
            return false;
        }
        self.send_set_slot_plugin(
            inst.id,
            &inst.plugin_id,
            inst.state.as_deref().map(<[u8]>::to_vec),
        )
    }

    pub(crate) fn restore_plugin_from_song(&mut self, song: &Song) {
        if self.ipc.plugin_db.is_none() {
            tracing::warn!("plugin database not loaded; cannot resolve plugin ids");
            return;
        }
        // v29: 帰属も chain 内の位置も送らない (host は device_id だけでアドレスする)。
        let mut to_send: Vec<common::model::PluginInstance> = Vec::new();
        for track in song.tracks.iter() {
            to_send.extend(track.devices.iter().cloned());
        }
        to_send.extend(song.master_fx_chain.iter().cloned());
        for inst in to_send {
            self.restore_device(&inst);
        }
    }

    /// 指定 track id 群の devices だけを plugin host に `SetSlotPlugin` で
    /// 実体化する (paste したトラックの plugin を state 込みで新インスタンス化)。
    /// [`Self::restore_plugin_from_song`] の track 限定版。`self.song_doc.song()` を読むため
    /// to_send を先に owned で確保してから送る (borrow 回避)。
    pub(crate) fn restore_plugins_for_tracks(&mut self, track_ids: &[u32]) {
        let to_send: Vec<common::model::PluginInstance> = self
            .song_doc
            .song()
            .tracks
            .iter()
            .filter(|t| track_ids.contains(&t.id))
            .flat_map(|t| t.devices.iter().cloned())
            .collect();
        for inst in to_send {
            self.restore_device(&inst);
        }
    }

    pub(crate) fn action_save(&mut self) {
        if let Some(path) = self.song_doc.file_path.clone() {
            self.begin_save(path);
        } else {
            self.action_save_as();
        }
    }

    /// ウィンドウを閉じる要求 (`WindowEvent::CloseRequested` = ✕ / Alt+F4 /
    /// システムメニュー / タスクバー) のエントリ。 r.md #61 で終了経路が
    /// 増えたので、実体は [`AppData::request_quit`] (全経路の合流点)。
    pub fn request_close(&mut self) {
        self.request_quit(crate::shutdown::QuitRequest::USER);
    }

    /// 現在のプロジェクトを破棄する操作 (終了 / New / Open /
    /// Open Recent) のエントリ。 未保存変更があれば確認モーダルを開き、
    /// 無ければ即 `action` を実行する。 ふつうの DAW と同じく「破棄する前に
    /// 保存するか確認」 する。
    pub fn request_guarded_action(&mut self, action: DirtyGuardAction) {
        // 既に終了確定 / 保存後アクション待ち / queue drain 待ち / モーダル表示中
        // なら、 連打で多重に処理しない (= 二重操作の無視 / ユーザーの判断待ち)。
        if self.shutdown.is_shutting_down()
            || self.ui_ephemeral.guard_after_save.is_some()
            || self.ui_ephemeral.guard_pending_action.is_some()
            || self.ui_ephemeral.dirty_guard.is_some()
        {
            return;
        }
        // plugin-state round-trip (Save / Deferred edit / Copy) が in-flight の間は、
        // self.song_doc.song() も dirty 判定も確定していない (Deferred edit は完了時に track を
        // 削除する等)。 確認モーダルを出さず、 破壊操作も走らせず、 queue が drain
        // したら最新状態で **再評価** する (= `on_all_states_from_child` 末尾)。
        // 出してしまうと: ① 保存完了で clean 化した後も「未保存です」 と聞く stale
        // 表示、 ② Deferred edit (track 削除等) 完了前に self.song_doc.song() を差し替えると、
        // pending な編集が別 project に誤適用されデータ破壊、 になる。
        if !self.ipc.pending_state_queue.is_empty() {
            self.ui_ephemeral.guard_pending_action = Some(action);
            return;
        }
        if self.song_doc.is_dirty() {
            self.ui_ephemeral.dirty_guard = Some(action);
        } else {
            self.perform_guard_action(action);
        }
    }

    /// ガード確認を抜けた (= 保存済 / 破棄選択 / clean) あとに、 保留していた
    /// 操作を実際に実行する。
    pub(crate) fn perform_guard_action(&mut self, action: DirtyGuardAction) {
        // データ破壊ガード: New / Open / OpenPath は self.song_doc.song() / file_path を
        // 破壊的に差し替える。 pending_state_queue に未完了 round-trip
        // (Save / Deferred edit / Copy) が残っている間に実行すると、 その完了処理が
        // 「差し替え後の song」 を「差し替え前に捕まえた path / track_id」 で扱い、
        // 別 project を上書き / 別 project の track を削除して破壊する。 queue が
        // drain するまで保留し、 完了ハンドラ (`on_all_states_from_child` 末尾) が
        // queue 空の状態で再評価する。 (Quit は song を触らないので保留不要。)
        if !self.ipc.pending_state_queue.is_empty()
            && matches!(
                action,
                DirtyGuardAction::New | DirtyGuardAction::Open | DirtyGuardAction::OpenPath(_)
            )
        {
            self.ui_ephemeral.guard_pending_action = Some(action);
            return;
        }
        match action {
            // r.md #61: 「終了する」で即 exit するのではなく、子プロセスの
            // graceful teardown を待つシーケンスへ入る。
            DirtyGuardAction::Quit(req) => self.begin_shutdown(req),
            DirtyGuardAction::New => self.action_new(),
            DirtyGuardAction::Open => self.action_open(),
            DirtyGuardAction::OpenPath(path) => self.action_open_path(path),
        }
    }

    /// ガードモーダルで「保存して続行」 を選んだ処理。 save を発行し:
    /// - 同期保存が済んだ (plugin 無し / 既存 path) → 即 `action` を実行
    /// - plugin state 取得待ちで非同期保存が enqueue された → `guard_after_save`
    ///   を立て、 `on_all_states_from_child` の完了で `action` を実行する
    /// - 新規 project で Save As ダイアログが非同期に開いた → `guard_after_save`
    ///   を立て、 dialog 解決後の begin_save 完了 (`SaveAsResolved`) で実行する
    /// - Save As ダイアログをキャンセルした (保存されず pending も無い) →
    ///   何もしない (モーダルは閉じてアプリに留まる)
    pub(crate) fn guard_save(&mut self) {
        let Some(action) = self.ui_ephemeral.dirty_guard.take() else {
            return;
        };
        self.action_save();
        if !self.song_doc.is_dirty() {
            self.perform_guard_action(action);
        } else if self.has_pending_save() {
            self.ui_ephemeral.guard_after_save = Some(action);
        } else if self.ui_ephemeral.save_as_dialog_open {
            // dialog をキャンセルしたら `SaveAsResolved` 側でこの intent を取り消す。
            self.ui_ephemeral.guard_after_save = Some(action);
        }
    }

    /// `pending_state_queue` に未処理の `Save` request が残っているか。
    /// 非同期保存 (plugin state 取得待ち) の in-flight 判定に使う。
    pub(crate) fn has_pending_save(&self) -> bool {
        self.ipc.pending_state_queue
            .iter()
            .any(|r| matches!(r, PendingStateRequest::Save { .. }))
    }

    /// Bitwig / Ableton / Logic 流: project = bundle directory。 UX として
    /// ユーザーは普通の「名前を付けて保存」 dialog でプロジェクト名
    /// (例: `wav03.daw`) を入力する。 daw_01 はその親フォルダ内に
    /// **同名のフォルダを作成** し、 中に project file (`wav03.daw`) と
    /// `samples/` (imported audio copy)、 将来 `bounce/` 等を配置する。
    /// = ユーザー入力 `<parent>/wav03.daw` → 実際の保存先は
    /// `<parent>/wav03/wav03.daw`。 これにより
    /// 「ファイル名だけ選んだら samples/ がどこに作られるか分からない」
    /// 旧挙動と「pick_folder dialog では新規フォルダを作れない」 (Windows
    /// の input 欄問題) を同時に解消する。 仕様書:
    /// `docs/plan_audio_clip.md` §5 / §13 Q2。
    pub(crate) fn action_save_as(&mut self) {
        if self.ui_ephemeral.save_as_dialog_open {
            return;
        }
        self.ui_ephemeral.save_as_dialog_open = true;
        let dialog = rfd::FileDialog::new()
            .add_filter("daw", &["daw"])
            .set_title("プロジェクト名 / 保存先を選択 (フォルダは自動作成されます)");
        // save dialog + 上書き確認 (MessageDialog) を **worker thread** で開く。 GUI
        // スレッドで同期に開くと preview window 等の再描画 flood で modal pump が枯れて
        // フリーズするため (spawn_file_dialog と同じ理由)。 ここは 2 段 dialog + path
        // 導出があるので generic helper ではなく専用 worker。 最終 .daw path を
        // `SaveAsResolved` で返し、 GUI スレッドで create_dir_all + begin_save する。
        #[cfg(windows)]
        let parent_hwnd = self.ui_ephemeral.main_window_hwnd;
        let proxy = self.ipc.event_proxy.clone();
        std::thread::spawn(move || {
            #[cfg(windows)]
            let dialog = match parent_hwnd {
                Some(hwnd) => dialog.set_parent(&Win32Parent { hwnd }),
                None => dialog,
            };
            let resolved = (|| {
                let picked = dialog.save_file()?;
                let stem = picked
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)?;
                let parent = picked
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                let project_dir = parent.join(&stem);
                let path = project_dir.join(format!("{stem}.daw"));
                if path.exists() {
                    let confirm = rfd::MessageDialog::new()
                        .set_title("プロジェクトの上書き確認")
                        .set_description(format!(
                            "{} は既に存在します。 上書きしますか？",
                            path.display()
                        ))
                        .set_buttons(rfd::MessageButtons::YesNo);
                    #[cfg(windows)]
                    let confirm = match parent_hwnd {
                        Some(hwnd) => confirm.set_parent(&Win32Parent { hwnd }),
                        None => confirm,
                    };
                    if confirm.show() != rfd::MessageDialogResult::Yes {
                        return None;
                    }
                }
                Some(path)
            })();
            proxy.send(AppEvent::SaveAsResolved { path: resolved });
        });
    }

    /// Song 内に CLAP/VST3 plugin が 1 つでもあるか。 何も無ければ
    /// `RequestAllStates` を発行する意味が無いので、 deferred / save の
    /// dispatcher は plugin なしを早期判定して即時実行に切り替える。
    pub(crate) fn song_has_plugin(&self) -> bool {
        !self.song_doc.song().master_fx_chain.is_empty()
            || self.song_doc.song().tracks.iter().any(|t| !t.devices.is_empty())
    }

    /// `AllPluginStates` で受け取った各 plugin の state を `Song` の
    /// 対応する `PluginInstance::state` に書き戻す。 save flow と Undo
    /// snapshot deferred path の両方で呼ばれる共通 helper。
    ///
    /// v29: SlotState は安定 `device_id` keyed。 track/master のどの位置に
    /// 居ても id 一致で書き戻すので、 deferred path で並びが変わっていても
    /// 壊れない (`docs/plan_arch_refactor.md` §1)。
    pub(crate) fn apply_plugin_states_to(song: &mut Song, states: &[SlotState]) {
        for s in states {
            // Phase 6 review (silent corruption fix): plugin_host が
            // `state_save()` で `Err` を返したエントリは `error` 付きで
            // 来る。 そのとき `s.data` は None なので、 既存 state を
            // 上書きすると **過去 save 時に保存された state が消える**
            // (= 旧バグ: save 失敗 → 次 save で空 state 確定)。 error あり
            // のエントリは skip して既存 state を保つ。
            if s.error.is_some() {
                tracing::warn!(
                    device_id = s.device_id,
                    error = s.error.as_deref(),
                    "apply_plugin_states: state save errored, preserving previous state",
                );
                continue;
            }
            let device = song
                .tracks
                .iter_mut()
                .flat_map(|t| t.devices.iter_mut())
                .chain(song.master_fx_chain.iter_mut())
                .find(|d| d.id == s.device_id);
            let Some(p) = device else {
                tracing::warn!(device_id = s.device_id, "apply_plugin_states: device id not found");
                continue;
            };
            p.state = s.data.clone().map(std::sync::Arc::from);
            // (r.md #5 ARA2) Only overwrite the ARA archive when the plug-in
            // actually produced one; a non-ARA device or a not-yet-bound
            // session reports None, and we must not wipe a previously-saved
            // archive in that case.
            if s.ara_archive.is_some() {
                p.ara_archive = s.ara_archive.clone().map(std::sync::Arc::from);
            }
        }
    }

    /// `RequestAllStates` 待ちの request を [`AppData::pending_state_queue`]
    /// に積む。 queue が空 (= 現在 in-flight なし) なら同時に
    /// `RequestAllStates` を 1 発送る。 既に in-flight なら積むだけで
    /// IPC は発行しない (= 先行 request の応答処理時に次の `RequestAllStates`
    /// が改めて送られる、 [`AppData::on_all_states_from_child`] 参照)。
    pub(crate) fn enqueue_state_request(&mut self, req: PendingStateRequest) {
        let was_idle = self.ipc.pending_state_queue.is_empty();
        self.ipc.pending_state_queue.push_back(req);
        if was_idle {
            self.dispatch_front_state_request();
        }
    }

    /// queue 先頭 request の state 収集を開始する (= `RequestAllStates` 送信)。
    /// 先頭が **まだ snapshot を持たない `Save`** なら、 送信する **この瞬間** の
    /// live song を凍結して snapshot に充填する。 これで snapshot の plugin slot
    /// 配置と、 host が `RequestAllStates` を処理して返す state の配置が同時刻
    /// サンプリングになる: FIFO IPC により、 この送信より前に出された layout 変更
    /// (先行 Deferred の `RemoveSlotPlugin` 等) は既に host で処理済み・live にも
    /// 反映済みであり、 この送信より後の変更は host では `RequestAllStates` の後に
    /// 処理されるため、 返る state は必ず「今 live にある配置」 と一致する。
    pub(crate) fn dispatch_front_state_request(&mut self) {
        // plugin host が居ない (crash 後 respawn 断念 = crash-loop
        // 上限 / supervisor 無し / respawn 失敗) と RequestAllStates は届かず応答も
        // 永久に来ない。 一方 enqueue gate は接続状態でなく `song_has_plugin()`
        // (model 上 plugin が在るか) なので、 この degraded 状態でも round-trip が
        // 積まれてしまう。 30s watchdog を待たせるのは無駄なので、 host 不在を
        // 検知したら即 round-trip を破棄して脱出する (待っても完了し得ない)。
        if self.ipc.plugin_tx.is_none() {
            tracing::warn!(
                "plugin host unavailable; aborting state round-trip immediately (no host to answer)"
            );
            self.abort_state_roundtrip();
            self.ui_ephemeral.status_message =
                "プラグインホストが応答しないため保存/操作を中止しました（オートセーブは保持されています）"
                    .into();
            return;
        }
        let needs_snapshot = matches!(
            self.ipc.pending_state_queue.front(),
            Some(PendingStateRequest::Save { snapshot: None, .. })
        );
        if needs_snapshot {
            let snap = Box::new(self.song_doc.song().clone());
            let epoch = self.song_doc.edit_epoch();
            if let Some(PendingStateRequest::Save {
                snapshot,
                snap_epoch,
                ..
            }) = self.ipc.pending_state_queue.front_mut()
            {
                *snapshot = Some(snap);
                *snap_epoch = epoch;
            }
        }
        self.send_plugin(PluginCommand::RequestAllStates);
        // この瞬間から応答 (AllStatesReceived) までを on_tick の watchdog
        // が監視する。 host が hang して応答が来ないと永久ロックになるため。
        self.ipc.state_request_sent_at = Some(std::time::Instant::now());
    }

    /// in-flight な plugin-state round-trip を強制的に破棄する。 plugin host が
    /// crash した (`handle_child_disconnected`) / hang して応答が来ない
    /// (`poll_state_roundtrip_watchdog`) / そもそも host が居ない
    /// (`dispatch_front_state_request` の不在検知) ときの共通脱出口。
    ///
    /// stale な `pending_state_queue` をクリアし、 round-trip 完了待ちで保留して
    /// いたダーティーガード操作 (`guard_after_save` / `guard_pending_action`) を
    /// **実行せず破棄** する。 クリアしないと `enqueue_state_request` の `was_idle`
    /// 判定が永久に false のまま以後の保存が一切 dispatch されず、 さらに `guard_*`
    /// が Some のまま `request_guarded_action` が早期 return し続けて
    /// New / Open / Open Recent / 終了(✕) が GUI から不可能になる (= #63/#64 の症状)。
    ///
    /// 保留していた破棄系操作 (New/Open) を **実行しない** のは、 保存が成立して
    /// いない状態で project を差し替えると未保存変更を失う / 別 project を破壊する
    /// ため (autosave があるのでデータ自体は失われない)。
    ///
    /// (r.md #61) ただし **終了 (`Quit`) だけは意図を捨てずに再評価する**。
    /// 旧実装は Quit も黙って捨てていたので、「保存して終了」の途中で
    /// plugin host が死ぬと warn ログ 1 行だけ残して終了意図が消え、
    /// ユーザーからは「✕ が効かなかった」ようにしか見えなかった。かといって
    /// そのまま終了させるのも誤り — 保存が成立していないので未保存変更を失う。
    /// 正しいのは「queue が空になった最新状態でガードをやり直す」ことで、これは
    /// `on_all_states_from_child` 末尾の正常系とまったく同じ扱い。
    pub(crate) fn abort_state_roundtrip(&mut self) {
        self.ipc.pending_state_queue.clear();
        self.ipc.state_request_sent_at = None;
        // 両方とも無条件に take する (`||` の短絡で 2 つ目が消えないように)。
        let after_save = self.ui_ephemeral.guard_after_save.take();
        let pending = self.ui_ephemeral.guard_pending_action.take();
        if after_save.is_none() && pending.is_none() {
            return;
        }
        // 終了意図は片方にしか載らない (両方に載る経路は無い) が、拾い漏らさない
        // よう両方を見る。
        let quit = [after_save, pending]
            .into_iter()
            .flatten()
            .find(|a| matches!(a, DirtyGuardAction::Quit(_)));
        match quit {
            Some(action) => {
                tracing::warn!(
                    "aborted an in-flight plugin-state round-trip while quitting; \
                     re-asking with the current (unsaved) state"
                );
                self.request_guarded_action(action);
            }
            None => tracing::warn!(
                "aborted an in-flight plugin-state round-trip; \
                 dropping the deferred dirty-guard action"
            ),
        }
    }

    /// plugin-state round-trip (`RequestAllStates` → `AllStatesReceived`)
    /// の hang watchdog。 `on_tick` (33ms / ~30Hz の playhead poll、 plugin host
    /// とは独立した daw_audio 由来なので host が hang しても発火し続ける) から毎回
    /// 呼ばれる。 応答が一定時間来なければ round-trip を破棄して脱出口を作る。
    ///
    /// 引数 `now` を取るのは test が経過時間を注入できるようにするため (`Instant` は
    /// 任意時刻を構築できないので `elapsed()` を内部で呼ばず、 渡された `now` との
    /// 差で判定する)。 production は `Instant::now()` を渡す。
    ///
    /// 閾値は export watchdog (60s) より短い。 plugin の `state_save` は通常 1 秒
    /// 未満で、 host main-thread が別の重い操作 (plugin GUI 起動等) で詰まっても
    /// 数秒で済む。 30 秒を超えるのは実質 hang のみ (= 誤発火しない一方、 永久
    /// ロックよりは遥かに短く脱出できる)。
    pub fn poll_state_roundtrip_watchdog(&mut self, now: std::time::Instant) {
        // export 進行中は handle_event の gate (`Tick` のみ
        // whitelist) が `AllStatesReceived` を drop するので、 この間は応答が来ても
        // round-trip は完了し得ない。 deadline を進めると、 export 開始直前に
        // armed だった round-trip を「hang した」と誤判定して、 実際には応答が
        // gate に食われただけの save を中止してしまう。 gate と同条件の間は watchdog を
        // 止め、 export 後 (gate 解除後) に再評価する (応答が来ない真の hang なら、
        // gate 解除後に改めて閾値超過で発火する)。
        if self.transport.export_stage.is_some() || self.transport.pending_video_export.is_some() {
            return;
        }
        const STATE_ROUNDTRIP_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(30);
        let Some(since) = self.ipc.state_request_sent_at else {
            return;
        };
        if now.saturating_duration_since(since) <= STATE_ROUNDTRIP_WATCHDOG {
            return;
        }
        tracing::error!(
            elapsed_s = now.saturating_duration_since(since).as_secs(),
            "plugin-state round-trip stalled past watchdog timeout; aborting (host hang?)"
        );
        self.abort_state_roundtrip();
        self.ui_ephemeral.status_message =
            "プラグインが応答しないため保存/操作を中止しました（オートセーブは保持されています）"
                .into();
    }

    /// project save の trigger。 plugin がある場合は plugin_host から
    /// 最新 state を取って Song に書き戻してから save する。 plugin が
    /// 1 つもなければ即 save。 既に `RequestAllStates` 在線中なら queue
    /// に積んで先行 request の応答後に処理させる (= 順序保持)。
    pub(crate) fn begin_save(&mut self, path: PathBuf) {
        if !self.song_has_plugin() {
            // plugin が無ければ state 収集 (RequestAllStates) は不要。 今の live を
            // そのまま凍結して即 serialize する。 cache migration は finish_save 内で
            // 行う (= live と snapshot の両方に適用、 file_path は成功時のみ確定)。
            let snap_epoch = self.song_doc.edit_epoch();
            let snapshot = Box::new(self.song_doc.song().clone());
            self.finish_save(snapshot, path, snap_epoch);
            return;
        }
        // plugin 有り: snapshot は **state 収集を始める瞬間** に取る (co-temporal)。
        // ここでは None で積み、 dispatch_front_state_request が RequestAllStates を
        // 送るその瞬間に live を凍結する。 こうすると snapshot の plugin slot 配置と、
        // 返ってくる state の配置が一致し、 待機中の slot 削除等による誤適用が消える。
        self.enqueue_state_request(PendingStateRequest::Save {
            path,
            snapshot: None,
            snap_epoch: 0,
        });
    }

    /// plugin state 取得待ちで save が非同期進行中か (= queue に Save あり)。
    /// この間 load_overlay が「保存中…」インジケータを出す
    /// (= 非ブロック、 編集は続行可)。
    pub(crate) fn is_async_save_pending(&self) -> bool {
        self.ipc.pending_state_queue
            .iter()
            .any(|r| matches!(r, PendingStateRequest::Save { .. }))
    }

    /// `song` 内の未保存 import/bounce cache source を `<project_dir>/samples,bounce/`
    /// へ移して path を `ProjectRelative` に書き換える。 save flow で **直列化する
    /// snapshot と working state の live の両方** に適用する: ファイルは move なので、
    /// 片方だけ移すと他方が移動後ファイルを見失う (= 初回呼び出しが move、 2 回目以降は
    /// dst.exists で path 書換のみ)。 失敗しても save は続行し missing source として
    /// 扱う。 status へ最後の失敗メッセージを残す (`&mut status` で借用衝突を避ける)。
    pub(crate) fn migrate_unsaved_sources(song: &mut Song, project_dir: &Path, status: &mut String) {
        // Phase 1 PR3: 未保存 project 中に import した audio source (`docs/plan_audio_clip.md`
        // §13 Q2)。 Phase 2 PR-C: 未保存 project の Bounce 出力 (`docs/plan_audio_followup.md`)。
        if let Err(e) = import_audio::migrate_unsaved_audio_sources_into(song, project_dir) {
            tracing::warn!(error = ?e, "import_cache → samples/ への移行で一部失敗");
            *status = format!("Audio sources の samples/ 移行で一部失敗: {e}");
        }
        if let Err(e) = import_audio::migrate_unsaved_bounce_sources_into(song, project_dir) {
            tracing::warn!(error = ?e, "bounce_cache → bounce/ への移行で一部失敗");
            *status = format!("Audio sources の bounce/ 移行で一部失敗: {e}");
        }
    }

    /// 凍結済み `snapshot` をファイルへ書き出して保存を完了する。
    ///
    /// cache migration は **2 段階**で行い、 破壊的なファイル移動を serialize 成功後に
    /// のみ確定する: (1) serialize 前に snapshot の audio path だけを `ProjectRelative`
    /// へ書き換えて move plan を取る (I/O なし)、 (2) serialize 成功後に plan を commit
    /// (実ファイル move) し、 live も migrate する。 こうすると書き出し失敗時に
    /// import_cache のファイルが無傷で残り、 live は `Absolute(cache)` のまま
    /// autosave/recovery が健全に働く。 **serialize が成功して初めて** file_path を確定し
    /// (旧契約)、 audio engine へ新 project_dir + song を流す。 saved baseline = snapshot、
    /// `is_dirty` は live と snapshot の差で再計算する (state 待ちの間の編集が live に
    /// あれば dirty)。
    pub(crate) fn finish_save(&mut self, mut snapshot: Box<Song>, path: PathBuf, snap_epoch: u64) {
        // serialize する snapshot の path を ProjectRelative に書き換え、 実ファイル
        // 移動の plan を取る (= ここでは I/O しない、 破棄しても無害)。
        let (audio_moves, bounce_moves) = match path.parent() {
            Some(dir) => (
                import_audio::plan_unsaved_audio_migration(&mut snapshot, dir),
                import_audio::plan_unsaved_bounce_migration(&mut snapshot, dir),
            ),
            None => (Vec::new(), Vec::new()),
        };
        // 現在の表示状態を同梱して保存する (snapshot は楽曲のみ凍結、
        // view は presentation なので保存実行時の live を採るので十分)。
        let view = self.snapshot_view_state();
        match common::project::save_project(&path, &snapshot, Some(&view)) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "saved project");
                // serialize 成功 → 破壊的 migration を確定する。 まず snapshot 由来の
                // ファイルを move (plan を commit)、 次に live を migrate して live も
                // ProjectRelative + 自己完結にする (plan 済みファイルは dst.exists で
                // dedup、 live 固有 source があれば move)。
                if let Err(e) = import_audio::commit_migration(&audio_moves) {
                    tracing::warn!(error = ?e, "samples/ への移行確定で一部失敗");
                    self.ui_ephemeral.status_message =
                        format!("Audio sources の samples/ 移行で一部失敗: {e}");
                }
                if let Err(e) = import_audio::commit_migration(&bounce_moves) {
                    tracing::warn!(error = ?e, "bounce/ への移行確定で一部失敗");
                    self.ui_ephemeral.status_message =
                        format!("Audio sources の bounce/ 移行で一部失敗: {e}");
                }
                // round-trip 中に live へ編集が入ったかを epoch 差で先に記録する
                // (下の live migration は「保存完了処理の正規化」 で epoch を進める
                // ため、 記録後に行う)。
                let edited_since_snapshot = self.song_doc.edit_epoch() != snap_epoch;
                if let Some(dir) = path.parent() {
                    let mut status = std::mem::take(&mut self.ui_ephemeral.status_message);
                    self.normalize_song(|song| Self::migrate_unsaved_sources(song, dir, &mut status));
                    self.ui_ephemeral.status_message = status;
                }
                // serialize 成功時のみ file_path を確定する (旧契約)。
                self.song_doc.file_path = Some(path.clone());
                // 保存が現在の live 内容を含む (= round-trip 中の編集なし) なら
                // clean。 編集が入っていれば dirty のまま (下の guard_after_save
                // 再保存 loop が残りを確定する)。 save 後も Undo できるよう履歴は
                // 残す (replace_song は使わない)。
                if !edited_since_snapshot {
                    self.song_doc.mark_saved();
                }
                // 保存成功後、 この project の autosave (sidecar + 未保存→Save As
                // 用の session recovery file) を削除する。 save 後の .daw が
                // authoritative なので、 古い autosave が残ると unclean exit 後の
                // 次回 Open / 起動で recovery modal が「save より古い」 状態を提示し、
                // 復元すると保存内容を巻き戻してしまう。
                self.clear_stale_autosave_after_save(&path);
                // 保存内容が source of truth になったので、 同 file の sidecar
                // autosave (前回までの未保存 snapshot) を削除する。 残すと
                // クラッシュ / 強制終了でクリーン終了処理が走らなかったとき、
                // 次回 Open 時に recovery modal が「save より古い状態」 を復元
                // 候補として提示してしまう (= 保存した作業の巻き戻し事故)。
                let sidecar = common::recovery::sidecar_for(&path);
                match std::fs::remove_file(&sidecar) {
                    Ok(()) => tracing::info!(
                        sidecar = %sidecar.display(),
                        "removed stale sidecar autosave after save"
                    ),
                    // NotFound は正常 (autosave 未作成 / Save As の新規 path)。
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => tracing::warn!(
                        error = ?e,
                        sidecar = %sidecar.display(),
                        "failed to remove sidecar autosave after save"
                    ),
                }
                // この session で既に modal 候補に入っていた場合も除く。
                self.ui_ephemeral.recovery_candidates.retain(|p| p != &sidecar);
                if self.ui_ephemeral.recovery_candidates.is_empty() {
                    self.ui_ephemeral.show_recovery_modal = false;
                }
                // 「最近開いたファイル」 にも入れる (= save した file は次回
                // 開きたい候補なので、 user 期待としては自然)。 さらに
                // 「最近保存したファイル」 別 list にも記録する。
                self.push_recent(path.to_path_buf());
                self.push_recent_saved(path.to_path_buf());
                // PR6: migration (直上の normalize) で audio_sources の path が
                // `Absolute(import_cache)` → `ProjectRelative(samples/)` に書き換わり、
                // project_dir も新たに確定した (file_path は上で path に設定済)。
                // normalize は必ず epoch を bump するので、 ここで flush_song_sync が
                // 最新 live song + project_dir (= file_path.parent()) を audio engine
                // へ届けて `AudioClipRenderer` を rebuild させる (SetProjectDir →
                // LoadSong の順序保証つき)。 epoch bump 済なので no-op にならない。
                self.flush_song_sync();
                // 「保存して続行」: この保存は成功した。 plugin state 待ちの間に live へ
                // 編集が入って dirty なら (co-temporal snapshot は編集前で凍結されている
                // ため、 その編集はこの保存に含まれない)、 残りを確定するため同じ path へ
                // 再保存して保留操作を維持する。 clean なら保留操作 (終了 / New / Open)
                // を実行する。 save 成功が分かるこの場所で判定するので、 失敗時の無限
                // 再保存ループに陥らない。
                if self.ui_ephemeral.guard_after_save.is_some() {
                    if self.song_doc.is_dirty() {
                        self.begin_save(path);
                    } else if let Some(action) = self.ui_ephemeral.guard_after_save.take() {
                        self.perform_guard_action(action);
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to save project");
                self.ui_ephemeral.status_message = format!("保存に失敗しました: {e}");
                // 保存失敗 → 操作を実行しない (データ損失回避)。 保留操作はクリアして、
                // state 待ちのたびに再保存が走り続ける無限ループを防ぐ。
                self.ui_ephemeral.guard_after_save = None;
            }
        }
    }

}
