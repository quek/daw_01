//! handler::recovery — autosave (sidecar / recovery_dir) と、クラッシュ復旧 modal の
//! 復元 / 破棄、 正常終了 / タブを閉じるときの recovery file と未保存の置き場の掃除。
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
                common::recovery::recovery_path_for(&dir, self.cur.song_doc.unsaved.id())
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
            stale.push(common::recovery::recovery_path_for(&dir, self.cur.song_doc.unsaved.id()));
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
            stale.push(common::recovery::recovery_path_for(&dir, self.cur.song_doc.unsaved.id()));
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
    ///
    /// recovery_dir 内のものは **その id ごと** 引き継ぐ: 復元前に取り込んだ素材はその id の
    /// 置き場 (`crate::unsaved_place`) にあり、 以後の autosave も同じファイルへ書く。 どちらの
    /// 形でも autosave ファイルは消さない — 復元した中身は保存先にまだ書かれていないので、
    /// 保存 / 破棄 / 閉じるまではそれが唯一の控え (復元直後に落ちてもまた復元できる)。 同じ理由で
    /// 復元した文書は未保存 (`*`) にする — clean にすると確認なしで閉じられ、 控えごと消える。
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
        let restore_to = common::recovery::original_file_for_sidecar(&autosave_path);
        // 置き場を引き継げるか (別の daw_gui が先に復元していないか) を、何かを壊す前に確かめる。
        let Ok(adopted) = self.adopt_recovery_place(&autosave_path, restore_to.is_none()) else {
            return;
        };
        // `docs/plan_project_tabs.md` §5.4: Open と同じ規則 — **同じファイルを 2 つのタブで
        // 開かない** (開くと互いに上書きし合う)。sidecar の復元先は元の .daw なので、
        // それを開いているタブがあればそこへ復元する。
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
        // 置き換えられる文書の未保存の間の autosave / 置き場を片付け、 復元する文書の置き場へ。
        self.retire_replaced_unsaved_storage(adopted);
        self.cur.song_doc.replace_song(song);
        self.cur.song_doc.file_path = restore_to;
        // 履歴と Song スコープ状態を破棄する (action_open_path と同じく decode / view 復元より先)。
        // sidecar は元 project と同じ `project_id` を持つので、同一 project の
        // 復元ではキャッシュは温存される。
        self.after_song_replaced();
        self.cur.song_doc.mark_dirty_after_load_fixup();
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
        self.ui_ephemeral.recovery_candidates.retain(|p| p != &autosave_path);
        if self.ui_ephemeral.recovery_candidates.is_empty() {
            self.ui_ephemeral.show_recovery_modal = false;
        }
        tracing::info!(
            recovered_to = ?self.cur.song_doc.file_path,
            "recovery restored"
        );
    }

    /// recovery_dir 内の `<id>.autosave.daw` (`untitled`) なら、その id の置き場を引き継ぐ。
    /// sidecar / per-user データフォルダ無しは `Ok(None)` (引き継ぐ置き場が無い)。
    /// 別のプロセスが使用中などで引き継げなければ status に出して `Err`。
    fn adopt_recovery_place(
        &mut self,
        autosave_path: &Path,
        untitled: bool,
    ) -> Result<Option<crate::unsaved_place::UnsavedPlace>, ()> {
        let id = common::recovery::DocId::of_recovery_file(autosave_path).filter(|_| untitled);
        let (Some(id), Some(dirs)) = (id, self.ui_prefs.app_dirs.as_ref()) else {
            return Ok(None);
        };
        crate::unsaved_place::UnsavedPlace::adopt(dirs, id).map(Some).map_err(|e| {
            tracing::warn!(error = %e, %id, "cannot adopt the unsaved media place of a recovery");
            self.ui_ephemeral.status_message = format!("復元失敗: {e}");
        })
    }

    /// Recovery modal で「破棄」 を押した処理。 file 削除 + candidates から外す。
    /// recovery_dir 内のもの (`<id>.autosave.daw`) は、その id の未保存の置き場も消す。
    pub(crate) fn discard_recovery(&mut self, autosave_path: PathBuf) {
        if let Err(e) = std::fs::remove_file(&autosave_path) {
            tracing::warn!(
                error = ?e,
                path = %autosave_path.display(),
                "failed to remove recovery file"
            );
        }
        if let (Some(id), Some(dirs)) = (
            common::recovery::DocId::of_recovery_file(&autosave_path),
            self.ui_prefs.app_dirs.as_ref(),
        ) {
            crate::unsaved_place::remove_unowned(dirs, id);
        }
        self.ui_ephemeral.recovery_candidates.retain(|p| p != &autosave_path);
        if self.ui_ephemeral.recovery_candidates.is_empty() {
            self.ui_ephemeral.show_recovery_modal = false;
        }
    }

    /// アプリ終了の完了時 (`finish_shutdown`) に呼ぶ cleanup。 **全タブ** の文書が per-user
    /// データフォルダに持つもの (recovery_dir の autosave / sidecar / 未保存の置き場) を削除。
    /// 無ければ no-op。 削除失敗は warn でログのみ (残った置き場は次の起動の掃除が拾う)。
    pub fn on_shutdown(&mut self) {
        let dirs = self.ui_prefs.app_dirs.clone();
        Self::retire_doc_files(dirs.as_ref(), &mut self.cur);
        for ps in &mut self.tabs.parked {
            Self::retire_doc_files(dirs.as_ref(), ps);
        }
    }

    /// アクティブなタブの文書のファイルを削除する (タブを閉じるとき)。
    pub(crate) fn remove_recovery_files_of_cur(&mut self) {
        let dirs = self.ui_prefs.app_dirs.clone();
        Self::retire_doc_files(dirs.as_ref(), &mut self.cur);
    }

    fn retire_doc_files(dirs: Option<&common::app_dirs::AppDirs>, ps: &mut ProjectState) {
        // 自セッションの recovery_dir file と未保存の置き場
        if let Some(dirs) = dirs {
            Self::retire_unsaved_storage(dirs, &mut ps.song_doc);
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
