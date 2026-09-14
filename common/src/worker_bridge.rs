//! Shared-memory layout for the audio-engine ↔ plugin-host worker pool
//! handshake.
//!
//! The two processes each spawn N worker threads in 1:1 pairs. When audio
//! engine `worker[i]` wants `plugin_host worker[i]` to call
//! `plugin.process()` for a particular plugin instance, it hands the
//! instance's **token** (`protocol::InstanceToken`, plugin_host 採番・非再利用。
//! `device_id` は project をまたいで衝突するので使わない) over `channels[i]`.
//!
//! # 受け渡し (回って待ち、間に合わなければ寝る)
//!
//! 依頼と完了は **共有メモリ上の番号** で受け渡す ([`WorkerChannel`])。audio 側は token を置いて依頼番号を進め、
//! host 側は依頼番号が進んだのを見て `process()` を呼び、終えた番号を書く。どちらも相手を **少しの間回って待ち**
//! (buffer 周期に比例した短い時間、[`spin_budget`])、間に合わなければ自分の event で寝る。寝る側は「寝る」印を
//! 立ててから番号を見直し、起こす側は番号を書いてから印を読んで、立っていたときだけ event を signal する
//! (Dekker 型: どちらかが必ず相手を見る)。
//!
//! 1 つの chain の plugin は同じ pair に続けて依頼されるので、依頼の間隔 (audio 側の内蔵 device や合流) も
//! plugin の処理そのものも、多くは回っている間に終わる — 1 往復ごとに 2 回の kernel event と 2 回のスレッドの
//! 起床 (~10 µs) を払わない。寝ている相手を起こすときだけ event を使う。
//!
//! # 世代
//!
//! 依頼番号は `generation << 32 | seq` で、host は **自分の世代の番号だけ** を受ける。pool を作り直すと (plugin_host
//! の respawn / `OpenWorkerPool` の再送) 世代が進むので、前の世代で timeout したまま残った依頼 (poisoned pair) を
//! 新しい host が拾って、別の plugin に同じ token で `process()` させない。起動の順序も問わない:
//! audio 側が先に依頼を置いても、後から起きた host は自分の世代の未処理の依頼として拾う。host は token を読んだ後に
//! 依頼番号を読み直し、変わっていたら読み直す (前の世代の worker が止まったまま残っていて、後から書き込んだ場合)。
//!
//! Only one shmem instance exists for the whole daw_01 session — its name
//! is fixed (see `plugin_ref::worker_bridge_shmem_id`).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use crate::protocol::InstanceToken;

/// Hard cap on workers. CPU core counts above this are extremely rare for
/// audio workloads (2026: typical desktop is 4–24 cores).
pub const MAX_WORKERS: usize = 32;

/// runner の数に上乗せする予備の pair の数。timeout した依頼の中に host の worker が居る pair (poisoned) を借りていた
/// runner が借り替える先 (host がその依頼を終えれば pair は空きに戻る)。同時に詰まる plugin がこの数を越えて
/// 残るときだけ、その runner に回った plugin が素通しになり、持続すれば plugin_host を立て直す。
pub const SPARE_PAIRS: u32 = 2;

/// 回って待つ時間 = buffer 周期の 1/100 (256 frame @ 48 kHz で ~53 µs、1024 frame で ~213 µs)。1 往復の event と
/// 起床 (~10 µs) より十分長く、相手の core を奪い続けるほどは長くない。値は共有メモリ由来 (信頼境界の外) でも
/// あるので、どんな値でも [`MAX_SPIN`] で頭打ちにする。
#[must_use]
pub fn spin_budget(frames: u32, sample_rate: u32) -> Duration {
    let micros = u64::from(frames) * 1_000_000 / u64::from(sample_rate.max(1)) / 100;
    Duration::from_micros(micros).min(MAX_SPIN)
}

/// [`spin_budget`] の上限 (8 kHz で最大 buffer を回しても超えない程度)。
pub const MAX_SPIN: Duration = Duration::from_millis(2);

/// 1 組の worker pair (audio worker i ↔ plugin-host worker i) の受け渡し口。pair ごとに cache line を分ける。
#[repr(C, align(64))]
pub struct WorkerChannel {
    /// 依頼の番号 (`generation << 32 | seq`)。書くのは audio 側だけ。
    pub request: AtomicU64,
    /// 依頼の instance token。audio 側が `request` を進める **前** に書く (host は `request` を読んでから読む)。
    pub task: AtomicU64,
    /// host が終えた依頼の番号。書くのは host 側だけ。
    pub completed: AtomicU64,
    /// host が wake event で寝ている (寝ようとしている)。
    pub host_parked: AtomicU32,
    /// audio 側が done event で寝ている (寝ようとしている)。
    pub audio_parked: AtomicU32,
}

impl WorkerChannel {
    const fn new() -> Self {
        Self {
            request: AtomicU64::new(0),
            task: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            host_parked: AtomicU32::new(0),
            audio_parked: AtomicU32::new(0),
        }
    }

    /// audio 側: 次の依頼番号 (この世代の最初なら seq 1)。書き手は audio 側だけなので読んで足すだけでよい。
    fn next_request(&self, generation: u32) -> u64 {
        let prev = self.request.load(Ordering::Relaxed);
        let seq = if (prev >> 32) as u32 == generation { (prev as u32).wrapping_add(1).max(1) } else { 1 };
        (u64::from(generation) << 32) | u64::from(seq)
    }
}

/// [`WorkerChannel`] の依頼の結果 (audio 側)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// plugin_host worker が process() を完了した (通常経路)。
    Done,
    /// `timeout_ms` 内に完了しなかった。**この worker pair は poisoned**
    /// — 呼び出し側は該当 device を quarantine し、host がその依頼を終えるまでこの
    /// pair で依頼しないこと (`plugin_ref` の module doc の contract 参照)。
    TimedOut,
    /// 依頼は置いたが done event の待ち自体が失敗した (`WAIT_FAILED` 等)。host 側で `process()` が走っているか
    /// 分からないので **`TimedOut` と同じ** 扱い (pair を poison、device を quarantine)。
    WaitFailed,
}

#[repr(C)]
pub struct WorkerBridge {
    /// `channels[i]` = audio-engine `worker[i]` ↔ plugin-host `worker[i]`。
    pub channels: [WorkerChannel; MAX_WORKERS],
}

impl WorkerBridge {
    pub fn zeroed() -> Self {
        Self { channels: [const { WorkerChannel::new() }; MAX_WORKERS] }
    }
}

#[cfg(windows)]
mod handshake {
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{INFINITE, SetEvent, WaitForSingleObject};

    use super::*;

    /// `until` まで `ready` を回って見る (時刻は数回に 1 度だけ読む)。
    fn spin_until(until: Instant, ready: impl Fn() -> bool) -> bool {
        let mut k = 0u32;
        loop {
            if ready() {
                return true;
            }
            k = k.wrapping_add(1);
            if k.is_multiple_of(16) && Instant::now() >= until {
                return false;
            }
            std::hint::spin_loop();
        }
    }

    impl WorkerChannel {
        /// audio 側: `token` の `process()` を依頼し、完了を **有界** に待つ (`spin` まで回ってから done event で寝る)。
        /// RT から呼ぶので失敗も値で返す (確保しない)。
        pub fn dispatch(
            &self,
            generation: u32,
            token: InstanceToken,
            wake: HANDLE,
            done: HANDLE,
            spin: Duration,
            timeout_ms: u32,
        ) -> DispatchOutcome {
            let start = Instant::now();
            let request = self.next_request(generation);
            self.task.store(token.0, Ordering::SeqCst);
            self.request.store(request, Ordering::SeqCst);
            // 書いてから寝ている印を読む (host は印を立ててから番号を見直す)。起こせなくても (handle の故障) 依頼は
            // 共有メモリに置いてあり、回っている host は拾うので、結果は完了の待ちで判定する (拾われなければ timeout)。
            if self.host_parked.load(Ordering::SeqCst) != 0 {
                unsafe {
                    let _ = SetEvent(wake);
                }
            }
            let finished = || self.completed.load(Ordering::SeqCst) == request;
            if spin_until(start + spin, finished) {
                return DispatchOutcome::Done;
            }
            let deadline = start + Duration::from_millis(u64::from(timeout_ms));
            loop {
                // 立ててから見直す (host は完了を書いてから印を読む)。起きたら番号で判定する — 前の依頼で寝ずに
                // 済ませた回の signal が残っていても、番号が進んでいなければ寝直すだけ。
                self.audio_parked.store(1, Ordering::SeqCst);
                let now = Instant::now();
                if finished() || now >= deadline {
                    self.audio_parked.store(0, Ordering::SeqCst);
                    return if finished() { DispatchOutcome::Done } else { DispatchOutcome::TimedOut };
                }
                let left_ms = u32::try_from((deadline - now).as_millis()).unwrap_or(u32::MAX).max(1);
                let wait = unsafe { WaitForSingleObject(done, left_ms) };
                if wait != WAIT_OBJECT_0 && wait != WAIT_TIMEOUT {
                    self.audio_parked.store(0, Ordering::SeqCst);
                    return DispatchOutcome::WaitFailed;
                }
            }
        }

        /// host 側: この世代でまだ終えていない依頼の番号の起点 (前にこの世代で終えた依頼があればそれ)。
        #[must_use]
        pub fn host_start(&self, generation: u32) -> u64 {
            let done = self.completed.load(Ordering::SeqCst);
            if (done >> 32) as u32 == generation { done } else { u64::from(generation) << 32 }
        }

        /// host 側: この世代の次の依頼 (`last` より後) を待つ。`spin` まで回ってから wake event で寝る。`None` =
        /// `shutdown` が立った。
        pub fn wait_request(
            &self,
            generation: u32,
            last: u64,
            wake: HANDLE,
            spin: Duration,
            shutdown: &AtomicBool,
        ) -> Option<(u64, InstanceToken)> {
            let pending = || {
                let r = self.request.load(Ordering::SeqCst);
                ((r >> 32) as u32 == generation && r != last).then_some(r)
            };
            // token を読んだ後に番号を読み直す: 番号が変わらなければ、その token はこの番号の依頼のもの。
            let take = || loop {
                let r = pending()?;
                let token = InstanceToken(self.task.load(Ordering::SeqCst));
                if self.request.load(Ordering::SeqCst) == r {
                    return Some((r, token));
                }
            };
            if spin_until(Instant::now() + spin, || pending().is_some() || shutdown.load(Ordering::SeqCst))
                && !shutdown.load(Ordering::SeqCst)
                && let Some(got) = take()
            {
                return Some(got);
            }
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    return None;
                }
                // 立ててから見直す (audio 側は依頼を書いてから印を読む)。
                self.host_parked.store(1, Ordering::SeqCst);
                if pending().is_some() {
                    self.host_parked.store(0, Ordering::SeqCst);
                    if let Some(got) = take() {
                        return Some(got);
                    }
                    continue;
                }
                // 不変条件 4 が禁じているのは RT が **他プロセスの完了を** 無限に待つこと。ここは host の worker が
                // 次の依頼を待つ側 (RT deadline を握らない)。完了待ちの audio 側は bounded。
                unsafe {
                    WaitForSingleObject(wake, INFINITE); // arch-lint: allow-infinite
                }
                self.host_parked.store(0, Ordering::SeqCst);
            }
        }

        /// host 側: 依頼 `request` を終えた (寝ている audio 側だけを起こす)。
        pub fn complete(&self, request: u64, done: HANDLE) {
            self.completed.store(request, Ordering::SeqCst);
            if self.audio_parked.load(Ordering::SeqCst) != 0 {
                unsafe {
                    let _ = SetEvent(done);
                }
            }
        }
    }
}

#[cfg(windows)]
mod shmem_handle {
    use anyhow::Result;

    use super::WorkerBridge;
    use crate::shmem::NamedShmem;

    pub struct WorkerBridgeHandle {
        shmem: NamedShmem,
    }

    impl WorkerBridgeHandle {
        pub fn create(os_id: &str) -> Result<Self> {
            let shmem = NamedShmem::create(os_id, std::mem::size_of::<WorkerBridge>())?;
            // The shmem view is backed by `MapViewOfFile`, which returns a
            // pointer aligned to at least 64 KiB (the system allocation
            // granularity). That trivially satisfies `WorkerBridge`'s
            // alignment (== `WorkerChannel` align == 64).
            debug_assert!(
                (shmem.as_ptr() as usize).is_multiple_of(std::mem::align_of::<WorkerBridge>()),
                "worker_bridge shmem pointer is not WorkerBridge-aligned"
            );
            // SAFETY: `shmem` is freshly created with at least
            // `size_of::<WorkerBridge>()` bytes (`.size(...)` above) and the
            // pointer is 64 KiB-aligned (asserted), so it is valid for a
            // single aligned write of a `WorkerBridge`. No other handle to
            // this mapping exists yet, so there is no aliasing.
            unsafe {
                let bridge = shmem.as_ptr() as *mut WorkerBridge;
                std::ptr::write(bridge, WorkerBridge::zeroed());
            }
            Ok(Self { shmem })
        }

        pub fn open(os_id: &str) -> Result<Self> {
            let shmem = NamedShmem::open(os_id, std::mem::size_of::<WorkerBridge>())?;
            // `MapViewOfFile` aligns the view to the 64 KiB system allocation
            // granularity, which covers `WorkerBridge`'s 64-byte alignment.
            debug_assert!(
                (shmem.as_ptr() as usize).is_multiple_of(std::mem::align_of::<WorkerBridge>()),
                "worker_bridge shmem pointer is not WorkerBridge-aligned"
            );
            Ok(Self { shmem })
        }

        pub fn bridge(&self) -> &WorkerBridge {
            // SAFETY: the mapping is at least `size_of::<WorkerBridge>()` bytes
            // (checked in `create`/`open`) and the `MapViewOfFile` pointer is
            // 64 KiB-aligned, satisfying `WorkerBridge`'s 64-byte alignment.
            // `WorkerBridge` is `#[repr(C)]` and holds only atomics, which
            // are valid for any bit pattern, so the mapped bytes are always a
            // valid `WorkerBridge`. Concurrent access from the peer process is
            // sound because all fields are atomics.
            unsafe { &*(self.shmem.as_ptr() as *const WorkerBridge) }
        }
    }

    unsafe impl Send for WorkerBridgeHandle {}
    unsafe impl Sync for WorkerBridgeHandle {}
}

#[cfg(windows)]
pub use shmem_handle::WorkerBridgeHandle;

#[cfg(all(test, windows))]
mod tests;
