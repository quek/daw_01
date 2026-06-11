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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

/// per-worker param-event ring の容量。 plugin GUI で knob をドラッグした
/// ときだけ埋まる。 1024 もあれば 1 buffer 分の gesture を取りこぼさない。
const PARAM_RING_CAP: usize = 1024;

/// plugin-GUI 発の param event の種別。 `PluginEvent` の touch/value/release を
/// RT 経路で alloc せず ring に詰めるための flat tag (enum 変換は drain 側)。
#[derive(Clone, Copy)]
enum RtParamKind {
    Touch,
    Value,
    Release,
}

/// RT worker → drain thread に運ぶ param event 1 件。 `Copy` なので ring の
/// 固定 slot にそのまま書ける (heap なし)。
#[derive(Clone, Copy)]
struct RtParamEvent {
    kind: RtParamKind,
    track: u32,
    index: u32,
    plugin_id: u32,
    param_id: u32,
    value: f64,
}

impl Default for RtParamEvent {
    fn default() -> Self {
        Self {
            kind: RtParamKind::Touch,
            track: 0,
            index: 0,
            plugin_id: 0,
            param_id: 0,
            value: 0.0,
        }
    }
}

/// 固定長 lock-free SPSC ring。 producer = 単一 RT worker thread、 consumer =
/// 単一 drain thread。 RT 側 `push` は「書くだけ」 (alloc/lock/syscall なし、
/// 満杯時は drop)。 worker 1 本につき 1 ring を持つので producer は常に 1 つ、
/// SPSC で十分。 旧実装は RT worker から tokio unbounded mpsc に send して
/// いて block 境界跨ぎで heap alloc していた (code review 2026-06-06 #10)。
struct ParamEventRing {
    buf: Box<[std::cell::UnsafeCell<RtParamEvent>]>,
    /// consumer が次に読む通し index (mod cap で slot)。
    head: AtomicUsize,
    /// producer が次に書く通し index。
    tail: AtomicUsize,
    cap: usize,
}

// SAFETY: SPSC 規律 — producer は tail のみ、 consumer は head のみ進め、
// `head == tail` (空) / `tail - head == cap` (満杯) を atomic で判定するので、
// producer と consumer が同一 slot に同時アクセスすることはない。 RtParamEvent
// は `Copy` (内部に参照/ポインタを持たない)。
unsafe impl Sync for ParamEventRing {}

impl ParamEventRing {
    fn new(cap: usize) -> Self {
        let buf = (0..cap)
            .map(|_| std::cell::UnsafeCell::new(RtParamEvent::default()))
            .collect();
        Self {
            buf,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            cap,
        }
    }

    /// producer (RT worker) 専用。 満杯なら `false` を返して drop する
    /// (= alloc しない)。 RT-safe。
    fn push(&self, ev: RtParamEvent) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= self.cap {
            return false;
        }
        // SAFETY: この slot (`tail % cap`) は head..tail の外 = consumer が
        // 読まない未使用領域。 producer は単一なので排他。
        unsafe {
            *self.buf[tail % self.cap].get() = ev;
        }
        // Release: 上の書き込みを consumer の Acquire load(tail) から可視化。
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// consumer (drain thread) 専用。
    fn pop(&self) -> Option<RtParamEvent> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        // SAFETY: head < tail なので この slot は producer が書き終えて tail を
        // Release で進めた後。 consumer は単一なので排他。
        let ev = unsafe { *self.buf[head % self.cap].get() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(ev)
    }
}

/// `RtParamEvent` を IPC 用 `PluginEvent` へ変換 (drain thread = 非RT で実行)。
fn rt_param_to_event(ev: RtParamEvent) -> crate::PluginEvent {
    match ev.kind {
        RtParamKind::Touch => crate::PluginEvent::PluginParamTouched {
            track: ev.track,
            index: ev.index,
            plugin_id: ev.plugin_id,
            param_id: ev.param_id,
        },
        RtParamKind::Value => crate::PluginEvent::PluginParamValueChanged {
            track: ev.track,
            index: ev.index,
            plugin_id: ev.plugin_id,
            param_id: ev.param_id,
            value: ev.value,
        },
        RtParamKind::Release => crate::PluginEvent::PluginParamGestureEnd {
            track: ev.track,
            index: ev.index,
            plugin_id: ev.plugin_id,
            param_id: ev.param_id,
        },
    }
}

/// drain thread 本体: 全 worker ring を 2ms 間隔で poll し、 拾った param
/// event を `evt_tx` (tokio、 非RT) へ流す。 worker は `shutdown` 前に join
/// されるので、 shutdown 観測時には全 event が ring に揃っており、 最終 drain
/// で取りこぼさない。
fn run_param_drain(
    rings: Vec<Arc<ParamEventRing>>,
    evt_tx: tokio::sync::mpsc::UnboundedSender<crate::PluginEvent>,
    drain_quit: Arc<AtomicBool>,
) {
    loop {
        let mut any = false;
        for ring in &rings {
            while let Some(ev) = ring.pop() {
                any = true;
                let _ = evt_tx.send(rt_param_to_event(ev));
            }
        }
        if drain_quit.load(Ordering::Acquire) {
            // `drain_quit` は teardown が **全 worker を join した後** に立てる
            // (`shutdown` とは別 flag)。 よって break 時点で ring への新規 push
            // は起き得ない。 直近 push 分を最終 drain で確実に拾ってから抜ける。
            for ring in &rings {
                while let Some(ev) = ring.pop() {
                    let _ = evt_tx.send(rt_param_to_event(ev));
                }
            }
            break;
        }
        if !any {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// dispatch-critical section の teardown を `Drop` に集約する guard。
///
/// `plugin.process()` が **panic** すると、 通常 path の `dispatch.exit` /
/// slot を IDLE に戻す store / `SetEvent(done)` が全て skip され、 結果
/// plugin-main thread の [`WorkerPool::quiesce`] が対応する `exit` を永久に
/// 待って hang する。 これらの操作を guard の `Drop` に移すことで、 normal
/// return でも panic unwind でも **必ず** 実行され、 **当 buffer** の quiesce
/// は解ける。 worker thread 自体の生存 (= 以降の buffer も処理し続ける) は
/// `run_worker` が `process()` を `catch_unwind` で囲むことで担保している。
///
/// 操作順は normal path と同一 (exit → slot IDLE 化 → SetEvent)。
struct DispatchGuard<'a> {
    dispatch: &'a DispatchCounter,
    bridge: &'a WorkerBridgeHandle,
    idx: usize,
    done: SendableHandle,
}

impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        // dispatch-critical section を閉じる。 ここから先 registry entry の
        // pointer を deref しないので、 plugin-main thread が `Box` を drop
        // しても safe になる。
        self.dispatch.exit(self.idx);
        // 次の stale wake がよからぬ plugin を起こさないよう、 slot を
        // IDLE に戻す。
        self.bridge.bridge().worker_task[self.idx].store(WorkerBridge::IDLE, Ordering::Release);
        unsafe {
            let _ = SetEvent(self.done.0);
        }
    }
}

/// Owns every worker thread and the shared shutdown flag. Dropped (or
/// `shutdown()`-ed) on CloseWorkerPool / process exit.
pub struct WorkerPool {
    workers: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    /// drain thread 専用の終了 flag。 `shutdown` (= worker 用) とは別にして、
    /// teardown が **全 worker を join した後** にこれを立てることで、 drain の
    /// break 時点では「新規 push が起き得ない」 が真に保証される (worker は
    /// loop 先頭でしか `shutdown` を見ないため、 shutdown 直前に wake された
    /// buffer が ring に trailing push し得る — それを取りこぼさないため)。
    drain_quit: Arc<AtomicBool>,
    /// Wake events kept here so `shutdown()` can `SetEvent` each one and
    /// release the worker from its `WaitForSingleObject`.
    wake_events: Vec<HANDLE>,
    /// [`Self::quiesce`] が参照する `enter` / `exit` counter pair。
    dispatch: Arc<DispatchCounter>,
    /// 実際に起動した worker 数。 `dispatch.enter`/`exit` の
    /// `n_workers` 以降 の slot は `run_worker` から触らないので、
    /// `quiesce` は `[0, n_workers)` のみを iterate する。
    n_workers: u32,
    /// plugin GUI 発の param event を RT worker → 非RT に運ぶ per-worker
    /// SPSC ring (worker 1 本 = 1 ring)。 drain thread が consumer。
    param_rings: Vec<Arc<ParamEventRing>>,
    /// param ring を poll して `evt_tx` へ流す非RT thread。 `shutdown()` で
    /// worker join 後に join する。
    drain_thread: Option<JoinHandle<()>>,
}

impl WorkerPool {
    pub fn open(
        n_workers: u32,
        worker_bridge_shmem_id: &str,
        wake_event_names: &[String],
        done_event_names: &[String],
        registry: PluginRegistry,
        evt_tx: tokio::sync::mpsc::UnboundedSender<crate::PluginEvent>,
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
        let mut param_rings = Vec::with_capacity(n_workers as usize);

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
            // per-worker SPSC ring: この worker が唯一の producer。 pool 側は
            // drain 用に 1 本保持し、 worker 側へ 1 本 move する。
            let ring = Arc::new(ParamEventRing::new(PARAM_RING_CAP));
            param_rings.push(Arc::clone(&ring));
            let handle = std::thread::Builder::new()
                .name(format!("plugin-worker-{i}"))
                .spawn(move || {
                    run_worker(
                        idx, bridge_w, shutdown_w, registry_w, dispatch_w, wake_s, done_s, ring,
                    )
                })?;
            workers.push(handle);
        }

        // drain thread: RT worker が ring に書いた param event を非RT で
        // `evt_tx` (tokio) へ流す。 これで RT 経路から tokio send (alloc) を
        // 排除する。 `evt_tx` はここで move (worker は ring を使うので不要)。
        let drain_rings: Vec<Arc<ParamEventRing>> =
            param_rings.iter().map(Arc::clone).collect();
        let drain_quit = Arc::new(AtomicBool::new(false));
        let drain_quit_w = Arc::clone(&drain_quit);
        let drain_thread = std::thread::Builder::new()
            .name("plugin-param-drain".into())
            .spawn(move || run_param_drain(drain_rings, evt_tx, drain_quit_w))?;

        tracing::info!(n_workers, "plugin worker pool started");
        Ok(Self {
            workers,
            shutdown,
            drain_quit,
            wake_events,
            dispatch,
            n_workers,
            param_rings,
            drain_thread: Some(drain_thread),
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

    pub fn shutdown(mut self) {
        self.teardown();
    }

    /// 全 worker と drain thread を停止・join する。 `shutdown()` と、
    /// `shutdown()` を経ずに drop された場合の `Drop` の両方から呼ばれる。
    /// 既に teardown 済の状態で再呼び出しされても no-op (冪等)。
    fn teardown(&mut self) {
        if self.drain_thread.is_none() && self.workers.is_empty() {
            return;
        }
        self.shutdown.store(true, Ordering::Release);
        // Wake every worker so it sees the flag and exits its loop.
        for &wake in &self.wake_events {
            unsafe {
                let _ = SetEvent(wake);
            }
        }
        for h in self.workers.drain(..) {
            if h.join().is_err() {
                tracing::error!("plugin worker thread panicked");
            }
        }
        // 全 worker が join 済 ⇒ ring への新規 push は起き得ない。 ここで初めて
        // drain に終了を指示する (worker 用 `shutdown` とは別 flag `drain_quit`)。
        // これで drain の break 前に全 push が完了している = trailing event を
        // 取りこぼさない。
        self.drain_quit.store(true, Ordering::Release);
        if let Some(d) = self.drain_thread.take()
            && d.join().is_err()
        {
            tracing::error!("plugin param drain thread panicked");
        }
        tracing::info!("plugin worker pool stopped");
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        // 正常系は `shutdown()` 済でここは no-op (workers drain 済 / drain_thread
        // None)。 `shutdown()` を経ずに drop された異常系 (panic unwind 等) のみ、
        // worker / drain thread の detach を防ぐためここで停止・join する。
        self.teardown();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    idx: u32,
    bridge: Arc<WorkerBridgeHandle>,
    shutdown: Arc<AtomicBool>,
    registry: PluginRegistry,
    dispatch: Arc<DispatchCounter>,
    wake: SendableHandle,
    done: SendableHandle,
    param_ring: Arc<ParamEventRing>,
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
    // Phase 2b: parameter events extracted from `pd.events_in` (= EventKind::
    // ParamValue) and passed to `LoadedPlugin::process` as a second event
    // stream. Pre-allocated like `events_in` so the audio thread never
    // allocates inside the dispatch loop.
    let mut param_events_in: Vec<crate::plugin_instance::TimedParamEvent> =
        Vec::with_capacity(common::process_data::MAX_EVENTS);
    let mut events_out: Vec<TimedNoteEvent> = Vec::with_capacity(common::process_data::MAX_EVENTS);
    // Phase 2c (`docs/plan_automation.md` §7.5): plugin GUI 発の
    // PARAM_GESTURE_BEGIN / PARAM_VALUE を出力 events から拾うための
    // pre-allocated buffer。 plugin.process() 終了後に drain して
    // evt_tx へ送る。
    let mut out_param_touches: Vec<u32> = Vec::with_capacity(64);
    let mut out_param_values: Vec<(u32, f64)> = Vec::with_capacity(common::process_data::MAX_EVENTS);
    // Phase 4 Step C-3: PARAM_GESTURE_END collector。 plugin GUI で knob を
    // release した瞬間に 1 entry。
    let mut out_param_releases: Vec<u32> = Vec::with_capacity(64);

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

        // teardown (dispatch.exit / slot IDLE 化 / SetEvent(done)) を
        // `Drop` に集約する。 これで `plugin.process()` が panic しても
        // `done` が必ず signal され、 plugin-main thread の `quiesce` が
        // 永久 wait して worker pool 全体が hang するのを防ぐ。
        let _guard = DispatchGuard {
            dispatch: &dispatch,
            bridge: &bridge,
            idx: idx as usize,
            done,
        };

        // SAFETY: 上の `dispatch.enter` で dispatch-critical section
        // に入っている。 `dispatch.exit` (= `plugin.process()` 完了後)
        // が走るまでは plugin-main thread の `WorkerPool::quiesce` が
        // block するので、 この raw pointer の指す `Box` は drop され
        // ない。 happens-before の論証は module-level docs 参照。
        let plugin = unsafe { &mut *entry.plugin.0 };
        let pd = unsafe { &mut *entry.process_data };
        // shmem 由来の `frames` を MAX_FRAMES に clamp してから slice する。
        // audio engine 側は常に `<= MAX_FRAMES` を書くが、 shmem は信頼境界
        // の外なので out-of-bounds slice (panic) を防ぐ防御 clamp。
        let n = (pd.frames as usize).min(common::process_data::MAX_FRAMES);
        let frames = n as u32;

        // Decode events_in → TimedNoteEvent / TimedParamEvent.
        // Phase 2b (`docs/plan_automation.md` §8.3): ParamValue events
        // are no longer dropped — they go through to the plugin as
        // CLAP_EVENT_PARAM_VALUE / VST3 IParameterChanges via
        // `LoadedPlugin::process(.., param_events, ..)`.
        events_in.clear();
        param_events_in.clear();
        let n_events_in = pd.n_events_in as usize;
        for ev in &pd.events_in[..n_events_in.min(pd.events_in.len())] {
            match ev.kind {
                EventKind::NoteOn => events_in.push(TimedNoteEvent {
                    time: ev.time,
                    event: NoteTransition::On {
                        note_id: ev.note_id,
                        key: ev.key,
                        velocity: ev.velocity,
                    },
                }),
                EventKind::NoteOff => events_in.push(TimedNoteEvent {
                    time: ev.time,
                    event: NoteTransition::Off {
                        note_id: ev.note_id,
                        key: ev.key,
                    },
                }),
                EventKind::ParamValue => {
                    param_events_in.push(crate::plugin_instance::TimedParamEvent {
                        time: ev.time,
                        param_id: ev.param_id,
                        value: ev.value,
                    });
                }
            }
        }
        events_in.sort_unstable_by_key(|e| e.time);
        param_events_in.sort_unstable_by_key(|e| e.time);

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

        let transport = crate::plugin_instance::TransportContext::from_process_data(pd);
        // builtin / Rust 製 plugin が process() で panic すると unwind が
        // `run_worker` の loop を抜けて worker thread が死に、 以降その idx に
        // dispatch された buffer は誰も done を signal せず audio engine が
        // 永久 hang する (DispatchGuard は当 buffer の quiesce しか救えない)。
        // catch_unwind で unwind を止め worker thread を生かす。 C/C++ plugin
        // の例外はそもそも Rust panic として unwind しないので対象は builtin。
        let process_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            plugin.process(
                frames,
                &events_in,
                &param_events_in,
                &input_audio,
                &aux_inputs,
                &transport,
            )
        }));
        let process_ok = match process_result {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                tracing::error!(error = ?e, plugin_id, "plugin.process() failed");
                false
            }
            Err(_panic) => {
                tracing::error!(
                    plugin_id,
                    "plugin.process() panicked; worker survived, buffer skipped"
                );
                false
            }
        };
        if process_ok {
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
            // Phase 2c: drain plugin-emitted param touches / values。
            //
            // RT 安全 (code review 2026-06-06 #10): この `run_worker` は
            // TIME_CRITICAL + MMCSS の audio dispatch thread。 旧実装は
            // `evt_tx.send` (tokio unbounded mpsc) で block 境界跨ぎの heap
            // alloc を起こしていた (再生中の knob ドラッグで per-buffer 発火)。
            // 現在は per-worker SPSC `param_ring` に「書くだけ」 (alloc/lock/
            // syscall なし、 満杯時は drop) で、 非RT の drain thread が拾って
            // `evt_tx` へ流す。
            out_param_touches.clear();
            out_param_values.clear();
            out_param_releases.clear();
            plugin.drain_out_param_touches_into(&mut out_param_touches);
            plugin.drain_out_param_values_into(&mut out_param_values);
            plugin.drain_out_param_releases_into(&mut out_param_releases);
            if !out_param_touches.is_empty()
                || !out_param_values.is_empty()
                || !out_param_releases.is_empty()
            {
                let track = entry.track;
                let index = entry.index;
                for param_id in out_param_touches.drain(..) {
                    param_ring.push(RtParamEvent {
                        kind: RtParamKind::Touch,
                        track,
                        index,
                        plugin_id,
                        param_id,
                        value: 0.0,
                    });
                }
                for (param_id, value) in out_param_values.drain(..) {
                    param_ring.push(RtParamEvent {
                        kind: RtParamKind::Value,
                        track,
                        index,
                        plugin_id,
                        param_id,
                        value,
                    });
                }
                for param_id in out_param_releases.drain(..) {
                    param_ring.push(RtParamEvent {
                        kind: RtParamKind::Release,
                        track,
                        index,
                        plugin_id,
                        param_id,
                        value: 0.0,
                    });
                }
            }
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

        // dispatch-critical section の teardown (dispatch.exit / slot を
        // IDLE に戻す / SetEvent(done)) は `_guard` の `Drop` がここ (loop
        // body スコープ末尾) で実行する。 panic unwind でも同様に走る。
    }
    tracing::info!(worker_idx = idx, "plugin worker exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    /// param ring は FIFO で push した順に pop できる。
    #[test]
    fn param_ring_push_pop_fifo() {
        let r = ParamEventRing::new(4);
        assert!(r.pop().is_none());
        for i in 0..3 {
            assert!(r.push(RtParamEvent { param_id: i, ..Default::default() }));
        }
        for i in 0..3 {
            assert_eq!(r.pop().unwrap().param_id, i);
        }
        assert!(r.pop().is_none());
    }

    /// 満杯のとき push は false を返して drop し、 pop で空けば再び入る。
    #[test]
    fn param_ring_drops_when_full() {
        let r = ParamEventRing::new(2);
        assert!(r.push(RtParamEvent::default()));
        assert!(r.push(RtParamEvent::default()));
        assert!(!r.push(RtParamEvent::default()), "full ring must drop");
        assert!(r.pop().is_some());
        assert!(r.push(RtParamEvent::default()), "room after pop");
    }

    /// 別スレッドの producer / consumer で全件が順序保存で渡る (SPSC)。
    #[test]
    fn param_ring_spsc_threaded() {
        let r = Arc::new(ParamEventRing::new(64));
        let rp = Arc::clone(&r);
        let producer = std::thread::spawn(move || {
            let mut sent = 0u32;
            while sent < 50_000 {
                if rp.push(RtParamEvent { param_id: sent, ..Default::default() }) {
                    sent += 1;
                }
            }
        });
        let mut got = 0u32;
        while got < 50_000 {
            if let Some(ev) = r.pop() {
                assert_eq!(ev.param_id, got, "SPSC must preserve order");
                got += 1;
            }
        }
        producer.join().unwrap();
        assert_eq!(got, 50_000);
    }

    /// どの worker も `enter` を bump していない状態では `quiesce`
    /// は即座に return する (in-flight なし → wait loop 抜ける)。
    #[test]
    fn quiesce_returns_immediately_when_idle() {
        let dispatch = Arc::new(DispatchCounter::new());
        let pool = WorkerPool {
            workers: Vec::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            drain_quit: Arc::new(AtomicBool::new(false)),
            wake_events: Vec::new(),
            dispatch: Arc::clone(&dispatch),
            n_workers: 4,
            param_rings: Vec::new(),
            drain_thread: None,
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
            drain_quit: Arc::new(AtomicBool::new(false)),
            wake_events: Vec::new(),
            dispatch: Arc::clone(&dispatch),
            n_workers: 4,
            param_rings: Vec::new(),
            drain_thread: None,
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
            drain_quit: Arc::new(AtomicBool::new(false)),
            wake_events: Vec::new(),
            dispatch: Arc::clone(&dispatch),
            n_workers: 2,
            param_rings: Vec::new(),
            drain_thread: None,
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
