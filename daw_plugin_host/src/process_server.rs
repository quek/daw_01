//! Audio engine ↔ plugin host worker pool の plugin host 側。 audio
//! engine の N workers と 1:1 対応する N 個の worker thread を持つ。
//!
//! # buffer 毎の dispatch
//!
//! 各 worker:
//!   1. 自分専用の `wake` event を待つ (`SetEvent` は audio engine 側)。
//!   2. audio 側が `WorkerBridge::worker_task` の対応 slot に書いた
//!      **安定 device id (u64)** を読む (v29 — session-unique plugin_id は
//!      廃止)。
//!   3. その id を [`PluginRegistry`] (`HashMap<u64, PluginEntry>` の
//!      ArcSwap snapshot) で resolve し、 live な entry に対応していれば
//!      audio half の `process()` を呼ぶ。
//!   4. `done` event を signal して audio worker を再開させる。
//!
//! # plugin Drop の同期: `DispatchCounter` + `quiesce`
//!
//! Registry entry は [`AudioHalf`] の `Arc` を持つ (v29 — 旧 raw pointer
//! into Box)。 Arc なので stale snapshot が allocation を dangle させる
//! ことは構造的に無いが、 audio half の中の FFI ポインタ (plugin 本体) は
//! main half の Drop で無効になるため、 **アクセスの直列化** は従来どおり
//! quiesce protocol が担う:
//!
//!   - `enter[i]` は worker `i` が dispatch-critical section に入る直前に
//!     increment する。
//!   - `exit[i]` は `process()` return 後、 audio half への参照を手放した
//!     時点で increment する。
//!
//! plugin-main thread の [`WorkerPool::quiesce`] は `enter` を snapshot し、
//! 全 worker で `exit` が追いつくのを待つ。 registry から entry を外して
//! (`registry_remove`) から `quiesce` を呼べば、 以後 worker はその audio
//! half に触れない — そこで初めて main half (と FFI plugin) を安全に
//! deactivate / drop できる。
//!
//! IDLE wake / missing-entry でも counter は bump する (SeqCst 全順序の
//! 論証を分岐 free に保つため)。

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;

use anyhow::Result;
use common::metrics_bridge::MetricsBridgeHandle;
use common::plugin_ref::open_named_event;
use common::process_data::{Event, EventKind};
use common::protocol::PluginEvent;
use common::worker_bridge::{MAX_WORKERS, WorkerBridge, WorkerBridgeHandle};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    GetCurrentThread, INFINITE, SetEvent, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    WaitForSingleObject,
};

use crate::plugin_instance::{AudioHalf, NoteTransition, TimedNoteEvent};

/// Per-plugin process-server entry (v29: device_id keyed, flat)。
/// `audio` は split-half の audio 側 ([`AudioHalf`])、 `process_data` は
/// audio engine が入力を書く shmem slot。
pub struct PluginEntry {
    pub audio: Arc<AudioHalf>,
    pub process_data: *mut common::process_data::ProcessData,
    /// per-publish one-shot: `process()` Err / panic を最初の 1 回だけ
    /// log する (TIME_CRITICAL thread での毎 buffer format+log を排除)。
    /// republish (再 SetSlotPlugin / reinit) で新 entry になりリセット。
    pub err_logged: Arc<AtomicBool>,
    /// L1: この device が claim した metrics slot index のキャッシュ
    /// (`u32::MAX` = 未 claim)。 worker が初回 `process()` で
    /// `MetricsBridge::claim_plugin_metric_slot` を呼んで確定し、 以後 O(1) store。
    pub metric_slot: Arc<AtomicU32>,
    /// r.md #87: この instance へ渡す musical timeline を **曲全体の位置に固定**
    /// するか。既定 (`false`) は行の時間軸 = ランチャーで撃った行の plugin は
    /// セルの拍で動く。`true` になるのは **ARA を bind した instance だけ** —
    /// ARA の playback region は song 時間に固定されており、行の拍を渡すと
    /// Melodyne が曲頭付近を鳴らす (ARA のセル対応は未実装、`vst3_plugin.rs`
    /// の注記)。load 時の ARA bind 結果で確定する定数。
    pub transport_pinned_to_song: bool,
}

/// `PluginEntry::metric_slot` の未 claim sentinel。
pub const METRIC_SLOT_UNCLAIMED: u32 = u32::MAX;

impl Clone for PluginEntry {
    fn clone(&self) -> Self {
        Self {
            audio: Arc::clone(&self.audio),
            process_data: self.process_data,
            err_logged: Arc::clone(&self.err_logged),
            metric_slot: Arc::clone(&self.metric_slot),
            transport_pinned_to_song: self.transport_pinned_to_song,
        }
    }
}

unsafe impl Send for PluginEntry {}
unsafe impl Sync for PluginEntry {}

/// Lock-free `device_id` → [`PluginEntry`] lookup the worker pool reads
/// during dispatch. plugin-main thread が add / remove ごとに新しい
/// `HashMap` を publish する; 古い snapshot は最後の worker guard が落ちる
/// まで生きる (Arc entry なので dangle しない)。
pub type PluginRegistry = Arc<arc_swap::ArcSwap<HashMap<u64, PluginEntry>>>;

/// Publish (insert or replace) one registry entry.
pub fn registry_insert(registry: &PluginRegistry, device_id: u64, entry: PluginEntry) {
    let mut next: HashMap<u64, PluginEntry> = (**registry.load()).clone();
    next.insert(device_id, entry);
    registry.store(Arc::new(next));
}

/// Remove one registry entry, returning it if present.
pub fn registry_remove(registry: &PluginRegistry, device_id: u64) -> Option<PluginEntry> {
    let current = registry.load();
    if !current.contains_key(&device_id) {
        return None;
    }
    let mut next: HashMap<u64, PluginEntry> = (**current).clone();
    let removed = next.remove(&device_id);
    drop(current);
    registry.store(Arc::new(next));
    removed
}

/// Snapshot every entry and clear the registry (ReinitAllPlugins 用)。
pub fn registry_take_all(registry: &PluginRegistry) -> HashMap<u64, PluginEntry> {
    let all: HashMap<u64, PluginEntry> = (**registry.load()).clone();
    registry.store(Arc::new(HashMap::new()));
    all
}

/// Re-publish a set of entries at once (ReinitAllPlugins の republish)。
pub fn registry_restore_all(registry: &PluginRegistry, entries: HashMap<u64, PluginEntry>) {
    registry.store(Arc::new(entries));
}

/// `HANDLE` is `*mut c_void` and therefore `!Send`. We only ever wait on
/// or signal these from one thread (the worker).
#[derive(Copy, Clone)]
struct SendableHandle(HANDLE);
unsafe impl Send for SendableHandle {}

/// audio half への参照を守る、 worker 毎の `enter` / `exit` counter ペア。
/// すべての increment は `SeqCst` (`PluginRegistry` update との総順序)。
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

    /// worker が dispatch-critical section に入る直前に呼ぶ。
    #[inline]
    fn enter(&self, idx: usize) {
        self.enter[idx].fetch_add(1, Ordering::SeqCst);
    }

    /// worker が `process()` から return し audio half への参照を手放した
    /// 直後に呼ぶ。
    #[inline]
    fn exit(&self, idx: usize) {
        self.exit[idx].fetch_add(1, Ordering::SeqCst);
    }
}

/// per-worker param-event ring の容量。
const PARAM_RING_CAP: usize = 1024;

/// plugin-GUI 発の param event の種別 (RT 経路で alloc しない flat tag)。
#[derive(Clone, Copy)]
enum RtParamKind {
    Touch,
    Value,
    Release,
}

/// RT worker → drain thread に運ぶ param event 1 件 (`Copy`、 heap なし)。
#[derive(Clone, Copy)]
struct RtParamEvent {
    kind: RtParamKind,
    device_id: u64,
    param_id: u32,
    value: f64,
}

impl Default for RtParamEvent {
    fn default() -> Self {
        Self {
            kind: RtParamKind::Touch,
            device_id: 0,
            param_id: 0,
            value: 0.0,
        }
    }
}

/// 固定長 lock-free SPSC ring。 producer = 単一 RT worker thread、
/// consumer = 単一 drain thread。 RT 側 `push` は「書くだけ」 (alloc/lock/
/// syscall なし、 満杯時は drop)。
struct ParamEventRing {
    buf: Box<[std::cell::UnsafeCell<RtParamEvent>]>,
    /// consumer が次に読む通し index (mod cap で slot)。
    head: AtomicUsize,
    /// producer が次に書く通し index。
    tail: AtomicUsize,
    cap: usize,
}

// SAFETY: SPSC 規律 — producer は tail のみ、 consumer は head のみ進め、
// 同一 slot への同時アクセスは起きない。 RtParamEvent は `Copy`。
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

    /// producer (RT worker) 専用。 満杯なら `false` を返して drop する。
    fn push(&self, ev: RtParamEvent) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= self.cap {
            return false;
        }
        // SAFETY: この slot は head..tail の外 = consumer が読まない領域。
        unsafe {
            *self.buf[tail % self.cap].get() = ev;
        }
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
        // SAFETY: head < tail なので producer の書き込み完了後。
        let ev = unsafe { *self.buf[head % self.cap].get() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(ev)
    }
}

/// `RtParamEvent` を wire 用 [`PluginEvent`] へ変換 (drain thread = 非RT)。
fn rt_param_to_event(ev: RtParamEvent) -> PluginEvent {
    match ev.kind {
        RtParamKind::Touch => PluginEvent::PluginParamTouched {
            device_id: ev.device_id,
            param_id: ev.param_id,
            // display_name は daw_gui 側で plugin_params cache から解決する
            // (= host での文字列構築は placeholder のみ)。
            display_name: format!("Param {}", ev.param_id),
        },
        RtParamKind::Value => PluginEvent::PluginParamValueChanged {
            device_id: ev.device_id,
            param_id: ev.param_id,
            value: ev.value,
        },
        RtParamKind::Release => PluginEvent::PluginParamGestureEnd {
            device_id: ev.device_id,
            param_id: ev.param_id,
        },
    }
}

/// drain thread 本体: 全 worker ring を poll し、 拾った param event を
/// `evt_tx` (非RT) へ流す。
///
/// r.md #49: 空振り時の sleep は **backoff する**。 旧実装は常に 2ms 固定で、
/// param が 1 つも動いていないアイドル状態でも **毎秒 500 回**このスレッドを
/// 起こしていた (プラグインを 1 つでも読み込めば常時)。 RT 側から `SetEvent` を
/// 打ってイベント駆動にする案は「RT スレッドはシステムコールを最小化する」
/// (CLAUDE.md) と衝突するので採らず、 poll のまま間隔を伸ばす。
///
/// イベントを 1 つでも拾ったら即座に最小間隔へ戻すので、 ノブを回している間の
/// 追従は従来どおり。 静止状態から動かし始めた最初の 1 イベントだけ最大
/// `PARAM_DRAIN_MAX_MS` 遅れるが、 これは GUI の数値表示更新であって音ではない。
fn run_param_drain(
    rings: Vec<Arc<ParamEventRing>>,
    evt_tx: tokio::sync::mpsc::UnboundedSender<PluginEvent>,
    drain_quit: Arc<AtomicBool>,
) {
    const PARAM_DRAIN_MIN_MS: u64 = 2;
    const PARAM_DRAIN_MAX_MS: u64 = 32;
    let mut idle_sleep_ms = PARAM_DRAIN_MIN_MS;
    loop {
        let mut any = false;
        for ring in &rings {
            while let Some(ev) = ring.pop() {
                any = true;
                let _ = evt_tx.send(rt_param_to_event(ev));
            }
        }
        if drain_quit.load(Ordering::Acquire) {
            // `drain_quit` は teardown が全 worker join 後に立てるので、
            // break 時点で新規 push は起き得ない。 最終 drain で拾い切る。
            for ring in &rings {
                while let Some(ev) = ring.pop() {
                    let _ = evt_tx.send(rt_param_to_event(ev));
                }
            }
            break;
        }
        if any {
            idle_sleep_ms = PARAM_DRAIN_MIN_MS;
        } else {
            std::thread::sleep(std::time::Duration::from_millis(idle_sleep_ms));
            idle_sleep_ms = (idle_sleep_ms * 2).min(PARAM_DRAIN_MAX_MS);
        }
    }
}

/// dispatch-critical section の teardown を `Drop` に集約する guard。
/// `process()` が panic しても `exit` / slot IDLE 化 / `SetEvent(done)` が
/// 必ず実行され、 quiesce の永久 wait を防ぐ。
struct DispatchGuard<'a> {
    dispatch: &'a DispatchCounter,
    bridge: &'a WorkerBridgeHandle,
    idx: usize,
    done: SendableHandle,
}

impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        // dispatch-critical section を閉じる。 ここから先 audio half に
        // 触れないので、 plugin-main が entry を drop しても safe。
        self.dispatch.exit(self.idx);
        // 次の stale wake がよからぬ device を起こさないよう IDLE に戻す。
        self.bridge.bridge().worker_task[self.idx].store(WorkerBridge::IDLE, Ordering::Release);
        unsafe {
            let _ = SetEvent(self.done.0);
        }
    }
}

/// Owns every worker thread and the shared shutdown flag.
pub struct WorkerPool {
    workers: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    /// drain thread 専用の終了 flag (worker join 後に立てる)。
    drain_quit: Arc<AtomicBool>,
    /// Wake events kept here so `shutdown()` can release the workers.
    wake_events: Vec<HANDLE>,
    /// [`Self::quiesce`] が参照する counter pair。
    dispatch: Arc<DispatchCounter>,
    /// 実際に起動した worker 数。
    n_workers: u32,
    /// plugin GUI 発の param event を RT → 非RT に運ぶ per-worker SPSC ring。
    param_rings: Vec<Arc<ParamEventRing>>,
    /// param ring を poll して `evt_tx` へ流す非RT thread。
    drain_thread: Option<JoinHandle<()>>,
}

impl WorkerPool {
    pub fn open(
        n_workers: u32,
        worker_bridge_shmem_id: &str,
        metrics_shmem_id: &str,
        wake_event_names: &[String],
        done_event_names: &[String],
        registry: PluginRegistry,
        evt_tx: tokio::sync::mpsc::UnboundedSender<PluginEvent>,
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
        // resource monitor: per-plugin の process() 時間を publish する共有
        // メモリ。
        let metrics = Arc::new(MetricsBridgeHandle::open(metrics_shmem_id)?);
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
            let metrics_w = Arc::clone(&metrics);
            let shutdown_w = Arc::clone(&shutdown);
            let registry_w = Arc::clone(&registry);
            let dispatch_w = Arc::clone(&dispatch);
            let idx = i as u32;
            let wake_s = SendableHandle(wake);
            let done_s = SendableHandle(done);
            let ring = Arc::new(ParamEventRing::new(PARAM_RING_CAP));
            param_rings.push(Arc::clone(&ring));
            let handle = std::thread::Builder::new()
                .name(format!("plugin-worker-{i}"))
                .spawn(move || {
                    run_worker(
                        idx, bridge_w, metrics_w, shutdown_w, registry_w, dispatch_w, wake_s,
                        done_s, ring,
                    )
                })?;
            workers.push(handle);
        }

        // drain thread: RT worker が ring に書いた param event を非RT で
        // `evt_tx` へ流す。
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
    /// 呼び出し側は drop / mutate 予定の device 全てについて、 この method
    /// を呼ぶ **前に** registry から entry を外しておく必要がある
    /// ([`registry_remove`])。 return した時点で、 旧 snapshot を hold した
    /// まま audio half に触れている worker は存在しない。
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

    /// 全 worker と drain thread を停止・join する (冪等)。
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
        // 全 worker join 済 ⇒ ring への新規 push は起き得ない。
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
        // 正常系は `shutdown()` 済で no-op。 panic unwind 等の異常系のみ
        // ここで停止・join する。
        self.teardown();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    idx: u32,
    bridge: Arc<WorkerBridgeHandle>,
    metrics: Arc<MetricsBridgeHandle>,
    shutdown: Arc<AtomicBool>,
    registry: PluginRegistry,
    dispatch: Arc<DispatchCounter>,
    wake: SendableHandle,
    done: SendableHandle,
    param_ring: Arc<ParamEventRing>,
) {
    // Best-effort priority boost so we don't lose the CPAL buffer deadline.
    unsafe {
        let h = GetCurrentThread();
        if let Err(e) = SetThreadPriority(h, THREAD_PRIORITY_TIME_CRITICAL) {
            tracing::warn!(error = ?e, worker_idx = idx, "failed to raise plugin worker priority");
        }
    }
    // MMCSS "Pro Audio" task class (reverts on Drop).
    let _mmcss = common::mmcss::join_pro_audio();
    if _mmcss.is_none() {
        tracing::warn!(worker_idx = idx, "plugin worker MMCSS join failed");
    }
    // CLAP `thread_check`: this thread counts as an audio thread.
    crate::clap_host::mark_audio_thread();
    tracing::info!(worker_idx = idx, "plugin worker started");
    // Pre-allocated event-conversion buffers (RT path never allocates).
    let mut events_in: Vec<TimedNoteEvent> = Vec::with_capacity(common::process_data::MAX_EVENTS);
    // r.md #89: param modulation は制御グリッド化で 1 buffer あたり最大
    // `MAX_PARAM_MODS` 件届く (刻みごと × param 数)。`MAX_EVENTS` (= ノート側の
    // 上限) のままだと audio worker thread で realloc する。
    let mut param_events_in: Vec<crate::plugin_instance::TimedParamEvent> =
        Vec::with_capacity(common::process_data::MAX_RT_EVENT_BUFFER);
    let mut events_out: Vec<TimedNoteEvent> = Vec::with_capacity(common::process_data::MAX_EVENTS);
    let mut out_param_touches: Vec<u32> = Vec::with_capacity(64);
    let mut out_param_values: Vec<(u32, f64)> = Vec::with_capacity(common::process_data::MAX_EVENTS);
    let mut out_param_releases: Vec<u32> = Vec::with_capacity(64);
    // per-worker one-shot: 「registry に居ない device」への dispatch 警告は
    // 同じ id が続く限り 1 回だけ (TIME_CRITICAL thread での毎 buffer log を
    // 排除 — respawn 待ちの間 audio 側は毎 buffer dispatch し続ける)。
    let mut warned_missing: Option<u64> = None;

    loop {
        // 仕事が来るまでの park。不変条件 4 が禁じているのは「**他プロセスの完了待ち**を
        // 無限にすること」で、この wake は同一プロセス内の dispatch 側が起こす。
        // RT deadline を握らない (起きなければ何も走らないだけ)。audio 側から見た
        // この worker の完了待ちは daw_audio が DISPATCH_TIMEOUT_MS で bounded にしている。
        unsafe {
            WaitForSingleObject(wake.0, INFINITE); // arch-lint: allow-infinite
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // dispatch-critical section を、 観測可能な操作の **前** に開く
        // (happens-before の論証は module docs)。
        dispatch.enter(idx as usize);

        let device_id = bridge.bridge().worker_task[idx as usize].load(Ordering::Acquire);
        if device_id == WorkerBridge::IDLE {
            dispatch.exit(idx as usize);
            unsafe {
                let _ = SetEvent(done.0);
            }
            continue;
        }

        let snapshot = registry.load();
        let entry_opt = snapshot.get(&device_id);
        let Some(entry) = entry_opt else {
            // one-shot per distinct id (旧実装は毎 buffer warn = RT 違反)。
            if warned_missing != Some(device_id) {
                warned_missing = Some(device_id);
                tracing::warn!(device_id, "no plugin registered for device (suppressing repeats)");
            }
            bridge.bridge().worker_task[idx as usize]
                .store(WorkerBridge::IDLE, Ordering::Release);
            dispatch.exit(idx as usize);
            unsafe {
                let _ = SetEvent(done.0);
            }
            continue;
        };
        warned_missing = None;

        // teardown (exit / slot IDLE 化 / SetEvent(done)) を `Drop` に集約。
        let _guard = DispatchGuard {
            dispatch: &dispatch,
            bridge: &bridge,
            idx: idx as usize,
            done,
        };

        // SAFETY: dispatch-critical section 内 (`dispatch.enter` 済)。
        // plugin-main は registry から外して quiesce するまでこの audio
        // half に `&mut` を発行しない (AudioHalf の契約)。
        let plugin = unsafe { entry.audio.get() };
        let pd = unsafe { &mut *entry.process_data };
        // shmem 由来の `frames` を clamp (信頼境界の外なので防御)。
        let n = (pd.frames as usize).min(common::process_data::MAX_FRAMES);
        let frames = n as u32;

        // Decode events_in → TimedNoteEvent / TimedParamEvent。
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
                        kind: crate::plugin_instance::ParamEventKind::Value,
                    });
                }
            }
        }
        // r.md #89: lane 非依存モジュレーションは `events_in` ではなく専用配列で
        // 届く (`docs/plan_rmd_88_89_cross_modulation.md` §2.2)。制御グリッドが
        // 64 サンプル刻みになるとノート枠を押し出すので枠を分けてある。
        for m in pd.param_mods_iter() {
            param_events_in.push(crate::plugin_instance::TimedParamEvent {
                time: m.time,
                param_id: m.param_id,
                value: m.value,
                kind: crate::plugin_instance::ParamEventKind::Mod,
            });
        }
        events_in.sort_unstable_by_key(|e| e.time);
        param_events_in.sort_unstable_by_key(|e| e.time);

        let (in_a, in_b) = pd.buffer_in.split_at(1);
        let input_audio: [&[f32]; 2] = [&in_a[0][..n], &in_b[0][..n]];

        // PR4 sidechain: per-aux-port input slices。
        let aux_inputs: [crate::plugin_instance::AuxInputBuf<'_>;
            common::process_data::MAX_AUX_IN] = std::array::from_fn(|port| {
            let active = pd.aux_in_active[port] != 0;
            crate::plugin_instance::AuxInputBuf {
                active,
                l: &pd.buffer_aux_in[port][0][..n],
                r: &pd.buffer_aux_in[port][1][..n],
            }
        });
        // Reset aux_in_active for the next buffer (stale routing 防止)。
        for flag in &mut pd.aux_in_active {
            *flag = 0;
        }
        // パラアウト: clear aux_out_active up front (失敗 buffer の stale
        // flag 防止)。
        for flag in &mut pd.aux_out_active {
            *flag = 0;
        }

        let transport = crate::plugin_instance::TransportContext::from_process_data(
            pd,
            entry.transport_pinned_to_song,
        );
        // builtin / Rust 製 plugin の panic で worker thread を殺さない
        // (以後の dispatch が誰も done を signal しなくなる)。
        let proc_start = std::time::Instant::now();
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
                // per-entry one-shot (v29): 落ち続ける plugin が毎 buffer
                // format+log で RT thread を汚さない。
                if !entry.err_logged.swap(true, Ordering::Relaxed) {
                    tracing::error!(error = ?e, device_id, "plugin.process() failed (suppressing repeats)");
                }
                false
            }
            Err(_panic) => {
                if !entry.err_logged.swap(true, Ordering::Relaxed) {
                    tracing::error!(
                        device_id,
                        "plugin.process() panicked; worker survived, buffer skipped (suppressing repeats)"
                    );
                }
                false
            }
        };
        // resource monitor: per-plugin の process() 時間 (μs) を publish。
        // L1: device_id (u64、 非有界) を配列 index にせず、 device_id を値で保持する
        // slot を claim する。 slot index は entry にキャッシュされるので線形 scan は
        // plugin ごと初回だけ。 満杯 / 競合で未 claim なら次 buffer で再試行。
        let proc_us = u32::try_from(proc_start.elapsed().as_micros()).unwrap_or(u32::MAX);
        let mut slot = entry.metric_slot.load(Ordering::Relaxed);
        if slot == METRIC_SLOT_UNCLAIMED
            && let Some(i) = metrics.claim_plugin_metric_slot(device_id)
        {
            entry.metric_slot.store(i as u32, Ordering::Relaxed);
            slot = i as u32;
        }
        if slot != METRIC_SLOT_UNCLAIMED {
            metrics.set_plugin_dsp_us_slot(slot as usize, proc_us);
        }
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

            // パラアウト: copy each declared aux output port。
            for port in 0..common::process_data::MAX_AUX_OUT {
                let Some(aux_l) = plugin.aux_output_buffer(port, 0) else {
                    continue;
                };
                let aux_r = plugin
                    .aux_output_buffer(port, 1)
                    .unwrap_or(aux_l);
                pd.buffer_aux_out[port][0][..n].copy_from_slice(&aux_l[..n]);
                pd.buffer_aux_out[port][1][..n].copy_from_slice(&aux_r[..n]);
                pd.aux_out_active[port] = 1;
            }

            // Drain plugin output events back into the shmem.
            events_out.clear();
            plugin.drain_out_notes_into(&mut events_out);
            // Drain plugin-emitted param touches / values / releases into the
            // per-worker SPSC ring (alloc/lock/syscall なし、 満杯時 drop)。
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
                for param_id in out_param_touches.drain(..) {
                    param_ring.push(RtParamEvent {
                        kind: RtParamKind::Touch,
                        device_id,
                        param_id,
                        value: 0.0,
                    });
                }
                for (param_id, value) in out_param_values.drain(..) {
                    param_ring.push(RtParamEvent {
                        kind: RtParamKind::Value,
                        device_id,
                        param_id,
                        value,
                    });
                }
                for param_id in out_param_releases.drain(..) {
                    param_ring.push(RtParamEvent {
                        kind: RtParamKind::Release,
                        device_id,
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

        // dispatch-critical section teardown は `_guard` の Drop。
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

    /// param event の wire 変換が device_id を保持する (v29)。
    #[test]
    fn rt_param_event_carries_device_id() {
        let ev = RtParamEvent {
            kind: RtParamKind::Value,
            device_id: 0xDEAD_BEEF_0001,
            param_id: 7,
            value: 0.25,
        };
        match rt_param_to_event(ev) {
            PluginEvent::PluginParamValueChanged { device_id, param_id, value } => {
                assert_eq!(device_id, 0xDEAD_BEEF_0001);
                assert_eq!(param_id, 7);
                assert_eq!(value, 0.25);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    /// どの worker も `enter` を bump していない状態では `quiesce` は即座に
    /// return する。
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

    /// `quiesce` は in-flight な worker が `exit` を bump するまで return
    /// しない。 UAF guard の中核を verify する test。
    #[test]
    fn quiesce_waits_for_inflight_dispatch() {
        let dispatch = Arc::new(DispatchCounter::new());
        // slot 2 で in-flight な状態を作る。
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
        assert!(
            elapsed < release_after + Duration::from_millis(5),
            "quiesce の所要時間が想定外に長い ({elapsed:?})"
        );
    }

    /// `quiesce` が snapshot を取った **後** に到着する `enter` bump は
    /// wait を延長してはならない (teardown starvation 防止)。
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

    /// registry の insert / remove / take_all round-trip (device_id keyed)。
    #[test]
    fn registry_insert_remove_roundtrip() {
        struct NullHalf;
        impl crate::plugin_instance::AudioProcessorHalf for NullHalf {
            fn process(
                &mut self,
                _frames: u32,
                _events: &[TimedNoteEvent],
                _param_events: &[crate::plugin_instance::TimedParamEvent],
                _input_audio: &[&[f32]],
                _aux_inputs: &[crate::plugin_instance::AuxInputBuf<'_>],
                _transport: &crate::plugin_instance::TransportContext,
            ) -> Result<i32> {
                Ok(0)
            }
            fn output_buffer(&self, _channel: usize) -> Option<&[f32]> {
                None
            }
            fn drain_out_notes_into(&mut self, _out: &mut Vec<TimedNoteEvent>) {}
        }

        let registry: PluginRegistry =
            Arc::new(arc_swap::ArcSwap::from_pointee(HashMap::new()));
        let entry = PluginEntry {
            audio: AudioHalf::new(Box::new(NullHalf)),
            process_data: std::ptr::null_mut(),
            err_logged: Arc::new(AtomicBool::new(false)),
            metric_slot: Arc::new(AtomicU32::new(METRIC_SLOT_UNCLAIMED)),
            transport_pinned_to_song: false,
        };
        registry_insert(&registry, 42, entry);
        assert!(registry.load().contains_key(&42));
        let removed = registry_remove(&registry, 42);
        assert!(removed.is_some());
        assert!(registry.load().is_empty());
        assert!(registry_remove(&registry, 42).is_none());

        // take_all + restore_all round-trip。
        let entry2 = PluginEntry {
            audio: AudioHalf::new(Box::new(NullHalf)),
            process_data: std::ptr::null_mut(),
            err_logged: Arc::new(AtomicBool::new(false)),
            metric_slot: Arc::new(AtomicU32::new(METRIC_SLOT_UNCLAIMED)),
            transport_pinned_to_song: false,
        };
        registry_insert(&registry, 7, entry2);
        let all = registry_take_all(&registry);
        assert!(registry.load().is_empty());
        assert_eq!(all.len(), 1);
        registry_restore_all(&registry, all);
        assert!(registry.load().contains_key(&7));
    }
}
