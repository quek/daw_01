//! Per-plugin shared-memory handle (`PluginRef`) and per-worker handshake
//! handle (`WorkerSyncRef`).
//!
//! daw_audio owns a `PluginRef` per loaded plugin instance — it points at
//! the shared `ProcessData` slot the plugin host will read inputs from
//! and write outputs into. The audio engine has exclusive write access to
//! the input fields (`frames`, `events_in`, `buffer_in`) and read access
//! to outputs; the plugin host does the inverse.
//!
//! daw_audio also owns N `WorkerSyncRef`, one per audio-engine worker
//! thread. `worker[i]` uses `worker_sync[i]` to wake `plugin_host worker[i]`
//! (a 1:1 pair) and tell it which plugin to process via the shared
//! `WorkerBridge::worker_task[i]` atomic. Because the audio engine
//! dispatches per **track** (and a track's chain runs serially in one
//! audio worker), the same plugin instance is never asked to process
//! concurrently — CLAP spec is upheld without per-plugin locking.
//!
//! The events are auto-reset, so a single waiter consumes the signal and
//! the event is immediately ready for the next dispatch.

use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(windows)]
use windows::Win32::{
    Foundation::HANDLE,
    System::Threading::{INFINITE, SetEvent, WaitForSingleObject},
};

use crate::process_data::ProcessData;

/// Owned by daw_audio. One per loaded plugin instance.
pub struct PluginRef {
    pub plugin_id: u32,
    pub process_data: *mut ProcessData,
}

unsafe impl Send for PluginRef {}
unsafe impl Sync for PluginRef {}

impl PluginRef {
    /// Read-only view of the shared `ProcessData`. The audio engine uses
    /// this after the worker handshake returns to read outputs.
    pub fn data(&self) -> &ProcessData {
        unsafe { &*self.process_data }
    }

    /// Mutable view used to fill inputs before dispatching. The audio
    /// engine must hold exclusive access during this window — guaranteed
    /// by the per-track dispatch + serial chain rule.
    #[allow(clippy::mut_from_ref)]
    pub fn data_mut(&self) -> &mut ProcessData {
        unsafe { &mut *self.process_data }
    }
}

/// Owned by an audio-engine worker. The `worker_task` pointer references
/// the matching slot in the shared `WorkerBridge` shmem.
#[cfg(windows)]
pub struct WorkerSyncRef {
    pub worker_idx: u32,
    pub worker_task: *const AtomicU32,
    pub event_wake: HANDLE,
    pub event_done: HANDLE,
}

#[cfg(windows)]
unsafe impl Send for WorkerSyncRef {}

#[cfg(windows)]
impl WorkerSyncRef {
    /// Hand a plugin to the matching plugin-host worker and block until
    /// `process()` finishes. The caller must have already populated the
    /// `ProcessData` for `plugin_id` (frames / events_in / buffer_in).
    ///
    /// Order of operations is load-bearing:
    ///   1. Publish `plugin_id` so the host worker can read it after the
    ///      wake fires (`Release`).
    ///   2. Signal the wake event.
    ///   3. Wait on the done event (auto-reset; one return = one signal).
    pub fn dispatch(&self, plugin_id: u32) -> anyhow::Result<()> {
        unsafe {
            (*self.worker_task).store(plugin_id, Ordering::Release);
            SetEvent(self.event_wake)?;
            WaitForSingleObject(self.event_done, INFINITE);
        }
        Ok(())
    }
}

/// Build the OS-namespaced names the audio engine and plugin host use to
/// open the per-worker event pair. The `pid` is the daw_gui PID so two
/// concurrent daw_01 sessions on the same machine don't clash.
pub fn worker_wake_event_name(pid: u32, worker_idx: u32) -> String {
    format!("daw_01_worker_wake_{pid}_{worker_idx}")
}

pub fn worker_done_event_name(pid: u32, worker_idx: u32) -> String {
    format!("daw_01_worker_done_{pid}_{worker_idx}")
}

/// Build the shared-memory id for a plugin instance's `ProcessData` slot.
pub fn process_data_shmem_id(pid: u32, plugin_id: u32) -> String {
    format!("daw_01_process_data_{pid}_{plugin_id}")
}

/// Build the shared-memory id for the worker bridge (`WorkerBridge`,
/// containing the `worker_task` array).
pub fn worker_bridge_shmem_id(pid: u32) -> String {
    format!("daw_01_worker_bridge_{pid}")
}

#[cfg(windows)]
mod win_event {
    use std::ffi::CString;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::CreateEventA;
    use windows::core::PCSTR;

    /// Create-or-open an auto-reset, initially non-signaled named event.
    /// On Windows, `CreateEventA` with an existing object name simply
    /// returns a new handle to the same kernel object (with
    /// `GetLastError() == ERROR_ALREADY_EXISTS`), so both the audio side
    /// and the plugin-host side call this — whoever runs first creates,
    /// the second one opens.
    pub fn create_named_event(name: &str) -> anyhow::Result<HANDLE> {
        let cname = CString::new(name)
            .map_err(|e| anyhow::anyhow!("event name has interior NUL: {e}"))?;
        unsafe {
            CreateEventA(
                None,
                false,
                false,
                PCSTR(cname.as_ptr() as *const u8),
            )
            .map_err(|e| anyhow::anyhow!("CreateEventA({name}) failed: {e}"))
        }
    }

    /// Alias kept for code clarity at the call site (the audio engine's
    /// "create" vs the plugin host's "open" intent are different even if
    /// the call is identical).
    pub fn open_named_event(name: &str) -> anyhow::Result<HANDLE> {
        create_named_event(name)
    }
}

#[cfg(windows)]
pub use win_event::{create_named_event, open_named_event};
