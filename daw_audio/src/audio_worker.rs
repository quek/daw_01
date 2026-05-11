//! Audio-engine worker pool. Pairs 1:1 with the plugin host's worker
//! pool: each audio worker `i` dispatches plugin work to plugin-host
//! worker `i` via `WorkerSyncRef[i]`.
//!
//! Master calls `dispatch_and_wait` once per buffer. The master itself
//! also drains the work queue (work-stealing fanout) so machines with
//! `available_parallelism() == 1` still make progress without spawning.
//!
//! All shared state crosses the worker boundary as raw pointers stashed
//! in atomics. The dispatch handshake (wake events + pending counter +
//! all_done event) guarantees those pointers are only read while the
//! master holds the matching memory exclusively.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering,
};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use common::model::Song;
use common::plugin_ref::{PluginRef, WorkerSyncRef};
use common::protocol::PluginSlot;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    GetCurrentThread, INFINITE, SetEvent, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    WaitForSingleObject,
};

use crate::engine::process_track_owned;
use crate::mixer::TrackScratch;

#[derive(Copy, Clone)]
struct SendableHandle(HANDLE);
unsafe impl Send for SendableHandle {}
unsafe impl Sync for SendableHandle {}

/// Raw-pointer view of the per-buffer state.
///
/// Master writes the pointers + scalars before signalling the workers;
/// workers (and master itself) only read while the dispatch is in
/// flight.
pub struct DispatchShared {
    pub next_track: AtomicU32,
    pub n_tracks: AtomicU32,
    pub pending: AtomicU32,

    pub song_ptr: AtomicPtr<Song>,
    pub scratch_base: AtomicPtr<TrackScratch>,
    pub plugin_refs_ptr: AtomicPtr<HashMap<u32, PluginRef>>,
    pub slot_map_ptr: AtomicPtr<HashMap<(u32, PluginSlot), u32>>,
    /// PR6: per-buffer audio clip render snapshot. `null` ⇒ workers
    /// pass `None` to `process_track_owned` (= 旧挙動互換、 audio
    /// clip mix を skip)。
    pub audio_renderer_ptr: AtomicPtr<crate::audio_clip_renderer::AudioClipRenderer>,
    pub worker_syncs_base: AtomicPtr<WorkerSyncRef>,
    pub n_worker_syncs: AtomicU32,
    pub master_l_base: AtomicPtr<f32>,
    pub master_r_base: AtomicPtr<f32>,
    pub master_len: AtomicU32,
    pub sample_rate: AtomicU32,
    pub playhead: AtomicU64,
    pub frames: AtomicU32,
    pub playing: AtomicU8,
    pub any_solo: AtomicU8,
    /// PR4.5 sidechain plugin-internal alignment: per-track input delay
    /// in samples (= `Schedule::input_delay_per_track` snapshotted into
    /// the worker pool's shared state for the current dispatch). `null`
    /// means the engine's main loop didn't pass a slice (fallback / not
    /// yet wired); workers treat that as 0 delay for every track.
    pub input_delays_base: AtomicPtr<u32>,
    pub n_input_delays: AtomicU32,
    /// Phase 4 Step C-2: 「現在 recording 中の lane」 set への ptr
    /// (= `SharedState.recording_lanes.load()` 結果)。 master が dispatch
    /// 前に store、 workers + master が `fill_track_param_ramps` の引数に
    /// 渡して curve eval を bypass する判定に使う。 null → 空 set 相当
    /// (= 全 lane eval、 旧挙動)。
    pub recording_lanes_ptr:
        AtomicPtr<std::collections::HashSet<(u32, common::model::AutomationTarget)>>,
}

unsafe impl Send for DispatchShared {}
unsafe impl Sync for DispatchShared {}

impl DispatchShared {
    pub fn new() -> Self {
        Self {
            next_track: AtomicU32::new(0),
            n_tracks: AtomicU32::new(0),
            pending: AtomicU32::new(0),
            song_ptr: AtomicPtr::new(std::ptr::null_mut()),
            scratch_base: AtomicPtr::new(std::ptr::null_mut()),
            plugin_refs_ptr: AtomicPtr::new(std::ptr::null_mut()),
            slot_map_ptr: AtomicPtr::new(std::ptr::null_mut()),
            audio_renderer_ptr: AtomicPtr::new(std::ptr::null_mut()),
            worker_syncs_base: AtomicPtr::new(std::ptr::null_mut()),
            n_worker_syncs: AtomicU32::new(0),
            master_l_base: AtomicPtr::new(std::ptr::null_mut()),
            master_r_base: AtomicPtr::new(std::ptr::null_mut()),
            master_len: AtomicU32::new(0),
            sample_rate: AtomicU32::new(48_000),
            playhead: AtomicU64::new(0),
            frames: AtomicU32::new(0),
            playing: AtomicU8::new(0),
            any_solo: AtomicU8::new(0),
            input_delays_base: AtomicPtr::new(std::ptr::null_mut()),
            n_input_delays: AtomicU32::new(0),
            recording_lanes_ptr: AtomicPtr::new(std::ptr::null_mut()),
        }
    }
}

impl Default for DispatchShared {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AudioWorkerPool {
    workers: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    wakes: Vec<SendableHandle>,
    all_done: SendableHandle,
    shared: Arc<DispatchShared>,
}

impl AudioWorkerPool {
    /// Spawn `n_workers` worker threads + create the wake/all_done
    /// events. Each worker idles on its dedicated wake event.
    pub fn new(n_workers: u32) -> Result<Self> {
        let shared = Arc::new(DispatchShared::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        // One unnamed all_done event; created with auto-reset, initial
        // non-signaled. We don't need a named event because both ends
        // live in the same process.
        let all_done = SendableHandle(create_anonymous_event()?);
        let mut wakes = Vec::with_capacity(n_workers as usize);
        for _ in 0..n_workers {
            wakes.push(SendableHandle(create_anonymous_event()?));
        }

        let mut workers = Vec::with_capacity(n_workers as usize);
        for (i, &wake) in wakes.iter().enumerate() {
            let shared_w = Arc::clone(&shared);
            let shutdown_w = Arc::clone(&shutdown);
            let all_done_w = all_done;
            let handle = std::thread::Builder::new()
                .name(format!("audio-worker-{i}"))
                .spawn(move || run_worker(shared_w, shutdown_w, wake, all_done_w))?;
            workers.push(handle);
        }

        Ok(Self {
            workers,
            shutdown,
            wakes,
            all_done,
            shared,
        })
    }

    /// Run one buffer of work. Master writes the dispatch state into
    /// `shared`, signals every worker, then participates in the
    /// work-stealing loop itself before waiting for the all_done event.
    ///
    /// SAFETY: caller must hold exclusive ownership of every referenced
    /// `&mut` slice / map for the duration of this call. The dispatch
    /// barrier (wake/all_done) ensures the workers only touch this
    /// state inside the call.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_and_wait(
        &self,
        song: Option<&Song>,
        scratch: &mut [TrackScratch],
        plugin_refs: &HashMap<u32, PluginRef>,
        slot_map: &HashMap<(u32, PluginSlot), u32>,
        audio_renderer: &crate::audio_clip_renderer::AudioClipRenderer,
        worker_syncs: &[WorkerSyncRef],
        master_l: &mut [f32],
        master_r: &mut [f32],
        sample_rate: u32,
        playhead: u64,
        frames: u32,
        playing: bool,
        any_solo: bool,
        input_delay_per_track: &[u32],
        recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    ) {
        let n_tracks = song.map(|s| s.tracks.len() as u32).unwrap_or(0);
        let n_tracks = n_tracks.min(scratch.len() as u32);
        if n_tracks == 0 {
            return;
        }

        // Publish raw pointers + scalars into the shared state.
        self.shared
            .song_ptr
            .store(song.map_or(std::ptr::null_mut(), |s| s as *const Song as *mut Song), Ordering::Release);
        self.shared
            .scratch_base
            .store(scratch.as_mut_ptr(), Ordering::Release);
        self.shared.plugin_refs_ptr.store(
            plugin_refs as *const _ as *mut _,
            Ordering::Release,
        );
        self.shared
            .slot_map_ptr
            .store(slot_map as *const _ as *mut _, Ordering::Release);
        self.shared.audio_renderer_ptr.store(
            audio_renderer as *const _ as *mut _,
            Ordering::Release,
        );
        self.shared.worker_syncs_base.store(
            worker_syncs.as_ptr() as *mut WorkerSyncRef,
            Ordering::Release,
        );
        self.shared
            .n_worker_syncs
            .store(worker_syncs.len() as u32, Ordering::Release);
        self.shared
            .master_l_base
            .store(master_l.as_mut_ptr(), Ordering::Release);
        self.shared
            .master_r_base
            .store(master_r.as_mut_ptr(), Ordering::Release);
        self.shared
            .master_len
            .store(master_l.len() as u32, Ordering::Release);
        self.shared.sample_rate.store(sample_rate, Ordering::Release);
        self.shared.playhead.store(playhead, Ordering::Release);
        self.shared.frames.store(frames, Ordering::Release);
        self.shared
            .playing
            .store(if playing { 1 } else { 0 }, Ordering::Release);
        self.shared
            .any_solo
            .store(if any_solo { 1 } else { 0 }, Ordering::Release);
        self.shared.recording_lanes_ptr.store(
            recording_lanes as *const _ as *mut _,
            Ordering::Release,
        );
        // PR4.5: publish per-track input delay slice so workers can read
        // their track's value without locking. Empty slice (= no
        // sidechain wiring anywhere) → null pointer + len 0.
        if input_delay_per_track.is_empty() {
            self.shared
                .input_delays_base
                .store(std::ptr::null_mut(), Ordering::Release);
            self.shared.n_input_delays.store(0, Ordering::Release);
        } else {
            self.shared.input_delays_base.store(
                input_delay_per_track.as_ptr() as *mut u32,
                Ordering::Release,
            );
            self.shared
                .n_input_delays
                .store(input_delay_per_track.len() as u32, Ordering::Release);
        }

        let n_workers = self.workers.len() as u32;
        // pending = N workers; master itself is *also* a runner but is
        // not counted because it joins the work-stealing loop directly
        // and waits for all_done after.
        self.shared.next_track.store(0, Ordering::Release);
        self.shared.n_tracks.store(n_tracks, Ordering::Release);
        self.shared.pending.store(n_workers, Ordering::Release);

        // Wake all workers.
        for w in &self.wakes {
            unsafe {
                let _ = SetEvent(w.0);
            }
        }

        // Master joins the work-stealing loop too. This way machines
        // with n_workers == 0 / 1 still make progress and we get the
        // full physical-core throughput.
        run_work_loop(&self.shared);

        // Wait for the last worker to flag completion.
        unsafe {
            WaitForSingleObject(self.all_done.0, INFINITE);
        }
    }

    pub fn shutdown(self) {
        self.shutdown.store(true, Ordering::Release);
        for w in &self.wakes {
            unsafe {
                let _ = SetEvent(w.0);
            }
        }
        for h in self.workers {
            if h.join().is_err() {
                tracing::error!("audio worker thread panicked");
            }
        }
    }

    pub fn n_workers(&self) -> usize {
        self.workers.len()
    }
}

fn run_worker(
    shared: Arc<DispatchShared>,
    shutdown: Arc<AtomicBool>,
    wake: SendableHandle,
    all_done: SendableHandle,
) {
    boost_thread_priority("audio worker");
    // Join "Pro Audio" so MMCSS keeps this thread on the priority-class
    // schedule the audio mixer/sequencer loop relies on. Held until the
    // worker drops out of `run_worker`, then auto-reverted.
    let _mmcss = common::mmcss::join_pro_audio();
    if _mmcss.is_none() {
        tracing::warn!("audio worker: MMCSS join (Pro Audio) failed");
    }
    loop {
        unsafe {
            WaitForSingleObject(wake.0, INFINITE);
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        run_work_loop(&shared);
        // Last worker out signals completion.
        if shared.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            unsafe {
                let _ = SetEvent(all_done.0);
            }
        }
    }
}

/// Pull tracks off `next_track` until `n_tracks`, calling
/// `process_track_owned` for each.
fn run_work_loop(shared: &DispatchShared) {
    let n_tracks = shared.n_tracks.load(Ordering::Acquire);
    if n_tracks == 0 {
        return;
    }
    let song_ptr = shared.song_ptr.load(Ordering::Acquire);
    let scratch_base = shared.scratch_base.load(Ordering::Acquire);
    let plugin_refs_ptr = shared.plugin_refs_ptr.load(Ordering::Acquire);
    let slot_map_ptr = shared.slot_map_ptr.load(Ordering::Acquire);
    let audio_renderer_ptr = shared.audio_renderer_ptr.load(Ordering::Acquire);
    let worker_syncs_base = shared.worker_syncs_base.load(Ordering::Acquire);
    let n_worker_syncs = shared.n_worker_syncs.load(Ordering::Acquire);
    let sample_rate = shared.sample_rate.load(Ordering::Acquire);
    let playhead = shared.playhead.load(Ordering::Acquire);
    let frames = shared.frames.load(Ordering::Acquire);
    let playing = shared.playing.load(Ordering::Acquire) != 0;
    let any_solo = shared.any_solo.load(Ordering::Acquire) != 0;
    // PR4.5 sidechain plugin-internal alignment: input-delay slice
    // published by `dispatch_and_wait`. May be null (no sidechain wiring),
    // in which case every track gets 0 delay.
    let input_delays_base = shared.input_delays_base.load(Ordering::Acquire);
    let n_input_delays = shared.n_input_delays.load(Ordering::Acquire);
    // Phase 4 Step C-2: recording lane snapshot ptr。 null なら 旧挙動互換
    // (= empty set、 全 lane の curve eval する)。 master が
    // `dispatch_and_wait` 内で store、 ここでは &HashSet として復元する。
    let recording_lanes_ptr = shared.recording_lanes_ptr.load(Ordering::Acquire);
    let empty_recording_lanes: std::collections::HashSet<(u32, common::model::AutomationTarget)> =
        std::collections::HashSet::new();
    let recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)> =
        if recording_lanes_ptr.is_null() {
            &empty_recording_lanes
        } else {
            // SAFETY: master holds the ArcSwap Guard / Arc snapshot alive
            // for the dispatch window via `dispatch_and_wait` 's local var.
            unsafe { &*recording_lanes_ptr }
        };

    if scratch_base.is_null()
        || plugin_refs_ptr.is_null()
        || slot_map_ptr.is_null()
        || worker_syncs_base.is_null()
    {
        return;
    }

    let song: Option<&Song> = if song_ptr.is_null() {
        None
    } else {
        // SAFETY: master holds the snapshot Arc<Song> for the dispatch
        // window, so the pointee is alive.
        Some(unsafe { &*song_ptr })
    };
    let plugin_refs = unsafe { &*plugin_refs_ptr };
    let slot_map = unsafe { &*slot_map_ptr };
    // PR6: audio_renderer_ptr が null のときは render を skip (= None)。
    let audio_renderer: Option<&crate::audio_clip_renderer::AudioClipRenderer> =
        if audio_renderer_ptr.is_null() {
            None
        } else {
            // SAFETY: master holds the AudioClipRenderer (Guard or
            // ArcSwap snapshot) alive for the dispatch window.
            Some(unsafe { &*audio_renderer_ptr })
        };
    let worker_syncs = unsafe {
        std::slice::from_raw_parts(worker_syncs_base, n_worker_syncs as usize)
    };

    loop {
        let track_idx = shared.next_track.fetch_add(1, Ordering::AcqRel);
        if track_idx >= n_tracks {
            break;
        }
        // Per-track scratch is exclusive to this dispatch via the
        // claim-by-index counter above.
        let scratch = unsafe { &mut *scratch_base.add(track_idx as usize) };
        // Every audio worker has its own WorkerSyncRef (1:1 paired
        // with a plugin-host worker). Master uses worker_syncs[0]
        // when it joins the work loop.
        let ws_idx = (track_idx as usize) % worker_syncs.len().max(1);
        let worker_sync = worker_syncs.get(ws_idx);

        let Some(song) = song else { continue };
        let Some(song_track) = song.tracks.get(track_idx as usize) else {
            continue;
        };

        // PR4.5: read this track's input_delay; falls back to 0 if the
        // master didn't publish a slice (= no sidechain wiring) or the
        // index is out of range (defensive, shouldn't happen).
        let input_delay = if input_delays_base.is_null() || track_idx >= n_input_delays {
            0u32
        } else {
            // SAFETY: master holds the slice alive for the dispatch window
            // (it lives in `Schedule::input_delay_per_track` cached on the
            // engine), and `track_idx < n_input_delays` keeps us in bounds.
            unsafe { *input_delays_base.add(track_idx as usize) }
        };
        // master_{l,r} are reduced sequentially by the master thread
        // after `dispatch_and_wait` returns — workers leave the
        // post-fader audio in `scratch.track_l/r`.
        process_track_owned(
            track_idx,
            song_track,
            scratch,
            plugin_refs,
            slot_map,
            audio_renderer,
            worker_sync,
            sample_rate,
            playhead,
            frames,
            playing,
            Some(song),
            any_solo,
            input_delay,
            recording_lanes,
        );
    }
}

fn create_anonymous_event() -> Result<HANDLE> {
    use windows::Win32::System::Threading::CreateEventA;
    use windows::core::PCSTR;
    unsafe {
        CreateEventA(
            None,
            false,
            false,
            PCSTR(std::ptr::null()),
        )
        .context("CreateEventA failed for anonymous audio worker event")
    }
}

/// Best-effort: raise the calling thread's priority to TIME_CRITICAL so
/// CPAL buffer deadlines aren't missed. Failures are logged and ignored
/// — without admin rights some priority changes silently no-op, which
/// is fine for development.
fn boost_thread_priority(label: &str) {
    unsafe {
        let h = GetCurrentThread();
        if let Err(e) = SetThreadPriority(h, THREAD_PRIORITY_TIME_CRITICAL) {
            tracing::warn!(error = ?e, "{label}: failed to raise thread priority");
        }
    }
}
