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
//! (a 1:1 pair) and tell it which device to process via the shared
//! `WorkerBridge::worker_task[i]` atomic (v29: 安定 device id, u64)。
//! Because the audio engine dispatches per **track** (and a track's chain
//! runs serially in one audio worker), the same plugin instance is never
//! asked to process concurrently — CLAP spec is upheld without per-plugin
//! locking.
//!
//! The events are auto-reset, so a single waiter consumes the signal and
//! the event is immediately ready for the next dispatch.
//!
//! # 有界 dispatch と poisoning contract (`docs/plan_arch_refactor.md` §4)
//!
//! [`WorkerSyncRef::dispatch`] は done event を **有界** (`timeout_ms`) で
//! 待つ。RT スレッドが他プロセスを無限待ちすると、プラグインの SEH crash /
//! `process()` 内ハングで CPAL コールバックが永久凍結し (named event は
//! 所有プロセスが死んでも signal されない)、respawn しても復旧不能になる —
//! 3 プロセス分離の目的そのものが崩れるため、無限待ちは禁止。
//!
//! **timeout 後の worker pair は poisoned**: 後から plugin_host 側が
//! `process()` を終えて done を signal すると、auto-reset event が待ち手
//! なしで signaled のまま残る。この状態で次の dispatch をすると「まだ
//! 走っている process と並行に入力を書く」ことになる。よって timeout を
//! 観測した worker は以後 dispatch してはならず、pool 再構築 (=
//! `OpenWorkerPool` の再送) まで該当 pair を停止する。pool 再構築時は
//! event 名に **generation** を含めて mint し (`worker_wake_event_name`)、
//! 旧世代の stale signal が新 pool に漏れないようにする。

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(windows)]
use windows::Win32::{
    Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{SetEvent, WaitForSingleObject},
};

use crate::process_data::ProcessData;

/// worker dispatch の done 待ちの既定 timeout (ms)。数 buffer 分 (~10-21ms/
/// buffer) より十分大きく、GUI が「音が止まった」と感じる前に quarantine が
/// 発動する値。debug ビルドの重いプラグインでも通常は超えない。
pub const DISPATCH_TIMEOUT_MS: u32 = 500;

/// Owned by daw_audio. One per loaded plugin instance.
#[derive(Clone, Copy)]
pub struct PluginRef {
    /// 安定 device id (`PluginInstance::id`)。
    pub device_id: u64,
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

/// [`WorkerSyncRef::dispatch`] の結果。
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// plugin_host worker が process() を完了した (通常経路)。
    Done,
    /// `timeout_ms` 内に done が来なかった。**この worker pair は poisoned**
    /// — 呼び出し側は該当 device を quarantine し、pool 再構築までこの
    /// pair で dispatch しないこと (module doc の contract 参照)。
    TimedOut,
}

/// Owned by an audio-engine worker. The `worker_task` pointer references
/// the matching slot in the shared `WorkerBridge` shmem.
#[cfg(windows)]
pub struct WorkerSyncRef {
    pub worker_idx: u32,
    pub worker_task: *const AtomicU64,
    pub event_wake: HANDLE,
    pub event_done: HANDLE,
}

#[cfg(windows)]
unsafe impl Send for WorkerSyncRef {}

// Safe: the audio engine and the export thread only read `worker_task`
// (atomic), `event_wake`, and `event_done`. The handles themselves are
// kernel objects with their own internal sync; Rust's `*const AtomicU64`
// carries the actual atomicity for `worker_task`.
#[cfg(windows)]
unsafe impl Sync for WorkerSyncRef {}

#[cfg(windows)]
impl WorkerSyncRef {
    /// Hand a device to the matching plugin-host worker and wait (bounded)
    /// until `process()` finishes. The caller must have already populated
    /// the `ProcessData` for `device_id` (frames / events_in / buffer_in).
    ///
    /// Order of operations is load-bearing:
    ///   1. Publish `device_id` so the host worker can read it after the
    ///      wake fires (`Release`).
    ///   2. Signal the wake event.
    ///   3. Wait on the done event (auto-reset; one return = one signal),
    ///      **bounded by `timeout_ms`** — see the module-level poisoning
    ///      contract for what `TimedOut` obliges the caller to do.
    pub fn dispatch(&self, device_id: u64, timeout_ms: u32) -> anyhow::Result<DispatchOutcome> {
        unsafe {
            (*self.worker_task).store(device_id, Ordering::Release);
            SetEvent(self.event_wake)?;
            match WaitForSingleObject(self.event_done, timeout_ms) {
                WAIT_OBJECT_0 => Ok(DispatchOutcome::Done),
                WAIT_TIMEOUT => Ok(DispatchOutcome::TimedOut),
                WAIT_FAILED => Err(anyhow::anyhow!(
                    "WaitForSingleObject(done) failed: {:?}",
                    windows::core::Error::from_thread()
                )),
                other => Err(anyhow::anyhow!(
                    "WaitForSingleObject(done) unexpected result: {other:?}"
                )),
            }
        }
    }
}

/// Build the OS-namespaced names the audio engine and plugin host use to
/// open the per-worker event pair. The `pid` is the daw_gui PID so two
/// concurrent daw_01 sessions on the same machine don't clash;
/// `generation` is the pool generation (daw_gui bumps it on every
/// `OpenWorkerPool`) so a rebuilt pool never observes a stale auto-reset
/// signal left by a poisoned pair of the previous generation.
pub fn worker_wake_event_name(pid: u32, generation: u32, worker_idx: u32) -> String {
    format!("daw_01_worker_wake_{pid}_{generation}_{worker_idx}")
}

pub fn worker_done_event_name(pid: u32, generation: u32, worker_idx: u32) -> String {
    format!("daw_01_worker_done_{pid}_{generation}_{worker_idx}")
}

/// Build the shared-memory id for a plugin instance's `ProcessData` slot.
/// v29: keyed by the stable device id.
pub fn process_data_shmem_id(pid: u32, device_id: u64) -> String {
    format!("daw_01_process_data_{pid}_{device_id}")
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
