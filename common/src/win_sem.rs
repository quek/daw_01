use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CreateSemaphoreW, OpenSemaphoreW, ReleaseSemaphore, SEMAPHORE_ALL_ACCESS,
    WaitForSingleObject,
};
use windows::core::HSTRING;

const INFINITE_MS: u32 = 0xFFFF_FFFF;

/// RAII wrapper around a Win32 named semaphore.
pub struct Semaphore {
    handle: HANDLE,
}

unsafe impl Send for Semaphore {}
unsafe impl Sync for Semaphore {}

impl Semaphore {
    pub fn create(name: &str, initial: i32, max: i32) -> Result<Self> {
        let wname = HSTRING::from(name);
        let handle = unsafe { CreateSemaphoreW(None, initial, max, &wname) }
            .with_context(|| format!("CreateSemaphoreW {name}"))?;
        Ok(Self { handle })
    }

    pub fn open(name: &str) -> Result<Self> {
        let wname = HSTRING::from(name);
        let handle = unsafe { OpenSemaphoreW(SEMAPHORE_ALL_ACCESS, false, &wname) }
            .with_context(|| format!("OpenSemaphoreW {name}"))?;
        Ok(Self { handle })
    }

    pub fn wait(&self) -> Result<()> {
        let r = unsafe { WaitForSingleObject(self.handle, INFINITE_MS) };
        anyhow::ensure!(r == WAIT_OBJECT_0, "WaitForSingleObject returned {r:?}");
        Ok(())
    }

    /// Returns `Ok(true)` if the semaphore was acquired, `Ok(false)` on timeout.
    pub fn wait_timeout_ms(&self, ms: u32) -> Result<bool> {
        let r = unsafe { WaitForSingleObject(self.handle, ms) };
        if r == WAIT_OBJECT_0 {
            Ok(true)
        } else if r == WAIT_TIMEOUT {
            Ok(false)
        } else {
            anyhow::bail!("WaitForSingleObject returned {r:?}")
        }
    }

    pub fn release(&self) -> Result<()> {
        unsafe { ReleaseSemaphore(self.handle, 1, None) }.context("ReleaseSemaphore")?;
        Ok(())
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
