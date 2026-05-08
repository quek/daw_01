//! Audio engine ↔ plugin host worker pool の plugin host 側。 audio
//! engine の N workers と 1:1 対応する N 個の worker thread を持つ。
//!
//! # buffer 毎の dispatch
//!
//! 各 worker:
//!   1. 自分専用の `wake` event を待つ (`SetEvent` は audio engine 側)。
//!   2. audio 側が `WorkerBridge::worker_task` の対応 slot に書いた
//!      plugin id を読む。
//!   3. その id を [`PluginRegistry`] で resolve し、 live な entry に
//!      対応していれば `plugin.process()` を呼ぶ。
//!   4. `done` event を signal して audio worker を再開させる。
//!
//! `MainToChild::OpenWorkerPool` で spawn され、 `CloseWorkerPool`
//! (または process 終了) で teardown する。 wake/done event handle
//! は worker 毎に保持し、 worker thread 終了時に close される。
//!
//! # plugin Drop の同期: `DispatchCounter` + `quiesce`
//!
//! plugin は plugin-main thread の `Box<dyn LoadedPlugin>` が所有
//! するが、 worker pool は `PluginRegistry` 経由の raw pointer で
//! deref する。 plugin-main thread が `plugin.process()` 実行中に
//! plugin を drop すると、 worker は free 済み COM object / unload
//! 済み DLL に触れて UAF になる (`daw_plugin_host` が ~84-100ms 後
//! に AV: Windows loader が `DllMain(DLL_PROCESS_DETACH)` の cleanup
//! を完了し、 worker が無効になった code page の中で何かを実行
//! しようとするタイミングと一致する)。
//!
//! audio engine をこのために pause することはできない (ユーザーが
//! 再生中に track を削除するのは許される動作)。 そこで in-flight な
//! `process()` を排出する形で同期する。 worker 毎に単調増加 counter
//! の pair を持ち:
//!
//!   - `enter[i]` は worker `i` が dispatch-critical section に入る
//!     直前 (raw pointer deref の前) に increment する。
//!   - `exit[i]`  は `plugin.process()` が return した後、 raw pointer
//!     view を手放した時点で increment する。
//!
//! plugin-main thread の [`WorkerPool::quiesce`] は `enter` を snapshot
//! し、 全 worker で `exit` がそれに追いつくのを待つ。 ここまで来れば
//! その plugin の `Box` を drop しても safe。 `quiesce` は registry に
//! `None` を publish した **後** に呼ぶ必要があり、 そうすれば新規 dispatch
//! は drop 予定の entry を見つけられない (skip path に流れる)。
//!
//! IDLE wake / `None` registry slot でも counter は bump する。 deref
//! の前後を SeqCst 全順序で確実に挟むためで、 ペアになっていれば
//! `quiesce` の wait は伸びない。

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use anyhow::Result;
use common::plugin_ref::open_named_event;
use common::process_data::{Event, EventKind};
use common::worker_bridge::{MAX_WORKERS, WorkerBridge, WorkerBridgeHandle};
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

/// `unsafe { &mut *entry.plugin.0 }` の deref を守る、 worker 毎の
/// `enter` / `exit` counter ペア。 詳細は module-level docs 参照。
///
/// すべての increment は `SeqCst`。 これにより counter 間および
/// `PluginRegistry` の update との総順序が成立する (`arc_swap` の
/// 内部 ordering に依存しない)。 `SeqCst` の fetch_add は x86_64 では
/// `AcqRel` と同コスト (`lock` prefix 付き RMW)、 ARM64 でも `dmb ish`
/// が 1 つ増える程度。 dispatch path は CLAP/VST3 process() で十分
/// 重いので、 この差は誤差。
struct DispatchCounter {
    enter: [AtomicU64; MAX_WORKERS],
    exit: [AtomicU64; MAX_WORKERS],
}

impl DispatchCounter {
    fn new() -> Self {
        Self {
            enter: [const { AtomicU64::new(0) }; MAX_WORKERS],
            exit: [const { AtomicU64::new(0) }; MAX_WORKERS],
        }
    }

    /// worker が dispatch-critical section に入る直前に呼ぶ
    /// (`unsafe { &mut *entry.plugin.0 }` の前)。 [`Self::exit`] と pair。
    #[inline]
    fn enter(&self, idx: usize) {
        self.enter[idx].fetch_add(1, Ordering::SeqCst);
    }

    /// worker が `plugin.process()` から return し、 entry の raw pointer
    /// view を手放した直後に呼ぶ。 [`Self::enter`] と pair。
    #[inline]
    fn exit(&self, idx: usize) {
        self.exit[idx].fetch_add(1, Ordering::SeqCst);
    }
}

/// Owns every worker thread and the shared shutdown flag. Dropped (or
/// `shutdown()`-ed) on CloseWorkerPool / process exit.
pub struct WorkerPool {
    workers: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    /// Wake events kept here so `shutdown()` can `SetEvent` each one and
    /// release the worker from its `WaitForSingleObject`.
    wake_events: Vec<HANDLE>,
    /// [`Self::quiesce`] が参照する `enter` / `exit` counter pair。
    dispatch: Arc<DispatchCounter>,
    /// 実際に起動した worker 数。 `dispatch.enter`/`exit` の
    /// `n_workers` 以降 の slot は `run_worker` から触らないので、
    /// `quiesce` は `[0, n_workers)` のみを iterate する。
    n_workers: u32,
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
        let dispatch = Arc::new(DispatchCounter::new());
        let mut workers = Vec::with_capacity(n_workers as usize);
        let mut wake_events = Vec::with_capacity(n_workers as usize);

        for i in 0..n_workers as usize {
            let wake = open_named_event(&wake_event_names[i])?;
            let done = open_named_event(&done_event_names[i])?;
            wake_events.push(wake);

            let bridge_w = Arc::clone(&bridge);
            let shutdown_w = Arc::clone(&shutdown);
            let registry_w = Arc::clone(&registry);
            let dispatch_w = Arc::clone(&dispatch);
            let idx = i as u32;
            let wake_s = SendableHandle(wake);
            let done_s = SendableHandle(done);
            let handle = std::thread::Builder::new()
                .name(format!("plugin-worker-{i}"))
                .spawn(move || {
                    run_worker(
                        idx, bridge_w, shutdown_w, registry_w, dispatch_w, wake_s, done_s,
                    )
                })?;
            workers.push(handle);
        }

        tracing::info!(n_workers, "plugin worker pool started");
        Ok(Self {
            workers,
            shutdown,
            wake_events,
            dispatch,
            n_workers,
        })
    }

    /// 全 worker が in-flight な `process()` を完了するまで待つ。
    /// **plugin-main thread からのみ呼ぶ — RT thread からは呼ばない。**
    ///
    /// 呼び出し側は drop 予定の plugin id 全てについて、 この method を
    /// 呼ぶ **前に** `PluginRegistry` で `None` を publish しておく必要が
    /// ある。 これにより、 まだ critical section に入っていない worker は
    /// drop 対象の entry を見つけられない。
    ///
    /// この method が return した時点で、 `None` publish 前に取られた
    /// registry snapshot を hold したまま `unsafe { &mut *entry.plugin.0 }`
    /// に居る worker は存在しない。 したがって対応する
    /// `Box<dyn LoadedPlugin>` を plugin-main thread で drop しても safe。
    ///
    /// # 実装
    ///
    /// 各 worker `i` について、 `enter[i]` を snapshot し、
    /// `exit[i] >= snap` になるまで poll する。 invariant:
    ///
    ///   - worker のコード path: wake 直後 `enter.fetch_add` → registry
    ///     resolve → unsafe deref → `plugin.process` → `exit.fetch_add`
    ///     (program order、 すべて SeqCst)。 `enter` と deref の間に
    ///     registry を load するので、 load 時に `Some` だった entry の
    ///     pointer は、 対応する `exit` が来るまで hold される。
    ///   - caller の order: `registry.store(None)` → `quiesce()`。
    ///   - 古い (Some) snapshot を観測した worker について: program order
    ///     により `enter.fetch_add` は `registry.load` より前。 その
    ///     `registry.load` は (古い値を観測したので) caller の
    ///     `registry.store(None)` より前。 SeqCst により worker の
    ///     `enter.fetch_add` と caller の `enter.load` の relative order
    ///     が決まるので、 caller は worker の bump を観測して、 対応する
    ///     `exit` を待てる。
    ///   - 新しい (None) snapshot を観測した worker についても、
    ///     IDLE / `None` skip path で `enter` / `exit` は対称に bump
    ///     されるので wait は伸びない。
    ///
    /// 200µs 間隔で poll する。 plugin-main thread は RT ではないので
    /// sleep は問題ない。 200µs は audio buffer 長より十分短いので、
    /// 典型的な RemoveTrack は 1-2 回の poll で抜ける。
    pub fn quiesce(&self) {
        for i in 0..self.n_workers as usize {
            let snap = self.dispatch.enter[i].load(Ordering::SeqCst);
            while self.dispatch.exit[i].load(Ordering::SeqCst) < snap {
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
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
    dispatch: Arc<DispatchCounter>,
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

        // dispatch-critical section を、 観測可能な操作を行う **前** に
        // 開く。 plugin-main thread の `WorkerPool::quiesce` は registry
        // で `None` を publish した後に `enter` を load するので、 ここ
        // で `enter` を bump しておけば happens-before が成立する: 古い
        // `Some` snapshot を見た `registry.load()` は必ず `None` publish
        // より前 → そこから quiesce 側の `enter.load` に SeqCst 全順序で
        // 繋がる → quiesce は our bump を観測して、 下の `exit` まで wait
        // する。
        //
        // IDLE skip / None skip path でも unconditional に bump する
        // (exit と pair) のは、 SeqCst 全順序の証明を分岐 free に保つため。
        // コストは wake あたり cache line 1 本の RMW で、 CLAP/VST3
        // process() に比べれば無視できる。
        dispatch.enter(idx as usize);

        let plugin_id = bridge.bridge().worker_task[idx as usize].load(Ordering::Acquire);
        if plugin_id == WorkerBridge::IDLE {
            dispatch.exit(idx as usize);
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
            dispatch.exit(idx as usize);
            unsafe {
                let _ = SetEvent(done.0);
            }
            continue;
        };

        // SAFETY: 上の `dispatch.enter` で dispatch-critical section
        // に入っている。 `dispatch.exit` (= `plugin.process()` 完了後)
        // が走るまでは plugin-main thread の `WorkerPool::quiesce` が
        // block するので、 この raw pointer の指す `Box` は drop され
        // ない。 happens-before の論証は module-level docs 参照。
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
                        note_id: ev.note_id,
                        key: ev.key,
                        velocity: ev.velocity,
                    },
                },
                EventKind::NoteOff => TimedNoteEvent {
                    time: ev.time,
                    event: NoteTransition::Off {
                        note_id: ev.note_id,
                        key: ev.key,
                    },
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
                    NoteTransition::On { note_id, key, velocity } => Event {
                        kind: EventKind::NoteOn,
                        _pad: [0; 3],
                        time: tev.time,
                        key,
                        channel: 0,
                        _pad1: [0; 2],
                        velocity,
                        param_id: 0,
                        note_id,
                        value: 0.0,
                    },
                    NoteTransition::Off { note_id, key } => Event {
                        kind: EventKind::NoteOff,
                        _pad: [0; 3],
                        time: tev.time,
                        key,
                        channel: 0,
                        _pad1: [0; 2],
                        velocity: 0.0,
                        param_id: 0,
                        note_id,
                        value: 0.0,
                    },
                };
                pd.n_events_out += 1;
            }
        }

        // dispatch-critical section を閉じる。 ここから先 registry
        // entry の pointer を deref しないので、 plugin-main thread が
        // `Box` を drop しても safe になる。
        dispatch.exit(idx as usize);

        // 次の stale wake がよからぬ plugin を起こさないよう、 slot を
        // IDLE に戻す。
        bridge.bridge().worker_task[idx as usize].store(WorkerBridge::IDLE, Ordering::Release);
        unsafe {
            let _ = SetEvent(done.0);
        }
    }
    tracing::info!(worker_idx = idx, "plugin worker exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    /// どの worker も `enter` を bump していない状態では `quiesce`
    /// は即座に return する (in-flight なし → wait loop 抜ける)。
    #[test]
    fn quiesce_returns_immediately_when_idle() {
        let dispatch = Arc::new(DispatchCounter::new());
        let pool = WorkerPool {
            workers: Vec::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            wake_events: Vec::new(),
            dispatch: Arc::clone(&dispatch),
            n_workers: 4,
        };
        let start = Instant::now();
        pool.quiesce();
        assert!(start.elapsed() < Duration::from_millis(5));
    }

    /// `quiesce` は in-flight な worker が `exit` を bump するまで
    /// return しない。 UAF guard の中核を verify する test。
    #[test]
    fn quiesce_waits_for_inflight_dispatch() {
        let dispatch = Arc::new(DispatchCounter::new());
        // slot 2 で in-flight な状態を作る (`enter` だけ bump して
        // 対応する `exit` を出さない)。
        dispatch.enter(2);

        let pool = WorkerPool {
            workers: Vec::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            wake_events: Vec::new(),
            dispatch: Arc::clone(&dispatch),
            n_workers: 4,
        };

        let dispatch_for_releaser = Arc::clone(&dispatch);
        let release_after = Duration::from_millis(50);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(release_after);
            dispatch_for_releaser.exit(2);
        });

        let start = Instant::now();
        pool.quiesce();
        let elapsed = start.elapsed();
        releaser.join().unwrap();

        assert!(
            elapsed >= release_after,
            "quiesce が in-flight worker の exit bump 前に return した \
             (elapsed={elapsed:?}, expected >= {release_after:?}) — UAF guard 破壊"
        );
        // 200µs poll 間隔なので 5ms の遅延は十分許容内。
        assert!(
            elapsed < release_after + Duration::from_millis(5),
            "quiesce の所要時間が想定外に長い ({elapsed:?})"
        );
    }

    /// `quiesce` が snapshot を取った **後** に到着する `enter` bump は
    /// wait を延長してはならない。 そうでないと、 RemoveTrack 中も
    /// 動き続ける audio engine の dispatch によって teardown が
    /// 無限に starve される。
    #[test]
    fn quiesce_ignores_new_enters_after_snapshot() {
        let dispatch = Arc::new(DispatchCounter::new());
        let pool = WorkerPool {
            workers: Vec::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            wake_events: Vec::new(),
            dispatch: Arc::clone(&dispatch),
            n_workers: 2,
        };

        // background thread で enter/exit pair を高速に回す。
        // 「removed 対象でない plugin への dispatch を audio engine が
        // 続けている」 状況の simulation。
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = Arc::clone(&stop);
        let dispatch_w = Arc::clone(&dispatch);
        let busy = std::thread::spawn(move || {
            while !stop_w.load(Ordering::Relaxed) {
                dispatch_w.enter(0);
                dispatch_w.exit(0);
                dispatch_w.enter(1);
                dispatch_w.exit(1);
            }
        });

        // quiesce は snapshot を取った時点で exit が enter に
        // 追いついているので、 即座に return するはず。
        let start = Instant::now();
        pool.quiesce();
        let elapsed = start.elapsed();
        stop.store(true, Ordering::Relaxed);
        busy.join().unwrap();

        assert!(
            elapsed < Duration::from_millis(50),
            "quiesce が継続中の dispatch に starve された (elapsed={elapsed:?})"
        );
    }
}
