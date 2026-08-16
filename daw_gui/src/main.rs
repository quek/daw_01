// release ではコンソール窓を出さない (windows-subsystem)。 debug は console の
// まま (cargo run + ログ grep の動線を維持)。 docs/plan_icon_and_console.md (#48)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::audio_bridge::AudioBridgeHandle;
use common::metrics_bridge::MetricsBridgeHandle;
use daw_gui::bootstrap::ChildEvent;
use tokio::sync::mpsc::UnboundedReceiver;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event_loop::EventLoopProxy;
use winit::window::{Icon, WindowAttributes};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::bootstrap::{Bootstrap, bootstrap_subprocess};
use daw_gui::dispatcher::{Win32JobDispatcher, WinitDispatcher};
#[cfg(feature = "script")]
use daw_gui::script::run_scripted;
use daw_gui::view::runner::{RunnerInit, run as run_runner};

/// CLI 引数。 GUI mode の場合は `script` / `output` / `smoke_test` ともに `None`。
struct CliArgs {
    script: Option<PathBuf>,
    output: Option<PathBuf>,
    /// Free-form `--arg KEY=VALUE` pairs. Exposed to JS as
    /// `daw.scriptArgs[key]` so test scripts can take parameters.
    extra: Vec<(String, String)>,
    /// `--smoke-test <fixture.mp4>` の path。 Some の時は通常 GUI を起動
    /// しつつ background で [`daw_gui::smoke_test::spawn_orchestrator`] が
    /// programmatic に fixture を import → play → preview window capture →
    /// pixel histogram assertion → process::exit(0/1) する。
    /// `--script` とは相互排他。
    smoke_test: Option<PathBuf>,
    /// `--smoke-test-text` 引数なし flag。 true のとき
    /// [`daw_gui::smoke_test::spawn_text_overlay_orchestrator`] が
    /// track 追加 + AddTextClipAt → preview → Play → capture → text 描画
    /// histogram で assertion → process::exit(0/1)。 gui_01 Phase 78 runtime 検証用。
    /// 他の smoke / script 系と相互排他。
    smoke_test_text: bool,
}

fn parse_args() -> Result<CliArgs> {
    let args: Vec<String> = std::env::args().collect();
    let mut script: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut extra: Vec<(String, String)> = Vec::new();
    let mut smoke_test: Option<PathBuf> = None;
    let mut smoke_test_text = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--script" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--script needs a path"))?;
                script = Some(PathBuf::from(v));
            }
            "--output" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--output needs a path"))?;
                output = Some(PathBuf::from(v));
            }
            "--arg" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--arg needs KEY=VALUE"))?;
                let (k, val) = v
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--arg requires KEY=VALUE form (got {v:?})"))?;
                extra.push((k.to_string(), val.to_string()));
            }
            "--smoke-test" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("--smoke-test needs a path"))?;
                smoke_test = Some(PathBuf::from(v));
            }
            "--smoke-test-text" => {
                smoke_test_text = true;
            }
            "--help" | "-h" => {
                println!(
                    "daw_gui [--script <path.js>] [--output <path.wav>] [--arg KEY=VALUE]... \
                     [--smoke-test <fixture.mp4>] [--smoke-test-text]"
                );
                std::process::exit(0);
            }
            _ => {
                tracing::warn!(arg = %args[i], "ignoring unknown argument");
            }
        }
        i += 1;
    }
    if script.is_some() && (smoke_test.is_some() || smoke_test_text) {
        anyhow::bail!("--script and --smoke-test[-text] are mutually exclusive");
    }
    if smoke_test.is_some() && smoke_test_text {
        anyhow::bail!("--smoke-test and --smoke-test-text are mutually exclusive");
    }
    Ok(CliArgs {
        script,
        output,
        extra,
        smoke_test,
        smoke_test_text,
    })
}

/// メインウィンドウ左上 (タイトルバー) のアイコンを、 build.rs が
/// `OUT_DIR/window_icon.rgba` にラスタライズした 256x256 straight-RGBA から構築する。
/// `Icon::from_rgba` は straight (非 premultiplied) RGBA / 所有 `Vec<u8>` を要求する。
/// 構築失敗 (寸法不一致等) は握りつぶさず warn して None (= アイコン無し) で続行する。
/// exe / タスクバー / Alt+Tab のアイコンは build.rs の embed-resource 側 (別経路)。
fn window_icon() -> Option<Icon> {
    const ICON_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/window_icon.rgba"));
    // 寸法の SSoT は build.rs の WINDOW_ICON_SIZE 一本。 ここで再宣言して二重化せず、
    // build.rs が書いた square straight-RGBA バッファ長から edge を導出する
    // (4 byte/px の正方形なので edge = sqrt(len/4))。
    let edge = ((ICON_RGBA.len() / 4) as f64).sqrt() as u32;
    match Icon::from_rgba(ICON_RGBA.to_vec(), edge, edge) {
        Ok(icon) => Some(icon),
        Err(e) => {
            tracing::warn!(error = %e, "failed to build window icon; continuing without");
            None
        }
    }
}

fn main() -> Result<()> {
    let _log_guard = common::logging::init_tracing_for("daw_gui");
    tracing::info!("daw_gui starting");

    let cli = parse_args()?;

    // 対話 GUI は single-instance。 2 つ目の起動は既存ウィンドウを
    // 前面化して即終了する。 --script (WAV 書き出し) / --smoke-test[-text] (CI
    // 検証) は対象外 — 開発インスタンスを開いたまま並行実行できる必要があるため。
    // bootstrap_subprocess (子プロセス起動 / audio device open) より前に弾く。
    let interactive = cli.script.is_none() && cli.smoke_test.is_none() && !cli.smoke_test_text;
    let mut singleton_primary = false;
    let _singleton = if interactive {
        match daw_gui::single_instance::acquire() {
            Ok(daw_gui::single_instance::SingleInstance::AlreadyRunning) => {
                tracing::info!(
                    "daw_gui already running; brought the existing window to front, exiting"
                );
                return Ok(());
            }
            Ok(daw_gui::single_instance::SingleInstance::Primary(g)) => {
                singleton_primary = true;
                Some(g)
            }
            Err(e) => {
                tracing::warn!(error = ?e, "single-instance gate unavailable; continuing without it");
                None
            }
        }
    } else {
        None
    };

    let bootstrap = bootstrap_subprocess()?;

    if let Some(script_path) = cli.script.as_ref() {
        tracing::info!(script = %script_path.display(), "headless script mode");
        #[cfg(feature = "script")]
        return run_scripted(bootstrap, script_path, cli.output.as_deref(), &cli.extra);
        // boa_engine (JS エンジン) は default ビルドのコールド時間短縮のため除外している。
        // --script を使う headless テストは `--features script` を付けてビルドすること。
        #[cfg(not(feature = "script"))]
        {
            // output / extra は script モード専用。feature off では未使用なので明示的に
            // 読んで dead_code を回避しつつ、bootstrap (子プロセス所有) を畳んでから返す。
            let _ = (cli.output.as_ref(), cli.extra.len());
            drop(bootstrap);
            anyhow::bail!(
                "--script requires building daw_gui with `--features script` \
                 (the JS test driver / boa_engine is gated out of default builds to keep them fast)"
            );
        }
    }

    // `_singleton` は run_gui (= event loop) が返るまで保持し、 mutex を握り続ける。
    run_gui(bootstrap, cli.smoke_test, cli.smoke_test_text, singleton_primary)
}

fn run_gui(
    mut bootstrap: Bootstrap,
    smoke_test_fixture: Option<PathBuf>,
    #[cfg_attr(not(windows), allow(unused_variables))] smoke_test_text: bool,
    singleton_primary: bool,
) -> Result<()> {
    tracing::info!("opening main window");

    // GUI mode で必要な channel/handle を `Bootstrap` から取り出す。
    // private keep-alive (子プロセス / Win32 Handle / shmem) は `bootstrap`
    // 自身が握ったまま、 この関数 stack で `run_runner` 終了まで生かす。
    // Bootstrap drop で `JobHandle` も drop され、 Job Object 経由で
    // 子プロセスが kill される — 正しい shutdown 順序。
    let audio_tx = bootstrap.audio_tx.clone();
    let plugin_tx = bootstrap.plugin_tx.clone();
    let incoming_rx = bootstrap
        .take_incoming_rx()
        .expect("Bootstrap.incoming_rx already taken");
    let bridge = Arc::clone(&bootstrap.bridge);
    let metrics = Arc::clone(&bootstrap.metrics);
    // r.md #50: マスター出力サンプルのリング (テレメトリポーラの解析器が読む)。
    let scope = Arc::clone(&bootstrap.scope);
    let job = Arc::clone(&bootstrap.job);
    let plugin_db = bootstrap.plugin_db.clone();
    let supervisor = Arc::clone(&bootstrap.supervisor);
    // (A1 r.md #8) bootstrap が解決したデバイス実レート (Copy なので closure へ move)。
    let sample_rate = bootstrap.sample_rate;

    // 前回終了時の window geometry を復元。 存在しなければ default (1280×800)。
    // 位置は physical (= screen 座標)、 サイズは logical (= DPI 差吸収) で保存。
    // per-user データディレクトリの SSoT。 window_state load / AppData の
    // recent / recovery 永続化が全てここから解決される。
    let app_dirs = common::app_dirs::AppDirs::production();
    let saved_window = app_dirs
        .as_ref()
        .map(|d| d.window_state())
        .and_then(daw_gui::window_state::load);
    let init_state = saved_window.unwrap_or_default();
    let mut window_attrs = WindowAttributes::default()
        .with_title("daw_01")
        .with_window_icon(window_icon())
        .with_inner_size(LogicalSize::new(init_state.width, init_state.height))
        .with_position(PhysicalPosition::new(init_state.x, init_state.y));
    if init_state.maximized {
        window_attrs = window_attrs.with_maximized(true);
    }
    let init = RunnerInit {
        window_attrs,
        build_app: Box::new(move |proxy: EventLoopProxy<AppEvent>| {
            let event_dispatcher: Arc<dyn daw_gui::dispatcher::BackgroundDispatcher> =
                Arc::new(WinitDispatcher::new(proxy.clone()));
            let job_dispatcher: Arc<dyn daw_gui::dispatcher::JobDispatcher> =
                Arc::new(Win32JobDispatcher::new(job));
            let mut app = AppData::new(
                audio_tx,
                plugin_tx,
                None,
                plugin_db,
                event_dispatcher,
                job_dispatcher,
                Some(supervisor),
                app_dirs.clone(),
                sample_rate,
            );
            // resource monitor (r.md #3): per-plugin CPU の直接読み出し用に
            // MetricsBridge ハンドルを AppData へ持たせる。
            app.ipc.metrics_bridge = Some(Arc::clone(&metrics));
            // r.md #49: smoke test は preview 窓を `PrintWindow` で pixel capture して
            // 検証する。窓がフォーカスを得るかは実行環境次第なので、省電力で描画が
            // 止まると「真っ黒 = 視覚回帰」と誤検出しうる。検証対象は描画結果なので
            // この経路だけアクティブ判定を固定する。
            app.activity.force_active = smoke_test_fixture.is_some() || smoke_test_text;

            // r.md #49: 省電力中は背景 poller 自身に止まってもらう
            // (`AppData` 側が毎イベントで最新値を書き込む共有フラグ)。
            let awake = Arc::clone(&app.activity.awake);
            spawn_playhead_poller(
                bridge,
                Arc::clone(&metrics),
                scope,
                Arc::clone(&app.meter_control),
                proxy.clone(),
                Arc::clone(&awake),
            );
            spawn_resource_sysinfo_poller(proxy.clone(), awake);
            spawn_autosave_timer(proxy.clone());
            spawn_midi_input(proxy.clone());
            spawn_incoming_bridge(incoming_rx, proxy.clone());

            // primary インスタンスは、 2 つ目の起動からの前面化要求を
            // 待つ listener を立てる (event 受信 → RaiseMainWindow を event loop へ)。
            // smoke-test / script モードはゲート対象外なので primary フラグは立たない。
            if singleton_primary {
                let raise_proxy = proxy.clone();
                if let Err(e) = daw_gui::single_instance::spawn_raise_listener(move || {
                    let _ = raise_proxy.send_event(AppEvent::RaiseMainWindow);
                }) {
                    tracing::warn!(error = ?e, "failed to start single-instance raise listener");
                }
            }

            // `--smoke-test <fixture>` → background orchestrator drives
            // ImportVideo / TogglePreviewWindow / Play via the same
            // proxy and runs a pixel-histogram assertion against the
            // preview window. Process exits with 0 (pass) / 1 (fail)
            // when the orchestrator finishes — see `smoke_test.rs`.
            #[cfg(windows)]
            if let Some(fixture) = smoke_test_fixture {
                tracing::info!(
                    fixture = %fixture.display(),
                    "smoke test mode — orchestrator will exit the process on completion"
                );
                daw_gui::smoke_test::spawn_orchestrator(fixture, proxy.clone());
            }

            // `--smoke-test-text` → text overlay orchestrator drives
            // AddInstrumentTrack + AddTextClipAt → TogglePreviewWindow → Play
            // and asserts the preview shows visible text. Mutually exclusive with the
            // fixture-based smoke (= CLI parser rejects the combination).
            #[cfg(windows)]
            if smoke_test_text {
                tracing::info!(
                    "text overlay smoke test mode — orchestrator will exit the process on completion"
                );
                daw_gui::smoke_test::spawn_text_overlay_orchestrator(proxy.clone());
            }

            app
        }),
    };

    if let Err(e) = run_runner(init) {
        tracing::error!(error = ?e, "event loop error");
    }
    drop(bootstrap);
    tracing::info!("daw_gui exiting");
    Ok(())
}

// ----- 背景スレッド (GUI mode 専用) -----------------------------------------

fn spawn_incoming_bridge(
    mut rx: UnboundedReceiver<ChildEvent>,
    proxy: EventLoopProxy<AppEvent>,
) {
    std::thread::spawn(move || {
        while let Some(msg) = rx.blocking_recv() {
            // v29: pipe が型分割されたので bridge も 2 系統。 変換は従来どおり
            // 1 protocol event = 1 AppEvent を維持する。
            // direct-wrap: protocol event を丸ごと `AppEvent` に包んで app へ送る。
            // variant ごとの処理接続は `handler::ipc` の dispatch_* が担う
            // (旧 1:1 bridge audio_event_to_app / plugin_event_to_app を廃止)。
            let event = match msg {
                ChildEvent::Audio(ev) => AppEvent::Audio(ev),
                ChildEvent::Plugin(ev) => AppEvent::Plugin(ev),
            };
            if proxy.send_event(event).is_err() {
                break;
            }
        }
    });
}

fn spawn_midi_input(proxy: EventLoopProxy<AppEvent>) {
    let proxy_for_midi = proxy.clone();
    std::thread::spawn(move || {
        match daw_gui::midi::open_default_input(proxy_for_midi) {
            Ok(Some(handle)) => {
                let name = handle.port_name.clone();
                Box::leak(Box::new(handle));
                let _ = proxy.send_event(AppEvent::MidiInputOpened(Some(name)));
            }
            Ok(None) => {
                tracing::info!("no MIDI input ports available");
                let _ = proxy.send_event(AppEvent::MidiInputOpened(None));
            }
            Err(e) => {
                tracing::warn!(error = ?e, "failed to open MIDI input");
                let _ = proxy.send_event(AppEvent::MidiInputOpened(None));
            }
        }
    });
}

fn spawn_autosave_timer(proxy: EventLoopProxy<AppEvent>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(30));
            if proxy.send_event(AppEvent::AutosaveTick).is_err() {
                break;
            }
        }
    });
}

/// r.md #49: 省電力中の poll 間隔。
///
/// 完全に止めないのは `on_tick` に同居する 3 つの watchdog (panic 遅延 reinit /
/// 書き出し 60s / plugin state round-trip) を生かしておくため — いずれも閾値が
/// 秒オーダーなので数 Hz で足りる。
///
/// 1 秒まで伸ばさないのは**復帰の応答性**のため。フォーカスが戻っても、寝ている
/// スレッドは起きるまで間隔ぶん待つので、そのまま復帰後のメーター / プレイヘッドの
/// 遅れになる。値が変わらなければ再描画は起きない (指紋比較) ので、この頻度の
/// 起床自体はほぼ無コスト。30Hz のうち 7/8 の起床が消える。
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// リソースモニターの表示更新レート。ステータスバーの読み値 (整数パーセント) は
/// 毎 tick 揺れるので、30Hz で流すと **停止中でも DSP% が変わるだけで全画面を
/// 30fps 描き直す**ことになる。表示に必要なのは数 Hz なので tick を間引く
/// (REAPER が `Meter update frequency` を別設定に持っているのと同じ理由)。
const METRICS_TICK_DIVISOR: u32 = 8;

fn spawn_playhead_poller(
    bridge: Arc<AudioBridgeHandle>,
    metrics: Arc<MetricsBridgeHandle>,
    scope: Arc<common::scope_bridge::ScopeBridgeHandle>,
    meter_control: Arc<
        std::sync::Mutex<daw_gui::master_meter::settings::MeterControl>,
    >,
    proxy: EventLoopProxy<AppEvent>,
    awake: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut peaks_buf: Vec<(f32, f32)> = Vec::with_capacity(common::audio_bridge::MAX_TRACKS);
        let mut mod_buf: Vec<f32> = Vec::with_capacity(common::audio_bridge::MAX_MOD_SOURCES);
        // r.md #50: マスター出力サンプルのリング読み手と、そこから全メーターを
        // 導く解析器。ここが唯一の読み手 (単一 reader 前提のカーソル)。
        let mut scope_reader = scope.reader();
        let mut scope_buf: Vec<[f32; 2]> =
            Vec::with_capacity(common::scope_bridge::max_read_frames(scope.sample_rate()));
        let mut analyzer =
            daw_gui::master_meter::MasterAnalyzer::new(scope.sample_rate());
        let mut last_meter_at = std::time::Instant::now();
        let mut tick_count: u32 = 0;
        loop {
            let awake_now = awake.load(std::sync::atomic::Ordering::Acquire);
            std::thread::sleep(if awake_now {
                Duration::from_millis(33)
            } else {
                IDLE_POLL_INTERVAL
            });
            tick_count = tick_count.wrapping_add(1);
            let samples = bridge.playhead_samples();
            let preroll = bridge.preroll_remaining();
            // r.md #51: 「走っているか」「録音してよいか」は engine が所有する
            // 事実。GUI は他の telemetry と同じ面で観測する。
            let playing = bridge.playing();
            if proxy
                .send_event(AppEvent::Tick {
                    samples,
                    preroll,
                    playing,
                    recording_live: bridge.recording_live(),
                })
                .is_err()
            {
                break;
            }
            // r.md #50: マスターメーター。パネルが閉じているときは解析ごと止める
            // (リングは読み捨ててカーソルだけ進め、再表示で古い音が流れ込まない
            // ようにする)。
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(last_meter_at).as_secs_f32();
            last_meter_at = now;
            scope_buf.clear();
            let outcome = scope_reader.read(&scope, &mut scope_buf);
            let control = meter_control.lock().ok().map(|c| daw_gui::master_meter::settings::MeterControl {
                settings: c.settings,
                loudness_reset_epoch: c.loudness_reset_epoch,
                peak_reset_epoch: c.peak_reset_epoch,
                active: c.active,
            });
            if let Some(control) = control
                && control.active
            {
                // r.md #57: 停止したら積算を止める (EBU Tech 3341 §2.2 の
                // running / stand-by)。count-in 中は engine が scope リングへ何も
                // 書かない = プログラムではないので running に含めない。
                let rolling = playing && preroll == 0;
                let snapshot = analyzer
                    .tick(
                        &control,
                        scope.sample_rate(),
                        &scope_buf,
                        elapsed,
                        outcome.dropped > 0,
                        rolling,
                    )
                    .clone();
                if proxy
                    .send_event(AppEvent::MasterMeterTick(Box::new(snapshot)))
                    .is_err()
                {
                    break;
                }
            }
            // docs/plan_modulation.md §4.2: poll the modulation scalars on the
            // same ~30Hz tick as peaks and stream them to the model so visual
            // modulation (image / group / video fx) can apply per frame.
            bridge.mod_scalars(&mut mod_buf);
            if proxy
                .send_event(AppEvent::ModScalarsTick(std::mem::take(&mut mod_buf)))
                .is_err()
            {
                break;
            }
            bridge.track_peaks(&mut peaks_buf);
            // `peaks_buf` を毎 tick clone せず move でイベントに渡す。 次 tick の
            // `track_peaks` が `out.clear()` + push で再充填するので、 take 後に
            // 空になっても問題ない。 clone の memcpy を省く効果のみ (take は
            // capacity ごと move out するので次 tick で確保し直す = per-tick の
            // alloc 回数自体は不変。 30Hz の background thread なので無害)。
            if proxy
                .send_event(AppEvent::TrackPeaksTick(std::mem::take(&mut peaks_buf)))
                .is_err()
            {
                break;
            }
            // resource monitor (r.md #3): DSP load (peak は swap でリセット) /
            // xrun / buffer を読み UI へ流す。
            // r.md #49: 表示レートは playhead より低くてよいので間引く。
            if !tick_count.is_multiple_of(METRICS_TICK_DIVISOR) {
                continue;
            }
            let (buffer_frames, sample_rate) = metrics.buffer_info();
            if proxy
                .send_event(AppEvent::MetricsTick {
                    dsp_load_peak: metrics.take_dsp_load_peak(),
                    dsp_load_avg: metrics.dsp_load_avg(),
                    xrun_count: metrics.xrun_count(),
                    buffer_frames,
                    sample_rate,
                })
                .is_err()
            {
                break;
            }
        }
    });
}

/// resource monitor (r.md #3): daw_01 セッション (daw_gui + 子プロセス群) の
/// system CPU% と常駐メモリを sysinfo で ~1Hz ポーリングし UI へ流す。 DSP load
/// とは別物の「アプリ全体の重さ」。 RT パス外の専用スレッド。
fn spawn_resource_sysinfo_poller(
    proxy: EventLoopProxy<AppEvent>,
    awake: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let self_pid = sysinfo::get_current_pid().ok();
        let mut sys = sysinfo::System::new();
        loop {
            std::thread::sleep(Duration::from_millis(1000));
            // r.md #49: `refresh_processes(All)` は**全プロセス列挙**で、この
            // poller の中で圧倒的に重い。省電力中は読み手 (リソースモニター) が
            // 見えていないので、送るのを止めるのではなく **poll 自体をしない**。
            if !awake.load(std::sync::atomic::Ordering::Acquire) {
                continue;
            }
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let Some(self_pid) = self_pid else {
                continue;
            };
            // 自プロセス + その子 (daw_audio / daw_plugin_host / VOICEVOX engine)
            // を合算 = daw_01 セッション全体の負荷。
            let mut cpu = 0.0f32;
            let mut mem_bytes = 0u64;
            for proc in sys.processes().values() {
                if proc.pid() == self_pid || proc.parent() == Some(self_pid) {
                    cpu += proc.cpu_usage();
                    mem_bytes += proc.memory();
                }
            }
            // sysinfo の cpu_usage は 1 コア = 100%。 システム全体に対する % へ正規化。
            let n_cpus = sys.cpus().len().max(1) as f32;
            if proxy
                .send_event(AppEvent::SystemMetricsTick {
                    cpu: cpu / n_cpus,
                    mem_mb: mem_bytes as f32 / (1024.0 * 1024.0),
                })
                .is_err()
            {
                break;
            }
        }
    });
}
