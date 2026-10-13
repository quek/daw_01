//! stall で呼び側が先に戻った後も文脈を持って走る runner (生き残り) の検査。
//!
//! plugin host の代わりに pair ごとの偽の host thread を立て、依頼を受けたら寝てから完了を返す。生き残りは
//! worker の thread を OS の suspend で止めて作る (debugger の停止 / ページング / OS の suspend の再現 — plugin の
//! dispatch は 1 回ずつ期限で切れるので、生きた runner が進みを止めるのはこの形だけ)。

use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::audio_bridge::AudioBridgeHandle;
use common::device_scope_bridge::{DeviceScopeBridgeHandle, MAX_DEVICE_SCOPES};
use common::model::{Device, PluginInstance, Song, Track};
use common::plugin_ref::{WorkerSyncRef, create_named_event};
use common::protocol::ProjectKey;
use common::song_index::SongIndex;
use common::timing::SongBounds;
use common::worker_bridge::{WorkerBridgeHandle, WorkerChannel};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{ResumeThread, SetEvent, SuspendThread};

use super::AudioWorkerPool;
use crate::audio_clip_renderer::AudioClipRenderer;
use crate::engine::{
    DeviceBundle, DeviceRt, EngineCommand, EngineCommandSender, EngineShared, PlaybackCommand, PluginEntry, PluginRefs, ProjectDelivery,
    ProjectRt, ProjectShared, RecordingLanes, RtBundle, SharedState, SyncSlot, WorkerRig,
};

/// pool の世代 (依頼番号に焼き込む)。
const GEN: u32 = 1;
/// 生き残りを止めておく時間。master が諦めるのは進みが最後に変わってから `POOL_WAIT_TIMEOUT_MS` (1 s) を 1〜2 回
/// 待った後なので、その 2 倍より長く、窓の間の検査が終わるまで生き残りが動き出さない長さ。
const SUSPEND: Duration = Duration::from_millis(2_600);
/// [`FakeHost::hold`] で止めたままにする上限 (検査が途中で落ちても host の thread を join できるように)。
const HOLD_MAX: Duration = Duration::from_secs(30);
const SR: u32 = 48_000;
const FRAMES: usize = 256;

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

struct SendPtr(*const WorkerChannel);
unsafe impl Send for SendPtr {}

/// plugin host の worker の代わり (pair ごとに 1 本)。実物の plugin host と同じく、受け渡しの shmem は自分で open
/// した mapping で読む (rig の mapping は rig と一緒に解放される)。
struct FakeHost {
    /// host 側の mapping。thread を join してから落とす (`Drop` の後に field が落ちる)。
    _bridge: WorkerBridgeHandle,
    shutdown: Arc<AtomicBool>,
    wakes: Vec<usize>,
    threads: Vec<JoinHandle<()>>,
    /// 終えた依頼の数 (index = pair)。
    calls: Arc<[AtomicU64; 2]>,
    /// 立っている間、worker (pair 1) の次の依頼で audio worker の thread を [`SUSPEND`] だけ止める。依頼は止めている
    /// 間に終えるので、起きた worker は完了を見て手を続ける (期限切れにも quarantine にもならない)。
    suspend_next: Arc<AtomicBool>,
    /// 立っている間、[`Self::suspend_next`] で止めた worker を [`SUSPEND`] で動かさず、下ろされるまで止めたままにする
    /// (呼び側が窓の閉じるのを諦めて戻った後に生き残りを動かす形を、時間の揺れに依らずに作る)。
    hold: Arc<AtomicBool>,
}

impl FakeHost {
    /// pair `i` で終えた依頼の数。
    fn calls_on(&self, i: usize) -> u64 {
        self.calls[i].load(Ordering::SeqCst)
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.hold.store(false, Ordering::SeqCst);
        for &w in &self.wakes {
            unsafe {
                let _ = SetEvent(HANDLE(w as *mut _));
            }
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for FakeHost {
    fn drop(&mut self) {
        self.stop();
    }
}

/// runner 2 本 (callback 役 + worker 1 本) の pool と、pair ごとに `delays[i]` 寝てから返す偽の host。
///
/// worker が寝付くのを待ってから返し、callback 役の pair (`delays[0]`) を遅めにするのは、worker が起きてもう 1 本の
/// leaf を取る間を作るため — でないと負荷の下で callback 役が両方の leaf を描き切り、worker が何もしないまま検査が
/// 素通りする (cargo-mutants の IDLE の下で実際に起きた)。
fn pooled_rig(tag: &str, delays: [Duration; 2]) -> (Arc<WorkerRig>, FakeHost) {
    let pid = std::process::id();
    let bridge_name = format!("daw01_test_stall_wb_{tag}_{pid}");
    let bridge = WorkerBridgeHandle::create(&bridge_name).unwrap();
    let host_bridge = WorkerBridgeHandle::open(&bridge_name).unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let calls = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let suspend_next = Arc::new(AtomicBool::new(false));
    let hold = Arc::new(AtomicBool::new(false));
    let worker_thread = Arc::new(AtomicUsize::new(0));
    let mut slots = Vec::new();
    let mut threads = Vec::new();
    let mut wakes = Vec::new();
    for (i, &delay) in delays.iter().enumerate() {
        let (wake_name, done_name) = (format!("daw01_test_stall_{tag}_{pid}_w{i}"), format!("daw01_test_stall_{tag}_{pid}_d{i}"));
        let channel = &bridge.bridge().channels[i] as *const WorkerChannel;
        slots.push(SyncSlot {
            sync: WorkerSyncRef {
                worker_idx: i as u32,
                channel,
                generation: GEN,
                event_wake: create_named_event(&wake_name).unwrap(),
                event_done: create_named_event(&done_name).unwrap(),
            },
            poisoned: AtomicBool::new(false),
        });
        let wake = create_named_event(&wake_name).unwrap().0 as usize;
        let done = create_named_event(&done_name).unwrap().0 as usize;
        wakes.push(wake);
        let channel = SendPtr(&host_bridge.bridge().channels[i] as *const WorkerChannel);
        let (shutdown, calls, suspend_next, hold, worker_thread) = (
            Arc::clone(&shutdown),
            Arc::clone(&calls),
            Arc::clone(&suspend_next),
            Arc::clone(&hold),
            Arc::clone(&worker_thread),
        );
        threads.push(std::thread::spawn(move || {
            let channel = channel;
            let ch = unsafe { &*channel.0 };
            let (wake, done) = (HANDLE(wake as *mut _), HANDLE(done as *mut _));
            let mut last = ch.host_start(GEN);
            while let Some((req, _token)) = ch.wait_request(GEN, last, wake, Duration::ZERO, &shutdown) {
                last = req;
                let worker = HANDLE(worker_thread.load(Ordering::SeqCst) as *mut _);
                let suspended =
                    i == 1 && suspend_next.swap(false, Ordering::SeqCst) && unsafe { SuspendThread(worker) } != u32::MAX;
                if suspended && hold.load(Ordering::SeqCst) {
                    let until = Instant::now() + HOLD_MAX;
                    while hold.load(Ordering::SeqCst) && Instant::now() < until {
                        std::thread::sleep(ms(1));
                    }
                } else {
                    std::thread::sleep(if suspended { SUSPEND } else { delay });
                }
                calls[i].fetch_add(1, Ordering::SeqCst);
                ch.complete(req, done);
                if suspended {
                    unsafe {
                        ResumeThread(worker);
                    }
                }
            }
        }));
    }
    let pool = AudioWorkerPool::new(2).unwrap();
    worker_thread.store(pool.workers[0].as_raw_handle() as usize, Ordering::SeqCst);
    // worker が起動して寝る (idle の bit を立てる) まで待つ — まだ寝ていない worker は buffer の頭で起こされない。
    let until = Instant::now() + ms(10_000);
    while pool.shared.idle.load(Ordering::SeqCst) & (1 << 1) == 0 {
        assert!(Instant::now() < until, "worker が起動しない");
        std::thread::sleep(ms(1));
    }
    let rig = Arc::new(WorkerRig::new(Some(pool), slots, 2, bridge));
    (rig, FakeHost { _bridge: host_bridge, shutdown, wakes, threads, calls, suspend_next, hold })
}

fn pool(rig: &WorkerRig) -> &AudioWorkerPool {
    rig.pool.as_ref().expect("pool")
}

/// 文脈を持ったまま走っている runner が居る。
fn window_open(rig: &WorkerRig) -> bool {
    pool(rig).window_open()
}

/// 窓が閉じるまで待つ (有界)。
fn wait_window_closed(rig: &WorkerRig) {
    let until = Instant::now() + ms(10_000);
    while window_open(rig) {
        assert!(Instant::now() < until, "生き残りが戻らない");
        std::thread::sleep(ms(5));
    }
}

fn fx(id: u64) -> Device {
    Device::Plugin(PluginInstance {
        id,
        ..PluginInstance::with_ports(
            format!("test.fx{id}"),
            common::plugin_format::PluginFormat::Clap,
            common::port_config::PortConfig { has_audio_input: true, has_audio_output: true, ..Default::default() },
        )
    })
}

/// group (track 3) の子 2 本、それぞれ plugin `per_track` 個の chain (id は 1 から通し)。子は group の子にする —
/// master バスを書く job は callback スレッド専用 (`RenderGraph` の `on_master`) なので、子が master へ直接合流すると
/// worker は job を取らない。
fn chain_song(per_track: u64) -> Song {
    let mut song = Song::default();
    for t in 0..2u64 {
        song.tracks.push(Track {
            id: t as u32 + 1,
            parent_group_id: Some(3),
            devices: (1..=per_track).map(|k| fx(t * per_track + k)).collect(),
            ..Track::default()
        });
    }
    song.tracks.push(Track { id: 3, ..Track::default() });
    song.enforce_edit_invariants();
    song
}

/// heap の `ProcessData` を指す plugin entry の表 (`_pds` が entry より長く生きる)。
struct Plugins {
    _pds: Vec<Box<common::process_data::ProcessData>>,
    refs: Arc<PluginRefs>,
}

fn plugins(ids: impl Iterator<Item = u64>) -> Plugins {
    let mut pds = Vec::new();
    let mut refs = PluginRefs::new();
    for id in ids {
        let mut pd = Box::new(common::process_data::ProcessData::empty());
        refs.insert(id, Arc::new(PluginEntry::for_test(id, &raw mut *pd)));
        pds.push(pd);
    }
    Plugins { _pds: pds, refs: Arc::new(refs) }
}

fn quarantined(p: &Plugins) -> usize {
    p.refs.values().filter(|e| e.quarantined.load(Ordering::Acquire)).count()
}

/// CPAL callback の側 (`DeviceRt`) と、off-thread 側の ring の片割れ。
struct Live {
    dev: DeviceRt,
    /// recv loop が送る鍵盤プレビュー / ランチャーの操作の口。
    cmd_tx: EngineCommandSender,
    project_tx: rtrb::Producer<ProjectDelivery>,
    project_recycle_rx: rtrb::Consumer<Box<ProjectRt>>,
    device_tx: rtrb::Producer<DeviceBundle>,
    device_recycle_rx: rtrb::Consumer<DeviceBundle>,
    bridge: AudioBridgeHandle,
    scope: common::scope_bridge::ScopeBridgeHandle,
    device_scope: DeviceScopeBridgeHandle,
    shared: SharedState,
}

/// off-thread 側の 1 project の口。
struct Tab {
    shared: Arc<ProjectShared>,
    bundle_tx: rtrb::Producer<RtBundle>,
    bundle_recycle_rx: rtrb::Consumer<RtBundle>,
}

impl Live {
    fn new(tag: &str, rig: &Arc<WorkerRig>) -> Self {
        let engine = Arc::new(EngineShared::new());
        let (cmd_tx, cmd_rx) = EngineCommandSender::channel();
        let (project_tx, project_rx) = rtrb::RingBuffer::new(8);
        let (project_recycle_tx, project_recycle_rx) = rtrb::RingBuffer::new(8);
        let (mut device_tx, device_rx) = rtrb::RingBuffer::new(4);
        let (device_recycle_tx, device_recycle_rx) = rtrb::RingBuffer::new(4);
        let dev = DeviceRt::new(
            common::process_data::MAX_FRAMES,
            cmd_rx,
            engine,
            project_rx,
            project_recycle_tx,
            device_rx,
            device_recycle_tx,
        );
        device_tx.push(DeviceBundle { worker: Some(Arc::clone(rig)), sampler: None }).ok().unwrap();
        let pid = std::process::id();
        Self {
            dev,
            cmd_tx,
            project_tx,
            project_recycle_rx,
            device_tx,
            device_recycle_rx,
            bridge: AudioBridgeHandle::create(&format!("daw01_test_stall_ab_{tag}_{pid}")).unwrap(),
            scope: common::scope_bridge::ScopeBridgeHandle::create(&format!("daw01_test_stall_sc_{tag}_{pid}")).unwrap(),
            device_scope: DeviceScopeBridgeHandle::create(&format!("daw01_test_stall_ds_{tag}_{pid}")).unwrap(),
            shared: SharedState::new(),
        }
    }

    /// off-thread 側と同じ手順で project slot を開く (recv loop の `OpenProject`)。
    fn open(&mut self, key: ProjectKey) -> Tab {
        let slot = self.bridge.claim_project_slot(key).unwrap();
        let (shared, pool_rx, pool_recycle_tx) = ProjectShared::new_with_stretch_rings(key, slot);
        let shared = Arc::new(shared);
        let (bundle_tx, bundle_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, bundle_recycle_rx) = rtrb::RingBuffer::new(8);
        let rt = ProjectRt::new(
            common::process_data::MAX_FRAMES,
            Arc::clone(&shared),
            bundle_rx,
            recycle_tx,
            pool_rx,
            pool_recycle_tx,
        );
        self.project_tx.push(ProjectDelivery::Open(Box::new(rt))).ok().unwrap();
        Tab { shared, bundle_tx, bundle_recycle_rx }
    }

    fn callback(&mut self) {
        self.dev.process_buffer(&self.shared, &self.bridge, &self.scope, &self.device_scope, SR, FRAMES);
    }

    /// recv loop の housekeeping と同じく、RT が返した物を off-thread で落とす。
    fn reap(&mut self, tab: &mut Tab) {
        while let Ok(old) = self.device_recycle_rx.pop() {
            drop(old);
        }
        while let Ok(old) = tab.bundle_recycle_rx.pop() {
            drop(old);
        }
    }

    fn project(&self, key: ProjectKey) -> Option<&ProjectRt> {
        self.dev.projects.iter().find(|p| p.key == key).map(|b| &**b)
    }
}

fn bundle(song: &Arc<Song>, refs: &Arc<PluginRefs>) -> RtBundle {
    RtBundle {
        song: Some(Arc::clone(song)),
        song_index: Arc::new(SongIndex::build(song)),
        song_bounds: SongBounds::of(Some(song), SR),
        tempo_map: Arc::new(common::tempo_map::TempoMap::from_song(song)),
        schedule: Some(
            crate::graph::compile_schedule(
                song,
                &crate::graph::DeviceLatencies::new(),
                &crate::graph::LoadingDevices::new(),
                SR,
                FRAMES as u32,
                common::protocol::RenderScope::Mix,
            )
            .unwrap(),
        ),
        reset_song_scoped_state: false,
        input_delay_replacements: Vec::new(),
        scratch_growth: Some(crate::mixer::ScratchGrowth::new(0, song.tracks.len())),
        launcher_growth: Some(crate::launcher::LauncherGrowth::for_song(song)),
        plane: None,
        follower_maps: None,
        plugin_refs: Arc::clone(refs),
        preview_sequence: None,
        loop_region: common::model::LoopRegion::default(),
        recording_lanes: Arc::new(RecordingLanes::new()),
        audio_clip_renderer: Arc::new(AudioClipRenderer::empty()),
        device_scope_watch: [0; MAX_DEVICE_SCOPES],
        mod_plan: None,
        mod_phase_table: None,
    }
}

/// 再生中の project 1 つを、`rig` の pool で生き残りが出るまで回す (callback 役が戻った時点で worker が文脈を
/// 持ったまま止まっている)。
fn stall_live(live: &mut Live, tab: &Tab, rig: &WorkerRig, host: &FakeHost) {
    live.callback();
    tab.shared.playback.store(PlaybackCommand::Play as u8, Ordering::Release);
    live.callback();
    assert!(!pool(rig).is_stalled(), "前提: 止めるまでは stall しない");
    host.suspend_next.store(true, Ordering::SeqCst);
    for _ in 0..8 {
        live.callback();
        if pool(rig).is_stalled() {
            break;
        }
    }
    assert!(pool(rig).is_stalled() && window_open(rig), "前提: 文脈を持った生き残りが居る stall");
}

/// live (callback) の窓: stall で callback が先に戻っても、生き残りが文脈を手放す (`inside` = 0) までは、その project
/// の資源 (曲 / schedule / scratch / bus) を差し替えも再利用もせず、生き残りが借りている rig も返さない。窓が閉じたら
/// 続きから再開し、rig を off-thread へ返す。
#[test]
fn 窓が開いている間_live_は_project_にも_rig_にも触らず_閉じたら再開する() {
    let (rig, host) = pooled_rig("live", [ms(20), ms(2)]);
    let plugins = plugins(1..=8);
    let mut live = Live::new("live", &rig);
    let key = ProjectKey(1);
    let mut tab = live.open(key);
    let song = Arc::new(chain_song(4));
    tab.bundle_tx.push(bundle(&song, &plugins.refs)).ok().unwrap();
    stall_live(&mut live, &tab, &rig, &host);
    // 生き残りは chain (plugin 4 つ) の頭の依頼の中で止まっている。動き出したらその 1 回だけを終える。
    let calls_at_stall = host.calls_on(1);

    // 窓の間: 新しい曲の便と、立て直した rig (plugin host の respawn) が届く。
    let playhead = tab.shared.playhead.load(Ordering::Acquire);
    let old_rig: Weak<WorkerRig> = Arc::downgrade(&rig);
    let song2 = Arc::new(chain_song(4));
    tab.bundle_tx.push(bundle(&song2, &plugins.refs)).ok().unwrap();
    let fresh = Arc::new(WorkerRig::new(
        None,
        Vec::new(),
        1,
        WorkerBridgeHandle::create(&format!("daw01_test_stall_wb_fresh_{}", std::process::id())).unwrap(),
    ));
    live.device_tx.push(DeviceBundle { worker: Some(Arc::clone(&fresh)), sampler: None }).ok().unwrap();
    drop(rig);
    // 鍵盤プレビューも届く (積むと、生き残りが読む scratch を書き換える)。
    live.cmd_tx.send(EngineCommand::PreviewNoteOn { project: key, track: 0, pitch: 60, velocity: 1.0 });
    for _ in 0..3 {
        live.callback();
    }
    live.reap(&mut tab);
    assert!(
        live.project(key).expect("project").scratch.iter().all(|s| s.state.pending_preview.is_empty()),
        "窓の間の project へ鍵盤プレビューを積んだ (生き残りが読む scratch を書き換えた)"
    );
    assert!(window_open(&old_rig.upgrade().expect("窓の間に rig を返した (生き残りが slot / bridge を使っている)")));
    assert_eq!(tab.shared.playhead.load(Ordering::Acquire), playhead, "窓の間に transport を進めた (project の資源を再利用した)");
    let p = live.project(key).expect("project");
    assert!(p.cached_song.as_ref().is_some_and(|s| Arc::ptr_eq(s, &song)), "窓の間に便を取り込んだ (生き残りが読む曲を手放した)");
    assert!(live.dev.master_l[..FRAMES].iter().all(|&v| v == 0.0), "窓の間に生き残りが書く bus を出力へ混ぜた");

    // 窓が閉じたら、次の callback から再開する。
    wait_window_closed(&old_rig.upgrade().expect("rig"));
    assert!(
        host.calls_on(1) <= calls_at_stall + 1,
        "動き出した生き残りが、見捨てられた buffer の残りの plugin を叩いた (窓が長引く)"
    );
    live.callback();
    live.reap(&mut tab);
    assert!(old_rig.upgrade().is_none(), "窓が閉じても rig を off-thread へ返さない");
    let p = live.project(key).expect("project");
    assert!(p.cached_song.as_ref().is_some_and(|s| Arc::ptr_eq(s, &song2)), "窓が閉じても便を取り込まない");
    live.callback();
    assert!(tab.shared.playhead.load(Ordering::Acquire) > playhead, "窓が閉じても transport が止まったまま");
    assert_eq!(quarantined(&plugins), 0, "前提: どの plugin も期限を過ぎていない");
    drop(host);
}

/// 描き切った後、見直しの途中で止められた worker が文脈を持ったままでも、callback は期限で戻り (RT で無限に回らない)、
/// 窓を開けたまま project を凍結する。worker は止めずに、止められた worker が持つ印 (`inside`) だけを立てて作る —
/// 「全 job を終えてから `inside` を下ろすまで」の数命令の間を OS の suspend では狙えないため。
#[test]
fn 描き切った後に文脈を手放さない_worker_が居ても_callback_は期限で戻る() {
    let (rig, host) = pooled_rig("finished", [ms(2), ms(2)]);
    let plugins = plugins(1..=8);
    let mut live = Live::new("finished", &rig);
    let key = ProjectKey(1);
    let mut tab = live.open(key);
    tab.bundle_tx.push(bundle(&Arc::new(chain_song(4)), &plugins.refs)).ok().unwrap();
    pool(&rig).shared.inside.fetch_add(1, Ordering::SeqCst);
    live.callback();
    assert!(pool(&rig).is_stalled(), "文脈を手放さない worker を待ち続けた、または窓を開けずに戻った");
    assert!(live.project(key).is_some_and(ProjectRt::in_window), "窓の開いた project を凍結しない");
    assert!(host.calls_on(0) + host.calls_on(1) >= 8, "前提: グラフは描き切った");
    pool(&rig).shared.inside.fetch_sub(1, Ordering::SeqCst);
    live.callback();
    assert!(live.project(key).is_some_and(|p| !p.in_window()), "窓が閉じても再開しない");
    live.reap(&mut tab);
    drop(host);
}

/// 描き切った後、見直しの途中の worker (文脈を持ったまま) は期限まで待つ — すぐに stall にすると、生きた worker の
/// ために pool を捨てて plugin host を立て直すことになる。
#[test]
fn 描き切った後の見直しの途中の_worker_は期限まで待つ() {
    let (rig, host) = pooled_rig("recheck", [ms(2), ms(2)]);
    let plugins = plugins(1..=8);
    let mut live = Live::new("recheck", &rig);
    let key = ProjectKey(1);
    let mut tab = live.open(key);
    tab.bundle_tx.push(bundle(&Arc::new(chain_song(4)), &plugins.refs)).ok().unwrap();
    pool(&rig).shared.inside.fetch_add(1, Ordering::SeqCst);
    let release = {
        let rig = Arc::clone(&rig);
        std::thread::spawn(move || {
            std::thread::sleep(ms(200));
            pool(&rig).shared.inside.fetch_sub(1, Ordering::SeqCst);
        })
    };
    live.callback();
    release.join().unwrap();
    assert!(!pool(&rig).is_stalled(), "期限の前に手放す worker を待たずに stall にした");
    assert!(live.project(key).is_some_and(|p| !p.in_window()), "描き切った project を凍結した");
    live.reap(&mut tab);
    drop(host);
}

/// 窓の開いたまま `DeviceRt` を落としても (stream の作り直し・終了)、その project は解放しない — 生き残りが文脈を通して
/// まだ触りうる。project が持つ rig の参照ごと残る。
#[test]
fn 窓の開いたまま_device_rt_を落としても_project_を解放しない() {
    let (rig, host) = pooled_rig("drop", [ms(2), ms(2)]);
    let plugins = plugins(1..=8);
    let mut live = Live::new("drop", &rig);
    let mut tab = live.open(ProjectKey(1));
    tab.bundle_tx.push(bundle(&Arc::new(chain_song(4)), &plugins.refs)).ok().unwrap();
    pool(&rig).shared.inside.fetch_add(1, Ordering::SeqCst);
    live.callback();
    assert!(pool(&rig).is_stalled(), "前提: 窓が開いた");
    live.reap(&mut tab);
    drop(live);
    assert_eq!(Arc::strong_count(&rig), 2, "窓の開いた project を解放した (rig の参照が残らない)");
    pool(&rig).shared.inside.fetch_sub(1, Ordering::SeqCst);
    drop(host);
}

/// 窓の外の project は今までどおり: 走行の判定、書き出しの予約中の playhead の publish、コマンドの配達。
#[test]
fn 窓の外の_project_は今までどおり扱う() {
    let (rig, host) = pooled_rig("rest", [ms(1), ms(1)]);
    let plugins = plugins(1..=8);
    let mut live = Live::new("rest", &rig);
    let key = ProjectKey(1);
    let mut tab = live.open(key);
    tab.bundle_tx.push(bundle(&Arc::new(chain_song(4)), &plugins.refs)).ok().unwrap();
    live.callback();
    assert!(!live.dev.any_rolling() && !live.dev.any_playing(), "止まっている project を走っているとみなした");
    tab.shared.playback.store(PlaybackCommand::Play as u8, Ordering::Release);
    // plugin の出力を鳴らす (偽の host は出力を書かず、`ProcessData::prepare` は出力を消さない)。
    let mut plugins = plugins;
    for pd in &mut plugins._pds {
        pd.buffer_out[0].fill(0.25);
        pd.buffer_out[1].fill(0.25);
    }
    live.callback();
    assert!(live.dev.any_rolling() && live.dev.any_playing(), "再生中の project を止まっているとみなした");
    let p = live.project(key).expect("project");
    assert!(p.bus_l[0] != 0.0 && p.bus_r[0] != 0.0, "前提: 鳴っている");
    assert_eq!(
        (&live.dev.master_l[..FRAMES], &live.dev.master_r[..FRAMES]),
        (&p.bus_l[..FRAMES], &p.bus_r[..FRAMES]),
        "デバイスの出力が project の bus の和でない"
    );
    assert_eq!(live.dev.shared.last_buffer_frames.load(Ordering::Acquire), FRAMES as u32, "buffer の長さを publish しない");
    // 書き出しの予約中は描かないが、playhead は publish し、コマンドは届く (積まれたまま次の描画を待つ)。
    live.dev.shared.export_running.store(true, Ordering::SeqCst);
    tab.shared.playhead.store(12_345, Ordering::Release);
    live.cmd_tx.send(EngineCommand::PreviewNoteOn { project: key, track: 0, pitch: 60, velocity: 1.0 });
    live.callback();
    let p = live.project(key).expect("project");
    assert_eq!(live.bridge.project(p.telemetry_slot).playhead_samples(), 12_345, "書き出しの予約中に playhead を publish しない");
    assert_eq!(p.scratch[0].state.pending_preview.len(), 1, "窓の外の project へコマンドが届かない");
    live.reap(&mut tab);
    drop(host);
}

/// 窓の間に閉じた project は RT から外れるが、窓が閉じるまで off-thread へ返さない (返すと recv loop が解放する)。
#[test]
fn 窓の間に閉じた_project_は窓が閉じてから_off_thread_へ戻る() {
    let (rig, host) = pooled_rig("close", [ms(20), ms(2)]);
    let plugins = plugins(1..=8);
    let mut live = Live::new("close", &rig);
    let key = ProjectKey(1);
    let mut tab = live.open(key);
    tab.bundle_tx.push(bundle(&Arc::new(chain_song(4)), &plugins.refs)).ok().unwrap();
    stall_live(&mut live, &tab, &rig, &host);

    live.project_tx.push(ProjectDelivery::Close(key)).ok().unwrap();
    live.callback();
    assert!(live.project(key).is_none(), "閉じた project が RT に残っている");
    assert!(live.project_recycle_rx.pop().is_err(), "窓の間に閉じた project を off-thread へ返した (解放される)");
    live.callback();
    assert!(live.project_recycle_rx.pop().is_err(), "預かった project を、窓が閉じる前に off-thread へ返した (解放される)");
    wait_window_closed(&rig);
    live.callback();
    assert_eq!(live.project_recycle_rx.pop().map(|p| p.key).ok(), Some(key), "窓が閉じても閉じた project を返さない");
    live.reap(&mut tab);
    drop(host);
}

/// 書き出しの窓: 書き出しのスレッドは stall で先に戻った後、生き残りが文脈を手放すまで、その文脈が指す scratch /
/// schedule / 曲を解放しない。stall は失敗として返す (無音の続きを成功として書かない)。
#[test]
fn 書き出しは窓が閉じるまで資源を手放さず_stall_を失敗として返す() {
    offline_stall_window_closes("export", |engine, project, dir| {
        crate::export::run_export(
            dir.join("stall.wav"),
            engine,
            project,
            chain_song(4),
            SR,
            common::process_data::MAX_FRAMES,
            crate::export::RenderSpan::RangeCold { start_beat: 0.0, end_beat: 16.0 },
            common::protocol::RenderScope::Mix,
            false,
            |_, _| {},
        )
        .is_ok()
    });
}

/// ラウドネス解析も同じ走査 (`render_loop`) を通る — 窓が閉じるまで資源を手放さず、stall を失敗として返す
/// (待つ上限は書き出しと同じ)。
#[test]
fn ラウドネス解析も窓が閉じるまで資源を手放さず_stall_を失敗として返す() {
    offline_stall_window_closes("loudness", |engine, project, _| {
        crate::export::run_loudness_analysis(
            engine,
            project,
            chain_song(4),
            SR,
            common::process_data::MAX_FRAMES,
            crate::export::RenderSpan::RangeCold { start_beat: 0.0, end_beat: 16.0 },
            |_| {},
        )
        .is_ok()
    });
}

/// オフラインの走査 (`run`、戻り値 = 成功したか) の途中で worker を [`SUSPEND`] だけ止める。窓は待つ上限 (17 s) の
/// 内に閉じるので、走査は窓が閉じてから失敗で戻り、走査の物は解放する。
fn offline_stall_window_closes(tag: &str, run: impl FnOnce(Arc<EngineShared>, Arc<ProjectShared>, &std::path::Path) -> bool) {
    let (rig, host) = pooled_rig(tag, [ms(20), ms(2)]);
    let plugins = plugins(1..=8);
    let engine = Arc::new(EngineShared::new());
    engine.worker.store(Some(Arc::clone(&rig)));
    let project = Arc::new(ProjectShared::new(ProjectKey(1), 0));
    project.plugin_refs.store(Arc::clone(&plugins.refs));
    let dir = std::env::temp_dir().join(format!("daw01_test_stall_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    host.suspend_next.store(true, Ordering::SeqCst);
    let refs_before = Arc::strong_count(&plugins.refs);
    let succeeded = run(Arc::clone(&engine), Arc::clone(&project), &dir);
    let open_at_return = window_open(&rig);
    assert!(pool(&rig).is_stalled(), "前提: 走査の途中で stall した");
    assert!(!open_at_return, "走査が窓の閉じる前に戻った (生き残りが読む scratch / schedule / 曲を解放した)");
    assert!(!succeeded, "stall した走査を成功として返した");
    // 窓が期限内に閉じたら、走査の物は普通に解放する (leak するのは閉じなかったときだけ)。
    assert_eq!(Arc::strong_count(&plugins.refs), refs_before, "窓が閉じたのに走査の物を leak した");
    assert_eq!(quarantined(&plugins), 0, "前提: どの plugin も期限を過ぎていない");
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 書き出しの窓が期限の内に閉じない (止められた worker が戻らない): 走査は器が指す物を持ち主ごと忘れて (leak) 失敗で
/// 戻り、書き出しのスレッドは終わる (本番の `offline_jobs` と同じく、スタックは解放される)。その後に動き出した
/// 生き残りが器の referent に触っても、それは走査の持ち主 (heap) の中に居るので、触って戻り、窓が閉じる。待つ上限は
/// 引数で縮める (本番は 17 s)。
#[test]
fn 書き出しの窓が期限の内に閉じなければ_器の指す物を番地ごと残して失敗で返す() {
    let (rig, host) = pooled_rig("leak", [ms(20), ms(2)]);
    let plugins = plugins(1..=8);
    let engine = Arc::new(EngineShared::new());
    engine.worker.store(Some(Arc::clone(&rig)));
    let project = Arc::new(ProjectShared::new(ProjectKey(1), 0));
    project.plugin_refs.store(Arc::clone(&plugins.refs));
    let dir = std::env::temp_dir().join(format!("daw01_test_stall_leak_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    host.hold.store(true, Ordering::SeqCst);
    host.suspend_next.store(true, Ordering::SeqCst);
    let refs_before = Arc::strong_count(&plugins.refs);
    let export = {
        let (engine, project, path) = (Arc::clone(&engine), Arc::clone(&project), dir.join("leak.wav"));
        std::thread::spawn(move || {
            crate::export::export_wav(
                path,
                engine,
                project,
                chain_song(4),
                SR,
                common::process_data::MAX_FRAMES,
                crate::export::RenderSpan::RangeCold { start_beat: 0.0, end_beat: 16.0 },
                common::protocol::RenderScope::Mix,
                false,
                ms(50),
                |_, _| {},
            )
            .map(|o| o.frames)
            .map_err(|e| e.to_string())
        })
    };
    let outcome = export.join().expect("書き出しのスレッドが panic した (器の referent が呼び側のスタックに居る?)");
    assert!(pool(&rig).is_stalled() && window_open(&rig), "前提: 窓の閉じないまま書き出しが諦めて戻った");
    assert!(outcome.is_err(), "stall した書き出しを成功として返した");
    assert!(Arc::strong_count(&plugins.refs) > refs_before, "窓が閉じないのに書き出しの物を手放した");

    // 書き出しのスレッドが終わった後に生き残りを動かす。
    let calls_at_stall = host.calls_on(1);
    host.hold.store(false, Ordering::SeqCst);
    wait_window_closed(&rig);
    assert!(
        host.calls_on(1) <= calls_at_stall + 1,
        "動き出した生き残りが、見捨てられた buffer の残りの plugin を叩いた (窓が長引く)"
    );
    assert_eq!(quarantined(&plugins), 0, "前提: どの plugin も期限を過ぎていない");
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// live の窓: callback は窓が閉じる前に戻り、そのスタックを次の callback が使い回す。器の referent は project
/// (`Box<ProjectRt>`) と `Arc` の中に居る (InFlight のたびに `in_flight` が debug build で番地を見る) ので、窓の間に
/// callback のスタックを塗り潰してから生き残りを動かしても、触って戻り、窓が閉じて続きから再開する。
#[test]
fn live_の窓の間に_callback_のスタックを使い回しても_生き残りは器の指す物に触って戻る() {
    let (rig, host) = pooled_rig("stack", [ms(20), ms(2)]);
    let plugins = plugins(1..=8);
    let mut live = Live::new("stack", &rig);
    let key = ProjectKey(1);
    let mut tab = live.open(key);
    tab.bundle_tx.push(bundle(&Arc::new(chain_song(4)), &plugins.refs)).ok().unwrap();
    host.hold.store(true, Ordering::SeqCst);
    stall_live(&mut live, &tab, &rig, &host);
    let calls_at_stall = host.calls_on(1);
    let playhead = tab.shared.playhead.load(Ordering::Acquire);
    for _ in 0..4 {
        live.callback();
        clobber_stack();
    }
    assert!(window_open(&rig), "前提: 塗り潰す間、生き残りは止まったまま");

    host.hold.store(false, Ordering::SeqCst);
    wait_window_closed(&rig);
    assert!(
        host.calls_on(1) <= calls_at_stall + 1,
        "動き出した生き残りが、見捨てられた buffer の残りの plugin を叩いた (窓が長引く)"
    );
    live.callback();
    live.callback();
    assert!(tab.shared.playhead.load(Ordering::Acquire) > playhead, "窓が閉じても transport が止まったまま");
    assert_eq!(quarantined(&plugins), 0, "前提: どの plugin も期限を過ぎていない");
    live.reap(&mut tab);
    drop(host);
}

/// 窓の規則の口 (`in_flight`) そのもの: 器の referent が呼び側のスレッドのスタックに居る文脈で InFlight を返したら
/// 止め、スタックに居る物を残らず名指しする (debug build)。スタックの範囲の取り方や比較が崩れて口が黙って素通り
/// する形と、`RenderCtx::referents` の列の項目が 0 や heap の番地にすり替わる形を捕まえる。referent 30 個のうち
/// `Schedule` の `Vec` の中身 5 個 (track の program / delay line / follower / node / 入力遅延) は型の上で heap なので
/// 置けず、残り 25 個を全部スタックに置く。
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "[\"song\", \"index\", \"scratch\", \"master_program\", \"master_l\", \"master_r\", \"solo\", \
                           \"graph\", \"rig\", \"plugin_refs\", \"audio_renderer\", \"recording_lanes\", \"mod_plane.ids\", \
                           \"mod_plane.col_of\", \"mod_plane.values\", \"mod_plane.depth_ids\", \"mod_plane.depth_col_of\", \
                           \"mod_plane.depths\", \"mod_plane.nodes\", \"follower_drive.spans\", \
                           \"follower_drive.col_of_slot\", \"follower_drive.eff\", \"rows\", \"native_io.scopes.bridge\", \
                           \"native_io.scopes.watch\"] が呼び側のスレッドのスタック")]
fn 器が呼び側のスタックを指したまま_in_flight_を返すと_スタックに居る物を全部名指しして止める() {
    let mut song = chain_song(1);
    song.mod_sources = vec![common::model::ModSource {
        id: 1,
        owner_track_id: 1,
        color: [0.0; 3],
        kind: common::model::ModSourceKind::Lfo(common::model::LfoConfig::default()),
        enabled: true,
    }];
    let mod_nodes = [common::mod_graph::build_plan(&song, 1, |beat| beat * 0.5).nodes[0].clone()];
    let index = SongIndex::build(&song);
    let mut schedule = crate::graph::compile_schedule(
        &song,
        &crate::graph::DeviceLatencies::new(),
        &crate::graph::LoadingDevices::new(),
        SR,
        FRAMES as u32,
        common::protocol::RenderScope::Mix,
    )
    .unwrap();
    let mut scratch = [(); 3].map(|()| crate::mixer::TrackScratch::new());
    assert_eq!(scratch.len(), song.tracks.len(), "前提: track ごとの scratch");
    let (mut master_l, mut master_r) = ([0.0f32; FRAMES], [0.0f32; FRAMES]);
    let pid = std::process::id();
    let rig = WorkerRig::new(None, Vec::new(), 1, WorkerBridgeHandle::create(&format!("daw01_test_stall_wb_onstack_{pid}")).unwrap());
    let refs = PluginRefs::new();
    let renderer = AudioClipRenderer::empty();
    let recording_lanes = RecordingLanes::new();
    let (mod_ids, mod_col_of, mod_values) = ([1u32], [(1u32, 0u32)], [0.0f32]);
    let (depth_ids, depth_col_of, depths) = ([1u32], [(1u32, 0u32)], [0.0f32]);
    let spans = [crate::mod_tick::TickSpan { row: 0, frame: 0, frames: FRAMES as u32 }];
    let col_of_slot = [0u16];
    let eff = [crate::mod_tick::FollowerEff { attack_ms: 1.0, release_ms: 1.0, gain: 1.0, hp_hz: 20.0, lp_hz: 20_000.0 }];
    let rows = crate::launcher::RowSourceTable::default();
    let scope_bridge = DeviceScopeBridgeHandle::create(&format!("daw01_test_stall_ds_onstack_{pid}")).unwrap();
    let scope_watch = [0u64; MAX_DEVICE_SCOPES];
    let params = crate::graph::step::BufferParams {
        sample_rate: SR,
        frames: FRAMES as u32,
        playing: true,
        any_solo: false,
        recording_lanes: &recording_lanes,
        current_bpm: 120.0,
        playhead_beats: 0.0,
        loop_region: common::model::LoopRegion::default(),
        mod_plane: common::mod_plane::ModTickPlaneRef {
            ids: &mod_ids,
            col_of: &mod_col_of,
            values: &mod_values,
            depth_ids: &depth_ids,
            depth_col_of: &depth_col_of,
            depths: &depths,
            lead: 0,
            first_sample: 0,
            nodes: &mod_nodes,
        },
        follower_drive: crate::mod_tick::FollowerDrive { spans: &spans, first_sample: 0, col_of_slot: &col_of_slot, eff: &eff, n_cols: 1 },
        rows: &rows,
        native_io: crate::graph::NativeIo {
            sc_listen: 0,
            scopes: Some(crate::graph::DeviceScopeTap { bridge: &scope_bridge, watch: &scope_watch }),
        },
        transpose: 0,
    };
    let ctx = crate::graph::step::RenderCtx::new(
        &song,
        &index,
        &mut schedule,
        &mut scratch,
        &mut master_l,
        &mut master_r,
        &refs,
        Some(&renderer),
        Some(&rig),
        params,
    );
    let _ = super::in_flight(&ctx);
}

/// 呼び側のスタックの、callback が使った辺りを塗り潰す (器がスタックを指していれば、生き残りは壊れた値を読む)。
#[inline(never)]
fn clobber_stack() {
    let mut fill = [0xA5u8; 256 * 1024];
    std::hint::black_box(&mut fill);
}

/// stall の判定は「進みが止まった」で行う。plugin の dispatch は 1 回ずつ期限で切れるので、期限内の遅い plugin が
/// 1 つの手 (track の chain) に積み重なっても、生きた runner は dispatch ごとに進んでいる — stall にしない
/// (stall は pool を捨てて plugin host を立て直す)。
#[test]
fn 期限内の遅い_plugin_が_1_つの手に積み重なっても_pool_は_stall_しない() {
    // worker の pair が遅い: worker が取った track の chain は 380 ms × 8 ≈ 3 s。plugin は 1 つずつ期限 (500 ms) の
    // 内に返すが、手 1 つは master の待ちの打ち切り (進みが変わってから 1〜2 s) より長い。callback 役の chain
    // (100 ms × 8) の間に worker が起きてもう 1 本を取る。
    let (rig, host) = pooled_rig("slow", [ms(100), ms(380)]);
    let plugins = plugins(1..=16);
    let mut live = Live::new("slow", &rig);
    let mut tab = live.open(ProjectKey(1));
    tab.bundle_tx.push(bundle(&Arc::new(chain_song(8)), &plugins.refs)).ok().unwrap();
    live.callback();
    assert!(!pool(&rig).is_stalled(), "期限内の遅い plugin を積み重ねただけの生きた runner を stall と判定した");
    assert_eq!(host.calls_on(1), 8, "前提: worker が遅い pair で chain を 1 本描いた");
    assert_eq!(quarantined(&plugins), 0, "前提: どの plugin も期限を過ぎていない");
    live.reap(&mut tab);
    drop(host);
}
