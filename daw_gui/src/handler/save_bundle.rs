//! 保存の完了処理 — serialize 成功後に project bundle を自己完結させる
//! (`crate::media_bundle`)。 `begin_save` (plugin state の回収) は `project.rs`、
//! こちらは凍結済み snapshot を受け取ってからの後半。
//!
//! 順序 (どれも serialize 成功が前提):
//! 1. 未保存の置き場 → bundle の運搬を commit (snapshot 由来。 自分の置き場は move、
//!    別の文書の置き場は copy。 Song が凍っている間は自分の置き場も copy)
//! 2. Save As なら旧 bundle の参照ファイルを新 bundle へ複製
//! 3. live と undo / redo 全段の path も bundle 相対へ書き換え (履歴側の
//!    `Absolute(cache)` を残すと Undo で音源を見失う)。 運べなかったものは置き場を指したまま残す
//! 4. file_path 確定 (運び切れなければ未保存のまま) → autosave 掃除 → recent 更新
//! 5. bundle 内の未参照ファイルをゴミ箱へ (書いた snapshot + live + 履歴 + 進行中 render の予約が「参照」)
//! 6. 何も指さなくなった未保存の置き場を消す
//! 7. audio engine へ新 project_dir + song を流す

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use common::model::Song;

use crate::media_bundle::{self, TransferFailure};
use crate::state::*;

impl AppData {
    /// serialize する `snapshot` の未保存の置き場の媒体を bundle 相対へ書き換え、 実ファイルの運び方を返す
    /// (I/O なし)。 未保存の置き場は注入された `app_dirs` の下だけ (`crate::media_dest` と同じ解決) —
    /// `app_dirs` が無ければ置き場へ取り込めていないので、 運ぶものも無い。
    fn plan_unsaved_migrations(&self, snapshot: &mut Song, project_dir: &Path) -> Vec<media_bundle::MediaTransfer> {
        let Some(dirs) = self.ui_prefs.app_dirs.as_ref() else { return Vec::new() };
        media_bundle::plan_unsaved_migration(snapshot, project_dir, dirs, self.cur.song_doc.unsaved.id())
    }

    /// 書き出した保存のあとで、 live と undo / redo 全段が未保存の置き場を指す媒体を `project_dir` の bundle へ
    /// 運び、 **運べたものだけ** bundle 相対へ書き換える ([`media_bundle::migrate_unsaved`])。 自分の置き場の
    /// ファイルは移すので、 片方だけ書き換えると他方が移動後のファイルを見失う。 `failures` = この保存で
    /// 運べなかったもの (試し直さず、 ここで増えた分も足す)。 運べなかったものは置き場を指したまま鳴り続ける。
    ///
    /// **Song が凍っている間 (オフライン描画中、 書き出しロック) は live を書き換えない** — 書き換えは epoch を
    /// 進め、 描画中の engine へ全曲の `LoadSong` を送ってしまう。 live は置き場を指したまま残るので、 置き場の
    /// 実体は移さずに複製する (移すと live が指す実体が消える)。 残った参照は sidecar の autosave の前
    /// ([`Self::settle_unsaved_place_of_saved_doc`]) か次の保存で運ぶ。 書き換えられるときは運ぶものが無くても
    /// `normalize_song` を通す — epoch が進み、 保存の最後の `flush_song_sync` が新しい project_dir を engine へ届ける。
    fn migrate_doc_into_bundle(&mut self, project_dir: &Path, failures: &mut Vec<TransferFailure>) {
        let unsaved = self.ui_prefs.app_dirs.clone().map(|d| (d, self.cur.song_doc.unsaved.id()));
        let frozen = self.offline_render_busy();
        if !frozen {
            self.normalize_song(|song| {
                if let Some((dirs, own)) = &unsaved {
                    media_bundle::migrate_unsaved(song, project_dir, dirs, *own, false, failures);
                }
            });
        }
        if let Some((dirs, own)) = &unsaved {
            self.cur.song_doc.rewrite_history(|song| {
                media_bundle::migrate_unsaved(song, project_dir, dirs, *own, frozen, failures);
            });
        }
    }

    /// 保存済みの文書がまだ未保存の置き場を指していれば、 bundle へ運んで何も指さなくなった置き場を消す。
    /// 指したまま残るのは、 保存で運べなかったとき / オフライン描画中の保存で live を書き換えられなかったとき /
    /// 未保存の間に始めた Bounce・Glue が保存の後に焼き上がったとき。
    ///
    /// **sidecar の autosave を書く前に呼ぶ** (`maybe_autosave`)。 置き場を残す持ち主の記録は起動中のロックと
    /// recovery_dir の autosave だけ (`crate::unsaved_place`) で、 sidecar はそれにならない — sidecar に置き場の
    /// パスを書いたまま落ちると、 次の起動の掃除が置き場を消し、 復元した文書の音源が無くなる。 Song が凍っている
    /// 間は live を書き換えられないので運ばない。
    pub(crate) fn settle_unsaved_place_of_saved_doc(&mut self) {
        let (Some(dir), Some(dirs)) = (self.project_dir(), self.ui_prefs.app_dirs.clone()) else { return };
        // 置き場を指す参照があるのは、 このプロセスが置き場へ書いた / 引き継いだ文書だけ。
        if !self.cur.song_doc.unsaved.is_claimed() {
            return;
        }
        if !self.offline_render_busy() && self.doc_references_inside(&self.own_unsaved_place_dirs(&dirs)) {
            let mut failures = Vec::new();
            self.migrate_doc_into_bundle(&dir, &mut failures);
            Self::report_transfer_failures(&failures, &mut self.ui_ephemeral.status_message);
        }
        self.release_unsaved_place_if_unreferenced();
    }

    fn report_transfer_failures(failures: &[TransferFailure], status: &mut String) {
        for f in failures {
            tracing::warn!(detail = %f.message, "未保存の置き場 → bundle への運搬に失敗");
        }
        if let Some(last) = failures.last() {
            *status = format!(
                "メディア {} 件を bundle へ運べませんでした (未保存のまま残します): {}",
                failures.len(),
                last.message
            );
        }
    }

    /// 凍結済み `snapshot` をファイルへ書き出して保存を完了する。
    ///
    /// cache migration は **2 段階**で行い、 破壊的なファイル移動を serialize 成功後に
    /// のみ確定する: (1) serialize 前に snapshot の path だけを `ProjectRelative`
    /// へ書き換えて move plan を取る (I/O なし)、 (2) serialize 成功後に plan を commit
    /// (実ファイル move) し、 live / 履歴も migrate する。 こうすると書き出し失敗時に
    /// import_cache のファイルが無傷で残り、 live は `Absolute(cache)` のまま
    /// autosave/recovery が健全に働く。 **serialize が成功して初めて** file_path を確定し
    /// (旧契約)、 audio engine へ新 project_dir + song を流す。 saved baseline = snapshot、
    /// `is_dirty` は live と snapshot の差で再計算する (state 待ちの間の編集が live に
    /// あれば dirty)。
    pub(crate) fn finish_save(&mut self, mut snapshot: Box<Song>, path: PathBuf, snap_epoch: u64) {
        let Some(dir) = path.parent().map(Path::to_path_buf) else {
            tracing::error!(path = %path.display(), "save path has no parent dir");
            self.ui_ephemeral.status_message = "保存先フォルダを決められません".into();
            self.ui_ephemeral.guard_after_save = None;
            return;
        };
        // serialize する snapshot の path を ProjectRelative に書き換え、 実ファイル
        // 運搬の plan を取る (= ここでは I/O しない、 破棄しても無害)。
        let moves = self.plan_unsaved_migrations(&mut snapshot, &dir);
        // 現在の表示状態を同梱して保存する (snapshot は楽曲のみ凍結、
        // view は presentation なので保存実行時の live を採るので十分)。
        let view = self.snapshot_view_state();
        if let Err(e) = common::project::save_project(&path, &snapshot, Some(&view)) {
            tracing::error!(error = ?e, path = %path.display(), "failed to save project");
            self.ui_ephemeral.status_message = format!("保存に失敗しました: {e}");
            // 保存失敗 → 操作を実行しない (データ損失回避)。 保留操作はクリアして、
            // state 待ちのたびに再保存が走り続ける無限ループを防ぐ。
            self.ui_ephemeral.guard_after_save = None;
            return;
        }
        tracing::info!(path = %path.display(), "saved project");
        // serialize 成功 → 破壊的 migration を確定する。 まず snapshot 由来の
        // ファイルを move (plan を commit)、 次に live / 履歴を migrate して
        // ProjectRelative + 自己完結にする (plan 済みファイルは dst.exists で
        // dedup、 live 固有 source があれば move)。 Song が凍っていて live を書き換えられない
        // 間は移さずに複製する (`migrate_doc_into_bundle` の doc)。
        let mut failures = media_bundle::commit_transfers(&moves, self.offline_render_busy());
        // Save As (保存先フォルダが変わった): 旧 bundle の参照ファイルを新 bundle へ
        // 複製する。 live と履歴の `ProjectRelative` はこの時点ではまだ旧 bundle 相対
        // なので、 file_path を差し替える **前** に旧 dir を読む。
        let old_dir = self.cur.song_doc.file_path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
        if let Some(old_dir) = old_dir.filter(|old| *old != dir) {
            self.relocate_bundle(&old_dir, &dir);
        }
        // round-trip 中に live へ編集が入ったかを epoch 差で先に記録する
        // (下の live migration は「保存完了処理の正規化」 で epoch を進める
        // ため、 記録後に行う)。
        let edited_since_snapshot = self.cur.song_doc.edit_epoch() != snap_epoch;
        self.migrate_doc_into_bundle(&dir, &mut failures);
        // serialize 成功時のみ file_path を確定する (旧契約)。
        self.cur.song_doc.file_path = Some(path.clone());
        // 保存が現在の live 内容を含む (= round-trip 中の編集なし) なら
        // clean。 編集が入っていれば dirty のまま (下の guard_after_save
        // 再保存 loop が残りを確定する)。 save 後も Undo できるよう履歴は
        // 残す (replace_song は使わない)。 bundle へ運べなかった媒体は保存したファイルに
        // 入っていない (live は置き場を指して鳴り続ける) ので、 未保存のまま次の保存でやり直させる。
        if !edited_since_snapshot && failures.is_empty() {
            self.cur.song_doc.mark_saved();
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
        self.push_recent(path.clone());
        self.push_recent_saved(path.clone());
        // bundle 内の未参照ファイルをゴミ箱へ。 migration / 複製が済んで live と履歴の
        // 参照が全部この bundle 相対になった **後**、 engine へ流す前に行う。
        self.sweep_bundle(&dir, &path, &snapshot);
        // 未保存の間の置き場は、 中身が bundle へ移って何も指さなくなったら消す (同じく migration の後)。
        self.release_unsaved_place_if_unreferenced();
        // 運べなかった媒体は、 掃除の報告より優先して出す (文書が未保存のまま残る理由)。
        Self::report_transfer_failures(&failures, &mut self.ui_ephemeral.status_message);
        // PR6: migration (直上の normalize) で audio_sources の path が
        // `Absolute(import_cache)` → `ProjectRelative(samples/)` に書き換わり、
        // project_dir も新たに確定した (file_path は上で path に設定済)。
        // normalize は必ず epoch を bump するので、 ここで flush_song_sync が
        // 最新 live song + project_dir (= file_path.parent()) を audio engine
        // へ届けて `AudioClipRenderer` を rebuild させる (SetProjectDir →
        // LoadSong の順序保証つき)。 epoch bump 済なので no-op にならない (Song が凍っている間は
        // normalize を通さないので no-op — 描画中の engine を差し替えず、 描画の後の最初の同期が届ける)。
        self.flush_song_sync();
        // 「保存して続行」: この保存は成功した。 plugin state 待ちの間に live へ
        // 編集が入って dirty なら (co-temporal snapshot は編集前で凍結されている
        // ため、 その編集はこの保存に含まれない)、 残りを確定するため同じ path へ
        // 再保存して保留操作を維持する。 clean なら保留操作 (終了 / New / Open)
        // を実行する。 save 成功が分かるこの場所で判定するので、 失敗時の無限
        // 再保存ループに陥らない。
        if self.ui_ephemeral.guard_after_save.is_some() {
            if !failures.is_empty() {
                // 媒体を運び切れなかった: 保留操作 (終了 / タブを閉じる) を実行すると置き場ごと消える。
                // 再保存しても同じ理由で失敗し続けるので、 保存失敗と同じく保留を捨てる。
                self.ui_ephemeral.guard_after_save = None;
            } else if self.cur.song_doc.is_dirty() {
                self.begin_save(path);
            } else if let Some(action) = self.ui_ephemeral.guard_after_save.take() {
                self.perform_guard_action(action);
            }
        }
    }

    /// live + undo / redo 全段が bundle 内に持つ参照 (project-relative)。
    fn bundle_refs(&self, project_dir: &Path) -> HashSet<PathBuf> {
        let mut refs = HashSet::new();
        media_bundle::collect_bundle_refs(self.cur.song_doc.song(), project_dir, &mut refs);
        for song in self.cur.song_doc.history_songs() {
            media_bundle::collect_bundle_refs(song, project_dir, &mut refs);
        }
        refs
    }

    /// Save As: 旧 bundle が持つ参照ファイル (live + 履歴) を新 bundle へ複製する。
    /// 同期コピー — 保存は元々同期 I/O で、 engine へ新 project_dir を流す前に
    /// 実体が揃っていなければならない (無いと decode が missing source を積む)。
    fn relocate_bundle(&mut self, old_dir: &Path, new_dir: &Path) {
        let refs = self.bundle_refs(old_dir);
        let plan = media_bundle::plan_relocation(&refs, old_dir, new_dir);
        let report = media_bundle::commit_relocation(&plan);
        tracing::info!(
            old = %old_dir.display(),
            new = %new_dir.display(),
            copied = report.copied,
            missing = report.missing.len(),
            failed = report.failures.len(),
            "relocated bundle media for Save As"
        );
        for m in &report.missing {
            tracing::warn!(path = %m.display(), "Save As: 旧 bundle に実体が無い参照 (複製できず)");
        }
        for f in &report.failures {
            tracing::warn!(detail = %f, "Save As: メディアの複製に失敗");
        }
        if !report.failures.is_empty() {
            self.ui_ephemeral.status_message =
                format!("メディアの複製に {} 件失敗しました (ログ参照)", report.failures.len());
        }
    }

    /// 進行中の bounce / glue が名前を予約したファイル (絶対)。 render が書き終わるまで song に
    /// 載らないので、 掃除 / 置き場の後始末はこれも「使用中」 に数える。
    pub(crate) fn in_flight_render_paths(&self) -> impl Iterator<Item = &Path> + '_ {
        let bounce = self.cur.pipc.pending_clip_fx_bounce.as_ref().map(|p| p.out_path.as_path());
        let glue = self
            .cur.pipc
            .pending_glue_bake
            .iter()
            .flat_map(|p| p.jobs.iter().map(|j| j.out_path.as_path()));
        bounce.into_iter().chain(glue)
    }

    /// [`Self::in_flight_render_paths`] のうち bundle 内のもの (project-relative)。
    fn in_flight_render_outputs(&self, project_dir: &Path) -> impl Iterator<Item = PathBuf> + '_ {
        let dir = project_dir.to_path_buf();
        self.in_flight_render_paths()
            .filter_map(move |abs| abs.strip_prefix(&dir).ok().map(Path::to_path_buf))
    }

    /// bundle 内の未参照ファイルをゴミ箱へ送る。 「参照」 = 書いた `snapshot` + live + undo / redo 全段 +
    /// 進行中 render の予約。 同じフォルダに別 project があれば見送る。 `snapshot` を数えるのは、 live を
    /// 書き換えられなかった保存 (オフライン描画中) では、 保存したファイルだけが bundle の複製を指すから。
    fn sweep_bundle(&mut self, project_dir: &Path, project_file: &Path, snapshot: &Song) {
        let mut keep = self.bundle_refs(project_dir);
        media_bundle::collect_bundle_refs(snapshot, project_dir, &mut keep);
        keep.extend(self.in_flight_render_outputs(project_dir));
        let orphans = match media_bundle::orphan_media_files(project_dir, project_file, &keep) {
            Ok(list) => list,
            Err(media_bundle::SweepSkip::SharedFolder(other)) => {
                tracing::info!(
                    other = %other.display(),
                    "bundle sweep skipped: another project shares the folder"
                );
                return;
            }
        };
        if orphans.is_empty() {
            return;
        }
        match media_bundle::trash_files(&orphans) {
            Ok(()) => {
                for p in &orphans {
                    tracing::info!(path = %p.display(), "moved unreferenced media to trash");
                }
                self.ui_ephemeral.status_message =
                    format!("保存しました。 未使用メディア {} 件をゴミ箱へ送りました", orphans.len());
            }
            Err(e) => {
                tracing::warn!(error = %e, count = orphans.len(), "failed to trash unreferenced media");
                self.ui_ephemeral.status_message =
                    format!("未使用メディア {} 件をゴミ箱へ送れませんでした: {e}", orphans.len());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use common::model::{
        AudioContent, AudioEvent, AudioSource, AudioSourcePath, Clip, ClipContent, Song, Track,
    };
    use tempfile::tempdir;

    use crate::test_support::headless_app;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    fn source(rel: &str) -> AudioSource {
        AudioSource {
            path: AudioSourcePath::ProjectRelative(PathBuf::from(rel)),
            sample_rate: 48_000,
            channels: 1,
            frames: 1,
            original_bpm: None,
            root_key: None,
        }
    }

    /// 1 track / 2 clip。 clip 1 → samples/a.wav、 clip 2 → samples/b.wav。
    fn two_clip_song() -> Song {
        let mut song = Song::default();
        let mut clips = Vec::new();
        for (source_id, rel) in [(1u32, "samples/a.wav"), (2u32, "samples/b.wav")] {
            song.media.audio_sources.insert(source_id, source(rel));
            let cid = song.alloc_content_id();
            song.clip_contents.insert(
                cid,
                ClipContent::Audio(AudioContent {
                    events: vec![AudioEvent { source_id, ..Default::default() }],
                    next_event_id: 0,
                }),
            );
            clips.push(Clip {
                id: source_id,
                start_beat: f64::from(source_id),
                length_beats: 1.0,
                content_id: cid,
                ..Default::default()
            });
        }
        song.tracks = vec![Track { id: 1, clips, next_clip_id: 3, ..Track::default() }];
        song
    }

    /// Save As: 旧 bundle の参照ファイルが新 bundle へ複製され (Undo で戻る分も)、
    /// 未参照は複製されない。 旧 bundle は触らない。 続く上書き保存で新 bundle の
    /// 未参照ファイルだけがゴミ箱へ行き、 Undo が参照する分は残る。
    #[test]
    fn save_as_relocates_referenced_media_then_sweep_keeps_undo_refs() {
        let old = tempdir().unwrap();
        let new = tempdir().unwrap();
        let old_daw = old.path().join("p1.daw");
        touch(&old_daw);
        touch(&old.path().join("samples").join("a.wav"));
        touch(&old.path().join("samples").join("b.wav"));
        touch(&old.path().join("samples").join("orphan.wav"));

        let mut app = headless_app();
        app.cur.song_doc.file_path = Some(old_daw.clone());
        app.cur.song_doc.replace_song(two_clip_song());
        // clip 2 を消す → b.wav は undo 履歴だけが参照する。
        app.edit_song(|song| song.tracks[0].clips.retain(|c| c.id != 2));
        assert!(app.cur.song_doc.can_undo());

        let new_dir = new.path().join("p2");
        let new_daw = new_dir.join("p2.daw");
        std::fs::create_dir_all(&new_dir).unwrap();
        let epoch = app.cur.song_doc.edit_epoch();
        let snapshot = Box::new(app.cur.song_doc.song().clone());
        app.finish_save(snapshot, new_daw.clone(), epoch);

        assert!(new_daw.exists(), "project file written");
        assert_eq!(app.cur.song_doc.file_path.as_deref(), Some(new_daw.as_path()));
        assert!(new_dir.join("samples").join("a.wav").exists(), "live 参照は複製");
        assert!(new_dir.join("samples").join("b.wav").exists(), "undo 参照も複製");
        assert!(!new_dir.join("samples").join("orphan.wav").exists(), "未参照は複製しない");
        assert!(old.path().join("samples").join("orphan.wav").exists(), "旧 bundle は触らない");
        assert!(!app.cur.song_doc.is_dirty());

        // 上書き保存: 新 bundle に紛れ込んだ未参照ファイルだけゴミ箱へ、 b.wav は残る。
        let junk = new_dir.join("bounce").join("junk.wav");
        touch(&junk);
        let epoch = app.cur.song_doc.edit_epoch();
        let snapshot = Box::new(app.cur.song_doc.song().clone());
        app.finish_save(snapshot, new_daw.clone(), epoch);
        assert!(!junk.exists(), "未参照はゴミ箱へ (status: {})", app.ui_ephemeral.status_message);
        assert!(new_dir.join("samples").join("b.wav").exists(), "undo 参照は残る");
        assert!(new_dir.join("samples").join("a.wav").exists());
        assert!(app.ui_ephemeral.status_message.contains("1 件"), "{}", app.ui_ephemeral.status_message);
    }

    /// 同じフォルダに別の `.daw` があれば掃除しない (相乗り project の参照は分からない)。
    #[test]
    fn sweep_is_skipped_in_a_shared_folder() {
        let dir = tempdir().unwrap();
        let daw = dir.path().join("p.daw");
        touch(&dir.path().join("other.daw"));
        touch(&dir.path().join("samples").join("a.wav"));
        let junk = dir.path().join("samples").join("junk.wav");
        touch(&junk);

        let mut app = headless_app();
        app.cur.song_doc.file_path = Some(daw.clone());
        let mut song = two_clip_song();
        song.tracks[0].clips.truncate(1);
        app.cur.song_doc.replace_song(song);
        let epoch = app.cur.song_doc.edit_epoch();
        let snapshot = Box::new(app.cur.song_doc.song().clone());
        app.finish_save(snapshot, daw, epoch);
        assert!(junk.exists(), "相乗りフォルダでは掃除しない");
    }
}
