use std::cell::Cell;
use std::ffi::{CStr, c_char, c_void};

use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui};
use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
use clap_sys::host::clap_host;
use clap_sys::version::CLAP_VERSION;

use crate::plugin_instance::HostCallbacks;

thread_local! {
    /// Set to `true` on each process_server worker thread by
    /// `mark_audio_thread`; left `false` everywhere else (plugin-main /
    /// IPC / GUI). Read by the CLAP `thread_check` extension callback so
    /// plugins can validate they aren't calling main-thread-only APIs
    /// from inside `process()`.
    static IS_AUDIO_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Mark the calling thread as a CLAP "audio thread". Call this once per
/// worker thread (right after the priority boost) so the plugin's
/// `is_audio_thread()` check sees `true` while `plugin.process()` runs.
pub fn mark_audio_thread() {
    IS_AUDIO_THREAD.with(|c| c.set(true));
}

fn is_audio_thread_tls() -> bool {
    IS_AUDIO_THREAD.with(|c| c.get())
}

const NAME: &CStr = c"daw_01";
const VENDOR: &CStr = c"daw_01";
const URL: &CStr = c"";
const VERSION: &CStr = c"0.1.0";

/// CLAP host impl. Pinned via `Box<Host>` so the raw `host_data` pointer the
/// plugin holds remains valid for the plugin's lifetime.
#[repr(C)]
pub struct Host {
    /// Must be first so that a `*const clap_host` pointer into this struct
    /// aliases a `*const Host` — we rely on this for the `host_data` trick
    /// below. `#[repr(C)]` makes the offset deterministic.
    pub clap: clap_host,
    pub clap_gui: clap_host_gui,
    pub clap_thread_check: clap_host_thread_check,
    pub callbacks: HostCallbacks,
}

impl Host {
    pub fn new(callbacks: HostCallbacks) -> Box<Self> {
        let mut host = Box::new(Self {
            clap: clap_host {
                clap_version: CLAP_VERSION,
                host_data: std::ptr::null_mut(),
                name: NAME.as_ptr(),
                vendor: VENDOR.as_ptr(),
                url: URL.as_ptr(),
                version: VERSION.as_ptr(),
                get_extension: Some(get_extension),
                request_restart: Some(request_restart),
                request_process: Some(request_process),
                request_callback: Some(request_callback),
            },
            clap_gui: clap_host_gui {
                resize_hints_changed: Some(gui_resize_hints_changed),
                request_resize: Some(gui_request_resize),
                request_show: Some(gui_request_show),
                request_hide: Some(gui_request_hide),
                closed: Some(gui_closed),
            },
            clap_thread_check: clap_host_thread_check {
                is_main_thread: Some(thread_check_is_main_thread),
                is_audio_thread: Some(thread_check_is_audio_thread),
            },
            callbacks,
        });
        // Point host_data at the Box's heap allocation so the extension
        // callbacks can recover `&Host` from `*const clap_host`.
        host.clap.host_data = std::ptr::from_mut(&mut *host) as *mut c_void;
        host
    }

    /// Recovers `&Host` from a CLAP callback's `*const clap_host`. Returns
    /// `None` if the pointer or host_data is null (defensive, shouldn't
    /// happen in practice).
    ///
    /// # Safety
    /// `host` must be a pointer previously returned from `Host::new` (i.e.
    /// the `&self.clap` field of a live `Box<Host>`).
    unsafe fn from_clap<'a>(host: *const clap_host) -> Option<&'a Self> {
        if host.is_null() {
            return None;
        }
        let data = unsafe { (*host).host_data };
        if data.is_null() {
            return None;
        }
        Some(unsafe { &*(data as *const Self) })
    }
}

// --- clap_host entries ----------------------------------------------------

unsafe extern "C" fn get_extension(
    host: *const clap_host,
    id: *const c_char,
) -> *const c_void {
    let Some(this) = (unsafe { Host::from_clap(host) }) else {
        return std::ptr::null();
    };
    if id.is_null() {
        return std::ptr::null();
    }
    let id_cstr = unsafe { CStr::from_ptr(id) };
    if id_cstr == CLAP_EXT_GUI {
        return std::ptr::from_ref(&this.clap_gui) as *const c_void;
    }
    if id_cstr == CLAP_EXT_THREAD_CHECK {
        return std::ptr::from_ref(&this.clap_thread_check) as *const c_void;
    }
    std::ptr::null()
}

// --- clap_host_thread_check entries --------------------------------------

unsafe extern "C" fn thread_check_is_main_thread(_host: *const clap_host) -> bool {
    !is_audio_thread_tls()
}

unsafe extern "C" fn thread_check_is_audio_thread(_host: *const clap_host) -> bool {
    is_audio_thread_tls()
}

unsafe extern "C" fn request_restart(_host: *const clap_host) {
    tracing::info!("host callback: request_restart");
}

unsafe extern "C" fn request_process(_host: *const clap_host) {
    tracing::info!("host callback: request_process");
}

unsafe extern "C" fn request_callback(_host: *const clap_host) {
    tracing::info!("host callback: request_callback");
}

// --- clap_host_gui entries ------------------------------------------------

unsafe extern "C" fn gui_resize_hints_changed(_host: *const clap_host) {
    tracing::info!("host callback: resize_hints_changed");
}

unsafe extern "C" fn gui_request_resize(
    host: *const clap_host,
    width: u32,
    height: u32,
) -> bool {
    let Some(this) = (unsafe { Host::from_clap(host) }) else {
        return false;
    };
    (this.callbacks.on_request_resize)(width, height);
    // We accept the hint; the actual resize is scheduled asynchronously via
    // daw_gui → pipe → plugin-main → `plugin.gui.set_size()`. Returning
    // `true` tells the plugin it doesn't need to call `set_size` itself.
    true
}

unsafe extern "C" fn gui_request_show(host: *const clap_host) -> bool {
    // C6 (r.md #8): plugin が GUI を前面化要求。 host 所有の editor 窓を
    // SetForegroundWindow する (callback → channel → plugin-main loop)。
    let Some(this) = (unsafe { Host::from_clap(host) }) else {
        return false;
    };
    (this.callbacks.on_request_show)();
    true
}

unsafe extern "C" fn gui_request_hide(host: *const clap_host) -> bool {
    // C6 (r.md #8): plugin が GUI を隠す要求。 host 所有の editor 窓を hide する。
    let Some(this) = (unsafe { Host::from_clap(host) }) else {
        return false;
    };
    (this.callbacks.on_request_hide)();
    true
}

unsafe extern "C" fn gui_closed(host: *const clap_host, was_destroyed: bool) {
    tracing::info!(was_destroyed, "host callback: gui closed");
    let Some(this) = (unsafe { Host::from_clap(host) }) else {
        return;
    };
    (this.callbacks.on_closed)();
}
