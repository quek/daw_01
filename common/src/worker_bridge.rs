//! Shared-memory layout for the audio-engine ↔ plugin-host worker pool
//! handshake.
//!
//! The two processes each spawn N worker threads in 1:1 pairs. When audio
//! engine `worker[i]` wants `plugin_host worker[i]` to call
//! `plugin.process()` for a particular plugin instance, it writes the
//! plugin id into `worker_task[i]` (Release), then signals `worker_wake[i]`
//! (a Win32 named event). The plugin-host worker reads the slot (Acquire)
//! after waking, runs `process()`, and signals `worker_done[i]`.
//!
//! Only one shmem instance exists for the whole daw_01 session — its name
//! is fixed (see `plugin_ref::worker_bridge_shmem_id`).

use std::sync::atomic::AtomicU32;

/// Hard cap on workers. CPU core counts above this are extremely rare for
/// audio workloads (2026: typical desktop is 4–24 cores). Sized so the
/// whole struct stays under one cache line on most machines.
pub const MAX_WORKERS: usize = 32;

#[repr(C)]
pub struct WorkerBridge {
    /// `worker_task[i]` is the plugin id audio-engine `worker[i]` is
    /// asking plugin-host `worker[i]` to process this dispatch.
    /// `u32::MAX` means "idle, ignore" (used during shutdown wake).
    pub worker_task: [AtomicU32; MAX_WORKERS],
}

impl WorkerBridge {
    pub const IDLE: u32 = u32::MAX;

    pub fn zeroed() -> Self {
        Self {
            worker_task: [const { AtomicU32::new(Self::IDLE) }; MAX_WORKERS],
        }
    }
}

#[cfg(windows)]
mod shmem_handle {
    use anyhow::{Context, Result};
    use shared_memory::{Shmem, ShmemConf};

    use super::WorkerBridge;

    pub struct WorkerBridgeHandle {
        shmem: Shmem,
    }

    impl WorkerBridgeHandle {
        pub fn create(os_id: &str) -> Result<Self> {
            let shmem = ShmemConf::new()
                .size(std::mem::size_of::<WorkerBridge>())
                .os_id(os_id)
                .create()
                .with_context(|| format!("failed to create worker_bridge shmem {os_id}"))?;
            // Initialise every slot to IDLE so a stray wake before the
            // first dispatch doesn't make a worker go process plugin 0.
            unsafe {
                let bridge = shmem.as_ptr() as *mut WorkerBridge;
                std::ptr::write(bridge, WorkerBridge::zeroed());
            }
            Ok(Self { shmem })
        }

        pub fn open(os_id: &str) -> Result<Self> {
            let shmem = ShmemConf::new()
                .os_id(os_id)
                .open()
                .with_context(|| format!("failed to open worker_bridge shmem {os_id}"))?;
            anyhow::ensure!(
                shmem.len() >= std::mem::size_of::<WorkerBridge>(),
                "worker_bridge shmem too small: {} < {}",
                shmem.len(),
                std::mem::size_of::<WorkerBridge>()
            );
            Ok(Self { shmem })
        }

        pub fn bridge(&self) -> &WorkerBridge {
            unsafe { &*(self.shmem.as_ptr() as *const WorkerBridge) }
        }
    }

    unsafe impl Send for WorkerBridgeHandle {}
    unsafe impl Sync for WorkerBridgeHandle {}
}

#[cfg(windows)]
pub use shmem_handle::WorkerBridgeHandle;
