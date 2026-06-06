//! Background worker thread for video frame decode
//! (`docs/plan_video.md` §3 P5 lookahead architecture, extended in
//! `docs/plan_video_perf.md` P4 to a true `PREVIEW_RING_SIZE`-frame
//! lookahead ring).
//!
//! Replaces the synchronous `VideoPlaybackEngine::decode_at` call that
//! lived on the GUI thread. With the worker, the GUI never blocks on
//! WMF — the user's transport controls (Stop / Play / seek) are
//! responsive even when the project source is high-resolution / high-
//! framerate (= the 5-second-freeze-on-Stop pathology in synchronous
//! 1080p60 mode).
//!
//! ## Data flow (P4 ring snapshot)
//!
//! ```text
//!   GUI thread                          worker thread
//!   ─────────                          ─────────────
//!   request(id, path,                  pending: HashMap<id, req>
//!           center, step)  ──────►      (latest entry per id wins;
//!                                        worker drains the whole map
//!                                        each cycle)
//!                                       │
//!                                       ▼
//!                                      for i in 0..PREVIEW_RING_SIZE:
//!                                        engine.decode_at(id, path,
//!                                          center + i * step,
//!                                          slot_idx = i)
//!                                       │
//!                                       ▼
//!   drain_results()           ◄──────  result_tx.send(DecodedRing)
//!     ──► import per-slot               (= N RingSlots, each with
//!         shared handles                   target_micros + decoded
//!         and present nearest               frame's slot_idx)
//! ```
//!
//! Each `RingSlot` references one slot in the source's `SharedPool`;
//! consecutive ring entries write into independent D3D11 textures so
//! the GUI thread can present slot N while the worker is filling
//! slot N+1 without contention.
//!
//! ## Coalescing
//!
//! `pending` is a `Mutex<HashMap<VideoSourceId, PendingRequest>>` so
//! the GUI thread can replace any outstanding request for the same
//! source before the worker picks it up (= "latest center wins"
//! without a channel backlog). The worker drains the whole map per
//! cycle so multi-track composite sees all active sources updated
//! together.
//!
//! ## COM apartment
//!
//! WMF objects (IMFSourceReader / IMFSample / IMFMediaBuffer) require
//! a COM-initialized apartment. The main thread calls
//! [`crate::import_video::ensure_mf_startup_pub`] which does
//! `CoInitializeEx + MFStartup` once globally; the worker thread needs
//! its own per-thread `CoInitializeEx(MTA)` since COM apartment state
//! is thread-local. `MFStartup` is process-wide so the global guard
//! covers both.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use common::model::VideoSourceId;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

use crate::video_playback::{
    DEFAULT_FORWARD_BUDGET_MICROS, DecodedFrame, PREVIEW_RING_SIZE, VideoPlaybackEngine,
};

#[derive(Clone)]
struct PendingRequest {
    source_path: PathBuf,
    /// Ring center — the target μs the main thread is currently
    /// presenting. `decode_at(target = center)` lands in slot 0.
    center_target_micros: u64,
    /// μs per frame, derived by the caller from the project's
    /// `video_framerate`. Slot `i` decodes
    /// `center_target_micros + i * step_micros`. Worker side never
    /// reaches back into project state — `step_micros` is the entire
    /// look-ahead cadence contract.
    step_micros: u64,
}

/// One slot inside a `DecodedRing`. `target_micros` is the source-side
/// time the slot was decoded for (= what the main thread compares
/// against the current playhead when picking the nearest slot).
/// `frame` is whatever `VideoPlaybackEngine::decode_at` returned for
/// that target; on the HW path the variant carries its own `slot_idx`
/// (= position in `SharedPool::slots`).
pub struct RingSlot {
    pub target_micros: u64,
    pub frame: DecodedFrame,
}

/// One ring snapshot handed back to the GUI thread. Slots are pushed
/// in `target_micros`-ascending order; if any individual slot decode
/// failed (e.g. EOF past the source end) it is silently skipped and
/// the ring is shorter than `PREVIEW_RING_SIZE`. An empty ring is not
/// sent (= worker stays quiet if it could not decode anything).
pub struct DecodedRing {
    pub source_id: VideoSourceId,
    pub slots: Vec<RingSlot>,
}

/// Handle owned by `RunnerState`. Spawns the worker on `new`, joins it
/// on `Drop`. Cheap to clone? No — it's not cloneable on purpose, the
/// single owner is responsible for shutdown.
pub struct PreviewDecodeWorker {
    pending: Arc<Mutex<HashMap<VideoSourceId, PendingRequest>>>,
    has_pending: Arc<Condvar>,
    shutdown: Arc<AtomicBool>,
    result_rx: Receiver<DecodedRing>,
    thread: Option<JoinHandle<()>>,
}

impl PreviewDecodeWorker {
    pub fn new() -> Self {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let has_pending = Arc::new(Condvar::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let (result_tx, result_rx) = mpsc::channel();

        let pending_t = pending.clone();
        let has_pending_t = has_pending.clone();
        let shutdown_t = shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("video-decode-worker".to_string())
            .spawn(move || {
                // Per-thread COM init. MTA matches the main thread's
                // apartment so IMFSourceReader handles are not strictly
                // bound to one thread (we still keep all WMF calls on
                // this one worker, but the apartment choice avoids
                // surprises if a future change crosses threads).
                unsafe {
                    let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
                    if hr.is_err() && hr.0 != RPC_E_CHANGED_MODE.0 {
                        tracing::error!(?hr, "video worker: CoInitializeEx failed");
                        return;
                    }
                }
                // MFStartup is process-wide; share the guard with main.
                if let Err(e) = crate::import_video::ensure_mf_startup_pub() {
                    tracing::error!(error = %e, "video worker: MFStartup failed");
                    return;
                }
                let mut engine = VideoPlaybackEngine::new();
                worker_loop(
                    &mut engine,
                    pending_t,
                    has_pending_t,
                    shutdown_t,
                    result_tx,
                );
            })
            .expect("spawn video-decode-worker");

        Self {
            pending,
            has_pending,
            shutdown,
            result_rx,
            thread: Some(thread),
        }
    }

    /// GUI-thread API: request a ring decode for `source_id` centered
    /// at `center_target_micros`, stepping by `step_micros` per slot
    /// (= `1_000_000 / project.video_framerate`). The worker
    /// overwrites any outstanding pending request for the same source
    /// (= "latest center wins"), so calling this every frame is cheap
    /// and bounded.
    pub fn request(
        &self,
        source_id: VideoSourceId,
        source_path: PathBuf,
        center_target_micros: u64,
        step_micros: u64,
    ) {
        {
            let mut p = self.pending.lock().expect("pending mutex poisoned");
            p.insert(
                source_id,
                PendingRequest {
                    source_path,
                    center_target_micros,
                    step_micros,
                },
            );
        }
        self.has_pending.notify_one();
    }

    /// GUI-thread API: drain any ring snapshots that landed since the
    /// last call. Non-blocking; returns `Vec::new()` when the worker
    /// is idle. Caller imports each ring's per-slot shared handle
    /// into wgpu (if not already cached) and picks the slot nearest
    /// to the current playhead for the present pass.
    pub fn drain_results(&self) -> Vec<DecodedRing> {
        let mut results = Vec::new();
        while let Ok(r) = self.result_rx.try_recv() {
            results.push(r);
        }
        results
    }
}

impl Default for PreviewDecodeWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PreviewDecodeWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.has_pending.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn worker_loop(
    engine: &mut VideoPlaybackEngine,
    pending: Arc<Mutex<HashMap<VideoSourceId, PendingRequest>>>,
    has_pending: Arc<Condvar>,
    shutdown: Arc<AtomicBool>,
    result_tx: Sender<DecodedRing>,
) {
    while !shutdown.load(Ordering::Acquire) {
        // Wait until there is something to do. The Condvar wait
        // releases the Mutex during sleep, so the GUI thread is free
        // to push more requests without contending here.
        let snapshot: Vec<(VideoSourceId, PendingRequest)> = {
            let mut p = pending.lock().expect("pending mutex poisoned");
            loop {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                if !p.is_empty() {
                    break p.drain().collect();
                }
                p = has_pending.wait(p).expect("Condvar wait poisoned");
            }
        };

        for (source_id, req) in snapshot {
            if shutdown.load(Ordering::Acquire) {
                return;
            }

            // docs/plan_video_perf.md P4: decode `PREVIEW_RING_SIZE`
            // consecutive frames into independent `SharedPool` slots.
            // `decode_at` exploits forward-walk between successive
            // targets (each one is `step_micros` ahead, well under
            // the 100ms forward budget), so slots 1..N cost almost
            // nothing extra over slot 0 once the first HW decode is
            // warm.
            let mut ring_slots: Vec<RingSlot> = Vec::with_capacity(PREVIEW_RING_SIZE);
            for i in 0..PREVIEW_RING_SIZE {
                // Observe shutdown mid-ring so teardown/join stays bounded
                // even if a slot decode (incl. an ffmpeg fallback spawn)
                // is slow — the close/exit path must not hang.
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                let slot_idx = i as u8;
                let target =
                    req.center_target_micros.saturating_add((i as u64) * req.step_micros);
                // slot 0 keeps the default seek budget. Slots 1..N allow a
                // forward-walk from the previous slot (which is `step_micros`
                // behind) instead of re-seeking — this only changes behaviour
                // for low-fps sources where `step_micros` > default budget
                // (fps < ~10); for normal fps `step <= default` so the budget
                // stays the default and decode behaviour is unchanged.
                let forward_budget = if i == 0 {
                    DEFAULT_FORWARD_BUDGET_MICROS
                } else {
                    req.step_micros.max(DEFAULT_FORWARD_BUDGET_MICROS)
                };
                match engine.decode_at(
                    source_id,
                    &req.source_path,
                    target,
                    slot_idx,
                    forward_budget,
                ) {
                    Ok(frame) => ring_slots.push(RingSlot {
                        target_micros: target,
                        frame,
                    }),
                    Err(e) => {
                        // Likely EOS past the source's last frame; log
                        // once at debug level (avoid spamming at the
                        // common late-into-clip case) and stop
                        // extending the ring further (= later slots
                        // would just repeat the EOS).
                        tracing::debug!(
                            error = %e,
                            source_id,
                            target,
                            slot_idx,
                            "video worker: ring slot decode failed (truncating ring)"
                        );
                        break;
                    }
                }
            }

            if ring_slots.is_empty() {
                continue;
            }
            if result_tx
                .send(DecodedRing {
                    source_id,
                    slots: ring_slots,
                })
                .is_err()
            {
                // Receiver dropped (= worker is being torn down).
                return;
            }
        }
    }
}
