//! MMCSS (Multimedia Class Scheduler Service) thin wrapper.
//!
//! Joining the "Pro Audio" task class boosts the calling thread's
//! scheduling priority so it can keep up with low-latency audio buffer
//! deadlines. Handle is dropped via `AvRevertMmThreadCharacteristics` so
//! the revert always pairs with the join even on panic.
//!
//! Failures (no MMCSS available, missing permissions, etc.) are reported
//! by `join_pro_audio` returning `None` — the caller logs and continues
//! at the previously-set thread priority.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};
use windows::core::w;

/// RAII wrapper: drop calls `AvRevertMmThreadCharacteristics` on the
/// stored handle so the thread leaves the MMCSS task class cleanly when
/// the worker exits.
pub struct MmcssJoin {
    handle: HANDLE,
}

impl Drop for MmcssJoin {
    fn drop(&mut self) {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.handle);
        }
    }
}

/// Join the calling thread to the "Pro Audio" MMCSS task class. Returns
/// `None` if MMCSS rejected the request (logged by the caller); a
/// successful join boosts I/O priority for the buffer deadline.
pub fn join_pro_audio() -> Option<MmcssJoin> {
    let mut task_index: u32 = 0;
    let result = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) };
    match result {
        Ok(handle) if !handle.is_invalid() => Some(MmcssJoin { handle }),
        Ok(_) | Err(_) => None,
    }
}
