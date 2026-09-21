//! script host の核: `daw.*` の実装が触る状態 ([`ScriptHost`]) と、それを headless / GUI の
//! 2 つのモードで動かす仕組み。
//!
//! - **headless** (`--script`): script スレッド (= main) が `AppData` も IPC の受け口も持つ。
//! - **GUI** (`--script --gui`): `AppData` は winit の event loop スレッドの物。script は別スレッドで
//!   boa を回し、app に触る手 ([`with_app`]) だけを event loop へ送って結果を待つ。待ち
//!   (`sleepMs` / `pump_until`) は script スレッドで寝るだけなので、その間 GUI は描画し続ける。
//!
//! `daw.*` の実装は [`with_app`] (app に触る) と [`with_host`] (script 側の物だけ触る) を使い分ける。
//! **`with_app` の中で待ってはいけない** — GUI では event loop を塞ぐ。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use boa_engine::{Context, Source};
use common::protocol::{AudioCommand, DeviceAddr, PluginCommand, PluginEvent};

use crate::app::{AppData, AppEvent};
use crate::bootstrap::{Bootstrap, ChildEvent};
use crate::dispatcher::RecordingDispatcher;

thread_local! {
    /// Running script の host state。 headless では `run_scripted` が、GUI では script スレッドの
    /// 入口がセットし、終了時に `None` に戻す。 native function は `with_host` / `with_app` で触る。
    pub(super) static HOST: RefCell<Option<ScriptHost>> = const { RefCell::new(None) };
}

/// production binary が `--script <path>` で呼ばれたときの entry (**headless**: winit も窓も無い)。
/// `output_override` は `--output <path>` を script から `daw.scriptArgs.output`
/// として参照可能にするため。 runtime 終了で exit code 0 / JS error で 1。
/// `app_dirs` は main が作った隔離 root (`common::app_dirs::IsolatedAppDirs`)。
pub fn run_scripted(
    bootstrap: Bootstrap,
    script_path: &Path,
    output_override: Option<&Path>,
    extra_args: &[(String, String)],
    app_dirs: Option<common::app_dirs::AppDirs>,
) -> Result<()> {
    let source = std::fs::read_to_string(script_path)
        .with_context(|| format!("failed to read script {}", script_path.display()))?;
    HOST.with_borrow_mut(|h| {
        let output = output_override.map(PathBuf::from);
        *h = Some(ScriptHost::new_headless(bootstrap, output, extra_args.to_vec(), app_dirs));
    });

    let result = eval_script(&source);

    // (r.md #61) script mode も同じ終了シーケンスを通す。旧実装は
    // `*h = None` の drop 任せで、子プロセスは Job Object に強制 kill され、
    // プラグインの deactivate / destroy が走らなかった。
    if let Some(host) = HOST.with_borrow_mut(std::option::Option::take) {
        host.shutdown_headless();
    }
    result
}

/// boa の context を作って `source` を最後まで評価する (両モード共通)。
fn eval_script(source: &str) -> Result<()> {
    let mut ctx = Context::default();
    super::register_daw_globals(&mut ctx)?;
    let parsed = Source::from_bytes(source.as_bytes());
    ctx.eval(parsed).map_err(|e| anyhow!("script error: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// GUI モード: 同じ JS を、窓・wgpu・poller が生きている本物の GUI に対して走らせる
//
// script は **専用スレッド** で boa を回す。`AppData` は event loop スレッドの物なので、
// script が app に触る手 ([`ScriptHost::with_app`]) は closure を event loop へ送って
// 結果を待つ (= 実機で人が操作したのと同じスレッドで、同じ順に処理される)。
// 待ち (`sleepMs` / `pump_until`) は script スレッドで寝るだけなので、その間 GUI は
// 普通に描画し続ける。子プロセスからの event は runner が app へ dispatch した後、
// 写しを [`ScriptAttach::observe`] が鏡の queue に積み、`pump_until` はそれを読む。
// ---------------------------------------------------------------------------

/// script が触る IPC / shmem のハンドル。`Bootstrap` から抜いた clone で、headless でも
/// GUI でも同じ形 (= API 実装がモードを意識しない)。
#[derive(Clone)]
pub struct ScriptIo {
    pub audio_tx: tokio::sync::mpsc::UnboundedSender<AudioCommand>,
    pub plugin_tx: tokio::sync::mpsc::UnboundedSender<PluginCommand>,
    pub bridge: Arc<common::audio_bridge::AudioBridgeHandle>,
    pub metrics: Arc<common::metrics_bridge::MetricsBridgeHandle>,
    pub scope: Arc<common::scope_bridge::ScopeBridgeHandle>,
    pub plugin_db: Option<Arc<common::plugin_db::PluginDatabase>>,
}

impl ScriptIo {
    pub fn from_bootstrap(b: &Bootstrap) -> Self {
        Self {
            audio_tx: b.audio_tx.clone(),
            plugin_tx: b.plugin_tx.clone(),
            bridge: Arc::clone(&b.bridge),
            metrics: Arc::clone(&b.metrics),
            scope: Arc::clone(&b.scope),
            plugin_db: b.plugin_db.clone(),
        }
    }
}

/// `AppData` と同じ場所 (= 同じスレッド) に住む script 側の帳簿。
#[derive(Default)]
pub struct ScriptSide {
    /// v29: 生 `daw.setSlotPlugin` 用の要求 generation counter (AppData の
    /// counter と衝突しないよう script 側でも単調増加を維持し、 送信前に
    /// `app.cur.pipc.pending_plugin_loads` へ登録して echo を通す)。
    pub(super) next_raw_load_generation: u64,
    /// `daw.takePluginLoadEventsJson()` が返して clear する観測バッファ。
    /// plugin load の成否をログ grep ではなく **script 内の assertion** で
    /// 判定するために貯める (`tests/scripts/reopen_same_project.js`)。
    pub(super) plugin_load_events: PluginLoadEvents,
}

impl ScriptSide {
    /// 子プロセスからの event を観測して帳簿に載せる (両モード共通。app への dispatch は
    /// headless では [`dispatch_incoming_headless`]、GUI では runner がやる)。
    pub fn observe(&mut self, msg: &ChildEvent) {
        let ChildEvent::Plugin(msg) = msg else { return };
        match msg {
            PluginEvent::SlotPluginLoaded { device: DeviceAddr { device_id, .. }, .. } => {
                self.plugin_load_events.loaded.push(*device_id);
            }
            PluginEvent::SlotPluginLoadFailed {
                device: DeviceAddr { device_id, .. },
                plugin_id,
                reason,
                ..
            } => {
                tracing::error!(device_id, %plugin_id, %reason, "script: plugin load failed");
                self.plugin_load_events.failed.push(FailedPluginLoad {
                    device_id: *device_id,
                    plugin_id: plugin_id.clone(),
                    reason: reason.clone(),
                });
            }
            _ => {}
        }
    }
}

/// event loop スレッドへ渡す 1 手。戻り値は型消去して script スレッドで戻す。
pub type Step =
    Box<dyn FnOnce(&mut AppData, &mut ScriptSide, &ScriptIo) -> Box<dyn std::any::Any + Send> + Send>;

/// 鏡の queue の上限。pipe event は plugin の load 応答や書き出し完了程度の頻度
/// (telemetry は shmem) なので、長い `sleepMs` の間に溜まる量はこれで十分。
const EVENT_MIRROR_CAP: usize = 4096;

/// GUI モードで runner が持つ script の受け口 ([`crate::view::runner::RunnerScript`])。
pub struct ScriptAttach {
    steps: Arc<std::sync::Mutex<std::collections::VecDeque<Step>>>,
    reply_tx: std::sync::mpsc::Sender<Box<dyn std::any::Any + Send>>,
    events: Arc<std::sync::Mutex<std::collections::VecDeque<ChildEvent>>>,
    side: ScriptSide,
    io: ScriptIo,
}

impl crate::view::runner::RunnerScript for ScriptAttach {
    fn observe(&mut self, event: &AppEvent) {
        let ce = match event {
            AppEvent::Audio(e) => ChildEvent::Audio(e.clone()),
            AppEvent::Plugin(e) => ChildEvent::Plugin(e.clone()),
            _ => return,
        };
        self.side.observe(&ce);
        let mut q = self.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if q.len() >= EVENT_MIRROR_CAP {
            q.pop_front();
        }
        q.push_back(ce);
    }

    fn run_steps(&mut self, app: &mut AppData) -> bool {
        let mut ran = false;
        loop {
            let step = self
                .steps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front();
            let Some(step) = step else { break };
            let reply = step(app, &mut self.side, &self.io);
            // 受け手 (script スレッド) が居なくなっていても runner は止めない。
            let _ = self.reply_tx.send(reply);
            ran = true;
        }
        ran
    }
}

/// script スレッド側の GUI への口。
struct GuiLink {
    proxy: winit::event_loop::EventLoopProxy<AppEvent>,
    steps: Arc<std::sync::Mutex<std::collections::VecDeque<Step>>>,
    reply_rx: std::sync::mpsc::Receiver<Box<dyn std::any::Any + Send>>,
    events: Arc<std::sync::Mutex<std::collections::VecDeque<ChildEvent>>>,
}

impl GuiLink {
    /// 1 手を event loop スレッドへ積み、実行されて返事が来るまで待つ。
    fn run<R, F>(&mut self, f: F) -> R
    where
        R: Send + 'static,
        F: FnOnce(&mut AppData, &mut ScriptSide, &ScriptIo) -> R + Send + 'static,
    {
        let step: Step = Box::new(move |app, side, io| Box::new(f(app, side, io)));
        self.steps.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push_back(step);
        loop {
            // 起こす。runner の state がまだ無い瞬間に届いた wake は落ちるので、
            // 返事が無ければ送り直す (手は queue に残っている)。
            let _ = self.proxy.send_event(AppEvent::ScriptStep);
            match self.reply_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(reply) => {
                    return *reply.downcast::<R>().expect("script step reply has the closure's type");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("script: event loop went away while a step was pending");
                }
            }
        }
    }
}

/// proxy が手に入ってから script スレッドを起こす closure ([`prepare_gui`] の戻り)。
pub type ScriptStarter = Box<dyn FnOnce(winit::event_loop::EventLoopProxy<AppEvent>) + Send>;

/// `--script <js> --gui` の準備。runner に渡す受け口と、proxy が手に入ってから script スレッドを
/// 起動する starter を返す (proxy は runner の中でしか作れないので後結び)。
///
/// script が終わると `AppEvent::Quit` (automated、JS エラーなら exit code 1) を送る =
/// headless と同じく「script の終わり = プロセスの終わり」。
pub fn prepare_gui(
    script_path: PathBuf,
    output: Option<PathBuf>,
    extra: Vec<(String, String)>,
    io: ScriptIo,
) -> (ScriptAttach, ScriptStarter) {
    let steps = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let events = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let attach = ScriptAttach {
        steps: Arc::clone(&steps),
        reply_tx,
        events: Arc::clone(&events),
        side: ScriptSide::default(),
        io: io.clone(),
    };
    let starter: ScriptStarter = Box::new(move |proxy: winit::event_loop::EventLoopProxy<AppEvent>| {
        let quit_proxy = proxy.clone();
        let spawned = std::thread::Builder::new().name("script".into()).spawn(move || {
            let source = match std::fs::read_to_string(&script_path) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = ?e, path = %script_path.display(), "failed to read script");
                    let _ = quit_proxy
                        .send_event(AppEvent::Quit(crate::shutdown::QuitRequest::automated(1)));
                    return;
                }
            };
            HOST.with_borrow_mut(|h| {
                *h = Some(ScriptHost {
                    script_args: ScriptArgs { output, extra },
                    io,
                    link: Link::Gui(GuiLink { proxy, steps, reply_rx, events }),
                });
            });
            let code = match eval_script(&source) {
                Ok(()) => 0,
                Err(e) => {
                    tracing::error!(error = %e, "script failed");
                    eprintln!("{e}");
                    1
                }
            };
            HOST.with_borrow_mut(|h| *h = None);
            let _ = quit_proxy.send_event(AppEvent::Quit(crate::shutdown::QuitRequest::automated(code)));
        });
        if let Err(e) = spawned {
            tracing::error!(error = ?e, "failed to spawn script thread");
        }
    });
    (attach, starter)
}

/// headless の中身 (`AppData` が大きいので Box で持つ)。
struct HeadlessLink {
    bootstrap: Bootstrap,
    app: AppData,
    side: ScriptSide,
}

/// app がどこに住んでいるか。
enum Link {
    /// headless: script スレッド (= main) が app も IPC の受け口も持つ。
    Headless(Box<HeadlessLink>),
    /// GUI: app は runner の物。手は event loop へ送り、event は鏡から読む。
    Gui(GuiLink),
}

/// boa context が握る host state。
pub(super) struct ScriptHost {
    /// `--output` etc. を script に渡すための args bag。
    pub(super) script_args: ScriptArgs,
    pub(super) io: ScriptIo,
    link: Link,
}

/// script が観測した plugin load 応答 (`daw.takePluginLoadEventsJson`)。
#[derive(Default, serde::Serialize)]
pub(super) struct PluginLoadEvents {
    /// `SlotPluginLoaded` を受けた device id (受信順)。
    loaded: Vec<u64>,
    /// `SlotPluginLoadFailed` を受けた device (理由付き)。
    failed: Vec<FailedPluginLoad>,
}

#[derive(serde::Serialize)]
struct FailedPluginLoad {
    device_id: u64,
    plugin_id: String,
    reason: String,
}

#[derive(Default, Clone)]
pub(super) struct ScriptArgs {
    pub(super) output: Option<PathBuf>,
    /// Free-form `--arg KEY=VALUE` pairs from the CLI, exposed as
    /// `daw.scriptArgs[key]` properties.
    pub(super) extra: Vec<(String, String)>,
}

impl ScriptHost {
    fn new_headless(
        bootstrap: Bootstrap,
        output: Option<PathBuf>,
        extra: Vec<(String, String)>,
        app_dirs: Option<common::app_dirs::AppDirs>,
    ) -> Self {
        // AppData::new は audio_tx / plugin_tx の clone を要求する。
        // bootstrap 内の sender は production と同形なのでそのまま渡せる
        // (= app.handle_event 内の send_audio / send_plugin がそのまま
        // bootstrap が握る IPC channel に流れる)。 dispatcher は test
        // 用 noop / recording。 `_proxy` を返す Recording 実装を
        // BackgroundDispatcher として渡し、 background thread は
        // script では使わないので spawn されない (AppData の API を
        // 同期呼び出しするだけ)。
        let app = AppData::new(
            bootstrap.audio_tx.clone(),
            bootstrap.plugin_tx.clone(),
            None,
            None,
            RecordingDispatcher::new(),
            // (review) script mode でも VOICEVOX engine の lazy spawn は起きる
            // (loadSongFile → 歌唱合成)。 Noop だと Job Object 未登録で script
            // プロセス終了後に engine が zombie 化するので production と同じ
            // dispatcher を渡す。
            Arc::new(crate::dispatcher::Win32JobDispatcher::new(Arc::clone(
                &bootstrap.job,
            ))),
            // script mode は同 process 内の bootstrap が握る supervisor を
            // 渡しても安全だが、 script 中に子プロセスが死ぬケースは
            // テスト・録画用途では発生しない前提なので None で十分。
            None,
            // テスト / 検証の起動なので、ユーザーの実データではなく隔離 root。
            app_dirs,
            // (A1 r.md #8) bootstrap が解決したデバイス実レート。
            bootstrap.sample_rate,
        );
        let io = ScriptIo::from_bootstrap(&bootstrap);
        Self {
            script_args: ScriptArgs { output, extra },
            io,
            link: Link::Headless(Box::new(HeadlessLink { bootstrap, app, side: ScriptSide::default() })),
        }
    }

    /// (r.md #61) 子プロセスへ終了を伝え、有界に待ってから `Bootstrap` を
    /// 明示解体する。GUI の `Runner::drive_shutdown` と同じ契約
    /// (policy = `AppData::begin_shutdown`、待ち方だけが blocking)。
    fn shutdown_headless(self) {
        let Link::Headless(headless) = self.link else {
            // GUI では runner の終了シーケンスが解体する (`run_gui`)。
            return;
        };
        let HeadlessLink { bootstrap, mut app, .. } = *headless;
        // `AppData` には supervisor を渡していない (script mode は respawn しない)
        // ので、pipe loop への「これ以降の切断は crash ではない」通知はここで直接。
        let supervisor = Arc::clone(&bootstrap.supervisor);
        supervisor.begin_shutdown();
        app.begin_shutdown(crate::shutdown::QuitRequest::USER);
        let deadline = std::time::Instant::now() + crate::shutdown::DRAIN_TIMEOUT;
        let stragglers = supervisor.wait_for_children_exit(deadline);
        if !stragglers.is_empty() {
            let names: Vec<&str> = stragglers.iter().map(|k| k.as_str()).collect();
            tracing::warn!(?names, "script mode: children did not exit in time");
        }
        drop(supervisor);
        // 上で `wait_for_children_exit` 済み。二重に待たせない。
        bootstrap.shutdown(true);
    }

    /// app に触る 1 手。headless ではその場で、GUI では event loop スレッドで実行して結果を待つ。
    ///
    /// **この中で待ってはいけない** (`pump_until` / `sleepMs` を呼ぶと GUI の event loop を
    /// 塞ぐ)。送る → 待つ → 読む、は別々の手に分ける。
    fn with_app<R, F>(&mut self, f: F) -> R
    where
        R: Send + 'static,
        F: FnOnce(&mut AppData, &mut ScriptSide, &ScriptIo) -> R + Send + 'static,
    {
        match &mut self.link {
            Link::Headless(h) => f(&mut h.app, &mut h.side, &self.io),
            Link::Gui(g) => g.run(f),
        }
    }

    /// 直近の窓のグラフ内訳。headless は自分で shmem から取り出す (他に読み手が居ない)。GUI は
    /// daw_gui の poller が取り出して app に置いた値を読む (二重に取り出すと両方が半端な窓になる)。
    pub(super) fn graph_window(&mut self) -> common::metrics_bridge::GraphBreakdown {
        if matches!(self.link, Link::Gui(_)) {
            self.with_app(|app, _side, _io| app.ipc.metrics.graph)
        } else {
            self.io.metrics.take_graph()
        }
    }

    /// `daw.*` 1 呼び出し = GUI の 1 frame とみなし、抜けたところで frame 末と同じ子プロセス sync を
    /// 回す (headless には frame loop が無い)。GUI では runner が手の後に回すので何もしない。
    pub(super) fn flush_after_call(&mut self) {
        if let Link::Headless(h) = &mut self.link {
            h.app.flush_all_song_sync();
        }
    }

    /// 条件 `pred` を満たす event が来るまで待つ。
    ///
    /// headless: `incoming_rx` から受けて production と同じ経路で app へ dispatch し
    /// (`spawn_incoming_bridge` 相当)、`pred` を見る。
    /// GUI: runner が dispatch 済みの写し (鏡の queue) を読んで `pred` を見る。
    /// timeout を超えたら `Err`。
    pub(super) fn pump_until<F>(&mut self, mut pred: F, timeout: Duration) -> Result<ChildEvent>
    where
        F: FnMut(&ChildEvent) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            match self.next_incoming()? {
                Some(msg) => {
                    if pred(&msg) {
                        return Ok(msg);
                    }
                }
                None => {
                    if Instant::now() >= deadline {
                        return Err(anyhow!("pump_until timed out"));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    /// pending な incoming events を `for_duration` だけ捌く。
    /// headless では `exportWav` の前に PluginLatencyChanged を取り込んで song を
    /// 最新化するために使う (export thread が song を snapshot する前に
    /// LoadSong を届けたい)。GUI では runner が常に捌いているので寝るだけ。
    /// 期間内に新規 event が無くても block しない。
    pub(super) fn drain_pending_for(&mut self, for_duration: Duration) {
        let deadline = Instant::now() + for_duration;
        while Instant::now() < deadline {
            match self.next_incoming() {
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
    }

    /// 次の 1 event を取り出す (無ければ `None`)。headless はここで app へ dispatch まで済ませる。
    fn next_incoming(&mut self) -> Result<Option<ChildEvent>> {
        match &mut self.link {
            Link::Headless(h) => {
                let HeadlessLink { bootstrap, app, side } = &mut **h;
                // tokio mpsc の try_recv は &mut self を要求。 split-borrow で
                // `audio_tx` と `incoming_rx` を別々に持つため、 receive ごとに
                // local 変数に取り出す。
                let recv = bootstrap
                    .incoming_rx
                    .as_mut()
                    .expect("Bootstrap.incoming_rx already taken (GUI mode)")
                    .try_recv();
                match recv {
                    Ok(msg) => {
                        side.observe(&msg);
                        dispatch_incoming_headless(app, &self.io, &msg);
                        // GUI の frame 末と同じ子プロセス sync (event の処理が Song を編集して
                        // いれば、次に送る命令より先に engine へ届く)。
                        app.flush_all_song_sync();
                        Ok(Some(msg))
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(None),
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        Err(anyhow!("incoming pipe closed"))
                    }
                }
            }
            Link::Gui(g) => Ok(g
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()),
        }
    }
}

/// headless で届いた event を production (GUI runner) と同じ経路で app へ dispatch する。
/// audio 側の完了通知で song を書き換える非同期フロー (bounce / `J` Glue の焼き込み) は、
/// これが無いと headless では**永久に完了しない** — script モードは実機と同じ経路を
/// 通ってこそ検証になる。 走行中でない export の完了等は app 側が stale ガードで無視する
/// (`handler::ipc`)。
fn dispatch_incoming_headless(app: &mut AppData, io: &ScriptIo, msg: &ChildEvent) {
    let msg = match msg {
        ChildEvent::Audio(ev) => {
            app.handle_event(AppEvent::Audio(ev.clone()));
            return;
        }
        ChildEvent::Plugin(msg) => msg,
    };
    match msg {
        // GUI runner と同じく app へ dispatch する。これで `loaded_devices` が
        // 埋まり、OpenPluginShmem 送信 + `sync_vocal_metadata` 再 flush
        // (= builtin VOICEVOX の歌唱/読み上げ合成 trigger) が走る。これが
        // 無いと、 slot ロード前の初回 flush が skip されたまま再 flush されず、
        // VOICEVOX を含む project の headless export が無音になる
        // (`docs/plan_voicevox_talk.md` §7 で talk export 検証時に発覚)。
        // `SlotPluginShmemReleased` は `ClosePluginShmem` を daw_audio へ転送させる
        // (無いと project 切替 / plugin 差し替えで daw_audio に stale mapping が残る)。
        // `SlotPluginLoadFailed` は `pending_plugin_loads` を解放する (= script の
        // 「全 load 完了」判定が失敗でも進む)。
        PluginEvent::SlotPluginLoaded { .. }
        | PluginEvent::SlotPluginShmemReleased { .. }
        | PluginEvent::SlotPluginLoadFailed { .. } => {
            app.handle_event(AppEvent::Plugin(msg.clone()));
        }
        PluginEvent::SlotPluginUnloaded { device: DeviceAddr { device_id, .. } } => {
            let _ = io.audio_tx.send(AudioCommand::SetDeviceLatency {
                project: app.pk(),
                device_id: *device_id,
                samples: 0,
            });
        }
        PluginEvent::PluginLatencyChanged { device: DeviceAddr { device_id, .. }, samples } => {
            // GUI mode と同じく **device 単位のまま** engine へ中継する
            // (track 合計は `compile_schedule` が導出する = 集計を二重に持たない)。
            let _ = io.audio_tx.send(AudioCommand::SetDeviceLatency {
                project: app.pk(),
                device_id: *device_id,
                samples: *samples,
            });
        }
        _ => {}
    }
}

/// v29: `(track_id, device_index)` → 安定 device id (app の song から)。
pub(super) fn device_id_at_app(app: &AppData, track_id: u32, index: u32) -> Option<u64> {
    crate::app::device_id_at(app.cur.song_doc.song(), track_id, index)
}

/// `HOST` を borrow_mut してクロージャを実行する短縮ヘルパ (script スレッド側の物だけ触る:
/// `io` / `script_args` / `pump_until` / `drain_pending_for`)。app に触るなら [`with_app`]。
pub(super) fn with_host<F, R>(f: F) -> R
where
    F: FnOnce(&mut ScriptHost) -> R,
{
    HOST.with_borrow_mut(|h| {
        let host = h.as_mut().expect("script host not initialized");
        f(host)
    })
}

/// app に触る 1 手 ([`ScriptHost::with_app`])。
pub(super) fn with_app<R, F>(f: F) -> R
where
    R: Send + 'static,
    F: FnOnce(&mut AppData, &mut ScriptSide, &ScriptIo) -> R + Send + 'static,
{
    with_host(|h| h.with_app(f))
}
