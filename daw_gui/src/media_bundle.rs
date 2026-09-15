//! project bundle (`<project_dir>/{samples,bounce,images}/`) の **自己完結性**
//! (`docs/plan_audio_clip.md` §13 Q2)。 保存フローの 3 つの責務をここに集める:
//!
//! 1. **未保存キャッシュの取り込み**: 未保存 project で import / bounce した媒体は
//!    その文書の置き場 ([`crate::media_dest`]) に `Absolute` で置かれる。 保存時に bundle へ
//!    運んで `ProjectRelative` に書き換える ([`plan_unsaved_migration`] → [`commit_transfers`])。
//!    自分の置き場のものは移し、別の文書の置き場のもの (文書ごとに分ける前の共有の置き場を含む)
//!    は複製する。
//! 2. **Save As の複製**: 旧 bundle が持つ参照中ファイルを新 bundle へコピーする plan。
//!    これが無いと新しい `.daw` は `samples/...` を参照したまま実体が無く、 元フォルダを
//!    消すと開けない。
//! 3. **掃除**: bundle 内で誰も参照しないファイルを **ゴミ箱へ** 送る。 「誰も」 は
//!    live song + undo / redo 全段 + 進行中の bounce / glue の予約ファイル。 削除では
//!    なくゴミ箱なので、 判定漏れがあっても戻せる。
//!
//! plan (path 書換のみ、 I/O なし) と commit (実ファイル操作) を分けるのは、 serialize が失敗したら
//! plan を捨てるだけで無傷に戻るから。 書き出した後の live / 履歴は、 運べたものだけを書き換える
//! ([`migrate_unsaved`] — 運べなかったものは置き場を指したまま鳴り続ける)。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use common::app_dirs::AppDirs;
use common::atomic_file::Published;
use common::model::{AudioSourcePath, ImageSourcePath, Song, VideoSourcePath};
use common::recovery::DocId;

use crate::media_dest::{MediaPool, TransferMode};

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

/// 保存時に未保存の置き場から bundle へ運ぶ 1 ファイル。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaTransfer {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub mode: TransferMode,
}

/// bundle へ運べなかった 1 件。元 (`src`) は未保存の置き場に残っている。
#[derive(Debug)]
pub struct TransferFailure {
    pub src: PathBuf,
    pub message: String,
}

impl TransferFailure {
    fn new(t: &MediaTransfer, e: &std::io::Error) -> Self {
        Self { src: t.src.clone(), message: format!("{} → {}: {e}", t.src.display(), t.dst.display()) }
    }
}

/// 未保存の置き場を指す `song` の媒体 (audio → `samples/`、 Bounce の出力 → `bounce/`、
/// video → `samples/`、 image → `images/`) を `project_dir` の bundle 相対へ **その場で** 書き換え、
/// 実ファイルの運び方を返す (I/O なし)。 自分 (`own`) の置き場のものは移し、 別の文書の置き場の
/// ものは複製する ([`MediaPool::transfer_mode`])。 置き場の外 (bundle / 外部ファイルへの link) は
/// 触らない。
///
/// path の書き換え (Song を捨てれば戻る) と実ファイル操作 (戻せない) を分けるので、 保存は
/// project file を先に書き、 成功してから [`commit_transfers`] できる — 書き出しに失敗しても
/// 置き場のファイルは半端に動かない。
pub fn plan_unsaved_migration(
    song: &mut Song,
    project_dir: &Path,
    dirs: &AppDirs,
    own: DocId,
) -> Vec<MediaTransfer> {
    let mut plan = Vec::new();
    rewrite_unsaved_sources(song, project_dir, dirs, own, |t| {
        plan.push(t);
        true
    });
    plan
}

/// project file を書き出した **後** に、 live / 履歴の 1 曲が未保存の置き場を指す媒体を bundle へ運び、
/// **運べたものだけ** bundle 相対へ書き換える。 運べなかったもの (と、 この保存で既に運べなかった
/// `failures` の元 — 試し直さない) は置き場を指したまま残す: 実体は置き場に残っているので鳴り続け、
/// 次の保存でやり直せる。 失敗は `failures` に足す。
///
/// `keep_sources` = 自分の置き場の実体も移さずに複製する (置き場を指したまま書き換えられない参照が
/// 残るとき — 移すとその参照の実体が消える)。
pub fn migrate_unsaved(
    song: &mut Song,
    project_dir: &Path,
    dirs: &AppDirs,
    own: DocId,
    keep_sources: bool,
    failures: &mut Vec<TransferFailure>,
) {
    rewrite_unsaved_sources(song, project_dir, dirs, own, |t| {
        if failures.iter().any(|f| f.src == t.src) {
            return false;
        }
        match commit_one(&t, keep_sources) {
            Ok(()) => true,
            Err(e) => {
                failures.push(TransferFailure::new(&t, &e));
                false
            }
        }
    });
}

/// 未保存の置き場を指す `song` の媒体を 1 件ずつ `transfer` に渡し、 `true` を返したものを bundle 相対へ
/// 書き換える ([`plan_unsaved_migration`] / [`migrate_unsaved`] の走査の SSoT)。
fn rewrite_unsaved_sources(
    song: &mut Song,
    project_dir: &Path,
    dirs: &AppDirs,
    own: DocId,
    mut transfer: impl FnMut(MediaTransfer) -> bool,
) {
    for source in song.media.audio_sources.values_mut() {
        let AudioSourcePath::Absolute(abs) = &source.path else { continue };
        let Some((t, rel)) = MediaPool::of_unsaved_audio(abs, dirs)
            .and_then(|pool| plan_cache_transfer(abs, pool, dirs, own, project_dir))
        else {
            continue;
        };
        if transfer(t) {
            source.path = AudioSourcePath::ProjectRelative(rel);
        }
    }
    for source in song.media.video_sources.values_mut() {
        let VideoSourcePath::Absolute(abs) = &source.path else { continue };
        let Some((t, rel)) = plan_cache_transfer(abs, MediaPool::Samples, dirs, own, project_dir)
        else {
            continue;
        };
        if transfer(t) {
            source.path = VideoSourcePath::ProjectRelative(rel);
        }
    }
    for source in song.media.image_sources.values_mut() {
        let ImageSourcePath::Absolute(abs) = &source.path else { continue };
        let Some((t, rel)) = plan_cache_transfer(abs, MediaPool::Images, dirs, own, project_dir)
        else {
            continue;
        };
        if transfer(t) {
            source.path = ImageSourcePath::ProjectRelative(rel);
        }
    }
}

/// `song` の媒体が `dirs` (フォルダ) の中のファイルを絶対パスで指しているか (到達可能性ではなく pool 全体 —
/// Undo で戻る参照も実体が要る)。
pub fn references_inside(song: &Song, dirs: &[PathBuf]) -> bool {
    let inside = |p: &Path| dirs.iter().any(|d| p.starts_with(d));
    song.media.audio_sources.values().any(|s| matches!(&s.path, AudioSourcePath::Absolute(p) if inside(p)))
        || song.media.video_sources.values().any(|s| matches!(&s.path, VideoSourcePath::Absolute(p) if inside(p)))
        || song.media.image_sources.values().any(|s| matches!(&s.path, ImageSourcePath::Absolute(p) if inside(p)))
}

/// `abs` が `pool` の未保存の置き場にあれば、 bundle へ運ぶ手順と Song に記録する相対パス。
fn plan_cache_transfer(
    abs: &Path,
    pool: MediaPool,
    dirs: &AppDirs,
    own: DocId,
    project_dir: &Path,
) -> Option<(MediaTransfer, PathBuf)> {
    let mode = pool.transfer_mode(abs, dirs, own)?;
    let rel = PathBuf::from(pool.bundle_subdir()).join(abs.file_name()?);
    Some((MediaTransfer { src: abs.to_path_buf(), dst: project_dir.join(&rel), mode }, rel))
}

/// [`plan_unsaved_migration`] の実ファイル操作。 project file の書き出しが **成功してから** 呼ぶ。
/// 冪等: `dst` が既にあれば (同じ名前 = 同じ内容、 または同じ保存の先の plan が運び済み)、 移す側は
/// 置き場のファイルを捨て、 複製する側は何もしない。 1 件失敗しても残りは続ける (1 つの壊れた
/// ファイルが後ろの全部を置き場に取り残さない)。 `keep_sources` は [`migrate_unsaved`] と同じ。
/// 戻り値は運べなかったもの (元は置き場に残る)。
pub fn commit_transfers(plan: &[MediaTransfer], keep_sources: bool) -> Vec<TransferFailure> {
    plan.iter()
        .filter_map(|t| commit_one(t, keep_sources).err().map(|e| TransferFailure::new(t, &e)))
        .collect()
}

fn commit_one(t: &MediaTransfer, keep_source: bool) -> std::io::Result<()> {
    if let Some(dir) = t.dst.parent() {
        fs::create_dir_all(dir)?;
    }
    match t.mode {
        TransferMode::Move if keep_source => {
            common::atomic_file::copy_new(&t.src, &t.dst)?;
        }
        TransferMode::Move if t.dst.exists() => {
            let _ = fs::remove_file(&t.src);
        }
        // 同じボリューム内の rename は atomic。 失敗 (別ボリューム / 開いている読み手) は複製して
        // 元を消す。 複製は書きかけを `dst` に出さない — 途中で落ちた `dst` を次の保存が上の
        // `exists` で「移行済み」と見なすと、 置き場の原本を消して完成品がどこにも残らない。
        TransferMode::Move => {
            if fs::rename(&t.src, &t.dst).is_err() {
                common::atomic_file::copy_new(&t.src, &t.dst)?;
                let _ = fs::remove_file(&t.src);
            }
        }
        TransferMode::Copy => {
            common::atomic_file::copy_new(&t.src, &t.dst)?;
        }
    }
    Ok(())
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
/// addressing なので上書きしない)。 だから書きかけを dst に出さない — 複製中に落ちた
/// dst は次の Save As でも「複製済み」と見なされ、壊れたまま残る ([`common::atomic_file`])。
/// src が無ければ `missing` に記録して続行。
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
            .and_then(|()| common::atomic_file::copy_new(src, dst));
        match result {
            Ok(Published::Written) => report.copied += 1,
            Ok(Published::AlreadyPresent) => {}
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

    /// plan は全プールの path を bundle 相対へ書き換えるが **ファイルを動かさない** (保存は
    /// project file の書き出しが成功してから commit する)。自分の置き場は移す、別の文書の置き場と
    /// 文書ごとに分ける前の共有の置き場は複製する、外部 link は触らない。
    #[test]
    fn unsaved_plan_moves_own_place_copies_foreign_places_and_leaves_links() {
        let proj = tempdir().unwrap();
        let data = tempdir().unwrap();
        let dirs = AppDirs::under(data.path());
        let (own, other) = (DocId::new(), DocId::new());
        let mine = MediaPool::Samples.unsaved_dir(&dirs, own);
        let theirs = MediaPool::Samples.unsaved_dir(&dirs, other);
        let legacy = dirs.import_cache_dir();
        let bounce = MediaPool::Bounce.unsaved_dir(&dirs, own);
        let mut song = Song::default();
        song.media.audio_sources.insert(1, audio(AudioSourcePath::Absolute(mine.join("a_1.wav"))));
        song.media.audio_sources.insert(2, audio(AudioSourcePath::Absolute(theirs.join("b_2.wav"))));
        song.media.audio_sources.insert(3, audio(AudioSourcePath::Absolute(bounce.join("c_fx.wav"))));
        song.media.video_sources.insert(1, video(VideoSourcePath::Absolute(legacy.join("v_3.mp4"))));
        song.media.image_sources.insert(1, image(ImageSourcePath::Absolute(mine.join("i_4.png"))));
        song.media
            .image_sources
            .insert(2, image(ImageSourcePath::Absolute("C:/elsewhere/linked.png".into())));

        let mut plan = plan_unsaved_migration(&mut song, proj.path(), &dirs, own);
        plan.sort_by(|a, b| a.dst.cmp(&b.dst));
        let p = proj.path();
        let t = |src: PathBuf, dst: PathBuf, mode| MediaTransfer { src, dst, mode };
        assert_eq!(
            plan,
            vec![
                t(bounce.join("c_fx.wav"), p.join("bounce").join("c_fx.wav"), TransferMode::Move),
                t(mine.join("i_4.png"), p.join("images").join("i_4.png"), TransferMode::Move),
                t(mine.join("a_1.wav"), p.join("samples").join("a_1.wav"), TransferMode::Move),
                t(theirs.join("b_2.wav"), p.join("samples").join("b_2.wav"), TransferMode::Copy),
                t(legacy.join("v_3.mp4"), p.join("samples").join("v_3.mp4"), TransferMode::Copy),
            ]
        );
        assert_eq!(
            song.media.audio_sources[&2].path,
            AudioSourcePath::ProjectRelative(PathBuf::from("samples").join("b_2.wav"))
        );
        assert_eq!(
            song.media.video_sources[&1].path,
            VideoSourcePath::ProjectRelative(PathBuf::from("samples").join("v_3.mp4"))
        );
        assert!(matches!(song.media.image_sources[&2].path, ImageSourcePath::Absolute(_)));
        assert!(!p.join("samples").exists(), "plan must not touch the filesystem");
    }

    /// commit: 移す側は置き場から消え、複製する側は持ち主の置き場に残る。同じ保存の 2 回目
    /// (live / 履歴の migration) は運び済みなので何も壊さない。1 件の失敗で残りを止めない。
    #[test]
    fn commit_moves_copies_is_idempotent_and_continues_past_failures() {
        let data = tempdir().unwrap();
        let proj = tempdir().unwrap();
        let (mine, theirs) = (data.path().join("mine"), data.path().join("theirs"));
        touch(&mine.join("a.wav"));
        touch(&theirs.join("b.wav"));
        let p = proj.path().join("samples");
        let plan = vec![
            MediaTransfer { src: mine.join("gone.wav"), dst: p.join("gone.wav"), mode: TransferMode::Move },
            MediaTransfer { src: mine.join("a.wav"), dst: p.join("a.wav"), mode: TransferMode::Move },
            MediaTransfer { src: theirs.join("b.wav"), dst: p.join("b.wav"), mode: TransferMode::Copy },
        ];
        let failures = commit_transfers(&plan, false);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].src, mine.join("gone.wav"));
        assert!(!mine.join("a.wav").exists() && p.join("a.wav").exists(), "own place: moved");
        assert!(theirs.join("b.wav").exists() && p.join("b.wav").exists(), "foreign place: copied");

        assert!(commit_transfers(&plan[1..], false).is_empty(), "second pass is a no-op");
        assert!(p.join("a.wav").exists() && theirs.join("b.wav").exists());

        // 置き場を指したまま書き換えられない参照が残るときは、自分の置き場の実体も移さない。
        let kept = MediaTransfer { src: mine.join("c.wav"), dst: p.join("c.wav"), mode: TransferMode::Move };
        touch(&kept.src);
        assert!(commit_transfers(std::slice::from_ref(&kept), true).is_empty());
        assert!(kept.src.exists() && kept.dst.exists(), "keep_sources: copied, not moved");
    }
}
