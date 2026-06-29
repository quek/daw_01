// release ではコンソール窓を出さない (windows-subsystem)。 debug は console の
// まま (standalone 起動時に stdout/tracing が見える)。 docs/plan_icon_and_console.md (#48)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ara;
mod builtin;
mod clap_host;
mod clap_plugin;
mod editor_window;
mod plugin_instance;
mod process_server;
mod vst3_events;
mod vst3_host;
mod vst3_params;
mod vst3_plugin;
mod vst3_stream;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use common::plugin_format::PluginFormat;
use common::protocol::{
    AudioSession, ChildKind, ChildToMain, MainToChild, RenderMode, SlotState,
};
use common::wire::{read_msg, write_msg};
use tokio::net::windows::named_pipe::NamedPipeClient;
use tokio::sync::mpsc as tmpsc;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_REMOVE, PeekMessageW, PostThreadMessageW,
    TranslateMessage, WM_APP,
};

use crate::plugin_instance::{HostCallbacks, LoadedPlugin, load_plugin};

/// Custom Win32 message id used to wake the plugin-main thread's `GetMessage`
/// loop after a command has been pushed into the mpsc queue.
const WM_COMMAND_WAKE: u32 = WM_APP + 1;

/// Track-and-device-index-addressed events pushed from the plugin-main thread
/// (or its CLAP callbacks) to the IPC sender.
#[derive(Debug, Clone)]
pub enum PluginEvent {
    /// Every plugin reinitialised (deactivate→activate) to a clean state
    /// (reply to `PluginCommand::ReinitAllPlugins` — export prep or
    /// the panic button). `From<PluginEvent>` maps to
    /// `ChildToMain::PluginsReinitDone`.
    PluginsReinitDone,
    SlotGuiOpened {
        track: u32,
        index: u32,
        width: u32,
        height: u32,
    },
    SlotGuiClosed {
        track: u32,
        index: u32,
    },
    /// builtin VOICEVOX の歌唱合成が要求世代まで完了 (or timeout) した。
    /// `From<PluginEvent> for ChildToMain` が `ChildToMain::VocalSynthReady` に変換。
    VocalSynthReady {
        plugin_id: u32,
    },
    /// builtin VOICEVOX の synth thread の状態遷移 (busy / failing)。
    /// `set_voicevox_status_reporter` で仕込んだ callback が任意スレッドから emit する。
    /// `ChildToMain::VoicevoxSynthStatus` に変換して daw_gui へ継続報告する。
    VoicevoxSynthStatus {
        plugin_id: u32,
        busy: bool,
        failing: bool,
    },
    SlotPluginLoaded {
        track: u32,
        index: u32,
        id: String,
        name: String,
        plugin_id: u32,
        shmem_id: String,
        state_load_error: Option<String>,
        aux_output_count: u8,
    },
    SlotPluginState {
        track: u32,
        index: u32,
        data: Option<Vec<u8>>,
    },
    AllPluginStates {
        entries: Vec<SlotState>,
    },
    /// Plugin destroyed (RemoveSlotPlugin / RemoveTrack 経由)。 daw_gui が
    /// `MainToChild::ClosePluginShmem { plugin_id }` を daw_audio に
    /// 転送して plugin_refs / slot_to_plugin_id を整理させるための通知。
    SlotPluginUnloaded {
        plugin_id: u32,
    },
    /// PR3.3: plugin が報告した自身の processing latency。 plugin が
    /// activate された直後 (CLAP `activate` / VST3 `setActive(true)`) に
    /// query して emit。 daw_gui へ forward され、 schedule に反映される。
    PluginLatencyChanged {
        plugin_id: u32,
        samples: u32,
    },
    /// Phase 2 (`docs/plan_automation.md` §7.5): plugin の parameter
    /// 一覧。 activate / state restore 完了後に 1 度 emit。
    PluginParamList {
        track: u32,
        index: u32,
        plugin_id: u32,
        params: Vec<common::protocol::PluginParamInfo>,
        has_embedded_gui: bool,
    },
    /// Phase 2c: plugin GUI で knob を touch した通知 (CLAP
    /// PARAM_GESTURE_BEGIN out event 経由)。 process_server で drain
    /// して emit、 daw_gui に転送して last_touched_param を更新させる。
    PluginParamTouched {
        track: u32,
        index: u32,
        plugin_id: u32,
        param_id: u32,
    },
    /// Phase 2c: plugin GUI で knob 値を変更した通知 (CLAP PARAM_VALUE
    /// out event 経由)。 Phase 4 recording mode で point 生成 source。
    PluginParamValueChanged {
        track: u32,
        index: u32,
        plugin_id: u32,
        param_id: u32,
        value: f64,
    },
    /// Phase 4 Step C-3: plugin GUI で knob を release した通知 (CLAP
    /// PARAM_GESTURE_END out event 経由)。 daw_gui で
    /// `active_param_gestures` から該当 PluginParam target を remove する。
    PluginParamGestureEnd {
        track: u32,
        index: u32,
        plugin_id: u32,
        param_id: u32,
    },
    /// `SetSlotPlugin` の load が失敗した (`load_plugin` Err か
    /// `ProcessDataHandle::create` Err)。 daw_gui の `pending_plugin_loads`
    /// を解放するために emit する。 emit せずに `continue` だけで戻ると
    /// pending stuck で Play queue が永久に解放されない。
    PluginLoadFailed {
        track: u32,
        index: u32,
        plugin_id: String,
        reason: String,
    },
}

impl From<PluginEvent> for ChildToMain {
    fn from(e: PluginEvent) -> Self {
        match e {
            PluginEvent::SlotGuiOpened { track, index, width, height } => {
                ChildToMain::SlotGuiOpened { track, index, width, height }
            }
            PluginEvent::SlotGuiClosed { track, index } => {
                ChildToMain::SlotGuiClosed { track, index }
            }
            PluginEvent::VocalSynthReady { plugin_id } => {
                ChildToMain::VocalSynthReady { plugin_id }
            }
            PluginEvent::VoicevoxSynthStatus { plugin_id, busy, failing } => {
                ChildToMain::VoicevoxSynthStatus { plugin_id, busy, failing }
            }
            PluginEvent::PluginsReinitDone => ChildToMain::PluginsReinitDone,
            PluginEvent::SlotPluginLoaded {
                track,
                index,
                id,
                name,
                plugin_id,
                shmem_id,
                state_load_error,
                aux_output_count,
            } => ChildToMain::SlotPluginLoaded {
                track,
                index,
                id,
                name,
                plugin_id,
                shmem_id,
                state_load_error,
                aux_output_count,
            },
            PluginEvent::SlotPluginState { track, index, data } => {
                ChildToMain::SlotPluginState { track, index, data }
            }
            PluginEvent::AllPluginStates { entries } => ChildToMain::AllPluginStates { entries },
            PluginEvent::SlotPluginUnloaded { plugin_id } => {
                ChildToMain::SlotPluginUnloaded { plugin_id }
            }
            PluginEvent::PluginLatencyChanged { plugin_id, samples } => {
                ChildToMain::PluginLatencyChanged { plugin_id, samples }
            }
            PluginEvent::PluginParamList {
                track,
                index,
                plugin_id,
                params,
                has_embedded_gui,
            } => ChildToMain::PluginParamList {
                track,
                index,
                plugin_id,
                params,
                has_embedded_gui,
            },
            PluginEvent::PluginParamTouched {
                track,
                index,
                plugin_id: _,
                param_id,
            } => ChildToMain::PluginParamTouched {
                track,
                index,
                param_id,
                // display_name は daw_gui 側で AppData.plugin_params から
                // 引いて解決する (= host で文字列構築は不要、 IPC
                // payload も短くなる)。
                display_name: format!("Param {param_id}"),
            },
            PluginEvent::PluginParamValueChanged {
                track,
                index,
                plugin_id: _,
                param_id,
                value,
            } => ChildToMain::PluginParamValueChanged {
                track,
                index,
                param_id,
                value,
            },
            PluginEvent::PluginParamGestureEnd {
                track,
                index,
                plugin_id: _,
                param_id,
            } => ChildToMain::PluginParamGestureEnd {
                track,
                index,
                param_id,
            },
            PluginEvent::PluginLoadFailed {
                track,
                index,
                plugin_id,
                reason,
            } => ChildToMain::SlotPluginLoadFailed {
                track,
                index,
                plugin_id,
                reason,
            },
        }
    }
}

/// Atomically publish a new entry (or `None` to remove) in the plugin
/// registry. Clones the current `Vec` so old worker snapshots stay
/// valid until they're dropped.
fn publish_plugin_registry(
    registry: &PluginRegistry,
    plugin_id: u32,
    entry: Option<PluginEntry>,
) {
    let current = registry.load();
    let mut next: Vec<Option<PluginEntry>> = (**current)
        .iter()
        .map(|opt| opt.as_ref().map(|e| PluginEntry {
            plugin: PluginPtr(e.plugin.0),
            process_data: e.process_data,
            track: e.track,
            index: e.index,
        }))
        .collect();
    let idx = plugin_id as usize;
    if next.len() <= idx {
        next.resize_with(idx + 1, || None);
    }
    next[idx] = entry;
    registry.store(std::sync::Arc::new(next));
}

/// Re-publish an existing registry entry with a corrected `(track, index)`
/// address, preserving the live `plugin` pointer and `process_data` slot.
/// Used after `ReorderChain` permutes a chain so `PluginEntry.index` (read
/// by the worker pool to stamp param events) reflects the new position.
/// No-op if the plugin id has no live entry.
fn republish_entry_slot(
    registry: &PluginRegistry,
    plugin_id: u32,
    track: u32,
    index: u32,
) {
    let current = registry.load();
    let Some(Some(existing)) = current.get(plugin_id as usize) else {
        return;
    };
    let entry = PluginEntry {
        plugin: PluginPtr(existing.plugin.0),
        process_data: existing.process_data,
        track,
        index,
    };
    // Drop the load guard before re-entering `publish_plugin_registry`
    // (which takes its own `load()`); avoids holding two guards at once.
    drop(current);
    publish_plugin_registry(registry, plugin_id, Some(entry));
}

/// Commands processed serially on the plugin-main thread.
enum PluginCommand {
    /// Reinitialise (deactivate→activate) every loaded plugin to a clean
    /// state, then reply `PluginEvent::PluginsReinitDone`. Shared by export
    /// prep and the panic button.
    ReinitAllPlugins,
    SetSlotPlugin {
        track: u32,
        index: u32,
        format: PluginFormat,
        path: PathBuf,
        plugin_id: String,
        initial_state: Option<Vec<u8>>,
    },
    RemoveSlotPlugin {
        track: u32,
        index: u32,
    },
    /// 歌唱 bounce 用に、 `plugin_id` の builtin VOICEVOX の合成が
    /// (直前の metadata flush 世代まで) 完了するのを待って `PluginEvent::VocalSynthReady`
    /// を emit する。 plugin-main が builtin の世代 Arc を見て poll thread を spawn する。
    PrepareVocalSynth {
        plugin_id: u32,
    },
    /// Single-chain redesign: apply a complete chain permutation as a live
    /// move. `moves` lists `(old_index, new_index)` for every loaded plugin
    /// on `track` (one entry per device, possibly `old == new`).
    ReorderChain {
        track: u32,
        moves: Vec<(u32, u32)>,
    },
    RemoveTrack {
        track: u32,
    },
    RequestSlotState {
        track: u32,
        index: u32,
    },
    /// (r.md #5 ARA2) Build/replace the ARA document for the plugin at
    /// `track`/`index` from `clips`, binding it for playback rendering.
    SetupAraDocument {
        track: u32,
        index: u32,
        clips: Vec<common::protocol::AraClipSpec>,
    },
    /// (r.md #5 ARA2) Tear down the ARA session for the plugin at `track`/`index`.
    ClearAraDocument {
        track: u32,
        index: u32,
    },
    RequestAllStates,
    OpenSlotGui {
        track: u32,
        index: u32,
        title: String,
    },
    CloseSlotGui {
        track: u32,
        index: u32,
    },
    /// Stand up the per-buffer plugin process worker pool. Drives
    /// `process_server::WorkerPool::open` on the plugin-main thread so
    /// the audio engine on the daw_audio side can dispatch
    /// `plugin.process()` calls via the worker_wake/done event pairs.
    OpenWorkerPool {
        n_workers: u32,
        worker_bridge_shmem_id: String,
        wake_event_names: Vec<String>,
        done_event_names: Vec<String>,
    },
    /// Tear down the worker pool started by `OpenWorkerPool`.
    CloseWorkerPool,
    /// Set every loaded plugin's CLAP render mode (Realtime ↔ Offline).
    /// Sent by daw_audio bookending an offline export.
    SetRenderMode(RenderMode),
    /// Builtin plugin (`PluginFormat::Builtin`) に per-note metadata を
    /// flush する (PR-V2.2)。 plugin-main thread で plugin instance に
    /// `LoadedPlugin::set_note_metadata(bpm, entries)` を呼ぶ。 plugin_id
    /// に該当する slot が無い / CLAP / VST3 plugin の場合は default
    /// no-op で吸収 (= warning も発生しない、 IPC 経路は format-neutral)。
    SetBuiltinPluginNoteMetadata {
        plugin_id: u32,
        bpm: f32,
        entries: Vec<common::plugin_metadata::NoteMetadata>,
        /// (talk) 同トラックの読み上げ群 (`docs/plan_voicevox_talk.md` §3.3)。
        talk: Vec<common::plugin_metadata::TalkMetadata>,
    },
    Shutdown,
}

/// Phase 6 review fix: `shared_memory` crate は Windows で
/// `%TEMP%/shared_memory-rs/<unique_id>` を `create_new(true)` で作り、
/// 重複名 (= 別プロセス / 過去残骸) を弾く設計。 daw_01 plugin_host が
/// 異常終了で leftover を残すと、 Windows の PID 再利用で次セッションが
/// 同 PID + plugin_id=1 で衝突して plugin load 失敗。
///
/// 起動時に「自分の PID 配下」 の leftover (= `daw_01_pd_<my_pid>_*`) を
/// 削除する。 自分の PID 配下なら過去オーナーは確実に死亡 (= Windows は
/// PID 再利用前に旧 process を完全 reap する)、 安全に削除可能。 他 PID
/// 配下の leftover は触らない (= 他 daw_01 plugin_host が現役の可能性)。
///
/// 失敗は warn ログのみ (= 起動を妨げず、 実際の plugin load で
/// `failed to create shmem` が再発したら user に見える)。
fn cleanup_leftover_shmems(my_pid: u32) {
    let dir = std::env::temp_dir().join("shared_memory-rs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let prefix = format!("daw_01_pd_{my_pid}_");
    let mut removed = 0usize;
    let mut failed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        if !name_str.starts_with(&prefix) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "cleanup_leftover_shmems: failed to remove (= 別プロセスがまだ open している可能性)"
                );
                failed += 1;
            }
        }
    }
    if removed > 0 || failed > 0 {
        tracing::info!(my_pid, removed, failed, "cleanup_leftover_shmems: done");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // probe (--probe-vst3 / --probe-clap) の早期 return より前に guard を束縛し、
    // probe 実行分のログも flush されるようにする。
    let _log_guard = common::logging::init_tracing_for("daw_plugin_host");
    tracing::info!("daw_plugin_host started");

    // one-shot VST3 port-probe モード。 daw_gui の rescan が VST3
    // ごとにこのプロセスを使い捨てで起動し (プロセス隔離 + caller 側 timeout)、
    // bus 構成から port 構成 (note in/out・audio out) を得る。 plugin の instantiate
    // を別プロセスへ押し込むことで、 壊れた / ハングする VST3 がスキャン本体を
    // 巻き込まない。 **成功時のみ** stdout に `note_in=.. note_out=.. audio_out=..`
    // を 1 行出して即 exit (IPC handshake へ進まない)。 失敗 (Err / panic) は無出力
    // → caller は scan-time 暫定値を保持する (退行しない)。
    if std::env::args().nth(1).as_deref() == Some("--probe-vst3") {
        let path = std::env::args()
            .nth(2)
            .context("--probe-vst3 needs <path>")?;
        let target_id = std::env::args().nth(3).unwrap_or_default();
        // VST3 instantiate は plugin-main thread idiom に合わせ専用 thread で。
        let ports = std::thread::spawn(move || {
            vst3_plugin::probe_ports(std::path::Path::new(&path), &target_id)
        })
        .join();
        if let Ok(Ok(cfg)) = ports {
            println!("{}", cfg.to_line());
        }
        return Ok(());
    }

    // one-shot CLAP port-probe モード。 VST3 と対称 (--probe-vst3 と
    // 同じ行形式・同じ失敗時無出力)。 CLAP descriptor の feature には note 出力の
    // 有無が無いので、 dual-role 検出には instance 生成後の note-ports/audio-ports
    // query が要る。
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

    let pipe_name = std::env::args()
        .nth(1)
        .context("expected pipe name as first argument")?;

    let mut pipe = common::client::perform_handshake(&pipe_name, ChildKind::PluginHost).await?;
    tracing::info!("daw_plugin_host handshake complete");

    let session = common::client::read_session(&mut pipe).await?;
    tracing::info!(?session, "audio session received");

    let (evt_tx, evt_rx) = tmpsc::unbounded_channel::<PluginEvent>();
    let plugin_thread = PluginThread::spawn(session, evt_tx)?;

    // Multiplex pipe I/O: read commands in, write events out, on the same
    // socket (no cloning needed).
    pipe_loop(pipe, plugin_thread.sender(), evt_rx).await;

    tracing::info!("daw_plugin_host shutting down");
    plugin_thread.shutdown();
    tracing::info!("daw_plugin_host exiting");
    Ok(())
}

// --- PluginThread wrapper --------------------------------------------------

struct PluginThread {
    join: Option<JoinHandle<()>>,
    cmd_tx: mpsc::Sender<PluginCommand>,
    thread_id: u32,
}

impl PluginThread {
    fn spawn(session: AudioSession, evt_tx: tmpsc::UnboundedSender<PluginEvent>) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PluginCommand>();
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
        let _ = self.cmd_tx.send(PluginCommand::Shutdown);
        wake_thread(self.thread_id);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
struct PluginThreadSender {
    cmd_tx: mpsc::Sender<PluginCommand>,
    thread_id: u32,
}

impl PluginThreadSender {
    fn send(&self, cmd: PluginCommand) {
        if self.cmd_tx.send(cmd).is_err() {
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

// --- Plugin-main thread loop ----------------------------------------------

fn plugin_main_loop(
    session: AudioSession,
    cmd_rx: mpsc::Receiver<PluginCommand>,
    evt_tx: tmpsc::UnboundedSender<PluginEvent>,
) {
    let mut tracks = TracksHandle::new();
    // A2: plugin-process worker pool paired 1:1 with audio-engine
    // workers. Stored as an Option so OpenWorkerPool can replace any
    // stale pool (e.g. on session restart).
    let mut worker_pool: Option<process_server::WorkerPool> = None;

    // A2: plugin instance registry.
    //   - `next_plugin_id` issues a session-unique id every time a
    //     plugin instance is loaded.
    //   - `plugin_shmems` owns the `ProcessData` shmem created here so
    //     daw_audio can `OpenShared` it via `ChildToMain::SlotPluginLoaded`.
    //   - `plugin_lookup` maps `(track, index)` to the live plugin id so
    //     RemoveSlotPlugin / RemoveTrack / SwapTracks can clean up.
    //   - `plugin_registry` is the lock-free `plugin_id` → entry table
    //     read by the worker pool during dispatch.
    let plugin_host_pid = std::process::id();
    // Phase 6 review fix: `shared_memory` crate (Windows) は temp dir に
    // persistent file (`%TEMP%/shared_memory-rs/daw_01_pd_<pid>_<id>`) を作り、
    // `create_new(true)` で重複を弾く。 daw_01 が crash / 異常終了で
    // クリーンアップしそびれると leftover が残り続け、 Windows が PID を
    // 再利用したときに新セッションが「自分の名前空間」 の leftover と衝突して
    // plugin load が失敗する (user 報告: ERROR failed to create shmem
    // daw_01_pd_<pid>_1)。 起動時に自分の PID 配下の leftover を一掃する。
    // 別 PID 配下の leftover は他プロセスが現役の可能性があるので触らない
    // (= 過去 PID 由来でも、 同 PID の新プロセスがあれば共有中)。
    cleanup_leftover_shmems(plugin_host_pid);
    let mut next_plugin_id: u32 = 1;
    let mut plugin_shmems: HashMap<u32, common::process_data::ProcessDataHandle> = HashMap::new();
    let mut plugin_lookup: HashMap<(u32, u32), u32> = HashMap::new();
    // Defensive dedup: if the GUI somehow sends `SetSlotPlugin` twice
    // for the same (track, index, plugin_id) (we've seen the picker
    // double-fire) we skip the second to avoid the workers racing on
    // a destroy → re-install path. Keyed by (track, index) → loaded
    // plugin's stable id string.
    let mut loaded_id_for_slot: HashMap<(u32, u32), String> = HashMap::new();
    // PR4.5 fix: cache the display name + shmem id alongside loaded_id so
    // we can re-emit a `SlotPluginLoaded` event when SetSlotPlugin arrives
    // for an already-loaded device (= 2nd LoadSong of the same project).
    // Without re-emitting, daw_gui's `pending_plugin_loads` never clears
    // and queued Play (`pending_play`) can never fire — playback freezes.
    // (id, name, aux_output_count) cached per slot so the dedup branch can
    // re-emit SlotPluginLoaded with the same metadata (パラアウト: incl. the
    // aux output count) on a 2nd LoadSong of the same project.
    let mut loaded_meta_for_slot: HashMap<(u32, u32), (String, String, u8)> =
        HashMap::new();
    let plugin_registry: PluginRegistry =
        Arc::new(arc_swap::ArcSwap::from_pointee(Vec::new()));

    // plugin editor windows are now owned by THIS process. Each
    // open editor has a host-created top-level window (`EditorWindow`) keyed
    // by (track, index). Created/destroyed only on this (plugin-main) thread.
    let mut editor_windows: HashMap<(u32, u32), editor_window::EditorWindow> =
        HashMap::new();
    // Plugin-initiated GUI resize requests (CLAP `request_resize` / VST3
    // `IPlugFrame::resizeView`). The host callback runs on this thread and
    // pushes here; the loop drains it to resize the owning `EditorWindow` and
    // call the plugin's `onSize`/`set_size`. Using a tokio unbounded sender
    // keeps the `Send + Sync` bound the `HostCallbacks` closures require
    // (std `mpsc::Sender` is `!Sync`).
    let (gui_resize_tx, mut gui_resize_rx) =
        tmpsc::unbounded_channel::<(u32, u32, u32, u32)>();
    // Plugin-initiated GUI close (CLAP `clap_host_gui.closed` / lost gui
    // connection). The host callback queues (track, index); the loop drains it
    // and runs the full teardown (gui_destroy + DestroyWindow + notify). For
    // embedded GUIs this is rare (we own the window), but routing it through
    // the loop keeps the editor-window map and daw_gui's open set consistent.
    let (gui_close_tx, mut gui_close_rx) = tmpsc::unbounded_channel::<(u32, u32)>();

    // Per-(track, index) host callbacks: each loaded plugin captures its
    // (track, index) so the async CLAP callback (request_resize / closed)
    // can stamp the event with the correct address before reaching daw_gui.
    let make_callbacks = |track: u32, index: u32| HostCallbacks {
        on_request_resize: {
            let tx = gui_resize_tx.clone();
            Arc::new(move |w, h| {
                let _ = tx.send((track, index, w, h));
            })
        },
        on_closed: {
            let tx = gui_close_tx.clone();
            Arc::new(move || {
                let _ = tx.send((track, index));
            })
        },
        // VST3 param gesture (IComponentHandler::beginEdit/performEdit/endEdit)。
        // resize / closed と同 idiom で evt_tx に流す。 plugin_id は
        // PluginEvent → ChildToMain 変換で破棄される (= daw_gui は (track, index,
        // param_id) で解決する) ので 0 placeholder。 CLAP plugin はこの callback
        // を呼ばない (out_events 経由) ので二重発火しない。
        on_param_gesture_begin: {
            let tx = evt_tx.clone();
            Arc::new(move |param_id| {
                let _ = tx.send(PluginEvent::PluginParamTouched {
                    track,
                    index,
                    plugin_id: 0,
                    param_id,
                });
            })
        },
        on_param_value: {
            let tx = evt_tx.clone();
            Arc::new(move |param_id, value| {
                let _ = tx.send(PluginEvent::PluginParamValueChanged {
                    track,
                    index,
                    plugin_id: 0,
                    param_id,
                    value,
                });
            })
        },
        on_param_gesture_end: {
            let tx = evt_tx.clone();
            Arc::new(move |param_id| {
                let _ = tx.send(PluginEvent::PluginParamGestureEnd {
                    track,
                    index,
                    plugin_id: 0,
                    param_id,
                });
            })
        },
    };

    tracing::info!("plugin-main thread running");

    loop {
        loop {
            let cmd = match cmd_rx.try_recv() {
                Ok(c) => c,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // worker pool を先に shutdown する。 worker thread が
                    // 動いたまま `tracks.shutdown()` で plugin Drop を
                    // 走らせると UAF。
                    if let Some(pool) = worker_pool.take() {
                        pool.shutdown();
                    }
                    tracks.shutdown();
                    return;
                }
            };
            match cmd {
                PluginCommand::Shutdown => {
                    if let Some(pool) = worker_pool.take() {
                        pool.shutdown();
                    }
                    tracks.shutdown();
                    tracing::info!("plugin-main thread exiting");
                    return;
                }
                PluginCommand::OpenWorkerPool {
                    n_workers,
                    worker_bridge_shmem_id,
                    wake_event_names,
                    done_event_names,
                } => {
                    if let Some(pool) = worker_pool.take() {
                        pool.shutdown();
                    }
                    match process_server::WorkerPool::open(
                        n_workers,
                        &worker_bridge_shmem_id,
                        &session.metrics_shmem_id,
                        &wake_event_names,
                        &done_event_names,
                        Arc::clone(&plugin_registry),
                        evt_tx.clone(),
                    ) {
                        Ok(pool) => worker_pool = Some(pool),
                        Err(e) => {
                            tracing::error!(error = ?e, "failed to open plugin worker pool");
                        }
                    }
                }
                PluginCommand::CloseWorkerPool => {
                    if let Some(pool) = worker_pool.take() {
                        pool.shutdown();
                    }
                }
                PluginCommand::SetRenderMode(mode) => {
                    // Forward the CLAP render hint to every loaded
                    // plugin in every chain. Failures (plugin missing
                    // the extension, mode rejected) are best-effort and
                    // don't surface to the audio side.
                    for chain in tracks.tracks.chains.values_mut() {
                        for plugin in chain.devices.iter_mut() {
                            let _ = plugin.set_render_mode(mode);
                        }
                    }
                    tracing::info!(?mode, "render mode broadcast to all plugins");
                }
                PluginCommand::ReinitAllPlugins => {
                    // Force every plugin back to a clean, silent state with a
                    // two-pronged reset (one prong alone is insufficient):
                    //   - deactivate→activate clears stubborn held voices that
                    //     survive CLAP `reset()` (VCV Rack 2 keeps a live voice
                    //     ringing through reset / start-stop_processing);
                    //   - `reset()` (CLAP `clap_plugin.reset`) clears the audio
                    //     processing state — filters / delay lines / reverb
                    //     tails — that a deactivate→activate alone leaves intact
                    //     for a CLAP reverb's internal feedback-delay network
                    //     (実機: パニック後もテイルが鳴り続けた).
                    // Shared by export prep (clean cold render) and
                    // the panic button (kill all sound now). Same
                    // safety contract
                    // as plugin teardown: detach from the registry so workers
                    // skip these instances, `quiesce` to drain in-flight
                    // dispatch, mutate on this (plugin-main) thread, then
                    // republish — safe even while the live callback is running.
                    let sr = session.sample_rate;
                    let mf = session.max_frames;
                    // (1) snapshot registry entries (Boxes never move during an
                    //     in-place reinit, so the raw pointers stay valid) and
                    //     detach every plugin.
                    type SavedEntry = (
                        u32,
                        *mut (dyn LoadedPlugin + 'static),
                        *mut common::process_data::ProcessData,
                        u32,
                        u32,
                    );
                    let saved: Vec<SavedEntry> = plugin_registry
                        .load()
                        .iter()
                        .enumerate()
                        .filter_map(|(pid, opt)| {
                            opt.as_ref()
                                .map(|e| (pid as u32, e.plugin.0, e.process_data, e.track, e.index))
                        })
                        .collect();
                    for (pid, ..) in &saved {
                        publish_plugin_registry(&plugin_registry, *pid, None);
                    }
                    // (2) drain in-flight dispatch so no worker derefs a plugin
                    //     while we deactivate it.
                    if let Some(pool) = worker_pool.as_ref() {
                        pool.quiesce();
                    }
                    // (3) full CLAP/VST3 lifecycle reset in place.
                    let mut n = 0u32;
                    for chain in tracks.tracks.chains.values_mut() {
                        for plugin in chain.devices.iter_mut() {
                            plugin.stop_processing();
                            plugin.deactivate();
                            if let Err(e) = plugin.activate(f64::from(sr), 64, mf) {
                                tracing::error!(error = ?e, "reinit: activate failed");
                            }
                            if let Err(e) = plugin.start_processing() {
                                tracing::error!(error = ?e, "reinit: start_processing failed");
                            }
                            // Clear DSP tails (reverb / delay / filters) now that
                            // the instance is active + processing. CLAP forwards
                            // to `clap_plugin.reset()`; VST3 / builtin already
                            // flushed via the deactivate / stop_processing above.
                            plugin.reset();
                            n += 1;
                        }
                    }
                    // (4) republish the saved entries (pointers unchanged).
                    for (pid, ptr, pd, track, index) in saved {
                        publish_plugin_registry(
                            &plugin_registry,
                            pid,
                            Some(PluginEntry {
                                plugin: PluginPtr(ptr),
                                process_data: pd,
                                track,
                                index,
                            }),
                        );
                    }
                    tracing::info!(plugins = n, "reinitialised all plugins to clean state (export prep / panic)");
                    let _ = evt_tx.send(PluginEvent::PluginsReinitDone);
                }
                PluginCommand::SetSlotPlugin {
                    track,
                    index,
                    format,
                    path,
                    plugin_id,
                    initial_state,
                } => {
                    // Defensive dedup against picker double-fire. Same
                    // plugin id at the same index ⇒ ignore re-load, but
                    // we must STILL emit `SlotPluginLoaded` so daw_gui
                    // can clear its `pending_plugin_loads` entry for this
                    // device — otherwise a second project-load (same plugins)
                    // leaves the entry pending forever and `play()`
                    // refuses to start (`pending_play=true` in
                    // `app.rs::play()`).
                    if loaded_id_for_slot.get(&(track, index)) == Some(&plugin_id) {
                        tracing::info!(
                            track,
                            index,
                            id = %plugin_id,
                            "SetSlotPlugin: same plugin already loaded, re-emitting SlotPluginLoaded"
                        );
                        if let (Some(&new_plugin_id), Some((cached_id, cached_name, cached_aux_out))) = (
                            plugin_lookup.get(&(track, index)),
                            loaded_meta_for_slot.get(&(track, index)),
                        ) {
                            let shmem_id = format!(
                                "daw_01_pd_{plugin_host_pid}_{new_plugin_id}"
                            );
                            let _ = evt_tx.send(PluginEvent::SlotPluginLoaded {
                                track,
                                index,
                                id: cached_id.clone(),
                                name: cached_name.clone(),
                                plugin_id: new_plugin_id,
                                shmem_id,
                                aux_output_count: *cached_aux_out,
                                // 同 plugin の re-emit path。 state_load
                                // を呼んでいないので error は常に None。
                                state_load_error: None,
                            });
                        } else {
                            tracing::warn!(
                                track, index,
                                "duplicate SetSlotPlugin: meta cache miss, daw_gui pending may stick"
                            );
                        }
                        continue;
                    }
                    // Note: ユーザーの Play/Stop 状態は chain 編集を
                    // またいで維持する (playback_state には触らない)。
                    //
                    // 順序: load_plugin → 旧 plugin の detach → registry
                    // None publish → quiesce → 旧 plugin teardown → 新
                    // plugin install。 load_plugin が失敗した場合は
                    // 旧 plugin をそのまま残す (UX 上自然な挙動)。

                    // (1) 新 plugin の instantiate。 ここまでで失敗 ⇒
                    //     旧 plugin の状態は触らずに早期 return。
                    let callbacks = make_callbacks(track, index);
                    let mut plugin = match load_plugin(format, &path, &plugin_id, callbacks) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::error!(error = ?e, ?format, path = %path.display(), "load failed");
                            // pending stuck 防止: daw_gui の
                            // `pending_plugin_loads` を解放するため失敗
                            // 通知を送る。 旧 plugin は touch していない
                            // ので chain はそのまま (= 旧 plugin が居れば
                            // 継続再生)。
                            let _ = evt_tx.send(PluginEvent::PluginLoadFailed {
                                track,
                                index,
                                plugin_id: plugin_id.clone(),
                                reason: format!("{e}"),
                            });
                            continue;
                        }
                    };
                    // Phase 6 review (silent corruption fix): state_load
                    // 失敗を `tracing::error!` だけで握りつぶしていたので、
                    // ユーザーは saved project を開いて plugin が default
                    // 状態になっていることに気付けなかった。 失敗理由を
                    // `state_load_error` に格納し SlotPluginLoaded に同梱
                    // して daw_gui の status_message へ伝える。 plugin
                    // 自体は default 状態で chain に挿さる (= 旧挙動と
                    // 互換、 partial recovery)。
                    let state_load_error: Option<String> = if let Some(bytes) =
                        initial_state
                    {
                        match plugin.state_load(&bytes) {
                            Ok(()) => None,
                            Err(e) => {
                                let reason = format!("{e:#}");
                                tracing::error!(
                                    track,
                                    index,
                                    plugin = %plugin_id,
                                    error = %reason,
                                    "state_load failed (= plugin は default 状態で進む、 \
                                     daw_gui に SlotPluginLoaded.state_load_error で通知)",
                                );
                                Some(reason)
                            }
                        }
                    } else {
                        None
                    };

                    // (2) 旧 plugin を chain から detach (DLL call なし)。
                    // REPLACE semantics: `index < devices.len()` のとき同 index
                    // の既存 device を取り出す (`install_plugin` が同 index に
                    // 差し替える)。 `index == devices.len()` は append なので
                    // detach 対象なし。
                    let old_pid = plugin_lookup.get(&(track, index)).copied();
                    let mut detached_old: Option<Box<dyn LoadedPlugin>> = None;
                    tracks.mutate(|t| {
                        if let Some(chain) = t.chains.get_mut(&track) {
                            detached_old = detach_plugin(chain, index);
                        }
                    });

                    // (3) 旧 plugin_id の registry を None で publish。
                    if let Some(pid) = old_pid {
                        publish_plugin_registry(&plugin_registry, pid, None);
                    }

                    // (4) worker pool に in-flight な process() を排出させる。
                    //     旧 plugin を deref している worker が居る間は block。
                    if let Some(pool) = worker_pool.as_ref() {
                        pool.quiesce();
                    }

                    // (5) 旧 plugin を teardown + drop。
                    if let Some(old) = detached_old {
                        teardown_plugin(old);
                    }
                    if let Some(pid) = old_pid {
                        // 旧 plugin の editor window が開いていたら、
                        // teardown (gui_destroy) の後に container を破棄する。
                        // stable plugin id で照合 (slot ずれ耐性) + SlotGuiClosed 通知。
                        destroy_editor_windows_where(
                            &mut editor_windows,
                            &evt_tx,
                            |_, w| w.plugin_id() == pid,
                        );
                        plugin_lookup.remove(&(track, index));
                        plugin_shmems.remove(&pid);
                    }

                    // (6) 新 plugin を chain に install (activate +
                    //     start_processing 含む)。
                    let loaded_id = plugin.id().to_string();
                    let loaded_name = plugin.name().to_string();
                    // パラアウト (docs/plan_paraout.md): capture the aux output
                    // port count now, before `plugin` is moved into the chain,
                    // so SlotPluginLoaded can tell the GUI how many child tracks
                    // to make on "explode".
                    let loaded_aux_out_count = plugin.aux_output_port_count().min(u8::MAX as usize) as u8;
                    let sr = session.sample_rate;
                    let mf = session.max_frames;
                    tracks.mutate(|t| {
                        install_plugin(t.ensure_track(track), index, plugin, sr, mf)
                    });

                    // (7) 新 plugin に plugin_id を割り当て、 ProcessData
                    //     shmem を作って daw_gui に通知する。
                    let new_plugin_id = next_plugin_id;
                    next_plugin_id += 1;
                    let shmem_id =
                        format!("daw_01_pd_{plugin_host_pid}_{new_plugin_id}");
                    match common::process_data::ProcessDataHandle::create(&shmem_id) {
                        Ok(handle) => {
                            let pd_ptr = handle.ptr();
                            plugin_shmems.insert(new_plugin_id, handle);
                            plugin_lookup.insert((track, index), new_plugin_id);
                            loaded_id_for_slot.insert((track, index), plugin_id.clone());
                            // PR4.5: 同 plugin_id の SetSlotPlugin が再度
                            // 来たとき (= 同プロジェクトの 2 度目 LoadSong)
                            // に dedup branch から SlotPluginLoaded を再
                            // emit するためのキャッシュ。
                            loaded_meta_for_slot.insert(
                                (track, index),
                                (loaded_id.clone(), loaded_name.clone(), loaded_aux_out_count),
                            );

                            // Install 直後の plugin pointer を short-lived な
                            // borrow scope で取得。 borrow が抜けたあとで
                            // tracks を再利用する。
                            let plugin_ptr_raw: Option<*mut (dyn LoadedPlugin + 'static)> = {
                                let opt = tracks.tracks.plugin_at_mut(track, index);
                                opt.map(|p| {
                                    // SAFETY: この trait object を所有する
                                    // `Box` は `tracks.chains` に住み、 drop
                                    // するパスは
                                    //   detach_plugin → registry-None →
                                    //   WorkerPool::quiesce → teardown_plugin
                                    // のみ。 worker pool の dispatch は
                                    // `DispatchCounter::enter` / `exit` で
                                    // この raw pointer の使用期間を
                                    // synchronize する (process_server.rs
                                    // の module-level docs 参照)。
                                    let r: &mut dyn LoadedPlugin = p;
                                    let raw: *mut dyn LoadedPlugin = r;
                                    unsafe {
                                        std::mem::transmute::<
                                            *mut dyn LoadedPlugin,
                                            *mut (dyn LoadedPlugin + 'static),
                                        >(raw)
                                    }
                                })
                            };
                            if let Some(p) = plugin_ptr_raw {
                                publish_plugin_registry(
                                    &plugin_registry,
                                    new_plugin_id,
                                    Some(PluginEntry {
                                        plugin: PluginPtr(p),
                                        process_data: pd_ptr,
                                        track,
                                        index,
                                    }),
                                );
                            }
                            let _ = evt_tx.send(PluginEvent::SlotPluginLoaded {
                                track,
                                index,
                                id: loaded_id,
                                name: loaded_name,
                                plugin_id: new_plugin_id,
                                shmem_id,
                                state_load_error,
                                aux_output_count: loaded_aux_out_count,
                            });
                            // PR3.3: activate 直後 (CLAP `[main-thread &
                            // active]` / VST3 `[UI-thread & Setup Done]`
                            // を満たすここ) で plugin の latency を query
                            // して daw_gui へ送る。 samples == 0 でも送る
                            // ことで、 同 index の plugin 入れ替え時に
                            // 古い値を上書きできる。
                            if let Some(p) = plugin_ptr_raw {
                                let samples = unsafe { (*p).query_latency() };
                                tracing::info!(
                                    plugin_id = new_plugin_id,
                                    samples,
                                    "plugin reported latency"
                                );
                                let _ = evt_tx.send(PluginEvent::PluginLatencyChanged {
                                    plugin_id: new_plugin_id,
                                    samples,
                                });
                                // Phase 2 (`docs/plan_automation.md` §7.5):
                                // activate 完了直後 (CLAP `[main-thread &
                                // active]` / VST3 `[UI-thread & Setup Done]`)
                                // で param 一覧を query して daw_gui へ送る。
                                // daw_gui は `AppData.plugin_params` に
                                // キャッシュして Parameter Picker / lane の
                                // label 解決に使う。
                                let params = unsafe { (*p).enumerate_params() };
                                if !params.is_empty() {
                                    tracing::info!(
                                        plugin_id = new_plugin_id,
                                        count = params.len(),
                                        "plugin enumerated params"
                                    );
                                }
                                // 埋め込み GUI の有無を一緒に通知。
                                // daw_gui がチェーン行ボタン (GUI window vs
                                // インライン param パネル) を分岐する判断材料。
                                let has_embedded_gui =
                                    unsafe { (*p).gui_is_embed_supported() };
                                let _ = evt_tx.send(PluginEvent::PluginParamList {
                                    track,
                                    index,
                                    plugin_id: new_plugin_id,
                                    params,
                                    has_embedded_gui,
                                });
                                // builtin VOICEVOX なら合成状態 reporter を仕込む。
                                // synth thread が busy/failing 遷移ごとに呼び、daw_gui へ継続
                                // 報告する (= クリップ上スピナー + 全体オーバーレイ + engine
                                // 未接続警告)。voicevox_synth_progress().is_some() = builtin
                                // VOICEVOX 判定 (それ以外は default no-op なので Box を作らない)。
                                if unsafe { (*p).voicevox_synth_progress().is_some() } {
                                    let evt = evt_tx.clone();
                                    let pid = new_plugin_id;
                                    unsafe {
                                        (*p).set_voicevox_status_reporter(Box::new(
                                            move |busy, failing| {
                                                let _ = evt.send(
                                                    PluginEvent::VoicevoxSynthStatus {
                                                        plugin_id: pid,
                                                        busy,
                                                        failing,
                                                    },
                                                );
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                error = ?e,
                                new_plugin_id,
                                "failed to create ProcessData shmem"
                            );
                            // orphan cleanup: 新 plugin は既に
                            // `install_plugin` で chain に live。 SetSlotPlugin
                            // の旧 plugin teardown と同じ「detach → quiesce
                            // → teardown」 dance で安全に外す。 registry
                            // 側は new_plugin_id が未 publish なので publish
                            // None は不要。 旧 plugin は既に teardown 済
                            // なので device 列は空のまま。
                            let mut detached: Option<Box<dyn LoadedPlugin>> = None;
                            tracks.mutate(|t| {
                                if let Some(chain) = t.chains.get_mut(&track) {
                                    detached = detach_plugin(chain, index);
                                }
                            });
                            if let Some(pool) = worker_pool.as_ref() {
                                pool.quiesce();
                            }
                            if let Some(p) = detached {
                                teardown_plugin(p);
                            }
                            let _ = evt_tx.send(PluginEvent::PluginLoadFailed {
                                track,
                                index,
                                plugin_id: plugin_id.clone(),
                                reason: format!("shmem create failed: {e}"),
                            });
                        }
                    }
                }
                PluginCommand::RemoveSlotPlugin { track, index } => {
                    // 削除前の device 数を控える (= shift 範囲 index+1.. を
                    // 後で 1 つ手前へ詰めるため)。
                    let len_before = tracks
                        .tracks
                        .chains
                        .get(&track)
                        .map(|c| c.devices.len())
                        .unwrap_or(0);

                    // (1) plugin を chain から取り出すだけ (DLL call なし)。
                    // `Vec::remove(index)` は後続 device を 1 つ手前へ詰める
                    // (= daw_gui / daw_audio の挙動と一致)。
                    let mut detached: Option<Box<dyn LoadedPlugin>> = None;
                    tracks.mutate(|t| {
                        if let Some(chain) = t.chains.get_mut(&track) {
                            detached = detach_plugin(chain, index);
                        }
                    });
                    let removed_pid = plugin_lookup.get(&(track, index)).copied();

                    // (2) registry から `None` を publish して、 以降の
                    // worker dispatch が この plugin_id を見つけられないよう
                    // にする。
                    if let Some(pid) = removed_pid {
                        publish_plugin_registry(&plugin_registry, pid, None);
                    }

                    // (3) worker pool に in-flight な process() があれば
                    // 排出する。 pool が None (= OpenWorkerPool 前 / 後) なら
                    // worker そのものが居ないので no-op。
                    if let Some(pool) = worker_pool.as_ref() {
                        pool.quiesce();
                    }

                    // (4) ここまで来れば worker は plugin を deref
                    // していないので、 plugin-main thread で安全に teardown
                    // + drop できる。
                    if let Some(plugin) = detached {
                        teardown_plugin(plugin);
                    }

                    if let Some(pid) = removed_pid {
                        // destroy this plugin's editor window (if
                        // open) after gui_destroy. Match by STABLE plugin id,
                        // not by (track, index): the plugin's index may have
                        // shifted while the editor was open (a lower-index
                        // removal), so an index-keyed remove would orphan the
                        // window. The helper also emits SlotGuiClosed so
                        // daw_gui clears its open-GUI set.
                        destroy_editor_windows_where(
                            &mut editor_windows,
                            &evt_tx,
                            |_, w| w.plugin_id() == pid,
                        );
                        plugin_lookup.remove(&(track, index));
                        plugin_shmems.remove(&pid);
                        loaded_id_for_slot.remove(&(track, index));
                        loaded_meta_for_slot.remove(&(track, index));

                        // `Vec::remove(index)` shifted every device at old
                        // idx > index down to idx-1. Re-key the `(track, idx)`
                        // books for that range so they stay aligned with the
                        // device列 (snapshot-remove-all-then-reinsert to avoid
                        // a re-keyed entry colliding with a not-yet-freed old
                        // one). `republish_entry_slot` corrects each moved
                        // plugin's `PluginEntry.index` so the worker pool
                        // stamps param events at the right device.
                        type IdxBookkeeping = (
                            u32,
                            u32,
                            Option<String>,
                            Option<(String, String, u8)>,
                            Option<editor_window::EditorWindow>,
                        );
                        let mut remapped: Vec<IdxBookkeeping> = Vec::new();
                        for old in (index + 1)..(len_before as u32) {
                            if let Some(&pid) = plugin_lookup.get(&(track, old)) {
                                let lid = loaded_id_for_slot.remove(&(track, old));
                                let meta = loaded_meta_for_slot.remove(&(track, old));
                                let win = editor_windows.remove(&(track, old));
                                plugin_lookup.remove(&(track, old));
                                remapped.push((old - 1, pid, lid, meta, win));
                            }
                        }
                        for (new, pid, lid, meta, win) in remapped {
                            plugin_lookup.insert((track, new), pid);
                            if let Some(lid) = lid {
                                loaded_id_for_slot.insert((track, new), lid);
                            }
                            if let Some(meta) = meta {
                                loaded_meta_for_slot.insert((track, new), meta);
                            }
                            if let Some(win) = win {
                                editor_windows.insert((track, new), win);
                            }
                            republish_entry_slot(&plugin_registry, pid, track, new);
                        }

                        // PR2.1: daw_gui に `ClosePluginShmem` を audio engine
                        // に転送させて、 audio thread が destroyed plugin に
                        // process() を呼び続けないようにする。
                        let _ = evt_tx.send(PluginEvent::SlotPluginUnloaded {
                            plugin_id: pid,
                        });
                    }
                }
                PluginCommand::ReorderChain { track, moves } => {
                    // Single-chain redesign: apply a complete chain permutation
                    // as a LIVE move over the single `devices: Vec<Box<dyn
                    // LoadedPlugin>>`. Every plugin's `Box` keeps its heap
                    // address across the shuffle (the worker pool's `PluginPtr`
                    // stays valid → no re-instantiation, no audio glitch), and
                    // the open editor windows follow by re-keying
                    // `editor_windows`. `moves` is the COMPLETE permutation: one
                    // `(old_index, new_index)` per device (possibly old == new).
                    //
                    // The audio engine is re-keyed directly by daw_gui (the
                    // same `ReorderChain` is sent to it), so we do NOT re-emit
                    // `SlotPluginLoaded` here.

                    // --- 1. Validate `moves` is a permutation of 0..n so a
                    //        malformed list can never drop a live Box. ---
                    let n = tracks
                        .tracks
                        .chains
                        .get(&track)
                        .map(|c| c.devices.len())
                        .unwrap_or(0);
                    let froms: Vec<u32> = moves.iter().map(|&(f, _)| f).collect();
                    let tos: Vec<u32> = moves.iter().map(|&(_, t)| t).collect();
                    // Both `old` and `new` columns must be exactly 0..n (no
                    // dup, contiguous targets, full cover).
                    let is_perm = |v: &[u32]| {
                        if v.len() != n {
                            return false;
                        }
                        let mut seen = vec![false; n];
                        for &x in v {
                            let i = x as usize;
                            if i >= n || seen[i] {
                                return false;
                            }
                            seen[i] = true;
                        }
                        true
                    };
                    let valid = is_perm(&froms) && is_perm(&tos);
                    if !valid {
                        tracing::warn!(
                            track,
                            n_moves = moves.len(),
                            n,
                            "ReorderChain skipped: moves are not a complete chain permutation"
                        );
                    } else {
                        // --- 2. Permute the live Boxes in place. Pull all
                        //        devices into a temp map keyed by OLD index,
                        //        then rebuild a Vec of length n with each at
                        //        its NEW index. ---
                        tracks.mutate(|t| {
                            if let Some(chain) = t.chains.get_mut(&track) {
                                let mut pool: HashMap<u32, Box<dyn LoadedPlugin>> =
                                    HashMap::new();
                                for (i, p) in chain.devices.drain(..).enumerate() {
                                    pool.insert(i as u32, p);
                                }
                                let mut rebuilt: Vec<Option<Box<dyn LoadedPlugin>>> =
                                    (0..n).map(|_| None).collect();
                                for &(from, to) in &moves {
                                    if let Some(p) = pool.remove(&from) {
                                        rebuilt[to as usize] = Some(p);
                                    }
                                }
                                // `valid` guarantees every target was filled,
                                // so `flatten` cannot silently drop a plugin.
                                chain.devices = rebuilt.into_iter().flatten().collect();
                            }
                        });
                        // --- 3. Re-key every (track, index) book old→new.
                        //        Remove ALL old keys first, then re-insert, so a
                        //        new index never collides with a not-yet-freed
                        //        old one. ---
                        type IdxBookkeeping = (
                            u32,
                            u32,
                            Option<String>,
                            Option<(String, String, u8)>,
                            Option<editor_window::EditorWindow>,
                        );
                        let mut remapped: Vec<IdxBookkeeping> = Vec::new();
                        for &(from, to) in &moves {
                            if let Some(&pid) = plugin_lookup.get(&(track, from)) {
                                let lid = loaded_id_for_slot.remove(&(track, from));
                                let meta = loaded_meta_for_slot.remove(&(track, from));
                                let win = editor_windows.remove(&(track, from));
                                plugin_lookup.remove(&(track, from));
                                remapped.push((to, pid, lid, meta, win));
                            }
                        }
                        for (to, pid, lid, meta, win) in remapped {
                            plugin_lookup.insert((track, to), pid);
                            if let Some(lid) = lid {
                                loaded_id_for_slot.insert((track, to), lid);
                            }
                            if let Some(meta) = meta {
                                loaded_meta_for_slot.insert((track, to), meta);
                            }
                            if let Some(win) = win {
                                editor_windows.insert((track, to), win);
                            }
                            republish_entry_slot(&plugin_registry, pid, track, to);
                        }
                        tracing::info!(track, n = moves.len(), "ReorderChain applied (live move)");
                    }
                }
                PluginCommand::RemoveTrack { track } => {
                    // `track` は stable な `Track::id`。 この track に
                    // 属する plugin_id を集めておき、 drop の **後** に
                    // daw_gui へ通知する。
                    let removed_pids: Vec<u32> = plugin_lookup
                        .iter()
                        .filter_map(|(&(t, _), &pid)| if t == track { Some(pid) } else { None })
                        .collect();

                    // (1) chain ごと取り出すだけ (DLL call なし)。
                    let mut detached_chain: Option<Chain> = None;
                    tracks.mutate(|t| {
                        detached_chain = t.chains.remove(&track);
                    });

                    // (2) registry から、 この track 由来の plugin_id を
                    // 全て None で publish。 以降の worker dispatch は
                    // skip path に流れる。
                    for &pid in &removed_pids {
                        publish_plugin_registry(&plugin_registry, pid, None);
                    }

                    // (3) in-flight な process() を排出する。
                    if let Some(pool) = worker_pool.as_ref() {
                        pool.quiesce();
                    }

                    // (4) plugin-main thread で teardown + drop。 worker は
                    // もうこれらの plugin を deref していない。
                    //
                    // **teardown 順序を守ること**: stop_processing →
                    // deactivate → gui_destroy → drop。 これを skip すると
                    // 一部 VST3 plugin (kHs Chorus 等) が internal worker
                    // thread が active なまま COM object が消えて Drop で
                    // crash する。
                    if let Some(mut chain) = detached_chain {
                        for device in chain.devices.drain(..) {
                            teardown_plugin(device);
                        }
                    }

                    plugin_lookup.retain(|&(t, _), _| t != track);
                    loaded_id_for_slot.retain(|&(t, _), _| t != track);
                    loaded_meta_for_slot.retain(|&(t, _), _| t != track);
                    // destroy this track's editor windows after the
                    // plugins' gui_destroy above, and notify daw_gui for each.
                    destroy_editor_windows_where(
                        &mut editor_windows,
                        &evt_tx,
                        |&(t, _), _| t == track,
                    );
                    for pid in removed_pids {
                        plugin_shmems.remove(&pid);
                        // registry は (2) で既に None 化済み。
                        let _ = evt_tx.send(PluginEvent::SlotPluginUnloaded { plugin_id: pid });
                    }
                }
                PluginCommand::RequestSlotState { track, index } => {
                    let data = match tracks.plugin_at_mut(track, index) {
                        Some(plugin) => match plugin.state_save() {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!(error = ?e, "state_save failed");
                                None
                            }
                        },
                        None => None,
                    };
                    let _ = evt_tx.send(PluginEvent::SlotPluginState { track, index, data });
                }
                PluginCommand::SetupAraDocument { track, index, clips } => {
                    match tracks.plugin_at_mut(track, index) {
                        Some(plugin) => match plugin.setup_ara(&clips) {
                            Ok(true) => {
                                tracing::info!(track, index, n = clips.len(), "ARA document set up");
                            }
                            Ok(false) => {
                                tracing::warn!(
                                    track,
                                    index,
                                    "SetupAraDocument: plugin is not ARA-capable, ignoring"
                                );
                            }
                            Err(e) => {
                                tracing::error!(error = ?e, track, index, "ARA setup failed");
                            }
                        },
                        None => {
                            tracing::warn!(track, index, "SetupAraDocument: no plugin at slot");
                        }
                    }
                }
                PluginCommand::ClearAraDocument { track, index } => {
                    if let Some(plugin) = tracks.plugin_at_mut(track, index) {
                        plugin.clear_ara();
                        tracing::info!(track, index, "ARA document cleared");
                    }
                }
                PluginCommand::RequestAllStates => {
                    let entries = collect_all_states(&mut tracks);
                    let _ = evt_tx.send(PluginEvent::AllPluginStates { entries });
                }
                PluginCommand::OpenSlotGui { track, index, title } => {
                    // Stable id so plugin removal can match this editor even
                    // after the plugin's index shifts (a lower-index removal /
                    // a reorder) while the editor is open.
                    let plugin_id = plugin_lookup.get(&(track, index)).copied().unwrap_or(0);
                    match open_gui(
                        &mut tracks,
                        &mut editor_windows,
                        track,
                        index,
                        plugin_id,
                        &title,
                    ) {
                        Ok(Some((w, h))) => {
                            let _ = evt_tx.send(PluginEvent::SlotGuiOpened {
                                track,
                                index,
                                width: w,
                                height: h,
                            });
                        }
                        Ok(None) => {
                            let _ = evt_tx.send(PluginEvent::SlotGuiClosed { track, index });
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, track, index, "failed to open GUI");
                            // open_gui cleaned up its own (plugin + window) on
                            // failure; close_slot_gui is idempotent and also
                            // emits SlotGuiClosed for daw_gui's open-state set.
                            close_slot_gui(&mut tracks, &mut editor_windows, track, index, &evt_tx);
                        }
                    }
                }
                PluginCommand::CloseSlotGui { track, index } => {
                    close_slot_gui(&mut tracks, &mut editor_windows, track, index, &evt_tx);
                }
                PluginCommand::SetBuiltinPluginNoteMetadata {
                    plugin_id,
                    bpm,
                    entries,
                    talk,
                } => {
                    // plugin_lookup は `(track, index) -> plugin_id` の
                    // forward map。 逆引きは O(n) walk。 同 frame に
                    // 数件しか呼ばれない ので n ≤ 数十で問題なし。
                    let target =
                        plugin_lookup.iter().find_map(|(k, v)| {
                            (*v == plugin_id).then_some(*k)
                        });
                    let Some((track, index)) = target else {
                        tracing::warn!(
                            plugin_id,
                            "SetBuiltinPluginNoteMetadata: plugin_id not found"
                        );
                        continue;
                    };
                    tracks.mutate(|t| {
                        if let Some(plugin) = t.plugin_at_mut(track, index) {
                            plugin.set_note_metadata(bpm, &entries, &talk);
                        }
                    });
                }
                PluginCommand::PrepareVocalSynth { plugin_id } => {
                    // 歌唱 bounce の前に合成完了を保証する。 plugin_id の builtin
                    // VOICEVOX の (queued, done) 世代 Arc を取り出し、 直前 flush 世代まで
                    // done になるのを別 thread で poll して VocalSynthReady を emit する
                    // (非同期 HTTP 合成が offline render より遅れて無音になるのを防ぐ)。
                    // 該当 builtin が無い (= 歌唱でない / 未 load) なら即 ready。
                    let target = plugin_lookup
                        .iter()
                        .find_map(|(k, v)| (*v == plugin_id).then_some(*k));
                    let mut progress = None;
                    if let Some((track, index)) = target {
                        tracks.mutate(|t| {
                            if let Some(plugin) = t.plugin_at_mut(track, index) {
                                progress = plugin.voicevox_synth_progress();
                            }
                        });
                    }
                    if let Some((queued, done)) = progress {
                        use std::sync::atomic::Ordering;
                        let target_gen = queued.load(Ordering::SeqCst);
                        let evt_thread = evt_tx.clone();
                        let spawn = std::thread::Builder::new()
                            .name("voicevox-bounce-synth-wait".into())
                            .spawn(move || {
                                let deadline = std::time::Instant::now()
                                    + std::time::Duration::from_secs(30);
                                while done.load(Ordering::SeqCst) < target_gen
                                    && std::time::Instant::now() < deadline
                                {
                                    std::thread::sleep(std::time::Duration::from_millis(50));
                                }
                                let _ = evt_thread.send(PluginEvent::VocalSynthReady { plugin_id });
                            });
                        if spawn.is_err() {
                            // thread spawn 失敗時は bounce を hang させないよう即 ready。
                            let _ = evt_tx.send(PluginEvent::VocalSynthReady { plugin_id });
                        }
                    } else {
                        let _ = evt_tx.send(PluginEvent::VocalSynthReady { plugin_id });
                    }
                }
            }
        }

        // drain plugin-initiated resize requests. The host
        // callback (CLAP request_resize / VST3 resizeView) ran on this
        // thread and queued (track, index, w, h); resize the owning editor
        // window then tell the plugin to lay out into the new client size.
        while let Ok((track, index, w, h)) = gui_resize_rx.try_recv() {
            if let Some(win) = editor_windows.get(&(track, index)) {
                win.set_client_size(w, h);
            }
            resize_gui(&mut tracks, track, index, w, h);
        }

        // handle plugin-initiated closes (CLAP `closed`).
        while let Ok((track, index)) = gui_close_rx.try_recv() {
            close_slot_gui(&mut tracks, &mut editor_windows, track, index, &evt_tx);
        }

        // handle editor windows the user closed via the window's
        // ✕ (WNDPROC flipped the close flag). Tear the GUI down in the
        // spec-correct order (plugin.gui_destroy → DestroyWindow) and notify
        // daw_gui so it clears its open-GUI state.
        let to_close: Vec<(u32, u32)> = editor_windows
            .iter()
            .filter(|(_, win)| win.take_close_request())
            .map(|(&key, _)| key)
            .collect();
        for (track, index) in to_close {
            close_slot_gui(&mut tracks, &mut editor_windows, track, index, &evt_tx);
        }

        unsafe {
            let mut msg = MSG::default();
            let ret = GetMessageW(&mut msg, Some(HWND(std::ptr::null_mut())), 0, 0);
            if ret.0 <= 0 {
                break;
            }
            if msg.message != WM_COMMAND_WAKE {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    // worker pool を先に shutdown して、 worker thread が止まってから
    // plugin を drop する。 順序を逆にすると UAF。
    if let Some(pool) = worker_pool.take() {
        pool.shutdown();
    }
    // tear down plugins first (`tracks.shutdown()` calls
    // `gui_destroy` = view.removed() on each plugin), THEN destroy the
    // editor windows. Reversing the order would DestroyWindow a container
    // whose plugin child is still attached.
    tracks.shutdown();
    drop(editor_windows);
    tracing::info!("plugin-main thread exiting (WM_QUIT)");
}

/// device `index` に `plugin` を置く。 単一チェーン redesign の install
/// semantics:
/// - `index == devices.len()` → append (push)
/// - `index < devices.len()`  → REPLACE。 呼び出し側が先に
///   [`detach_plugin`] で同 index の既存 device を抜き取り、
///   `WorkerPool::quiesce` で in-flight な `process()` を排出してから
///   [`teardown_plugin`] に渡しているので、 ここでは抜けた穴 (= 同 index)
///   に `Vec::insert(index, ..)` で差し戻すだけ。
///
/// ここでは新 plugin の `activate` / `start_processing` と Vec への挿入
/// のみを行う。
fn install_plugin(
    chain: &mut Chain,
    index: u32,
    mut plugin: Box<dyn LoadedPlugin>,
    sample_rate: u32,
    max_frames: u32,
) {
    // A2: 旧実装は audio thread 側の prologue で activate /
    // start_processing を呼んでいた。 worker pool 経由の process()
    // dispatch に切り替えた現在は、 chain に置く時点で started 状態に
    // しておく必要がある。
    if let Err(e) = plugin.activate(f64::from(sample_rate), 64, max_frames) {
        tracing::error!(error = ?e, index, "plugin.activate failed");
    }
    if let Err(e) = plugin.start_processing() {
        tracing::error!(error = ?e, index, "start_processing failed; plugin may be silent");
    }
    let i = (index as usize).min(chain.devices.len());
    chain.devices.insert(i, plugin);
}

/// device `index` の plugin を chain から **取り出すだけ** (DLL call なし)。
/// `Vec::remove(index)` は後続 device を 1 つ手前へ詰める。 戻り値の
/// `Box<dyn LoadedPlugin>` はまだ active / processing 状態のことがあるため、
/// 呼び出し側は registry から `None` を publish し `WorkerPool::quiesce` で
/// in-flight `process()` を排出した後、 [`teardown_plugin`] で破棄する。
///
/// 「detach → registry-None → quiesce → teardown」 の順序を分けて持つ
/// のは、 plugin-main thread が `Box` を drop する瞬間に worker thread
/// が `unsafe { &mut *entry.plugin.0 }` で deref していると UAF に
/// なるため。 詳細は `process_server.rs` の module-level docs。
fn detach_plugin(chain: &mut Chain, index: u32) -> Option<Box<dyn LoadedPlugin>> {
    let i = index as usize;
    (i < chain.devices.len()).then(|| chain.devices.remove(i))
}

/// CLAP / VST3 spec の teardown 順 (stop_processing → deactivate →
/// gui_destroy → drop) を実行する。 呼び出し側は事前に [`detach_plugin`]
/// および `WorkerPool::quiesce` を済ませて、 worker thread からの
/// 参照が無いことを保証する。
fn teardown_plugin(mut plugin: Box<dyn LoadedPlugin>) {
    plugin.stop_processing();
    plugin.deactivate();
    plugin.gui_destroy();
    drop(plugin);
}

/// Wrap `plugin.state_save()` with explicit error logging + error string
/// extraction. Mirror of the inline pattern in the `RequestSlotState` handler
/// so silent corruption (= `Err → None`) is surfaced both in logs (= plugin
/// name + track/index で trace) と IPC (= `SlotState.error` で daw_gui
/// status_message へ伝播) で見えるようになる。
fn save_plugin_state_with_err(
    plugin: &mut dyn crate::plugin_instance::LoadedPlugin,
    track_id: u32,
    index: u32,
) -> (Option<Vec<u8>>, Option<String>) {
    match plugin.state_save() {
        Ok(s) => (s, None),
        Err(e) => {
            let reason = format!("{e:#}");
            tracing::error!(
                track = track_id,
                index,
                plugin = plugin.name(),
                error = %reason,
                "state_save failed (= project save 時に plugin 状態が \
                 silent に欠落するのを防ぐため、 SlotState.error 経由で \
                 daw_gui に通知)",
            );
            (None, Some(reason))
        }
    }
}

fn collect_all_states(handle: &mut TracksHandle) -> Vec<SlotState> {
    let mut out = Vec::new();
    // Iterate tracks in deterministic id order so save files diff cleanly.
    let mut keys: Vec<u32> = handle.tracks.chains.keys().copied().collect();
    keys.sort();
    for &track_id in &keys {
        let device_count = match handle.tracks.chains.get(&track_id) {
            Some(chain) => chain.devices.len(),
            None => continue,
        };
        // Flat walk over the single `devices` Vec: one entry per device.
        for i in 0..device_count {
            let index = i as u32;
            if let Some(plugin) = handle.plugin_at_mut(track_id, index) {
                let (data, error) =
                    save_plugin_state_with_err(plugin, track_id, index);
                out.push(SlotState {
                    track: track_id,
                    index,
                    data,
                    error,
                });
            }
        }
    }
    out
}

/// open the plugin editor inside a top-level window THIS process
/// owns (created on the plugin-main thread). On success the `EditorWindow` is
/// stored in `editor_windows` keyed by (track, index). On any failure the
/// plugin GUI and the (local) window are torn down before returning, so the
/// caller never sees a half-open editor.
fn open_gui(
    handle: &mut TracksHandle,
    editor_windows: &mut HashMap<(u32, u32), editor_window::EditorWindow>,
    track: u32,
    index: u32,
    plugin_id: u32,
    title: &str,
) -> Result<Option<(u32, u32)>> {
    let Some(plugin) = handle.plugin_at_mut(track, index) else {
        return Ok(None);
    };
    if !plugin.gui_is_embed_supported() {
        tracing::warn!(plugin = %plugin.name(), "plugin does not support embedded win32 gui");
        return Ok(None);
    }
    // CLAP embedded GUI sequence per gui.h:
    //   create → set_scale → (can_resize info only) → get_size → set_parent → show
    //
    // We do NOT call set_size here: per spec that's reserved for restoring a
    // persisted size from a previous session. Calling it on first open
    // breaks plugins like VCV Rack that treat it as an invalid operation
    // before show.
    plugin.gui_create_embedded()?;

    // MVP: hardcode scale = 1.0. A DPI-aware version would query
    // `GetDpiForWindow` on the host HWND.
    if let Err(e) = plugin.gui_set_scale(1.0) {
        tracing::warn!(error = ?e, "gui.set_scale failed (ignored)");
    }

    let resizable = plugin.gui_can_resize();
    // Default to a sane size when the pre-attach query is missing or 0×0
    // (some VST3 editors only know their size after `attached`). Attaching
    // into a real-sized window avoids plugins that misbehave when parented
    // into a ~1px container; the post-attach re-query below fixes the size.
    let size = plugin
        .gui_get_size()
        .filter(|&(w, h)| w > 0 && h > 0)
        .unwrap_or((800, 600));
    tracing::info!(
        plugin = %plugin.name(),
        resizable,
        width = size.0,
        height = size.1,
        "plugin gui initial size"
    );

    // Create the host-owned, ownerless top-level container in THIS process.
    let editor = match editor_window::EditorWindow::create(plugin_id, size.0, size.1, title) {
        Ok(w) => w,
        Err(e) => {
            plugin.gui_destroy();
            return Err(anyhow::anyhow!("create editor window: {e}"));
        }
    };

    if let Err(e) = plugin.gui_set_parent_hwnd(editor.hwnd_u64()) {
        // `editor` drops here → DestroyWindow. Attach failed so the plugin's
        // gui_attached flag is false and gui_destroy's removed() is skipped.
        plugin.gui_destroy();
        drop(editor);
        return Err(e);
    }

    // Some plugins post themselves an internal "finish init" message from
    // inside set_parent. Drain whatever the plugin queued before calling
    // show so it can complete initialization on the current thread.
    pump_pending_messages();

    match plugin.gui_show() {
        Ok(true) => {}
        Ok(false) => {
            // VCV Rack 2 returns false here even though its GUI is actually
            // visible in our container. Since create + set_parent succeeded,
            // keep the GUI alive and just log — tearing down on a false return
            // from `show` destroys a working editor for these plugins.
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

    // re-query the size AFTER attach/show. Some VST3 editors
    // (e.g. Arturia Analog Lab) report 0×0 (or a placeholder) from `getSize`
    // BEFORE the view is attached, and only return the real editor size once
    // attached to a parent. If we sized the container from the pre-attach
    // value we'd get a ~1px window that looks like "the editor won't open".
    // Fall back to the pre-attach size only if the post-attach query is
    // missing or non-positive.
    let final_size = plugin
        .gui_get_size()
        .filter(|&(w, h)| w > 0 && h > 0)
        .unwrap_or(size);

    // Size the container's client area to the editor, then bring it to the
    // front. daw_gui grants foreground rights (AllowSetForegroundWindow)
    // before sending the open request, so this SetForegroundWindow is honored
    // and the editor doesn't open hidden behind the main DAW window.
    editor.set_client_size(final_size.0, final_size.1);
    editor.set_foreground();
    editor_windows.insert((track, index), editor);

    tracing::info!(
        plugin = %plugin.name(),
        width = final_size.0,
        height = final_size.1,
        "plugin gui opened"
    );
    Ok(Some(final_size))
}

/// Non-blocking drain of pending Win32 messages on the current thread. Used
/// between CLAP GUI calls that rely on a host message pump being present
/// (plugins that use `PostMessage` internally during initialization).
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

/// close the editor for (track, index): tear the plugin GUI down
/// (`gui_hide` → `gui_destroy` = view.removed()) BEFORE destroying the
/// host-owned container window, then notify daw_gui so it clears its
/// open-GUI state. Idempotent — missing plugin and/or missing window are
/// each a no-op, so it is safe to call from the WNDPROC close poll, an
/// explicit `CloseSlotGui`, and the `open_gui` failure path.
fn close_slot_gui(
    handle: &mut TracksHandle,
    editor_windows: &mut HashMap<(u32, u32), editor_window::EditorWindow>,
    track: u32,
    index: u32,
    evt_tx: &tmpsc::UnboundedSender<PluginEvent>,
) {
    if let Some(plugin) = handle.plugin_at_mut(track, index) {
        let _ = plugin.gui_hide();
        plugin.gui_destroy();
    }
    // Drop = DestroyWindow, run after gui_destroy detached the plugin child.
    editor_windows.remove(&(track, index));
    let _ = evt_tx.send(PluginEvent::SlotGuiClosed { track, index });
}

/// destroy every editor window matching `pred` (used on plugin
/// removal, matched by STABLE `plugin_id` so a window isn't orphaned when the
/// plugin's index shifted while open) and notify daw_gui with each window's
/// (track, index) key so it clears its open-GUI state. The plugin's own
/// `gui_destroy` is the caller's responsibility (it happens in
/// `teardown_plugin` before this runs); here we only drop the container
/// window (= `DestroyWindow`) and emit `SlotGuiClosed`.
fn destroy_editor_windows_where(
    editor_windows: &mut HashMap<(u32, u32), editor_window::EditorWindow>,
    evt_tx: &tmpsc::UnboundedSender<PluginEvent>,
    mut pred: impl FnMut(&(u32, u32), &editor_window::EditorWindow) -> bool,
) {
    let keys: Vec<(u32, u32)> = editor_windows
        .iter()
        .filter(|(k, w)| pred(k, w))
        .map(|(&k, _)| k)
        .collect();
    for key in keys {
        editor_windows.remove(&key); // Drop = DestroyWindow
        let _ = evt_tx.send(PluginEvent::SlotGuiClosed {
            track: key.0,
            index: key.1,
        });
    }
}

fn resize_gui(
    handle: &mut TracksHandle,
    track: u32,
    index: u32,
    width: u32,
    height: u32,
) {
    let Some(plugin) = handle.plugin_at_mut(track, index) else {
        return;
    };
    if let Err(e) = plugin.gui_set_size(width, height) {
        tracing::warn!(error = ?e, width, height, track, index, "gui.set_size failed");
    }
}

// --- pipe_loop: multiplex read (commands) + write (events) ---------------

async fn pipe_loop(
    pipe: NamedPipeClient,
    plugin: PluginThreadSender,
    mut evt_rx: tmpsc::UnboundedReceiver<PluginEvent>,
) {
    // wire.rs の framing (read_exact / write_all) は **cancellation-unsafe**。
    // 旧構造の `tokio::select! { read_msg, evt_rx.recv()=>write_msg }` は、
    // read_msg が大きい body (例: 3.8MB の LoadSong) を読んでいる途中で
    // evt_rx (= 書き戻し event の flood) が ready になると read future を drop し、
    // 既に pipe から消費したバイトを捨てる → stream が desync → 次の read が
    // body の途中を length prefix と誤読 (= 1GB の garbage length) → crash。
    // pipe を read/write half に split して別タスクで回し、read を絶対に
    // cancel しない (daw_audio::recv_loop と同じ pattern)。
    let (mut read_half, mut write_half) = tokio::io::split(pipe);
    let writer = tokio::spawn(async move {
        while let Some(evt) = evt_rx.recv().await {
            let child_msg = ChildToMain::from(evt);
            if let Err(e) = write_msg(&mut write_half, &child_msg).await {
                tracing::error!(error = ?e, ?child_msg, "failed to forward plugin event");
                break;
            }
        }
    });
    loop {
        match read_msg::<_, MainToChild>(&mut read_half).await {
            Ok(m) => handle_main_to_child(m, &plugin),
            Err(e) => {
                tracing::info!(error = ?e, "pipe ended");
                break;
            }
        }
    }
    writer.abort();
}

fn handle_main_to_child(msg: MainToChild, plugin: &PluginThreadSender) {
    match msg {
        MainToChild::SetSlotPlugin {
            track,
            index,
            format,
            path,
            plugin_id,
            initial_state,
        } => {
            tracing::info!(
                track,
                index,
                ?format,
                path = %path.display(),
                id = %plugin_id,
                has_state = initial_state.is_some(),
                "received SetSlotPlugin"
            );
            plugin.send(PluginCommand::SetSlotPlugin {
                track,
                index,
                format,
                path,
                plugin_id,
                initial_state,
            });
        }
        MainToChild::RemoveSlotPlugin { track, index } => {
            tracing::info!(track, index, "received RemoveSlotPlugin");
            plugin.send(PluginCommand::RemoveSlotPlugin { track, index });
        }
        MainToChild::ReorderChain { track, moves } => {
            tracing::info!(track, n = moves.len(), "received ReorderChain");
            plugin.send(PluginCommand::ReorderChain { track, moves });
        }
        MainToChild::RemoveTrack { track } => {
            tracing::info!(track, "received RemoveTrack");
            plugin.send(PluginCommand::RemoveTrack { track });
        }
        MainToChild::RequestSlotState { track, index } => {
            tracing::info!(track, index, "received RequestSlotState");
            plugin.send(PluginCommand::RequestSlotState { track, index });
        }
        MainToChild::SetupAraDocument { track, index, clips } => {
            tracing::info!(track, index, n = clips.len(), "received SetupAraDocument");
            plugin.send(PluginCommand::SetupAraDocument { track, index, clips });
        }
        MainToChild::ClearAraDocument { track, index } => {
            tracing::info!(track, index, "received ClearAraDocument");
            plugin.send(PluginCommand::ClearAraDocument { track, index });
        }
        MainToChild::RequestAllStates => {
            tracing::info!("received RequestAllStates");
            plugin.send(PluginCommand::RequestAllStates);
        }
        MainToChild::OpenSlotGuiEmbedded {
            track,
            index,
            title,
        } => {
            tracing::info!(track, index, %title, "received OpenSlotGuiEmbedded");
            plugin.send(PluginCommand::OpenSlotGui { track, index, title });
        }
        MainToChild::CloseSlotGui { track, index } => {
            tracing::info!(track, index, "received CloseSlotGui");
            plugin.send(PluginCommand::CloseSlotGui { track, index });
        }
        MainToChild::OpenWorkerPool {
            n_workers,
            worker_bridge_shmem_id,
            wake_event_names,
            done_event_names,
        } => {
            tracing::info!(
                n_workers,
                shmem = %worker_bridge_shmem_id,
                "received OpenWorkerPool"
            );
            plugin.send(PluginCommand::OpenWorkerPool {
                n_workers,
                worker_bridge_shmem_id,
                wake_event_names,
                done_event_names,
            });
        }
        MainToChild::CloseWorkerPool => {
            tracing::info!("received CloseWorkerPool");
            plugin.send(PluginCommand::CloseWorkerPool);
        }
        MainToChild::SetRenderMode(mode) => {
            tracing::info!(?mode, "received SetRenderMode");
            plugin.send(PluginCommand::SetRenderMode(mode));
        }
        MainToChild::SetBuiltinPluginNoteMetadata { plugin_id, bpm, entries, talk } => {
            tracing::debug!(
                plugin_id,
                bpm,
                count = entries.len(),
                talk = talk.len(),
                "received SetBuiltinPluginNoteMetadata"
            );
            plugin.send(PluginCommand::SetBuiltinPluginNoteMetadata {
                plugin_id,
                bpm,
                entries,
                talk,
            });
        }
        MainToChild::PrepareVocalSynth { plugin_id } => {
            tracing::info!(plugin_id, "received PrepareVocalSynth");
            plugin.send(PluginCommand::PrepareVocalSynth { plugin_id });
        }
        MainToChild::ExportWav { .. } => {
            // ExportWav is consumed by daw_audio (which freewheels the
            // song through its existing AudioWorker pool). The plugin
            // host doesn't drive the render any more — it only switches
            // render mode on `MainToChild::SetRenderMode`.
        }
        MainToChild::ReinitAllPlugins => {
            tracing::info!("received ReinitAllPlugins");
            plugin.send(PluginCommand::ReinitAllPlugins);
        }
        // OpenPluginShmem / ClosePluginShmem flow daw_gui → daw_audio,
        // not into the plugin host (the plugin host is the *creator* of
        // the shmem and already owns the handle in `plugin_shmems`).
        // We log if these arrive here just to flag a routing bug.
        MainToChild::OpenPluginShmem { plugin_id, shmem_id, track, index } => {
            tracing::warn!(
                plugin_id,
                shmem = %shmem_id,
                track,
                index,
                "OpenPluginShmem reached plugin_host (should be daw_audio only)"
            );
        }
        MainToChild::ClosePluginShmem { plugin_id } => {
            tracing::warn!(
                plugin_id,
                "ClosePluginShmem reached plugin_host (should be daw_audio only)"
            );
        }
        // Phase 6 review (enum exhaustiveness fix): 旧コードは `other =>` で
        // 無視していて、 `MainToChild` に新 variant を追加してもコンパイラ
        // が警告してくれず、 取りこぼし easy だった。 audio-engine 専属の
        // command 群を明示的に列挙して、 plugin_host では no-op であることを
        // doc 化する。 これで新 variant 追加時に rustc が match arm 不足を
        // 強制してくれる。
        //
        // 以下は全て daw_audio が consume するもの (= plugin_host で受け取って
        // も意味がない / 過剰ログを避けるため silent ignore)。 daw_gui の
        // broadcaster が両 child に等しく fan-out する設計のため plugin_host
        // にも届く。
        MainToChild::Ack
        | MainToChild::Play
        | MainToChild::Stop
        | MainToChild::Panic
        | MainToChild::PanicRelease
        | MainToChild::Session(_)
        | MainToChild::LoadSong(_)
        | MainToChild::SetLoop(_)
        | MainToChild::SetMasterGain(_)
        | MainToChild::BounceClipFxOnline { .. }
        | MainToChild::SeekTo { .. }
        | MainToChild::SetProjectDir(_)
        | MainToChild::SetTrackVolume { .. }
        | MainToChild::SetTrackPan { .. }
        | MainToChild::SetTrackMuted { .. }
        | MainToChild::SetTrackSolo { .. }
        | MainToChild::SetSendGain { .. }
        | MainToChild::SetSendEnabled { .. }
        | MainToChild::SetTrackArmed { .. }
        | MainToChild::SetSongBpm { .. }
        | MainToChild::SetSongTimeSigNumerator { .. }
        | MainToChild::SetRecordingLanes { .. }
        | MainToChild::SetMetronomeEnabled(_)
        | MainToChild::PreviewNoteOn { .. }
        | MainToChild::PreviewNoteOff { .. }
        | MainToChild::CancelExport
        | MainToChild::StartCountIn { .. } => {
            // daw_audio 専属、 plugin_host では no-op (silent)。
        }
    }
}

// --- Chain + audio thread ------------------------------------------------

/// Wraps a raw pointer so it can be moved into the audio thread closure.
/// Both CLAP and VST3 partition their APIs between main-thread and
/// audio-thread, so simultaneous main-thread GUI calls and audio-thread
/// `process()` calls touch disjoint fields (this assumes plugins conform
/// to the spec). The pointer is a trait-object fat pointer (data +
/// vtable) so the audio thread can call `LoadedPlugin` methods against
/// whichever backend — CLAP or VST3 — is behind the slot.
pub struct PluginPtr(pub *mut (dyn LoadedPlugin + 'static));
unsafe impl Send for PluginPtr {}
// `Sync` is the contract that the plugin-main thread and the
// process-server worker that owns this plugin's slot won't touch the
// instance simultaneously. The plugin-main thread restarts the worker
// pool whenever it mutates the chain (load/remove/swap), so a plugin
// pointer is only ever accessed by one thread at a time.
unsafe impl Sync for PluginPtr {}

/// Per-plugin process-server entry. `plugin` is the trait-object pointer
/// the worker calls `process()` on; `process_data` is the shared-memory
/// `ProcessData` slot the audio engine wrote inputs into. The pair lives
/// in `plugin_registry` keyed by `plugin_id`.
pub struct PluginEntry {
    pub plugin: PluginPtr,
    pub process_data: *mut common::process_data::ProcessData,
    /// Phase 2c: process_server (audio thread) が plugin GUI 発の
    /// param events を `PluginEvent::PluginParamTouched / ValueChanged`
    /// に詰めるための逆引き。 register / publish 時に固定。
    pub track: u32,
    pub index: u32,
}
unsafe impl Send for PluginEntry {}
unsafe impl Sync for PluginEntry {}

/// Lock-free `plugin_id` → `PluginEntry` lookup the worker pool reads
/// during dispatch. The plugin-main thread publishes a fresh `Vec` on
/// every plugin add / remove via `ArcSwap::store`; old snapshots stay
/// valid until the last worker drops its `Guard`.
pub type PluginRegistry =
    std::sync::Arc<arc_swap::ArcSwap<Vec<Option<PluginEntry>>>>;

/// Per-track signal chain owned on the plugin-main thread. The
/// process-server worker pool reads `PluginPtr` snapshots from the
/// shared `plugin_registry` to call `plugin.process()`.
///
/// Single-chain redesign (`docs/plan_linear_chain.md`): the old 3-section
/// model (midi_fx_chain / instrument / fx_chain) is collapsed into one
/// `devices: Vec<Box<dyn LoadedPlugin>>` addressed by `device_index: u32`.
/// Roles (MIDI FX / instrument / audio FX) are derived from each device's
/// declared ports by daw_audio when it walks the chain; the plugin host
/// stores devices position-only.
///
/// Each device holds a `Box<dyn LoadedPlugin>` so CLAP (`ClapPlugin`) and
/// VST3 (`Vst3Plugin`) implementations can coexist on the same chain.
/// Boxing keeps the plugin pinned on the heap so the raw pointers stored
/// in `PluginEntry` remain valid across `Vec` reallocations.
#[derive(Default)]
struct Chain {
    devices: Vec<Box<dyn LoadedPlugin>>,
}

impl Chain {
    fn plugin_at_mut(&mut self, index: u32) -> Option<&mut (dyn LoadedPlugin + '_)> {
        self.devices
            .get_mut(index as usize)
            .map(|b| &mut **b as &mut dyn LoadedPlugin)
    }
}

/// All tracks with loaded plugins. Lazily-populated: `ensure_track` creates
/// an empty chain on first access so a Track with no plugins isn't stored.
///
/// **The HashMap key is `Track::id` (stable across track add / remove /
/// reorder), not the song's Vec position.** PR2.1 moved away from
/// index-based keys so `Vec::insert` / `swap` / reorder on the GUI side
/// no longer drift the chain map. Without that, `shift_after_remove` /
/// `swap_indices` / `reorder_indices` were removed — chains are
/// addressed solely by id and Vec position changes are invisible to the
/// plugin host.
#[derive(Default)]
struct Tracks {
    chains: HashMap<u32, Chain>,
}

impl Tracks {
    fn ensure_track(&mut self, track_id: u32) -> &mut Chain {
        self.chains.entry(track_id).or_default()
    }

    fn plugin_at_mut(
        &mut self,
        track_id: u32,
        index: u32,
    ) -> Option<&mut (dyn LoadedPlugin + '_)> {
        self.chains.get_mut(&track_id).and_then(|c| c.plugin_at_mut(index))
    }
}

/// 全 track の signal chain を持つ RAII owner。 mutation は `mutate`
/// 経由で行うが、 これ自体は plain field update であり同期はしない。
/// plugin の **drop** のように worker thread からの参照と衝突する
/// 編集は、 呼び出し側で
///   `detach_plugin → publish_plugin_registry(None) →
///    WorkerPool::quiesce → teardown_plugin`
/// の順序で実施する責務がある。 invariant の詳細は process_server.rs
/// の module-level docs を参照。
struct TracksHandle {
    tracks: Tracks,
}

impl TracksHandle {
    fn new() -> Self {
        Self {
            tracks: Tracks::default(),
        }
    }

    fn plugin_at_mut(
        &mut self,
        track: u32,
        index: u32,
    ) -> Option<&mut (dyn LoadedPlugin + '_)> {
        self.tracks.plugin_at_mut(track, index)
    }

    /// 内部 `Tracks` に `f` を適用する。 process_server の worker pool
    /// が `plugin.process()` を駆動している現在、 ここでの mutation は
    /// 単なる field update に過ぎない (worker thread の synchronization は
    /// 行わない)。 worker thread が観測中の plugin を **drop** するような
    /// 編集は、 呼び出し側で `detach_plugin` + `WorkerPool::quiesce` を
    /// 経由してから `teardown_plugin` でやる。 詳細は struct doc-comment
    /// および `process_server.rs` の module-level docs 参照。
    fn mutate<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Tracks),
    {
        f(&mut self.tracks);
    }

    /// 全 plugin を main thread で drop する。 **呼び出し側は事前に
    /// worker pool を `WorkerPool::shutdown` で閉じておくこと。** さも
    /// なければ worker thread が `plugin.process()` を実行している最中に
    /// `Box` の Drop が走り UAF になる。
    fn shutdown(mut self) {
        // Drop の前に GUI 系を解除しておく (CLAP/VST3 の spec に従う
        // 順序: gui_destroy → 自動 Drop で stop_processing/deactivate/
        // destroy/terminate)。
        for chain in self.tracks.chains.values_mut() {
            for device in &mut chain.devices {
                device.gui_destroy();
            }
        }
        // ここで `Plugin::drop` が main thread で走る。
    }
}


