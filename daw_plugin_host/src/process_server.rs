//! Plugin-host side of the audio-engine ↔ plugin-host worker pool. N
//! worker threads paired 1:1 with the audio engine's N workers.
//!
//! Each worker:
//!   1. waits on its dedicated `wake` event (`SetEvent` from the audio
//!      engine);
//!   2. reads the plugin id the audio side wrote into the matching slot
//!      of the shared `WorkerBridge::worker_task`;
//!   3. runs `plugin.process()` for that instance (PR5 wires the
//!      registry — currently a stub that just signals back);
//!   4. signals its `done` event so the audio worker can resume.
//!
//! Spawned in response to `MainToChild::OpenWorkerPool`. Torn down on
//! `MainToChild::CloseWorkerPool` (or process exit). The wake/done event
//! handles are owned per-worker (closed when the worker thread exits).

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use anyhow::Result;
use common::plugin_ref::open_named_event;
use common::process_data::{Event, EventKind};
use common::worker_bridge::{WorkerBridge, WorkerBridgeHandle};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    GetCurrentThread, INFINITE, SetEvent, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    WaitForSingleObject,
};

use crate::PluginRegistry;
use crate::plugin_instance::{NoteTransition, TimedNoteEvent};

/// `HANDLE` is `*mut c_void` and therefore `!Send`. We only ever wait on
/// or signal these from one thread (the worker) so wrapping them with an
/// explicit `unsafe impl Send` is safe.
#[derive(Copy, Clone)]
struct SendableHandle(HANDLE);
unsafe impl Send for SendableHandle {}

/// Owns every worker thread and the shared shutdown flag. Dropped (or
/// `shutdown()`-ed) on CloseWorkerPool / process exit.
pub struct WorkerPool {
    workers: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    /// Wake events kept here so `shutdown()` can `SetEvent` each one and
    /// release the worker from its `WaitForSingleObject`.
    wake_events: Vec<HANDLE>,
}

impl WorkerPool {
    pub fn open(
        n_workers: u32,
        worker_bridge_shmem_id: &str,
        wake_event_names: &[String],
        done_event_names: &[String],
        registry: PluginRegistry,
    ) -> Result<Self> {
        anyhow::ensure!(
            wake_event_names.len() == n_workers as usize,
            "wake_event_names len {} != n_workers {}",
            wake_event_names.len(),
            n_workers
        );
        anyhow::ensure!(
            done_event_names.len() == n_workers as usize,
            "done_event_names len {} != n_workers {}",
            done_event_names.len(),
            n_workers
        );
        anyhow::ensure!(
            (n_workers as usize) <= common::worker_bridge::MAX_WORKERS,
            "n_workers {} exceeds MAX_WORKERS {}",
            n_workers,
            common::worker_bridge::MAX_WORKERS
        );

        let bridge = Arc::new(WorkerBridgeHandle::open(worker_bridge_shmem_id)?);
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(n_workers as usize);
        let mut wake_events = Vec::with_capacity(n_workers as usize);

        for i in 0..n_workers as usize {
            let wake = open_named_event(&wake_event_names[i])?;
            let done = open_named_event(&done_event_names[i])?;
            wake_events.push(wake);

            let bridge_w = Arc::clone(&bridge);
            let shutdown_w = Arc::clone(&shutdown);
            let registry_w = Arc::clone(&registry);
            let idx = i as u32;
            let wake_s = SendableHandle(wake);
            let done_s = SendableHandle(done);
            let handle = std::thread::Builder::new()
                .name(format!("plugin-worker-{i}"))
                .spawn(move || {
                    run_worker(idx, bridge_w, shutdown_w, registry_w, wake_s, done_s)
                })?;
            workers.push(handle);
        }

        tracing::info!(n_workers, "plugin worker pool started");
        Ok(Self {
            workers,
            shutdown,
            wake_events,
        })
    }

    pub fn shutdown(self) {
        self.shutdown.store(true, Ordering::Release);
        // Wake every worker so it sees the flag and exits its loop.
        for &wake in &self.wake_events {
            unsafe {
                let _ = SetEvent(wake);
            }
        }
        for h in self.workers {
            if h.join().is_err() {
                tracing::error!("plugin worker thread panicked");
            }
        }
        tracing::info!("plugin worker pool stopped");
    }
}

fn run_worker(
    idx: u32,
    bridge: Arc<WorkerBridgeHandle>,
    shutdown: Arc<AtomicBool>,
    registry: PluginRegistry,
    wake: SendableHandle,
    done: SendableHandle,
) {
    // Best-effort priority boost so we don't lose the CPAL buffer
    // deadline. Failure is logged but non-fatal.
    unsafe {
        let h = GetCurrentThread();
        if let Err(e) = SetThreadPriority(h, THREAD_PRIORITY_TIME_CRITICAL) {
            tracing::warn!(error = ?e, worker_idx = idx, "failed to raise plugin worker priority");
        }
    }
    // MMCSS "Pro Audio" task class: held for the worker's lifetime so
    // the OS scheduler keeps `plugin.process()` calls on the realtime
    // priority class. Reverts automatically on Drop.
    let _mmcss = common::mmcss::join_pro_audio();
    if _mmcss.is_none() {
        tracing::warn!(worker_idx = idx, "plugin worker MMCSS join failed");
    }
    // Tell the CLAP `thread_check` extension this thread counts as an
    // audio thread, so plugins calling `host.is_audio_thread()` from
    // inside `process()` get the correct answer.
    crate::clap_host::mark_audio_thread();
    tracing::info!(worker_idx = idx, "plugin worker started");
    // Pre-allocated event-conversion buffer so the RT path doesn't
    // touch the allocator during dispatch.
    let mut events_in: Vec<TimedNoteEvent> = Vec::with_capacity(common::process_data::MAX_EVENTS);
    let mut events_out: Vec<TimedNoteEvent> = Vec::with_capacity(common::process_data::MAX_EVENTS);

    loop {
        unsafe {
            WaitForSingleObject(wake.0, INFINITE);
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let plugin_id = bridge.bridge().worker_task[idx as usize].load(Ordering::Acquire);
        if plugin_id == WorkerBridge::IDLE {
            unsafe {
                let _ = SetEvent(done.0);
            }
            continue;
        }

        let snapshot = registry.load();
        let entry_opt = snapshot
            .get(plugin_id as usize)
            .and_then(|opt| opt.as_ref());
        let Some(entry) = entry_opt else {
            tracing::warn!(plugin_id, "no plugin registered for id");
            bridge.bridge().worker_task[idx as usize]
                .store(WorkerBridge::IDLE, Ordering::Release);
            unsafe {
                let _ = SetEvent(done.0);
            }
            continue;
        };

        // SAFETY: the contract upheld by the plugin-main thread is that
        // `tracks.mutate` is the only path that drops a plugin Box, and
        // it only runs while the audio engine is paused. Worker
        // dispatch synchronises with the wake/done event pair, so we
        // never run process() while a teardown is in flight.
        let plugin = unsafe { &mut *entry.plugin.0 };
        let pd = unsafe { &mut *entry.process_data };
        let frames = pd.frames;
        let n = frames as usize;

        // Decode events_in → TimedNoteEvent. Param events are dropped
        // here — `LoadedPlugin::process` doesn't take them today.
        events_in.clear();
        let n_events_in = pd.n_events_in as usize;
        for ev in &pd.events_in[..n_events_in.min(pd.events_in.len())] {
            let timed = match ev.kind {
                EventKind::NoteOn => TimedNoteEvent {
                    time: ev.time,
                    event: NoteTransition::On {
                        key: ev.key,
                        velocity: ev.velocity,
                    },
                },
                EventKind::NoteOff => TimedNoteEvent {
                    time: ev.time,
                    event: NoteTransition::Off { key: ev.key },
                },
                EventKind::ParamValue => continue,
            };
            events_in.push(timed);
        }
        events_in.sort_unstable_by_key(|e| e.time);

        let (in_a, in_b) = pd.buffer_in.split_at(1);
        let input_audio: [&[f32]; 2] = [&in_a[0][..n], &in_b[0][..n]];

        // PR4 sidechain: build per-aux-port input slices from
        // `pd.buffer_aux_in` + `pd.aux_in_active`. The order is the host's
        // declared aux port order (port 0 first), matching what the
        // plugin's `is_main=false` declarations should be in.
        let aux_inputs: [crate::plugin_instance::AuxInputBuf<'_>;
            common::process_data::MAX_AUX_IN] = std::array::from_fn(|port| {
            let active = pd.aux_in_active[port] != 0;
            crate::plugin_instance::AuxInputBuf {
                active,
                l: &pd.buffer_aux_in[port][0][..n],
                r: &pd.buffer_aux_in[port][1][..n],
            }
        });
        // Reset aux_in_active for the next buffer; the audio engine is
        // responsible for re-asserting it via `NodeOp::SidechainTap`. This
        // keeps stale routing from leaking when the user disconnects the
        // sidechain (no SidechainTap emitted ⇒ aux_in_active stays 0).
        for flag in &mut pd.aux_in_active {
            *flag = 0;
        }

        if let Err(e) =
            plugin.process(frames, &events_in, &input_audio, &aux_inputs)
        {
            tracing::error!(error = ?e, plugin_id, "plugin.process() failed");
        } else {
            // Copy output audio into the shmem.
            if let Some(out_l) = plugin.output_buffer(0) {
                pd.buffer_out[0][..n].copy_from_slice(&out_l[..n]);
            } else {
                pd.buffer_out[0][..n].fill(0.0);
            }
            if let Some(out_r) = plugin.output_buffer(1).or_else(|| plugin.output_buffer(0)) {
                pd.buffer_out[1][..n].copy_from_slice(&out_r[..n]);
            } else {
                pd.buffer_out[1][..n].fill(0.0);
            }

            // Drain plugin output events back into the shmem.
            events_out.clear();
            plugin.drain_out_notes_into(&mut events_out);
            pd.n_events_out = 0;
            for tev in &events_out {
                if pd.n_events_out as usize >= common::process_data::MAX_EVENTS {
                    break;
                }
                let i = pd.n_events_out as usize;
                pd.events_out[i] = match tev.event {
                    NoteTransition::On { key, velocity } => Event {
                        kind: EventKind::NoteOn,
                        _pad: [0; 3],
                        time: tev.time,
                        key,
                        channel: 0,
                        _pad1: [0; 2],
                        velocity,
                        param_id: 0,
                        _pad2: [0; 4],
                        value: 0.0,
                    },
                    NoteTransition::Off { key } => Event {
                        kind: EventKind::NoteOff,
                        _pad: [0; 3],
                        time: tev.time,
                        key,
                        channel: 0,
                        _pad1: [0; 2],
                        velocity: 0.0,
                        param_id: 0,
                        _pad2: [0; 4],
                        value: 0.0,
                    },
                };
                pd.n_events_out += 1;
            }
        }

        // Reset the slot so a stray wake won't fire a stale plugin.
        bridge.bridge().worker_task[idx as usize].store(WorkerBridge::IDLE, Ordering::Release);
        unsafe {
            let _ = SetEvent(done.0);
        }
    }
    tracing::info!(worker_idx = idx, "plugin worker exiting");
}
