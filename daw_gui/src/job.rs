use std::mem::size_of;

use anyhow::{Context, Result};
use tokio::process::Child;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

pub struct JobHandle {
    handle: HANDLE,
}

// HANDLE is `*mut c_void`. We own the handle exclusively and never dereference it
// in ways that require &mut; the Win32 job APIs are thread-safe for the operations
// we perform.
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

impl JobHandle {
    pub fn new() -> Result<Self> {
        let handle = unsafe { CreateJobObjectW(None, None) }.context("CreateJobObjectW failed")?;

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let info_size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .expect("JOBOBJECT_EXTENDED_LIMIT_INFORMATION size fits in u32");

        unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                info_size,
            )
        }
        .context("SetInformationJobObject failed")?;

        Ok(Self { handle })
    }

    pub fn assign(&self, child: &Child) -> Result<()> {
        let raw = child
            .raw_handle()
            .ok_or_else(|| anyhow::anyhow!("child has no Windows HANDLE"))?;
        unsafe { AssignProcessToJobObject(self.handle, HANDLE(raw)) }
            .context("AssignProcessToJobObject failed")?;
        Ok(())
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
