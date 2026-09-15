//! handler::unsaved_storage — 取り込み / 生成したメディアの置き場 ([`crate::media_dest`]) の解決と、
//! 未保存の文書の置き場 ([`crate::unsaved_place`]) の持ち主としての操作 (書く前の使用中の記録 /
//! 別の文書の置き場から運んできた媒体の複製 / 保存後・破棄時の後始末)。

use std::path::{Path, PathBuf};

use common::app_dirs::AppDirs;
use common::model::{AudioSourcePath, ImageSourcePath, MediaManifest, VideoSourcePath};

use crate::media_dest::{MediaDest, MediaPool, TransferMode};
use crate::state::{AppData, SongDoc};
use crate::unsaved_place::UnsavedPlace;

impl AppData {
    /// 保存済みプロジェクトのディレクトリ (`samples/` の親)。未保存なら `None`。
    pub(crate) fn project_dir(&self) -> Option<PathBuf> {
        self.cur.song_doc.file_path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf))
    }

    /// 取り込み / 生成したメディアの置き場 ([`crate::media_dest`])。未保存なら **この文書の置き場** を
    /// 使用中と記録してから返す (起動時の掃除や別プロセスに消させない)。未保存の置き場は注入された
    /// `app_dirs` からだけ引く。置き場が無い / 使えなければ `what` を付けて status に出して `None`。
    pub(crate) fn media_dest(&mut self, pool: MediaPool, what: &str) -> Option<MediaDest> {
        if let Some(dir) = self.project_dir() {
            return Some(MediaDest::bundle(&dir, pool));
        }
        let Some(dirs) = self.ui_prefs.app_dirs.clone() else {
            self.ui_ephemeral.status_message = format!("{what}: {}", crate::media_dest::NO_UNSAVED_DIR);
            return None;
        };
        let place = &mut self.cur.song_doc.unsaved;
        if let Err(e) = place.claim(&dirs) {
            self.ui_ephemeral.status_message = format!("{what}: {e}");
            return None;
        }
        Some(MediaDest::unsaved(pool.unsaved_dir(&dirs, place.id())))
    }

    /// 別の文書から運んできた媒体の写し (clipboard / タブ間ドラッグ) のうち、**別の文書の未保存の
    /// 置き場** にあるものをこの文書の置き場 (保存済みなら bundle) へ複製し、写しのパスを張り替える。
    ///
    /// 置き場の持ち主はその文書だけ (保存で自分の bundle へ移し、閉じたら消す) なので、パスを
    /// 借りたまま取り込むと、持ち主の保存 / 破棄で貼り先の音源が消える。複製できなかったもの
    /// (元が既に無い) はパスを変えない (decode が「見つからない」を出す)。
    pub(crate) fn adopt_foreign_unsaved_media(&mut self, media: &mut MediaManifest) {
        let Some(dirs) = self.ui_prefs.app_dirs.clone() else { return };
        let own = self.cur.song_doc.unsaved.id();
        let saved = self.project_dir().is_some();
        // 自分の置き場のもの (Move) は、未保存なら自分の持ち物のまま。保存済みなら置き場はもう
        // 使わない (保存後に出来上がった Bounce の出力など) ので bundle へ運ぶ。
        let foreign = |pool: MediaPool, abs: &Path| match pool.transfer_mode(abs, &dirs, own) {
            Some(TransferMode::Copy) => true,
            Some(TransferMode::Move) => saved,
            None => false,
        };
        for (_, a) in &mut media.audio {
            let AudioSourcePath::Absolute(abs) = &a.path else { continue };
            let Some(pool) = MediaPool::of_unsaved_audio(abs, &dirs).filter(|p| foreign(*p, abs)) else {
                continue;
            };
            if let Some((dest, name)) = self.copy_into_own_storage(abs, pool) {
                a.path = dest.audio_path(&name);
            }
        }
        for (_, v) in &mut media.video {
            let VideoSourcePath::Absolute(abs) = &v.path else { continue };
            if foreign(MediaPool::Samples, abs)
                && let Some((dest, name)) = self.copy_into_own_storage(abs, MediaPool::Samples)
            {
                v.path = dest.video_path(&name);
            }
        }
        for (_, i) in &mut media.image {
            let ImageSourcePath::Absolute(abs) = &i.path else { continue };
            if foreign(MediaPool::Images, abs)
                && let Some((dest, name)) = self.copy_into_own_storage(abs, MediaPool::Images)
            {
                i.path = dest.image_path(&name);
            }
        }
    }

    /// `src` をこの文書の `pool` の置き場へ同じ名前で複製する (名前は内容 hash / 一意な Bounce 名
    /// なので、既にあれば同じもの)。
    fn copy_into_own_storage(&mut self, src: &Path, pool: MediaPool) -> Option<(MediaDest, String)> {
        let name = src.file_name()?.to_str()?.to_string();
        let dest = self.media_dest(pool, "貼り付けた素材の複製")?;
        let dir = dest.dir();
        let copied = std::fs::create_dir_all(&dir)
            .and_then(|()| common::atomic_file::copy_new(src, &dir.join(&name)));
        match copied {
            Ok(_) => Some((dest, name)),
            Err(e) => {
                tracing::warn!(error = %e, src = %src.display(), "failed to copy pasted media from another document's unsaved place");
                self.ui_ephemeral.status_message =
                    format!("貼り付けた素材を複製できません ({}): {e}", src.display());
                None
            }
        }
    }

    /// 保存が済んで live と履歴の参照が bundle へ移った後: この文書の未保存の置き場を消す。
    /// 進行中の Bounce / Glue が置き場へ書いている間は残す (出来上がった出力は次の保存で移り、
    /// そこで消える。保存しなければ閉じるときに消える)。
    pub(crate) fn release_unsaved_place_after_save(&mut self) {
        let Some(dirs) = self.ui_prefs.app_dirs.clone() else { return };
        let id = self.cur.song_doc.unsaved.id();
        let place: Vec<PathBuf> =
            MediaPool::UNSAVED_ROOTS.iter().map(|pool| pool.unsaved_dir(&dirs, id)).collect();
        let writing = self.in_flight_render_paths().any(|p| place.iter().any(|d| p.starts_with(d)));
        if !writing {
            self.cur.song_doc.unsaved.release(&dirs);
        }
    }

    /// アクティブなタブの文書を別の文書で置き換える前 (Open / recovery の復元): 置き換えられる
    /// 文書の未保存の間の autosave と素材の置き場を消し、`next` (復元する recovery の置き場) が
    /// あれば差し替える。
    pub(crate) fn retire_replaced_unsaved_storage(&mut self, next: Option<UnsavedPlace>) {
        let Some(dirs) = self.ui_prefs.app_dirs.clone() else { return };
        Self::retire_unsaved_storage(&dirs, &mut self.cur.song_doc);
        if let Some(next) = next {
            self.cur.song_doc.unsaved = next;
        }
    }

    /// 文書を捨てるとき (閉じる / 終了 / 置き換え) の共通部分: `recovery/<id>.autosave.daw` を消し、
    /// 未保存の置き場を消す。autosave が先 — 間で落ちても中身の欠けた復元候補を残さない
    /// (持ち主の記録が無くなった置き場は起動時の掃除が拾う)。
    pub(crate) fn retire_unsaved_storage(dirs: &AppDirs, doc: &mut SongDoc) {
        let autosave = common::recovery::recovery_path_for(&dirs.recovery_dir(), doc.unsaved.id());
        match std::fs::remove_file(&autosave) {
            Ok(()) => tracing::info!(path = %autosave.display(), "removed session autosave"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = ?e, path = %autosave.display(), "failed to remove session autosave"),
        }
        doc.unsaved.release(dirs);
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use common::app_dirs::AppDirs;
    use common::model::{AudioSourcePath, ClipKey};
    use common::recovery::{DocId, lock_path_for, recovery_path_for};

    use crate::app_types::ImportTrackTarget;
    use crate::media_dest::MediaPool;
    use crate::state::AppData;
    use crate::test_support::headless_app_with_data_root;
    use crate::unsaved_place::{ClaimError, UnsavedPlace};

    fn write_wav(path: &Path) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..4_800 {
            w.write_sample((i % 1_000) as i16).unwrap();
        }
        w.finalize().unwrap();
    }

    fn touch(p: PathBuf) -> PathBuf {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"x").unwrap();
        p
    }

    /// アクティブなタブの (唯一の) 音源の絶対パス。
    fn source_path(app: &AppData) -> PathBuf {
        let song = app.cur.song_doc.song();
        let (_, source) = song.media.audio_sources.iter().next().expect("audio source");
        match &source.path {
            AudioSourcePath::Absolute(p) => p.clone(),
            other => panic!("expected Absolute, got {other:?}"),
        }
    }

    fn import(app: &mut AppData, wav: &Path) -> PathBuf {
        app.action_import_audio(vec![wav.to_path_buf()], ImportTrackTarget::NoHint, None);
        assert!(app.ui_ephemeral.status_message.contains("完了"), "{}", app.ui_ephemeral.status_message);
        source_path(app)
    }

    fn save(app: &mut AppData, daw: &Path) {
        std::fs::create_dir_all(daw.parent().unwrap()).unwrap();
        let epoch = app.cur.song_doc.edit_epoch();
        let snapshot = Box::new(app.cur.song_doc.song().clone());
        app.finish_save(snapshot, daw.to_path_buf(), epoch);
        assert!(daw.exists(), "saved: {}", app.ui_ephemeral.status_message);
    }

    /// 置き場のフォルダかロックファイルが 1 つでも残っているか。
    fn place_exists(dirs: &AppDirs, id: DocId) -> bool {
        MediaPool::UNSAVED_ROOTS.iter().any(|p| p.unsaved_dir(dirs, id).exists())
            || lock_path_for(&dirs.recovery_dir(), id).exists()
    }

    /// 取り込んで autosave を書いた未保存の文書を残し、プロセスが落ちたのと同じ状態にする
    /// (drop で OS のロックは外れ、ファイルは残る)。
    fn crashed_unsaved_doc(dirs: &AppDirs, wav: &Path) -> (DocId, PathBuf, PathBuf) {
        let mut app = headless_app_with_data_root(dirs.root());
        let imported = import(&mut app, wav);
        app.cur.song_doc.last_autosave = Instant::now() - Duration::from_secs(61);
        app.maybe_autosave();
        let id = app.cur.song_doc.unsaved.id();
        let autosave = recovery_path_for(&dirs.recovery_dir(), id);
        assert!(autosave.exists());
        (id, imported, autosave)
    }

    /// 2 つの未保存タブが同じ内容の素材を取り込むと、それぞれ自分の置き場に持つ。片方を保存して
    /// 自分の置き場を bundle へ移し置き場を消しても、もう片方は読めて保存もできる。
    #[test]
    fn saving_one_unsaved_tab_keeps_the_other_tabs_copy_of_the_same_media() {
        let tmp = tempfile::tempdir().unwrap();
        let wav = tmp.path().join("kick.wav");
        write_wav(&wav);
        let dirs = AppDirs::under(tmp.path().join("appdata"));
        let mut app = headless_app_with_data_root(dirs.root());
        let (a_key, a) = (app.cur.key, app.cur.song_doc.unsaved.id());
        let in_a = import(&mut app, &wav);
        app.new_tab().unwrap();
        let (b_key, b) = (app.cur.key, app.cur.song_doc.unsaved.id());
        let in_b = import(&mut app, &wav);
        assert!(in_a.starts_with(MediaPool::Samples.unsaved_dir(&dirs, a)), "{}", in_a.display());
        assert!(in_b.starts_with(MediaPool::Samples.unsaved_dir(&dirs, b)), "{}", in_b.display());

        app.switch_tab(a_key);
        let proj_a = tmp.path().join("a");
        save(&mut app, &proj_a.join("a.daw"));
        let name = in_a.file_name().unwrap();
        assert!(proj_a.join("samples").join(name).exists(), "A の bundle へ移った");
        assert!(!place_exists(&dirs, a), "A の置き場は消えた");

        app.switch_tab(b_key);
        assert!(crate::import_audio::decode_audio(&in_b).is_ok(), "B の音源はまだ読める");
        let proj_b = tmp.path().join("b");
        save(&mut app, &proj_b.join("b.daw"));
        assert!(proj_b.join("samples").join(name).exists(), "B も保存できる");
        assert!(!place_exists(&dirs, b));
    }

    /// 別の未保存タブの置き場から運んできた素材は、貼り先の置き場 (保存済みなら bundle) へ
    /// 複製してから指す。持ち主を保存せずに閉じて置き場が消えても、貼り先の音源は残る。
    #[test]
    fn media_pasted_from_another_unsaved_tab_is_copied_into_this_documents_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let wav = tmp.path().join("snare.wav");
        write_wav(&wav);
        let dirs = AppDirs::under(tmp.path().join("appdata"));
        let mut app = headless_app_with_data_root(dirs.root());
        let (a_key, a) = (app.cur.key, app.cur.song_doc.unsaved.id());
        let in_a = import(&mut app, &wav);
        let track = &app.cur.song_doc.song().tracks[0];
        let key = ClipKey { track_id: track.id, clip_id: track.clips[0].id };
        let (envelope, _, _) = app.clips_copy_envelope(&[key]).unwrap();
        let crate::clipboard::ClipboardPayload::Clips(clips) = envelope.payload else { unreachable!() };
        let paste = |app: &mut AppData| {
            let track_id = app.cur.song_doc.song().tracks[0].id;
            let n = app.paste_clips_at(clips.clone(), envelope.source_project_id, track_id, 0.0, &envelope.media);
            assert_eq!(n, 1, "{}", app.ui_ephemeral.status_message);
        };

        app.new_tab().unwrap();
        let b = app.cur.song_doc.unsaved.id();
        paste(&mut app);
        let in_b = source_path(&app);
        assert!(in_b.starts_with(MediaPool::Samples.unsaved_dir(&dirs, b)), "{}", in_b.display());

        app.new_tab().unwrap();
        let proj_c = tmp.path().join("c");
        app.cur.song_doc.file_path = Some(proj_c.join("c.daw"));
        paste(&mut app);
        let name = in_a.file_name().unwrap();
        let (_, source) = app.cur.song_doc.song().media.audio_sources.iter().next().unwrap();
        assert_eq!(source.path, AudioSourcePath::ProjectRelative(PathBuf::from("samples").join(name)));
        assert!(proj_c.join("samples").join(name).exists());

        app.close_tab_now(a_key);
        assert!(!in_a.exists() && !place_exists(&dirs, a), "閉じた A の置き場は消えた");
        assert!(crate::import_audio::decode_audio(&in_b).is_ok(), "B の音源は残る");
    }

    /// 落ちた後: recovery が残っていれば起動時の掃除は置き場を残し、復元は id ごと引き継ぐ
    /// (autosave は消さず、未保存にする)。復元した文書を閉じると autosave も置き場も消える。
    #[test]
    fn restore_adopts_the_crashed_documents_place_and_closing_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let wav = tmp.path().join("hat.wav");
        write_wav(&wav);
        let dirs = AppDirs::under(tmp.path().join("appdata"));
        let (id, imported, autosave) = crashed_unsaved_doc(&dirs, &wav);

        let mut app = headless_app_with_data_root(dirs.root());
        assert!(imported.exists(), "recovery が持ち主なので掃除しない");
        assert_eq!(app.ui_ephemeral.recovery_candidates, vec![autosave.clone()]);
        app.restore_recovery(autosave.clone());
        assert_eq!(app.cur.song_doc.unsaved.id(), id);
        assert_eq!(source_path(&app), imported);
        assert!(autosave.exists(), "復元しただけでは控えを消さない");
        assert!(app.cur.song_doc.is_dirty(), "中身は保存先に書かれていない");
        assert!(
            matches!(UnsavedPlace::adopt(&dirs, id), Err(ClaimError::Busy)),
            "別のプロセスは同じ置き場を引き継げない"
        );

        let key = app.cur.key;
        app.close_tab_now(key);
        assert!(!autosave.exists() && !imported.exists() && !place_exists(&dirs, id));
    }

    #[test]
    fn discarding_a_recovery_removes_its_place() {
        let tmp = tempfile::tempdir().unwrap();
        let wav = tmp.path().join("tom.wav");
        write_wav(&wav);
        let dirs = AppDirs::under(tmp.path().join("appdata"));
        let (id, imported, autosave) = crashed_unsaved_doc(&dirs, &wav);

        let mut app = headless_app_with_data_root(dirs.root());
        app.discard_recovery(autosave.clone());
        assert!(!autosave.exists() && !imported.exists() && !place_exists(&dirs, id));
    }

    /// 起動時の掃除: 持ち主の記録 (recovery ファイル / 別プロセスのロック) が無い置き場だけを消す。
    /// 文書ごとに分ける前の版が直下に置いたファイルと、id の形でないフォルダには触らない。
    #[test]
    fn startup_sweep_removes_only_places_without_an_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(tmp.path().join("appdata"));
        let recovery = dirs.recovery_dir();
        let orphan = DocId::new();
        let orphan_files = [
            touch(MediaPool::Samples.unsaved_dir(&dirs, orphan).join("a.wav")),
            touch(MediaPool::Bounce.unsaved_dir(&dirs, orphan).join("b.wav")),
            touch(lock_path_for(&recovery, orphan)),
            touch(lock_path_for(&recovery, DocId::new())),
        ];
        let recovered = DocId::new();
        touch(recovery_path_for(&recovery, recovered));
        let busy = DocId::new();
        let kept = [
            touch(MediaPool::Samples.unsaved_dir(&dirs, recovered).join("c.wav")),
            touch(MediaPool::Bounce.unsaved_dir(&dirs, busy).join("d.wav")),
            touch(dirs.import_cache_dir().join("legacy_12345678.wav")),
            touch(dirs.import_cache_dir().join("notes").join("e.wav")),
        ];
        let _other_process = UnsavedPlace::adopt(&dirs, busy).unwrap();

        let _app = headless_app_with_data_root(dirs.root());

        for p in &orphan_files {
            assert!(!p.exists(), "orphan left: {}", p.display());
        }
        assert!(!place_exists(&dirs, orphan));
        for p in &kept {
            assert!(p.exists(), "owned / legacy removed: {}", p.display());
        }
    }
}
