//! Audio-engine worker pool. Pairs 1:1 with the plugin host's worker
//! pool: each audio worker `i` dispatches plugin work to plugin-host
//! worker `i` via `SyncSlot[i]`.
//!
//! 1 buffer の処理は依存グラフ ([`RenderGraph`]、`docs/plan_parallel_graph.md`) で、master (callback
//! スレッド) と worker が同じ ready queue から job を取り合って流す (Ardour `libs/ardour/graph.cc` と同型)。
//! master も runner として参加するので、`available_parallelism() == 1` でも spawn 無しで進む。
//!
//! **寝起きは runner ごとの event と、寝ている runner の bit 列 (`idle`)**: 仕事が無くなった runner は自分の bit を
//! 立ててから待ち行列を見直し、何も無ければ自分の event で寝る (spin しない)。起こす側は job を積んでから bit を
//! 読み、**自分で下ろせた bit** の runner の event だけを signal する。起こす数と寝ている runner が 1 対 1 に
//! 対応するので、取りこぼしも余分な起こし (次の buffer で空振りする permit) も無い。1 buffer あたりの寝起きは
//! runner 1 本につき高々 1 回 (buffer の頭で起こされ、queue が空になったら寝る)。
//!
//! 1 buffer の文脈 ([`RenderCtx`]) は pool が持つ器 (`DispatchShared::window`) に置き、ポインタ 1 本で worker へ
//! 渡る。worker は `inside` を立ててからポインタを読み、master はグラフが終わったらポインタを消して `inside` を
//! 数える (Dekker 型: 立てる → 読む / 消す → 数える、どちらかが必ず相手を見る)。
//!
//! **窓**: master (= 呼び側、callback か書き出しのスレッド) の 1 回の描画は、文脈を読んだ runner が全員それを
//! 手放す (`inside` = 0) まで終わらない。ふだん master は 0 を見てから戻るが、stall (下) では待たずに戻る — RT は
//! 他の runner の完了を無限に待たない (不変条件 4)。そのとき [`PoolRun::InFlight`] を返し、文脈を持ったまま走る
//! runner (生き残り) が居ることを呼び側へ渡す。**呼び側は [`AudioWorkerPool::window_open`] が false になるまで、
//! 文脈が指す物 (曲 / schedule / scratch / master バス / plugin の表 / rig …) を解放も再利用もしない**。文脈そのものは
//! pool の器に居て、生き残りは pool の共有部を `Arc` で持つので、呼び側が先に戻っても消えない。
//!
//! 呼び側は窓が閉じる前に戻りうる (live の callback は戻り、書き出しは有界に待って諦める) ので、**文脈が指す物は
//! 呼び側のスレッドのスタックに置かない** — heap の持ち主の中に置く (live は `Box<ProjectRt>` と `Arc`、書き出しは
//! 走査の持ち主 `export::WalkOwner`)。窓が閉じないまま諦めて leak するときは、**referent を番地ごと持つ持ち主
//! (`Box` / `Arc`) を忘れる形でだけ** 行う — スタックに居る値を `forget` へ動かしても heap の中身が残るだけで、文脈が
//! 指していた番地は残らない。忘れるか解放するかは持ち主の Drop が窓を見て決める (live の `DeviceRt`、書き出しの
//! `WalkOwner`)。この規則は InFlight を返す口 (`gate::in_flight` — InFlight の証はここでしか作れない) が debug build
//! で毎回番地を見る ([`RenderCtx::referents`])。
//!
//! plan §4 (有界化): stall の判定は **進みが止まったか** で行う。runner は手を終えるたび・plugin の dispatch を
//! 終えるたびに進みを数え ([`AudioWorkerPool::note_progress`])、dispatch は 1 回ずつ `DISPATCH_TIMEOUT_MS` で
//! bounded (+ timeout で pair を poison) なので、生きて走る runner の進みはその間隔より長く止まらない。どの runner
//! も `POOL_WAIT_TIMEOUT_MS` の間進まなければ、OS に止められた (debugger / ページング / suspend) か死んだとみなし、
//! pool は **stalled** (以後 dispatch しない) になる。止められた runner は後で動き出しうるので、窓は上のとおり
//! `inside` = 0 まで続く。動き出した runner は見捨てられた buffer の残りの plugin を叩かない (出力は捨てられる —
//! 叩くほど窓が長引き、立て直される host へ無駄な依頼を送る) ので、窓は動き出してから高々 dispatch 1 回 (期限つき)
//! と、plugin を飛ばした残りの手の分で閉じる。notify thread が `AudioEvent::WorkerPoolStalled` を送って GUI が plugin_host を
//! respawn → `OpenWorkerPool` 再送 (= 新 pool) で復旧する。

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use common::plugin_ref::DISPATCH_TIMEOUT_MS;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetCurrentThread, INFINITE, SetEvent, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL, WaitForSingleObject,
};

use crate::graph::render_graph::{Park, RenderGraph, drain, run_graph};
use crate::graph::step::{RenderCtx, run_step};

/// 進みが止まったとみなす間隔 (plan §4)。生きて走る runner は手か plugin の dispatch を終えるたびに進む
/// (dispatch は 1 回ずつ `DISPATCH_TIMEOUT_MS` で bounded) ので、どの runner もこれだけ進まなければ OS に止められたか
/// 死んだ thread とみなす。止められた runner は後で動き出しうる — 窓の扱いは module doc。
const POOL_WAIT_TIMEOUT_MS: u32 = DISPATCH_TIMEOUT_MS * 2;

/// [`AudioWorkerPool::run`] の結果。
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolRun {
    /// 描き切り、文脈を読んだ runner は全員それを手放した。
    Finished,
    /// stall: この buffer を描き切っていない (pool は以後 dispatch しない)。文脈を持った runner は居ない — 渡した物は
    /// もう呼び側の物。
    Stalled,
    /// stall (か、描き切った後に runner が文脈を手放さないまま止まった): 文脈を持ったまま走る runner (生き残り) が
    /// 居る。呼び側は [`AudioWorkerPool::window_open`] が false になるまで、文脈が指す物を解放も再利用もしない
    /// (module doc の「窓」)。証 ([`gate::WindowChecked`]) を作れるのは `gate::in_flight` だけなので、InFlight は
    /// 必ずその口 (器の referent が呼び側のスタックに無いことを見る) を通って返る。
    InFlight(gate::WindowChecked),
}

/// runner の数の上限 (`idle` の bit 数)。
const MAX_RUNNERS: u32 = u64::BITS;

/// master (callback スレッド) の runner 番号 (= sync slot 0)。
const MASTER: usize = 0;

#[derive(Copy, Clone)]
struct SendableHandle(HANDLE);
unsafe impl Send for SendableHandle {}
unsafe impl Sync for SendableHandle {}

/// master と worker が共有する dispatch の状態。
struct DispatchShared {
    /// この buffer の文脈 ([`RenderCtx`]) を指す (指す先は `window`)。master が dispatch の間だけ置き、null = 仕事なし。
    ctx: AtomicPtr<RenderCtx<'static>>,
    /// 文脈の器。呼び側のスタックに置かないのは、stall で呼び側が先に戻った後も生き残りが読むから (module doc の
    /// 「窓」)。書くのは [`AudioWorkerPool::run`] の頭だけで、そのとき文脈を読んでいる runner は居ない (前の run は
    /// `inside` = 0 を見て戻ったか、stall して以後 run は器に触らない)。
    window: UnsafeCell<MaybeUninit<RenderCtx<'static>>>,
    /// `ctx` を読んでからまだ手放していない worker の数。0 になったら窓が閉じる (module doc)。
    inside: AtomicU32,
    /// 寝ている (寝ようとしている) runner の bit (bit i = runner i、master = bit 0)。立てるのは本人、下ろすのは
    /// 起こす側 (下ろせたらその runner の event を signal する) か、見直しで仕事を見つけた本人。
    idle: AtomicU64,
    /// runner ごとの起こし (auto-reset event、index = runner 番号)。1 回の release で全員を起こす semaphore にしないのは、
    /// kernel が全員を ready にし終えるまで起こす側がその呼び出しで止まるから (8 本で 6.5 µs 実測) — 1 本ずつ起こせば、
    /// 起きた worker は次の worker を起こしている間にもう job を取っている。
    events: Vec<SendableHandle>,
    /// runner ごとの「終えた手と plugin の dispatch」の数 (index = sync slot、master = 0)。stall の判定は **進みが
    /// 止まったか** で行う ([`MasterPark::stuck`]) — 待ちの時刻だけで打ち切ると、bounded な dispatch が順に積み重なる
    /// だけの生きた runner を置いて戻ってしまう。手だけを数えると、1 つの手 (track の chain) の中で期限内の遅い
    /// plugin が積み重なったときに同じことが起きる。runner ごとに cache line を分けて書く (取り合わない)。
    progress: Vec<PaddedCounter>,
}

// SAFETY: `window` 以外は atomic と kernel handle。`window` は module doc の窓の規約で守る — 書くのは
// `AudioWorkerPool::run` の頭 (文脈を読む runner が居ないとき) だけ、読むのは `ctx` から指された runner が `inside` を
// 立てている間だけ。`RenderCtx` 自身は Send + Sync (手ごとの排他、`graph::step` の module doc)。
unsafe impl Sync for DispatchShared {}
unsafe impl Send for DispatchShared {}

/// cache line 1 本ぶんの counter (書き手は 1 runner だけ)。
#[derive(Default)]
#[repr(align(64))]
struct PaddedCounter(std::sync::atomic::AtomicU64);

impl DispatchShared {
    /// runner `r` の bit を下ろせたら (= 寝ていた / 寝ようとしていた) その event を signal する。
    fn claim(&self, r: usize) -> bool {
        let bit = 1u64 << r;
        if self.idle.fetch_and(!bit, Ordering::SeqCst) & bit == 0 {
            return false;
        }
        if let Some(ev) = self.events.get(r) {
            unsafe {
                let _ = SetEvent(ev.0);
            }
        }
        true
    }

    /// 寝ている runner を最大 `n` 人起こす (worker を先に。master はグラフが終わるのを待っているだけのことが多い)。
    fn wake(&self, n: u32) {
        let mut left = n;
        while left > 0 {
            let m = self.idle.load(Ordering::SeqCst);
            if m == 0 {
                return;
            }
            let workers = m & !(1u64 << MASTER);
            let r = if workers != 0 { workers.trailing_zeros() } else { MASTER as u32 };
            if self.claim(r as usize) {
                left -= 1;
            }
        }
    }

    /// 全 runner の進み (終えた手と dispatch) の合計。
    fn progress(&self) -> u64 {
        self.progress.iter().map(|c| c.0.load(Ordering::Relaxed)).sum()
    }

    /// runner `r` の進みを 1 つ数える (書き手は runner `r` 自身だけ)。
    fn note_progress(&self, r: usize) {
        if let Some(c) = self.progress.get(r) {
            c.0.store(c.0.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
        }
    }

    /// 寝ている runner `r` の見直しの後始末: 自分の bit を下ろす。起こす側に先に下ろされていたら、その signal
    /// (もう打たれたか、次の命令で打たれる) を `timeout_ms` まで待って消費する — 残すと次に寝るときに空振りする。
    fn unregister(&self, r: usize, signaled: bool, timeout_ms: u32) {
        let bit = 1u64 << r;
        if self.idle.fetch_and(!bit, Ordering::SeqCst) & bit == 0
            && !signaled
            && let Some(ev) = self.events.get(r)
        {
            unsafe {
                let _ = WaitForSingleObject(ev.0, timeout_ms);
            }
        }
    }
}

pub struct AudioWorkerPool {
    workers: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    shared: Arc<DispatchShared>,
    /// plan §4: 進みが止まった (`POOL_WAIT_TIMEOUT_MS`、module doc) のを master が観測した = その buffer を見捨てた。
    /// 以後 `run` は器に触らずに即 return し (= 音は無音になるが CPAL callback は回り続ける)、生き残りは残りの plugin を
    /// 叩かない (`PairLease::slot` が plugin ごとに読む — `inside` / `idle` と cache line を分けるため `shared` に
    /// 置かない)。pool 再構築 (`OpenWorkerPool` 再送 = 新 `WorkerRig`) で復旧。
    stalled: AtomicBool,
    /// Drop で join を諦めて detach した worker がある (= event の HANDLE を close しない —
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
        let n_runners = n_sync_slots.clamp(1, MAX_RUNNERS);
        let mut events = Vec::with_capacity(n_runners as usize);
        for _ in 0..n_runners {
            events.push(SendableHandle(create_anonymous_event()?));
        }
        let shared = Arc::new(DispatchShared {
            ctx: AtomicPtr::new(std::ptr::null_mut()),
            window: UnsafeCell::new(MaybeUninit::uninit()),
            inside: AtomicU32::new(0),
            idle: AtomicU64::new(0),
            events,
            progress: (0..n_runners).map(|_| PaddedCounter::default()).collect(),
        });
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::with_capacity(n_runners as usize - 1);
        for runner in 1..n_runners as usize {
            let shared_w = Arc::clone(&shared);
            let shutdown_w = Arc::clone(&shutdown);
            let handle = std::thread::Builder::new()
                .name(format!("audio-worker-{}", runner - 1))
                .spawn(move || run_worker(&shared_w, &shutdown_w, runner))?;
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

    /// pool が stalled (= 進みが止まったのを観測して dispatch 停止中) か。
    /// notify thread が poll して `WorkerPoolStalled` を GUI へ送る。
    pub fn is_stalled(&self) -> bool {
        self.stalled.load(Ordering::Acquire)
    }

    /// 1 buffer のグラフを流す。master (呼び出しスレッド) も runner として job を取り、全 job が終わって全 worker が
    /// 文脈を手放すのを待つ。文脈は pool の器へ移す (呼び側のスタックに置かない — module doc の「窓」)。
    ///
    /// 呼び側の契約: `ctx` が借りた物 (`RenderCtx::new` の借用と、`ctx.rig` / `ctx.params` が指す物) は、呼び側の
    /// スレッドのスタックに置かず、[`PoolRun::InFlight`] が返ったら [`Self::window_open`] が false になるまで解放も
    /// 再利用もしない (module doc の「窓」)。Finished / Stalled なら戻った時点で呼び側の物。
    pub fn run(&self, ctx: RenderCtx<'_>) -> PoolRun {
        // plan §4: stalled pool は二度と dispatch しない (文脈の器を生き残りが読んでいうるので書かない)。
        if self.is_stalled() {
            return PoolRun::Stalled;
        }
        if ctx.graph.job_count() == 0 {
            return PoolRun::Finished;
        }
        let shared = &*self.shared;
        // SAFETY: 器を読んでいる runner は居ない (`DispatchShared::window` の doc)。`RenderCtx` は Drop を持たないので
        // 前の文脈を上書きしてよい。寿命は呼び側の契約 (上の doc) が守る。
        let ctx: &RenderCtx<'_> = unsafe {
            let slot = shared.window.get().cast::<RenderCtx<'_>>();
            slot.write(ctx);
            &*slot
        };
        let graph = ctx.graph;
        let start = std::time::Instant::now();
        graph.begin();
        shared.ctx.store(std::ptr::from_ref(ctx).cast_mut().cast::<RenderCtx<'static>>(), Ordering::SeqCst);
        // 置いてから寝ている worker を読む。前の buffer の worker は `inside` を持ったまま bit を立ててから抜けるので
        // (前の run は `inside` が 0 になってから戻った — stall した pool はここまで来ない)、ここで寝ている worker は
        // 全部 bit を立て終えている。
        shared.wake(graph.root_count());
        let park = MasterPark {
            shared,
            spin_until: start + master_spin_budget(ctx),
            last_progress: std::cell::Cell::new((shared.progress(), start)),
        };
        let finished = run_graph(graph, &park, |job| run_job(shared, ctx, graph, job, MASTER));
        shared.ctx.store(std::ptr::null_mut(), Ordering::SeqCst);
        if finished {
            // 全 job が終わっているので、残っている worker は queue の見直しだけをしている (plugin は呼ばない)。
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(u64::from(POOL_WAIT_TIMEOUT_MS));
            while shared.inside.load(Ordering::SeqCst) != 0 {
                if std::time::Instant::now() > deadline {
                    // 見直しの途中で止められた worker が居る: 窓を開けたまま戻る (下と同じ)。
                    self.stalled.store(true, Ordering::Release);
                    return in_flight(ctx);
                }
                std::hint::spin_loop();
            }
            return PoolRun::Finished;
        }
        // 進みが止まった: この buffer を見捨て、以後この pool では dispatch しない。止まった runner は後で動き出しうるので
        // 待たずに戻り、文脈を持ったままなら窓が開いていることを返す (module doc)。動き出した runner は残りの plugin を
        // 叩かない (`Self::stalled`)。通知 (WorkerPoolStalled) は notify thread が `is_stalled` を poll して
        // 送る — RT からは atomic store のみ (tracing / IPC 禁止)。
        self.stalled.store(true, Ordering::Release);
        if shared.inside.load(Ordering::SeqCst) == 0 { PoolRun::Stalled } else { in_flight(ctx) }
    }

    /// [`PoolRun::InFlight`] を返した描画の窓がまだ開いている (文脈を持ったまま走る runner が居る)。false を見た
    /// 呼び側は、文脈が指していた物を解放・再利用してよい — runner は最後に文脈へ触れた後で `inside` を下ろす
    /// (SeqCst) ので、その書き込みはここで見える。
    #[must_use]
    pub fn window_open(&self) -> bool {
        self.shared.inside.load(Ordering::SeqCst) != 0
    }

    /// runner `runner` が plugin の dispatch を 1 回終えた (完了でも期限切れでも)。stall の判定の進みに数える
    /// (`DispatchShared::progress`)。書き手はその runner 自身だけ。
    #[inline]
    pub fn note_progress(&self, runner: usize) {
        self.shared.note_progress(runner);
    }
}

impl Drop for AudioWorkerPool {
    /// Tear the pool down: flag shutdown, wake every worker out of its event wait (INFINITE), join the
    /// threads **bounded**, then close the event HANDLEs.
    ///
    /// v29 (plan §4): Drop は recycle ring 経由の off-thread (recv loop) で
    /// 走るが、 stuck worker を無限 join すると respawn 経路が二次ハングする
    /// ので有界化する。 pool (を持つ rig) が落ちるのは窓が閉じた後
    /// (`PoolRun::InFlight` を受けた呼び側は窓が閉じるまで rig を持つ — module doc) なので、
    /// worker は文脈を持っておらず、 block しうるのは event 待ち (INFINITE —
    /// ここで叩き起こす) だけ。 期限は 2×DISPATCH。 それでも抜けない worker
    /// (OS に止められたまま) は detach (leak) し、 その場合 HANDLE の close も
    /// 見送る (detached thread がまだ待っている可能性があるため — kernel handle
    /// 数個のリークは破滅的イベント時のみ)。 detach した worker が後で動き出しても、
    /// `shared` は自分の `Arc` で生きていて、 文脈は null なので rig に触らない。
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        for ev in self.shared.events.iter().skip(1) {
            unsafe {
                let _ = SetEvent(ev.0);
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
            for ev in &self.shared.events {
                unsafe {
                    let _ = CloseHandle(ev.0);
                }
            }
        }
    }
}

/// [`PoolRun::InFlight`] を作る唯一の口 ([`in_flight`]) と、その証 ([`WindowChecked`])。証の field はこの module の
/// 外から見えないので、`AudioWorkerPool::run` の中でも InFlight をこの口を通さずに作れない (戻りのどれか 1 つが
/// 検査を素通りする形を型で塞ぐ)。
mod gate {
    #[cfg(debug_assertions)]
    use windows::Win32::System::Threading::GetCurrentThreadStackLimits;

    use super::PoolRun;
    use crate::graph::step::RenderCtx;

    /// [`PoolRun::InFlight`] の証。作るのは [`in_flight`] だけ。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct WindowChecked(());

    /// 窓を開けたまま戻る。呼び側はこの後、窓が閉じる前にスタックを畳みうる (live の callback は戻り、書き出しは
    /// 待ちを諦めると戻る) ので、器の referent (`RenderCtx::referents`) が呼び側のスレッドのスタックに無いことを
    /// debug build で見て、居ればスタックに居る物を全部名指しして止める (module doc の「窓」の規則 — 手の列挙で
    /// 済ませない)。
    pub(super) fn in_flight(ctx: &RenderCtx<'_>) -> PoolRun {
        #[cfg(debug_assertions)]
        {
            let (mut low, mut high) = (0usize, 0usize);
            // SAFETY: 呼ぶスレッド自身のスタックの範囲を読むだけ (確保も待ちもしない)。
            unsafe { GetCurrentThreadStackLimits(&raw mut low, &raw mut high) };
            let referents = ctx.referents();
            let mut on_stack = referents.map(|_| "");
            let mut n = 0;
            for (name, at) in referents {
                if (low..high).contains(&at) {
                    on_stack[n] = name;
                    n += 1;
                }
            }
            assert!(
                n == 0,
                "窓の器の referent {:?} が呼び側のスレッドのスタック [0x{low:x}, 0x{high:x}) に居る — 呼び側が戻ると、\
                 生き残りが解放済み・再利用中のスタックを読み書きする",
                &on_stack[..n]
            );
        }
        PoolRun::InFlight(WindowChecked(()))
    }
}
use gate::in_flight;

/// job `job` の手を順に実行する (runner `slot` の `SyncSlot` で plugin を dispatch)。手を終えるたびに自分の
/// 進みを数える (stall の判定、[`MasterPark::stuck`]。dispatch ごとの進みは `dispatch_bounded` が数える)。
fn run_job(shared: &DispatchShared, ctx: &RenderCtx<'_>, graph: &RenderGraph, job: u32, slot: usize) {
    // 1 手ごとの所要を内訳へ (`crate::graph::profile`)。`dispatch_bounded` が中で
    // 「待ち」を別に数えるので、差が「グラフ自身の仕事」になる。
    let profile = ctx.rig.map(|rig| &rig.profile);
    for &step in graph.steps(job) {
        let started = std::time::Instant::now();
        run_step(ctx, step, slot);
        if let Some(pf) = profile {
            pf.add_step(slot, started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
        }
        shared.note_progress(slot);
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

/// callback スレッドの寝起き: `spin_until` までは回って待ち、それ以降は自分の event で bounded に寝る (起こすのは
/// 積んだ側 / グラフを終えた側)。
struct MasterPark<'a> {
    shared: &'a DispatchShared,
    spin_until: std::time::Instant,
    /// 直近に見た進みの合計と、それが最後に変わった時刻 (callback スレッドだけが触る)。
    last_progress: std::cell::Cell<(u64, std::time::Instant)>,
}

impl MasterPark<'_> {
    /// `POOL_WAIT_TIMEOUT_MS` の間、どの runner も進んでいない (手も plugin の dispatch も終えていない = stalled
    /// とみなす)。
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
    fn is_master(&self) -> bool {
        true
    }

    fn wake(&self, n: u32) {
        self.shared.wake(n);
    }

    fn wake_master(&self) {}

    fn finished(&self) {}

    fn park(&self, graph: &RenderGraph) -> bool {
        let s = self.shared;
        // 回って待つ (1 buffer の上限まで)。時刻は数回に 1 度だけ見る。
        let mut k = 0u32;
        loop {
            if graph.is_done() {
                return true;
            }
            if graph.has_work(true) {
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
        // 登録してから見直す: 積む側 / 終える側は「積んで (終えて) から bit を読む」ので、どちらかが必ず相手を見る。
        s.idle.fetch_or(1u64 << MASTER, Ordering::SeqCst);
        let mut signaled = false;
        if !(graph.has_work(true) || graph.is_done())
            && let Some(ev) = s.events.get(MASTER)
        {
            // 起こされても時間切れでも、判定は進みで行う (1 回の待ちの時刻では打ち切らない — module doc)。
            signaled = unsafe { WaitForSingleObject(ev.0, POOL_WAIT_TIMEOUT_MS) } == WAIT_OBJECT_0;
        }
        s.unregister(MASTER, signaled, POOL_WAIT_TIMEOUT_MS);
        !self.stuck()
    }
}

/// worker の寝起き ([`drain`] が job を積んだときに寝ている runner を起こすだけ。寝るのは [`run_worker`])。
struct WorkerPark<'a> {
    shared: &'a DispatchShared,
}

impl Park for WorkerPark<'_> {
    fn is_master(&self) -> bool {
        false
    }

    fn wake(&self, n: u32) {
        self.shared.wake(n);
    }

    fn wake_master(&self) {
        self.shared.claim(MASTER);
    }

    fn finished(&self) {
        self.shared.claim(MASTER);
    }

    fn park(&self, _: &RenderGraph) -> bool {
        unreachable!("worker は run_worker の event 待ちで寝る")
    }
}

/// worker `runner` (= sync slot、slot 0 は callback スレッド)。
fn run_worker(shared: &DispatchShared, shutdown: &AtomicBool, runner: usize) {
    let Some(&wake) = shared.events.get(runner) else { return };
    boost_thread_priority("audio worker");
    // Join "Pro Audio" so MMCSS keeps this thread on the priority-class
    // schedule the audio mixer/sequencer loop relies on. Held until the
    // worker drops out of `run_worker`, then auto-reverted.
    let _mmcss = common::mmcss::join_pro_audio();
    if _mmcss.is_none() {
        tracing::warn!("audio worker: MMCSS join (Pro Audio) failed");
    }
    let park = WorkerPark { shared };
    let bit = 1u64 << runner;
    shared.idle.fetch_or(bit, Ordering::SeqCst);
    loop {
        // 仕事が積まれるまで寝る (bit は起こした側が下ろしている)。不変条件 4 が禁じているのは「**他プロセスの完了待ち**を
        // 無限にすること」で、この event は同一プロセスの runner が起こす。RT deadline を握らない (起きなければ何も
        // 走らないだけ) ので INFINITE でよい。完了待ちの側 (master) は bounded。
        unsafe {
            WaitForSingleObject(wake.0, INFINITE); // arch-lint: allow-infinite
        }
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // 立ててから読む (module doc)。
        let inside = Inside::enter(&shared.inside);
        loop {
            // SAFETY: ここで読めた `ctx` は pool の器 (この thread も `Arc` で持つ) を指し、呼び側はこの `inside` が
            // 下りるまで、それが借りている物を解放も再利用もしない (module doc の「窓」)。
            let ctx = unsafe { shared.ctx.load(Ordering::SeqCst).cast_const().cast::<RenderCtx<'_>>().as_ref() };
            if let Some(ctx) = ctx {
                let graph = ctx.graph;
                drain(graph, &park, &mut |job| run_job(shared, ctx, graph, job, runner));
            }
            // 登録してから見直す: 積む側 (と buffer の頭で文脈を置く master) は「置いてから bit を読む」ので、この後に
            // 積まれた job では必ず起こされる。仕事が無ければ bit を立てたまま寝に行く。
            shared.idle.fetch_or(bit, Ordering::SeqCst);
            let ctx = unsafe { shared.ctx.load(Ordering::SeqCst).cast_const().cast::<RenderCtx<'_>>().as_ref() };
            if !ctx.is_some_and(|c| !c.graph.is_done() && c.graph.has_work(false)) {
                break;
            }
            // 仕事があった: bit を取り戻して続ける (起こす側に先に下ろされていたら、次の命令で打たれる signal を
            // 消費する — `inside` を持ったままなので bounded に)。消費した signal が Drop の起こしだったら抜ける。
            shared.unregister(runner, false, POOL_WAIT_TIMEOUT_MS);
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            // 予約済みで未書き込みの席 (書き手が次の命令で書く) を待つ間だけ回る。
            std::hint::spin_loop();
        }
        drop(inside);
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
    }
}

/// 文脈を読んでいる印 (`DispatchShared::inside`)。立てるのは [`Self::enter`]、下ろすのは drop (panic の unwind でも) —
/// 下ろし忘れると窓が永久に閉じず、呼び側は文脈の指す物を返せない (module doc)。
struct Inside<'a>(&'a AtomicU32);

impl<'a> Inside<'a> {
    fn enter(n: &'a AtomicU32) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        Self(n)
    }
}

impl Drop for Inside<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests;

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
pub(crate) fn boost_thread_priority(label: &str) {
    unsafe {
        let h = GetCurrentThread();
        if let Err(e) = SetThreadPriority(h, THREAD_PRIORITY_TIME_CRITICAL) {
            tracing::warn!(error = ?e, "{label}: failed to raise thread priority");
        }
    }
}
