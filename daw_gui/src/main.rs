use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::audio_bridge::AudioBridgeHandle;
use common::protocol::{ChildToMain, MainToChild};
use tokio::sync::mpsc::UnboundedReceiver;
use winit::dpi::LogicalSize;
use winit::event_loop::EventLoopProxy;
use winit::window::WindowAttributes;

use daw_gui::app::{AppData, AppEvent};
use daw_gui::bootstrap::{Bootstrap, bootstrap_subprocess};
use daw_gui::dispatcher::{Win32JobDispatcher, WinitDispatcher};
use daw_gui::script::run_scripted;
use daw_gui::view::runner::{RunnerInit, run as run_runner};

/// CLI 引数。 GUI mode の場合は `script` / `output` ともに `None`。
struct CliArgs {
    script: Option<PathBuf>,
    output: Option<PathBuf>,
}

fn parse_args() -> Result<CliArgs> {
    let args: Vec<String> = std::env::args().collect();
    let mut script: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
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
            "--help" | "-h" => {
                println!("daw_gui [--script <path.js>] [--output <path.wav>]");
                std::process::exit(0);
            }
            _ => {
                tracing::warn!(arg = %args[i], "ignoring unknown argument");
            }
        }
        i += 1;
    }
    Ok(CliArgs { script, output })
}

fn main() -> Result<()> {
    common::logging::init_tracing();
    tracing::info!("daw_gui starting");

    let cli = parse_args()?;
    let bootstrap = bootstrap_subprocess()?;

    if let Some(script_path) = cli.script.as_ref() {
        tracing::info!(script = %script_path.display(), "headless script mode");
        return run_scripted(bootstrap, script_path, cli.output.as_deref());
    }

    run_gui(bootstrap)
}

fn run_gui(mut bootstrap: Bootstrap) -> Result<()> {
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

    let init = RunnerInit {
        window_attrs: WindowAttributes::default()
            .with_title("daw_01")
            .with_inner_size(LogicalSize::new(1280.0, 800.0)),
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
            );

            spawn_playhead_poller(bridge, proxy.clone());
            spawn_autosave_timer(proxy.clone());
            spawn_midi_input(proxy.clone());
            spawn_incoming_bridge(incoming_rx, proxy.clone(), audio_tx_for_bridge);

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
                ChildToMain::ExportWavComplete { error } => {
                    Some(AppEvent::ExportWavComplete { error })
                }
                ChildToMain::Hello { .. } => None,
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
            if proxy
                .send_event(AppEvent::Tick { samples, peak_l, peak_r })
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
