// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Named shared-memory region (cross-process) の RAII wrapper。
//!
//! 旧実装は `shared_memory` crate (0.12) だったが、更新停止気味の `win-sys` 0.3 経由で
//! `windows` 0.34 を引き、workspace 主力の `windows` 0.62 と重複コンパイルされていた
//! (合計 4 バージョンの windows crate が並ぶ一因)。使っていた API は
//! create / open / as_ptr / len の最小サブセットだけなので、`win_sem.rs` と同じ流儀で
//! OS API 直呼びに置き換えて依存を排除した。
//!
//! - Windows: `CreateFileMappingW(INVALID_HANDLE_VALUE, ..)` によるページファイル裏付き
//!   named section + `MapViewOfFile`。view は 64 KiB (システム割り当て粒度) 境界に
//!   align される (各 bridge 構造体の align 要件を自明に満たす)。全プロセスがハンドルを
//!   閉じるとカーネルが自動回収する。
//! - Unix (設計のみ・実機未検証): POSIX `shm_open` + `mmap`。Windows と違い名前が
//!   ファイルシステム (`/dev/shm`) に永続するため、プロセス異常終了で stale エントリが
//!   残ると次回 `create` が失敗しうる (Linux を実働対象にする際に unlink 戦略を再検討)。
//!
//! `create` は「新規作成のみ、既存名なら失敗」(旧 `ShmemConf::create` と同じ invariant)。
//! `open` は既存 region への attach + 実サイズが `min_size` 以上であることの検証まで行う
//! (旧実装で呼び出し側が毎回書いていた `ensure!(shmem.len() >= SIZE)` を内部に吸収)。
//!
//! # 呼び出し側の責務: 名前を再利用しない
//!
//! 上の「既存名なら失敗」は**緩めてはならない** (既存名に相乗りすると、他プロセスが
//! まだ旧世代として読み書きしている領域を新世代が上書きする)。裏返しとして、`create`
//! に渡す名前は **1 回のリソース生成につき 1 回限り**でなければならない —
//! 「全プロセスがハンドルを閉じるとカーネルが自動回収」= 解放は他プロセス任せで
//! **完了時刻に上限が無い**ため、再利用される id (device_id 等) を名前にすると
//! 「作成者は閉じたのに他プロセスがまだ握っていて create が失敗する」が構造的に起きる。
//! 命名契約の詳細と現行の適用状況は [`crate::plugin_ref`] の module doc を参照。

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
        SetLastError, WIN32_ERROR,
    };
    use windows::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION,
        MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, PAGE_READWRITE,
        UnmapViewOfFile, VirtualQuery,
    };
    use windows::core::HSTRING;

    pub struct NamedShmem {
        mapping: HANDLE,
        ptr: *mut u8,
        len: usize,
    }

    impl NamedShmem {
        pub fn create(name: &str, size: usize) -> Result<Self> {
            let wname = HSTRING::from(name);
            let (size_high, size_low) = (u32::try_from(size >> 32)?, size as u32);
            // CreateFileMappingW は既存名でも成功して既存ハンドルを返し、last-error に
            // ERROR_ALREADY_EXISTS を立てるだけ (MSDN)。旧 crate の「既存なら失敗」
            // invariant を保つため、事前に last-error を 0 化してから明示チェックする。
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let mapping = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE, // ページファイル裏付き (ディスクファイル無し)
                    None,
                    PAGE_READWRITE,
                    size_high,
                    size_low,
                    &wname,
                )
            }
            .with_context(|| format!("CreateFileMappingW {name}"))?;
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe {
                    let _ = CloseHandle(mapping);
                }
                anyhow::bail!("shmem {name} already exists");
            }
            match Self::map_view(mapping, name, size) {
                Ok(ptr) => Ok(Self { mapping, ptr, len: size }),
                Err(e) => {
                    unsafe {
                        let _ = CloseHandle(mapping);
                    }
                    Err(e)
                }
            }
        }

        pub fn open(name: &str, min_size: usize) -> Result<Self> {
            let wname = HSTRING::from(name);
            let mapping = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, &wname) }
                .with_context(|| format!("OpenFileMappingW {name}"))?;
            // section 全体 (bytes=0) を map し、実サイズは VirtualQuery で取る。
            let ptr = match Self::map_view(mapping, name, 0) {
                Ok(ptr) => ptr,
                Err(e) => {
                    unsafe {
                        let _ = CloseHandle(mapping);
                    }
                    return Err(e);
                }
            };
            // 以降の失敗 (VirtualQuery / min_size 不足) は Drop に view+handle の
            // 解放を負わせる。len は Drop (UnmapViewOfFile/CloseHandle) に不要なので
            // 仮値 0 で先に構築してよい。
            let mut shmem = Self { mapping, ptr, len: 0 };
            shmem.len = Self::view_len(ptr, name)?;
            anyhow::ensure!(
                shmem.len >= min_size,
                "shmem {name} too small: {} < {min_size}",
                shmem.len
            );
            Ok(shmem)
        }

        fn map_view(mapping: HANDLE, name: &str, bytes: usize) -> Result<*mut u8> {
            let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, bytes) };
            if view.Value.is_null() {
                return Err(windows::core::Error::from_thread())
                    .with_context(|| format!("MapViewOfFile {name}"));
            }
            Ok(view.Value.cast())
        }

        /// map 済み view の連続領域サイズ (page 粒度に丸め済み) を返す。
        fn view_len(ptr: *mut u8, name: &str) -> Result<usize> {
            let mut info = MEMORY_BASIC_INFORMATION::default();
            let n = unsafe {
                VirtualQuery(
                    Some(ptr.cast_const().cast()),
                    &raw mut info,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            anyhow::ensure!(n != 0, "VirtualQuery {name} failed");
            Ok(info.RegionSize)
        }

        pub fn as_ptr(&self) -> *mut u8 {
            self.ptr
        }

        pub fn len(&self) -> usize {
            self.len
        }

        pub fn is_empty(&self) -> bool {
            self.len == 0
        }
    }

    impl Drop for NamedShmem {
        fn drop(&mut self) {
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.ptr.cast(),
                });
                let _ = CloseHandle(self.mapping);
            }
        }
    }

    // 生ポインタで !Send/!Sync に落ちるだけで、named section 自体はプロセス間共有が
    // 前提のカーネルオブジェクト。中身の同期は各 bridge 構造体 (全 field atomic /
    // セマフォ handshake) が担う。旧 shared_memory crate 時代の call site 側
    // unsafe impl Send/Sync と同じ理屈。
    unsafe impl Send for NamedShmem {}
    unsafe impl Sync for NamedShmem {}
}

#[cfg(unix)]
mod imp {
    use std::ffi::CString;

    use anyhow::{Context, Result};

    pub struct NamedShmem {
        ptr: *mut u8,
        len: usize,
    }

    /// POSIX shm 名は先頭 `/` 必須 (shm_open(3))。Windows 側の呼び出し規約
    /// (プレーンな `daw_01_audio_<pid>` 等) を変えずにここで吸収する。
    fn shm_name(name: &str) -> Result<CString> {
        CString::new(format!("/{name}")).with_context(|| format!("shm name {name}"))
    }

    fn last_errno(op: &str, name: &str) -> anyhow::Error {
        anyhow::Error::new(std::io::Error::last_os_error()).context(format!("{op} {name}"))
    }

    impl NamedShmem {
        pub fn create(name: &str, size: usize) -> Result<Self> {
            let cname = shm_name(name)?;
            // O_EXCL: 既存名なら失敗 (Windows 側の ERROR_ALREADY_EXISTS チェックと同じ
            // invariant)。プロセス異常終了で /dev/shm に stale エントリが残った場合も
            // ここで失敗する (モジュール doc 参照)。
            let fd = unsafe {
                libc::shm_open(
                    cname.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                    0o600 as libc::mode_t,
                )
            };
            if fd < 0 {
                return Err(last_errno("shm_open(create)", name));
            }
            let ok = unsafe { libc::ftruncate(fd, libc::off_t::try_from(size)?) } == 0;
            if !ok {
                let e = last_errno("ftruncate", name);
                unsafe {
                    libc::close(fd);
                    libc::shm_unlink(cname.as_ptr());
                }
                return Err(e);
            }
            let ptr = Self::map(fd, size, name);
            unsafe { libc::close(fd) };
            match ptr {
                Ok(ptr) => Ok(Self { ptr, len: size }),
                Err(e) => {
                    unsafe { libc::shm_unlink(cname.as_ptr()) };
                    Err(e)
                }
            }
        }

        pub fn open(name: &str, min_size: usize) -> Result<Self> {
            let cname = shm_name(name)?;
            let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0) };
            if fd < 0 {
                return Err(last_errno("shm_open(open)", name));
            }
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &raw mut st) } != 0 {
                let e = last_errno("fstat", name);
                unsafe { libc::close(fd) };
                return Err(e);
            }
            let len = usize::try_from(st.st_size)?;
            if len < min_size {
                unsafe { libc::close(fd) };
                anyhow::bail!("shmem {name} too small: {len} < {min_size}");
            }
            let ptr = Self::map(fd, len, name);
            unsafe { libc::close(fd) };
            Ok(Self { ptr: ptr?, len })
        }

        fn map(fd: libc::c_int, len: usize, name: &str) -> Result<*mut u8> {
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                return Err(last_errno("mmap", name));
            }
            Ok(p.cast())
        }

        pub fn as_ptr(&self) -> *mut u8 {
            self.ptr
        }

        pub fn len(&self) -> usize {
            self.len
        }

        pub fn is_empty(&self) -> bool {
            self.len == 0
        }
    }

    impl Drop for NamedShmem {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.ptr.cast(), self.len);
            }
        }
    }

    unsafe impl Send for NamedShmem {}
    unsafe impl Sync for NamedShmem {}
}

pub use imp::NamedShmem;

/// create → open → 双方向書き込み可視、の同一プロセス内 round-trip。
/// (クロスプロセスの実挙動は daw_gui 実機 smoke で検証する。)
#[cfg(test)]
mod tests {
    use super::NamedShmem;

    fn unique_name(tag: &str) -> String {
        format!("daw01_shmem_test_{tag}_{}", std::process::id())
    }

    #[test]
    fn create_open_roundtrip_and_size_check() {
        let name = unique_name("rt");
        let created = NamedShmem::create(&name, 4096).unwrap();
        assert!(created.len() >= 4096);
        unsafe { created.as_ptr().write(0xAB) };

        let opened = NamedShmem::open(&name, 4096).unwrap();
        assert!(opened.len() >= 4096);
        assert_eq!(unsafe { opened.as_ptr().read() }, 0xAB);

        // open 側の書き込みが create 側から見える (双方向)
        unsafe { opened.as_ptr().add(1).write(0xCD) };
        assert_eq!(unsafe { created.as_ptr().add(1).read() }, 0xCD);
    }

    #[test]
    fn create_fails_if_already_exists() {
        let name = unique_name("dup");
        let _held = NamedShmem::create(&name, 1024).unwrap();
        assert!(NamedShmem::create(&name, 1024).is_err());
    }

    #[test]
    fn open_fails_if_too_small() {
        let name = unique_name("small");
        // 1 page (4096B) 確保 → page 丸めを超える min_size 要求は失敗する
        let _held = NamedShmem::create(&name, 1024).unwrap();
        assert!(NamedShmem::open(&name, 1024 * 1024).is_err());
    }

    #[test]
    fn open_fails_if_missing() {
        assert!(NamedShmem::open(&unique_name("missing"), 16).is_err());
    }
}
