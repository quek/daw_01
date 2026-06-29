//! ARA (Audio Random Access) host layer for `daw_plugin_host`.
//!
//! This is the in-process side of ARA: the ARA model graph lives here, next to
//! the plug-in, and the host controller callbacks (audio access, archiving,
//! model updates, …) are serviced from this process. See `docs/plan_ara2.md`.
//!
//! The layer is built up across the ARA implementation steps — companion-API
//! binding (CLAP first, then VST3), host controllers, document lifecycle, and
//! playback-renderer wiring — so a module-wide `dead_code` allowance is kept
//! while the pieces are wired in. `non_camel_case_types` mirrors the C struct
//! names of the hand-written companion glue (as `clap-sys` / `ara-sys` do).
#![allow(dead_code, non_camel_case_types)]

pub mod audio_source;
pub mod clap_ara;
pub mod document;
pub mod extension;
pub mod host_controllers;
pub mod session;
pub mod vst3_ara;

/// Copy a versioned ARA struct from a plug-in pointer, honoring its `structSize`
/// (the mandatory first `ARASize` field of every ARA interface / instance
/// struct). Only the bytes the plug-in actually provides are copied; the rest of
/// our struct — generated from the newest ARA revision — stays zeroed (= `None`
/// fn pointers / null refs).
///
/// This is essential: a plug-in implementing an older, smaller ARA revision
/// allocates a smaller struct, so blindly `read_unaligned`-ing our full-size
/// struct over-reads past its allocation and **segfaults**. Reading individual
/// fields then yields `None` for anything the plug-in doesn't implement.
///
/// # Safety
/// `ptr` must be non-null and point to a valid ARA struct whose first field is
/// its `ARASize` `structSize`. `T` must be a `#[repr(C)]` ARA struct that is
/// valid when zero-initialized (all-`None` fn pointers / null refs / zero scalars).
pub(crate) unsafe fn read_versioned<T>(ptr: *const T) -> T {
    let plugin_size = unsafe { ptr.cast::<ara_sys::ARASize>().read_unaligned() };
    let copy_len = plugin_size.min(core::mem::size_of::<T>());
    let mut buf = core::mem::MaybeUninit::<T>::zeroed();
    unsafe {
        core::ptr::copy_nonoverlapping(ptr.cast::<u8>(), buf.as_mut_ptr().cast::<u8>(), copy_len);
        buf.assume_init()
    }
}

/// Log a milestone in the ARA bring-up sequence (document controller creation →
/// model graph → instance binding). These are FFI-boundary steps where a
/// misbehaving plug-in can crash, so the flow is logged at `info` to make a
/// post-mortem readable. For pinpointing a hard segfault (where the rolling
/// `non_blocking` appender can lose in-flight lines), use the synchronous
/// `--ara-selftest` mode in `main.rs`, which loads a plug-in and prints each
/// step to stdout.
pub(crate) fn trace(msg: &str) {
    tracing::info!(target: "ara", "{msg}");
}
