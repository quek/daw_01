//! インストール済み ASIO ドライバーの列挙 (registry を読むだけで、ドライバーは load しない)。
//!
//! 置き場所と読み方は SDK の `host/pc/asiolist.cpp` に合わせる:
//! `HKLM\software\asio\<key>` の `clsid` が COM の class id、`description` が表示名
//! (無ければ key 名。 `asiolist.cpp:51-54, 114-136, 188-190`)。 SDK と同じく、
//! `HKCR\clsid\<clsid>\InprocServer32` の DLL が実在しないもの (アンインストールの残骸) は
//! 載せない (`asiolist.cpp:59-100` の `findDrvPath`)。
//!
//! 64 bit プロセスが見るのは 64 bit 側の view だけ。 32 bit 専用ドライバー
//! (`WOW6432Node` 側) は列挙されないし、load もできない。

use std::path::Path;

use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Com::CLSIDFromString;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
use windows::core::{GUID, PCWSTR, PWSTR, w};

/// registry に登録された ASIO ドライバー 1 つ。
#[derive(Clone, Debug)]
pub struct DriverEntry {
    /// `HKLM\software\asio` 直下の key 名。
    pub key: String,
    /// 表示名 (`description`。 無ければ key 名)。
    pub name: String,
    /// COM の class id。 **永続する選択の id に使うならこれ** (表示名はドライバーの更新で変わりうる)。
    pub clsid: GUID,
    /// `InprocServer32` の DLL。
    pub dll_path: String,
}

/// registry の値 1 つを読むバッファ長 (UTF-16 単位)。 key 名 / CLSID / パスに十分。
const VALUE_CAP: usize = 1024;
/// 列挙の上限 (壊れた registry で回り続けないための歯止め)。
const MAX_DRIVERS: u32 = 256;

/// 開いた registry key (drop で閉じる)。
struct Key(HKEY);

impl Key {
    fn open(parent: HKEY, path: PCWSTR) -> Option<Self> {
        let mut key = HKEY::default();
        // SAFETY: `path` は NUL 終端の UTF-16、`key` は有効な出力先。
        let status = unsafe { RegOpenKeyExW(parent, path, None, KEY_READ, &raw mut key) };
        (status == ERROR_SUCCESS).then_some(Self(key))
    }

    /// `subkey` (NUL 終端の UTF-16) の下の文字列値を読む。 `value` が null なら既定値。
    fn string(&self, subkey: &[u16], value: PCWSTR) -> Option<String> {
        debug_assert_eq!(subkey.last(), Some(&0));
        let mut buf = [0u16; VALUE_CAP];
        let mut bytes = u32::try_from(size_of_val(&buf)).ok()?;
        // SAFETY: `buf` は `bytes` byte ぶん書ける。 型の制限で文字列以外を弾き (`REG_EXPAND_SZ` は
        // 展開して返る)、収まらなければ `ERROR_MORE_DATA` で失敗する (はみ出して書かない)。
        // 結果は NUL 終端。
        let status: WIN32_ERROR = unsafe {
            RegGetValueW(
                self.0,
                PCWSTR(subkey.as_ptr()),
                value,
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&raw mut bytes),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    /// `index` 番目の subkey 名 (NUL 終端つきの UTF-16)。 尽きたら `None`。
    fn subkey(&self, index: u32) -> Option<Vec<u16>> {
        let mut buf = [0u16; VALUE_CAP];
        let mut len = u32::try_from(buf.len()).ok()?;
        // SAFETY: `buf` は `len` 文字ぶん書ける。 戻りの `len` は NUL を含まない文字数。
        let status = unsafe {
            RegEnumKeyExW(self.0, index, Some(PWSTR(buf.as_mut_ptr())), &raw mut len, None, None, None, None)
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let len = usize::try_from(len).ok()?.min(buf.len() - 1);
        let mut name = buf[..len].to_vec();
        name.push(0);
        Some(name)
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: `open` が返した key を 1 回だけ閉じる。
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// インストール済みドライバーを registry の並び順で返す。 ASIO が 1 つも無ければ空。
pub fn enumerate() -> Vec<DriverEntry> {
    let Some(root) = Key::open(HKEY_LOCAL_MACHINE, w!("software\\asio")) else {
        return Vec::new();
    };
    let classes = Key::open(HKEY_CLASSES_ROOT, w!("clsid"));
    let mut drivers = Vec::new();
    for index in 0..MAX_DRIVERS {
        let Some(subkey) = root.subkey(index) else { break };
        if let Some(entry) = read_entry(&root, classes.as_ref(), &subkey) {
            drivers.push(entry);
        }
    }
    drivers
}

fn read_entry(root: &Key, classes: Option<&Key>, subkey: &[u16]) -> Option<DriverEntry> {
    let key = String::from_utf16_lossy(&subkey[..subkey.len() - 1]);
    let clsid_text = root.string(subkey, w!("clsid"))?;
    let clsid_wide: Vec<u16> = clsid_text.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `clsid_wide` は NUL 終端の UTF-16。
    let clsid = unsafe { CLSIDFromString(PCWSTR(clsid_wide.as_ptr())) }.ok()?;
    let server: Vec<u16> =
        format!("{clsid_text}\\InprocServer32").encode_utf16().chain(std::iter::once(0)).collect();
    let dll_path = classes?.string(&server, PCWSTR::null())?;
    if !Path::new(&dll_path).is_file() {
        return None;
    }
    let name = root.string(subkey, w!("description")).filter(|s| !s.is_empty()).unwrap_or_else(|| key.clone());
    Some(DriverEntry { key, name, clsid, dll_path })
}
