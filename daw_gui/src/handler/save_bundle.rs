//! 保存の完了処理 — serialize 成功後に project bundle を自己完結させる
//! (`crate::media_bundle`)。 `begin_save` (plugin state の回収) は `project.rs`、
//! こちらは凍結済み snapshot を受け取ってからの後半。
//!
//! 順序 (どれも serialize 成功が前提):
//! 1. 未保存キャッシュ → bundle の move を commit (snapshot 由来)
//! 2. Save As なら旧 bundle の参照ファイルを新 bundle へ複製
//! 3. live と undo / redo 全段の path も bundle 相対へ書き換え (履歴側の
//!    `Absolute(cache)` を残すと Undo で音源を見失う)
//! 4. file_path 確定 → autosave 掃除 → recent 更新
//! 5. bundle 内の未参照ファイルをゴミ箱へ (live + 履歴 + 進行中 render の予約が「参照」)
//! 6. audio engine へ新 project_dir + song を流す

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use common::model::Song;

use crate::import_audio;
use crate::media_bundle;
use crate::state::*;

impl AppData {
    /// `song` 内の未保存 import/bounce cache source を `<project_dir>/{samples,bounce,images}/`
    /// へ移して path を `ProjectRelative` に書き換える。 save flow で **直列化する
    /// snapshot と working state の live / 履歴のすべて** に適用する: ファイルは move
    /// なので、 片方だけ移すと他方が移動後ファイルを見失う (= 初回呼び出しが move、
    /// 2 回目以降は dst.exists で path 書換のみ)。 失敗しても save は続行し missing
    /// source として扱う。 status へ最後の失敗メッセージを残す。
    pub(crate) fn migrate_unsaved_sources(song: &mut Song, project_dir: &Path, status: &mut String) {
        let moves = Self::plan_unsaved_migrations(song, project_dir);
        if let Err(e) = import_audio::commit_migration(&moves) {
            tracing::warn!(error = ?e, "未保存キャッシュ → bundle への移行で一部失敗");
            *status = format!("メディアの bundle への移行で一部失敗: {e}");
        }
    }

    /// audio (`samples/`) / bounce (`bounce/`) / video (`samples/`) / image (`images/`)
    /// の 4 プールぶんの plan を 1 本にまとめる (path 書換のみ、 I/O なし)。
    fn plan_unsaved_migrations(song: &mut Song, project_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
        let mut moves = import_audio::plan_unsaved_audio_migration(song, project_dir);
        moves.extend(import_audio::plan_unsaved_bounce_migration(song, project_dir));
        moves.extend(media_bundle::plan_unsaved_video_migration(song, project_dir));
        moves.extend(media_bundle::plan_unsaved_image_migration(song, project_dir));
        moves
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
        // 移動の plan を取る (= ここでは I/O しない、 破棄しても無害)。
        let moves = Self::plan_unsaved_migrations(&mut snapshot, &dir);
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
        // dedup、 live 固有 source があれば move)。
        if let Err(e) = import_audio::commit_migration(&moves) {
            tracing::warn!(error = ?e, "bundle への移行確定で一部失敗");
            self.ui_ephemeral.status_message = format!("メディアの bundle への移行で一部失敗: {e}");
        }
        // Save As (保存先フォルダが変わった): 旧 bundle の参照ファイルを新 bundle へ
        // 複製する。 live と履歴の `ProjectRelative` はこの時点ではまだ旧 bundle 相対
        // なので、 file_path を差し替える **前** に旧 dir を読む。
        let old_dir = self.song_doc.file_path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
        if let Some(old_dir) = old_dir.filter(|old| *old != dir) {
            self.relocate_bundle(&old_dir, &dir);
        }
        // round-trip 中に live へ編集が入ったかを epoch 差で先に記録する
        // (下の live migration は「保存完了処理の正規化」 で epoch を進める
        // ため、 記録後に行う)。
        let edited_since_snapshot = self.song_doc.edit_epoch() != snap_epoch;
        let mut status = std::mem::take(&mut self.ui_ephemeral.status_message);
        self.normalize_song(|song| Self::migrate_unsaved_sources(song, &dir, &mut status));
        self.song_doc.rewrite_history(|song| Self::migrate_unsaved_sources(song, &dir, &mut status));
        self.ui_ephemeral.status_message = status;
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
        self.push_recent(path.clone());
        self.push_recent_saved(path.clone());
        // bundle 内の未参照ファイルをゴミ箱へ。 migration / 複製が済んで live と履歴の
        // 参照が全部この bundle 相対になった **後**、 engine へ流す前に行う。
        self.sweep_bundle(&dir, &path);
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

    /// live + undo / redo 全段が bundle 内に持つ参照 (project-relative)。
    fn bundle_refs(&self, project_dir: &Path) -> HashSet<PathBuf> {
        let mut refs = HashSet::new();
        media_bundle::collect_bundle_refs(self.song_doc.song(), project_dir, &mut refs);
        for song in self.song_doc.history_songs() {
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

    /// 進行中の bounce / glue が名前を予約したファイル (project-relative)。 render が
    /// 書き終わるまで song に載らないので、 参照集合に足さないと掃除が消してしまう。
    fn in_flight_render_outputs(&self, project_dir: &Path) -> impl Iterator<Item = PathBuf> + '_ {
        let bounce = self.ipc.pending_clip_fx_bounce.as_ref().map(|p| p.out_path.clone());
        let glue = self
            .ipc
            .pending_glue_bake
            .iter()
            .flat_map(|p| p.jobs.iter().map(|j| j.out_path.clone()));
        let dir = project_dir.to_path_buf();
        bounce
            .into_iter()
            .chain(glue)
            .filter_map(move |abs| abs.strip_prefix(&dir).ok().map(Path::to_path_buf))
    }

    /// bundle 内の未参照ファイルをゴミ箱へ送る。 「参照」 = live + undo / redo 全段 +
    /// 進行中 render の予約。 同じフォルダに別 project があれば見送る。
    fn sweep_bundle(&mut self, project_dir: &Path, project_file: &Path) {
        let mut keep = self.bundle_refs(project_dir);
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
        app.song_doc.file_path = Some(old_daw.clone());
        app.song_doc.replace_song(two_clip_song());
        // clip 2 を消す → b.wav は undo 履歴だけが参照する。
        app.edit_song(|song| song.tracks[0].clips.retain(|c| c.id != 2));
        assert!(app.song_doc.can_undo());

        let new_dir = new.path().join("p2");
        let new_daw = new_dir.join("p2.daw");
        std::fs::create_dir_all(&new_dir).unwrap();
        let epoch = app.song_doc.edit_epoch();
        let snapshot = Box::new(app.song_doc.song().clone());
        app.finish_save(snapshot, new_daw.clone(), epoch);

        assert!(new_daw.exists(), "project file written");
        assert_eq!(app.song_doc.file_path.as_deref(), Some(new_daw.as_path()));
        assert!(new_dir.join("samples").join("a.wav").exists(), "live 参照は複製");
        assert!(new_dir.join("samples").join("b.wav").exists(), "undo 参照も複製");
        assert!(!new_dir.join("samples").join("orphan.wav").exists(), "未参照は複製しない");
        assert!(old.path().join("samples").join("orphan.wav").exists(), "旧 bundle は触らない");
        assert!(!app.song_doc.is_dirty());

        // 上書き保存: 新 bundle に紛れ込んだ未参照ファイルだけゴミ箱へ、 b.wav は残る。
        let junk = new_dir.join("bounce").join("junk.wav");
        touch(&junk);
        let epoch = app.song_doc.edit_epoch();
        let snapshot = Box::new(app.song_doc.song().clone());
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
        app.song_doc.file_path = Some(daw.clone());
        let mut song = two_clip_song();
        song.tracks[0].clips.truncate(1);
        app.song_doc.replace_song(song);
        let epoch = app.song_doc.edit_epoch();
        let snapshot = Box::new(app.song_doc.song().clone());
        app.finish_save(snapshot, daw, epoch);
        assert!(junk.exists(), "相乗りフォルダでは掃除しない");
    }
}
