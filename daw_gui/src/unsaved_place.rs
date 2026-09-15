//! 未保存の文書の **素材の置き場** (`import_cache/<id>/` / `bounce_cache/<id>/`、
//! [`crate::media_dest::MediaPool::unsaved_dir`]) の持ち主と後始末。
//!
//! 持ち主の記録は 2 つで、どちらも [`DocId`] で名前が決まる:
//! - **起動中**: 文書の [`UnsavedPlace`] が `recovery/<id>.lock` を OS のファイルロックで握っている。
//!   ほかの daw_gui (single-instance ゲートの効かない別セッション / 非 Windows) の掃除は、ロックを
//!   取れない置き場を消さない。
//! - **落ちた後**: `recovery/<id>.autosave.daw` が残っている。復元した文書が id ごと引き継ぐ
//!   (`AppData::restore_recovery`)。
//!
//! どちらも無い置き場は誰のものでもない。消す時機は、保存して中身が bundle へ移ったとき
//! (`finish_save`)、保存せずに閉じた / 終了したとき (recovery ファイルと一緒に)、recovery を
//! 破棄したとき、起動時に持ち主の記録が無いものを見つけたとき ([`sweep_orphans`])。
//!
//! 消し方は **直接削除** (ゴミ箱を経由しない)。置き場は持ち主の autosave (recovery ファイル、
//! これも直接削除) に従属する per-user のキャッシュで、持ち主の有無という構造で消すかが決まる。
//! bundle の未参照ファイルをゴミ箱へ送る (`media_bundle::trash_files`) のは、ユーザーの
//! プロジェクトフォルダの中身を「参照されていない」という推定で消すからで、事情が違う。

use std::fs;
use std::io;
use std::path::Path;

use common::app_dirs::AppDirs;
use common::recovery::{DocId, lock_path_for, recovery_path_for};

use crate::media_dest::MediaPool;

/// 1 つの文書が持つ未保存の置き場。`SongDoc` が文書と同じ寿命で持つ。
#[derive(Debug)]
pub struct UnsavedPlace {
    id: DocId,
    /// `Some` = このプロセスが置き場へ書いた / 引き継いだ (= 中身があり得る)。drop で OS の
    /// ロックは外れるが中身は消えない — 消すのは [`UnsavedPlace::release`] だけ。
    lock: Option<PlaceLock>,
}

/// `recovery/<id>.lock` の排他ロック。ハンドルを持っていること自体がロックで、閉じると外れる。
#[derive(Debug)]
struct PlaceLock {
    _held: fs::File,
}

/// 置き場を使えなかった理由。
#[derive(Debug)]
pub enum ClaimError {
    /// 別のプロセスがこの id の置き場を使っている (同じ recovery を別の daw_gui が復元した)。
    Busy,
    Io(io::Error),
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("この未保存プロジェクトの素材置き場は別の daw_gui が使用中です"),
            Self::Io(e) => write!(f, "未保存プロジェクトの素材置き場を確保できません: {e}"),
        }
    }
}

impl From<io::Error> for ClaimError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl Default for UnsavedPlace {
    fn default() -> Self {
        Self::new()
    }
}

impl UnsavedPlace {
    /// 新しい文書の置き場 (まだ何も作らない)。
    #[must_use]
    pub fn new() -> Self {
        Self { id: DocId::new(), lock: None }
    }

    /// autosave のファイル名と置き場の名前。
    #[must_use]
    pub fn id(&self) -> DocId {
        self.id
    }

    /// 置き場へ書く前に呼ぶ: このプロセスが使っていると記録する。2 回目以降は何もしない。
    pub fn claim(&mut self, dirs: &AppDirs) -> Result<(), ClaimError> {
        if self.lock.is_none() {
            self.lock = Some(PlaceLock::acquire(dirs, self.id)?);
        }
        Ok(())
    }

    /// recovery `id` から復元する文書の置き場: 復元前に取り込んだ素材が入っている `id` の置き場を
    /// 使用中にして引き継ぐ。別のプロセスが先に引き継いでいたら [`ClaimError::Busy`]。
    pub fn adopt(dirs: &AppDirs, id: DocId) -> Result<Self, ClaimError> {
        Ok(Self { id, lock: Some(PlaceLock::acquire(dirs, id)?) })
    }

    /// 置き場を中身ごと消す。書いていなければ (= 置き場を作っていない) 何もしない。
    /// 消した後にまた書くなら [`UnsavedPlace::claim`] が作り直す。
    pub fn release(&mut self, dirs: &AppDirs) {
        if let Some(lock) = self.lock.take() {
            remove_place(dirs, self.id, lock);
        }
    }
}

impl PlaceLock {
    fn acquire(dirs: &AppDirs, id: DocId) -> Result<Self, ClaimError> {
        let dir = dirs.recovery_dir();
        fs::create_dir_all(&dir)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path_for(&dir, id))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _held: file }),
            Err(fs::TryLockError::WouldBlock) => Err(ClaimError::Busy),
            Err(fs::TryLockError::Error(e)) => Err(ClaimError::Io(e)),
        }
    }
}

/// ロックを握ったまま置き場とロックファイルを消し、最後に手放す。消せなかったもの (別の
/// スレッドがまだ開いている動画など) は残り、次の起動の [`sweep_orphans`] が拾う。
fn remove_place(dirs: &AppDirs, id: DocId, lock: PlaceLock) {
    for pool in MediaPool::UNSAVED_ROOTS {
        let dir = pool.unsaved_dir(dirs, id);
        remove_logged(&dir, fs::remove_dir_all(&dir));
    }
    let lock_file = lock_path_for(&dirs.recovery_dir(), id);
    remove_logged(&lock_file, fs::remove_file(&lock_file));
    drop(lock);
}

fn remove_logged(path: &Path, removed: io::Result<()>) {
    match removed {
        Ok(()) => tracing::info!(path = %path.display(), "removed (unsaved media place)"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "failed to remove (unsaved media place)"),
    }
}

/// どの文書も持っていない `id` の置き場を消す (recovery を破棄したとき / 起動時の掃除)。
/// recovery ファイルが残っている、または別の持ち主がロックしているなら消さずに `false`。
pub fn remove_unowned(dirs: &AppDirs, id: DocId) -> bool {
    if recovery_path_for(&dirs.recovery_dir(), id).exists() {
        return false;
    }
    match PlaceLock::acquire(dirs, id) {
        Ok(lock) => {
            remove_place(dirs, id, lock);
            true
        }
        Err(ClaimError::Busy) => false,
        Err(ClaimError::Io(e)) => {
            tracing::warn!(error = %e, %id, "cannot lock unsaved media place; leaving it");
            false
        }
    }
}

/// 起動時: 持ち主の記録 (起動中の文書 `live` / recovery ファイル / ロック) が無い置き場を消す。
/// 対象は id の名前を持つフォルダとロックファイルだけで、文書ごとに分ける前の版が直下に置いた
/// ファイルには触らない ([`AppDirs::import_cache_dir`] の doc)。
pub fn sweep_orphans(dirs: &AppDirs, live: &[DocId]) {
    let mut ids: Vec<DocId> = Vec::new();
    for pool in MediaPool::UNSAVED_ROOTS {
        ids.extend(
            entries(&pool.unsaved_root(dirs))
                .filter(|(_, is_dir)| *is_dir)
                .filter_map(|(name, _)| DocId::parse(&name)),
        );
    }
    ids.extend(
        entries(&dirs.recovery_dir())
            .filter(|(_, is_dir)| !*is_dir)
            .filter_map(|(name, _)| DocId::of_lock_file(Path::new(&name))),
    );
    ids.sort_unstable();
    ids.dedup();
    for id in ids.into_iter().filter(|id| !live.contains(id)) {
        if remove_unowned(dirs, id) {
            tracing::info!(%id, "swept orphaned unsaved media place");
        }
    }
}

/// `dir` 直下の (名前, フォルダか)。読めなければ空。
fn entries(dir: &Path) -> impl Iterator<Item = (String, bool)> {
    fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| {
        let is_dir = e.file_type().ok()?.is_dir();
        Some((e.file_name().into_string().ok()?, is_dir))
    })
}
