// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared-memory layout for the audio-engine ↔ plugin-host worker pool
//! handshake.
//!
//! The two processes each spawn N worker threads in 1:1 pairs. When audio
//! engine `worker[i]` wants `plugin_host worker[i]` to call
//! `plugin.process()` for a particular plugin instance, it writes the
//! stable **device id** (v29, `PluginInstance::id`) into `worker_task[i]`
//! (Release), then signals `worker_wake[i]` (a Win32 named event). The
//! plugin-host worker reads the slot (Acquire) after waking, runs
//! `process()`, and signals `worker_done[i]`.
//!
//! Only one shmem instance exists for the whole daw_01 session — its name
//! is fixed (see `plugin_ref::worker_bridge_shmem_id`).

use std::sync::atomic::AtomicU64;

/// Hard cap on workers. CPU core counts above this are extremely rare for
/// audio workloads (2026: typical desktop is 4–24 cores).
pub const MAX_WORKERS: usize = 32;

#[repr(C)]
pub struct WorkerBridge {
    /// `worker_task[i]` is the stable device id audio-engine `worker[i]`
    /// is asking plugin-host `worker[i]` to process this dispatch.
    /// `u64::MAX` means "idle, ignore" (used during shutdown wake).
    pub worker_task: [AtomicU64; MAX_WORKERS],
}

impl WorkerBridge {
    pub const IDLE: u64 = u64::MAX;

    pub fn zeroed() -> Self {
        Self {
            worker_task: [const { AtomicU64::new(Self::IDLE) }; MAX_WORKERS],
        }
    }
}

#[cfg(windows)]
mod shmem_handle {
    use anyhow::Result;

    use super::WorkerBridge;
    use crate::shmem::NamedShmem;

    pub struct WorkerBridgeHandle {
        shmem: NamedShmem,
    }

    impl WorkerBridgeHandle {
        pub fn create(os_id: &str) -> Result<Self> {
            let shmem = NamedShmem::create(os_id, std::mem::size_of::<WorkerBridge>())?;
            // The shmem view is backed by `MapViewOfFile`, which returns a
            // pointer aligned to at least 64 KiB (the system allocation
            // granularity). That trivially satisfies `WorkerBridge`'s
            // alignment (== `AtomicU64` align == 8).
            debug_assert!(
                (shmem.as_ptr() as usize).is_multiple_of(std::mem::align_of::<WorkerBridge>()),
                "worker_bridge shmem pointer is not WorkerBridge-aligned"
            );
            // Initialise every slot to IDLE so a stray wake before the
            // first dispatch doesn't make a worker go process plugin 0.
            // SAFETY: `shmem` is freshly created with at least
            // `size_of::<WorkerBridge>()` bytes (`.size(...)` above) and the
            // pointer is 64 KiB-aligned (asserted), so it is valid for a
            // single aligned write of a `WorkerBridge`. No other handle to
            // this mapping exists yet, so there is no aliasing.
            unsafe {
                let bridge = shmem.as_ptr() as *mut WorkerBridge;
                std::ptr::write(bridge, WorkerBridge::zeroed());
            }
            Ok(Self { shmem })
        }

        pub fn open(os_id: &str) -> Result<Self> {
            let shmem = NamedShmem::open(os_id, std::mem::size_of::<WorkerBridge>())?;
            // `MapViewOfFile` aligns the view to the 64 KiB system allocation
            // granularity, which covers `WorkerBridge`'s 8-byte alignment.
            debug_assert!(
                (shmem.as_ptr() as usize).is_multiple_of(std::mem::align_of::<WorkerBridge>()),
                "worker_bridge shmem pointer is not WorkerBridge-aligned"
            );
            Ok(Self { shmem })
        }

        pub fn bridge(&self) -> &WorkerBridge {
            // SAFETY: the mapping is at least `size_of::<WorkerBridge>()` bytes
            // (checked in `create`/`open`) and the `MapViewOfFile` pointer is
            // 64 KiB-aligned, satisfying `WorkerBridge`'s 8-byte alignment.
            // `WorkerBridge` is `#[repr(C)]` and holds only `AtomicU64`s, which
            // are valid for any bit pattern, so the mapped bytes are always a
            // valid `WorkerBridge`. Concurrent access from the peer process is
            // sound because all fields are atomics.
            unsafe { &*(self.shmem.as_ptr() as *const WorkerBridge) }
        }
    }

    unsafe impl Send for WorkerBridgeHandle {}
    unsafe impl Sync for WorkerBridgeHandle {}
}

#[cfg(windows)]
pub use shmem_handle::WorkerBridgeHandle;
