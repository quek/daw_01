//! Audio engine ↔ plugin host worker pool の plugin host 側。 audio
//! engine の N workers と 1:1 対応する N 個の worker thread を持つ。
//!
//! # buffer 毎の dispatch
//!
//! 各 worker:
//!   1. 自分の pair の `WorkerBridge::channels[i]` に、この世代の次の依頼 (instance token) が置かれるのを待つ
//!      (少し回ってから `wake` event で寝る — 手順は `common::worker_bridge` の module doc)。
//!   2. 自分の受け口 ([`RegistryInbox`]) に届いている最新の registry snapshot を採用し、
//!      その token を resolve して live な entry に対応していれば audio half の `process()` を呼ぶ。
//!   3. 完了を書く (audio worker が寝ていれば `done` event で起こす)。
//!
//! # registry snapshot の受け渡し (worker が旧 snapshot の最終参照にならない)
//!
//! 正本は plugin-main thread の [`PluginRegistry`]。変更のたびに新しい snapshot
//! (`Arc<RegistryMap>`) を worker ごとの単一 slot の受け口 ([`SnapshotMailbox`]) に置き、worker は
//! dispatch-critical section の頭でそれを取って差し替える。**差し替えた旧 snapshot は worker 上で
//! drop せず、recycle ring で plugin-main へ返す** (plugin-main が次の変更の前に回収して drop する)。
//! 旧実装は `ArcSwap::load` で読んでおり、store と重なると Guard が旧値の最終参照になって
//! worker (TIME_CRITICAL) 上で旧 map と、外した plugin の audio half が解放されえた
//! (arc-swap 1.9.1 hybrid 戦略、daw_audio の `RtBundle` と同じ根)。
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
//! 外す plugin を処理中の worker で `exit` が追いつくのを待つ。 registry から entry を外して
//! (`registry_remove` = 外した snapshot を全 worker の受け口に置く) から `quiesce` を呼べば、
//! 以後に critical section に入る worker は必ずその snapshot 以降を採用する (置く `swap` と
//! `enter` / 採用の `swap` がすべて `SeqCst`) ので、その audio half に触れない — そこで初めて
//! main half (と FFI plugin) を安全に deactivate / drop できる。
//!
//! 1 つの critical section が触れる audio half は依頼の token の 1 つだけなので、section に入る前に書く
//! `current[i]` が外す plugin でなければ、その worker は待たない (別の plugin で固まった worker に巻き込まれない)。
//! 待ちは有界で、上限までに抜けない plugin は token で返す (呼び出し側はその plugin に触れない — `crate::quiesce`)。
//!
//! missing-entry でも counter は bump する (SeqCst 全順序の
//! 論証を分岐 free に保つため)。

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;

use anyhow::Result;
use common::metrics_bridge::{MetricsBridgeHandle, PluginMetricsPlane, PluginSlotAllocator};
use common::protocol::{DeviceAddr, InstanceToken};
use common::plugin_ref::open_named_event;
use common::process_data::{Event, EventKind};
use common::protocol::{PluginEvent, WorkerPoolSpec};
use common::worker_bridge::{MAX_WORKERS, WorkerBridgeHandle, WorkerChannel};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetEvent, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};

use crate::plugin_instance::{AudioHalf, NoteTransition, TimedNoteEvent};

/// Per-plugin process-server entry (`InstanceToken` keyed, flat)。
/// `audio` は split-half の audio 側 ([`AudioHalf`])、 `process_data` は
/// audio engine が入力を書く shmem slot。
pub struct PluginEntry {
    /// この instance の住所 (param event を daw_gui へ返すときの宛先)。
    pub device: DeviceAddr,
    pub audio: Arc<AudioHalf>,
    pub process_data: *mut common::process_data::ProcessData,
    /// per-publish one-shot: `process()` Err / panic を最初の 1 回だけ
    /// log する (TIME_CRITICAL thread での毎 buffer format+log を排除)。
    /// republish (再 SetSlotPlugin / reinit) で新 entry になりリセット。
    pub err_logged: Arc<AtomicBool>,
    /// per-plugin 計測面 ([`PluginMetricsPlane`]) の枠番号。**registry に入れるとき plugin-main が決める**
    /// (`registry_insert` が token ごとの帳簿で上書きする。作る側は [`METRIC_SLOT_UNASSIGNED`] を入れる)。
    /// worker は store するだけ — RT で空き枠を探さない (`docs/plan_unbounded_tracks.md` §4)。detach /
    /// republish / reinit を跨いで同じ枠を使い、instance を壊すときに [`registry_release_metric_slot`] で空ける。
    pub metric_slot: u32,
    /// r.md #87: この instance へ渡す musical timeline を **曲全体の位置に固定**
    /// するか。既定 (`false`) は行の時間軸 = ランチャーで撃った行の plugin は
    /// セルの拍で動く。`true` になるのは **ARA を bind した instance だけ** —
    /// ARA の playback region は song 時間に固定されており、行の拍を渡すと
    /// Melodyne が曲頭付近を鳴らす (ARA のセル対応は未実装、`vst3_plugin.rs`
    /// の注記)。load 時の ARA bind 結果で確定する定数。
    pub transport_pinned_to_song: bool,
}

/// `PluginEntry::metric_slot` の「まだ枠を割り当てていない」sentinel。
pub const METRIC_SLOT_UNASSIGNED: u32 = u32::MAX;

impl Clone for PluginEntry {
    fn clone(&self) -> Self {
        Self {
            device: self.device,
            audio: Arc::clone(&self.audio),
            process_data: self.process_data,
            err_logged: Arc::clone(&self.err_logged),
            metric_slot: self.metric_slot,
            transport_pinned_to_song: self.transport_pinned_to_song,
        }
    }
}

unsafe impl Send for PluginEntry {}
unsafe impl Sync for PluginEntry {}

/// registry 1 世代の中身 ([`InstanceToken`] → [`PluginEntry`])。
pub type RegistryMap = HashMap<InstanceToken, PluginEntry>;

/// worker へ渡す registry の 1 世代: entry 表と、その世代の per-plugin 計測面。計測面を作り直したら
/// 新しい世代として置き直すので、worker が採用した entry と計測面は必ず同じ世代の組になる。
pub struct RegistrySnapshot {
    entries: RegistryMap,
    metrics: Option<Arc<PluginMetricsPlane>>,
}

impl std::ops::Deref for RegistrySnapshot {
    type Target = RegistryMap;
    fn deref(&self) -> &RegistryMap {
        &self.entries
    }
}

/// per-plugin 計測面の書き手 (plugin-main)。`WorkerPool::open` が固定 shmem を開いて繋ぐ。
struct MetricsWriter {
    bridge: MetricsBridgeHandle,
    plane: Option<Arc<PluginMetricsPlane>>,
    generation: u32,
}

/// worker が差し替えた旧 snapshot を plugin-main へ返す ring の容量。plugin-main は snapshot を置く
/// **前に** 必ず回収するので、1 回の回収から次の回収までに 1 本の worker が返すのは「回収と同時に
/// 採用していた 1 本」+「その回収の後に置かれた 1 本」の高々 2 本。余裕を持たせた値で、満杯には
/// 到達しない ([`RegistryInbox::adopt_latest`] は満杯でも worker 上で解放しない)。
const RETIRED_RING_CAP: usize = 8;

/// per-plugin 計測面の作成を世代を変えて試す回数 (名前の衝突を避ける。`ensure_metrics_capacity`)。
const METRICS_PLANE_CREATE_ATTEMPTS: usize = 4;

/// 単一 slot の最新 snapshot の受け渡し口 (worker 1 本ぶん)。所有権を `AtomicPtr` で移すので、
/// 置く側 (plugin-main) と取る側 (worker) のどちらも相手の保持中の値を解放しない。
/// 置く / 取るは `SeqCst` (`DispatchCounter` との全順序、module doc の quiesce の論証)。
struct SnapshotMailbox {
    slot: std::sync::atomic::AtomicPtr<RegistrySnapshot>,
}

impl SnapshotMailbox {
    fn new() -> Self {
        Self { slot: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }
    }

    /// plugin-main (非 RT): `snap` を置き、worker がまだ取っていなかった前の値を返す (解放は呼び出し側)。
    fn post(&self, snap: Arc<RegistrySnapshot>) -> Option<Arc<RegistrySnapshot>> {
        let prev = self.slot.swap(Arc::into_raw(snap).cast_mut(), Ordering::SeqCst);
        // SAFETY: slot に入るのは `post` の `Arc::into_raw` だけで、取り出した側が所有権を 1 回だけ戻す。
        (!prev.is_null()).then(|| unsafe { Arc::from_raw(prev) })
    }

    /// worker (RT): 置かれていれば取る。swap 1 回だけ (確保・解放なし)。
    fn take(&self) -> Option<Arc<RegistrySnapshot>> {
        let p = self.slot.swap(std::ptr::null_mut(), Ordering::SeqCst);
        // SAFETY: `post` と同じ。
        (!p.is_null()).then(|| unsafe { Arc::from_raw(p) })
    }
}

impl Drop for SnapshotMailbox {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// plugin-main 側の送り口 (worker 1 本ぶん)。
struct RegistryFeed {
    mailbox: Arc<SnapshotMailbox>,
    retired: rtrb::Consumer<Arc<RegistrySnapshot>>,
}

/// worker thread 側の受け口。worker が所有し、dispatch-critical section の頭で
/// [`Self::adopt_latest`] を呼ぶ。
pub struct RegistryInbox {
    mailbox: Arc<SnapshotMailbox>,
    retired: rtrb::Producer<Arc<RegistrySnapshot>>,
    current: Arc<RegistrySnapshot>,
    /// recycle ring が満杯だったときに返しそびれた旧 snapshot (次の採用で先に返す)。
    stash: Option<Arc<RegistrySnapshot>>,
}

impl RegistryInbox {
    /// 置かれている最新の snapshot を採用して、今の snapshot を返す。**worker (RT) 上で確保も解放もしない** —
    /// 差し替えた旧 snapshot は recycle ring で plugin-main へ返す。ring は `RETIRED_RING_CAP` の doc の上限に
    /// より満杯にならない。満杯なら手元に 1 本保って次の採用で先に返す (2 本目も溢れるのは上限の数倍の
    /// 未回収で、そこだけは worker 上で解放される)。
    fn adopt_latest(&mut self) -> &RegistrySnapshot {
        if let Some(stashed) = self.stash.take()
            && let Err(rtrb::PushError::Full(back)) = self.retired.push(stashed)
        {
            self.stash = Some(back);
        }
        if let Some(latest) = self.mailbox.take() {
            let old = std::mem::replace(&mut self.current, latest);
            if let Err(rtrb::PushError::Full(back)) = self.retired.push(old) {
                self.stash.get_or_insert(back);
            }
        }
        &self.current
    }
}

struct RegistryState {
    current: Arc<RegistrySnapshot>,
    feeds: Vec<RegistryFeed>,
    /// per-plugin 計測面の枠の帳簿 (面が繋がる前から割り当てる)。
    slots: PluginSlotAllocator,
    /// instance → 割り当て済みの枠。registry から外しても (detach) 残り、instance を壊したときに空ける。
    assigned: HashMap<InstanceToken, u32>,
    metrics: Option<MetricsWriter>,
}

impl RegistryState {
    /// 新しい中身を正本にして全 worker の受け口に置く。置く前に、worker が返した旧 snapshot と
    /// まだ取られていなかった前の snapshot を回収して **ここ (plugin-main) で** drop する。
    fn publish(&mut self, next: RegistryMap) {
        let metrics = self.metrics.as_ref().and_then(|m| m.plane.clone());
        let next = Arc::new(RegistrySnapshot { entries: next, metrics });
        for feed in &mut self.feeds {
            while let Ok(old) = feed.retired.pop() {
                drop(old);
            }
            drop(feed.mailbox.post(Arc::clone(&next)));
        }
        self.current = next;
    }

    /// 計測面が `capacity` 枠を持つよう (足りなければ 2 冪で) 作り直し、`entries` の instance を同じ枠番号で
    /// 載せ直す。**publish の前**に呼ぶ (新しい面は次の snapshot と一緒に worker へ届く)。面を作れなければ
    /// (shmem の作成失敗) 旧面のまま = 溢れた instance の CPU 表示が出ないだけ。
    fn ensure_metrics_capacity(&mut self, capacity: u32, entries: &RegistryMap) {
        let Some(m) = self.metrics.as_mut() else { return };
        let have = m.plane.as_ref().map_or(0, |p| p.capacity());
        if m.plane.is_some() && capacity <= have {
            return;
        }
        let cap = common::metrics_bridge::plugins::plugin_plane_capacity(capacity);
        // 名前の衝突 (死んだ同じ pid の host が作った面を GUI がまだ開いている — 世代はプロセスごとに 1 から
        // 数える) は次の世代で作り直して避ける。
        let mut last_error = None;
        for _ in 0..METRICS_PLANE_CREATE_ATTEMPTS {
            m.generation = m.generation.wrapping_add(1).max(1);
            let id = common::audio_bridge::plane_id(std::process::id(), m.generation);
            match PluginMetricsPlane::create(m.bridge.os_id(), id, cap) {
                Ok(next) => {
                    for (token, e) in entries {
                        next.assign(e.metric_slot, *token, m.plane.as_ref().map_or(0, |p| p.us(e.metric_slot)));
                    }
                    m.plane = Some(Arc::new(next));
                    return;
                }
                Err(e) => last_error = Some(e),
            }
        }
        tracing::error!(error = ?last_error, capacity, "plugin metrics plane の作成に失敗");
    }

    /// 計測面の id を GUI へ知らせる (publish の後 = worker が新しい面を採用できる状態になってから)。
    fn announce_metrics(&self) {
        if let Some(m) = &self.metrics {
            m.bridge.set_plugin_plane_id(m.plane.as_ref().map_or(0, |p| p.id()));
        }
    }

    /// `next` を正本にして publish する。計測面が `next` の枠を持たなければ作り直し (全 instance を載せ直す)、
    /// 今の正本に居ない instance (新規 / detach からの republish) を面の枠に載せてから publish する
    /// (worker は RT で枠を探さない)。作り直したら publish の後に GUI へ知らせる。
    fn publish_with_metrics(&mut self, next: RegistryMap) {
        let capacity = self.metrics.as_ref().and_then(|m| m.plane.as_ref()).map_or(0, |p| p.capacity());
        let need = next.values().map(|e| e.metric_slot.saturating_add(1)).max().unwrap_or(0);
        self.ensure_metrics_capacity(need, &next);
        if let Some(plane) = self.metrics.as_ref().and_then(|m| m.plane.as_ref()) {
            for (token, e) in next.iter().filter(|(token, _)| !self.current.contains_key(token)) {
                plane.assign(e.metric_slot, *token, plane.us(e.metric_slot));
            }
        }
        self.publish(next);
        if need > capacity {
            self.announce_metrics();
        }
    }
}

/// [`InstanceToken`] → [`PluginEntry`] の registry。正本と worker への送り口を持ち、**plugin-main
/// thread だけが触る** (worker は [`RegistryInbox`] で読む。module doc の「registry snapshot の受け渡し」)。
pub struct PluginRegistry {
    state: std::sync::Mutex<RegistryState>,
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self {
            state: std::sync::Mutex::new(RegistryState {
                current: Arc::new(RegistrySnapshot { entries: HashMap::new(), metrics: None }),
                feeds: Vec::new(),
                slots: PluginSlotAllocator::default(),
                assigned: HashMap::new(),
                metrics: None,
            }),
        }
    }
}

impl PluginRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        // plugin-main だけが取る lock なので poison は panic 中の再入だけ。中身は整合しているので使い続ける。
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 今の中身 (plugin-main / テスト用の読み取り)。
    pub fn snapshot(&self) -> Arc<RegistrySnapshot> {
        Arc::clone(&self.lock().current)
    }

    /// per-plugin 計測の固定 shmem (`metrics_shmem_id`) に繋ぎ、今の instance を全部載せた計測面を作って
    /// publish する (`WorkerPool::open`)。同じ shmem に繋ぎ直すときは既存の面を使い続ける。
    fn attach_metrics(&self, metrics_shmem_id: &str) -> Result<()> {
        let mut state = self.lock();
        if state.metrics.as_ref().is_none_or(|m| m.bridge.os_id() != metrics_shmem_id) {
            let bridge = MetricsBridgeHandle::open(metrics_shmem_id)?;
            state.metrics = Some(MetricsWriter { bridge, plane: None, generation: 0 });
        }
        let current = Arc::clone(&state.current);
        let need = current.values().map(|e| e.metric_slot.saturating_add(1)).max().unwrap_or(0);
        state.ensure_metrics_capacity(need, &current.entries);
        state.publish(current.entries.clone());
        state.announce_metrics();
        Ok(())
    }

    /// `n` 本の worker の受け口を作り、以前の送り口と置き換える (`WorkerPool::open`)。受け口は今の中身から始まる。
    fn attach_workers(&self, n: usize) -> Vec<RegistryInbox> {
        let mut state = self.lock();
        let current = Arc::clone(&state.current);
        let (feeds, inboxes) = (0..n)
            .map(|_| {
                let mailbox = Arc::new(SnapshotMailbox::new());
                let (tx, rx) = rtrb::RingBuffer::new(RETIRED_RING_CAP);
                (
                    RegistryFeed { mailbox: Arc::clone(&mailbox), retired: rx },
                    RegistryInbox { mailbox, retired: tx, current: Arc::clone(&current), stash: None },
                )
            })
            .unzip();
        state.feeds = feeds;
        inboxes
    }
}

/// Publish (insert or replace) one registry entry. `token` の計測枠をここで決め (初めてなら割り当て、
/// detach → republish は同じ枠)、計測面が足りなければ作り直してから publish する (worker は RT で枠を探さない)。
pub fn registry_insert(registry: &PluginRegistry, token: InstanceToken, mut entry: PluginEntry) {
    let mut state = registry.lock();
    entry.metric_slot = match state.assigned.get(&token) {
        Some(&slot) => slot,
        None => {
            let slot = state.slots.allocate();
            state.assigned.insert(token, slot);
            slot
        }
    };
    let mut next = state.current.entries.clone();
    next.insert(token, entry);
    state.publish_with_metrics(next);
}

/// Remove one registry entry, returning it if present. 計測枠は空けない (detach → republish で同じ枠を使う)。
/// instance を壊すときは quiesce の後に [`registry_release_metric_slot`] を呼ぶ。
pub fn registry_remove(registry: &PluginRegistry, token: InstanceToken) -> Option<PluginEntry> {
    let mut state = registry.lock();
    if !state.current.contains_key(&token) {
        return None;
    }
    let mut next = state.current.entries.clone();
    let removed = next.remove(&token);
    state.publish(next);
    removed
}

/// instance を壊した (registry から外して quiesce 済み) ときに `token` の計測枠を空ける。以後この枠は別の
/// instance が使う。枠を持っていなければ何もしない。
pub fn registry_release_metric_slot(registry: &PluginRegistry, token: InstanceToken) {
    let mut state = registry.lock();
    let Some(slot) = state.assigned.remove(&token) else { return };
    if let Some(plane) = state.metrics.as_ref().and_then(|m| m.plane.as_ref()) {
        plane.release(slot);
    }
    state.slots.release(slot);
}

/// Snapshot every entry and clear the registry (ReinitAllPlugins 用)。
pub fn registry_take_all(registry: &PluginRegistry) -> RegistryMap {
    let mut state = registry.lock();
    let all = state.current.entries.clone();
    state.publish(HashMap::new());
    all
}

/// Re-publish a set of entries at once (ReinitAllPlugins の republish)。
pub fn registry_restore_all(registry: &PluginRegistry, entries: RegistryMap) {
    registry.lock().publish_with_metrics(entries);
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
    /// worker ごとの、いま (最後に) dispatch-critical section に入った依頼の token。`enter` を上げる **前** に書くので、
    /// `exit < enter` (処理中) の間に読めばその処理中の依頼の token。
    current: [AtomicU64; MAX_WORKERS],
}

impl DispatchCounter {
    fn new() -> Self {
        Self {
            enter: [const { AtomicU64::new(0) }; MAX_WORKERS],
            exit: [const { AtomicU64::new(0) }; MAX_WORKERS],
            current: [const { AtomicU64::new(0) }; MAX_WORKERS],
        }
    }

    /// worker が `token` の依頼で dispatch-critical section に入る直前に呼ぶ。
    #[inline]
    fn enter(&self, idx: usize, token: InstanceToken) {
        self.current[idx].store(token.0, Ordering::SeqCst);
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

/// [`WorkerPool::quiesce`] が処理中の `process()` を待つ上限。audio 側は依頼を `DISPATCH_TIMEOUT_MS` で見切り、
/// 予備の pair も尽きた状態が `DISPATCH_TIMEOUT_MS * 4` 続けば plugin_host を立て直すので、それと同じ長さ。
const QUIESCE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(common::plugin_ref::DISPATCH_TIMEOUT_MS as u64 * 4);

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
    device: DeviceAddr,
    param_id: u32,
    value: f64,
}

impl Default for RtParamEvent {
    fn default() -> Self {
        Self {
            kind: RtParamKind::Touch,
            device: DeviceAddr::new(common::protocol::ProjectKey::NONE, 0),
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

/// 登録の無い token への依頼の数 (RT worker が数え、drain thread が集計してログに出す)。
///
/// plugin の再初期化中 (書き出しの準備 / panic) や削除の直後は、audio 側がまだその token を依頼してくる。
/// 1 worker は 1 buffer に何本もの plugin を順に処理するので、「同じ token が続く間は 1 回だけ」ではログが
/// 止まらず、TIME_CRITICAL の worker から大量に出ていた (実機で 1 回の書き出し準備に 309 行)。RT では数えるだけ。
#[derive(Default)]
struct MissingTokens {
    /// 書き手は worker 1 本だけ。
    count: AtomicU64,
    last: AtomicU64,
}

/// worker 1 本から非RT (drain thread) へ渡すもの。
#[derive(Default)]
struct WorkerOutbox {
    params: ParamEventRing,
    missing: MissingTokens,
}

impl Default for ParamEventRing {
    fn default() -> Self {
        Self::new(PARAM_RING_CAP)
    }
}

/// `RtParamEvent` を wire 用 [`PluginEvent`] へ変換 (drain thread = 非RT)。
fn rt_param_to_event(ev: RtParamEvent) -> PluginEvent {
    match ev.kind {
        RtParamKind::Touch => PluginEvent::PluginParamTouched {
            device: ev.device,
            param_id: ev.param_id,
            // display_name は daw_gui 側で plugin_params cache から解決する
            // (= host での文字列構築は placeholder のみ)。
            display_name: format!("Param {}", ev.param_id),
        },
        RtParamKind::Value => PluginEvent::PluginParamValueChanged {
            device: ev.device,
            param_id: ev.param_id,
            value: ev.value,
        },
        RtParamKind::Release => PluginEvent::PluginParamGestureEnd {
            device: ev.device,
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
///
/// 登録の無い token への依頼 ([`MissingTokens`]) も、ここで worker ごとに増えた分を 1 秒に 1 回まとめてログに出す。
fn run_param_drain(
    outboxes: Vec<Arc<WorkerOutbox>>,
    evt_tx: tokio::sync::mpsc::UnboundedSender<PluginEvent>,
    drain_quit: Arc<AtomicBool>,
) {
    const PARAM_DRAIN_MIN_MS: u64 = 2;
    const PARAM_DRAIN_MAX_MS: u64 = 32;
    const MISSING_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
    let mut idle_sleep_ms = PARAM_DRAIN_MIN_MS;
    let mut reported = vec![0u64; outboxes.len()];
    let mut last_report = std::time::Instant::now();
    let report_missing = |reported: &mut [u64]| {
        for (i, (out, seen)) in outboxes.iter().zip(reported.iter_mut()).enumerate() {
            let count = out.missing.count.load(Ordering::Relaxed);
            if count != *seen {
                tracing::warn!(
                    worker_idx = i,
                    dispatches = count.wrapping_sub(*seen),
                    last_token = out.missing.last.load(Ordering::Relaxed),
                    "dispatches to tokens with no registered plugin (reinitialising / removed); skipped"
                );
                *seen = count;
            }
        }
    };
    loop {
        let mut any = false;
        for out in &outboxes {
            while let Some(ev) = out.params.pop() {
                any = true;
                let _ = evt_tx.send(rt_param_to_event(ev));
            }
        }
        if last_report.elapsed() >= MISSING_REPORT_INTERVAL {
            report_missing(&mut reported);
            last_report = std::time::Instant::now();
        }
        if drain_quit.load(Ordering::Acquire) {
            // `drain_quit` は teardown が worker を join (止まらないものは手放し) した後に立てるので、
            // join した worker の push はここで拾い切る。
            for out in &outboxes {
                while let Some(ev) = out.params.pop() {
                    let _ = evt_tx.send(rt_param_to_event(ev));
                }
            }
            report_missing(&mut reported);
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
/// `process()` が panic しても `exit` / 完了の書き込みが
/// 必ず実行され、 quiesce の永久 wait と audio 側の timeout を防ぐ。
struct DispatchGuard<'a> {
    dispatch: &'a DispatchCounter,
    channel: &'a WorkerChannel,
    idx: usize,
    request: u64,
    done: SendableHandle,
}

impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        // dispatch-critical section を閉じる。 ここから先 audio half に
        // 触れないので、 plugin-main が entry を drop しても safe。
        self.dispatch.exit(self.idx);
        self.channel.complete(self.request, self.done.0);
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
    /// RT → 非RT に運ぶ per-worker の受け渡し (plugin GUI 発の param event の SPSC ring と、登録の無い token の数)。
    outboxes: Vec<Arc<WorkerOutbox>>,
    /// outbox を poll して `evt_tx` へ流す非RT thread。
    drain_thread: Option<JoinHandle<()>>,
}

impl WorkerPool {
    pub fn open(
        spec: &WorkerPoolSpec,
        metrics_shmem_id: &str,
        registry: &PluginRegistry,
        evt_tx: tokio::sync::mpsc::UnboundedSender<PluginEvent>,
    ) -> Result<Self> {
        spec.validate()?;
        // host の worker は pair ごとに 1 本 (audio 側の runner の数 + 予備)。
        let (n_workers, generation) = (spec.n_pairs, spec.generation);

        let bridge = Arc::new(WorkerBridgeHandle::open(&spec.worker_bridge_shmem_id)?);
        // resource monitor: per-plugin の process() 時間を載せる計測面に繋ぐ (worker は registry の
        // snapshot 経由で面を受け取る)。
        registry.attach_metrics(metrics_shmem_id)?;
        // 途中で失敗したら `Drop` がそこまでに起こした worker を止める (手放すと、どの pool にも数えられない worker が
        // 依頼を受け続け、quiesce から見えない dispatch になる)。
        let mut pool = Self {
            workers: Vec::with_capacity(n_workers as usize),
            shutdown: Arc::new(AtomicBool::new(false)),
            drain_quit: Arc::new(AtomicBool::new(false)),
            wake_events: Vec::with_capacity(n_workers as usize),
            dispatch: Arc::new(DispatchCounter::new()),
            n_workers,
            outboxes: Vec::with_capacity(n_workers as usize),
            drain_thread: None,
        };
        // worker ごとの registry の受け口 (以前の pool の送り口はここで置き換わる)。
        let inboxes = registry.attach_workers(n_workers as usize);

        for (i, inbox) in inboxes.into_iter().enumerate() {
            let wake = open_named_event(&spec.wake_event_names[i])?;
            let done = open_named_event(&spec.done_event_names[i])?;
            pool.wake_events.push(wake);

            let bridge_w = Arc::clone(&bridge);
            let shutdown_w = Arc::clone(&pool.shutdown);
            let dispatch_w = Arc::clone(&pool.dispatch);
            let idx = i as u32;
            let wake_s = SendableHandle(wake);
            let done_s = SendableHandle(done);
            let outbox = Arc::new(WorkerOutbox::default());
            pool.outboxes.push(Arc::clone(&outbox));
            let handle = std::thread::Builder::new()
                .name(format!("plugin-worker-{i}"))
                .spawn(move || {
                    run_worker(idx, generation, bridge_w, shutdown_w, inbox, dispatch_w, wake_s, done_s, outbox)
                })?;
            pool.workers.push(handle);
        }

        // drain thread: RT worker が outbox に書いたものを非RT で `evt_tx` / ログへ流す。
        let drain_outboxes: Vec<Arc<WorkerOutbox>> = pool.outboxes.iter().map(Arc::clone).collect();
        let drain_quit_w = Arc::clone(&pool.drain_quit);
        pool.drain_thread = Some(
            std::thread::Builder::new()
                .name("plugin-param-drain".into())
                .spawn(move || run_param_drain(drain_outboxes, evt_tx, drain_quit_w))?,
        );

        tracing::info!(n_workers, "plugin worker pool started");
        Ok(pool)
    }

    /// `tokens` の plugin の `process()` を処理中の worker が抜けるまで待つ (上限 [`QUIESCE_TIMEOUT`])。
    /// **plugin-main thread からのみ呼ぶ — RT thread からは呼ばない。**
    ///
    /// 呼び出し側は drop / mutate 予定の device 全てについて、 この method
    /// を呼ぶ **前に** registry から entry を外しておく必要がある
    /// ([`registry_remove`])。 空の戻り値で return した時点で、 `tokens` の audio half に触れている worker は
    /// 存在しない (処理中の依頼が別の plugin の worker は、旧 snapshot を持っていても外した plugin には触れないので
    /// 待たない)。
    ///
    /// 戻り値 = 上限までに抜けなかった (その plugin の `process()` の中で固まっている) token。呼び出し側はその
    /// plugin に触ってはならない — 以前は無期限に待ったので、固まった plugin を 1 つ外そうとするだけで plugin-main
    /// が止まったままになった (audio 側は予備の pair に借り替えて動き続けるので、この状態が普通に起こる)。
    pub fn quiesce(&self, tokens: &[InstanceToken]) -> Vec<InstanceToken> {
        let d = &self.dispatch;
        let n = self.n_workers as usize;
        let snaps: Vec<u64> = (0..n).map(|i| d.enter[i].load(Ordering::SeqCst)).collect();
        let deadline = std::time::Instant::now() + QUIESCE_TIMEOUT;
        let mut stuck = Vec::new();
        for (i, &snap) in snaps.iter().enumerate() {
            loop {
                // `current` を先に読む: その後で `exit < snap` なら、読んだ値は snapshot 時点で処理中だった依頼の
                // token (worker は `exit` を上げてから次の依頼の `current` を書く)。
                let current = InstanceToken(d.current[i].load(Ordering::SeqCst));
                if d.exit[i].load(Ordering::SeqCst) >= snap || !tokens.contains(&current) {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    if !stuck.contains(&current) {
                        stuck.push(current);
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
        stuck
    }

    /// 全 worker と drain thread を止める。戻り値は [`Self::teardown`]。
    pub fn shutdown(mut self) -> Vec<InstanceToken> {
        self.teardown()
    }

    /// 全 worker と drain thread を停止・join する (冪等)。
    ///
    /// 戻り値 = [`QUIESCE_TIMEOUT`] までに止まらなかった worker が処理中の plugin の token。その worker は join せずに
    /// 手放す (plugin の `process()` から戻らない thread を待つと plugin-main が止まったままになる) ので、呼び出し側は
    /// その plugin を壊してはならず、別の pool で使ってもならない (まだ `process()` の中にいる)。
    fn teardown(&mut self) -> Vec<InstanceToken> {
        if self.drain_thread.is_none() && self.workers.is_empty() {
            return Vec::new();
        }
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake every worker so it sees the flag and exits its loop.
        for &wake in &self.wake_events {
            unsafe {
                let _ = SetEvent(wake);
            }
        }
        let deadline = std::time::Instant::now() + QUIESCE_TIMEOUT;
        while self.workers.iter().any(|h| !h.is_finished()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let d = &self.dispatch;
        let mut wedged = Vec::new();
        for (i, h) in self.workers.drain(..).enumerate() {
            if h.is_finished() {
                if h.join().is_err() {
                    tracing::error!("plugin worker thread panicked");
                }
                continue;
            }
            // quiesce と同じ読み順: `exit < enter` なら、その間に読んだ `current` は処理中の依頼の token。
            let enter = d.enter[i].load(Ordering::SeqCst);
            let token = InstanceToken(d.current[i].load(Ordering::SeqCst));
            let in_process = d.exit[i].load(Ordering::SeqCst) < enter;
            tracing::error!(worker_idx = i, in_process, ?token, "plugin worker did not stop in time; detached");
            if in_process {
                wedged.push(token);
            }
        }
        // join した worker はもう ring へ push しない (手放した worker が後で push した分は捨てられる)。
        self.drain_quit.store(true, Ordering::Release);
        if let Some(d) = self.drain_thread.take()
            && d.join().is_err()
        {
            tracing::error!("plugin param drain thread panicked");
        }
        tracing::info!("plugin worker pool stopped");
        wedged
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        // 正常系は `shutdown()` 済で no-op。 `open` の途中失敗 / panic unwind 等の異常系のみ
        // ここで停止・join する。
        self.teardown();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    idx: u32,
    generation: u32,
    bridge: Arc<WorkerBridgeHandle>,
    shutdown: Arc<AtomicBool>,
    mut registry: RegistryInbox,
    dispatch: Arc<DispatchCounter>,
    wake: SendableHandle,
    done: SendableHandle,
    outbox: Arc<WorkerOutbox>,
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
    // **各物理コアの 1 本目の論理 CPU の集合** に制限する (`common::cpu_topology` の module doc に
    // 実測)。worker 同士が SMT sibling に乗り合わせると同じ `process()` が伸びて「CPU 50% で xrun」に
    // なる。1 本ずつ特定の CPU に固定はしない — worker がコアより多いと、空いたコアに移れない。
    // `DAW_PIN_WORKERS=0` で制限を外せる (A/B と、制限が合わない環境の逃げ道)。
    if std::env::var("DAW_PIN_WORKERS").as_deref() != Ok("0") {
        let mask = common::cpu_topology::one_logical_per_core_mask();
        let ok = common::cpu_topology::restrict_current_thread_to(mask);
        tracing::info!(worker_idx = idx, mask = format!("{mask:#x}"), ok, "plugin worker restricted to one logical CPU per core");
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
    let Some(channel) = bridge.bridge().channels.get(idx as usize) else { return };
    // 直前に終えた依頼と、次の依頼を回って待つ時間 (直前の buffer 周期から)。
    let mut last = channel.host_start(generation);
    let mut spin = std::time::Duration::ZERO;

    // 次の依頼を待つ (`None` = shutdown)。
    while let Some((request, token)) = channel.wait_request(generation, last, wake.0, spin, &shutdown) {
        last = request;

        // dispatch-critical section を、 観測可能な操作の **前** に開く
        // (happens-before の論証は module docs)。
        dispatch.enter(idx as usize, token);

        // `enter` の後に採用する (quiesce の論証、module doc)。旧 snapshot は plugin-main へ返る。
        let snapshot = registry.adopt_latest();
        let entry_opt = snapshot.get(&token);
        let Some(entry) = entry_opt else {
            // RT ではログを出さず数えるだけ (drain thread がまとめて出す — `MissingTokens`)。
            let missing = &outbox.missing;
            missing.count.store(missing.count.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
            missing.last.store(token.0, Ordering::Relaxed);
            dispatch.exit(idx as usize);
            channel.complete(request, done.0);
            continue;
        };
        let device = entry.device;

        // teardown (exit / 完了の書き込み) を `Drop` に集約。
        let _guard = DispatchGuard {
            dispatch: &dispatch,
            channel,
            idx: idx as usize,
            request,
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
        spin = common::worker_bridge::spin_budget(frames, pd.sample_rate);

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
                    param_events_in.push(crate::plugin_instance::TimedParamEvent::global(
                        ev.time,
                        ev.param_id,
                        ev.value,
                        crate::plugin_instance::ParamEventKind::Value,
                    ));
                }
                // 出力側専用 (plugin → engine)。 入力に混ざっていても無視。
                EventKind::NoteEnd => {}
            }
        }
        // r.md #89: lane 非依存モジュレーションは `events_in` ではなく専用配列で
        // 届く (`docs/plan_rmd_88_89_cross_modulation.md` §2.2)。制御グリッドが
        // 64 サンプル刻みになるとノート枠を押し出すので枠を分けてある。
        // r.md #117: ノート宛 (`note_id >= 0`) はそのまま運ぶ (per-format の扱いは backend)。
        for m in pd.param_mods_iter() {
            param_events_in.push(crate::plugin_instance::TimedParamEvent {
                time: m.time,
                param_id: m.param_id,
                value: m.value,
                kind: crate::plugin_instance::ParamEventKind::Mod,
                note_id: m.note_id,
            });
        }
        events_in.sort_unstable_by_key(|e| e.time);
        // r.md #89: 同時刻は **Value を先に**。Mod は「その時刻の automation 値」を
        // base に畳むので、同時刻の Value より先に来ると 1 刻み古い base で畳まれる。
        param_events_in.sort_unstable_by_key(|e| {
            (e.time, u8::from(e.kind == crate::plugin_instance::ParamEventKind::Mod))
        });

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
        pd.aux_in_active.fill(0);
        // パラアウト: clear aux_out_active up front (失敗 buffer の stale
        // flag 防止)。
        pd.aux_out_active.fill(0);

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
                    tracing::error!(error = ?e, ?device, "plugin.process() failed (suppressing repeats)");
                }
                false
            }
            Err(_panic) => {
                if !entry.err_logged.swap(true, Ordering::Relaxed) {
                    tracing::error!(
                        ?device,
                        "plugin.process() panicked; worker survived, buffer skipped (suppressing repeats)"
                    );
                }
                false
            }
        };
        // resource monitor: per-plugin の process() 時間 (μs) を、plugin-main が registry に入れるときに
        // 割り当てた枠へ store する (同じ snapshot の計測面 = entry と面は必ず同じ世代の組)。
        let proc_us = u32::try_from(proc_start.elapsed().as_micros()).unwrap_or(u32::MAX);
        if let Some(plane) = snapshot.metrics.as_deref() {
            plane.set_us(entry.metric_slot, proc_us);
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
                    outbox.params.push(RtParamEvent {
                        kind: RtParamKind::Touch,
                        device,
                        param_id,
                        value: 0.0,
                    });
                }
                for (param_id, value) in out_param_values.drain(..) {
                    outbox.params.push(RtParamEvent {
                        kind: RtParamKind::Value,
                        device,
                        param_id,
                        value,
                    });
                }
                for param_id in out_param_releases.drain(..) {
                    outbox.params.push(RtParamEvent {
                        kind: RtParamKind::Release,
                        device,
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
                    // r.md #117: ボイス終了の通知 (engine の per-note ボイス表が読む)。
                    NoteTransition::End { note_id, key } => Event {
                        kind: EventKind::NoteEnd,
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
        let addr = DeviceAddr::new(common::protocol::ProjectKey(3), 0xDEAD_BEEF_0001);
        let ev = RtParamEvent {
            kind: RtParamKind::Value,
            device: addr,
            param_id: 7,
            value: 0.25,
        };
        match rt_param_to_event(ev) {
            PluginEvent::PluginParamValueChanged { device, param_id, value } => {
                assert_eq!(device, addr);
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
        let pool = pool_over(&dispatch, 4);
        let start = Instant::now();
        assert!(pool.quiesce(&[InstanceToken(1)]).is_empty());
        assert!(start.elapsed() < Duration::from_millis(5));
    }

    /// `quiesce` は in-flight な worker が `exit` を bump するまで return
    /// しない。 UAF guard の中核を verify する test。
    #[test]
    fn quiesce_waits_for_inflight_dispatch() {
        let dispatch = Arc::new(DispatchCounter::new());
        // slot 2 で token 7 を in-flight な状態を作る。
        dispatch.enter(2, InstanceToken(7));
        let pool = pool_over(&dispatch, 4);

        let dispatch_for_releaser = Arc::clone(&dispatch);
        let release_after = Duration::from_millis(50);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(release_after);
            dispatch_for_releaser.exit(2);
        });

        let start = Instant::now();
        assert!(pool.quiesce(&[InstanceToken(7)]).is_empty());
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
        let pool = pool_over(&dispatch, 2);

        // background thread で enter/exit pair を高速に回す。
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = Arc::clone(&stop);
        let dispatch_w = Arc::clone(&dispatch);
        let busy = std::thread::spawn(move || {
            while !stop_w.load(Ordering::Relaxed) {
                dispatch_w.enter(0, InstanceToken(3));
                dispatch_w.exit(0);
                dispatch_w.enter(1, InstanceToken(3));
                dispatch_w.exit(1);
            }
        });

        let start = Instant::now();
        pool.quiesce(&[InstanceToken(3)]);
        let elapsed = start.elapsed();
        stop.store(true, Ordering::Relaxed);
        busy.join().unwrap();

        assert!(
            elapsed < Duration::from_millis(50),
            "quiesce が継続中の dispatch に starve された (elapsed={elapsed:?})"
        );
    }

    /// 別の plugin の `process()` で固まっている worker は、外す plugin に触れないので待たない。
    #[test]
    fn quiesce_does_not_wait_for_other_plugins() {
        let dispatch = Arc::new(DispatchCounter::new());
        dispatch.enter(1, InstanceToken(9));
        let pool = pool_over(&dispatch, 2);

        let start = Instant::now();
        assert!(pool.quiesce(&[InstanceToken(4)]).is_empty());
        assert!(start.elapsed() < Duration::from_millis(5));
    }

    /// 外す plugin の `process()` から上限までに抜けなければ、その token を返して戻る (plugin-main を止めない)。
    #[test]
    fn quiesce_reports_plugins_stuck_past_the_limit() {
        let dispatch = Arc::new(DispatchCounter::new());
        dispatch.enter(0, InstanceToken(5));
        dispatch.enter(1, InstanceToken(6));
        let pool = pool_over(&dispatch, 2);

        let start = Instant::now();
        let stuck = pool.quiesce(&[InstanceToken(5), InstanceToken(6)]);
        let elapsed = start.elapsed();

        assert_eq!(stuck, vec![InstanceToken(5), InstanceToken(6)]);
        assert!(elapsed >= QUIESCE_TIMEOUT, "上限より前に見切った ({elapsed:?})");
        assert!(elapsed < QUIESCE_TIMEOUT * 2, "worker ごとに上限を数え直している ({elapsed:?})");
    }

    /// plugin の `process()` から戻らない worker を待ち続けず、手放してその plugin の token を返す。
    #[test]
    fn shutdown_detaches_workers_stuck_in_process() {
        let dispatch = Arc::new(DispatchCounter::new());
        let mut pool = pool_over(&dispatch, 2);
        let (release, stuck_until) = std::sync::mpsc::channel::<()>();
        let d = Arc::clone(&dispatch);
        pool.workers.push(std::thread::spawn(move || {
            d.enter(0, InstanceToken(8));
            let _ = stuck_until.recv();
            d.exit(0);
        }));
        let d = Arc::clone(&dispatch);
        pool.workers.push(std::thread::spawn(move || {
            d.enter(1, InstanceToken(9));
            d.exit(1);
        }));
        while dispatch.enter[0].load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }

        let start = Instant::now();
        let wedged = pool.shutdown();
        let elapsed = start.elapsed();
        let _ = release.send(());

        assert_eq!(wedged, vec![InstanceToken(8)], "処理を終えた worker の token は返さない");
        assert!(elapsed < QUIESCE_TIMEOUT * 2, "止まらない worker を待ち続けた ({elapsed:?})");
    }

    fn pool_over(dispatch: &Arc<DispatchCounter>, n_workers: u32) -> WorkerPool {
        WorkerPool {
            workers: Vec::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            drain_quit: Arc::new(AtomicBool::new(false)),
            wake_events: Vec::new(),
            dispatch: Arc::clone(dispatch),
            n_workers,
            outboxes: Vec::new(),
            drain_thread: None,
        }
    }

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

    fn null_entry(device_id: u64) -> PluginEntry {
        PluginEntry {
            device: DeviceAddr::new(common::protocol::ProjectKey(1), device_id),
            audio: AudioHalf::new(Box::new(NullHalf)),
            process_data: std::ptr::null_mut(),
            err_logged: Arc::new(AtomicBool::new(false)),
            metric_slot: METRIC_SLOT_UNASSIGNED,
            transport_pinned_to_song: false,
        }
    }

    /// `docs/plan_unbounded_tracks.md` §4: 旧実装の枠 (512) を超える 600 instance が全部計測枠を持ち、
    /// 読み手は面の id を開き直して token で引ける。worker は snapshot の面へ store するだけ。
    /// detach → republish は同じ枠、壊した instance の枠は空いて再利用される。
    #[test]
    fn 容量を超える_instance_も計測枠を持ち壊すと枠が空く() {
        let name = format!("daw01_test_ps_metrics_{}", std::process::id());
        let gui = MetricsBridgeHandle::create(&name).expect("metrics bridge");
        let registry = PluginRegistry::default();
        let mut inbox = registry.attach_workers(1).pop().expect("inbox");
        registry.attach_metrics(&name).expect("attach");
        for id in 1..=600u64 {
            registry_insert(&registry, InstanceToken(id), null_entry(id));
        }
        // worker 相当: 採用した snapshot の面へ、その entry の枠で store する。
        let snap = inbox.adopt_latest();
        let plane = snap.metrics.as_deref().expect("計測面");
        for (token, e) in snap.iter() {
            plane.set_us(e.metric_slot, u32::try_from(token.0).unwrap());
        }
        let reader = PluginMetricsPlane::open(gui.os_id(), gui.plugin_plane_id()).expect("GUI が面を開ける");
        let mut out = Vec::new();
        reader.read(&mut out);
        assert_eq!(out.len(), 600, "600 個とも枠を持つ");
        assert!(out.contains(&(InstanceToken(600), 600)));

        let slot_of = |t: u64| registry.snapshot().get(&InstanceToken(t)).expect("entry").metric_slot;
        let slot_7 = slot_of(7);
        let detached = registry_remove(&registry, InstanceToken(7)).expect("detach");
        registry_insert(&registry, InstanceToken(7), detached);
        assert_eq!(slot_of(7), slot_7, "detach → republish は同じ枠");

        assert!(registry_remove(&registry, InstanceToken(7)).is_some(), "teardown");
        registry_release_metric_slot(&registry, InstanceToken(7));
        reader.read(&mut out);
        assert!(!out.iter().any(|(t, _)| *t == InstanceToken(7)), "壊した instance は読まれない");
        registry_insert(&registry, InstanceToken(601), null_entry(601));
        assert_eq!(slot_of(601), slot_7, "空いた枠を再利用する");
    }

    /// registry の insert / remove / take_all round-trip (token keyed)。
    #[test]
    fn registry_insert_remove_roundtrip() {
        let registry = PluginRegistry::default();
        let t42 = InstanceToken(42);
        registry_insert(&registry, t42, null_entry(42));
        assert!(registry.snapshot().contains_key(&t42));
        let removed = registry_remove(&registry, t42);
        assert!(removed.is_some());
        assert!(registry.snapshot().is_empty());
        assert!(registry_remove(&registry, t42).is_none());

        // take_all + restore_all round-trip。
        registry_insert(&registry, InstanceToken(7), null_entry(7));
        let all = registry_take_all(&registry);
        assert!(registry.snapshot().is_empty());
        assert_eq!(all.len(), 1);
        registry_restore_all(&registry, all);
        assert!(registry.snapshot().contains_key(&InstanceToken(7)));
    }

    /// worker (RT) は registry の変更を dispatch の頭で採用し、**差し替えた旧 snapshot を自分で解放しない**
    /// (plugin-main が次の変更の前に回収して drop する)。外した plugin の audio half の最終参照が worker に
    /// 残らないことを strong count で確かめる。外した後に採用した snapshot にはその entry が居ない (quiesce の前提)。
    #[test]
    fn worker_adopts_the_latest_snapshot_and_hands_the_old_one_back_to_plugin_main() {
        let registry = PluginRegistry::default();
        let mut inbox = registry.attach_workers(1).pop().expect("inbox");
        let t1 = InstanceToken(1);
        let entry = null_entry(1);
        let audio = Arc::clone(&entry.audio);
        registry_insert(&registry, t1, entry);
        assert!(inbox.adopt_latest().contains_key(&t1), "置いた snapshot を採用する");

        let removed = registry_remove(&registry, t1).expect("外す");
        drop(removed);
        assert_eq!(Arc::strong_count(&audio), 2, "前提: テスト + worker が採用中の旧 snapshot");
        assert!(!inbox.adopt_latest().contains_key(&t1), "外した後に採用した snapshot には居ない");
        assert_eq!(
            Arc::strong_count(&audio),
            2,
            "採用で差し替えた旧 snapshot は worker 上で drop されず、plugin-main への ring に居る"
        );
        registry_insert(&registry, InstanceToken(2), null_entry(2));
        assert_eq!(Arc::strong_count(&audio), 1, "plugin-main が次の変更の前に回収して drop した");

        // worker が採用しないうちに何度変更しても、受け口に残るのは最新だけ (前の値は plugin-main が drop)。
        for id in 3..40 {
            registry_insert(&registry, InstanceToken(id), null_entry(id));
        }
        assert_eq!(inbox.adopt_latest().len(), 38, "最新の中身を採用する");
        assert!(inbox.stash.is_none(), "ring は溢れない");
    }
}
