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
pub mod graph_plan;
pub mod host_controllers;
pub mod session;
pub mod states;
pub mod vst3_ara;

use anyhow::Result;

use crate::ara::session::{AraSession, Retired, StartTable};
pub use crate::ara::session::SavedArchive;

/// One `SetupAraDocument` edit handed to a plug-in's ARA session ([`AraSession::set_clips`]).
pub struct AraEdit<'a> {
    pub clips: &'a [common::protocol::AraClipSpec],
    pub bpm: f64,
    pub time_sig: (u16, u16),
    pub archive: Option<SavedArchive<'a>>,
    /// How each created modification starts ([`states::AraStates::resolve_starts`]).
    pub starts: &'a StartTable,
}

/// Backend hooks for the shared ARA lifecycle dance ([`run_setup_ara`] /
/// [`run_clear_ara`]). CLAP / VST3 は「deactivate → set_clips → restore →
/// reactivate」の字面一致コードを別々に持っていた (`docs/plan_arch_refactor.md`
/// §6 B8) — 実体をここに一本化し、backend は自分の activate / deactivate と
/// session slot を差すだけにする。
pub trait AraLifecycleHost {
    fn ara_session(&self) -> Option<&AraSession>;
    fn ara_session_mut(&mut self) -> &mut Option<AraSession>;
    fn is_active(&self) -> bool;
    /// 直近成功した activate の `(sample_rate, min_frames, max_frames)`。
    fn last_activate_params(&self) -> Option<(f64, u32, u32)>;
    fn do_deactivate(&mut self);
    fn do_activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()>;
}

/// Shared `setup_ara`: ARA の `addPlaybackRegion` / region detach は instance
/// inactive を要求するので、更新の前後で deactivate → reactivate する。bind
/// 自体は load 時 (`bind_ara_if_capable`) に済んでいる。グラフの差分編集と、
/// 新しく作った object の状態の restore は [`AraSession::set_clips`]。 戻り値 = destroy した modification の状態
/// (host が取っておく。 reactivate に失敗しても document の編集は済んでいるので返す)。 ARA 非 bind の instance は
/// `None`。
pub fn run_setup_ara(host: &mut dyn AraLifecycleHost, edit: AraEdit<'_>) -> Option<Retired> {
    host.ara_session()?;
    let was_active = host.is_active();
    let restore = host.last_activate_params();
    if was_active {
        host.do_deactivate();
    }
    let retired = host
        .ara_session_mut()
        .as_mut()
        .map(|session| session.set_clips(edit.clips, edit.bpm, edit.time_sig, edit.archive, edit.starts))
        .unwrap_or_default();
    if was_active
        && let Some((sample_rate, min_frames, max_frames)) = restore
        && let Err(e) = host.do_activate(sample_rate, min_frames, max_frames)
    {
        tracing::error!(error = ?e, "setup_ara: reactivate failed");
    }
    Some(retired)
}

/// Shared `clear_ara`: session を drop する間 instance を inactive にし、
/// 元の activation state を復元する。
pub fn run_clear_ara(host: &mut dyn AraLifecycleHost) {
    if host.ara_session().is_none() {
        return;
    }
    let was_active = host.is_active();
    let restore = host.last_activate_params();
    if was_active {
        host.do_deactivate();
    }
    *host.ara_session_mut() = None;
    if was_active
        && let Some((sample_rate, min_frames, max_frames)) = restore
        && let Err(e) = host.do_activate(sample_rate, min_frames, max_frames)
    {
        tracing::error!(error = ?e, "clear_ara: reactivate failed");
    }
}

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
