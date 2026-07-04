//! Plugin-host-owned Win32 top-level window that hosts a plugin's editor.
//!
//! Previously daw_gui (the GUI process) created the editor's
//! container window and handed its HWND across IPC, so the plugin editor was
//! a child window whose top-level ancestor lived in *another process*. JUCE
//! plugins (e.g. Scaler 2) gate cascade sub-menus on
//! `Process::isForegroundProcess()` — which compares the owning process of
//! the system-wide foreground window to the plugin's own process. With the
//! container owned by daw_gui, that check is always false inside the
//! plugin-host process, so JUCE dismisses any sub-menu (`componentAttachedTo
//! == nullptr` for a sub-menu, so JUCE's `isEmbeddedInForegroundProcess`
//! escape hatch can't apply). See `docs/plan_plugin_editor_topwindow.md`.
//!
//! The fix is to create the editor's top-level window *here*, in the
//! plugin-host process, on the plugin-main thread (the one that runs the
//! `GetMessageW` pump). Clicking into the editor then activates a window this
//! process owns → this process becomes the foreground process → JUCE's
//! `isForegroundProcess()` is true → first-level AND cascade menus work.
//!
//! The window must be a standalone top-level window with **no owner** — if it
//! were owned by daw_gui's main window, `GetAncestor(.., GA_ROOTOWNER)` would
//! climb back into the GUI process and reintroduce the bug.

#![cfg(windows)]

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    GWLP_USERDATA, GetWindowLongPtrW, HCURSOR, HICON, HMENU, RegisterClassExW, SWP_NOMOVE,
    SWP_NOZORDER, SW_HIDE, SW_SHOW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    SetWindowTextW, ShowWindow, WINDOW_EX_STYLE, WM_CLOSE, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::core::PCWSTR;

/// C1 (r.md #8): HWND の DPI scale (= `GetDpiForWindow` / 96)。 取得失敗 (dpi 0) や
/// 非 HiDPI は 1.0。 plugin の `gui.set_scale` / VST3 `setContentScaleFactor` に渡し、
/// HiDPI で editor が極小 / ぼやけるのを防ぐ。
#[must_use]
pub fn window_dpi_scale(hwnd_u64: u64) -> f64 {
    let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        1.0
    } else {
        f64::from(dpi) / 96.0
    }
}

static CLASS_ATOM: OnceLock<u16> = OnceLock::new();
// Win32 stores only a pointer into its class table, so the class-name buffer
// must outlive every window created with it.
static CLASS_NAME: OnceLock<Vec<u16>> = OnceLock::new();

/// Clamp a plugin-reported pixel dimension into a sane positive `i32`.
/// `gui_get_size` / `resizeView` come from the plugin, so a buggy or hostile
/// plugin could report a huge `u32` that would wrap to a negative value on a
/// bare `as i32`. Clamp to `[1, 16384]` — far beyond any real editor, well
/// under the Win32 window-size sanity limit.
fn clamp_dim(v: u32) -> i32 {
    v.clamp(1, 16_384) as i32
}

/// RAII wrapper for a plugin-host-owned editor container window. Created on
/// the plugin-main thread; never crosses threads.
///
/// v29: 所属は `InstanceRecord.editor` (device_id keyed の単一 map) が持つ
/// ので、旧 `plugin_id` フィールドと述語 matching は不要になった。
pub struct EditorWindow {
    hwnd: HWND,
    /// Set by the WNDPROC when the user clicks the window's ✕. The
    /// plugin-main loop polls this each iteration and runs the close flow
    /// (`plugin.gui_destroy()` then drop this window) over IPC notify.
    close_requested: Arc<AtomicBool>,
}

// HWND is !Send but EditorWindow is owned and used strictly on the
// plugin-main thread; it is never sent across threads.
unsafe impl Send for EditorWindow {}

impl EditorWindow {
    /// Create a standalone top-level container window with a `width × height`
    /// client area. **Must be called on the plugin-main thread** (the one
    /// running the `GetMessageW` pump) so its window messages land on that
    /// thread's queue. The window has no owner (see module docs).
    pub fn create(
        width: u32,
        height: u32,
        title: &str,
    ) -> windows::core::Result<Self> {
        let atom = class_atom()?;
        let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }?;
        let title_utf16: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

        // Compute the outer size so the *client* area matches the plugin's
        // requested size (title bar + borders added on top).
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: clamp_dim(width),
            bottom: clamp_dim(height),
        };
        unsafe {
            let _ = AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0));
        }
        let outer_w = (rect.right - rect.left).max(1);
        let outer_h = (rect.bottom - rect.top).max(1);

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(atom as usize as *const u16),
                PCWSTR(title_utf16.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                // x, y, outer width, outer height:
                120,
                120,
                outer_w,
                outer_h,
                None, // no parent
                Some(HMENU(std::ptr::null_mut())),
                Some(hinstance.into()),
                None,
            )
        }?;

        let close_requested = Arc::new(AtomicBool::new(false));
        unsafe {
            // Leak-into-pointer; reclaimed in Drop.
            let raw = Arc::into_raw(Arc::clone(&close_requested));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        Ok(Self {
            hwnd,
            close_requested,
        })
    }

    pub fn hwnd_u64(&self) -> u64 {
        self.hwnd.0 as u64
    }

    /// True once if the user clicked ✕ since the last call. The plugin-main
    /// loop polls this to drive the close flow.
    pub fn take_close_request(&self) -> bool {
        self.close_requested.swap(false, Ordering::AcqRel)
    }

    /// Bring the window to the foreground. Best-effort: the call may be
    /// refused by the foreground-lock rules, but cascade menus do not depend
    /// on it (clicking into the editor makes this process foreground anyway).
    pub fn set_foreground(&self) {
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    #[allow(dead_code)]
    pub fn set_title(&self, title: &str) {
        let title_utf16: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let _ = SetWindowTextW(self.hwnd, PCWSTR(title_utf16.as_ptr()));
        }
    }

    /// Resize so the *client area* is `width × height` (the plugin paints
    /// into the client area). Title bar + borders are added via
    /// `AdjustWindowRectEx`.
    pub fn set_client_size(&self, width: u32, height: u32) {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: clamp_dim(width),
            bottom: clamp_dim(height),
        };
        unsafe {
            let _ = AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0));
            let outer_w = (rect.right - rect.left).max(1);
            let outer_h = (rect.bottom - rect.top).max(1);
            if let Err(e) = SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                outer_w,
                outer_h,
                SWP_NOMOVE | SWP_NOZORDER,
            ) {
                tracing::warn!(error = ?e, "editor window SetWindowPos failed");
            }
        }
    }

    /// Hide the window without tearing it down (used as an interim step;
    /// final teardown is the `Drop`).
    #[allow(dead_code)]
    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl Drop for EditorWindow {
    fn drop(&mut self) {
        if !self.hwnd.0.is_null() {
            unsafe {
                // Reclaim the Arc we leaked into GWLP_USERDATA.
                let raw = GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) as *const AtomicBool;
                if !raw.is_null() {
                    SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                    drop(Arc::from_raw(raw));
                }
                let _ = DestroyWindow(self.hwnd);
            }
            tracing::info!(hwnd = self.hwnd.0 as usize, "editor window destroyed");
        }
    }
}

// --- Win32 class registration --------------------------------------------

fn class_atom() -> windows::core::Result<u16> {
    if let Some(atom) = CLASS_ATOM.get() {
        return Ok(*atom);
    }
    let (atom, name) = unsafe { register_class() }?;
    let _ = CLASS_NAME.set(name);
    let _ = CLASS_ATOM.set(atom);
    Ok(*CLASS_ATOM.get().unwrap())
}

unsafe fn register_class() -> windows::core::Result<(u16, Vec<u16>)> {
    let name: Vec<u16> = "daw_01_plugin_editor_window\0".encode_utf16().collect();
    let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }?;
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(editor_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance.into(),
        hIcon: HICON(std::ptr::null_mut()),
        hCursor: HCURSOR(std::ptr::null_mut()),
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(name.as_ptr()),
        hIconSm: HICON(std::ptr::null_mut()),
    };
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        return Err(windows::core::Error::from_thread());
    }
    Ok((atom, name))
}

/// WNDPROC: intercept ✕ (WM_CLOSE) to flip the close-request flag and hide
/// the window. We must NOT call DefWindowProcW for WM_CLOSE — that would
/// `DestroyWindow` the HWND behind our RAII wrapper. The plugin-main loop
/// polls the flag, runs `plugin.gui_destroy()`, then drops this window
/// (which is what actually destroys the HWND, in the spec-correct order).
unsafe extern "system" fn editor_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLOSE {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
            let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AtomicBool;
            if !raw.is_null() {
                (*raw).store(true, Ordering::Release);
            }
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
