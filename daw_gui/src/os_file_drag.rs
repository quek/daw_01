//! r.md #141: daw_01 の中のドラッグを **OS のファイルドラッグ** (OLE `DoDragDrop` + `CF_HDROP`) に
//! 切り替えて、別プロセスの窓 (daw_plugin_host が出すプラグインのエディタ窓 / Explorer) へ素材ファイルを
//! 渡す。受け取る側から見ると Explorer から落とされたのと同じデータになる。
//!
//! - **呼ぶのは本体窓のスレッドだけ。** winit がそのスレッドで `OleInitialize` 済み
//!   (winit `platform_impl/windows/window.rs` の `OleInitialize`)。`DoDragDrop` はモーダルループで、
//!   その間に届く窓イベントは winit が buffer して、ハンドラを抜けた後に流す
//!   (`event_loop/runner.rs` の `should_buffer`) — 再入しない代わりに、ドラッグ中は GUI の描画が止まる。
//! - `IDataObject` / `IDropSource` は自前で実装しない: 空の shell data object (`SHCreateDataObject`) に
//!   `CF_HDROP` を `SetData` し、`SHDoDragDrop` に既定の drop source とドラッグ画像を任せる。

use std::path::PathBuf;

/// カーソルの下が **自分以外のプロセスの窓** で、かつ実際のマウスの主ボタンが押されているか。
///
/// 実ボタンを見るのは、headless (`--script`) や widget テストが合成したドラッグで本物の
/// `DoDragDrop` が走らないため — 走るとボタンが離れているので即座に、カーソルの下にある
/// ユーザーの窓へ勝手に drop してしまう。
#[cfg(windows)]
pub fn cursor_over_foreign_window() -> bool {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::{
        GA_ROOT, GetAncestor, GetCursorPos, GetSystemMetrics, GetWindowThreadProcessId, SM_SWAPBUTTON,
        WindowFromPoint,
    };
    // GetAsyncKeyState は **物理** ボタンを見るので、左右入れ替え設定なら右を見る (MS docs)。
    let primary = if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0 { VK_RBUTTON } else { VK_LBUTTON };
    if unsafe { GetAsyncKeyState(i32::from(primary.0)) } >= 0 {
        return false;
    }
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&raw mut pt) }.is_err() {
        return false;
    }
    let hwnd = unsafe { WindowFromPoint(pt) };
    if hwnd.is_invalid() {
        return false;
    }
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let target = if root.is_invalid() { hwnd } else { root };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(target, Some(&raw mut pid)) };
    pid != 0 && pid != unsafe { GetCurrentProcessId() }
}

#[cfg(not(windows))]
pub fn cursor_over_foreign_window() -> bool {
    false
}

/// `paths` を `CF_HDROP` に載せて OS のドラッグを行い、終わるまで (= 落とされる / 取り消される
/// まで) 戻らない。戻り値は drop 先が受け入れたか。
#[cfg(windows)]
pub fn run(hwnd: isize, paths: &[PathBuf]) -> anyhow::Result<bool> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_NONE, IDropSource};
    use windows::Win32::UI::Shell::SHDoDragDrop;

    let data = file_data_object(paths)?;
    let effect = unsafe {
        SHDoDragDrop(Some(HWND(hwnd as *mut core::ffi::c_void)), &data, None::<&IDropSource>, DROPEFFECT_COPY)
    }?;
    Ok(effect != DROPEFFECT_NONE)
}

#[cfg(not(windows))]
pub fn run(_hwnd: isize, _paths: &[PathBuf]) -> anyhow::Result<bool> {
    Ok(false)
}

#[cfg(windows)]
fn hdrop_format() -> windows::Win32::System::Com::FORMATETC {
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Ole::CF_HDROP;
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        #[allow(clippy::cast_sign_loss)]
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// `paths` を `CF_HDROP` で持つ data object (Explorer からのドラッグと同じ形式)。
#[cfg(windows)]
fn file_data_object(paths: &[PathBuf]) -> anyhow::Result<windows::Win32::System::Com::IDataObject> {
    use std::mem::ManuallyDrop;
    use windows::Win32::System::Com::{IDataObject, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL};
    use windows::Win32::UI::Shell::SHCreateDataObject;

    let hglobal = hdrop_global(paths)?;
    let data: IDataObject = unsafe { SHCreateDataObject(None, None, None::<&IDataObject>) }?;
    let format = hdrop_format();
    let medium = STGMEDIUM {
        #[allow(clippy::cast_sign_loss)]
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: hglobal },
        pUnkForRelease: ManuallyDrop::new(None),
    };
    // release = true: 成功すれば HGLOBAL の所有は data object へ移る。
    if let Err(e) = unsafe { data.SetData(&raw const format, &raw const medium, true) } {
        let _ = unsafe { windows::Win32::Foundation::GlobalFree(Some(hglobal)) };
        return Err(e.into());
    }
    Ok(data)
}

/// `CF_HDROP` の中身: `DROPFILES` の後に UTF-16 のパスを NUL 区切りで並べ、最後に NUL をもう 1 つ。
#[cfg(windows)]
fn hdrop_global(paths: &[PathBuf]) -> anyhow::Result<windows::Win32::Foundation::HGLOBAL> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::Win32::System::Memory::{GHND, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::UI::Shell::DROPFILES;

    let mut wide: Vec<u16> = Vec::new();
    for p in paths {
        wide.extend(p.as_os_str().encode_wide());
        wide.push(0);
    }
    wide.push(0);
    let header = std::mem::size_of::<DROPFILES>();
    let bytes = header + wide.len() * 2;
    // GHND = moveable + zero-init。
    let hglobal = unsafe { GlobalAlloc(GHND, bytes) }?;
    let ptr = unsafe { GlobalLock(hglobal) }.cast::<u8>();
    if ptr.is_null() {
        let _ = unsafe { windows::Win32::Foundation::GlobalFree(Some(hglobal)) };
        anyhow::bail!("GlobalLock failed");
    }
    let df = DROPFILES { pFiles: u32::try_from(header)?, fWide: true.into(), ..Default::default() };
    // SAFETY: ptr は `bytes` 以上の確保済み領域。DROPFILES は packed(1) なので unaligned で書く。
    unsafe {
        std::ptr::write_unaligned(ptr.cast::<DROPFILES>(), df);
        std::ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), ptr.add(header), wide.len() * 2);
    }
    // 最後の lock を外すと Err(NO_ERROR) が返る仕様なので結果は見ない。
    let _ = unsafe { GlobalUnlock(hglobal) };
    Ok(hglobal)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows::Win32::System::Ole::{OleInitialize, ReleaseStgMedium};
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    /// drop target (Redux の JUCE / Explorer) と同じ読み方 (`GetData(CF_HDROP)` → `DragQueryFileW`) で、
    /// 渡したパスが順番どおり・非 ASCII も崩れずに読めること。
    #[test]
    fn hdrop_data_object_reads_back_like_an_explorer_drop() {
        unsafe { OleInitialize(None) }.expect("OleInitialize");
        let paths = vec![PathBuf::from(r"C:\samples\kick.wav"), PathBuf::from(r"D:\素材\声 01.wav")];
        let data = file_data_object(&paths).expect("data object");
        let format = hdrop_format();
        let mut medium = unsafe { data.GetData(&raw const format) }.expect("GetData(CF_HDROP)");
        let hdrop = HDROP(unsafe { medium.u.hGlobal }.0);
        let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
        let mut got = Vec::new();
        for i in 0..count {
            let mut buf = [0u16; 512];
            let n = unsafe { DragQueryFileW(hdrop, i, Some(&mut buf)) } as usize;
            got.push(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
        }
        unsafe { ReleaseStgMedium(&raw mut medium) };
        assert_eq!(got, paths);
    }
}
