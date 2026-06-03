use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::audio_bridge::AudioBridgeHandle;
use common::protocol::{ChildToMain, MainToChild};
use tokio::sync::mpsc::UnboundedReceiver;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event_loop::EventLoopProxy;
use winit::window::WindowAttributes;

use daw_gui::app::{AppData, AppEvent};
use daw_gui::bootstrap::{Bootstrap, bootstrap_subprocess};
use daw_gui::dispatcher::{Win32JobDispatcher, WinitDispatcher};
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

fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_gui starting");

    let cli = parse_args()?;
    let bootstrap = bootstrap_subprocess()?;

    if let Some(script_path) = cli.script.as_ref() {
        tracing::info!(script = %script_path.display(), "headless script mode");
        return run_scripted(bootstrap, script_path, cli.output.as_deref(), &cli.extra);
    }

    run_gui(bootstrap, cli.smoke_test, cli.smoke_test_text)
}

fn run_gui(
    mut bootstrap: Bootstrap,
    smoke_test_fixture: Option<PathBuf>,
    #[cfg_attr(not(windows), allow(unused_variables))] smoke_test_text: bool,
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
        .with_inner_size(LogicalSize::new(init_state.width, init_state.height))
        .with_position(PhysicalPosition::new(init_state.x, init_state.y));
    if init_state.maximized {
        window_attrs = window_attrs.with_maximized(true);
    }
    let init = RunnerInit {
        window_attrs,
        build_app: Box::new(move |proxy: EventLoopProxy<AppEvent>| {
            let audio_tx_for_bridge = audio_tx.clone();

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
            spawn_incoming_bridge(incoming_rx, proxy.clone(), audio_tx_for_bridge);

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
    audio_tx: tokio::sync::mpsc::UnboundedSender<MainToChild>,
) {
    std::thread::spawn(move || {
        while let Some(msg) = rx.blocking_recv() {
            let event = match msg {
                ChildToMain::SlotGuiOpened { track, slot, width, height } => Some(
                    AppEvent::GuiOpenedFromChild { track, slot, width, height },
                ),
                ChildToMain::SlotGuiRequestResize { track, slot, width, height } => Some(
                    AppEvent::GuiRequestResizeFromChild { track, slot, width, height },
                ),
                ChildToMain::SlotGuiClosed { track, slot } => {
                    Some(AppEvent::GuiClosedFromChild { track, slot })
                }
                ChildToMain::SlotPluginLoaded {
                    track,
                    slot,
                    id,
                    name,
                    plugin_id,
                    shmem_id,
                    state_load_error,
                } => {
                    let _ = audio_tx.send(MainToChild::OpenPluginShmem {
                        plugin_id,
                        shmem_id,
                        track,
                        slot,
                    });
                    Some(AppEvent::SlotPluginLoadedFromChild {
                        track,
                        slot,
                        id,
                        name,
                        plugin_id,
                        state_load_error,
                    })
                }
                ChildToMain::SlotPluginState { .. } => None,
                ChildToMain::AllPluginStates { entries } => {
                    Some(AppEvent::AllStatesReceived(entries))
                }
                ChildToMain::SlotPluginUnloaded { plugin_id } => {
                    let _ = audio_tx.send(MainToChild::ClosePluginShmem { plugin_id });
                    Some(AppEvent::SlotPluginUnloadedFromChild { plugin_id })
                }
                ChildToMain::SlotPluginLoadFailed {
                    track,
                    slot,
                    plugin_id,
                    reason,
                } => Some(AppEvent::SlotPluginLoadFailedFromChild {
                    track,
                    slot,
                    plugin_id,
                    reason,
                }),
                ChildToMain::PluginLatencyChanged { plugin_id, samples } => {
                    Some(AppEvent::PluginLatencyChangedFromChild { plugin_id, samples })
                }
                ChildToMain::ExportWavComplete { error } => {
                    Some(AppEvent::ExportWavComplete { error })
                }
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
                ChildToMain::Hello { .. } => None,
                // Phase 2 (`docs/plan_automation.md` §7.5): plugin の
                // parameter 一覧 / touch / value change を AppEvent に
                // 変換して app に流す。 詳細 handler は app.rs 側。
                ChildToMain::PluginParamList {
                    track,
                    slot,
                    plugin_id,
                    params,
                } => Some(AppEvent::PluginParamListFromChild {
                    track,
                    slot,
                    plugin_id,
                    params,
                }),
                ChildToMain::PluginParamTouched {
                    track,
                    slot,
                    param_id,
                    display_name,
                } => Some(AppEvent::PluginParamTouchedFromChild {
                    track,
                    slot,
                    param_id,
                    display_name,
                }),
                ChildToMain::PluginParamValueChanged {
                    track,
                    slot,
                    param_id,
                    value,
                } => Some(AppEvent::PluginParamValueChangedFromChild {
                    track,
                    slot,
                    param_id,
                    value,
                }),
                ChildToMain::PluginParamGestureEnd {
                    track,
                    slot,
                    param_id,
                } => Some(AppEvent::PluginParamGestureEndFromChild {
                    track,
                    slot,
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
            bridge.track_peaks(&mut peaks_buf);
            if proxy
                .send_event(AppEvent::TrackPeaksTick(peaks_buf.clone()))
                .is_err()
            {
                break;
            }
        }
    });
}
