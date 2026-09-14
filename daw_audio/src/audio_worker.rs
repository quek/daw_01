//! Audio-engine worker pool. Pairs 1:1 with the plugin host's worker
//! pool: each audio worker `i` dispatches plugin work to plugin-host
//! worker `i` via `SyncSlot[i]`.
//!
//! 1 buffer の処理は依存グラフ ([`RenderGraph`]、`docs/plan_parallel_graph.md`) で、master (callback
//! スレッド) と worker が同じ ready queue から job を取り合って流す (Ardour `libs/ardour/graph.cc` と同型)。
//! master も runner として参加するので、`available_parallelism() == 1` でも spawn 無しで進む。
//!
//! **寝起きは 1 本の semaphore**: worker は仕事が無ければ semaphore で寝て (spin しない)、job が積まれた数だけ
//! 起こされる。buffer をまたいでもそのまま寝ているので、1 buffer あたりの寝起きは worker 1 本につき高々 1 回
//! (buffer の頭で起こされ、queue が空になったら寝る)。master は semaphore と「グラフが終わった」event の両方で
//! 寝る (bounded)。
//!
//! 1 buffer の文脈 ([`RenderCtx`]) は master のスタックに生き、ポインタ 1 本で worker へ渡る。worker は
//! `inside` を立ててからポインタを読み、master はグラフが終わったらポインタを消して `inside` が 0 になるのを
//! 待つ (Dekker 型: 立てる → 読む / 消す → 数える、どちらかが必ず相手を見る)。したがって master が戻った後に
//! worker が文脈 (と、その project の schedule) に触ることはない。
//!
//! plan §4 (有界化): master の待ちは `POOL_WAIT_TIMEOUT_MS` で bounded。
//! per-pair の plugin dispatch 自体が `DISPATCH_TIMEOUT_MS` で bounded +
//! timeout で pair が poison される (以後 skip) ため、pool 全体の待ちが
//! これを超えるのは audio worker thread 自身の死 (panic / OS freeze) だけ。
//! timeout で pool は **stalled** (以後 dispatch しない) になり、notify
//! thread が `AudioEvent::WorkerPoolStalled` を送って GUI が plugin_host を
//! respawn → `OpenWorkerPool` 再送 (= 新 pool) で復旧する。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use common::plugin_ref::DISPATCH_TIMEOUT_MS;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CreateSemaphoreW, GetCurrentThread, INFINITE, ReleaseSemaphore, SetEvent, SetThreadPriority,
    THREAD_PRIORITY_TIME_CRITICAL, WaitForMultipleObjects,
};

use crate::graph::render_graph::{Park, RenderGraph, drain, run_graph};
use crate::graph::step::{RenderCtx, run_step};

/// master の待ちの上限 (plan §4)。 各 pair の dispatch は
/// `DISPATCH_TIMEOUT_MS` で bounded + 最初の timeout で pair が poison され
/// 以後 skip になるので、 健全な runner は「高々 1 回の timeout + 残りの
/// 高速 skip / 通常処理」 で必ず job を終える。 これを超える = worker thread 自身が
/// 死んでいる (job が終わらない) と解釈する。
const POOL_WAIT_TIMEOUT_MS: u32 = DISPATCH_TIMEOUT_MS * 2;

#[derive(Copy, Clone)]
struct SendableHandle(HANDLE);
unsafe impl Send for SendableHandle {}
unsafe impl Sync for SendableHandle {}

/// master と worker が共有する dispatch の状態。
struct DispatchShared {
    /// この buffer の文脈 ([`RenderCtx`])。master が dispatch の間だけ置き、null = 仕事なし。
    ctx: AtomicPtr<RenderCtx<'static>>,
    /// `ctx` を読んでからまだ手放していない worker の数 (master はこれが 0 になるまで戻らない)。
    inside: AtomicU32,
    /// semaphore で寝ている (寝ようとしている) runner の数 (起こし損ね防止の登録)。
    sleepers: AtomicU32,
    /// master が寝ている (「グラフが終わった」event を要る)。
    master_parked: AtomicBool,
    /// 仕事が積まれるのを待つ semaphore (master と worker が共有)。
    sem: SendableHandle,
    /// worker ごとの buffer の頭の起こし (auto-reset event、worker i = `wakes[i]`)。1 回の release で全員を
    /// 起こすと、kernel が全員を ready にし終えるまで callback スレッドがその呼び出しで止まる (8 本で 6.5 µs 実測)
    /// — 1 本ずつ起こせば、起きた worker は次の worker を起こしている間にもう job を取っている。
    wakes: Vec<SendableHandle>,
    /// グラフが終わったことを寝ている master へ知らせる event (auto-reset)。
    done: SendableHandle,
    /// runner ごとの「終えた手」の数 (index = sync slot、master = 0)。stall の判定は **進みが止まったか** で行う
    /// ([`MasterPark::stuck`]) — 待ちの時刻だけで打ち切ると、別々の slot で bounded な dispatch が順に積み重なる
    /// だけの生きた worker を置いて戻ってしまう。runner ごとに cache line を分けて書く (取り合わない)。
    progress: Vec<PaddedCounter>,
}

/// cache line 1 本ぶんの counter (書き手は 1 runner だけ)。
#[derive(Default)]
#[repr(align(64))]
struct PaddedCounter(std::sync::atomic::AtomicU64);

impl DispatchShared {
    /// 寝ている runner を最大 `n` 人起こす。
    fn wake(&self, n: u32) {
        self.release(n.min(self.sleepers.load(Ordering::SeqCst)));
    }

    /// semaphore を `k` 回分ひらく。
    fn release(&self, k: u32) {
        if k > 0 {
            unsafe {
                let _ = ReleaseSemaphore(self.sem.0, i32::try_from(k).unwrap_or(i32::MAX), None);
            }
        }
    }

    /// 全 runner が終えた手の合計。
    fn progress(&self) -> u64 {
        self.progress.iter().map(|c| c.0.load(Ordering::Relaxed)).sum()
    }

    /// グラフが終わった: master が寝ていれば起こす (寝る側は登録してから見直すので、登録前なら見直しで気付く)。
    fn notify_done(&self) {
        if self.master_parked.load(Ordering::SeqCst) {
            unsafe {
                let _ = SetEvent(self.done.0);
            }
        }
    }
}

pub struct AudioWorkerPool {
    workers: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    shared: Arc<DispatchShared>,
    /// plan §4: master の待ちの timeout を観測した。 以後 `run` は
    /// 即 return (= 音は無音になるが CPAL callback は回り続ける)。 pool
    /// 再構築 (`OpenWorkerPool` 再送 = 新 `WorkerRig`) で復旧。
    stalled: AtomicBool,
    /// Drop で join を諦めて detach した worker がある (= semaphore / event の HANDLE を close しない —
    /// detached thread がまだ待っている可能性がある)。
    detached: AtomicBool,
}

impl AudioWorkerPool {
    /// Create a pool sized to `n_sync_slots` (= the number of
    /// handshake pairs with the plugin host). Each
    /// concurrent runner owns exactly one sync slot for the whole
    /// dispatch: the master (which joins the graph from [`Self::run`]) owns slot 0,
    /// worker thread `i` owns slot `i + 1` — so `n_sync_slots - 1` worker threads are spawned.
    /// Sharing a slot between two concurrent runners is not allowed:
    /// the wake/done events are auto-reset, so overlapping `dispatch()`
    /// calls on one slot collapse SetEvent pairs and either deadlock the
    /// audio callback or feed a plugin stale buffers.
    ///
    /// v29: 呼び出しは recv loop (off-thread)。 spawn / join が RT を塞がない。
    pub fn new(n_sync_slots: u32) -> Result<Self> {
        let n_workers = n_sync_slots.saturating_sub(1);
        // 積まれた job の数だけ release するので、上限で失敗して起こし損ねないよう最大にしておく。
        let sem = unsafe { CreateSemaphoreW(None, 0, i32::MAX, None) }
            .context("CreateSemaphoreW failed for the audio worker graph queue")?;
        let mut wakes = Vec::with_capacity(n_workers as usize);
        for _ in 0..n_workers {
            wakes.push(SendableHandle(create_anonymous_event()?));
        }
        let shared = Arc::new(DispatchShared {
            ctx: AtomicPtr::new(std::ptr::null_mut()),
            inside: AtomicU32::new(0),
            sleepers: AtomicU32::new(0),
            master_parked: AtomicBool::new(false),
            sem: SendableHandle(sem),
            wakes,
            done: SendableHandle(create_anonymous_event()?),
            progress: (0..=n_workers).map(|_| PaddedCounter::default()).collect(),
        });
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::with_capacity(n_workers as usize);
        for i in 0..n_workers as usize {
            let shared_w = Arc::clone(&shared);
            let shutdown_w = Arc::clone(&shutdown);
            let handle = std::thread::Builder::new()
                .name(format!("audio-worker-{i}"))
                .spawn(move || run_worker(&shared_w, &shutdown_w, i))?;
            workers.push(handle);
        }

        Ok(Self {
            workers,
            shutdown,
            shared,
            stalled: AtomicBool::new(false),
            detached: AtomicBool::new(false),
        })
    }

    /// pool が stalled (= master の待ちの timeout を観測して dispatch 停止中) か。
    /// notify thread が poll して `WorkerPoolStalled` を GUI へ送る。
    pub fn is_stalled(&self) -> bool {
        self.stalled.load(Ordering::Acquire)
    }

    /// 1 buffer のグラフを流す。master (呼び出しスレッド) も runner として job を取り、全 job が終わって
    /// 全 worker が文脈を手放すまで待つ。`false` = stalled (この buffer は描き切っていない)。
    ///
    /// SAFETY 契約: `ctx` が指す資源は呼び出しの間 `ctx` だけが借りている (`RenderCtx::new` の借用)。
    /// worker が `ctx` を読むのは `inside` を立てている間だけで、この関数は `inside` が 0 になるまで戻らない
    /// (module doc)。stalled で戻るのは **どの runner も `POOL_WAIT_TIMEOUT_MS` の間 1 手も終えられなかった** とき
    /// だけ — plugin の dispatch は 1 回ごとに `DISPATCH_TIMEOUT_MS` で bounded + timeout で pair が poison されるので、
    /// 生きた runner は必ずその間に手を終える。残るのは死んだ thread (メモリに触らない) だけ、と解釈する。
    pub fn run(&self, ctx: &RenderCtx<'_>) -> bool {
        // plan §4: stalled pool は二度と dispatch しない (worker thread の生死が不明なため)。
        if self.stalled.load(Ordering::Acquire) {
            return false;
        }
        let graph = ctx.graph;
        if graph.job_count() == 0 {
            return true;
        }
        let start = std::time::Instant::now();
        graph.begin();
        let shared = &*self.shared;
        shared.ctx.store(std::ptr::from_ref(ctx).cast_mut().cast::<RenderCtx<'static>>(), Ordering::SeqCst);
        // buffer の頭は寝ている数を見ずに、最初から走れる job の数だけ worker を 1 本ずつ起こす: 直前の buffer から
        // 寝に向かっている途中の worker (登録前) も取りこぼさない (書き出しのように buffer を詰めて回すと常に起きる)。
        // 起きていた worker の event は次に寝るときに 1 回空振りするだけ。
        for w in shared.wakes.iter().take(graph.root_count() as usize) {
            unsafe {
                let _ = SetEvent(w.0);
            }
        }
        let park = MasterPark {
            shared,
            spin_until: start + master_spin_budget(ctx),
            last_progress: std::cell::Cell::new((shared.progress(), start)),
        };
        let finished = run_graph(graph, &park, |job| run_job(shared, ctx, graph, job, 0));
        shared.ctx.store(std::ptr::null_mut(), Ordering::SeqCst);
        // 全 job が終わっているので、残っている worker は queue の見直しだけをしている (plugin は呼ばない)。
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(u64::from(POOL_WAIT_TIMEOUT_MS));
        let mut ok = finished;
        while ok && shared.inside.load(Ordering::SeqCst) != 0 {
            if std::time::Instant::now() > deadline {
                ok = false;
            }
            std::hint::spin_loop();
        }
        if !ok {
            // 進みが止まった: worker thread が死んでいる (bounded dispatch の下で手が終わらないのは thread 消滅のみ)。
            // 以後この pool では dispatch しない。 通知 (WorkerPoolStalled) は notify thread が `is_stalled` を
            // poll して送る — RT からは atomic store のみ (tracing / IPC 禁止)。
            self.stalled.store(true, Ordering::Release);
        }
        ok
    }
}

impl Drop for AudioWorkerPool {
    /// Tear the pool down: flag shutdown, wake every worker out of its semaphore wait (INFINITE), join the
    /// threads **bounded**, then close the semaphore / event HANDLEs.
    ///
    /// v29 (plan §4): Drop は recycle ring 経由の off-thread (recv loop) で
    /// 走るが、 stuck worker を無限 join すると respawn 経路が二次ハングする
    /// ので有界化する。 worker が block しうるのは (a) semaphore 待ち (INFINITE —
    /// ここで叩き起こす)、 (b) plugin dispatch (bounded `DISPATCH_TIMEOUT_MS`)
    /// のみなので、 期限は 2×DISPATCH で十分。 期限超過の worker は detach
    /// (leak) し、 その場合 HANDLE の close も見送る (detached
    /// thread がまだ待っている可能性があるため — kernel handle 数個のリーク
    /// は破滅的イベント時のみ)。
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        for w in &self.shared.wakes {
            unsafe {
                let _ = SetEvent(w.0);
            }
        }
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(u64::from(DISPATCH_TIMEOUT_MS) * 2);
        for h in std::mem::take(&mut self.workers) {
            // JoinHandle に timeout 付き join は無いので is_finished を poll
            // する (off-thread なので短い sleep は許容)。
            while !h.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if h.is_finished() {
                if h.join().is_err() {
                    tracing::error!("audio worker thread panicked");
                }
            } else {
                self.detached.store(true, Ordering::Release);
                tracing::warn!(
                    "audio worker did not exit within the bounded join; detaching"
                );
            }
        }
        if !self.detached.load(Ordering::Acquire) {
            // Threads have exited; the handles are now unused.
            unsafe {
                let _ = CloseHandle(self.shared.sem.0);
                let _ = CloseHandle(self.shared.done.0);
                for w in &self.shared.wakes {
                    let _ = CloseHandle(w.0);
                }
            }
        }
    }
}

/// job `job` の手を順に実行する (runner `slot` の `SyncSlot` で plugin を dispatch)。手を終えるたびに自分の
/// 進みを数える (stall の判定、[`MasterPark::stuck`])。
fn run_job(shared: &DispatchShared, ctx: &RenderCtx<'_>, graph: &RenderGraph, job: u32, slot: usize) {
    let progress = shared.progress.get(slot).map(|c| &c.0);
    for &step in graph.steps(job) {
        run_step(ctx, step, slot);
        if let Some(p) = progress {
            p.store(p.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
        }
    }
}

/// callback スレッドが寝る前に回ってよい時間の 1 buffer あたりの上限 = buffer 周期の 1/50。
///
/// callback スレッドはグラフが終わるまで他にすることが無い。worker が job を終えた瞬間に寝て起こされると、その
/// 起床遅延 (実測 ~5 µs) が buffer ごとに乗る — 小さいグラフでは処理そのものより長い。一方で回り続けると
/// plugin host の worker から core を奪うので、締め切りに比例した短い時間だけ回ってから寝る
/// (256 frame @ 48 kHz で ~107 µs、1024 frame で ~427 µs)。
fn master_spin_budget(ctx: &RenderCtx<'_>) -> std::time::Duration {
    let micros = u64::from(ctx.params.frames) * 1_000_000 / u64::from(ctx.params.sample_rate.max(1)) / 50;
    std::time::Duration::from_micros(micros)
}

/// callback スレッドの寝起き: `spin_until` までは回って待ち、それ以降は semaphore (仕事が積まれた) と `done`
/// (グラフが終わった) の両方で bounded に寝る。
struct MasterPark<'a> {
    shared: &'a DispatchShared,
    spin_until: std::time::Instant,
    /// 直近に見た進みの合計と、それが最後に変わった時刻 (callback スレッドだけが触る)。
    last_progress: std::cell::Cell<(u64, std::time::Instant)>,
}

impl MasterPark<'_> {
    /// `POOL_WAIT_TIMEOUT_MS` の間、どの runner も手を終えていない (= stalled とみなす)。
    fn stuck(&self) -> bool {
        let now = std::time::Instant::now();
        let sum = self.shared.progress();
        let (seen, since) = self.last_progress.get();
        if sum != seen {
            self.last_progress.set((sum, now));
            return false;
        }
        now.duration_since(since) >= std::time::Duration::from_millis(u64::from(POOL_WAIT_TIMEOUT_MS))
    }
}

impl Park for MasterPark<'_> {
    fn wake(&self, n: u32) {
        self.shared.wake(n);
    }

    fn finished(&self) {
        self.shared.notify_done();
    }

    fn park(&self, graph: &RenderGraph) -> bool {
        let s = self.shared;
        // 回って待つ (1 buffer の上限まで)。時刻は数回に 1 度だけ見る。
        let mut k = 0u32;
        loop {
            if graph.is_done() {
                return true;
            }
            if graph.has_work() {
                // 取れる job がある。取れなかったのなら予約済みで未書き込みの席 (書き手が次の命令で書く) なので回って
                // 見直すが、書き手が止まったままなら進みの期限で打ち切る (呼び側はこの手の後で必ず pop し直す)。
                std::hint::spin_loop();
                return !self.stuck();
            }
            std::hint::spin_loop();
            k = k.wrapping_add(1);
            if k.is_multiple_of(16) && std::time::Instant::now() >= self.spin_until {
                break;
            }
        }
        // 登録してから見直す: 積む側 / 終える側は「積んで (終えて) から登録を読む」ので、どちらかが必ず相手を見る。
        s.master_parked.store(true, Ordering::SeqCst);
        s.sleepers.fetch_add(1, Ordering::SeqCst);
        if !(graph.has_work() || graph.is_done()) {
            let handles = [s.sem.0, s.done.0];
            // 起こされても時間切れでも、判定は進みで行う (1 回の待ちの時刻では打ち切らない — module doc)。
            let _ = unsafe { WaitForMultipleObjects(&handles, false, POOL_WAIT_TIMEOUT_MS) };
        }
        s.sleepers.fetch_sub(1, Ordering::SeqCst);
        s.master_parked.store(false, Ordering::SeqCst);
        !self.stuck()
    }
}

/// worker の寝起き ([`drain`] が job を積んだときに寝ている runner を起こすだけ。寝るのは [`run_worker`])。
struct WorkerPark<'a> {
    shared: &'a DispatchShared,
}

impl Park for WorkerPark<'_> {
    fn wake(&self, n: u32) {
        self.shared.wake(n);
    }

    fn finished(&self) {
        self.shared.notify_done();
    }

    fn park(&self, _: &RenderGraph) -> bool {
        unreachable!("worker は run_worker の semaphore 待ちで寝る")
    }
}

/// worker `index` (sync slot = `index + 1`、slot 0 は callback スレッド)。
fn run_worker(shared: &DispatchShared, shutdown: &AtomicBool, index: usize) {
    let sync_slot = index + 1;
    let Some(&wake) = shared.wakes.get(index) else { return };
    boost_thread_priority("audio worker");
    // Join "Pro Audio" so MMCSS keeps this thread on the priority-class
    // schedule the audio mixer/sequencer loop relies on. Held until the
    // worker drops out of `run_worker`, then auto-reverted.
    let _mmcss = common::mmcss::join_pro_audio();
    if _mmcss.is_none() {
        tracing::warn!("audio worker: MMCSS join (Pro Audio) failed");
    }
    let park = WorkerPark { shared };
    // 寝る登録を済ませてから寝に行くか (グラフの途中で queue が空になったとき、見直しの前に登録してある)。
    let mut registered = false;
    loop {
        // 仕事が積まれるまで寝る (buffer の頭の自分宛ての event か、途中で積まれた job の semaphore)。不変条件 4 が
        // 禁じているのは「**他プロセスの完了待ち**を無限にすること」で、どちらも同一プロセスの runner が起こす。
        // RT deadline を握らない (起きなければ何も走らないだけ) ので INFINITE でよい。完了待ちの側 (master) は bounded。
        if !registered {
            shared.sleepers.fetch_add(1, Ordering::SeqCst);
        }
        unsafe {
            WaitForMultipleObjects(&[wake.0, shared.sem.0], false, INFINITE); // arch-lint: allow-infinite
        }
        shared.sleepers.fetch_sub(1, Ordering::SeqCst);
        registered = false;
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // 立ててから読む (module doc)。null = 仕事の無い buffer の間の余分な起こし。
        shared.inside.fetch_add(1, Ordering::SeqCst);
        let ctx = shared.ctx.load(Ordering::SeqCst);
        if !ctx.is_null() {
            // SAFETY: master は `inside` が 0 になるまで `run` から戻らない = `ctx` と、それが借りている schedule は
            // ここを抜けるまで生きている。
            let ctx = unsafe { &*ctx.cast_const().cast::<RenderCtx<'_>>() };
            let graph = ctx.graph;
            let mut run = |job| run_job(shared, ctx, graph, job, sync_slot);
            loop {
                drain(graph, &park, &mut run);
                if graph.is_done() {
                    break;
                }
                // 登録してから見直す: 積む側は「積んでから登録を読む」ので、この後に積まれた job では必ず起こされる。
                // 仕事があれば登録を戻して続け、無ければ登録したまま寝に行く。
                shared.sleepers.fetch_add(1, Ordering::SeqCst);
                if !graph.has_work() {
                    registered = true;
                    break;
                }
                shared.sleepers.fetch_sub(1, Ordering::SeqCst);
                // 予約済みで未書き込みの席 (書き手が次の命令で書く) を待つ間だけ回る。
                std::hint::spin_loop();
            }
        }
        shared.inside.fetch_sub(1, Ordering::SeqCst);
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
