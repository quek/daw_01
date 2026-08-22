use std::cell::Cell;
use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::Ordering;

use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, clap_host_params, clap_param_clear_flags,
    clap_param_rescan_flags};
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
    pub clap_latency: clap_host_latency,
    pub clap_params: clap_host_params,
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
            clap_latency: clap_host_latency {
                changed: Some(latency_changed),
            },
            clap_params: clap_host_params {
                rescan: Some(params_rescan),
                clear: Some(params_clear),
                request_flush: Some(params_request_flush),
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
    if id_cstr == CLAP_EXT_LATENCY {
        return std::ptr::from_ref(&this.clap_latency) as *const c_void;
    }
    if id_cstr == CLAP_EXT_PARAMS {
        return std::ptr::from_ref(&this.clap_params) as *const c_void;
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

/// CLAP `clap_host.request_restart` — plugin が deactivate → activate の
/// full reinit を要求。plugin-main の quiesced-reinit 経路 (per-plugin
/// cooldown 付き — reinit 無限ループの構造的防御) へ配線する。
unsafe extern "C" fn request_restart(host: *const clap_host) {
    tracing::info!("host callback: request_restart");
    let Some(this) = (unsafe { Host::from_clap(host) }) else {
        return;
    };
    (this.callbacks.on_request_restart)();
}

unsafe extern "C" fn request_process(_host: *const clap_host) {
    // Our worker pool processes continuously while the pool is open, so
    // there is no paused scheduler to poke. Accepting silently is correct.
    tracing::info!("host callback: request_process");
}

/// CLAP `clap_host.request_callback` — plugin-main thread で
/// `clap_plugin.on_main_thread()` を 1 回呼ぶ予約 (JUCE 系 plugin の
/// main-thread task 駆動)。旧実装はログのみで、task が永遠に走らなかった。
unsafe extern "C" fn request_callback(host: *const clap_host) {
    let Some(this) = (unsafe { Host::from_clap(host) }) else {
        return;
    };
    (this.callbacks.on_request_callback)();
}

// --- clap_host_latency / clap_host_params entries --------------------------

/// CLAP `clap_host_latency.changed` — [main-thread] で latency を再 query
/// して `PluginLatencyChanged` を再 emit させる (VST3 kLatencyChanged と対称)。
unsafe extern "C" fn latency_changed(host: *const clap_host) {
    let Some(this) = (unsafe { Host::from_clap(host) }) else {
        return;
    };
    (this.callbacks.on_latency_changed)();
}

/// CLAP `clap_host_params.rescan` — param 一覧の再送を要求。flags
/// (CLAP_PARAM_RESCAN_*) は「全リスト再送」で常に上位互換に扱えるので
/// host 側では区別しない。
unsafe extern "C" fn params_rescan(host: *const clap_host, flags: clap_param_rescan_flags) {
    tracing::info!(flags, "host callback: params.rescan");
    let Some(this) = (unsafe { Host::from_clap(host) }) else {
        return;
    };
    (this.callbacks.on_params_rescan)();
}

/// CLAP `clap_host_params.clear` — daw_gui 側の automation は param id で
/// 疎結合 (存在しない id のイベントは plugin が無視) なので、host 側の
/// per-param 状態クリアは不要。受理のみ。
unsafe extern "C" fn params_clear(
    _host: *const clap_host,
    param_id: u32,
    flags: clap_param_clear_flags,
) {
    tracing::debug!(param_id, flags, "host callback: params.clear (no host-side state)");
}

/// CLAP `clap_host_params.request_flush` — 「processing していないなら
/// flush を予約せよ」の hint。本 host は worker pool が開いている間
/// 毎 buffer `process()` を回す設計で、pool が閉じている間は音声経路自体が
/// 存在しない (param イベントも流れない) ため、専用 flush は不要。
unsafe extern "C" fn params_request_flush(_host: *const clap_host) {
    tracing::debug!("host callback: params.request_flush (continuous process model)");
}

// --- clap_host_gui entries ------------------------------------------------

unsafe extern "C" fn gui_resize_hints_changed(_host: *const clap_host) {
    tracing::info!("host callback: resize_hints_changed");
}

/// CLAP `clap_host_gui.request_resize` (`gui.h` L219-227)。
///
/// > *"Return true if the new size is accepted... **The host doesn't have to call
/// > set_size().** Note: if not called from the main thread, then a return value
/// > simply means that the host acknowledged the request and will process it
/// > asynchronously."*
///
/// r.md #65: main-thread から来たものは **同じコールスタックで**窓をリサイズする
/// (VST3 と違い `set_size` の呼び返し義務は無い — 窓を直せば完了)。窓がまだ無い /
/// 別スレッドからの呼び出しは非同期経路へ落として ack だけ返す (ヘッダが明示的に
/// 許している唯一の非同期ケース)。`adjust_size` は掛けない — ヘッダの手順にも
/// clap-host / clap-wrapper の実装にも無く、プラグイン自身が出した希望値なので既に valid。
unsafe extern "C" fn gui_request_resize(
    host: *const clap_host,
    width: u32,
    height: u32,
) -> bool {
    let Some(this) = (unsafe { Host::from_clap(host) }) else {
        return false;
    };
    if width == 0 || height == 0 {
        return false;
    }
    let hwnd = this.callbacks.editor_hwnd.load(Ordering::Acquire);
    if hwnd != 0 && crate::editor_window::plugin_requested_resize(hwnd, width, height) {
        return true;
    }
    (this.callbacks.on_request_resize)(width, height);
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
