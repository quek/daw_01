//! handler::tabs — プロジェクトタブ (`docs/plan_project_tabs.md` §5.2)。
//!
//! タブ = [`crate::state::ProjectState`]。見えているタブは `AppData::cur`、他は
//! `AppData::tabs.parked`。ここが担うのは **タブの生死と切替** だけで、Song の中身を
//! 触る処理 (load / teardown / sync) は既存の `handler::project` / `handler::sync` を
//! `with_project` 越しに使う。
//!
//! engine との対応: タブを作る = `AudioCommand::OpenProject`、閉じる =
//! `PluginCommand::UnloadProject` (teardown 経由) + `AudioCommand::CloseProject`、
//! アクティブ切替 = `AudioCommand::SetScopeProject` (マスターメーター / Global Sampler の
//! `Master` が写す project)。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use common::protocol::{AudioCommand, ProjectKey};

use crate::app_types::DirtyGuardAction;
use crate::event_tabs::TabEvent;
use crate::shutdown::QuitRequest;
use crate::state::{AppData, ProjectState};

impl AppData {
    pub(crate) fn handle_tab_event(&mut self, ev: TabEvent) {
        // 保存確認モーダルの最中にタブを切り替えると「どのタブを保存 / 破棄するか」が
        // ずれる (モーダルはアクティブなタブについて出ている)。確認が済むまで受けない。
        if self.ui_ephemeral.dirty_guard.is_some() || self.shutdown.is_shutting_down() {
            return;
        }
        match ev {
            TabEvent::New => {
                self.new_tab();
            }
            TabEvent::Open(path) => self.open_path_in_tab(path),
            TabEvent::Switch(key) => self.switch_tab(key),
            TabEvent::Next => {
                if let Some(k) = self.tabs.neighbor(self.cur.key, true) {
                    self.switch_tab_or_defer_during_drag(k);
                }
            }
            TabEvent::Prev => {
                if let Some(k) = self.tabs.neighbor(self.cur.key, false) {
                    self.switch_tab_or_defer_during_drag(k);
                }
            }
            TabEvent::Close(key) => self.request_close_tabs(vec![key]),
            TabEvent::CloseOthers(key) => {
                let keys: Vec<ProjectKey> =
                    self.tabs.order.iter().copied().filter(|k| *k != key).collect();
                self.request_close_tabs(keys);
            }
            TabEvent::CloseAll => {
                let keys = self.tabs.order.clone();
                self.request_close_tabs(keys);
            }
            TabEvent::Move { key, to } => self.tabs.move_to(key, to),
        }
    }

    // ---- 生成 / 切替 ----------------------------------------------------------

    /// 空の Untitled を新しいタブに開いてアクティブにする。上限 (`MAX_PROJECTS`) なら
    /// `None` (status に理由を出す)。engine には `OpenProject` を送り、Song 本体は次の
    /// frame flush (`flush_all_song_sync`) が `LoadSong` で届ける。
    pub(crate) fn new_tab(&mut self) -> Option<ProjectKey> {
        if !self.tabs.can_open_more() {
            self.ui_ephemeral.status_message = format!(
                "これ以上タブを開けません (最大 {} 個)",
                common::audio_bridge::MAX_PROJECTS
            );
            return None;
        }
        let key = self.tabs.mint();
        self.tabs.parked.push(ProjectState::new_untitled(key));
        self.tabs.order.push(key);
        self.send_audio(AudioCommand::OpenProject { project: key });
        self.switch_tab(key);
        tracing::info!(project = key.0, tabs = self.tabs.len(), "new tab");
        Some(key)
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab (§5.6): クリップ / トラックを運んでいる最中なら、先に
    /// アレンジ widget に payload へ昇格させるため保留する (widget が消費して切り替える)。
    fn switch_tab_or_defer_during_drag(&mut self, key: ProjectKey) {
        if self.cur.peph.arrange_xfer_drag_active {
            self.ui_ephemeral.pending_tab_switch = Some(key);
        } else {
            self.switch_tab(key);
        }
    }

    /// `key` のタブをアクティブにする (`cur` と swap)。閉じたタブ / 既にアクティブなら何もしない。
    pub(crate) fn switch_tab(&mut self, key: ProjectKey) {
        if key == self.cur.key || !self.tabs.swap_in(&mut self.cur, key) {
            return;
        }
        self.on_active_tab_changed();
    }

    /// アクティブなタブが変わった直後の副作用 (engine の scope / poller の重い面 /
    /// preview GPU 状態 / マスターメーター積算 / 旧タブの id を指す一時 UI)。
    fn on_active_tab_changed(&mut self) {
        let key = self.cur.key;
        self.send_audio(AudioCommand::SetScopeProject { project: key });
        self.activity.active_project.store(key.0, Ordering::Release);
        // preview 窓のフレームテクスチャ / decode ring は別 project の映像なので捨てる
        // (`Renderer` を持つ runner に世代印で伝える。main renderer 側のサムネイル /
        // 画像テクスチャはタブごとの cache に残るので触らない)。
        self.ui_ephemeral.project_generation = self.ui_ephemeral.project_generation.wrapping_add(1);
        // 積算ラウドネスは「いま聴いている曲」のもの。別の曲の値を引き継がない。
        self.reset_master_loudness();
        // picker / 範囲ダイアログ等は旧タブの track / clip id を指しているので畳む。
        self.close_transient_ui();
        tracing::info!(project = key.0, "switched tab");
    }

    // ---- 開く --------------------------------------------------------------

    /// `path` のプロジェクトを開く (Q4): アクティブなタブが pristine ならそのタブへ、
    /// そうでなければ新しいタブへ。既に別タブで開いているファイルはそのタブへ切り替える
    /// (同じファイルを 2 つのタブで編集して互いに上書きし合う事故を避ける)。
    pub(crate) fn open_path_in_tab(&mut self, path: PathBuf) {
        if let Some(key) = self.tab_with_path(&path) {
            self.switch_tab(key);
            self.ui_ephemeral.status_message = "既に開いているプロジェクトです".into();
            return;
        }
        // 読めないファイルのために空タブを作らない: 先に読む。
        let Some(loaded) = self.load_project_file(&path) else {
            return;
        };
        if !self.cur_tab_is_pristine() && self.new_tab().is_none() {
            return;
        }
        self.install_loaded_project(path, loaded);
    }

    /// アクティブなタブが「まだ何もしていない Untitled」か (Open が置き換えてよい条件)。
    #[must_use]
    pub(crate) fn cur_tab_is_pristine(&self) -> bool {
        self.cur.song_doc.file_path.is_none()
            && !self.cur.song_doc.is_dirty()
            && !self.cur.song_doc.can_undo()
    }

    /// `path` を開いているタブ (アクティブ含む)。
    #[must_use]
    pub(crate) fn tab_with_path(&self, path: &Path) -> Option<ProjectKey> {
        if self.cur.song_doc.file_path.as_deref() == Some(path) {
            return Some(self.cur.key);
        }
        self.tabs
            .parked
            .iter()
            .find(|p| p.song_doc.file_path.as_deref() == Some(path))
            .map(|p| p.key)
    }

    // ---- 閉じる ------------------------------------------------------------

    /// タブを順に閉じる (未保存なら 1 つずつ確認)。ガード確認中 / 終了中は無視。
    pub(crate) fn request_close_tabs(&mut self, keys: Vec<ProjectKey>) {
        if self.shutdown.is_shutting_down()
            || self.ui_ephemeral.dirty_guard.is_some()
            || self.ui_ephemeral.guard_after_save.is_some()
            || self.cur.pipc.guard_pending_action.is_some()
        {
            return;
        }
        self.continue_close_tabs(keys);
    }

    /// `keys` の先頭から順に閉じる。確認が要るタブに当たったらそのタブへ切り替えて
    /// ガードを出し、通ったら [`Self::perform_guard_action`] が先頭を閉じて残りで
    /// ここへ戻る。キャンセルはガードが `dirty_guard` を捨てるだけなので残りも止まる。
    pub(crate) fn continue_close_tabs(&mut self, mut keys: Vec<ProjectKey>) {
        while let Some(key) = keys.first().copied() {
            if !self.tabs.order.contains(&key) {
                keys.remove(0);
                continue;
            }
            if self.tab_needs_guard(key) {
                self.switch_tab(key);
                self.request_guarded_action(DirtyGuardAction::CloseTabs(keys));
                return;
            }
            keys.remove(0);
            self.close_tab_now(key);
        }
    }

    /// 閉じる / 終了の前に確認 (または round-trip の drain 待ち) が要るタブか。
    fn tab_needs_guard(&mut self, key: ProjectKey) -> bool {
        self.with_project(key, |app| {
            app.cur.song_doc.is_dirty() || !app.cur.pipc.pending_state_queue.is_empty()
        })
        .unwrap_or(false)
    }

    /// 確認を通った (or clean な) タブを実際に閉じる。書き出し / 解析中のタブは閉じない。
    /// 最後の 1 つなら先に空の Untitled を開いてから閉じる (Q5)。アクティブなら隣へ移る。
    pub(crate) fn close_tab_now(&mut self, key: ProjectKey) {
        if !self.tabs.order.contains(&key) {
            return;
        }
        let busy = self.with_project(key, |app| app.export_or_analysis_busy()).unwrap_or(false);
        if busy {
            self.ui_ephemeral.status_message = "書き出し / 解析中のタブは閉じられません".into();
            return;
        }
        if self.tabs.len() == 1 {
            if self.new_tab().is_none() {
                return;
            }
        } else if key == self.cur.key {
            // 右隣 (端なら巡回) へ。`with_project` の中 (背景タブ宛 event の完了で閉じる)
            // なら本当にアクティブなタブへ戻る。
            let back = self
                .tabs
                .visiting_from
                .filter(|k| *k != key && self.tabs.order.contains(k));
            if let Some(next) = back.or_else(|| self.tabs.neighbor(key, true)) {
                self.switch_tab(next);
            }
        }
        // ここで `key` は parked。engine / host 側の実体を畳んでから捨てる。
        self.with_project(key, |app| {
            app.stop();
            app.silence_monitor_notes();
            if app.cur.pipc.pending_glue_bake.is_some() {
                app.abort_glue_bake("タブを閉じたので Glue の焼き込みを中止しました".into());
            }
            // `ClosePluginShmem` × device → `UnloadProject` の順 (host が unmapped shmem を
            // 踏まないための順序は `teardown_all_loaded_plugins` が守る)。
            app.teardown_all_loaded_plugins();
            app.send_audio(AudioCommand::CloseProject { project: key });
            // main renderer 上のサムネイル / 画像テクスチャの破棄予約。
            app.discard_gpu_derived_caches();
            // このタブが書いた autosave はもう要らない (clean で閉じるか、確認で保存 /
            // 破棄を選んだ後なので、残すと次回起動の復元候補に化ける)。
            app.remove_recovery_files_of_cur();
        });
        // Global Sampler の録音源がこのタブの track なら Master へ戻す
        // (`docs/plan_project_tabs.md` §5.5)。放っておくと、engine 側は「その
        // project の buffer」を待ち続けて **リングに何も書かれない** —
        // 波形も MIDI Capture の時間軸も、理由の分からないまま止まる。
        if self.sampler.source.project() == Some(key) {
            self.set_sampler_source(common::protocol::SamplerSource::Master);
            self.ui_ephemeral.status_message =
                "録音源のタブを閉じたので Global Sampler を Master に戻しました".into();
        }
        self.tabs.parked.retain(|p| p.key != key);
        self.tabs.order.retain(|k| *k != key);
        // retained widget state (drag session / scroll) は root が次フレームで捨てる。
        self.ui_ephemeral.retained_state_to_drop.push(key);
        tracing::info!(project = key.0, tabs = self.tabs.len(), "closed tab");
    }

    // ---- 終了 --------------------------------------------------------------

    /// 終了の続き (Q9): 未保存 (or round-trip 中) のタブを表示順に 1 つずつ確認し、
    /// 全部通ったら [`Self::begin_shutdown`]。確認モーダルはアクティブなタブについて出る
    /// ので対象タブへ切り替えてから出す。「保存せず終了」はそのタブを閉じて次へ進む。
    pub(crate) fn continue_quit(&mut self, req: QuitRequest) {
        for key in self.tabs.order.clone() {
            if self.tab_needs_guard(key) {
                self.switch_tab(key);
                self.request_guarded_action(DirtyGuardAction::Quit(req));
                return;
            }
        }
        self.begin_shutdown(req);
    }

    // ---- 全タブ横断 ----------------------------------------------------------

    /// 子プロセスの respawn 後 (`handle_child_disconnected`): 新しいプロセスはどのタブも
    /// 知らないので、全タブの engine slot / plugin instance / session state を作り直す。
    pub(crate) fn restore_tabs_after_respawn(&mut self, kind: common::protocol::ChildKind) {
        use common::protocol::ChildKind;
        let active = self.cur.key;
        for key in self.tabs.order.clone() {
            if matches!(kind, ChildKind::Audio) {
                // engine slot を作り直してから Song を届ける (順序保証付き IPC)。
                self.send_audio(AudioCommand::OpenProject { project: key });
            }
            self.with_project(key, |app| {
                // state restore: plugin slots は restore_plugin_from_song で SetSlotPlugin
                // 再送、 project_dir + LoadSong は次の frame flush (`last_synced_epoch` を
                // 巻き戻して必ず送らせる)。
                let song_snapshot = app.cur.song_doc.song().clone();
                app.restore_plugin_from_song(&song_snapshot);
                app.cur.pipc.last_synced_epoch = 0;
                // ループ (ON/OFF + 範囲) は `Song` に載らない session state なので LoadSong
                // では戻らない。 新しい audio プロセスは既定 (OFF / 範囲未設定) で立ち上がる
                // ため、 明示的に送り直して GUI 表示と engine の実挙動を揃える。
                if matches!(kind, ChildKind::Audio) {
                    app.set_loop_region(app.cur.transport.loop_region);
                }
            });
        }
        if matches!(kind, ChildKind::Audio) {
            self.send_audio(AudioCommand::SetScopeProject { project: active });
            // Global Sampler のリングも session state。GUI 側の shmem は生きているので、
            // 新しい audio プロセスに同じ世代を open させる (レートが変わっていたら作り直す)。
            self.resume_sampler_ring_after_respawn();
        }
    }

    /// いずれかのタブが engine のオフライン描画 (書き出し / ラウドネス解析) を占有しているか。
    /// engine 側は全体で 1 本なので、止める / 待つ判断はこの述語で行う。
    #[must_use]
    pub fn any_tab_export_or_analysis_busy(&self) -> bool {
        self.export_or_analysis_busy()
            || self.tabs.parked.iter().any(|p| {
                p.transport.pending_video_export.is_some()
                    || p.transport.export_stage.is_some()
                    || p.loudness.phase.is_busy()
            })
    }

    /// いずれかのタブに未保存変更があるか (OS セッション終了のブロック理由 / 窓タイトル)。
    #[must_use]
    pub fn any_tab_dirty(&self) -> bool {
        self.cur.song_doc.is_dirty() || self.tabs.parked.iter().any(|p| p.song_doc.is_dirty())
    }

    /// frame 末の子プロセス sync を **全タブ** で回す (背景タブも plugin の param 変更等で
    /// epoch が進む)。各タブは自分の `last_synced_epoch` を持つので、変化の無いタブは no-op。
    pub fn flush_all_song_sync(&mut self) {
        for key in self.tabs.order.clone() {
            self.with_project(key, |app| app.flush_song_sync());
        }
    }

    /// autosave を全タブで回す (各タブが自分の sidecar / session file に書く)。
    pub(crate) fn autosave_all_tabs(&mut self) {
        for key in self.tabs.order.clone() {
            self.with_project(key, |app| app.maybe_autosave());
        }
    }

    /// タブ帯 / 窓タイトルに出す名前 (file stem、未保存は `Untitled`)。
    #[must_use]
    pub fn tab_label(ps: &ProjectState) -> String {
        ps.song_doc
            .file_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled")
            .to_string()
    }

    /// 窓タイトル = アクティブなタブ (`*` = 未保存)。
    #[must_use]
    pub fn window_title(&self) -> String {
        let name = Self::tab_label(&self.cur);
        if self.cur.song_doc.is_dirty() {
            format!("*{name}")
        } else {
            name
        }
    }

    /// `key` のタブ (アクティブ含む) を読む。
    #[must_use]
    pub fn tab(&self, key: ProjectKey) -> Option<&ProjectState> {
        if key == self.cur.key {
            Some(&self.cur)
        } else {
            self.tabs.parked_ref(key)
        }
    }
}
