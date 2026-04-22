use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use shared_memory::{Shmem, ShmemConf};

pub const SAMPLE_RATE: u32 = 48000;
pub const MAX_FRAMES: u32 = 1024;
pub const CHANNELS: u32 = 2;
pub const SAMPLE_BUFFER_LEN: usize = (MAX_FRAMES * CHANNELS) as usize;

/// Shared memory layout between daw_plugin_host (writer) and daw_audio (reader).
/// daw_audio populates `frames_requested` then signals the request semaphore;
/// daw_plugin_host fills `samples` (interleaved stereo) then signals the ready
/// semaphore.
#[repr(C)]
pub struct AudioBridge {
    pub frames_requested: AtomicU32,
    _pad: u32,
    pub samples: [f32; SAMPLE_BUFFER_LEN],
}

impl AudioBridge {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// Owning handle to the audio shared memory region.
pub struct AudioBridgeHandle {
    shmem: Shmem,
}

impl AudioBridgeHandle {
    pub fn create(os_id: &str) -> Result<Self> {
        let shmem = ShmemConf::new()
            .size(AudioBridge::SIZE)
            .os_id(os_id)
            .create()
            .with_context(|| format!("failed to create shmem {os_id}"))?;
        // Zero-initialize so the AtomicU32 starts at 0 and samples are silent.
        unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, AudioBridge::SIZE) };
        Ok(Self { shmem })
    }

    pub fn open(os_id: &str) -> Result<Self> {
        let shmem = ShmemConf::new()
            .os_id(os_id)
            .open()
            .with_context(|| format!("failed to open shmem {os_id}"))?;
        anyhow::ensure!(
            shmem.len() >= AudioBridge::SIZE,
            "shmem too small: {} < {}",
            shmem.len(),
            AudioBridge::SIZE
        );
        Ok(Self { shmem })
    }

    fn ptr(&self) -> *mut AudioBridge {
        self.shmem.as_ptr() as *mut AudioBridge
    }

    pub fn bridge(&self) -> &AudioBridge {
        unsafe { &*self.ptr() }
    }

    pub fn samples_ptr(&self) -> *mut f32 {
        let bridge = self.ptr();
        unsafe { (&raw mut (*bridge).samples) as *mut f32 }
    }

    pub fn set_frames_requested(&self, n: u32) {
        self.bridge().frames_requested.store(n, Ordering::Release);
    }

    pub fn frames_requested(&self) -> u32 {
        self.bridge().frames_requested.load(Ordering::Acquire)
    }
}

// The underlying shared memory is safe to share across threads; the single
// atomic counter and the sample buffer are protected by the request/ready
// semaphore handshake.
unsafe impl Send for AudioBridgeHandle {}
unsafe impl Sync for AudioBridgeHandle {}

pub fn shmem_id(parent_pid: u32) -> String {
    format!("daw_01_audio_{parent_pid}")
}

pub fn request_sem_id(parent_pid: u32) -> String {
    format!("daw_01_req_{parent_pid}")
}

pub fn ready_sem_id(parent_pid: u32) -> String {
    format!("daw_01_ready_{parent_pid}")
}
