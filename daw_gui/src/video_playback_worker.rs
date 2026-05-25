//! Background worker thread for video frame decode
//! (`docs/plan_video.md` §3 P5 lookahead architecture).
//!
//! Replaces the synchronous `VideoPlaybackEngine::decode_at` call that
//! lived on the GUI thread. With the worker, the GUI never blocks on
//! WMF — the user's transport controls (Stop / Play / seek) are
//! responsive even when the project source is high-resolution / high-
//! framerate (= the 5-second-freeze-on-Stop pathology in synchronous
//! 1080p60 mode).
//!
//! ## Data flow
//!
//! ```text
//!   GUI thread                          worker thread
//!   ─────────                          ─────────────
//!   request(id, path, micros)  ──────► pending: HashMap<id, req>
//!                                       (latest entry per id wins;
//!                                        worker drains the whole map
//!                                        each cycle)
//!                                       │
//!                                       ▼
//!                                      VideoPlaybackEngine::decode_at
//!                                       (per-source IMFSourceReader,
//!                                        seek-or-forward-walk)
//!                                       │
//!                                       ▼
//!   drain_results()           ◄──────  result_tx.send(DecodeResult)
//!     ──► upload to TextureHandle
//! ```
//!
//! ## Coalescing
//!
//! `pending` is a `Mutex<HashMap<VideoSourceId, PendingRequest>>` so
//! the GUI thread can replace any outstanding request for the same
//! source before the worker picks it up (= "latest target wins" without
//! a channel backlog). The worker drains the whole map per cycle so
//! multi-track composite sees all active sources updated together.
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

use crate::video_playback::{DecodedFrame, VideoPlaybackEngine};

#[derive(Clone)]
struct PendingRequest {
    source_path: PathBuf,
    target_micros: u64,
}

/// One decode outcome handed back to the GUI thread. `frame` is the
/// `Result` directly from `VideoPlaybackEngine::decode_at` — failures
/// are passed through so the caller can log + skip the layer for that
/// cycle (same idiom as the previous synchronous code).
pub struct DecodeResult {
    pub source_id: VideoSourceId,
    pub target_micros: u64,
    pub frame: Result<DecodedFrame, String>,
}

/// Handle owned by `RunnerState`. Spawns the worker on `new`, joins it
/// on `Drop`. Cheap to clone? No — it's not cloneable on purpose, the
/// single owner is responsible for shutdown.
pub struct PreviewDecodeWorker {
    pending: Arc<Mutex<HashMap<VideoSourceId, PendingRequest>>>,
    has_pending: Arc<Condvar>,
    shutdown: Arc<AtomicBool>,
    result_rx: Receiver<DecodeResult>,
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

    /// GUI-thread API: request a decode for `source_id` at
    /// `target_micros`. The worker will overwrite any outstanding
    /// pending request for the same source (= "latest target wins"),
    /// so calling this every frame is cheap and bounded.
    pub fn request(
        &self,
        source_id: VideoSourceId,
        source_path: PathBuf,
        target_micros: u64,
    ) {
        {
            let mut p = self.pending.lock().expect("pending mutex poisoned");
            p.insert(
                source_id,
                PendingRequest {
                    source_path,
                    target_micros,
                },
            );
        }
        self.has_pending.notify_one();
    }

    /// GUI-thread API: drain any decoded frames that landed since the
    /// last call. Non-blocking; returns `Vec::new()` when the worker
    /// is idle. Caller should upload each `Ok(frame)` into its
    /// per-source GPU texture and use that handle in the composite.
    pub fn drain_results(&self) -> Vec<DecodeResult> {
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
    result_tx: Sender<DecodeResult>,
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
            let frame = engine.decode_at(source_id, &req.source_path, req.target_micros);
            let result = DecodeResult {
                source_id,
                target_micros: req.target_micros,
                frame,
            };
            if result_tx.send(result).is_err() {
                // Receiver dropped (= worker is being torn down).
                return;
            }
        }
    }
}
