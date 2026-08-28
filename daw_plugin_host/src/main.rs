// release ではコンソール窓を出さない (windows-subsystem)。 debug は console の
// まま (standalone 起動時に stdout/tracing が見える)。 docs/plan_icon_and_console.md (#48)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! daw_plugin_host — CLAP / VST3 / builtin plugin をホストする子プロセス。
//!
//! v29 (`docs/plan_arch_refactor.md` §6): bookkeeping は
//! **安定 `device_id: u64` keyed の単一 `HashMap<u64, InstanceRecord>`** に
//! 一本化。旧 `(track, index)` positional キーの並行 4 map
//! (plugin_lookup / loaded_id_for_slot / loaded_meta_for_slot /
//! editor_windows) と、削除 / 並べ替え時の再キー儀式 (shift / permute) は
//! 全て削除 — reorder は Song 編集だけで完結し、plugin_host は順序概念を
//! 持たない (処理順は daw_audio の schedule が Song から compile する)。

mod ara;
mod builtin;
mod clap_scan;
mod plugin_scan;
mod vst3_scan;
mod clap_host;
mod clap_plugin;
mod editor_keys;
mod editor_window;
mod plugin_instance;
mod process_scaffold;
mod process_server;
mod vst3_events;
mod vst3_host;
mod vst3_params;
mod vst3_plugin;
mod vst3_stream;

use std::collections::{HashMap, HashSet};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use common::plugin_format::PluginFormat;
use common::plugin_ref::process_data_shmem_id;
use common::protocol::{AudioSession, PluginCommand, PluginEvent, SlotState};
use common::wire::{read_msg, write_msg};
use tokio::net::windows::named_pipe::NamedPipeClient;
use tokio::sync::mpsc as tmpsc;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, IsChild, KillTimer, MSG, PM_REMOVE, PeekMessageW,
    PostThreadMessageW, SetTimer, TranslateMessage, WM_APP, WM_TIMER,
};

use crate::plugin_instance::{HostCallbacks, LoadedPlugin, load_plugin};
use crate::process_server::{
    METRIC_SLOT_UNCLAIMED, PluginEntry, PluginRegistry, registry_insert, registry_remove,
    registry_restore_all, registry_take_all,
};

/// Custom Win32 message id used to wake the plugin-main thread's `GetMessage`
/// loop after a command has been pushed into the mpsc queue.
const WM_COMMAND_WAKE: u32 = WM_APP + 1;

/// (r.md #5 ARA2) Thread-timer id + interval used by the plugin-main thread to
/// pump every loaded ARA document's `notifyModelUpdates`.
const ARA_NOTIFY_TIMER_ID: usize = 1;
const ARA_NOTIFY_TIMER_MS: u32 = 30;

/// plugin 発 restart 要求の per-plugin cooldown 窓 (Melda 型 reinit 無限
/// ループの構造的防御): この窓内に [`RESTART_MAX_IN_WINDOW`] 回を超える
/// 要求は無視 + warn。
const RESTART_WINDOW: Duration = Duration::from_secs(10);
const RESTART_MAX_IN_WINDOW: usize = 3;

// ====================================================================
// plugin-main thread bookkeeping
// ====================================================================

/// per-plugin restart cooldown (10 秒窓 / 3 回)。
#[derive(Default)]
struct RestartWindowTracker {
    times: VecDeque<Instant>,
}

impl RestartWindowTracker {
    /// この restart 要求を実行してよいか。許可時は履歴に記録する。
    fn allow(&mut self, now: Instant) -> bool {
        while let Some(&front) = self.times.front() {
            if now.duration_since(front) > RESTART_WINDOW {
                self.times.pop_front();
            } else {
                break;
            }
        }
        if self.times.len() >= RESTART_MAX_IN_WINDOW {
            return false;
        }
        self.times.push_back(now);
        true
    }
}

/// 1 ロード済み plugin instance の全 bookkeeping (`docs/plan_arch_refactor.md`
/// §6)。旧 4 並行 map + shmem map + editor map を 1 record に併合。
/// **Field 宣言順 = drop 順が契約**: `plugin` (Drop 内で gui_destroy → FFI
/// destroy) が `editor` (DestroyWindow) より先に落ちる。
struct InstanceRecord {
    plugin: Box<dyn LoadedPlugin>,
    /// Editor container window (open 中のみ Some)。`plugin` の後に drop。
    editor: Option<editor_window::EditorWindow>,
    /// 所属 track (`RemoveTrack` の帰属情報。アドレスには使わない)。
    track_id: u32,
    /// GUI が要求した plugin id 文字列 (dedup キー — picker double-fire /
    /// 同プロジェクト 2 度目 LoadSong の再 emit 判定)。
    requested_id: String,
    /// 実際にロードされた descriptor id / 表示名 / パラアウト数 (dedup
    /// 再 emit 用キャッシュ)。
    loaded_id: String,
    name: String,
    aux_output_count: u8,
    /// この device の `ProcessData` shmem の RAII owner (作成者 = 本
    /// プロセス)。record が生きている間 mapping を保持する。
    _shmem: common::process_data::ProcessDataHandle,
    shmem_id: String,
    /// plugin 発 restart 要求の cooldown。
    restarts: RestartWindowTracker,
}

/// plugin 発の非同期 host 要求 (callback → channel → plugin-main loop)。
/// v29: 全て load 時に capture した安定 device_id を運ぶ (旧 (track,index)
/// 値 capture は削除 / 並べ替え後に stale になり「別デバイスの GUI を
/// destroy / 別 plugin を reinit」する実バグだった)。
#[derive(Debug, Clone, Copy)]
enum HostNotify {
    Resize(u64, u32, u32),
    Closed(u64),
    Show(u64, bool),
    /// plugin 発 restart。`Vst3(flags)` は RestartFlags、`Full` は CLAP
    /// `request_restart` (= 全 reinit)。
    Restart(u64, RestartKind),
    /// CLAP `request_callback` → `on_main_thread()` を 1 回呼ぶ。
    MainThreadCallback(u64),
    /// CLAP `clap_host_latency.changed`。
    LatencyChanged(u64),
    /// CLAP `clap_host_params.rescan`。
    ParamsRescan(u64),
}

#[derive(Debug, Clone, Copy)]
enum RestartKind {
    Full,
    Vst3(i32),
}

/// Commands processed serially on the plugin-main thread. wire の
/// [`PluginCommand`] をそのまま運ぶ (protocol 分割で audio 専用 variant が
/// 消えたので 1:1 転送できる)。
enum HostMsg {
    Cmd(PluginCommand),
    Shutdown,
}

#[tokio::main]
async fn main() -> Result<()> {
    // probe (--probe-vst3 / --probe-clap) の早期 return より前に guard を束縛し、
    // probe 実行分のログも flush されるようにする。
    let _log_guard = common::logging::init_tracing_for("daw_plugin_host");
    tracing::info!("daw_plugin_host started");

    // one-shot VST3 port-probe モード。 daw_gui の rescan が VST3 ごとに
    // このプロセスを使い捨てで起動し、 bus 構成から port 構成を得る。
    if std::env::args().nth(1).as_deref() == Some("--probe-vst3") {
        let path = std::env::args()
            .nth(2)
            .context("--probe-vst3 needs <path>")?;
        let target_id = std::env::args().nth(3).unwrap_or_default();
        let ports = std::thread::spawn(move || {
            vst3_plugin::probe_ports(std::path::Path::new(&path), &target_id)
        })
        .join();
        if let Ok(Ok(cfg)) = ports {
            println!("{}", cfg.to_line());
        }
        return Ok(());
    }

    // one-shot CLAP port-probe モード (VST3 と対称)。
    if std::env::args().nth(1).as_deref() == Some("--probe-clap") {
        let path = std::env::args()
            .nth(2)
            .context("--probe-clap needs <path>")?;
        let target_id = std::env::args().nth(3).unwrap_or_default();
        let ports = std::thread::spawn(move || {
            clap_plugin::probe_ports(std::path::Path::new(&path), &target_id)
        })
        .join();
        if let Ok(Ok(cfg)) = ports {
            println!("{}", cfg.to_line());
        }
        return Ok(());
    }

    // one-shot plugin scan モード。daw_gui の cold-start / rescan がこのプロセスを使い捨てで
    // 起動し、システムの CLAP/VST3 を列挙した `PluginDatabase` (JSON) を stdout に出す。DLL 実
    // ロードはこのサブプロセスが担い、GUI プロセスは dlopen しない。プラグインの crash はこの
    // 使い捨てプロセス内に隔離される (probe subprocess と同じ設計)。
    if std::env::args().nth(1).as_deref() == Some("--scan-plugins") {
        let db = std::thread::spawn(plugin_scan::scan_system).join();
        match db {
            Ok(Ok(db)) => match serde_json::to_string(&db) {
                Ok(json) => println!("{json}"),
                Err(e) => tracing::error!(error = ?e, "failed to serialize scanned plugin_db"),
            },
            Ok(Err(e)) => tracing::error!(error = ?e, "plugin scan failed"),
            Err(_) => tracing::error!("plugin scan panicked on worker thread"),
        }
        return Ok(());
    }

    // one-shot ARA bring-up self-test (r.md #5)。 Loads a plug-in and runs
    // the exact load-time ARA sequence with synchronous stdout tracing so a
    // plug-in segfault pinpoints the crashing call.
    if std::env::args().nth(1).as_deref() == Some("--ara-selftest") {
        let path = std::env::args().nth(2).context("--ara-selftest needs <path>")?;
        let target_id = std::env::args().nth(3).unwrap_or_default();
        let wav = std::env::args().nth(4);
        let joined = std::thread::spawn(move || {
            ara_selftest(std::path::Path::new(&path), &target_id, wav.as_deref())
        })
        .join();
        match joined {
            Ok(Ok(())) => println!("ara-selftest: SUCCESS"),
            Ok(Err(e)) => println!("ara-selftest: ERROR {e:#}"),
            Err(_) => println!("ara-selftest: PANIC on worker thread"),
        }
        return Ok(());
    }

    // one-shot editor-window self-test (r.md #65)。 プラグインを 1 つロードして
    // **daw_gui を一切起動せずに** エディタ窓だけを開き、activation / focus 系の
    // 窓メッセージをトレースし続ける。「アクティブにならない / すぐ非アクティブに
    // 戻る」の犯人が daw_gui 側か、プラグイン側か、こちらの WNDPROC かを切り分ける
    // ための最小再現環境 (`--ara-selftest` と同じ使い捨てプロセス方式)。
    //
    //   set RUST_LOG=info,editor_win=trace
    //   daw_plugin_host --editor-selftest "<path>.vst3" [plugin_id] [seconds]
    if std::env::args().nth(1).as_deref() == Some("--editor-selftest") {
        let path = std::env::args()
            .nth(2)
            .context("--editor-selftest needs <path>")?;
        let target_id = std::env::args().nth(3).unwrap_or_default();
        let seconds: u64 = std::env::args()
            .nth(4)
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let joined = std::thread::spawn(move || {
            editor_selftest(std::path::Path::new(&path), &target_id, seconds)
        })
        .join();
        match joined {
            Ok(Ok(())) => println!("editor-selftest: SUCCESS"),
            Ok(Err(e)) => println!("editor-selftest: ERROR {e:#}"),
            Err(_) => println!("editor-selftest: PANIC on worker thread"),
        }
        return Ok(());
    }

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    let mut pipe = common::client::perform_plugin_handshake(&pipe_name).await?;
    tracing::info!("daw_plugin_host handshake complete");

    let session = common::client::read_plugin_session(&mut pipe).await?;
    tracing::info!(?session, "audio session received");

    let (evt_tx, evt_rx) = tmpsc::unbounded_channel::<PluginEvent>();
    let plugin_thread = PluginThread::spawn(session, evt_tx)?;

    // Multiplex pipe I/O: read commands in, write events out.
    pipe_loop(pipe, plugin_thread.sender(), evt_rx).await;

    tracing::info!("daw_plugin_host shutting down");
    plugin_thread.shutdown();
    tracing::info!("daw_plugin_host exiting");
    Ok(())
}

/// Editor-window self-test (see the `--editor-selftest` dispatch in [`main`]).
///
/// **daw_gui を起動しない**ので、ここで観測される activation / focus の挙動には
/// daw_gui は一切関与しない。ここでも「クリックした瞬間だけアクティブ色になって
/// すぐ戻る」が再現するならホスト側 (この WNDPROC or プラグイン) が原因、
/// 再現しないなら daw_gui 側が原因、という二分になる。
fn editor_selftest(path: &std::path::Path, target_id: &str, seconds: u64) -> Result<()> {
    use std::io::Write;
    let step = |msg: &str| {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "editor-selftest: {msg}");
        let _ = out.flush();
    };

    let format = if path.extension().and_then(|e| e.to_str()) == Some("clap") {
        PluginFormat::Clap
    } else {
        PluginFormat::Vst3
    };
    step(&format!(
        "loading {} as {format:?} (target_id={target_id:?})",
        path.display()
    ));
    let mut plugin = load_plugin(format, path, target_id, HostCallbacks::noop())
        .context("load_plugin failed")?;
    anyhow::ensure!(
        plugin.gui_is_embed_supported(),
        "plugin does not support an embedded win32 GUI"
    );
    plugin.gui_create_embedded()?;
    let resizable = plugin
        .gui_sizer()
        .is_some_and(|s| plugin_instance::should_offer_resize_frame(&s.can_resize()));
    let size = plugin
        .gui_get_size()
        .filter(|&(w, h)| w > 0 && h > 0)
        .unwrap_or((800, 600));
    step(&format!(
        "gui created: resizable={resizable} size={}x{}",
        size.0, size.1
    ));

    // selftest は daw_gui 抜きで走るので owner になれる本体窓が無い。
    // owner 無しなら `WS_EX_TOOLWINDOW` も付かない (`OwnerBinding` の doc 参照)。
    let mut editor = editor_window::EditorWindow::create(
        size.0,
        size.1,
        "editor-selftest",
        resizable,
        None,
        editor_window::OwnerBinding::Standalone,
    )
    .map_err(|e| anyhow::anyhow!("create editor window: {e}"))?;
    step(&format!("container hwnd={:#x}", editor.hwnd_u64()));
    let dpi_scale = editor_window::window_dpi_scale(editor.hwnd_u64());
    let _ = plugin.gui_set_scale(dpi_scale);
    // open_gui と同じ順序 (attach の直前に差し込む) を保つ — 再現環境が本番と
    // 違う順序で動くと切り分けの意味が無くなる。
    if let Some(sizer) = plugin.gui_sizer() {
        editor.attach_sizer(sizer);
    }
    plugin.gui_set_parent_hwnd(editor.hwnd_u64())?;
    pump_pending_messages();
    let shown = plugin.gui_show()?;
    let resizable_now = plugin
        .gui_sizer()
        .is_some_and(|s| plugin_instance::should_offer_resize_frame(&s.can_resize()));
    if resizable_now != resizable {
        editor.set_resizable(resizable_now);
    }
    let final_size = plugin
        .gui_get_size()
        .filter(|&(w, h)| w > 0 && h > 0)
        .unwrap_or(size);
    editor.set_client_size(final_size.0, final_size.1);
    editor.show_and_focus();
    step(&format!(
        "attached: show={shown} final={}x{} resizable_after_attach={resizable_now} — pumping {seconds}s",
        final_size.0, final_size.1,
    ));

    // 窓メッセージをひたすら回す。トレースは editor_window の WNDPROC が出す。
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if editor.take_close_request() {
            step("close requested (✕)");
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    step("tearing down");
    plugin.gui_destroy();
    drop(editor);
    drop(plugin);
    Ok(())
}

/// Headless ARA bring-up self-test (see the `--ara-selftest` dispatch in
/// [`main`]).
fn ara_selftest(path: &std::path::Path, target_id: &str, wav: Option<&str>) -> Result<()> {
    use std::io::Write;
    let step = |msg: &str| {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "ara-selftest: {msg}");
        let _ = out.flush();
    };

    let format = if path.extension().and_then(|e| e.to_str()) == Some("clap") {
        PluginFormat::Clap
    } else {
        PluginFormat::Vst3
    };
    step(&format!("loading {} as {format:?} (target_id={target_id:?})", path.display()));
    let mut plugin = load_plugin(format, path, target_id, HostCallbacks::noop())
        .context("load_plugin failed")?;
    step("loaded ok; calling bind_ara_if_capable");
    let bound = plugin
        .bind_ara_if_capable()
        .context("bind_ara_if_capable failed")?;
    step(&format!("bind_ara_if_capable returned {bound}"));
    if bound {
        let clips: Vec<common::protocol::AraClipSpec> = match wav {
            Some(w) => {
                step(&format!("building clip from {w}"));
                vec![common::protocol::AraClipSpec {
                    source_wav: std::path::PathBuf::from(w),
                    persistent_id: "ara-selftest-source-1".to_string(),
                    placement: common::protocol::AraRegionPlacement {
                        start_in_playback_seconds: 0.0,
                        duration_in_playback_seconds: 10.0,
                        start_in_modification_seconds: 0.0,
                        duration_in_modification_seconds: 10.0,
                        time_stretch: false,
                    },
                }]
            }
            None => Vec::new(),
        };
        step(&format!("calling setup_ara with {} clip(s)", clips.len()));
        let _ = plugin.setup_ara(&clips, 120.0, (4, 4), None);
        step("setup_ara returned");

        // Activate AFTER the ARA bind + region setup, mirroring the real
        // engine's install order.
        match plugin.activate(48_000.0, 64, 512) {
            Ok(()) => step("plugin activated"),
            Err(e) => step(&format!("activate failed: {e:#}")),
        }
        let _ = plugin.start_processing();

        // (r.md #5 ARA2) Render repro: a render thread drives the audio
        // half's process() while this thread pumps `notifyModelUpdates`.
        if wav.is_some() {
            ara_render_concurrency_test(plugin.as_ref(), &step);
        }
    }
    step("dropping plugin (teardown)");
    drop(plugin);
    step("teardown complete");
    Ok(())
}

/// Headless reproduction of the ARA realtime-render concurrency: a render
/// thread drives the **audio half** `process()` (like a worker-pool thread)
/// while the caller's thread pumps `notifyModelUpdates` on the **main half**
/// (like plugin-main). v29 split-half によりこの並行は Rust の aliasing 的
/// にも正当 (旧: 同一オブジェクトへの `&mut` 並存)。
fn ara_render_concurrency_test(plugin: &dyn LoadedPlugin, step: &dyn Fn(&str)) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let audio = plugin.audio_half();

    let render_count = Arc::new(AtomicU64::new(0));
    let in_process = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    // Max abs output sample seen, in micro-units — non-zero means the
    // plug-in actually rendered audio for the region.
    let peak_micros = Arc::new(AtomicU64::new(0));

    let rc = Arc::clone(&render_count);
    let ip = Arc::clone(&in_process);
    let st = Arc::clone(&stop);
    let pk = Arc::clone(&peak_micros);
    let render_half = Arc::clone(&audio);
    let render = std::thread::spawn(move || {
        let sr = 48_000u32;
        let frames = 512u32;
        let silence = vec![0.0f32; frames as usize];
        let input: Vec<&[f32]> = vec![&silence, &silence];
        let mut playhead = 0u64;
        while !st.load(Ordering::Relaxed) {
            let transport = crate::plugin_instance::TransportContext {
                bpm: 120.0,
                sample_rate: sr,
                // Mimic the real engine: the true position lives in
                // `song_pos_beats` alone.
                song_pos_beats: playhead as f64 * 120.0 / (60.0 * f64::from(sr)),
                tsig_num: 4,
                tsig_denom: 4,
                is_playing: true,
                is_looping: false,
                loop_start_beats: 0.0,
                loop_end_beats: 0.0,
            };
            ip.store(true, Ordering::SeqCst);
            // SAFETY: this render thread is the only accessor of the audio
            // half while it runs (main thread only pumps main-half methods).
            let half = unsafe { render_half.get() };
            let _ = half.process(frames, &[], &[], &input, &[], &transport);
            ip.store(false, Ordering::SeqCst);
            // Scan main-output channels for the peak.
            let mut m = 0.0f32;
            for ch in 0..2 {
                if let Some(out) = half.output_buffer(ch) {
                    for &s in out.iter().take(frames as usize) {
                        m = m.max(s.abs());
                    }
                }
            }
            pk.fetch_max((m * 1_000_000.0) as u64, Ordering::Relaxed);
            rc.fetch_add(1, Ordering::SeqCst);
            playhead += u64::from(frames);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });

    // This thread = plugin-main: pump notify every ~30 ms and watch the
    // render counter. ~500 rounds * 30 ms ≈ 15 s.
    let mut last = 0u64;
    let mut stalled_rounds = 0u32;
    let mut hung = false;
    for round in 0..500u32 {
        plugin.notify_ara_model_updates();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let now = render_count.load(Ordering::SeqCst);
        if now == last {
            stalled_rounds += 1;
            // ~100 rounds * 30 ms ≈ 3 s with no progress = a real stall.
            if stalled_rounds >= 100 {
                step(&format!(
                    "RENDER STALLED: process() count stuck at {now} (in_process={}) after round {round}",
                    in_process.load(Ordering::SeqCst)
                ));
                hung = true;
                break;
            }
        } else {
            stalled_rounds = 0;
            last = now;
        }
        if round % 50 == 0 {
            let reads = crate::ara::host_controllers::AUDIO_READ_SAMPLES_CALLS
                .load(Ordering::Relaxed);
            step(&format!(
                "render progress: {now} process() calls, {reads} analysis sample-reads (round {round})"
            ));
        }
    }
    let reads = crate::ara::host_controllers::AUDIO_READ_SAMPLES_CALLS.load(Ordering::Relaxed);
    let peak = peak_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
    if !hung {
        step(&format!(
            "render OK: {} process() calls, {reads} analysis sample-reads, output peak={peak:.4}, no stall",
            render_count.load(Ordering::SeqCst)
        ));
    } else {
        step(&format!("(at stall: {reads} sample-reads, output peak={peak:.4})"));
    }
    stop.store(true, Ordering::SeqCst);
    // If process() is hung the render thread can't observe `stop`.
    if hung {
        step("(render thread left detached — process() is stuck)");
        std::mem::forget(render);
    } else {
        let _ = render.join();
    }
}

// --- PluginThread wrapper --------------------------------------------------

struct PluginThread {
    join: Option<JoinHandle<()>>,
    cmd_tx: mpsc::Sender<HostMsg>,
    thread_id: u32,
}

impl PluginThread {
    fn spawn(session: AudioSession, evt_tx: tmpsc::UnboundedSender<PluginEvent>) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<HostMsg>();
        let (tid_tx, tid_rx) = mpsc::channel::<u32>();

        let join = std::thread::Builder::new()
            .name("plugin-main".into())
            .spawn(move || {
                let tid = unsafe { GetCurrentThreadId() };
                let _ = tid_tx.send(tid);
                plugin_main_loop(session, cmd_rx, evt_tx);
            })
            .context("failed to spawn plugin-main thread")?;

        let thread_id = tid_rx
            .recv()
            .context("plugin-main thread failed to report its id")?;

        Ok(Self {
            join: Some(join),
            cmd_tx,
            thread_id,
        })
    }

    fn sender(&self) -> PluginThreadSender {
        PluginThreadSender {
            cmd_tx: self.cmd_tx.clone(),
            thread_id: self.thread_id,
        }
    }

    fn shutdown(mut self) {
        let _ = self.cmd_tx.send(HostMsg::Shutdown);
        wake_thread(self.thread_id);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
struct PluginThreadSender {
    cmd_tx: mpsc::Sender<HostMsg>,
    thread_id: u32,
}

impl PluginThreadSender {
    fn send(&self, msg: HostMsg) {
        if self.cmd_tx.send(msg).is_err() {
            tracing::warn!("plugin-main thread channel closed; command dropped");
            return;
        }
        wake_thread(self.thread_id);
    }
}

fn wake_thread(thread_id: u32) {
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_COMMAND_WAKE, WPARAM(0), LPARAM(0));
    }
}

// --- Plugin-main thread host state ------------------------------------------

/// plugin-main thread の全状態 + command handler 群。
struct PluginHost {
    session: AudioSession,
    evt_tx: tmpsc::UnboundedSender<PluginEvent>,
    notify_tx: tmpsc::UnboundedSender<HostNotify>,
    pid: u32,
    /// `ProcessData` shmem 名の世代カウンタ (`process_data_shmem_id` の
    /// `incarnation`)。**リソースの作成者が名前を所有する = SSoT** なので
    /// daw_gui 側の `generation` (stale 応答ガード用の client 側カウンタ) は
    /// 借りない。instantiate ごとに bump するだけで、名前は不透明 String と
    /// して daw_gui → daw_audio へ素通しされる。
    /// 契約は `common::plugin_ref` の module doc を参照。
    next_shmem_incarnation: u64,
    /// **唯一の bookkeeping** (v29): 安定 device_id → record。
    instances: HashMap<u64, InstanceRecord>,
    /// worker pool が dispatch 中に読む lock-free registry。
    registry: PluginRegistry,
    worker_pool: Option<process_server::WorkerPool>,
    /// ARA WAV 解決等 (v29 時点では `AraClipSpec.source_wav` が常に絶対
    /// パスなので参照されないが、契約として保持)。
    project_dir: Option<PathBuf>,
    /// r.md #36: daw_gui から通知された 「エディタ窓で拾ってよいキー」 の一覧。
    /// **意味論 (どのキーが何をするか) は持たない**。 数値比較だけに使う。
    forwarded_keys: Vec<common::protocol::KeyChord>,
    /// r.md #36: 横取り中の key-down の chord。 対応する key-up も同じ判定で
    /// 飲み込むために覚えておく (down だけ奪うとプラグインが押しっぱなしと誤認する)。
    swallowed_keys: Vec<common::protocol::KeyChord>,
    /// r.md #36: 「キーを全部プラグインに送る」 が ON の device (逃げ道)。
    send_all_keys: HashSet<u64>,
}

impl PluginHost {
    fn new(
        session: AudioSession,
        evt_tx: tmpsc::UnboundedSender<PluginEvent>,
        notify_tx: tmpsc::UnboundedSender<HostNotify>,
    ) -> Self {
        Self {
            session,
            evt_tx,
            notify_tx,
            pid: std::process::id(),
            next_shmem_incarnation: 0,
            instances: HashMap::new(),
            registry: Arc::new(arc_swap::ArcSwap::from_pointee(HashMap::new())),
            worker_pool: None,
            project_dir: None,
            forwarded_keys: Vec::new(),
            swallowed_keys: Vec::new(),
            send_all_keys: HashSet::new(),
        }
    }

    /// Per-device host callbacks: 各ロード plugin は自分の **安定
    /// device_id** を capture する (焼き込み座標の stale 問題が構造的に
    /// 消滅 — `docs/plan_arch_refactor.md` §1)。
    fn make_callbacks(&self, device_id: u64) -> HostCallbacks {
        let notify = |tx: &tmpsc::UnboundedSender<HostNotify>| tx.clone();
        HostCallbacks {
            on_request_resize: {
                let tx = notify(&self.notify_tx);
                Arc::new(move |w, h| {
                    let _ = tx.send(HostNotify::Resize(device_id, w, h));
                })
            },
            on_closed: {
                let tx = notify(&self.notify_tx);
                Arc::new(move || {
                    let _ = tx.send(HostNotify::Closed(device_id));
                })
            },
            on_request_show: {
                let tx = notify(&self.notify_tx);
                Arc::new(move || {
                    let _ = tx.send(HostNotify::Show(device_id, true));
                })
            },
            on_request_hide: {
                let tx = notify(&self.notify_tx);
                Arc::new(move || {
                    let _ = tx.send(HostNotify::Show(device_id, false));
                })
            },
            on_restart_component: {
                let tx = notify(&self.notify_tx);
                Arc::new(move |flags: i32| {
                    let _ = tx.send(HostNotify::Restart(device_id, RestartKind::Vst3(flags)));
                })
            },
            on_request_restart: {
                let tx = notify(&self.notify_tx);
                Arc::new(move || {
                    let _ = tx.send(HostNotify::Restart(device_id, RestartKind::Full));
                })
            },
            on_request_callback: {
                let tx = notify(&self.notify_tx);
                Arc::new(move || {
                    let _ = tx.send(HostNotify::MainThreadCallback(device_id));
                })
            },
            on_latency_changed: {
                let tx = notify(&self.notify_tx);
                Arc::new(move || {
                    let _ = tx.send(HostNotify::LatencyChanged(device_id));
                })
            },
            on_params_rescan: {
                let tx = notify(&self.notify_tx);
                Arc::new(move || {
                    let _ = tx.send(HostNotify::ParamsRescan(device_id));
                })
            },
            // VST3 param gesture (IComponentHandler)。CLAP plugin はこの
            // callback を呼ばない (out_events 経由) ので二重発火しない。
            on_param_gesture_begin: {
                let tx = self.evt_tx.clone();
                Arc::new(move |param_id| {
                    let _ = tx.send(PluginEvent::PluginParamTouched {
                        device_id,
                        param_id,
                        // display_name は daw_gui 側で plugin_params cache
                        // から解決する。
                        display_name: format!("Param {param_id}"),
                    });
                })
            },
            on_param_value: {
                let tx = self.evt_tx.clone();
                Arc::new(move |param_id, value| {
                    let _ = tx.send(PluginEvent::PluginParamValueChanged {
                        device_id,
                        param_id,
                        value,
                    });
                })
            },
            on_param_gesture_end: {
                let tx = self.evt_tx.clone();
                Arc::new(move |param_id| {
                    let _ = tx.send(PluginEvent::PluginParamGestureEnd { device_id, param_id });
                })
            },
            // builtin VOICEVOX の合成状態報告 (旧 set_voicevox_status_reporter
            // の第 2 callback 機構を HostCallbacks に統合)。
            on_vocal_synth_status: {
                let tx = self.evt_tx.clone();
                Arc::new(move |progress| {
                    let _ = tx.send(PluginEvent::VoicevoxSynthStatus {
                        device_id,
                        progress,
                    });
                })
            },
            // r.md #65: エディタ窓の HWND は plugin 側 (`gui_set_parent_hwnd` /
            // `gui_destroy`) が書き込む。plugin-main は窓の所有だけを持ち、
            // この atomic には触らない (「今どの窓に attach しているか」が SSoT)。
            editor_hwnd: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn emit(&self, evt: PluginEvent) {
        let _ = self.evt_tx.send(evt);
    }

    /// r.md #36: `hwnd` がどの device のエディタ窓 (コンテナ本体 or その子孫) かを返す。
    /// `instances` は device_id keyed の唯一の bookkeeping なので、 ここを引くだけで
    /// positional index を新設せずに解決できる (アーキテクチャ不変条件 #1)。
    fn editor_device_of(&self, hwnd: HWND) -> Option<(u64, bool)> {
        if hwnd.is_invalid() {
            return None;
        }
        for (&device_id, rec) in &self.instances {
            let Some(editor) = rec.editor.as_ref() else { continue };
            let container = HWND(editor.hwnd_u64() as *mut core::ffi::c_void);
            if container == hwnd {
                return Some((device_id, true));
            }
            if unsafe { IsChild(container, hwnd) }.as_bool() {
                return Some((device_id, false));
            }
        }
        None
    }

    /// r.md #36: プラグインエディタ由来のキーメッセージを処理する。
    ///
    /// 戻り値 `true` = **飲み込んだ** (呼び出し側は `TranslateMessage` /
    /// `DispatchMessageW` を呼ばない)。 `false` = 従来どおりプラグインへ流す。
    fn handle_editor_key(&mut self, msg: &MSG) -> bool {
        let relay_down = msg.message == editor_window::WM_EDITOR_KEY_RELAY_DOWN;
        let relay_up = msg.message == editor_window::WM_EDITOR_KEY_RELAY_UP;
        let is_relay = relay_down || relay_up;
        let is_down = relay_down || (!is_relay && editor_keys::is_key_down(msg.message));
        let is_up = relay_up || (!is_relay && editor_keys::is_key_up(msg.message));
        if !is_down && !is_up {
            return false;
        }
        let Some((device_id, is_container)) = self.editor_device_of(msg.hwnd) else {
            return false;
        };
        // 逃げ道 (REAPER の「Send all keyboard input to plug-in」相当): この device では
        // 一切横取りしない。 relay は既にプラグインが 「要らない」 と言ったものなので
        // 飲み込む (再 dispatch しても行き場が無い) が、 転送はしない。
        if self.send_all_keys.contains(&device_id) {
            return is_relay;
        }
        let chord = editor_keys::chord_of(msg);
        if is_up {
            // down を奪ったキーだけ up も奪う (対で奪わないとプラグインが押しっぱなしと誤認)。
            if let Some(i) = self.swallowed_keys.iter().position(|c| c.vk == chord.vk) {
                self.swallowed_keys.remove(i);
                return true;
            }
            // relay 経由の up は既にプラグインが捨てたものなので行き場が無い。
            return is_relay;
        }
        if !self.forwarded_keys.contains(&chord) {
            // 転送対象外のキーは触らない。 relay 経由 (= プラグインが捨てたキー) は
            // 行き場が無いのでここで捨てる。
            return is_relay;
        }
        // 経路 A: プラグインの子窓宛に **まだ届いていない** キーは、 フォーカス窓に
        // 「このキー要る?」 を問い合わせてから決める。 コンテナ窓宛 (= relay 含む) は
        // 既にプラグインが未消化を宣言しているので問い合わせ不要。
        //
        // **オートリピート判定より前に問い合わせる**。 逆順にすると、 プラグインの
        // テキスト欄で Space を長押ししたときに 2 発目以降が無条件で飲まれ、
        // 「最初の 1 文字しか入らない」 になる。
        if !is_container && !is_relay && editor_keys::window_wants_key(msg.hwnd, msg) {
            return false;
        }
        if editor_keys::is_auto_repeat(msg.lParam) {
            // オートリピートは 1 押下 1 発火にするため **飲み込むが emit しない**。
            return true;
        }
        // 取りこぼした key-up の残骸を回収する (エディタを閉じた / フォーカスを
        // 奪われた等で up が来ないケース)。 物理的に押されていない vk は捨てる。
        self.swallowed_keys
            .retain(|c| unsafe { GetAsyncKeyState(i32::from(c.vk)) } < 0);
        self.swallowed_keys.push(chord);
        self.emit(PluginEvent::EditorKey { device_id, chord });
        true
    }

    /// registry から `device_id` の entry を外し、worker の in-flight
    /// dispatch を排出する。戻り値 = 外した entry (republish 用)。
    /// entry が未 publish なら quiesce も不要 (worker は触れない)。
    fn detach_and_quiesce(&self, device_id: u64) -> Option<PluginEntry> {
        let saved = registry_remove(&self.registry, device_id);
        if saved.is_some()
            && let Some(pool) = self.worker_pool.as_ref()
        {
            pool.quiesce();
        }
        saved
    }

    /// `device_id` の plugin latency を再 query して `PluginLatencyChanged`
    /// を emit する (activate 直後 / restart / reinit / CLAP latency.changed
    /// の共通関数 — `docs/plan_arch_refactor.md` §6 の非対称是正)。
    fn requery_latency_and_emit(&mut self, device_id: u64) {
        let Some(rec) = self.instances.get_mut(&device_id) else {
            return;
        };
        let samples = rec.plugin.query_latency();
        tracing::info!(device_id, samples, "plugin reported latency");
        self.emit(PluginEvent::PluginLatencyChanged { device_id, samples });
    }

    /// param 一覧を (再) 送信する (activate 直後 / CLAP params.rescan)。
    fn emit_param_list(&mut self, device_id: u64) {
        let Some(rec) = self.instances.get_mut(&device_id) else {
            return;
        };
        let params = rec.plugin.enumerate_params();
        if !params.is_empty() {
            tracing::info!(device_id, count = params.len(), "plugin enumerated params");
        }
        let has_embedded_gui = rec.plugin.gui_is_embed_supported();
        self.emit(PluginEvent::PluginParamList {
            device_id,
            params,
            has_embedded_gui,
        });
    }

    // ----------------------------------------------------------------
    // command handlers
    // ----------------------------------------------------------------

    fn handle_command(&mut self, cmd: PluginCommand) {
        match cmd {
            // handshake 段階で消費される variant (ここに来るのは配線異常)。
            PluginCommand::Ack | PluginCommand::Session(_) => {
                tracing::warn!("unexpected handshake message after handshake; ignored");
            }
            PluginCommand::SetProjectDir(dir) => {
                tracing::info!(?dir, "project dir updated");
                self.project_dir = dir;
            }
            PluginCommand::OpenWorkerPool {
                n_workers,
                worker_bridge_shmem_id,
                wake_event_names,
                done_event_names,
            } => {
                if let Some(pool) = self.worker_pool.take() {
                    pool.shutdown();
                }
                match process_server::WorkerPool::open(
                    n_workers,
                    &worker_bridge_shmem_id,
                    &self.session.metrics_shmem_id,
                    &wake_event_names,
                    &done_event_names,
                    Arc::clone(&self.registry),
                    self.evt_tx.clone(),
                ) {
                    Ok(pool) => self.worker_pool = Some(pool),
                    Err(e) => {
                        tracing::error!(error = ?e, "failed to open plugin worker pool");
                    }
                }
            }
            PluginCommand::CloseWorkerPool => {
                if let Some(pool) = self.worker_pool.take() {
                    pool.shutdown();
                }
            }
            PluginCommand::SetRenderMode(mode) => {
                // Forward the render hint to every loaded plugin
                // (best-effort)。
                for rec in self.instances.values_mut() {
                    let _ = rec.plugin.set_render_mode(mode);
                }
                tracing::info!(?mode, "render mode broadcast to all plugins");
            }
            PluginCommand::ReinitAllPlugins => self.reinit_all_plugins(),
            PluginCommand::SetSlotPlugin {
                device_id,
                track_id,
                format,
                path,
                plugin_id,
                initial_state,
                generation,
            } => {
                self.set_slot_plugin(
                    device_id,
                    track_id,
                    format,
                    &path,
                    &plugin_id,
                    initial_state,
                    generation,
                );
            }
            PluginCommand::RemoveSlotPlugin { device_id } => {
                self.teardown_device(device_id, true);
            }
            PluginCommand::RemoveTrack { track_id } => {
                // `track_id` は stable な `Track::id`。属する device を全列挙
                // して個別 teardown (順序は不定でよい — chain 順序概念なし)。
                let ids: Vec<u64> = self
                    .instances
                    .iter()
                    .filter_map(|(&id, rec)| (rec.track_id == track_id).then_some(id))
                    .collect();
                for id in ids {
                    self.teardown_device(id, true);
                }
            }
            // (r.md #61) 終了要求は `pipe_loop` が read ループを抜けるために
            // 消費するので、ここへは届かない。届いたら配線が壊れているので
            // 黙って無視せず警告する (プロセスは pipe EOF で正しく畳まれる)。
            PluginCommand::Shutdown => {
                tracing::warn!("Shutdown reached handle_command; pipe_loop should have consumed it");
            }
            PluginCommand::UnloadAllPlugins => {
                // project 切替。`device_id` は Song スコープの名前なので、
                // 残すと新 project の同 id device が旧 instance に dedup
                // 吸収される (保存 state が復元されず前 project の音で鳴る)。
                let ids: Vec<u64> = self.instances.keys().copied().collect();
                tracing::info!(count = ids.len(), "UnloadAllPlugins: project switched");
                for id in ids {
                    self.teardown_device(id, true);
                }
            }
            PluginCommand::RequestSlotState { device_id } => {
                let data = match self.instances.get_mut(&device_id) {
                    Some(rec) => match rec.plugin.state_save() {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(error = ?e, device_id, "state_save failed");
                            None
                        }
                    },
                    None => None,
                };
                self.emit(PluginEvent::SlotPluginState { device_id, data });
            }
            PluginCommand::RequestAllStates => {
                let entries = self.collect_all_states();
                self.emit(PluginEvent::AllPluginStates { entries });
            }
            PluginCommand::OpenSlotGuiEmbedded {
                device_id,
                title,
                geometry,
                owner_main_window,
            } => {
                // r.md #65: owner が取れたかで、owner と `WS_EX_TOOLWINDOW` の
                // **両方**が決まる (片方だけ成立する経路を作らない)。
                let owner = editor_window::OwnerBinding::resolve(owner_main_window);
                match self.open_gui(device_id, &title, geometry, owner) {
                    Ok(Some(geometry)) => {
                        self.emit(PluginEvent::SlotGuiGeometry { device_id, geometry });
                    }
                    Ok(None) => {
                        self.emit(PluginEvent::SlotGuiClosed { device_id });
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, device_id, "failed to open GUI");
                        // open_gui cleaned up its own (plugin + window) on
                        // failure; close_slot_gui is idempotent and also
                        // emits SlotGuiClosed.
                        self.close_slot_gui(device_id);
                    }
                }
            }
            PluginCommand::CloseSlotGui { device_id } => {
                self.close_slot_gui(device_id);
            }
            // r.md #55: 開いている窓は host 自身が知っている。daw_gui から
            // device_id を並べてもらう形にすると、open 応答が返る前の窓が
            // 列挙から漏れて閉じ残る (UnloadAllPlugins と同じ理由)。
            PluginCommand::CloseAllSlotGuis => {
                let ids: Vec<u64> = self
                    .instances
                    .iter()
                    .filter_map(|(&id, rec)| rec.editor.is_some().then_some(id))
                    .collect();
                tracing::info!(count = ids.len(), "CloseAllSlotGuis");
                for id in ids {
                    self.close_slot_gui(id);
                }
            }
            // r.md #36: daw_gui の SHORTCUTS から導出された chord 列をそのまま保持する。
            // ここに意味論 (どのキーが何をするか) は無い。
            PluginCommand::SetEditorForwardedKeys { chords } => {
                tracing::info!(count = chords.len(), "editor forwarded keys updated");
                self.forwarded_keys = chords;
                // 転送対象が変わると「down を奪ったか」の前提も変わるので残骸を捨てる。
                self.swallowed_keys.clear();
            }
            PluginCommand::SetEditorSendAllKeys { device_id, enabled } => {
                if enabled {
                    self.send_all_keys.insert(device_id);
                } else {
                    self.send_all_keys.remove(&device_id);
                }
            }
            PluginCommand::SetBuiltinPluginNoteMetadata {
                device_id,
                bpm,
                chunk_secs,
                entries,
                talk,
            } => {
                let Some(rec) = self.instances.get_mut(&device_id) else {
                    tracing::warn!(device_id, "SetBuiltinPluginNoteMetadata: device not found");
                    return;
                };
                // VocalSynth capability (builtin VOICEVOX のみ Some)。CLAP /
                // VST3 は None → silent no-op (IPC 経路は format-neutral)。
                if let Some(vs) = rec.plugin.as_vocal_synth() {
                    vs.set_note_metadata(bpm, chunk_secs, &entries, &talk);
                }
            }
            PluginCommand::PrepareVocalSynth { device_id } => {
                self.prepare_vocal_synth(device_id);
            }
            PluginCommand::SetVocalSynthPriority { device_id, playhead_beats } => {
                // device 未発見でも **warn を出さない** — トランスポート中に毎秒来る
                // 順序ヒントなので、ログを汚さない。
                if let Some(rec) = self.instances.get_mut(&device_id)
                    && let Some(vs) = rec.plugin.as_vocal_synth()
                {
                    vs.set_priority_beats(playhead_beats);
                }
            }
            PluginCommand::SetupAraDocument { device_id, clips, bpm, time_sig, archive } => {
                // setup_ara は内部で deactivate→activate するので quiesce 契約。
                let saved = self.detach_and_quiesce(device_id);
                let published = saved.is_some();
                match self.instances.get_mut(&device_id) {
                    Some(rec) => {
                        if published {
                            rec.plugin.stop_processing();
                        }
                        match rec.plugin.setup_ara(&clips, bpm, time_sig, archive.as_deref()) {
                            Ok(true) => {
                                tracing::info!(device_id, n = clips.len(), "ARA document set up");
                            }
                            Ok(false) => {
                                tracing::warn!(
                                    device_id,
                                    "SetupAraDocument: plugin is not ARA-capable, ignoring"
                                );
                            }
                            Err(e) => {
                                tracing::error!(error = ?e, device_id, "ARA setup failed");
                            }
                        }
                        if published
                            && let Err(e) = rec.plugin.start_processing()
                        {
                            tracing::error!(error = ?e, device_id, "SetupAraDocument: start_processing failed");
                        }
                    }
                    None => {
                        tracing::warn!(device_id, "SetupAraDocument: no plugin for device");
                    }
                }
                if let Some(entry) = saved {
                    registry_insert(&self.registry, device_id, entry);
                }
            }
            PluginCommand::ClearAraDocument { device_id } => {
                // clear_ara も deactivate→activate を伴うので同じ quiesce 契約。
                let saved = self.detach_and_quiesce(device_id);
                let published = saved.is_some();
                if let Some(rec) = self.instances.get_mut(&device_id) {
                    if published {
                        rec.plugin.stop_processing();
                    }
                    rec.plugin.clear_ara();
                    if published
                        && let Err(e) = rec.plugin.start_processing()
                    {
                        tracing::error!(error = ?e, device_id, "ClearAraDocument: start_processing failed");
                    }
                    tracing::info!(device_id, "ARA document cleared");
                }
                if let Some(entry) = saved {
                    registry_insert(&self.registry, device_id, entry);
                }
            }
            PluginCommand::UpdateAraRegions { device_id, regions } => {
                match self.instances.get(&device_id) {
                    Some(rec) => {
                        rec.plugin.update_ara_regions(&regions);
                        tracing::info!(device_id, n = regions.len(), "ARA regions updated");
                    }
                    None => {
                        tracing::warn!(device_id, "UpdateAraRegions: no plugin for device");
                    }
                }
            }
        }
    }

    /// `SetSlotPlugin`: device の plugin を load / replace する。
    #[allow(clippy::too_many_arguments)]
    fn set_slot_plugin(
        &mut self,
        device_id: u64,
        track_id: u32,
        format: PluginFormat,
        path: &std::path::Path,
        plugin_id: &str,
        initial_state: Option<Vec<u8>>,
        generation: u64,
    ) {
        // Defensive dedup against picker double-fire / 同プロジェクト 2 度目
        // LoadSong: 同じ plugin id が既にこの device に居るなら reload せず、
        // ただし daw_gui の `pending_plugin_loads` を解放するため
        // `SlotPluginLoaded` は必ず再 emit する (generation echo 付き)。
        if let Some(rec) = self.instances.get_mut(&device_id)
            && rec.requested_id == plugin_id
        {
            // 帰属 track を必ず最新にする。ここを更新しないと、track を跨いで
            // 同 device_id が再利用されたとき `RemoveTrack` の列挙
            // (`rec.track_id == track_id`) から外れ、instance が永久に
            // 回収されなくなる。
            rec.track_id = track_id;
            tracing::info!(
                device_id,
                id = %plugin_id,
                "SetSlotPlugin: same plugin already loaded, re-emitting SlotPluginLoaded"
            );
            let evt = PluginEvent::SlotPluginLoaded {
                device_id,
                id: rec.loaded_id.clone(),
                name: rec.name.clone(),
                shmem_id: rec.shmem_id.clone(),
                // 同 plugin の re-emit path。state_load を呼んでいないので
                // error は常に None。
                state_load_error: None,
                aux_output_count: rec.aux_output_count,
                generation,
            };
            self.emit(evt);
            return;
        }

        // (1) 新 plugin の instantiate。失敗 ⇒ 旧 plugin は touch せず早期
        //     return (旧 plugin が居れば継続再生)。
        let callbacks = self.make_callbacks(device_id);
        let mut plugin = match load_plugin(format, path, plugin_id, callbacks) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = ?e, ?format, path = %path.display(), "load failed");
                self.emit(PluginEvent::SlotPluginLoadFailed {
                    device_id,
                    plugin_id: plugin_id.to_string(),
                    reason: format!("{e}"),
                    generation,
                });
                return;
            }
        };
        // (r.md #5 ARA2) Bind ARA before the first state load / activate /
        // GUI creation, as the ARA spec requires.
        if let Err(e) = plugin.bind_ara_if_capable() {
            tracing::error!(error = ?e, device_id, "ARA bind at load failed");
        }
        // state_load 失敗は握りつぶさず `state_load_error` で daw_gui へ
        // (silent corruption fix)。plugin 自体は default 状態で進む。
        let state_load_error: Option<String> = if let Some(bytes) = initial_state {
            match plugin.state_load(&bytes) {
                Ok(()) => None,
                Err(e) => {
                    let reason = format!("{e:#}");
                    tracing::error!(
                        device_id,
                        plugin = %plugin_id,
                        error = %reason,
                        "state_load failed (= plugin は default 状態で進む)",
                    );
                    Some(reason)
                }
            }
        } else {
            None
        };

        // (2) 旧 instance の teardown (registry detach → quiesce → drop)。
        //     旧 plugin の unload は GUI へは通知しない (直後の
        //     SlotPluginLoaded が同 device_id を上書きする)。ただし
        //     `SlotPluginShmemReleased` は teardown_device が必ず送るので、
        //     daw_audio は下の (4) で作る新 mapping を開く前に旧 mapping を
        //     落とす (= 旧へ書いて新を読む窓が閉じる)。
        self.teardown_device(device_id, false);

        // (3) activate + start_processing。v29: 失敗した plugin は registry
        //     に **publish しない** (旧実装は無条件 publish でゾンビ化 →
        //     RT worker が毎 buffer error を吐いた)。失敗は
        //     `SlotPluginLoadFailed` で GUI へ可視化する。
        let sr = f64::from(self.session.sample_rate);
        let mf = self.session.max_frames;
        if let Err(e) = plugin
            .activate(sr, 64, mf)
            .and_then(|()| plugin.start_processing())
        {
            tracing::error!(error = ?e, device_id, "activate/start_processing failed; not publishing");
            teardown_plugin(plugin);
            self.emit(PluginEvent::SlotPluginLoadFailed {
                device_id,
                plugin_id: plugin_id.to_string(),
                reason: format!("activate failed: {e:#}"),
                generation,
            });
            return;
        }

        // (4) ProcessData shmem を作成。名前は **この instantiation 専用**
        //     (incarnation を焼き込む)。device_id を再利用すると、前世代の
        //     mapping を daw_audio がまだ握っている間に同名 create が走って
        //     `already exists` で失敗する (解放は非同期で上限が無い) —
        //     `common::plugin_ref` の命名契約を参照。世代を含めれば前世代が
        //     生き残っていても create は必ず成功し、旧 mapping は保持者が
        //     居なくなった時点で静かに消える。
        self.next_shmem_incarnation += 1;
        let shmem_id = process_data_shmem_id(self.pid, device_id, self.next_shmem_incarnation);
        // 一意名にした後で `already exists` が出たら本物の設計バグ (同
        // incarnation の二重 create) なので、`NamedShmem::create` の排他
        // チェックは検出器として残してある。
        let shmem = match common::process_data::ProcessDataHandle::create(&shmem_id) {
            Ok(handle) => {
                // 名前をログに残す: この class の障害 (名前の再利用 / 世代ずれ) は
                // 「どの名前を誰が握っているか」が分からないと追えない。
                tracing::info!(device_id, %shmem_id, "created ProcessData shmem");
                handle
            }
            Err(e) => {
                tracing::error!(error = ?e, device_id, %shmem_id, "failed to create ProcessData shmem");
                teardown_plugin(plugin);
                self.emit(PluginEvent::SlotPluginLoadFailed {
                    device_id,
                    plugin_id: plugin_id.to_string(),
                    reason: format!("shmem create failed: {e}"),
                    generation,
                });
                return;
            }
        };

        // (5) record 化 + registry publish + 通知。latency / params query は
        //     publish **前** (= plugin-main が排他アクセスを持つ間) に行う。
        let loaded_id = plugin.id().to_string();
        let loaded_name = plugin.name().to_string();
        let aux_output_count = plugin.aux_output_port_count().min(u8::MAX as usize) as u8;
        let latency_samples = plugin.query_latency();
        let params = plugin.enumerate_params();
        let has_embedded_gui = plugin.gui_is_embed_supported();
        let audio = plugin.audio_half();
        let pd_ptr = shmem.ptr();

        self.instances.insert(
            device_id,
            InstanceRecord {
                plugin,
                editor: None,
                track_id,
                requested_id: plugin_id.to_string(),
                loaded_id: loaded_id.clone(),
                name: loaded_name.clone(),
                aux_output_count,
                _shmem: shmem,
                shmem_id: shmem_id.clone(),
                restarts: RestartWindowTracker::default(),
            },
        );
        registry_insert(
            &self.registry,
            device_id,
            PluginEntry {
                audio,
                process_data: pd_ptr,
                err_logged: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                metric_slot: Arc::new(std::sync::atomic::AtomicU32::new(METRIC_SLOT_UNCLAIMED)),
            },
        );

        self.emit(PluginEvent::SlotPluginLoaded {
            device_id,
            id: loaded_id,
            name: loaded_name,
            shmem_id,
            state_load_error,
            aux_output_count,
            generation,
        });
        tracing::info!(device_id, samples = latency_samples, "plugin reported latency");
        self.emit(PluginEvent::PluginLatencyChanged {
            device_id,
            samples: latency_samples,
        });
        if !params.is_empty() {
            tracing::info!(device_id, count = params.len(), "plugin enumerated params");
        }
        self.emit(PluginEvent::PluginParamList {
            device_id,
            params,
            has_embedded_gui,
        });
    }

    /// device の instance を完全 teardown する。`emit_unloaded` は
    /// `SlotPluginUnloaded` を送るか (replace 経路では送らない — 直後の
    /// `SlotPluginLoaded` が同 device_id を上書きするので、daw_gui の
    /// bookkeeping を一度空にする必要はない)。
    ///
    /// `SlotPluginShmemReleased` は **`emit_unloaded` に関わらず必ず**送る。
    /// 「この device の shmem はもう無い」は「device が空になった」とは別の
    /// 事実で、daw_audio が旧 mapping を掴んだまま新 instance と食い違うのを
    /// 防ぐのはこちらの責務 (protocol の doc 参照)。
    fn teardown_device(&mut self, device_id: u64, emit_unloaded: bool) {
        let Some(mut rec) = self.instances.remove(&device_id) else {
            return;
        };
        // (0) daw_audio に mapping を落とさせる。plugin の destroy より先に
        //     送ることで、daw_gui → daw_audio の順序保証 (Released → 後続の
        //     Loaded) がそのまま「close が open に先行する」になる。
        self.emit(PluginEvent::SlotPluginShmemReleased { device_id });
        // (1) registry から外す → (2) in-flight dispatch 排出。
        self.detach_and_quiesce(device_id);
        // (3) editor を先に取り出しておく (drop は plugin teardown の後)。
        let editor = rec.editor.take();
        // r.md #65 同件: `close_slot_gui` と同じく、**アクティブな窓を破棄すると
        // アクティブは owner ではなく Alt+Esc 順の次の窓へ移る**。判定は drop の
        // 前に取る。この経路 (device 削除 / 差し替え / track 削除 / shutdown) は
        // 通常 daw_gui 発なので前面は daw_gui = 自分のグループではなく、述語が
        // `None` を返して何もしない。エディタを触っている最中に消えた場合だけ効く。
        let restore_foreground = editor
            .as_ref()
            .and_then(editor_window::EditorWindow::owner_to_restore_if_foreground);
        // (4) plugin-main thread で teardown + drop。
        //     **teardown 順序を守ること**: stop_processing → deactivate →
        //     gui_destroy → drop (kHs 系 VST3 の Drop crash 防止)。
        teardown_plugin(rec.plugin);
        // (5) container window の破棄 (gui_destroy の後) + GUI 開状態の同期。
        if editor.is_some() {
            drop(editor);
            if let Some(owner) = restore_foreground {
                editor_window::restore_foreground_to_owner(owner);
            }
            // r.md #49: `close_slot_gui` と同じ理由 — 窓が尽きたら非アクティブへ落とす。
            if self.instances.values().all(|r| r.editor.is_none()) {
                editor_window::clear_windows_active();
            }
            self.emit(PluginEvent::SlotGuiClosed { device_id });
        }
        // (6) shmem handle は rec ごと drop 済 (rec._shmem)。名前は
        //     incarnation 入りで二度と再利用されないので、daw_audio 側の
        //     解放が遅れても次の create と衝突しない。
        if emit_unloaded {
            // daw_gui はこれを受けて自分側の bookkeeping を片付ける
            // (shmem の close は上の `SlotPluginShmemReleased` が担当)。
            self.emit(PluginEvent::SlotPluginUnloaded { device_id });
        }
    }

    /// ReinitAllPlugins: 全 plugin を clean state へ (export 前 / パニック)。
    fn reinit_all_plugins(&mut self) {
        // Force every plugin back to a clean, silent state with a
        // two-pronged reset (deactivate→activate + reset())。
        //   - deactivate→activate clears stubborn held voices (VCV Rack 2);
        //   - `reset()` clears CLAP DSP tails (reverb feedback network)。
        // Safety contract: registry detach → quiesce → mutate (this thread)
        // → republish。
        let saved = registry_take_all(&self.registry);
        if let Some(pool) = self.worker_pool.as_ref() {
            pool.quiesce();
        }
        let sr = f64::from(self.session.sample_rate);
        let mf = self.session.max_frames;
        let mut restored = HashMap::new();
        let mut reinitialised: Vec<u64> = Vec::new();
        let device_ids: Vec<u64> = self.instances.keys().copied().collect();
        for device_id in &device_ids {
            let Some(rec) = self.instances.get_mut(device_id) else {
                continue;
            };
            rec.plugin.stop_processing();
            rec.plugin.deactivate();
            let ok = match rec
                .plugin
                .activate(sr, 64, mf)
                .and_then(|()| rec.plugin.start_processing())
            {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(error = ?e, device_id, "reinit: activate/start failed; leaving detached");
                    false
                }
            };
            if ok {
                rec.plugin.reset();
                reinitialised.push(*device_id);
                // 成功したものだけ republish (v29: 失敗 plugin をゾンビ
                // publish しない — worker は無音バイパス)。
                if let Some(entry) = saved.get(device_id) {
                    restored.insert(*device_id, entry.clone());
                }
            }
        }
        registry_restore_all(&self.registry, restored);
        // v29: reinit 完了時にも per-plugin latency を再 query + re-emit
        // (restartComponent 経路と共通関数 — 旧実装は非対称で export 前
        // reinit 後に PDC が stale になった)。query は active な plugin
        // にしか許されない (CLAP latency ext は `[main-thread & active]`)
        // ので、reinit に成功したものだけ。
        let n = reinitialised.len();
        for device_id in reinitialised {
            self.requery_latency_and_emit(device_id);
        }
        tracing::info!(plugins = n, "reinitialised all plugins to clean state (export prep / panic)");
        self.emit(PluginEvent::PluginsReinitDone);
    }

    /// plugin 発 restart (CLAP `request_restart` / VST3 `restartComponent`)
    /// の実行部。cooldown 超過は無視 + warn。
    fn handle_restart(&mut self, device_id: u64, kind: RestartKind) {
        // VST3 RestartFlags の反応:
        //   kReloadComponent=1 / kIoChanged=2 → full reinit。
        //   kLatencyChanged=8 → latency 再 query のみ (deactivate→activate
        //     すると Melda 系が kLatencyChanged を再送 → 無限 reinit ループ。
        //     実機で 10k+ reinit/97s を観測済み)。
        //   cosmetic flags は無視。
        use vst3::Steinberg::Vst::RestartFlags_;
        let latency_only = match kind {
            RestartKind::Full => false,
            RestartKind::Vst3(flags) => {
                const VST3_REACTIVATE: i32 =
                    RestartFlags_::kReloadComponent | RestartFlags_::kIoChanged;
                let reinit = flags & VST3_REACTIVATE != 0;
                let latency = flags & RestartFlags_::kLatencyChanged != 0;
                if !reinit && !latency {
                    return;
                }
                !reinit && latency
            }
        };
        if !self.instances.contains_key(&device_id) {
            return;
        }
        if latency_only {
            // plugin は active のままなので query 可能。
            self.requery_latency_and_emit(device_id);
            return;
        }
        // full reinit — per-plugin cooldown (3 回 / 10s)。activate 中に
        // kIoChanged / kReloadComponent を再送する plugin への構造的防御。
        if let Some(rec) = self.instances.get_mut(&device_id)
            && !rec.restarts.allow(Instant::now())
        {
            tracing::warn!(
                device_id,
                "plugin restart request ignored: exceeded {RESTART_MAX_IN_WINDOW} restarts in {RESTART_WINDOW:?} (reinit loop guard)"
            );
            return;
        }
        let saved = self.detach_and_quiesce(device_id);
        let published = saved.is_some();
        let sr = f64::from(self.session.sample_rate);
        let mf = self.session.max_frames;
        let mut ok = false;
        if published {
            if let Some(rec) = self.instances.get_mut(&device_id) {
                rec.plugin.stop_processing();
                rec.plugin.deactivate();
                ok = match rec
                    .plugin
                    .activate(sr, 64, mf)
                    .and_then(|()| rec.plugin.start_processing())
                {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::error!(error = ?e, device_id, "restart reinit: activate/start failed");
                        false
                    }
                };
                if ok {
                    rec.plugin.reset();
                }
            }
            tracing::info!(device_id, ?kind, "plugin restart: reinitialised");
        }
        // registry 未掲載 (published=false) なら reinit 対象外 — 何もしない
        // (旧実装と同義)。失敗した plugin は republish しない (ゾンビ防止)。
        if ok && let Some(entry) = saved {
            registry_insert(&self.registry, device_id, entry);
        }
        // latency query は active な plugin にしか許されないので成功時のみ。
        if ok {
            self.requery_latency_and_emit(device_id);
        }
    }

    fn prepare_vocal_synth(&mut self, device_id: u64) {
        // bounce / 曲全体の WAV 書き出しの前に合成完了を保証する。builtin VOICEVOX の
        // (queued, done, heartbeat) を取り出し、直前 flush 世代まで done になるのを
        // 別 thread で poll して VocalSynthReady を emit する
        // (待ちの規則は `plugin_instance::spawn_vocal_synth_wait`)。
        // 該当 builtin が無ければ / spawn に失敗したら即 ready (bounce を hang させない)。
        let progress = self
            .instances
            .get_mut(&device_id)
            .and_then(|rec| rec.plugin.as_vocal_synth().map(|vs| vs.synth_progress()));
        let spawned = progress.is_some_and(|p| {
            let tx = self.evt_tx.clone();
            crate::plugin_instance::spawn_vocal_synth_wait(device_id, p, move |ev| {
                let _ = tx.send(ev);
            })
        });
        if !spawned {
            self.emit(PluginEvent::VocalSynthReady { device_id });
        }
    }

    fn collect_all_states(&mut self) -> Vec<SlotState> {
        let mut out = Vec::new();
        // Iterate in deterministic device_id order so save files diff cleanly.
        let mut ids: Vec<u64> = self.instances.keys().copied().collect();
        ids.sort_unstable();
        for device_id in ids {
            let Some(rec) = self.instances.get_mut(&device_id) else {
                continue;
            };
            let (data, error) = match rec.plugin.state_save() {
                Ok(s) => (s, None),
                Err(e) => {
                    let reason = format!("{e:#}");
                    tracing::error!(
                        device_id,
                        plugin = rec.plugin.name(),
                        error = %reason,
                        "state_save failed (= SlotState.error 経由で daw_gui に通知)",
                    );
                    (None, Some(reason))
                }
            };
            out.push(SlotState {
                device_id,
                data,
                ara_archive: rec.plugin.store_ara_archive(),
                error,
            });
        }
        out
    }

    // ----------------------------------------------------------------
    // GUI
    // ----------------------------------------------------------------

    /// open the plugin editor inside a top-level window THIS process owns.
    /// On success the `EditorWindow` is stored in the record. On failure the
    /// plugin GUI and the window are torn down before returning.
    ///
    /// # 順序 (r.md #65)
    ///
    /// VST3 SDK editorhost (`window.cpp::show` + `WindowController::onShow`) と
    /// CLAP `gui.h` L19-33 に合わせた **「窓は隠したまま作る → プラグインを attach →
    /// 最後に 1 回だけ見せて前面化」**。
    ///
    /// 旧実装は `EditorWindow::create` の中で即 `ShowWindow(SW_SHOW)` していたため、
    /// (a) 中身の無いフレームが一瞬見え、(b) その表示で得た activation を attach 中に
    /// プラグインが作る一時 top-level に奪われ、(c) 最後の `SetForegroundWindow` は
    /// **ワンショットの `AllowSetForegroundWindow` 許可を使い切った後**なので拒否され得た。
    /// = 「タイトルバーが一瞬アクティブ色になってすぐ戻る」。activation を **1 回に
    /// まとめる**のが構造的な解。
    fn open_gui(
        &mut self,
        device_id: u64,
        title: &str,
        saved: Option<common::model::EditorWindowGeometry>,
        owner: editor_window::OwnerBinding,
    ) -> Result<Option<common::model::EditorWindowGeometry>> {
        let Some(rec) = self.instances.get_mut(&device_id) else {
            return Ok(None);
        };
        let plugin = &mut rec.plugin;
        if !plugin.gui_is_embed_supported() {
            tracing::warn!(plugin = %plugin.name(), "plugin does not support embedded win32 gui");
            return Ok(None);
        }
        // CLAP embedded GUI sequence per gui.h:
        //   create → set_scale → can_resize → get_size → set_parent → show
        // set_size は初回 open では呼ばない (VCV Rack 対策、CLAUDE.md 参照)。
        plugin.gui_create_embedded()?;

        // pre-attach の可否。attach 後に再 query して貼り替える (Arturia 系は
        // attach 前に答えられない)。
        // **生の戻り値ごとログする** (r.md #65): 「false だった」だけだと、
        // プラグインが不可と答えたのか view が無くて問い合わせられなかったのかを
        // 区別できない。`phase` で pre/post を必ず分けて出す (post の行が本当に
        // 再 query の結果なのか、pre の値の再掲なのかもログから判る)。
        let probe = plugin
            .gui_sizer()
            .map_or_else(plugin_instance::ResizableProbe::unavailable, |s| s.can_resize());
        // r.md #65: 枠を出すかは **方針** (`should_offer_resize_frame`) が決める。
        // 申告値は捨てずにログへ (`verdict` / `queried` / `raw`)。
        let resizable = plugin_instance::should_offer_resize_frame(&probe);
        tracing::info!(
            target: "editor_resize",
            plugin = %plugin.name(),
            phase = "pre-attach",
            verdict = probe.verdict,
            queried = probe.queried,
            raw = format!("{:#x}", probe.raw),
            resize_frame = resizable,
            "canResize / can_resize probed"
        );
        // Default to a sane size when the pre-attach query is missing or
        // 0×0 (some VST3 editors only know their size after `attached`).
        let size = plugin
            .gui_get_size()
            .filter(|&(w, h)| w > 0 && h > 0)
            .unwrap_or((800, 600));
        tracing::info!(
            plugin = %plugin.name(),
            resizable,
            width = size.0,
            height = size.1,
            has_saved_geometry = saved.is_some(),
            "plugin gui initial size"
        );

        // Create the host-owned top-level container (**hidden**).
        // r.md #65: owner = daw_gui の本体窓。これで
        // "An owned window is always above its owner in the z-order" が効き、
        // エディタ窓が本体窓の後ろに回らなくなる (REAPER の FX 窓と同じ構成)。
        let mut editor = match editor_window::EditorWindow::create(
            size.0,
            size.1,
            title,
            resizable,
            saved.map(|g| (g.x, g.y)),
            owner,
        ) {
            Ok(w) => w,
            Err(e) => {
                plugin.gui_destroy();
                return Err(anyhow::anyhow!("create editor window: {e}"));
            }
        };
        // C1 (r.md #8): host HWND の DPI を query して set_scale に渡す。
        let dpi_scale = editor_window::window_dpi_scale(editor.hwnd_u64());
        if let Err(e) = plugin.gui_set_scale(dpi_scale) {
            tracing::warn!(error = ?e, scale = dpi_scale, "gui.set_scale failed (ignored)");
        }
        if (dpi_scale - 1.0).abs() > f64::EPSILON
            && let Some((sw, sh)) = plugin.gui_get_size().filter(|&(w, h)| w > 0 && h > 0)
        {
            editor.set_client_size(sw, sh);
        }

        // sizer は **attach の直前**に差し込む。
        // - 早すぎると (scale 反映のリサイズ等で) `attached` より前に `onSize` を
        //   呼んでしまう。editorhost はそんな呼び方をしない。
        // - 遅すぎると `attached` の内側から来る `resizeView` を捌けない
        //   (`iplugview.h`: *"in this call the plug-in could call a
        //   IPlugFrame::resizeView ()!"*)。
        if let Some(sizer) = plugin.gui_sizer() {
            editor.attach_sizer(sizer);
        }

        if let Err(e) = plugin.gui_set_parent_hwnd(editor.hwnd_u64()) {
            // `editor` drops here → DestroyWindow.
            plugin.gui_destroy();
            drop(editor);
            return Err(e);
        }

        // Some plugins post themselves an internal "finish init" message
        // from inside set_parent — pump once before show.
        pump_pending_messages();

        // r.md #65: attach 直後の view (HWND + style) を基準値として控える。
        // 以後 `WM_ACTIVATE` のたびに突き合わせ、「view がコンテナから外れて
        // top-level popup に化けた」瞬間を **ログだけで** 特定できるようにする。
        editor.record_view_baseline();

        match plugin.gui_show() {
            Ok(true) => {}
            Ok(false) => {
                // VCV Rack 2 returns false even though its GUI is visible.
                tracing::warn!(
                    plugin = %plugin.name(),
                    "gui.show returned false; keeping GUI alive (plugin may have already shown itself)"
                );
            }
            Err(e) => {
                plugin.gui_destroy();
                drop(editor);
                return Err(e);
            }
        }

        // attach 後の可否でスタイルを貼り替える (pre-attach 値の焼き込みは
        // 「attach 後にしか答えないプラグインが恒久的に固定枠になる」退行を作る)。
        let probe_now = plugin
            .gui_sizer()
            .map_or_else(plugin_instance::ResizableProbe::unavailable, |s| s.can_resize());
        let resizable_now = plugin_instance::should_offer_resize_frame(&probe_now);
        tracing::info!(
            target: "editor_resize",
            plugin = %plugin.name(),
            phase = "post-attach",
            verdict = probe_now.verdict,
            queried = probe_now.queried,
            raw = format!("{:#x}", probe_now.raw),
            resize_frame = resizable_now,
            changed_from_pre_attach = resizable_now != resizable,
            "canResize / can_resize probed"
        );
        if resizable_now != resizable {
            editor.set_resizable(resizable_now);
        }

        // re-query the size AFTER attach/show (Arturia 系は attach 前 0×0)。
        let plugin_size = plugin
            .gui_get_size()
            .filter(|&(w, h)| w > 0 && h > 0)
            .unwrap_or(size);
        // 前回セッションのサイズは **resizable のときだけ**復元する
        // (CLAP `gui.h` の手順 9 と同じ規約)。矯正を通してからホスト起点として
        // 適用する = `WM_SIZE` がプラグインへ `onSize` / `set_size` を届ける。
        let target_size = match saved.filter(|_| resizable_now) {
            Some(g) if g.width > 0 && g.height > 0 => plugin
                .gui_sizer()
                .map_or((g.width, g.height), |s| {
                    s.constrain_client_size(g.width, g.height)
                }),
            _ => plugin_size,
        };
        editor.set_client_size(target_size.0, target_size.1);

        // 表示 + 前面化はここ 1 回だけ。
        editor.show_and_focus();
        let geometry = editor.geometry();
        // open 中の `set_client_size` / `set_resizable` が `WM_SIZE` / `WM_MOVE` を
        // 起こして dirty を立てているので、ここで**食べておく**。残すと直後の
        // `poll_editor_geometry` が同じ値をもう 1 通送る。
        let _ = editor.take_geometry_change();
        rec.editor = Some(editor);

        tracing::info!(
            plugin = %rec.plugin.name(),
            width = geometry.width,
            height = geometry.height,
            x = geometry.x,
            y = geometry.y,
            resizable = resizable_now,
            "plugin gui opened"
        );
        Ok(Some(geometry))
    }

    /// close the editor for `device_id`: tear the plugin GUI down
    /// (`gui_hide` → `gui_destroy`) BEFORE destroying the container window,
    /// then notify daw_gui. Idempotent.
    fn close_slot_gui(&mut self, device_id: u64) {
        // r.md #65: 窓を壊す前に最後のジオメトリを送る (次回 open で復元する)。
        // ドラッグ確定時にも送っているが、閉じる直前の 1 発が「最後に見た形」を確定させる。
        // **最小化したまま閉じた場合は送らない** (`persistable_geometry` が None) —
        // `(-32000,-32000)` / `0×0` を保存すると次回 1×1 の窓で開いてしまう。
        let last_geometry = self
            .instances
            .get(&device_id)
            .and_then(|rec| rec.editor.as_ref())
            .and_then(editor_window::EditorWindow::persistable_geometry);
        if let Some(geometry) = last_geometry {
            self.emit(PluginEvent::SlotGuiGeometry { device_id, geometry });
        }
        // r.md #65: 破棄すると **アクティブは owner ではなく Alt+Esc 順の次の窓**へ
        // 移る (window-features "Destroying a Window")。前面が自分のグループだった
        // ときだけ、破棄後に owner (daw_gui 本体窓) へ明示的に戻す。判定は破棄の
        // **前**に取る (破棄後では自分の窓がもう無く、判定材料が消えている)。
        let restore_foreground = self
            .instances
            .get(&device_id)
            .and_then(|rec| rec.editor.as_ref())
            .and_then(editor_window::EditorWindow::owner_to_restore_if_foreground);
        if let Some(rec) = self.instances.get_mut(&device_id) {
            let _ = rec.plugin.gui_hide();
            rec.plugin.gui_destroy();
            // Drop = DestroyWindow, run after gui_destroy detached the child.
            rec.editor = None;
        }
        if let Some(owner) = restore_foreground {
            editor_window::restore_foreground_to_owner(owner);
        }
        // r.md #36: 窓を壊すと対応する key-up はもう届かない (`editor_device_of` が
        // None を返す) ので、 横取り中の記録をここで捨てる。 残すと次に別のキーの
        // key-up を誤って飲み込む。
        self.swallowed_keys.clear();
        // r.md #49: 窓が 1 枚も無いプロセスは定義上アクティブでない。アクティブな窓を
        // 閉じた場合 activation は他プロセスへ移るが、破棄済みの窓に `WM_ACTIVATEAPP`
        // は届かないので、ここで明示的に落とす (これが無いと daw_gui が
        // 「プラグイン GUI がアクティブなまま」と誤認して永久に park しない)。
        if self.instances.values().all(|r| r.editor.is_none()) {
            editor_window::clear_windows_active();
        }
        self.emit(PluginEvent::SlotGuiClosed { device_id });
    }

    fn handle_notify(&mut self, n: HostNotify) {
        match n {
            // r.md #65: プラグイン起点 resize の **非同期フォールバック**。
            // 通常は `Vst3PlugFrame::resizeView` / CLAP `request_resize` が
            // `editor_window::plugin_requested_resize` で同期処理を完了させるので
            // ここには来ない。来るのは (a) GUI をまだ開いていない、(b) plugin-main
            // 以外のスレッドから CLAP `request_resize` (= `[thread-safe]`) が来た、の 2 つ。
            // **同じ関数へ合流させる**ので、経路が 2 本でも実装は 1 本 (SSoT)。
            HostNotify::Resize(device_id, w, h) => {
                let Some(rec) = self.instances.get(&device_id) else {
                    return;
                };
                let Some(win) = rec.editor.as_ref() else {
                    // GUI 未 open。窓が無いので何もできない (次の open が
                    // プラグインの `get_size` を読んで正しいサイズで作る)。
                    return;
                };
                let outcome = editor_window::plugin_requested_resize(win.hwnd_u64(), w, h);
                if outcome != editor_window::PluginResizeOutcome::Applied {
                    tracing::debug!(device_id, w, h, ?outcome, "deferred plugin resize not applied");
                }
            }
            HostNotify::Closed(device_id) => {
                self.close_slot_gui(device_id);
            }
            // CLAP `clap_host_gui.request_show` / `request_hide`。
            // r.md #65: show は `hide()` で隠した窓を **見せ直せる**ようにする
            // (`SetForegroundWindow` だけでは非表示の窓は出てこない)。
            HostNotify::Show(device_id, show) => {
                if let Some(rec) = self.instances.get(&device_id)
                    && let Some(win) = rec.editor.as_ref()
                {
                    if show {
                        win.show_and_focus();
                    } else {
                        win.hide();
                    }
                }
            }
            HostNotify::Restart(device_id, kind) => {
                self.handle_restart(device_id, kind);
            }
            HostNotify::MainThreadCallback(device_id) => {
                // CLAP `request_callback` → `on_main_thread()` (この thread =
                // CLAP main-thread)。
                if let Some(rec) = self.instances.get_mut(&device_id) {
                    rec.plugin.on_main_thread();
                }
            }
            HostNotify::LatencyChanged(device_id) => {
                self.requery_latency_and_emit(device_id);
            }
            HostNotify::ParamsRescan(device_id) => {
                self.emit_param_list(device_id);
            }
        }
    }

    /// r.md #65: エディタ窓の位置 / サイズが確定した分を daw_gui へ流す。
    /// ドラッグ中は送らない (`WM_EXITSIZEMOVE` / プラグイン起点 resize 完了で
    /// 1 回 dirty が立つ)。
    fn poll_editor_geometry(&mut self) {
        let changes: Vec<(u64, common::model::EditorWindowGeometry)> = self
            .instances
            .iter()
            .filter_map(|(&id, rec)| {
                rec.editor
                    .as_ref()
                    .and_then(|w| w.take_geometry_change())
                    .map(|g| (id, g))
            })
            .collect();
        for (device_id, geometry) in changes {
            self.emit(PluginEvent::SlotGuiGeometry { device_id, geometry });
        }
    }

    /// r.md #65: modal move/size ループ (キャプション / サイズ枠のドラッグ) の
    /// **最中に**回してよい周期処理だけを実行する。
    ///
    /// **command / notify の drain はここでやらない**。`CloseSlotGui` や
    /// `HostNotify::Closed` は `DestroyWindow` に至り、いま WNDPROC が実行中の
    /// その窓を壊してしまう (modal ループの内側からの自窓破棄)。ドラッグが終われば
    /// `DefWindowProc` が返って外側の loop 先頭へ戻り、そこで即座に drain される
    /// ので、遅れるだけで落ちるものは無い。
    fn pump_modal(&mut self) {
        // (r.md #5 ARA2) Melodyne の解析はここが止まると進まない。
        self.notify_ara_model_updates();
        // r.md #49: ドラッグ中に activation が変わることがある (別プロセスへ移る等)。
        if let Some(active) = editor_window::take_activation_change() {
            self.emit(PluginEvent::HostWindowsActive(active));
        }
    }

    /// editor 窓が閉じられた分の close 処理。入口は 2 つある:
    ///
    /// 1. user が ✕ を押した (WNDPROC が flag を立て、この loop が poll する)。
    /// 2. **窓が我々の与り知らぬところで消えた** (r.md #65)。owner を daw_gui の
    ///    本体窓にしたので、daw_gui が異常終了すると Windows が
    ///    *"The system automatically destroys an owned window when its owner is destroyed."*
    ///    に従ってコンテナごと破棄する。このとき ✕ は押されないので 1. では拾えない。
    ///
    /// どちらも `close_slot_gui` の同じ close flow
    /// (`gui_destroy` → editor drop → `SlotGuiClosed`) に合流させる。
    /// **plugin に `removed()` を通す経路を飛ばさない**のが要点で、飛ばすと
    /// `IPlugView` が attach 済みのまま残る。
    fn poll_editor_close_requests(&mut self) {
        let to_close: Vec<u64> = self
            .instances
            .iter()
            .filter(|(_, rec)| {
                rec.editor.as_ref().is_some_and(|w| {
                    // `take_close_request` は副作用 (flag を落とす) があるので先に評価する。
                    let requested = w.take_close_request();
                    if requested {
                        // r.md #65: `WM_CLOSE` のログと対にして「✕ が押された」→
                        // 「poll が拾った」→「破棄した」の 3 点が全部見えるようにする
                        // (どこで切れているかを 1 往復で特定するため)。
                        tracing::info!(
                            target: "editor_resize",
                            hwnd = format!("{:#x}", w.hwnd_u64()),
                            "close request picked up by the plugin-main poll"
                        );
                        return true;
                    }
                    if !w.is_window_alive() {
                        tracing::warn!(
                            target: "editor_resize",
                            hwnd = format!("{:#x}", w.hwnd_u64()),
                            "editor container vanished without a close request \
                             (owner destroyed?) — running the normal close flow"
                        );
                        return true;
                    }
                    false
                })
            })
            .map(|(&id, _)| id)
            .collect();
        for device_id in to_close {
            self.close_slot_gui(device_id);
        }
    }

    fn has_any_ara_session(&self) -> bool {
        self.instances.values().any(|r| r.plugin.has_ara_session())
    }

    fn notify_ara_model_updates(&mut self) {
        for rec in self.instances.values_mut() {
            rec.plugin.notify_ara_model_updates();
        }
    }

    /// process 終了時の teardown。worker pool を先に閉じてから plugin を
    /// drop する (順序を逆にすると UAF)。
    ///
    /// (r.md #61) **plugin の畳み方は [`Self::teardown_device`] 一本**。
    /// 旧実装はここだけ `gui_destroy` + `instances.clear()` という独自順序で、
    /// `stop_processing` / `deactivate` を通らなかった。`ClapPlugin::drop` は
    /// ARA session がある場合しか deactivate しないので、通常の CLAP プラグインは
    /// **active のまま `clap_plugin.destroy`** に入っていた
    /// (CLAP `plugin.h`: `destroy` は `[main-thread & !active]`、
    /// "It is required to deactivate the plugin prior to this call")。
    ///
    /// `UnloadAllPlugins` と同じ列挙 (= plugin_host 自身の `instances`) と
    /// 同じ teardown を通すので、「全部畳め」の実装は 1 つしかない。
    fn shutdown(mut self) {
        // (1) worker pool を先に止める。`shutdown` は全 worker thread を join
        //     するので、以後 registry を触る RT スレッドは存在しない
        //     (= teardown_device の `detach_and_quiesce` より強い保証)。
        if let Some(pool) = self.worker_pool.take() {
            pool.shutdown();
        }
        // (2) 全 device を正規経路で畳む。`emit_unloaded` は false — device が
        //     「Song から外れた」のではなくプロセスが終わるだけで、受け手の
        //     daw_gui も同時に畳まれている。
        let ids: Vec<u64> = self.instances.keys().copied().collect();
        tracing::info!(count = ids.len(), "shutdown: tearing down all devices");
        for id in ids {
            self.teardown_device(id, false);
        }
    }
}

// --- modal move/size ループ中の pump ---------------------------------------
//
// キャプション / サイズ枠をドラッグしている間、plugin-main は `DefWindowProc` の
// 内側の modal ループにいて、外側の `GetMessageW` ループは回らない
// (MSDN WM_ENTERSIZEMOVE: *"The operation is complete when DefWindowProc returns."*)。
// エディタ窓の WNDPROC が `WM_TIMER` から [`pump_host_during_modal_loop`] を呼び、
// **再入して安全な周期処理だけ** ([`PluginHost::pump_modal`]) を回す。

thread_local! {
    /// `DispatchMessageW` を呼ぶ **直前にだけ**立てる `PluginHost` への借用口。
    /// その瞬間、呼び出し側フレームには `host` への Rust 参照が 1 つも生きていない
    /// ので、WNDPROC から `&mut` を作っても aliasing しない。
    ///
    /// `pump_pending_messages()` (= `open_gui` が `&mut self` を握ったまま呼ぶ) の
    /// 側では **立てない**ので、そちらから WM_TIMER が来ても null で弾かれる。
    static MODAL_PUMP_HOST: std::cell::Cell<*mut PluginHost> =
        const { std::cell::Cell::new(std::ptr::null_mut()) };
    /// 入れ子の pump を禁止するガード (`pump_modal` の中で更に dispatch が起きても
    /// `&mut` は 1 本しか作らない)。
    static IN_MODAL_PUMP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// [`IN_MODAL_PUMP`] を **Drop で必ず**降ろすためのガード。
///
/// 素の `set(true) ... set(false)` だと、`pump_modal` が呼ぶ ARA プラグインの
/// コードが unwind した瞬間にフラグが true に貼り付き、以後 modal ドラッグ中の
/// 周期処理が二度と回らなくなる (Melodyne の解析が窓ドラッグ後に永久停止する等)。
struct ModalPumpGuard;

impl Drop for ModalPumpGuard {
    fn drop(&mut self) {
        IN_MODAL_PUMP.set(false);
    }
}

/// エディタ窓の WNDPROC (modal move/size ループ中の `WM_TIMER`) から呼ぶ。
/// 借用口が立っていない / 既に pump 中なら何もしない。
pub fn pump_host_during_modal_loop() {
    if IN_MODAL_PUMP.get() {
        return;
    }
    let ptr = MODAL_PUMP_HOST.get();
    if ptr.is_null() {
        return;
    }
    IN_MODAL_PUMP.set(true);
    let _guard = ModalPumpGuard;
    // SAFETY: `ptr` は `plugin_main_loop` が `DispatchMessageW` の直前に自分の
    // `host` から作り、直後に null へ戻す。その区間で `host` への参照は 1 つも
    // 生きておらず、`IN_MODAL_PUMP` が再入も塞ぐので `&mut` は一意。
    unsafe { (*ptr).pump_modal() };
}

/// `DispatchMessageW` の呼び出しを [`MODAL_PUMP_HOST`] の設定で挟む。
/// 借用口は **この関数の中だけ**で有効。
fn dispatch_with_modal_pump(host: &mut PluginHost, msg: &MSG) {
    MODAL_PUMP_HOST.set(std::ptr::from_mut(host));
    unsafe {
        let _ = TranslateMessage(msg);
        DispatchMessageW(msg);
    }
    MODAL_PUMP_HOST.set(std::ptr::null_mut());
}

// --- Plugin-main thread loop ----------------------------------------------

fn plugin_main_loop(
    session: AudioSession,
    cmd_rx: mpsc::Receiver<HostMsg>,
    evt_tx: tmpsc::UnboundedSender<PluginEvent>,
) {
    // plugin 発の非同期 host 要求 (resize / close / restart / ...) を
    // callback → channel → この loop で直列化する。tokio unbounded sender
    // は `Send + Sync` (HostCallbacks の閉包要件)。
    let (notify_tx, mut notify_rx) = tmpsc::unbounded_channel::<HostNotify>();
    let mut host = PluginHost::new(session, evt_tx, notify_tx);

    tracing::info!("plugin-main thread running");

    // (r.md #5 ARA2) ARA notify timer (hwnd = None → WM_TIMER がこの thread
    // のキューに入り GetMessageW を起こす)。ARA session が居るときだけ回す。
    let mut ara_timer_active = false;

    // (r.md #61) 抜け方は 3 通り (cmd channel 切断 / `HostMsg::Shutdown` /
    // `GetMessageW` の WM_QUIT・エラー) あるが、**後始末は loop の直後 1 箇所**に
    // 集約する。旧実装は 3 箇所に散っていて、Disconnected 経路だけ `KillTimer` を
    // 呼ばない非対称があった。
    let exit_reason = 'main: loop {
        loop {
            let msg = match cmd_rx.try_recv() {
                Ok(m) => m,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'main "cmd channel disconnected",
            };
            match msg {
                HostMsg::Shutdown => break 'main "Shutdown",
                HostMsg::Cmd(cmd) => host.handle_command(cmd),
            }
        }

        // plugin 発の非同期要求 (resize / close / show / restart /
        // main-thread callback / latency / rescan) を drain。
        while let Ok(n) = notify_rx.try_recv() {
            host.handle_notify(n);
        }

        // editor 窓の ✕ (close flag) を poll。
        host.poll_editor_close_requests();

        // r.md #65: 窓の位置 / サイズが確定した分を daw_gui へ (次回 open で復元)。
        host.poll_editor_geometry();

        // r.md #49: エディタ窓の activation 変化を daw_gui へ報告する。close の
        // poll より後に置く — 最後のエディタを閉じた周回で `close_slot_gui` が
        // 非アクティブを強制するので、同じ周回でそれを送り出せる。
        if let Some(active) = editor_window::take_activation_change() {
            host.emit(PluginEvent::HostWindowsActive(active));
        }

        // ARA notify timer を live session の有無に同期。
        let want_ara_timer = host.has_any_ara_session();
        if want_ara_timer && !ara_timer_active {
            unsafe { SetTimer(None, ARA_NOTIFY_TIMER_ID, ARA_NOTIFY_TIMER_MS, None) };
            ara_timer_active = true;
        } else if !want_ara_timer && ara_timer_active {
            let _ = unsafe { KillTimer(None, ARA_NOTIFY_TIMER_ID) };
            ara_timer_active = false;
        }

        unsafe {
            let mut msg = MSG::default();
            let ret = GetMessageW(&mut msg, Some(HWND(std::ptr::null_mut())), 0, 0);
            if ret.0 <= 0 {
                break 'main "WM_QUIT";
            }
            if msg.message == WM_TIMER && msg.wParam.0 == ARA_NOTIFY_TIMER_ID {
                // (r.md #5 ARA2) Pump every ARA document's deferred analysis.
                host.notify_ara_model_updates();
            } else if host.handle_editor_key(&msg) {
                // r.md #36: プラグインが消化しなかった転送対象キー。 daw_gui へ渡し、
                // TranslateMessage / DispatchMessageW は呼ばない (= WM_CHAR も出ないので
                // プラグインのテキスト欄に空白が入らない)。
            } else if msg.message != WM_COMMAND_WAKE {
                // r.md #65: この dispatch の中でキャプション / サイズ枠のドラッグが
                // 始まると `DefWindowProc` の modal ループへ入って戻ってこない。
                // 借用口を立てて、WNDPROC の `WM_TIMER` から周期処理を回せるようにする。
                dispatch_with_modal_pump(&mut host, &msg);
            }
        }
    };

    if ara_timer_active {
        let _ = unsafe { KillTimer(None, ARA_NOTIFY_TIMER_ID) };
    }
    host.shutdown();
    tracing::info!(reason = exit_reason, "plugin-main thread exiting");
}

/// CLAP / VST3 spec の teardown 順 (stop_processing → deactivate →
/// gui_destroy → drop) を実行する。呼び出し側は事前に registry detach +
/// `WorkerPool::quiesce` を済ませて、worker からの参照が無いことを保証する。
fn teardown_plugin(mut plugin: Box<dyn LoadedPlugin>) {
    plugin.stop_processing();
    plugin.deactivate();
    plugin.gui_destroy();
    drop(plugin);
}

/// Non-blocking drain of pending Win32 messages on the current thread. Used
/// between CLAP GUI calls that rely on a host message pump being present.
fn pump_pending_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_COMMAND_WAKE {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// --- pipe_loop: multiplex read (commands) + write (events) ---------------

async fn pipe_loop(
    pipe: NamedPipeClient,
    plugin: PluginThreadSender,
    mut evt_rx: tmpsc::UnboundedReceiver<PluginEvent>,
) {
    // wire.rs の framing は cancellation-unsafe なので pipe を read/write
    // half に split して別タスクで回し、read を絶対に cancel しない
    // (daw_audio::recv_loop と同じ pattern)。
    let (mut read_half, mut write_half) = tokio::io::split(pipe);
    let writer = tokio::spawn(async move {
        while let Some(evt) = evt_rx.recv().await {
            if let Err(e) = write_msg(&mut write_half, &evt).await {
                tracing::error!(error = ?e, ?evt, "failed to forward plugin event");
                break;
            }
        }
    });
    loop {
        match read_msg::<_, PluginCommand>(&mut read_half).await {
            // (r.md #61) 正常終了要求。plugin-main へ転送せず **ここで loop を
            // 抜ける** — 後段の `plugin_thread.shutdown()` が `HostMsg::Shutdown`
            // を送って同じ teardown を走らせるので、終わり方は 1 本のまま。
            // 親 crash 時の pipe EOF (下の `Err` 枝) もそこへ合流する。
            Ok(PluginCommand::Shutdown) => {
                tracing::info!("received Shutdown");
                break;
            }
            Ok(m) => {
                log_command(&m);
                plugin.send(HostMsg::Cmd(m));
            }
            Err(e) => {
                tracing::info!(error = ?e, "pipe ended");
                break;
            }
        }
    }
    // teardown 中に emit される `SlotPluginShmemReleased` 等は、送り先の
    // daw_gui も同時に畳まれている (= Shutdown を送ったのは終了シーケンス
    // だけ) か既に消えている (= pipe EOF) ので、ここで writer を落として
    // よい。`emit` の `send` は receiver 消滅で失敗するが握り潰される。
    writer.abort();
}

/// 受信 command の diagnosis ログ (payload の大きい variant は要約)。
fn log_command(cmd: &PluginCommand) {
    match cmd {
        PluginCommand::SetSlotPlugin {
            device_id,
            track_id,
            format,
            path,
            plugin_id,
            initial_state,
            generation,
        } => {
            tracing::info!(
                device_id,
                track_id,
                ?format,
                path = %path.display(),
                id = %plugin_id,
                has_state = initial_state.is_some(),
                generation,
                "received SetSlotPlugin"
            );
        }
        PluginCommand::SetBuiltinPluginNoteMetadata { device_id, bpm, chunk_secs, entries, talk } => {
            tracing::debug!(
                device_id,
                bpm,
                chunk_secs,
                count = entries.len(),
                talk = talk.len(),
                "received SetBuiltinPluginNoteMetadata"
            );
        }
        // r.md #75: トランスポート中に毎 tick 来る順序ヒント。**必ず arm を持つこと** —
        // 下の catch-all は `info!` なので、arm が無いとログが優先度ヒントで溢れる。
        PluginCommand::SetVocalSynthPriority { device_id, playhead_beats } => {
            tracing::trace!(device_id, playhead_beats, "received SetVocalSynthPriority");
        }
        PluginCommand::SetupAraDocument { device_id, clips, bpm, archive, .. } => {
            tracing::info!(
                device_id,
                n = clips.len(),
                bpm,
                has_archive = archive.is_some(),
                "received SetupAraDocument"
            );
        }
        PluginCommand::UpdateAraRegions { device_id, regions } => {
            tracing::info!(device_id, n = regions.len(), "received UpdateAraRegions");
        }
        other => {
            tracing::info!(?other, "received command");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// restart cooldown: 10 秒窓で 3 回まで許可、4 回目は拒否、窓が過ぎれば
    /// 再び許可 (Melda 型 reinit 無限ループの構造的防御)。
    #[test]
    fn restart_cooldown_limits_bursts() {
        let mut w = RestartWindowTracker::default();
        let t0 = Instant::now();
        assert!(w.allow(t0));
        assert!(w.allow(t0 + Duration::from_secs(1)));
        assert!(w.allow(t0 + Duration::from_secs(2)));
        // 4 回目 (窓内) は拒否。
        assert!(!w.allow(t0 + Duration::from_secs(3)));
        assert!(!w.allow(t0 + Duration::from_secs(9)));
        // 窓 (10s) が過ぎた要求は最古が expire して再び許可される。
        assert!(w.allow(t0 + Duration::from_secs(11)));
    }
}
