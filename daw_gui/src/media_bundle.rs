//! project bundle (`<project_dir>/{samples,bounce,images}/`) の **自己完結性**
//! (`docs/plan_audio_clip.md` §13 Q2)。 保存フローの 3 つの責務をここに集める:
//!
//! 1. **未保存キャッシュの取り込み**: 未保存 project で import した video / image は
//!    `import_cache` に `Absolute` で置かれる。 保存時に bundle へ移して
//!    `ProjectRelative` に書き換える plan を作る (audio / bounce は
//!    [`crate::import_audio`] の同名 plan、 commit も同じ
//!    [`crate::import_audio::commit_migration`])。
//! 2. **Save As の複製**: 旧 bundle が持つ参照中ファイルを新 bundle へコピーする plan。
//!    これが無いと新しい `.daw` は `samples/...` を参照したまま実体が無く、 元フォルダを
//!    消すと開けない。
//! 3. **掃除**: bundle 内で誰も参照しないファイルを **ゴミ箱へ** 送る。 「誰も」 は
//!    live song + undo / redo 全段 + 進行中の bounce / glue の予約ファイル。 削除では
//!    なくゴミ箱なので、 判定漏れがあっても戻せる。
//!
//! plan (path 書換のみ、 I/O なし) と commit (実ファイル操作) を分けるのは
//! `import_audio` と同じ理由 — serialize が失敗したら plan を捨てるだけで無傷に戻る。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use common::model::{AudioSourcePath, ImageSourcePath, Song, VideoSourcePath};

use crate::import_audio::unsaved_import_cache_dir;

/// bundle 内でメディアを置くサブフォルダ。 掃除の対象はここだけ (直下のファイルのみ、
/// サブフォルダは辿らない)。 project file 本体・autosave sidecar・ユーザーが手で置いた
/// ものは対象外。
pub const MEDIA_SUBDIRS: [&str; 3] = ["samples", "bounce", "images"];

/// `song` が bundle 内に持つ参照を project-relative で `into` に集める。
/// 「参照」 は pool の存在ではなく **到達可能性** ([`Song::live_source_ids`]) —
/// in-memory の pool は Undo 用に参照ゼロの entry を残すので、 pool を見ると
/// 削除したクリップの音源が永遠に「使用中」 になる。 `Absolute` でも `project_dir`
/// 配下を指していれば相対化して含める (= 掃除で消さない側に倒す)。 `Generated` は
/// ディスクに実体を持たないので無視。
pub fn collect_bundle_refs(song: &Song, project_dir: &Path, into: &mut HashSet<PathBuf>) {
    let live = song.live_source_ids();
    let mut push = |rel: Option<PathBuf>| {
        if let Some(rel) = rel.filter(|r| stays_inside_bundle(r)) {
            into.insert(rel);
        }
    };
    for (id, s) in &song.media.audio_sources {
        if !live.audio.contains(id) {
            continue;
        }
        push(match &s.path {
            AudioSourcePath::ProjectRelative(rel) => Some(rel.clone()),
            AudioSourcePath::Absolute(abs) => relative_in(abs, project_dir),
            AudioSourcePath::Generated { .. } => None,
        });
    }
    for (id, s) in &song.media.video_sources {
        if !live.video.contains(id) {
            continue;
        }
        push(match &s.path {
            VideoSourcePath::ProjectRelative(rel) => Some(rel.clone()),
            VideoSourcePath::Absolute(abs) => relative_in(abs, project_dir),
        });
    }
    for (id, s) in &song.media.image_sources {
        if !live.image.contains(id) {
            continue;
        }
        push(match &s.path {
            ImageSourcePath::ProjectRelative(rel) => Some(rel.clone()),
            ImageSourcePath::Absolute(abs) => relative_in(abs, project_dir),
        });
    }
}

fn relative_in(abs: &Path, project_dir: &Path) -> Option<PathBuf> {
    abs.strip_prefix(project_dir).ok().map(Path::to_path_buf)
}

/// bundle の中を指す相対 path か (`..` / ルート / ドライブを含まない)。 `.daw` は外部入力
/// なので、 壊れた / 細工された `ProjectRelative("../..")` を Save As の複製で bundle の
/// 外へ書かせない。
fn stays_inside_bundle(rel: &Path) -> bool {
    rel.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// 未保存 project で import した video (`Absolute(import_cache/..)`) を
/// `<project_dir>/samples/` へ移す plan。 path を **その場で** `ProjectRelative` に
/// 書き換え、 実ファイルの `(cache_abs, dst_abs)` を返す (I/O なし)。
/// commit は [`crate::import_audio::commit_migration`]。
pub fn plan_unsaved_video_migration(song: &mut Song, project_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let cache_root = unsaved_import_cache_dir();
    let mut moves = Vec::new();
    for source in song.media.video_sources.values_mut() {
        let VideoSourcePath::Absolute(abs) = &source.path else { continue };
        let Some((dst, rel)) = plan_one(abs, &cache_root, project_dir, "samples") else {
            continue;
        };
        let abs = abs.clone();
        source.path = VideoSourcePath::ProjectRelative(rel);
        moves.push((abs, dst));
    }
    moves
}

/// [`plan_unsaved_video_migration`] の image 版 (→ `images/`、 保存済 project への
/// import 先 `import_image` と同じフォルダ)。
pub fn plan_unsaved_image_migration(song: &mut Song, project_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let cache_root = unsaved_import_cache_dir();
    let mut moves = Vec::new();
    for source in song.media.image_sources.values_mut() {
        let ImageSourcePath::Absolute(abs) = &source.path else { continue };
        let Some((dst, rel)) = plan_one(abs, &cache_root, project_dir, "images") else {
            continue;
        };
        let abs = abs.clone();
        source.path = ImageSourcePath::ProjectRelative(rel);
        moves.push((abs, dst));
    }
    moves
}

/// `abs` が `cache_root` 配下なら `(dst_abs, rel)` を返す。 配下でなければ `None`
/// (= 外部ファイルへの link、 触らない)。
fn plan_one(
    abs: &Path,
    cache_root: &Path,
    project_dir: &Path,
    dst_subdir: &str,
) -> Option<(PathBuf, PathBuf)> {
    if !abs.starts_with(cache_root) {
        return None;
    }
    let filename = abs.file_name()?;
    let rel = PathBuf::from(dst_subdir).join(filename);
    Some((project_dir.join(&rel), rel))
}

/// Save As: `old_dir` 側の参照ファイルを `new_dir` の同じ相対位置へ複製する plan
/// (I/O なし)。 順序は決定的 (相対 path 昇順)。
pub fn plan_relocation(
    refs: &HashSet<PathBuf>,
    old_dir: &Path,
    new_dir: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let mut rels: Vec<&PathBuf> = refs.iter().collect();
    rels.sort();
    rels.into_iter().map(|rel| (old_dir.join(rel), new_dir.join(rel))).collect()
}

/// [`plan_relocation`] の commit 結果。 一部失敗でも残りは続行するので、 件数と
/// 失敗理由を両方返す (caller が status に出す)。
#[derive(Debug, Default)]
pub struct RelocationReport {
    pub copied: usize,
    /// 旧 bundle に実体が無かった参照 (= 保存前から欠けていた。 複製のしようがない)。
    pub missing: Vec<PathBuf>,
    pub failures: Vec<String>,
}

/// 複製を実行する。 dst が既にあればスキップ (同 hash 名 = 同内容の content
/// addressing なので上書きしない)。 src が無ければ `missing` に記録して続行。
pub fn commit_relocation(copies: &[(PathBuf, PathBuf)]) -> RelocationReport {
    let mut report = RelocationReport::default();
    for (src, dst) in copies {
        if dst.exists() {
            continue;
        }
        if !src.exists() {
            report.missing.push(src.clone());
            continue;
        }
        let result = dst
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::copy(src, dst).map(|_| ()));
        match result {
            Ok(()) => report.copied += 1,
            Err(e) => report
                .failures
                .push(format!("{} → {}: {e}", src.display(), dst.display())),
        }
    }
    report
}

/// 掃除を見送った理由。
#[derive(Debug, PartialEq, Eq)]
pub enum SweepSkip {
    /// 同じフォルダに別の project file がある。 bundle = 1 project 1 フォルダが前提で、
    /// 相乗りしている他 project が何を参照しているか分からないので触らない。
    SharedFolder(PathBuf),
}

/// `project_dir` の [`MEDIA_SUBDIRS`] 直下にあって `keep` (project-relative) に無い
/// ファイルを列挙する (絶対 path、 昇順)。 `project_file` は今保存した `.daw`。
pub fn orphan_media_files(
    project_dir: &Path,
    project_file: &Path,
    keep: &HashSet<PathBuf>,
) -> Result<Vec<PathBuf>, SweepSkip> {
    if let Some(other) = sibling_project_file(project_dir, project_file) {
        return Err(SweepSkip::SharedFolder(other));
    }
    let mut orphans = Vec::new();
    for subdir in MEDIA_SUBDIRS {
        let Ok(entries) = fs::read_dir(project_dir.join(subdir)) else { continue };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let rel = PathBuf::from(subdir).join(entry.file_name());
            if !keep.contains(&rel) {
                orphans.push(entry.path());
            }
        }
    }
    orphans.sort();
    Ok(orphans)
}

/// `project_dir` 直下に `project_file` 以外の `.daw` があればそれを返す。 autosave
/// sidecar (`<file>.daw.autosave.daw`) も拡張子は `.daw` なので、 suffix で除く。
fn sibling_project_file(project_dir: &Path, project_file: &Path) -> Option<PathBuf> {
    let own = project_file.file_name()?;
    fs::read_dir(project_dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("daw"))
            && !common::recovery::is_autosave_file(p)
            && p.file_name().is_some_and(|n| n != own)
    })
}

/// ファイルを **ゴミ箱へ** 送る (削除しない)。 Windows は IFileOperation +
/// FOF_ALLOWUNDO、 Linux は freedesktop Trash spec (`trash` crate)。 呼び出し
/// スレッドで同期実行する — 1 回の shell 操作にまとまるので件数に対して速く、
/// 結果をその場で status に出せる。
pub fn trash_files(paths: &[PathBuf]) -> Result<(), trash::Error> {
    if paths.is_empty() {
        return Ok(());
    }
    trash::delete_all(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{AudioSource, ImageSource, VideoSource};
    use tempfile::tempdir;

    fn audio(path: AudioSourcePath) -> AudioSource {
        AudioSource {
            path,
            sample_rate: 48_000,
            channels: 2,
            frames: 1,
            original_bpm: None,
            root_key: None,
        }
    }

    fn video(path: VideoSourcePath) -> VideoSource {
        VideoSource {
            path,
            width: 1,
            height: 1,
            framerate: 30.0,
            duration_micros: 1,
            codec: String::new(),
            audio_source_id: None,
        }
    }

    fn image(path: ImageSourcePath) -> ImageSource {
        ImageSource { path, name: String::new(), width: 1, height: 1, format: String::new() }
    }

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }

    /// 1 track に audio / video / image の clip を 1 つずつ置き、 与えた source id を
    /// 参照させる。 pool に入れただけの source は到達不能 (= 削除済みクリップの残骸)。
    fn song_referencing(audio_ids: &[u32], video_ids: &[u32], image_ids: &[u32]) -> Song {
        use common::model::{
            AudioContent, AudioEvent, Clip, ClipContent, ImageContent, ImageEvent, Track,
            VideoContent, VideoEvent,
        };
        let mut song = Song::default();
        let mut clips = Vec::new();
        let mut add = |song: &mut Song, content: ClipContent| {
            let cid = song.alloc_content_id();
            song.clip_contents.insert(cid, content);
            let id = clips.len() as u32 + 1;
            clips.push(Clip { id, length_beats: 1.0, content_id: cid, ..Default::default() });
        };
        add(
            &mut song,
            ClipContent::Audio(AudioContent {
                events: audio_ids
                    .iter()
                    .map(|&source_id| AudioEvent { source_id, ..Default::default() })
                    .collect(),
                next_event_id: 0,
            }),
        );
        add(
            &mut song,
            ClipContent::Video(VideoContent {
                events: video_ids
                    .iter()
                    .map(|&source_id| VideoEvent { source_id, ..Default::default() })
                    .collect(),
            }),
        );
        add(
            &mut song,
            ClipContent::Image(ImageContent {
                events: image_ids
                    .iter()
                    .map(|&source_id| ImageEvent { source_id, ..Default::default() })
                    .collect(),
            }),
        );
        song.tracks = vec![Track { id: 1, clips, next_clip_id: 4, ..Track::default() }];
        song
    }

    #[test]
    fn refs_cover_reachable_sources_across_pools_and_absolute_inside_bundle() {
        let dir = tempdir().unwrap();
        let mut song = song_referencing(&[1, 2, 3], &[1], &[1]);
        song.media
            .audio_sources
            .insert(1, audio(AudioSourcePath::ProjectRelative("samples/a.wav".into())));
        song.media.audio_sources.insert(
            2,
            audio(AudioSourcePath::Absolute(dir.path().join("bounce").join("b.wav"))),
        );
        song.media
            .audio_sources
            .insert(3, audio(AudioSourcePath::Absolute("C:/elsewhere/c.wav".into())));
        // pool にはあるがどの clip も参照しない (= 削除済みクリップの残骸、 Undo 用に残る)。
        song.media
            .audio_sources
            .insert(4, audio(AudioSourcePath::ProjectRelative("samples/deleted.wav".into())));
        song.media
            .video_sources
            .insert(1, video(VideoSourcePath::ProjectRelative("samples/v.mp4".into())));
        song.media
            .image_sources
            .insert(1, image(ImageSourcePath::ProjectRelative("images/i.png".into())));
        // 口パク slot からの直接参照 (event には出ていない)。
        song.media
            .image_sources
            .insert(2, image(ImageSourcePath::ProjectRelative("images/mouth_a.png".into())));
        song.tracks[0].mouth_map =
            Some(common::model::MouthMap { a: 2, ..Default::default() });
        let mut refs = HashSet::new();
        collect_bundle_refs(&song, dir.path(), &mut refs);
        let mut got: Vec<_> = refs.into_iter().collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                PathBuf::from("bounce/b.wav"),
                PathBuf::from("images/i.png"),
                PathBuf::from("images/mouth_a.png"),
                PathBuf::from("samples/a.wav"),
                PathBuf::from("samples/v.mp4"),
            ]
        );
    }

    /// `..` を含む参照は bundle の外を指すので、 複製の対象 (= 書き先) にしない。
    #[test]
    fn refs_outside_the_bundle_are_ignored() {
        let dir = tempdir().unwrap();
        let mut song = song_referencing(&[1, 2], &[], &[]);
        song.media
            .audio_sources
            .insert(1, audio(AudioSourcePath::ProjectRelative("../../etc/x.wav".into())));
        song.media
            .audio_sources
            .insert(2, audio(AudioSourcePath::ProjectRelative("samples/ok.wav".into())));
        let mut refs = HashSet::new();
        collect_bundle_refs(&song, dir.path(), &mut refs);
        assert_eq!(refs.into_iter().collect::<Vec<_>>(), vec![PathBuf::from("samples/ok.wav")]);
    }

    #[test]
    fn orphans_are_unreferenced_files_in_media_subdirs_only() {
        let dir = tempdir().unwrap();
        let daw = dir.path().join("p.daw");
        touch(&daw);
        touch(&dir.path().join("samples/keep.wav"));
        touch(&dir.path().join("samples/orphan.wav"));
        touch(&dir.path().join("bounce/orphan2.wav"));
        touch(&dir.path().join("images/keep.png"));
        touch(&dir.path().join("samples/sub/nested.wav")); // サブフォルダは辿らない
        touch(&dir.path().join("notes.txt")); // 直下は対象外
        let keep: HashSet<PathBuf> =
            ["samples/keep.wav", "images/keep.png"].iter().map(PathBuf::from).collect();
        let got = orphan_media_files(dir.path(), &daw, &keep).unwrap();
        assert_eq!(
            got,
            vec![
                dir.path().join("bounce").join("orphan2.wav"),
                dir.path().join("samples").join("orphan.wav"),
            ]
        );
    }

    #[test]
    fn sweep_is_skipped_when_another_project_shares_the_folder() {
        let dir = tempdir().unwrap();
        let daw = dir.path().join("p.daw");
        touch(&daw);
        touch(&dir.path().join("other.daw"));
        touch(&dir.path().join("samples/x.wav"));
        let got = orphan_media_files(dir.path(), &daw, &HashSet::new());
        assert_eq!(got, Err(SweepSkip::SharedFolder(dir.path().join("other.daw"))));
    }

    #[test]
    fn autosave_sidecar_is_not_a_sibling_project() {
        let dir = tempdir().unwrap();
        let daw = dir.path().join("p.daw");
        touch(&daw);
        touch(&common::recovery::sidecar_for(&daw));
        touch(&dir.path().join("samples/x.wav"));
        let got = orphan_media_files(dir.path(), &daw, &HashSet::new()).unwrap();
        assert_eq!(got, vec![dir.path().join("samples").join("x.wav")]);
    }

    #[test]
    fn relocation_copies_missing_files_and_reports_absent_sources() {
        let old = tempdir().unwrap();
        let new = tempdir().unwrap();
        touch(&old.path().join("samples/a.wav"));
        touch(&old.path().join("images/i.png"));
        touch(&new.path().join("images/i.png")); // 既存はスキップ
        let refs: HashSet<PathBuf> = ["samples/a.wav", "images/i.png", "bounce/gone.wav"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let plan = plan_relocation(&refs, old.path(), new.path());
        assert_eq!(plan.len(), 3);
        let report = commit_relocation(&plan);
        assert_eq!(report.copied, 1);
        assert_eq!(report.missing, vec![old.path().join("bounce/gone.wav")]);
        assert!(report.failures.is_empty());
        assert!(new.path().join("samples/a.wav").exists());
    }

    #[test]
    fn unsaved_video_and_image_plans_rewrite_paths_without_io() {
        let proj = tempdir().unwrap();
        let cache = unsaved_import_cache_dir();
        let mut song = Song::default();
        song.media.video_sources.insert(
            1,
            video(VideoSourcePath::Absolute(cache.join("v_abcd1234.mp4"))),
        );
        song.media.image_sources.insert(
            1,
            image(ImageSourcePath::Absolute(cache.join("i_abcd1234.png"))),
        );
        song.media.image_sources.insert(
            2,
            image(ImageSourcePath::Absolute("C:/elsewhere/linked.png".into())),
        );
        let v = plan_unsaved_video_migration(&mut song, proj.path());
        let i = plan_unsaved_image_migration(&mut song, proj.path());
        assert_eq!(
            v,
            vec![(cache.join("v_abcd1234.mp4"), proj.path().join("samples").join("v_abcd1234.mp4"))]
        );
        assert_eq!(
            i,
            vec![(cache.join("i_abcd1234.png"), proj.path().join("images").join("i_abcd1234.png"))]
        );
        assert_eq!(
            song.media.video_sources[&1].path,
            VideoSourcePath::ProjectRelative(PathBuf::from("samples").join("v_abcd1234.mp4"))
        );
        assert_eq!(
            song.media.image_sources[&1].path,
            ImageSourcePath::ProjectRelative(PathBuf::from("images").join("i_abcd1234.png"))
        );
        // 外部 link は触らない。
        assert!(matches!(song.media.image_sources[&2].path, ImageSourcePath::Absolute(_)));
        assert!(!proj.path().join("samples").exists(), "plan must not touch the filesystem");
    }
}
