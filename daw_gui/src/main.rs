// release ではコンソール窓を出さない (windows-subsystem)。 debug は console の
// まま (cargo run + ログ grep の動線を維持)。 docs/plan_icon_and_console.md (#48)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::audio_bridge::AudioBridgeHandle;
use common::protocol::ChildToMain;
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

    // FIXME #27: 対話 GUI は single-instance。 2 つ目の起動は既存ウィンドウを
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
    let job = Arc::clone(&bootstrap.job);
    let plugin_db = bootstrap.plugin_db.clone();
    let supervisor = Arc::clone(&bootstrap.supervisor);

    // 前回終了時の window geometry を復元。 存在しなければ default (1280×800)。
    // 位置は physical (= screen 座標)、 サイズは logical (= DPI 差吸収) で保存。
    // per-user データディレクトリの SSoT。 window_state load / AppData の
    // recent / recovery 永続化が全てここから解決される。
    let app_dirs = common::app_dirs::AppDirs::production();
    let saved_window = app_dirs
        .as_ref()
        .map(|d| d.window_state())
        .and_then(common::window_state::load);
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
            let app = AppData::new(
                audio_tx,
                plugin_tx,
                None,
                plugin_db,
                event_dispatcher,
                job_dispatcher,
                Some(supervisor),
                app_dirs.clone(),
            );

            spawn_playhead_poller(bridge, proxy.clone());
            spawn_autosave_timer(proxy.clone());
            spawn_midi_input(proxy.clone());
            spawn_incoming_bridge(incoming_rx, proxy.clone());

            // FIXME #27: primary インスタンスは、 2 つ目の起動からの前面化要求を
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
    mut rx: UnboundedReceiver<ChildToMain>,
    proxy: EventLoopProxy<AppEvent>,
) {
    std::thread::spawn(move || {
        while let Some(msg) = rx.blocking_recv() {
            let event = match msg {
                ChildToMain::SlotGuiOpened { track, index, width, height } => Some(
                    AppEvent::GuiOpenedFromChild { track, index, width, height },
                ),
                ChildToMain::SlotGuiClosed { track, index } => {
                    Some(AppEvent::GuiClosedFromChild { track, index })
                }
                ChildToMain::SlotPluginLoaded {
                    track,
                    index,
                    id,
                    name,
                    plugin_id,
                    shmem_id,
                    state_load_error,
                } => {
                    // SSoT: `OpenPluginShmem` は AppData が live な
                    // `self.audio_tx` (respawn で差し替わる側) から送る。
                    // ここで bootstrap 時点の stale clone に直接送ると、
                    // audio engine respawn 後にロードした plugin の shmem が
                    // 開かれず無言で音が出なくなる。 shmem_id を AppEvent まで
                    // 運び、 handler (on_plugin_loaded_from_child) で送る。
                    Some(AppEvent::SlotPluginLoadedFromChild {
                        track,
                        index,
                        id,
                        name,
                        plugin_id,
                        shmem_id,
                        state_load_error,
                    })
                }
                ChildToMain::SlotPluginState { .. } => None,
                ChildToMain::AllPluginStates { entries } => {
                    Some(AppEvent::AllStatesReceived(entries))
                }
                ChildToMain::SlotPluginUnloaded { plugin_id } => {
                    // SSoT: `ClosePluginShmem` も AppData が live な audio_tx
                    // から送る (on_plugin_unloaded_from_child)。 stale clone に
                    // 直接送ると respawn 後に dangling shmem 参照が残る。
                    Some(AppEvent::SlotPluginUnloadedFromChild { plugin_id })
                }
                ChildToMain::SlotPluginLoadFailed {
                    track,
                    index,
                    plugin_id,
                    reason,
                } => Some(AppEvent::SlotPluginLoadFailedFromChild {
                    track,
                    index,
                    plugin_id,
                    reason,
                }),
                ChildToMain::PluginLatencyChanged { plugin_id, samples } => {
                    Some(AppEvent::PluginLatencyChangedFromChild { plugin_id, samples })
                }
                ChildToMain::ExportWavComplete { error, cancelled } => {
                    Some(AppEvent::ExportWavComplete { error, cancelled })
                }
                ChildToMain::ExportWavProgress { done, total } => {
                    Some(AppEvent::ExportWavProgress { done, total })
                }
                ChildToMain::PluginsReinitDone => Some(AppEvent::PluginsReinitDone),
                ChildToMain::BounceClipFxComplete {
                    path,
                    source_track,
                    source_clip,
                    error,
                    frames,
                } => Some(AppEvent::BounceClipFxComplete {
                    path,
                    source_track,
                    source_clip,
                    error,
                    frames,
                }),
                ChildToMain::VocalSynthReady { plugin_id } => {
                    Some(AppEvent::VocalSynthReady { plugin_id })
                }
                ChildToMain::VoicevoxSynthStatus { plugin_id, busy, failing } => {
                    Some(AppEvent::VoicevoxSynthStatus { plugin_id, busy, failing })
                }
                ChildToMain::Hello { .. } => None,
                // Phase 2 (`docs/plan_automation.md` §7.5): plugin の
                // parameter 一覧 / touch / value change を AppEvent に
                // 変換して app に流す。 詳細 handler は app.rs 側。
                ChildToMain::PluginParamList {
                    track,
                    index,
                    plugin_id,
                    params,
                    has_embedded_gui,
                } => Some(AppEvent::PluginParamListFromChild {
                    track,
                    index,
                    plugin_id,
                    params,
                    has_embedded_gui,
                }),
                ChildToMain::PluginParamTouched {
                    track,
                    index,
                    param_id,
                    display_name,
                } => Some(AppEvent::PluginParamTouchedFromChild {
                    track,
                    index,
                    param_id,
                    display_name,
                }),
                ChildToMain::PluginParamValueChanged {
                    track,
                    index,
                    param_id,
                    value,
                } => Some(AppEvent::PluginParamValueChangedFromChild {
                    track,
                    index,
                    param_id,
                    value,
                }),
                ChildToMain::PluginParamGestureEnd {
                    track,
                    index,
                    param_id,
                } => Some(AppEvent::PluginParamGestureEndFromChild {
                    track,
                    index,
                    param_id,
                }),
                ChildToMain::ChildDisconnected { kind } => {
                    Some(AppEvent::ChildDisconnected { kind })
                }
            };
            if let Some(event) = event
                && proxy.send_event(event).is_err()
            {
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

fn spawn_playhead_poller(bridge: Arc<AudioBridgeHandle>, proxy: EventLoopProxy<AppEvent>) {
    std::thread::spawn(move || {
        let mut peaks_buf: Vec<(f32, f32)> = Vec::with_capacity(common::audio_bridge::MAX_TRACKS);
        let mut mod_buf: Vec<f32> = Vec::with_capacity(common::audio_bridge::MAX_MOD_SOURCES);
        loop {
            std::thread::sleep(Duration::from_millis(33));
            let samples = bridge.playhead_samples();
            let (peak_l, peak_r) = bridge.peaks();
            let preroll = bridge.preroll_remaining();
            if proxy
                .send_event(AppEvent::Tick {
                    samples,
                    peak_l,
                    peak_r,
                    preroll,
                })
                .is_err()
            {
                break;
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
        }
    });
}
