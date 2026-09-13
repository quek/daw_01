//! handler::recovery — autosave (sidecar / recovery_dir) と、クラッシュ復旧 modal の
//! 復元 / 破棄、 正常終了 / タブを閉じるときの recovery file 掃除。
//!
//! `handler/project.rs` から機械分割した `impl AppData` メソッド群 (挙動は元と同一、
//! サイズ budget = 不変条件 9)。
use crate::state::*;
use std::path::{Path, PathBuf};

impl AppData {
    pub(crate) fn maybe_autosave(&mut self) {
        if !self.cur.song_doc.is_dirty() {
            return;
        }
        if self.cur.song_doc.last_autosave.elapsed() < std::time::Duration::from_secs(60) {
            return;
        }

        // 保存先決定: file_path Some なら sidecar、 None なら recovery_dir。
        let autosave_path = match self.cur.song_doc.file_path.as_ref() {
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
                    &self.cur.song_doc.recovery_session_id,
                )
            }
        };

        // autosave も表示状態を同梱する (= ダーティでなくても view が
        // 永続化される → スクロール/ズーム変更が `*` を立てずに次回 open で復元される)。
        let view = self.snapshot_view_state();
        match common::project::save_project(&autosave_path, self.cur.song_doc.song(), Some(&view)) {
            Ok(()) => {
                tracing::info!(path = %autosave_path.display(), "autosaved");
                self.cur.song_doc.last_autosave = std::time::Instant::now();
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
                &self.cur.song_doc.recovery_session_id,
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
        self.cur.song_doc.last_autosave = std::time::Instant::now();
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
        if let Some(orig) = self.cur.song_doc.file_path.as_ref() {
            stale.push(common::recovery::sidecar_for(orig));
        }
        if let Some(dir) = self.ui_prefs.app_dirs.as_ref().map(|d| d.recovery_dir()) {
            stale.push(common::recovery::recovery_path_for_session(
                &dir,
                &self.cur.song_doc.recovery_session_id,
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
        let (mut song, view, loop_region, hidden_lanes) =
            (loaded.song, loaded.view, loaded.loop_region, loaded.hidden_automation_lanes);
        song.ensure_ids();
        // `docs/plan_project_tabs.md` §5.4: Open と同じ規則 — **同じファイルを 2 つのタブで
        // 開かない** (開くと互いに上書きし合う)。sidecar の復元先は元の .daw なので、
        // それを開いているタブがあればそこへ復元する。
        let restore_to = common::recovery::original_file_for_sidecar(&autosave_path);
        if let Some(key) = restore_to.as_deref().and_then(|p| self.tab_with_path(p)) {
            self.switch_tab(key);
        } else if !self.cur_tab_is_pristine() && self.new_tab().is_none() {
            // pristine な Untitled ならそこへ、そうでなければ新しいタブへ。
            return;
        }
        // 別プロジェクトへの丸ごと差し替えなので、現プロジェクトの plugin と
        // 開いている editor window を先に全て破棄する (action_open_path /
        // action_new と同じ teardown。 これが無いと「plugin 入り project を開いた
        // 直後の復元」 で旧 plugin 実体・editor 窓・GUI cache が残る)。
        self.teardown_all_loaded_plugins();
        self.restore_plugin_from_song(&song);
        self.cur.song_doc.replace_song(song);
        self.cur.song_doc.file_path = restore_to;
        // 復元した内容を新しい保存ベースラインに確定し、 履歴と Song スコープ
        // 状態を破棄する (action_open_path と同じく decode / view 復元より先)。
        // sidecar は元 project と同じ `project_id` を持つので、同一 project の
        // 復元ではキャッシュは温存される。
        self.after_song_replaced();
        // recovery 復元も load path と同じく background streaming
        // decode へ。 file_path を先にセット済みなので ProjectRelative も解決可。
        self.begin_asset_decode("プロジェクトを読込中");
        // recovery も表示状態 + 選択クリップを復元 (autosave が view を書いている)。
        self.restore_view_state(view, loop_region, hidden_lanes);
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
            recovered_to = ?self.cur.song_doc.file_path,
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
    /// 自セッションで作った recovery file (sidecar / recovery_dir 両方) を **全タブ** で削除。
    /// recovery file が無ければ no-op。 削除失敗は warn でログのみ。
    pub fn on_shutdown(&self) {
        self.remove_recovery_files_of(&self.cur);
        for ps in &self.tabs.parked {
            self.remove_recovery_files_of(ps);
        }
    }

    /// アクティブなタブの recovery file を削除する (タブを閉じるとき)。
    pub(crate) fn remove_recovery_files_of_cur(&self) {
        self.remove_recovery_files_of(&self.cur);
    }

    fn remove_recovery_files_of(&self, ps: &ProjectState) {
        // 自セッションの recovery_dir file
        if let Some(dir) = self.ui_prefs.app_dirs.as_ref().map(|d| d.recovery_dir()) {
            let p = common::recovery::recovery_path_for_session(
                &dir,
                &ps.song_doc.recovery_session_id,
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
        if let Some(orig) = ps.song_doc.file_path.as_ref() {
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
}
