//! Audio-engine worker pool. Pairs 1:1 with the plugin host's worker
//! pool: each audio worker `i` dispatches plugin work to plugin-host
//! worker `i` via `SyncSlot[i]`.
//!
//! Master calls `dispatch_and_wait` once per buffer. The master itself
//! also drains the work queue (work-stealing fanout) so machines with
//! `available_parallelism() == 1` still make progress without spawning.
//!
//! All shared state crosses the worker boundary as raw pointers stashed
//! in atomics. The dispatch handshake (wake events + pending counter +
//! all_done event) guarantees those pointers are only read while the
//! master holds the matching memory exclusively.
//!
//! plan §4 (有界化): `all_done` 待ちは `POOL_WAIT_TIMEOUT_MS` で bounded。
//! per-pair の plugin dispatch 自体が `DISPATCH_TIMEOUT_MS` で bounded +
//! timeout で pair が poison される (以後 skip) ため、pool 全体の待ちが
//! これを超えるのは audio worker thread 自身の死 (panic / OS freeze) だけ。
//! timeout で pool は **stalled** (以後 dispatch しない) になり、notify
//! thread が `AudioEvent::WorkerPoolStalled` を送って GUI が plugin_host を
//! respawn → `OpenWorkerPool` 再送 (= 新 pool) で復旧する。

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use common::model::Song;
use common::plugin_ref::DISPATCH_TIMEOUT_MS;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetCurrentThread, INFINITE, SetEvent, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    WaitForSingleObject,
};

use crate::engine::{PluginRefs, SyncSlot};
use crate::graph::process_track_owned;
use crate::mixer::TrackScratch;

/// `all_done` 待ちの上限 (plan §4)。 各 pair の dispatch は
/// `DISPATCH_TIMEOUT_MS` で bounded + 最初の timeout で pair が poison され
/// 以後 skip になるので、 健全な worker は「高々 1 回の timeout + 残り track
/// の高速 skip / 通常処理」 で必ず終わる。 これを超える = worker thread 自身が
/// 死んでいる (pending が減らない) と解釈する。
const POOL_WAIT_TIMEOUT_MS: u32 = DISPATCH_TIMEOUT_MS * 2;

#[derive(Copy, Clone)]
struct SendableHandle(HANDLE);
unsafe impl Send for SendableHandle {}
unsafe impl Sync for SendableHandle {}

/// Raw-pointer view of the per-buffer state.
///
/// Master writes the pointers + scalars before signalling the workers;
/// workers (and master itself) only read while the dispatch is in
/// flight.
pub struct DispatchShared {
    pub next_track: AtomicU32,
    pub n_tracks: AtomicU32,
    pub pending: AtomicU32,

    pub song_ptr: AtomicPtr<Song>,
    pub scratch_base: AtomicPtr<TrackScratch>,
    /// v29: device_id → `Arc<PluginEntry>` map (安定 id addressing)。
    pub plugin_refs_ptr: AtomicPtr<PluginRefs>,
    /// PR6: per-buffer audio clip render snapshot. `null` ⇒ workers
    /// pass `None` to `process_track_owned` (= audio clip mix を skip)。
    pub audio_renderer_ptr: AtomicPtr<crate::audio_clip_renderer::AudioClipRenderer>,
    /// worker handshake slots (`SyncSlot`)。 master = slot 0, worker i =
    /// slot i+1 (`run_work_loop` 参照)。
    pub slots_base: AtomicPtr<SyncSlot>,
    pub n_slots: AtomicU32,
    pub sample_rate: AtomicU32,
    pub frames: AtomicU32,
    pub playing: AtomicU8,
    pub any_solo: AtomicU8,
    /// 再生ループの状態 (= `SharedState::loop_region` の copy)。 master が dispatch
    /// 直前に自分のスタック上の値を publish し、 workers が `process_track_owned` に
    /// 渡して plugin transport の `loop_*_beats` / `looping` / IS_LOOP_ACTIVE 判定に
    /// 使う。 ON/OFF と範囲を別々の atomic に割ると worker が食い違った組を読みうる
    /// ので、`song_ptr` / `recording_lanes_ptr` と同じ「master が dispatch 窓の間だけ
    /// 生かす値へのポインタ」 idiom で 1 スナップショットとして渡す。 null = 既定
    /// (ループ無し)。
    pub loop_region_ptr: AtomicPtr<common::model::LoopRegion>,
    /// PR4.5 sidechain plugin-internal alignment: per-track input delay
    /// in samples (= `Schedule::input_delay_per_track` snapshotted into
    /// the worker pool's shared state for the current dispatch). `null`
    /// means the engine's main loop didn't pass a slice; workers treat
    /// that as 0 delay for every track.
    pub input_delays_base: AtomicPtr<u32>,
    pub n_input_delays: AtomicU32,
    /// docs/plan_modulation.md §5: per-`ModSource` follower scalar snapshot
    /// (block-rate) published by the master each dispatch; workers read it for
    /// audio-param modulation. Null + len 0 = no modulation.
    pub mod_scalars_base: AtomicPtr<f32>,
    pub n_mod_scalars: AtomicU32,
    /// Phase 4 Step C-2: 「現在 recording 中の lane」 set への ptr
    /// (= `SharedState.recording_lanes.load()` 結果)。 master が dispatch
    /// 前に store、 workers + master が `fill_track_param_ramps` の引数に
    /// 渡して curve eval を bypass する判定に使う。 null → 空 set 相当。
    pub recording_lanes_ptr:
        AtomicPtr<std::collections::HashSet<(u32, common::model::AutomationTarget)>>,
    /// Phase 5 Step 5.2: 当該 buffer の effective bpm (= SongTempo lane 評価
    /// or song.bpm fallback) を f32 bits で atomic 配信。 workers が
    /// `f32::from_bits(load())` で復元。
    pub current_bpm_bits: AtomicU32,
    /// Phase 5 follow-up (MIDI tempo follow): buffer 開始時の累積 beat-domain
    /// playhead を f64 bits で atomic 配信。 workers が
    /// `f64::from_bits(load())` で復元、 `collect_events_for_buffer` に渡す。
    pub playhead_beats_bits: std::sync::atomic::AtomicU64,
}

unsafe impl Send for DispatchShared {}
unsafe impl Sync for DispatchShared {}

impl DispatchShared {
    pub fn new() -> Self {
        Self {
            next_track: AtomicU32::new(0),
            n_tracks: AtomicU32::new(0),
            pending: AtomicU32::new(0),
            song_ptr: AtomicPtr::new(std::ptr::null_mut()),
            scratch_base: AtomicPtr::new(std::ptr::null_mut()),
            plugin_refs_ptr: AtomicPtr::new(std::ptr::null_mut()),
            audio_renderer_ptr: AtomicPtr::new(std::ptr::null_mut()),
            slots_base: AtomicPtr::new(std::ptr::null_mut()),
            n_slots: AtomicU32::new(0),
            sample_rate: AtomicU32::new(48_000),
            frames: AtomicU32::new(0),
            playing: AtomicU8::new(0),
            any_solo: AtomicU8::new(0),
            loop_region_ptr: AtomicPtr::new(std::ptr::null_mut()),
            input_delays_base: AtomicPtr::new(std::ptr::null_mut()),
            n_input_delays: AtomicU32::new(0),
            mod_scalars_base: AtomicPtr::new(std::ptr::null_mut()),
            n_mod_scalars: AtomicU32::new(0),
            recording_lanes_ptr: AtomicPtr::new(std::ptr::null_mut()),
            current_bpm_bits: AtomicU32::new(120.0_f32.to_bits()),
            playhead_beats_bits: std::sync::atomic::AtomicU64::new(
                0.0_f64.to_bits(),
            ),
        }
    }
}

impl Default for DispatchShared {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AudioWorkerPool {
    workers: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    wakes: Vec<SendableHandle>,
    all_done: SendableHandle,
    shared: Arc<DispatchShared>,
    /// plan §4: `all_done` timeout を観測した。 以後 `dispatch_and_wait` は
    /// 即 return (= 音は無音になるが CPAL callback は回り続ける)。 pool
    /// 再構築 (`OpenWorkerPool` 再送 = 新 `WorkerRig`) で復旧。
    stalled: AtomicBool,
    /// Drop で join を諦めて detach した worker がある (= wake / all_done の
    /// HANDLE を close しない — detached thread がまだ待っている可能性がある)。
    detached: AtomicBool,
}

impl AudioWorkerPool {
    /// Create a pool sized to `n_sync_slots` (= the number of
    /// handshake pairs with the plugin host). Each
    /// concurrent runner owns exactly one sync slot for the whole
    /// dispatch: the master (which joins the work loop from
    /// `dispatch_and_wait`) owns slot 0, worker thread `i` owns slot
    /// `i + 1` — so `n_sync_slots - 1` worker threads are spawned.
    /// Sharing a slot between two concurrent runners is not allowed:
    /// the wake/done events are auto-reset, so overlapping `dispatch()`
    /// calls on one slot collapse SetEvent pairs and either deadlock the
    /// audio callback or feed a plugin stale buffers.
    ///
    /// v29: 呼び出しは recv loop (off-thread)。 spawn / join が RT を塞がない。
    pub fn new(n_sync_slots: u32) -> Result<Self> {
        let n_workers = n_sync_slots.saturating_sub(1);
        let shared = Arc::new(DispatchShared::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        // One unnamed all_done event; created with auto-reset, initial
        // non-signaled. We don't need a named event because both ends
        // live in the same process.
        let all_done = SendableHandle(create_anonymous_event()?);
        let mut wakes = Vec::with_capacity(n_workers as usize);
        for _ in 0..n_workers {
            wakes.push(SendableHandle(create_anonymous_event()?));
        }

        let mut workers = Vec::with_capacity(n_workers as usize);
        for (i, &wake) in wakes.iter().enumerate() {
            let shared_w = Arc::clone(&shared);
            let shutdown_w = Arc::clone(&shutdown);
            let all_done_w = all_done;
            // Worker i owns sync slot i+1 (slot 0 is the master's).
            let sync_slot = i + 1;
            let handle = std::thread::Builder::new()
                .name(format!("audio-worker-{i}"))
                .spawn(move || run_worker(shared_w, shutdown_w, wake, all_done_w, sync_slot))?;
            workers.push(handle);
        }

        Ok(Self {
            workers,
            shutdown,
            wakes,
            all_done,
            shared,
            stalled: AtomicBool::new(false),
            detached: AtomicBool::new(false),
        })
    }

    /// pool が stalled (= `all_done` timeout を観測して dispatch 停止中) か。
    /// notify thread が poll して `WorkerPoolStalled` を GUI へ送る。
    pub fn is_stalled(&self) -> bool {
        self.stalled.load(Ordering::Acquire)
    }

    /// Run one buffer of work. Master writes the dispatch state into
    /// `shared`, signals every worker, then participates in the
    /// work-stealing loop itself before waiting (bounded) for the all_done
    /// event.
    ///
    /// SAFETY: caller must hold exclusive ownership of every referenced
    /// `&mut` slice / map for the duration of this call. The dispatch
    /// barrier (wake/all_done) ensures the workers only touch this
    /// state inside the call. `all_done` timeout 後の残余アクセスは
    /// `POOL_WAIT_TIMEOUT_MS` の導出 (各 pair は高々 1 回の bounded timeout
    /// で poison → 以後 skip) により実質起こらない — 残るのは死んだ thread
    /// (メモリに触らない) だけ、 と解釈して stalled に遷移する。
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_and_wait(
        &self,
        song: Option<&Song>,
        scratch: &mut [TrackScratch],
        plugin_refs: &PluginRefs,
        audio_renderer: &crate::audio_clip_renderer::AudioClipRenderer,
        slots: &[SyncSlot],
        sample_rate: u32,
        frames: u32,
        playing: bool,
        any_solo: bool,
        input_delay_per_track: &[u32],
        recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
        current_bpm: f32,
        playhead_beats: f64,
        loop_region: &common::model::LoopRegion,
        mod_scalars: &[f32],
    ) {
        // plan §4: stalled pool は二度と dispatch しない (worker thread の
        // 生死が不明なため)。 無音のまま CPAL callback は回り続ける。
        if self.stalled.load(Ordering::Acquire) {
            return;
        }
        let n_tracks = song.map(|s| s.tracks.len() as u32).unwrap_or(0);
        let n_tracks = n_tracks.min(scratch.len() as u32);
        if n_tracks == 0 {
            return;
        }

        // Publish raw pointers + scalars into the shared state.
        self.shared
            .song_ptr
            .store(song.map_or(std::ptr::null_mut(), |s| s as *const Song as *mut Song), Ordering::Release);
        self.shared
            .scratch_base
            .store(scratch.as_mut_ptr(), Ordering::Release);
        self.shared.plugin_refs_ptr.store(
            plugin_refs as *const _ as *mut _,
            Ordering::Release,
        );
        self.shared.audio_renderer_ptr.store(
            audio_renderer as *const _ as *mut _,
            Ordering::Release,
        );
        self.shared.slots_base.store(
            slots.as_ptr() as *mut SyncSlot,
            Ordering::Release,
        );
        self.shared
            .n_slots
            .store(slots.len() as u32, Ordering::Release);
        self.shared.sample_rate.store(sample_rate, Ordering::Release);
        self.shared.frames.store(frames, Ordering::Release);
        self.shared
            .playing
            .store(if playing { 1 } else { 0 }, Ordering::Release);
        self.shared
            .any_solo
            .store(if any_solo { 1 } else { 0 }, Ordering::Release);
        self.shared.loop_region_ptr.store(
            loop_region as *const _ as *mut _,
            Ordering::Release,
        );
        self.shared.recording_lanes_ptr.store(
            recording_lanes as *const _ as *mut _,
            Ordering::Release,
        );
        self.shared
            .current_bpm_bits
            .store(current_bpm.to_bits(), Ordering::Release);
        self.shared
            .playhead_beats_bits
            .store(playhead_beats.to_bits(), Ordering::Release);
        // PR4.5: publish per-track input delay slice so workers can read
        // their track's value without locking. Empty slice (= no
        // sidechain wiring anywhere) → null pointer + len 0.
        if input_delay_per_track.is_empty() {
            self.shared
                .input_delays_base
                .store(std::ptr::null_mut(), Ordering::Release);
            self.shared.n_input_delays.store(0, Ordering::Release);
        } else {
            self.shared.input_delays_base.store(
                input_delay_per_track.as_ptr() as *mut u32,
                Ordering::Release,
            );
            self.shared
                .n_input_delays
                .store(input_delay_per_track.len() as u32, Ordering::Release);
        }
        // docs/plan_modulation.md §5: publish the follower scalar snapshot so
        // workers read it lock-free. Empty (no sources) → null + len 0.
        if mod_scalars.is_empty() {
            self.shared
                .mod_scalars_base
                .store(std::ptr::null_mut(), Ordering::Release);
            self.shared.n_mod_scalars.store(0, Ordering::Release);
        } else {
            self.shared
                .mod_scalars_base
                .store(mod_scalars.as_ptr() as *mut f32, Ordering::Release);
            self.shared
                .n_mod_scalars
                .store(mod_scalars.len() as u32, Ordering::Release);
        }

        let n_workers = self.workers.len() as u32;
        // pending = N workers; master itself is *also* a runner but is
        // not counted because it joins the work-stealing loop directly
        // and waits for all_done after.
        self.shared.next_track.store(0, Ordering::Release);
        self.shared.n_tracks.store(n_tracks, Ordering::Release);
        self.shared.pending.store(n_workers, Ordering::Release);

        // Wake all workers.
        for w in &self.wakes {
            unsafe {
                let _ = SetEvent(w.0);
            }
        }

        // Master joins the work-stealing loop too (as sync slot 0). This
        // way machines with 1 sync slot (= zero workers) still make
        // progress and we get the full physical-core throughput.
        run_work_loop(&self.shared, 0);

        // Wait for the last worker to flag completion — **bounded** (plan
        // §4)。 With zero workers nobody signals all_done — the master
        // already did all the work.
        if n_workers > 0 {
            let wait = unsafe { WaitForSingleObject(self.all_done.0, POOL_WAIT_TIMEOUT_MS) };
            if wait != WAIT_OBJECT_0 {
                // timeout / fail: worker thread が死んでいる (bounded dispatch
                // の下で pending が減らないのは thread 消滅のみ)。 以後この
                // pool では dispatch しない。 通知 (WorkerPoolStalled) は
                // notify thread が `is_stalled` を poll して送る — RT からは
                // atomic store のみ (tracing / IPC 禁止)。
                self.stalled.store(true, Ordering::Release);
            }
        }
    }

    pub fn n_workers(&self) -> usize {
        self.workers.len()
    }
}

impl Drop for AudioWorkerPool {
    /// Tear the pool down: flag shutdown, wake every worker so it drops out
    /// of its `WaitForSingleObject(INFINITE)` idle, join the threads
    /// **bounded**, then close the wake/all_done event HANDLEs.
    ///
    /// v29 (plan §4): Drop は recycle ring 経由の off-thread (recv loop) で
    /// 走るが、 stuck worker を無限 join すると respawn 経路が二次ハングする
    /// ので有界化する。 worker が block しうるのは (a) wake 待ち (INFINITE —
    /// ここで叩き起こす)、 (b) plugin dispatch (bounded `DISPATCH_TIMEOUT_MS`)
    /// のみなので、 期限は 2×DISPATCH で十分。 期限超過の worker は detach
    /// (leak) し、 その場合 wake/all_done の HANDLE close も見送る (detached
    /// thread がまだ待っている可能性があるため — kernel handle 数個のリーク
    /// は破滅的イベント時のみ)。
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        for w in &self.wakes {
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
            // Threads have exited; the wake/all_done events are now unused.
            for w in &self.wakes {
                unsafe {
                    let _ = CloseHandle(w.0);
                }
            }
            unsafe {
                let _ = CloseHandle(self.all_done.0);
            }
        }
    }
}

fn run_worker(
    shared: Arc<DispatchShared>,
    shutdown: Arc<AtomicBool>,
    wake: SendableHandle,
    all_done: SendableHandle,
    sync_slot: usize,
) {
    boost_thread_priority("audio worker");
    // Join "Pro Audio" so MMCSS keeps this thread on the priority-class
    // schedule the audio mixer/sequencer loop relies on. Held until the
    // worker drops out of `run_worker`, then auto-reverted.
    let _mmcss = common::mmcss::join_pro_audio();
    if _mmcss.is_none() {
        tracing::warn!("audio worker: MMCSS join (Pro Audio) failed");
    }
    loop {
        unsafe {
            WaitForSingleObject(wake.0, INFINITE);
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        run_work_loop(&shared, sync_slot);
        // Last worker out signals completion.
        if shared.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            unsafe {
                let _ = SetEvent(all_done.0);
            }
        }
    }
}

/// Pull tracks off `next_track` until `n_tracks`, calling
/// `process_track_owned` for each. `sync_slot` is this runner's dedicated
/// index into the `SyncSlot` array (master = 0, worker i = i + 1) — see
/// [`AudioWorkerPool::new`] for why slots must not be shared.
fn run_work_loop(shared: &DispatchShared, sync_slot: usize) {
    let n_tracks = shared.n_tracks.load(Ordering::Acquire);
    if n_tracks == 0 {
        return;
    }
    let song_ptr = shared.song_ptr.load(Ordering::Acquire);
    let scratch_base = shared.scratch_base.load(Ordering::Acquire);
    let plugin_refs_ptr = shared.plugin_refs_ptr.load(Ordering::Acquire);
    let audio_renderer_ptr = shared.audio_renderer_ptr.load(Ordering::Acquire);
    let slots_base = shared.slots_base.load(Ordering::Acquire);
    let n_slots = shared.n_slots.load(Ordering::Acquire);
    let sample_rate = shared.sample_rate.load(Ordering::Acquire);
    let frames = shared.frames.load(Ordering::Acquire);
    let playing = shared.playing.load(Ordering::Acquire) != 0;
    let any_solo = shared.any_solo.load(Ordering::Acquire) != 0;
    // PR4.5 sidechain plugin-internal alignment: input-delay slice
    // published by `dispatch_and_wait`. May be null (no sidechain wiring),
    // in which case every track gets 0 delay.
    let input_delays_base = shared.input_delays_base.load(Ordering::Acquire);
    let n_input_delays = shared.n_input_delays.load(Ordering::Acquire);
    // docs/plan_modulation.md §5: follower scalar snapshot (null = none). One
    // global slice (not per-track), reconstructed once for the work loop.
    let mod_scalars_base = shared.mod_scalars_base.load(Ordering::Acquire);
    let n_mod_scalars = shared.n_mod_scalars.load(Ordering::Acquire);
    let mod_scalars: &[f32] = if mod_scalars_base.is_null() || n_mod_scalars == 0 {
        &[]
    } else {
        // SAFETY: the master holds the snapshot Vec
        // (`LocalState::mod_scalars_snapshot`) alive for the dispatch window via
        // `dispatch_and_wait`'s borrow, and `n_mod_scalars` is its real length.
        unsafe {
            std::slice::from_raw_parts(mod_scalars_base as *const f32, n_mod_scalars as usize)
        }
    };
    // Phase 4 Step C-2: recording lane snapshot ptr。 null なら 空 set 相当
    // (= 全 lane の curve eval する)。 master が `dispatch_and_wait` 内で
    // store、 ここでは &HashSet として復元する。
    let recording_lanes_ptr = shared.recording_lanes_ptr.load(Ordering::Acquire);
    let empty_recording_lanes: std::collections::HashSet<(u32, common::model::AutomationTarget)> =
        std::collections::HashSet::new();
    let recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)> =
        if recording_lanes_ptr.is_null() {
            &empty_recording_lanes
        } else {
            // SAFETY: master holds the ArcSwap Guard / Arc snapshot alive
            // for the dispatch window via `dispatch_and_wait` 's local var.
            unsafe { &*recording_lanes_ptr }
        };
    // Phase 5 Step 5.2: master が当該 buffer の effective bpm を atomic で
    // 配信。 f32 bits を Acquire で load して復元する。
    let current_bpm =
        f32::from_bits(shared.current_bpm_bits.load(Ordering::Acquire));
    // Phase 5 follow-up (MIDI tempo follow): playhead_beats も同様に atomic
    // で配信。 f64 bits を Acquire で load。
    let playhead_beats =
        f64::from_bits(shared.playhead_beats_bits.load(Ordering::Acquire));
    // 再生ループの状態 (master が dispatch 前に store)。 `LoopRegion` は `Copy`
    // なので値へ deref して持つ (= 以後 raw pointer に触れない)。 null は既定
    // (ループ無し)。
    let loop_region_ptr = shared.loop_region_ptr.load(Ordering::Acquire);
    let loop_region = if loop_region_ptr.is_null() {
        common::model::LoopRegion::default()
    } else {
        // SAFETY: master holds the value on its stack for the dispatch window
        // via `dispatch_and_wait`'s `&LoopRegion` borrow.
        unsafe { *loop_region_ptr }
    };

    if scratch_base.is_null() || plugin_refs_ptr.is_null() || slots_base.is_null() {
        return;
    }

    let song: Option<&Song> = if song_ptr.is_null() {
        None
    } else {
        // SAFETY: master holds the snapshot Arc<Song> for the dispatch
        // window, so the pointee is alive.
        Some(unsafe { &*song_ptr })
    };
    let plugin_refs = unsafe { &*plugin_refs_ptr };
    // PR6: audio_renderer_ptr が null のときは render を skip (= None)。
    let audio_renderer: Option<&crate::audio_clip_renderer::AudioClipRenderer> =
        if audio_renderer_ptr.is_null() {
            None
        } else {
            // SAFETY: master holds the AudioClipRenderer (Guard or
            // ArcSwap snapshot) alive for the dispatch window.
            Some(unsafe { &*audio_renderer_ptr })
        };
    let slots = unsafe { std::slice::from_raw_parts(slots_base, n_slots as usize) };

    loop {
        let track_idx = shared.next_track.fetch_add(1, Ordering::AcqRel);
        if track_idx >= n_tracks {
            break;
        }
        // Per-track scratch is exclusive to this dispatch via the
        // claim-by-index counter above.
        let scratch = unsafe { &mut *scratch_base.add(track_idx as usize) };
        // This runner's dedicated SyncSlot (1:1 paired with a
        // plugin-host worker): master = slot 0, worker i = slot i+1.
        // Never select by track index — with work stealing, two runners
        // would then use one slot concurrently and the auto-reset
        // wake/done handshake collapses (deadlocked callback / stale
        // plugin buffers).
        let worker_sync = slots.get(sync_slot);

        let Some(song) = song else { continue };
        let Some(song_track) = song.tracks.get(track_idx as usize) else {
            continue;
        };

        // PR4.5: read this track's input_delay; falls back to 0 if the
        // master didn't publish a slice (= no sidechain wiring) or the
        // index is out of range (defensive, shouldn't happen).
        let input_delay = if input_delays_base.is_null() || track_idx >= n_input_delays {
            0u32
        } else {
            // SAFETY: master holds the slice alive for the dispatch window
            // (it lives in `Schedule::input_delay_per_track` cached on the
            // engine), and `track_idx < n_input_delays` keeps us in bounds.
            unsafe { *input_delays_base.add(track_idx as usize) }
        };
        process_track_owned(
            track_idx,
            song_track,
            scratch,
            plugin_refs,
            audio_renderer,
            worker_sync,
            sample_rate,
            frames,
            playing,
            Some(song),
            any_solo,
            input_delay,
            recording_lanes,
            current_bpm,
            playhead_beats,
            loop_region,
            mod_scalars,
        );
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
