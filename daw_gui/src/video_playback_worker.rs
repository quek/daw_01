// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Background worker thread for video frame decode
//! (`docs/plan_video.md` §3 P5).
//!
//! Replaces the synchronous `VideoPlaybackEngine::decode_at` call that
//! lived on the GUI thread. With the worker, the GUI never blocks on
//! decode — the user's transport controls (Stop / Play / seek) are
//! responsive even when the project source is high-resolution / high-
//! framerate (= the 5-second-freeze-on-Stop pathology in synchronous
//! 1080p60 mode).
//!
//! ## Data flow
//!
//! ```text
//!   GUI thread                          worker thread
//!   ─────────                          ─────────────
//!   request(id, path,                  pending: HashMap<id, req>
//!           center)        ──────►      (latest entry per id wins;
//!                                        worker drains the whole map
//!                                        each cycle)
//!                                       │
//!                                       ▼
//!                                      engine.decode_at(id, path, center)
//!                                        → BGRA frame (libav SW)
//!                                       │
//!                                       ▼
//!   drain_results()           ◄──────  result_tx.send(DecodedRing)
//!     ──► upload BGRA                   (= one RingSlot with the center
//!         to per-source texture           target_micros + decoded frame)
//! ```
//!
//! The libav BGRA sink is 1-frame-latest, so the worker decodes only the center
//! frame the GUI is presenting. The `DecodedRing` / `RingSlot` wrapper is kept
//! (a 1-slot snapshot) so the GUI-side drain / nearest-slot code is unchanged;
//! the per-slot GPU-texture lookahead ring was for the HW zero-copy path,
//! removed with Media Foundation (`docs/plan_video_decode_unify.md`).
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
//! Decode is done by the single libav engine (`libav_decoder`); it needs no
//! COM apartment or `MFStartup` (Media Foundation was removed,
//! `docs/plan_video_decode_unify.md`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use common::model::VideoSourceId;

use crate::video_playback::{DEFAULT_FORWARD_BUDGET_MICROS, DecodedFrame, VideoPlaybackEngine};

#[derive(Clone)]
struct PendingRequest {
    source_path: PathBuf,
    /// The target μs (source time) the main thread is currently presenting.
    center_target_micros: u64,
}

/// One slot inside a `DecodedRing`. `target_micros` is the source-side
/// time the slot was decoded for (= what the main thread compares
/// against the current playhead when picking the nearest slot).
/// `frame` is the BGRA frame `VideoPlaybackEngine::decode_at` returned.
pub struct RingSlot {
    pub target_micros: u64,
    pub frame: DecodedFrame,
}

/// A decode snapshot handed back to the GUI thread. With the libav BGRA sink
/// this holds exactly one slot (the center frame); the `Vec` is retained so the
/// GUI-side drain / nearest-slot code is unchanged. An empty snapshot is not
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
                // libav decode needs no COM apartment / MFStartup — Media
                // Foundation was removed (docs/plan_video_decode_unify.md).
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
    /// `center_target_micros` (the frame the GUI is presenting). The worker
    /// overwrites any outstanding pending request for the same source
    /// (= "latest center wins"), so calling this every frame is cheap
    /// and bounded.
    pub fn request(
        &self,
        source_id: VideoSourceId,
        source_path: PathBuf,
        center_target_micros: u64,
    ) {
        {
            let mut p = self.pending.lock().expect("pending mutex poisoned");
            p.insert(
                source_id,
                PendingRequest {
                    source_path,
                    center_target_micros,
                },
            );
        }
        self.has_pending.notify_one();
    }

    /// GUI-thread API: drain any decode snapshots that landed since the
    /// last call. Non-blocking; returns `Vec::new()` when the worker
    /// is idle. Caller uploads each snapshot's BGRA frame into the
    /// per-source preview texture for the present pass.
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

            // libav is a 1-frame-latest BGRA sink, so decode just the center
            // frame the main thread is presenting. The per-slot lookahead ring
            // (independent GPU textures) existed for the HW zero-copy path and
            // was removed with Media Foundation (docs/plan_video_decode_unify.md);
            // the `DecodedRing` wrapper is kept as a 1-slot snapshot so the main
            // thread's drain / nearest-slot code is unchanged.
            let frame = match engine.decode_at(
                source_id,
                &req.source_path,
                req.center_target_micros,
                0,
                DEFAULT_FORWARD_BUDGET_MICROS,
            ) {
                Ok(frame) => frame,
                Err(e) => {
                    // Likely EOS past the source's last frame; log once at
                    // debug level (avoid spamming the common late-into-clip
                    // case) and skip this source for the cycle.
                    tracing::debug!(
                        error = %e,
                        source_id,
                        target = req.center_target_micros,
                        "video worker: decode failed (dropping frame)"
                    );
                    continue;
                }
            };
            if result_tx
                .send(DecodedRing {
                    source_id,
                    slots: vec![RingSlot {
                        target_micros: req.center_target_micros,
                        frame,
                    }],
                })
                .is_err()
            {
                // Receiver dropped (= worker is being torn down).
                return;
            }
        }
    }
}
