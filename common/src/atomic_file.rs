//! 最終名に **書きかけを決して見せない** ファイル書き込みの SSoT。
//!
//! 同じ名前を複数の書き手が狙い (同じ内容を取り込む別プロセス / 別タブ、同じキーを合成する
//! 2 プロセス)、読み手がその名前を開く置き場で最終名へ直接書くと、読み手が書きかけを掴む —
//! `File::create` の truncate 直後の 0 byte や、長さ欄が 0 のままの WAV ヘッダ
//! (symphonia が `wav: missing data chunk` を返す)。さらに「無ければ書く」で重複排除して
//! いる置き場では、書き込み途中でプロセスが落ちると **壊れたファイルが完成品として以後ずっと
//! 再利用される**。
//!
//! ここを通すと、同じフォルダの一時ファイルに書き切って `sync_all` してから rename で公開する。
//! rename は同一ボリューム内で atomic なので、最終名には「無い」か「完成品」しか存在しない。
//!
//! - [`PendingFile::publish_new`] — 既にあれば置き換えない (first-writer-wins)。
//!   名前が内容で決まる置き場 (内容 hash 付きの取り込み先など) 用。
//! - [`PendingFile::publish_replace`] — 置き換える (last-writer-wins)。保存ファイルや、
//!   壊れていると分かったエントリの差し替え用。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 一時ファイル名の末尾。拡張子がこれになるので、`.wav` / `.json` を数える走査に拾われない。
pub const PARTIAL_SUFFIX: &str = ".partial";

/// 同じプロセス内で一時ファイル名を重ねないための連番 (プロセス間は pid で分ける)。
static SEQ: AtomicU64 = AtomicU64::new(0);

/// 公開の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// この呼び出しが書いたものが最終名になった。
    Written,
    /// 最終名に既に完成品があった (書かなかった、または並行した書き手が先に公開した)。
    AlreadyPresent,
}

/// 最終名 `dst` に対する書きかけ。[`PendingFile::path`] に書き切ってから `publish_*` で
/// 公開する。公開せずに drop すると一時ファイルを消す (途中で `?` で抜けても残骸を置かない)。
/// 置き場のフォルダは呼び出し側が作っておく。
#[must_use = "publish しないと書いた内容は捨てられる"]
pub struct PendingFile {
    tmp: PathBuf,
    dst: PathBuf,
}

impl PendingFile {
    pub fn new(dst: impl Into<PathBuf>) -> Self {
        let dst = dst.into();
        let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = dst.with_file_name(format!(".{name}.{pid}.{seq}{PARTIAL_SUFFIX}"));
        Self { tmp, dst }
    }

    /// 書き込み先 (最終名と同じフォルダの一時ファイル)。
    pub fn path(&self) -> &Path {
        &self.tmp
    }

    /// 最終名が無ければ公開する。既にあれば置き換えず [`Published::AlreadyPresent`]
    /// (書いた一時ファイルは drop で消える)。
    pub fn publish_new(self) -> io::Result<Published> {
        sync(&self.tmp)?;
        match rename_no_replace(&self.tmp, &self.dst) {
            Ok(()) => Ok(Published::Written),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(Published::AlreadyPresent),
            Err(e) => Err(e),
        }
    }

    /// 最終名を置き換えて公開する。`FILE_SHARE_DELETE` 付きで開いている読み手 (Rust の `File` は
    /// 既定でそう開く) は置き換え前の内容を読み切れる — `std::fs::rename` は置き換え先が開かれて
    /// いて `MoveFileExW` が拒むと、POSIX semantics の rename (`FileRenameInfoEx`) へ倒す。
    pub fn publish_replace(self) -> io::Result<()> {
        sync(&self.tmp)?;
        std::fs::rename(&self.tmp, &self.dst)
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        // 公開済みなら一時ファイルはもう無い (NotFound)。どちらでも結果は見ない。
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// `src` を `dst` へ複製する。`dst` が既にあれば何もしない (名前が内容で決まる置き場用)。
pub fn copy_new(src: &Path, dst: &Path) -> io::Result<Published> {
    if dst.exists() {
        return Ok(Published::AlreadyPresent);
    }
    let pending = PendingFile::new(dst);
    std::fs::copy(src, pending.path())?;
    pending.publish_new()
}

/// `bytes` を `dst` として書く。`dst` が既にあれば何もしない (名前が内容で決まる置き場用)。
pub fn write_new(dst: &Path, bytes: &[u8]) -> io::Result<Published> {
    if dst.exists() {
        return Ok(Published::AlreadyPresent);
    }
    let pending = PendingFile::new(dst);
    std::fs::write(pending.path(), bytes)?;
    pending.publish_new()
}

/// `bytes` で `dst` を置き換える。
pub fn write_replace(dst: &Path, bytes: &[u8]) -> io::Result<()> {
    let pending = PendingFile::new(dst);
    std::fs::write(pending.path(), bytes)?;
    pending.publish_replace()
}

/// 書いた内容をディスクへ落としてから公開する。rename だけが先に永続化されると、電源断の
/// 後に「完成品の名前で中身が欠けたファイル」が残り得る。
fn sync(path: &Path) -> io::Result<()> {
    std::fs::File::options().write(true).open(path)?.sync_all()
}

/// 置き換えない rename。既にあれば `ErrorKind::AlreadyExists` で失敗する (判定と rename の
/// 間に隙間が無い)。
#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
    use windows::core::HSTRING;
    // フラグ 0 = MOVEFILE_REPLACE_EXISTING 無し。既にあれば ERROR_ALREADY_EXISTS
    // (https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-movefileexw)。
    // FAT / exFAT でも使える (hard link は使えない)。
    // SAFETY: HSTRING は呼び出しの間生きている NUL 終端の UTF-16。
    unsafe { MoveFileExW(&HSTRING::from(from), &HSTRING::from(to), MOVE_FILE_FLAGS(0)) }.map_err(
        |e| {
            // windows crate の `From<Error> for io::Error` は HRESULT をそのまま raw os error に
            // するので `kind()` が分類されない。FACILITY_WIN32 は元の Win32 コードへ戻す。
            let hr = e.code().0 as u32;
            let raw = if (hr & 0xFFFF_0000) == 0x8007_0000 { hr & 0xFFFF } else { hr };
            io::Error::from_raw_os_error(raw as i32)
        },
    )
}

/// POSIX の `link` は既にあれば `EEXIST` で失敗する (atomic)。成功したら一時ファイルの
/// 名前は `PendingFile` の drop が外す。
#[cfg(not(windows))]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::hard_link(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn partials(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(PARTIAL_SUFFIX))
            .collect()
    }

    /// first-writer-wins: 後から来た書き手は置き換えず、先の内容が残る。一時ファイルも残らない。
    #[test]
    fn publish_new_keeps_the_first_writer() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("a.wav");
        assert_eq!(write_new(&dst, b"first").unwrap(), Published::Written);

        // `exists` の早期 return を通らない経路 (= 判定の後に先を越された書き手) を直接叩く。
        let late = PendingFile::new(&dst);
        std::fs::write(late.path(), b"second").unwrap();
        assert_eq!(late.publish_new().unwrap(), Published::AlreadyPresent);

        assert_eq!(std::fs::read(&dst).unwrap(), b"first");
        assert!(partials(dir.path()).is_empty(), "一時ファイルを残さない");
    }

    /// 書き手が途中で失敗して publish しなかったら、最終名には何も現れず一時ファイルも消える。
    #[test]
    fn dropped_pending_file_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("x.wav");
        {
            let pending = PendingFile::new(&dst);
            std::fs::write(pending.path(), b"half").unwrap();
        }
        assert!(!dst.exists());
        assert!(partials(dir.path()).is_empty());
    }
}
