//! デバイスの制御スレッド。デバイスを開く・鳴らす・止める (アイドル park)・開き直す・閉じる、ASIO の
//! コントロールパネルを開く — **デバイスに触る操作は全部このスレッド 1 本**で行う。
//!
//! ASIO ドライバーは作ったスレッドに縛られ (`Driver` は `!Send`)、そのスレッドは COM の STA で message を
//! 回し続けていなければならない (`crate::asio` の module doc)。OS 標準 (cpal) も同じスレッドで扱うので、
//! 「誰が stream を触るか」は 1 か所に決まる (旧実装は park を要求する側と解除する側を Mutex で直列化していた)。
//!
//! 他のスレッドは [`DeviceControl`] から依頼を送る。待ちは有界 ([`OPEN_TIMEOUT`] / [`SHUTDOWN_TIMEOUT`])。
//! スレッドは 100 ms ごと (と依頼が来たとき) に:
//! - アイドル park の要求 (`SharedState::park_requested`) へ stream を寄せる (要求 → 追従。r.md #49)。
//! - ドライバー側の変更を拾い (`Backend::poll`)、同じ条件で続けられるなら開き直してデバイス情報を送り直す。
//!   続けられない (レートや入力 ch が変わった) なら daw_gui に起こし直しを頼む (`AudioEvent::DeviceRestartRequired`
//!   — 切り替えの経路は daw_audio の起こし直し 1 本)。
//!
//! ドライバーがコントロールパネルを modal で出す間、このスレッドはその中に居る (park の追従と終了の依頼は
//! パネルを閉じるまで待つ。終了は [`SHUTDOWN_TIMEOUT`] で見切ってプロセスごと終わる)。

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::audio_device::{AudioDeviceConfig, AudioDeviceInfo};
use common::metrics_bridge::MetricsBridgeHandle;
use common::protocol::AudioEvent;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx,
    PM_REMOVE, PeekMessageW, QS_ALLINPUT, SetForegroundWindow, TranslateMessage, WINDOW_EX_STYLE, WS_POPUP,
};
use windows::core::w;

use super::callback::{CallbackSlot, EngineCallback};
use super::{Backend, Change, MetricsCell, OpenCtx};
use crate::engine::SharedState;

/// 周期処理の間隔。
const TICK: Duration = Duration::from_millis(100);
/// デバイスを開くのを待つ上限 (ASIO ドライバーの初期化は秒単位かかることがある。daw_gui の handshake の
/// 上限より短く)。
const OPEN_TIMEOUT: Duration = Duration::from_secs(12);
/// 終了でスレッドを待つ上限。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// この時間内の開き直しがこの回数を超えたら、それ以上は開き直さない (開くたびに開き直しを求めるドライバー)。
const REOPEN_WINDOW: Duration = Duration::from_secs(10);
const REOPEN_LIMIT: usize = 3;

/// engine をデバイスへつなぐときに渡すもの。
pub struct Install {
    pub callback: EngineCallback,
    pub shared: Arc<SharedState>,
    pub metrics: Arc<MetricsBridgeHandle>,
    pub events: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
}

enum Request {
    Open { config: Box<AudioDeviceConfig>, reply: SyncSender<AudioDeviceInfo> },
    Install(Box<Install>),
    ControlPanel,
    /// park の要求が取り下げられた。周期を待たずに追従する。
    Wake,
    Shutdown,
}

/// 制御スレッドを起こす event (kernel object なのでどのスレッドから触ってもよい)。
struct WakeEvent(HANDLE);

// SAFETY: event の handle は kernel object で、`SetEvent` / 待ちはスレッドを選ばない。
unsafe impl Send for WakeEvent {}
// SAFETY: 同上。
unsafe impl Sync for WakeEvent {}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        // SAFETY: `spawn` が作った handle を 1 回だけ閉じる。
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// 制御スレッドへの口。
pub struct DeviceControl {
    tx: Sender<Request>,
    wake: Arc<WakeEvent>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl DeviceControl {
    pub fn spawn() -> anyhow::Result<Self> {
        // SAFETY: 名前なしの auto-reset event を作るだけ。
        let wake = Arc::new(WakeEvent(unsafe { CreateEventW(None, false, false, None) }?));
        let (tx, rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let thread_wake = Arc::clone(&wake);
        let thread = std::thread::Builder::new().name("audio device control".into()).spawn(move || {
            run(&rx, &thread_wake);
            let _ = done_tx.send(());
        })?;
        Ok(Self { tx, wake, done, thread: Some(thread) })
    }

    fn send(&self, request: Request) {
        if self.tx.send(request).is_ok() {
            // SAFETY: 生きている event の handle。
            let _ = unsafe { SetEvent(self.wake.0) };
        }
    }

    /// 設定のデバイスを開く (まだ鳴らさない)。必ず何かが開く (`super::open`)。待ちは [`OPEN_TIMEOUT`] まで。
    pub fn open(&self, config: AudioDeviceConfig) -> anyhow::Result<AudioDeviceInfo> {
        let (reply, opened) = mpsc::sync_channel(1);
        self.send(Request::Open { config: Box::new(config), reply });
        opened
            .recv_timeout(OPEN_TIMEOUT)
            .map_err(|e| anyhow::anyhow!("audio device did not open within {OPEN_TIMEOUT:?}: {e}"))
    }

    /// engine をつないで鳴らし始める。
    pub fn install(&self, install: Install) {
        self.send(Request::Install(Box::new(install)));
    }

    pub fn open_control_panel(&self) {
        self.send(Request::ControlPanel);
    }

    /// park の要求を取り下げた後に呼ぶ (周期を待たずに stream を起こす)。
    pub fn wake(&self) {
        self.send(Request::Wake);
    }

    /// デバイスを閉じてスレッドを畳む。[`SHUTDOWN_TIMEOUT`] で見切る (ドライバーの中で止まっていても、
    /// プロセスは終われる)。
    pub fn shutdown(mut self) {
        self.send(Request::Shutdown);
        match self.done.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(()) => {
                if let Some(t) = self.thread.take()
                    && t.join().is_err()
                {
                    tracing::error!("audio device control thread panicked");
                }
            }
            Err(_) => tracing::error!("audio device control thread did not finish in time; exiting anyway"),
        }
    }
}

/// ASIO の `sysRef` に渡す隠し窓 (JUCE と同じく、表示しない自前の窓を渡す)。
///
/// daw_gui の本体窓は渡さない (`docs/plan_audio_io.md`「コントロールパネル」): パネルの窓を作るのはドライバーで、
/// 所有関係の契約 (`docs/plan_plugin_editor_topwindow.md` §窓契約 1-1) をこちらで守れない。modal のパネルは
/// 親を無効にするので、他プロセスの窓を渡すと daw_audio が落ちたとき daw_gui の窓が無効のまま残る。
struct SysWindow(HWND);

impl SysWindow {
    fn create() -> Option<Self> {
        // SAFETY: 既定の window class ("STATIC") で、表示しない top-level の窓を作るだけ。
        let created = unsafe {
            CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!("daw_audio"), WS_POPUP, 0, 0, 0, 0, None, None, None, None)
        };
        match created {
            Ok(hwnd) => Some(Self(hwnd)),
            Err(e) => {
                tracing::warn!(error = ?e, "failed to create the ASIO sysRef window");
                None
            }
        }
    }

    fn raw(window: Option<&Self>) -> *mut c_void {
        window.map_or(std::ptr::null_mut(), |w| w.0.0)
    }
}

impl Drop for SysWindow {
    fn drop(&mut self) {
        // SAFETY: このスレッドが作った窓。
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// このスレッド宛ての message を全部配る (STA の決まり)。
fn pump_messages() {
    let mut message = MSG::default();
    // SAFETY: `message` は有効な出力先。このスレッド宛ての message だけを取り出して配る。
    unsafe {
        while PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

fn run(rx: &Receiver<Request>, wake: &WakeEvent) {
    // COM の STA。失敗 (既に別方式で初期化済み) なら ASIO は開けないが、OS 標準は cpal が自分で初期化する。
    #[cfg(all(windows, target_pointer_width = "64"))]
    let _apartment = crate::asio::driver::ComApartment::enter()
        .inspect_err(|e| tracing::warn!(error = %e, "audio device control thread: COM STA init failed"))
        .ok();
    let window = SysWindow::create();
    let mut control = Control {
        slot: Arc::new(Mutex::new(None)),
        metrics: Arc::new(OnceLock::new()),
        sys_window: SysWindow::raw(window.as_ref()),
        backend: None,
        config: AudioDeviceConfig::default(),
        info: AudioDeviceInfo::default(),
        shared: None,
        events: None,
        running: false,
        restart_asked: false,
        reopens: Vec::new(),
    };
    let mut last_tick = Instant::now();
    'serve: loop {
        let wait = TICK.saturating_sub(last_tick.elapsed()).as_millis() as u32;
        // SAFETY: 生きている event の handle 1 つ。message が来ても起きる (STA の決まり)。
        unsafe { MsgWaitForMultipleObjectsEx(Some(&[wake.0]), wait, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        pump_messages();
        loop {
            match rx.try_recv() {
                Ok(Request::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => break 'serve,
                Ok(request) => control.handle(request),
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            control.tick();
        }
    }
    control.close();
    // 窓はデバイス (ドライバー) を閉じた後に壊す。
    drop(window);
}

/// 制御スレッドの持ち物。
struct Control {
    slot: CallbackSlot,
    metrics: MetricsCell,
    sys_window: *mut c_void,
    backend: Option<Backend>,
    /// 開いたときの設定 (開き直しに使う)。
    config: AudioDeviceConfig,
    info: AudioDeviceInfo,
    /// engine をつないだ後に `Some`。
    shared: Option<Arc<SharedState>>,
    events: Option<tokio::sync::mpsc::UnboundedSender<AudioEvent>>,
    /// stream を動かしているか (park していないか)。
    running: bool,
    /// daw_gui に起こし直しを頼み済み (同じことを何度も頼まない)。
    restart_asked: bool,
    /// 直近の開き直しの時刻 ([`REOPEN_LIMIT`])。
    reopens: Vec<Instant>,
}

impl Control {
    fn ctx(&self) -> OpenCtx {
        OpenCtx { slot: Arc::clone(&self.slot), metrics: Arc::clone(&self.metrics), sys_window: self.sys_window }
    }

    fn handle(&mut self, request: Request) {
        match request {
            Request::Open { config, reply } => {
                // 開き直すなら、先に古い方を閉じる (同じデバイスを二重に開かない)。
                self.backend = None;
                self.running = false;
                let (backend, info) = super::open(&config, &self.ctx());
                self.backend = Some(backend);
                self.config = *config;
                self.info = info.clone();
                let _ = reply.send(info);
            }
            Request::Install(install) => {
                let Install { callback, shared, metrics, events } = *install;
                let _ = self.metrics.set(metrics);
                *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(callback));
                self.shared = Some(shared);
                self.events = Some(events);
                self.publish_latency();
                self.reconcile_park();
            }
            Request::ControlPanel => self.open_control_panel(),
            Request::Wake => self.reconcile_park(),
            // `run` が先に拾う。
            Request::Shutdown => {}
        }
    }

    fn tick(&mut self) {
        self.reconcile_park();
        let Some(backend) = self.backend.as_mut() else { return };
        let metrics = self.metrics.get().map(Arc::as_ref);
        match backend.poll(&mut self.info, metrics) {
            Change::None => {}
            Change::Info => {
                self.publish_latency();
                self.send_info();
            }
            Change::Reopen(reason) => self.reopen(reason),
            Change::Lost(reason) => self.ask_restart(reason),
        }
    }

    /// stream を park の要求 (`SharedState::park_requested`) へ寄せる。engine をつなぐまでは鳴らさない。
    fn reconcile_park(&mut self) {
        let (Some(shared), Some(backend)) = (&self.shared, self.backend.as_mut()) else { return };
        let run = !shared.park_requested.load(Ordering::Acquire);
        if run == self.running {
            return;
        }
        match backend.set_running(run) {
            Ok(()) => {
                self.running = run;
                tracing::info!(parked = !run, "audio stream park state changed");
            }
            // 状態は変えないので、次の周期でもう一度試す。
            Err(e) => tracing::error!(error = %e, want_running = run, "failed to change audio stream park state"),
        }
    }

    /// RT が毎 buffer 読むレイテンシ (`DeviceIo`) を今のデバイス情報に合わせる。
    fn publish_latency(&self) {
        if let Some(shared) = &self.shared {
            shared.input_latency_frames.store(self.info.input_latency_frames, Ordering::Relaxed);
            shared.output_latency_frames.store(self.info.output_latency_frames, Ordering::Relaxed);
        }
    }

    fn send_info(&self) {
        if let Some(events) = &self.events {
            let _ = events.send(AudioEvent::DeviceInfo(Box::new(self.info.clone())));
        }
    }

    fn ask_restart(&mut self, reason: String) {
        if self.restart_asked {
            return;
        }
        self.restart_asked = true;
        tracing::warn!(%reason, "audio device needs a restart of daw_audio");
        if let Some(events) = &self.events {
            let _ = events.send(AudioEvent::DeviceRestartRequired { reason });
        }
    }

    /// ドライバーの求めで、同じ設定のまま開き直す。開けたものが今の session と合えば続け、合わなければ
    /// daw_gui に起こし直しを頼む (engine のレートと入力 ch はプロセスの間変えられない)。
    fn reopen(&mut self, reason: String) {
        let now = Instant::now();
        self.reopens.retain(|t| now.duration_since(*t) < REOPEN_WINDOW);
        if self.reopens.len() >= REOPEN_LIMIT {
            tracing::warn!(%reason, "audio driver keeps asking for a reset; ignoring");
            return;
        }
        self.reopens.push(now);
        tracing::info!(%reason, "reopening the audio device");
        // 古い方を閉じ切ってから開く (ドライバーは同時に 1 つ)。
        self.backend = None;
        self.running = false;
        let (backend, info) = super::open(&self.config, &self.ctx());
        let same_session = info.driver == self.info.driver
            && info.sample_rate == self.info.sample_rate
            && info.active_inputs == self.info.active_inputs
            && info.fallback_reason.is_none();
        if !same_session {
            // 開いたものは閉じる (合わないレートで鳴らさない)。daw_gui が起こし直すまで無音。
            drop(backend);
            self.ask_restart(reason);
            return;
        }
        self.backend = Some(backend);
        self.info = info;
        self.publish_latency();
        self.reconcile_park();
        self.send_info();
    }

    fn open_control_panel(&mut self) {
        let Some(backend) = &self.backend else { return };
        // パネルを前に出す: daw_gui が前面プロセスの権利を譲ってくれている (`AllowSetForegroundWindow`) ので、
        // 自分の隠し窓を前面にしてからドライバーにパネルを作らせる (tray icon がメニューを出す前にやるのと同じ)。
        if !self.sys_window.is_null() {
            // SAFETY: このスレッドが作った窓。
            let granted = unsafe { SetForegroundWindow(HWND(self.sys_window)) }.as_bool();
            tracing::info!(granted, "foreground for the driver control panel");
        }
        if let Err(e) = backend.control_panel() {
            tracing::warn!(error = %e, "failed to open the driver control panel");
        }
    }

    /// デバイスを閉じ、engine (描き手・描画スレッド) を片付ける。
    fn close(&mut self) {
        if let Some(backend) = self.backend.as_mut()
            && self.running
            && let Err(e) = backend.set_running(false)
        {
            tracing::warn!(error = %e, "failed to stop the audio stream for shutdown");
        }
        // ここでデバイスが実際に解放される。
        self.backend = None;
        tracing::info!("audio stream released");
        let callback = self.slot.lock().unwrap_or_else(|p| p.into_inner()).take();
        drop(callback);
    }
}
