//! Host-owned Win32 container window for a CLAP plugin's embedded GUI.
//!
//! The DAW GUI creates this top-level window, hands its HWND to
//! daw_plugin_host over IPC, and daw_plugin_host calls
//! `clap_plugin_gui.set_parent` so the plugin paints as a child of our
//! container. The container is owned by daw_gui — the DAW controls when it
//! opens, closes, and is resized — matching the CLAP "embedded" protocol.
//!
//! Why a top-level window rather than a child HWND inside the main DAW
//! window? The main window is owned by winit/wgpu, and creating an
//! additional native child inside the GPU surface would fight the renderer's
//! layout. A dedicated top-level container sidesteps that coupling while
//! preserving the best-practice invariant that the host (not the plugin)
//! owns the top-level window.

#![cfg(windows)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    GWLP_USERDATA, GetWindowLongPtrW, HCURSOR, HICON, HMENU, SWP_NOMOVE, SWP_NOZORDER, SW_HIDE,
    SW_SHOW, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, WINDOW_EX_STYLE,
    WM_CLOSE, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::PCWSTR;

static CLASS_ATOM: OnceLock<u16> = OnceLock::new();
// Registered class name kept alive for the lifetime of the process. Win32
// stores only a pointer into its class table, so the buffer must outlive
// every window created with it.
static CLASS_NAME: OnceLock<Vec<u16>> = OnceLock::new();

/// RAII wrapper for the host-owned container HWND.
pub struct PluginHostWindow {
    hwnd: HWND,
    /// Set to true by the WNDPROC when the user hits the ✕ button. The DAW
    /// side polls this each UI tick (in `AppData::on_tick`) and runs the
    /// full Close flow when set, cleanly tearing down plugin state via IPC.
    close_requested: Arc<AtomicBool>,
}

// HWND is !Send by default but we manage it strictly from the daw_gui main
// thread; the wrapper itself never crosses threads because we only pass the
// raw `u64` hwnd across to plugin_host over IPC.
unsafe impl Send for PluginHostWindow {}
unsafe impl Sync for PluginHostWindow {}

impl PluginHostWindow {
    /// Create a new top-level container window sized `width × height`.
    pub fn create(width: u32, height: u32, title: &str) -> windows::core::Result<Self> {
        let atom = class_atom()?;
        let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }?;
        let title_utf16: Vec<u16> = title
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(atom as usize as *const u16),
                PCWSTR(title_utf16.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                // x, y, width, height:
                100,
                100,
                width as i32,
                height as i32,
                None,
                Some(HMENU(std::ptr::null_mut())),
                Some(hinstance.into()),
                None,
            )
        }?;

        // Attach a close-request flag so the WNDPROC can signal the DAW
        // UI thread when the user clicks ✕.
        let close_requested = Arc::new(AtomicBool::new(false));
        unsafe {
            // Leak-into-pointer, reclaimed in Drop below.
            let raw = Arc::into_raw(Arc::clone(&close_requested));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
        }

        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        Ok(Self {
            hwnd,
            close_requested,
        })
    }

    /// Returns true once since the last call if the user clicked the ✕
    /// button. The DAW polls this each UI tick to drive the Close flow.
    pub fn take_close_request(&self) -> bool {
        self.close_requested.swap(false, Ordering::AcqRel)
    }

    /// Raw `HWND` as a `u64` suitable for bincode IPC.
    pub fn hwnd_u64(&self) -> u64 {
        self.hwnd.0 as u64
    }

    /// Update the title bar text.
    #[allow(dead_code)]
    pub fn set_title(&self, title: &str) {
        let title_utf16: Vec<u16> = title
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let _ = SetWindowTextW(self.hwnd, PCWSTR(title_utf16.as_ptr()));
        }
    }

    /// Resize the container so its *client area* is `width × height` — the
    /// plugin paints into the client area, so matching that to the plugin's
    /// preferred size prevents clipping. Title bar and borders are added
    /// via `AdjustWindowRectEx`.
    pub fn set_client_size(&self, width: u32, height: u32) {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        unsafe {
            if AdjustWindowRectEx(
                &mut rect,
                WS_OVERLAPPEDWINDOW,
                false,
                WINDOW_EX_STYLE(0),
            )
            .is_err()
            {
                tracing::warn!(width, height, "AdjustWindowRectEx failed; using raw size");
            }
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
                tracing::warn!(error = ?e, "SetWindowPos failed");
            }
        }
    }
}

impl Drop for PluginHostWindow {
    fn drop(&mut self) {
        if !self.hwnd.0.is_null() {
            unsafe {
                // Reclaim the Arc we leaked into GWLP_USERDATA so its
                // refcount is balanced.
                let raw = GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) as *const AtomicBool;
                if !raw.is_null() {
                    SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                    drop(Arc::from_raw(raw));
                }
                let _ = DestroyWindow(self.hwnd);
            }
            tracing::info!(hwnd = self.hwnd.0 as usize, "plugin host window destroyed");
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
    let name: Vec<u16> = "daw_01_plugin_host_window\0".encode_utf16().collect();
    let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }?;
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(container_wnd_proc),
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
    let atom = unsafe { windows::Win32::UI::WindowsAndMessaging::RegisterClassExW(&wc) };
    if atom == 0 {
        return Err(windows::core::Error::from_win32());
    }
    Ok((atom, name))
}

/// Simple WNDPROC: just defers to `DefWindowProcW`. We currently rely on
/// Windows' default behavior for resize and the close button; later we can
/// hook `WM_CLOSE` here to notify daw_plugin_host via IPC.
unsafe extern "system" fn container_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Intercept the ✕ button. We must NOT call DefWindowProcW for WM_CLOSE
    // (it would destroy the HWND behind our RAII wrapper). Instead: hide
    // the window, and flip the close-request flag so the DAW side can
    // run the full close flow (send CloseGui over IPC and Drop the
    // wrapper — which is what destroys the HWND).
    if msg == WM_CLOSE {
        tracing::info!("plugin host window received WM_CLOSE (hiding)");
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
