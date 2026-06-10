mod builtin;
mod clap_host;
mod clap_plugin;
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
    AudioSession, ChildKind, ChildToMain, MainToChild, PluginSlot, RenderMode, SlotState,
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

/// Track-and-slot-addressed events pushed from the plugin-main thread (or
/// its CLAP callbacks) to the IPC sender.
#[derive(Debug, Clone)]
pub enum PluginEvent {
    SlotGuiOpened {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    SlotGuiRequestResize {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    SlotGuiClosed {
        track: u32,
        slot: PluginSlot,
    },
    SlotPluginLoaded {
        track: u32,
        slot: PluginSlot,
        id: String,
        name: String,
        plugin_id: u32,
        shmem_id: String,
        state_load_error: Option<String>,
    },
    SlotPluginState {
        track: u32,
        slot: PluginSlot,
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
        slot: PluginSlot,
        plugin_id: u32,
        params: Vec<common::protocol::PluginParamInfo>,
    },
    /// Phase 2c: plugin GUI で knob を touch した通知 (CLAP
    /// PARAM_GESTURE_BEGIN out event 経由)。 process_server で drain
    /// して emit、 daw_gui に転送して last_touched_param を更新させる。
    PluginParamTouched {
        track: u32,
        slot: PluginSlot,
        plugin_id: u32,
        param_id: u32,
    },
    /// Phase 2c: plugin GUI で knob 値を変更した通知 (CLAP PARAM_VALUE
    /// out event 経由)。 Phase 4 recording mode で point 生成 source。
    PluginParamValueChanged {
        track: u32,
        slot: PluginSlot,
        plugin_id: u32,
        param_id: u32,
        value: f64,
    },
    /// Phase 4 Step C-3: plugin GUI で knob を release した通知 (CLAP
    /// PARAM_GESTURE_END out event 経由)。 daw_gui で
    /// `active_param_gestures` から該当 PluginParam target を remove する。
    PluginParamGestureEnd {
        track: u32,
        slot: PluginSlot,
        plugin_id: u32,
        param_id: u32,
    },
    /// `SetSlotPlugin` の load が失敗した (`load_plugin` Err か
    /// `ProcessDataHandle::create` Err)。 daw_gui の `pending_plugin_loads`
    /// を解放するために emit する。 emit せずに `continue` だけで戻ると
    /// pending stuck で Play queue が永久に解放されない。
    PluginLoadFailed {
        track: u32,
        slot: PluginSlot,
        plugin_id: String,
        reason: String,
    },
}

impl From<PluginEvent> for ChildToMain {
    fn from(e: PluginEvent) -> Self {
        match e {
            PluginEvent::SlotGuiOpened { track, slot, width, height } => {
                ChildToMain::SlotGuiOpened { track, slot, width, height }
            }
            PluginEvent::SlotGuiRequestResize { track, slot, width, height } => {
                ChildToMain::SlotGuiRequestResize { track, slot, width, height }
            }
            PluginEvent::SlotGuiClosed { track, slot } => {
                ChildToMain::SlotGuiClosed { track, slot }
            }
            PluginEvent::SlotPluginLoaded {
                track,
                slot,
                id,
                name,
                plugin_id,
                shmem_id,
                state_load_error,
            } => ChildToMain::SlotPluginLoaded {
                track,
                slot,
                id,
                name,
                plugin_id,
                shmem_id,
                state_load_error,
            },
            PluginEvent::SlotPluginState { track, slot, data } => {
                ChildToMain::SlotPluginState { track, slot, data }
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
                slot,
                plugin_id,
                params,
            } => ChildToMain::PluginParamList {
                track,
                slot,
                plugin_id,
                params,
            },
            PluginEvent::PluginParamTouched {
                track,
                slot,
                plugin_id: _,
                param_id,
            } => ChildToMain::PluginParamTouched {
                track,
                slot,
                param_id,
                // display_name は daw_gui 側で AppData.plugin_params から
                // 引いて解決する (= host で文字列構築は不要、 IPC
                // payload も短くなる)。
                display_name: format!("Param {param_id}"),
            },
            PluginEvent::PluginParamValueChanged {
                track,
                slot,
                plugin_id: _,
                param_id,
                value,
            } => ChildToMain::PluginParamValueChanged {
                track,
                slot,
                param_id,
                value,
            },
            PluginEvent::PluginParamGestureEnd {
                track,
                slot,
                plugin_id: _,
                param_id,
            } => ChildToMain::PluginParamGestureEnd {
                track,
                slot,
                param_id,
            },
            PluginEvent::PluginLoadFailed {
                track,
                slot,
                plugin_id,
                reason,
            } => ChildToMain::SlotPluginLoadFailed {
                track,
                slot,
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
            slot: e.slot,
        }))
        .collect();
    let idx = plugin_id as usize;
    if next.len() <= idx {
        next.resize_with(idx + 1, || None);
    }
    next[idx] = entry;
    registry.store(std::sync::Arc::new(next));
}

/// Re-publish an existing registry entry with a corrected `(track, slot)`
/// address, preserving the live `plugin` pointer and `process_data` slot.
/// Used after `MoveSlot` reorders a chain so `PluginEntry.slot` (read by
/// the worker pool to stamp param events) reflects the new position.
/// No-op if the plugin id has no live entry.
fn republish_entry_slot(
    registry: &PluginRegistry,
    plugin_id: u32,
    track: u32,
    slot: PluginSlot,
) {
    let current = registry.load();
    let Some(Some(existing)) = current.get(plugin_id as usize) else {
        return;
    };
    let entry = PluginEntry {
        plugin: PluginPtr(existing.plugin.0),
        process_data: existing.process_data,
        track,
        slot,
    };
    // Drop the load guard before re-entering `publish_plugin_registry`
    // (which takes its own `load()`); avoids holding two guards at once.
    drop(current);
    publish_plugin_registry(registry, plugin_id, Some(entry));
}

/// Commands processed serially on the plugin-main thread.
enum PluginCommand {
    SetSlotPlugin {
        track: u32,
        slot: PluginSlot,
        format: PluginFormat,
        path: PathBuf,
        plugin_id: String,
        initial_state: Option<Vec<u8>>,
    },
    RemoveSlotPlugin {
        track: u32,
        slot: PluginSlot,
    },
    MoveSlot {
        track: u32,
        from: PluginSlot,
        to: PluginSlot,
    },
    RemoveTrack {
        track: u32,
    },
    RequestSlotState {
        track: u32,
        slot: PluginSlot,
    },
    RequestAllStates,
    OpenSlotGui {
        track: u32,
        slot: PluginSlot,
        host_hwnd: u64,
    },
    CloseSlotGui {
        track: u32,
        slot: PluginSlot,
    },
    ResizeSlotGui {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
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
    common::logging::init_tracing();
    tracing::info!("daw_plugin_host started");

    // FIXME #26 Phase B: one-shot VST3 note-effect probe モード。 daw_gui の
    // rescan が VST3 ごとにこのプロセスを使い捨てで起動し (プロセス隔離 +
    // caller 側 timeout)、 bus 構成から note-effect 判定を得る。 plugin の
    // instantiate を別プロセスへ押し込むことで、 壊れた / ハングする VST3 が
    // スキャン本体を巻き込まない。 stdout に `note_effect=<bool>` を 1 行出して
    // 即 exit (IPC handshake へ進まない)。 どんな失敗でも false で出して退行
    // させない (caller は FX 扱いに fallback)。
    if std::env::args().nth(1).as_deref() == Some("--probe-vst3") {
        let path = std::env::args()
            .nth(2)
            .context("--probe-vst3 needs <path>")?;
        let target_id = std::env::args().nth(3).unwrap_or_default();
        // VST3 instantiate は plugin-main thread idiom に合わせ専用 thread で。
        let is_note_effect = std::thread::spawn(move || {
            vst3_plugin::probe_note_effect(std::path::Path::new(&path), &target_id)
                .unwrap_or(false)
        })
        .join()
        .unwrap_or(false);
        println!("note_effect={is_note_effect}");
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
    //   - `plugin_lookup` maps `(track, slot)` to the live plugin id so
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
    let mut plugin_lookup: HashMap<(u32, PluginSlot), u32> = HashMap::new();
    // Defensive dedup: if the GUI somehow sends `SetSlotPlugin` twice
    // for the same (track, slot, plugin_id) (we've seen the picker
    // double-fire) we skip the second to avoid the workers racing on
    // a destroy → re-install path. Keyed by (track, slot) → loaded
    // plugin's stable id string.
    let mut loaded_id_for_slot: HashMap<(u32, PluginSlot), String> = HashMap::new();
    // PR4.5 fix: cache the display name + shmem id alongside loaded_id so
    // we can re-emit a `SlotPluginLoaded` event when SetSlotPlugin arrives
    // for an already-loaded slot (= 2nd LoadSong of the same project).
    // Without re-emitting, daw_gui's `pending_plugin_loads` never clears
    // and queued Play (`pending_play`) can never fire — playback freezes.
    let mut loaded_meta_for_slot: HashMap<(u32, PluginSlot), (String, String)> =
        HashMap::new();
    let plugin_registry: PluginRegistry =
        Arc::new(arc_swap::ArcSwap::from_pointee(Vec::new()));

    // Per-(track, slot) host callbacks: each loaded plugin captures its
    // (track, slot) so the async CLAP callback (request_resize / closed)
    // can stamp the event with the correct address before reaching daw_gui.
    let make_callbacks = |track: u32, slot: PluginSlot| HostCallbacks {
        on_request_resize: {
            let tx = evt_tx.clone();
            Arc::new(move |w, h| {
                let _ = tx.send(PluginEvent::SlotGuiRequestResize {
                    track,
                    slot,
                    width: w,
                    height: h,
                });
            })
        },
        on_closed: {
            let tx = evt_tx.clone();
            Arc::new(move || {
                let _ = tx.send(PluginEvent::SlotGuiClosed { track, slot });
            })
        },
        // VST3 param gesture (IComponentHandler::beginEdit/performEdit/endEdit)。
        // resize / closed と同 idiom で evt_tx に流す。 plugin_id は
        // PluginEvent → ChildToMain 変換で破棄される (= daw_gui は (track, slot,
        // param_id) で解決する) ので 0 placeholder。 CLAP plugin はこの callback
        // を呼ばない (out_events 経由) ので二重発火しない。
        on_param_gesture_begin: {
            let tx = evt_tx.clone();
            Arc::new(move |param_id| {
                let _ = tx.send(PluginEvent::PluginParamTouched {
                    track,
                    slot,
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
                    slot,
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
                    slot,
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
                        for plugin in chain
                            .midi_fx_chain
                            .iter_mut()
                            .chain(chain.instrument.iter_mut())
                            .chain(chain.fx_chain.iter_mut())
                        {
                            let _ = plugin.set_render_mode(mode);
                        }
                    }
                    tracing::info!(?mode, "render mode broadcast to all plugins");
                }
                PluginCommand::SetSlotPlugin {
                    track,
                    slot,
                    format,
                    path,
                    plugin_id,
                    initial_state,
                } => {
                    // Defensive dedup against picker double-fire. Same
                    // plugin id at the same slot ⇒ ignore re-load, but
                    // we must STILL emit `SlotPluginLoaded` so daw_gui
                    // can clear its `pending_plugin_loads` entry for this
                    // slot — otherwise a second project-load (same plugins)
                    // leaves the entry pending forever and `play()`
                    // refuses to start (`pending_play=true` in
                    // `app.rs::play()`).
                    if loaded_id_for_slot.get(&(track, slot)) == Some(&plugin_id) {
                        tracing::info!(
                            track,
                            ?slot,
                            id = %plugin_id,
                            "SetSlotPlugin: same plugin already loaded, re-emitting SlotPluginLoaded"
                        );
                        if let (Some(&new_plugin_id), Some((cached_id, cached_name))) = (
                            plugin_lookup.get(&(track, slot)),
                            loaded_meta_for_slot.get(&(track, slot)),
                        ) {
                            let shmem_id = format!(
                                "daw_01_pd_{plugin_host_pid}_{new_plugin_id}"
                            );
                            let _ = evt_tx.send(PluginEvent::SlotPluginLoaded {
                                track,
                                slot,
                                id: cached_id.clone(),
                                name: cached_name.clone(),
                                plugin_id: new_plugin_id,
                                shmem_id,
                                // 同 plugin の re-emit path。 state_load
                                // を呼んでいないので error は常に None。
                                state_load_error: None,
                            });
                        } else {
                            tracing::warn!(
                                track, ?slot,
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
                    let callbacks = make_callbacks(track, slot);
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
                                slot,
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
                                    ?slot,
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
                    let old_pid = plugin_lookup.get(&(track, slot)).copied();
                    let mut detached_old: Option<Box<dyn LoadedPlugin>> = None;
                    tracks.mutate(|t| {
                        if let Some(chain) = t.chains.get_mut(&track) {
                            detached_old = detach_plugin(chain, slot);
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
                        plugin_lookup.remove(&(track, slot));
                        plugin_shmems.remove(&pid);
                    }

                    // (6) 新 plugin を chain に install (activate +
                    //     start_processing 含む)。
                    let loaded_id = plugin.id().to_string();
                    let loaded_name = plugin.name().to_string();
                    let sr = session.sample_rate;
                    let mf = session.max_frames;
                    tracks.mutate(|t| {
                        install_plugin(t.ensure_track(track), slot, plugin, sr, mf)
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
                            plugin_lookup.insert((track, slot), new_plugin_id);
                            loaded_id_for_slot.insert((track, slot), plugin_id.clone());
                            // PR4.5: 同 plugin_id の SetSlotPlugin が再度
                            // 来たとき (= 同プロジェクトの 2 度目 LoadSong)
                            // に dedup branch から SlotPluginLoaded を再
                            // emit するためのキャッシュ。
                            loaded_meta_for_slot.insert(
                                (track, slot),
                                (loaded_id.clone(), loaded_name.clone()),
                            );

                            // Install 直後の plugin pointer を short-lived な
                            // borrow scope で取得。 borrow が抜けたあとで
                            // tracks を再利用する。
                            let plugin_ptr_raw: Option<*mut (dyn LoadedPlugin + 'static)> = {
                                let opt = tracks.tracks.plugin_at_mut(track, slot);
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
                                        slot,
                                    }),
                                );
                            }
                            let _ = evt_tx.send(PluginEvent::SlotPluginLoaded {
                                track,
                                slot,
                                id: loaded_id,
                                name: loaded_name,
                                plugin_id: new_plugin_id,
                                shmem_id,
                                state_load_error,
                            });
                            // PR3.3: activate 直後 (CLAP `[main-thread &
                            // active]` / VST3 `[UI-thread & Setup Done]`
                            // を満たすここ) で plugin の latency を query
                            // して daw_gui へ送る。 samples == 0 でも送る
                            // ことで、 同 slot の plugin 入れ替え時に
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
                                let _ = evt_tx.send(PluginEvent::PluginParamList {
                                    track,
                                    slot,
                                    plugin_id: new_plugin_id,
                                    params,
                                });
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
                            // (line 540) なので slot は空のまま。
                            let mut detached: Option<Box<dyn LoadedPlugin>> = None;
                            tracks.mutate(|t| {
                                if let Some(chain) = t.chains.get_mut(&track) {
                                    detached = detach_plugin(chain, slot);
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
                                slot,
                                plugin_id: plugin_id.clone(),
                                reason: format!("shmem create failed: {e}"),
                            });
                        }
                    }
                }
                PluginCommand::RemoveSlotPlugin { track, slot } => {
                    // (1) plugin を chain から取り出すだけ (DLL call なし)。
                    let mut detached: Option<Box<dyn LoadedPlugin>> = None;
                    tracks.mutate(|t| {
                        if let Some(chain) = t.chains.get_mut(&track) {
                            detached = detach_plugin(chain, slot);
                        }
                    });
                    let removed_pid = plugin_lookup.get(&(track, slot)).copied();

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
                        plugin_lookup.remove(&(track, slot));
                        plugin_shmems.remove(&pid);
                        loaded_id_for_slot.remove(&(track, slot));
                        loaded_meta_for_slot.remove(&(track, slot));
                        // PR2.1: daw_gui に `ClosePluginShmem` を audio engine
                        // に転送させて、 audio thread が destroyed plugin に
                        // process() を呼び続けないようにする。
                        let _ = evt_tx.send(PluginEvent::SlotPluginUnloaded {
                            plugin_id: pid,
                        });
                    }
                }
                PluginCommand::MoveSlot { track, from, to } => {
                    let mut moved: Option<MovedRange> = None;
                    tracks.mutate(|t| {
                            if let Some(chain) = t.chains.get_mut(&track) {
                                moved = move_plugin(chain, from, to);
                            }
                    });
                    // move_plugin が Vec を並べ替えただけでは plugin_lookup /
                    // loaded_id_for_slot / loaded_meta_for_slot / PluginEntry.slot
                    // が旧順序のまま陳腐化する。 影響を受けた slot range を
                    // 新順序に貼り直し、 該当 plugin の registry entry を再 publish
                    // して slot 逆引きを正す。
                    if let Some(MovedRange { is_midi, a, b }) = moved {
                        let lo = a.min(b);
                        let hi = a.max(b);
                        let make_slot = |i: usize| {
                            if is_midi {
                                PluginSlot::MidiFx(i as u32)
                            } else {
                                PluginSlot::Fx(i as u32)
                            }
                        };
                        // `remove(a)` + `insert(b)` の置換: old index → new index。
                        let new_index = |old: usize| -> usize {
                            if old == a {
                                b
                            } else if a < b {
                                // a+1..=b が 1 つ手前へ詰まる。
                                old - 1
                            } else {
                                // b..=a-1 が 1 つ後ろへずれる。
                                old + 1
                            }
                        };
                        // (1) 影響 range の旧 bookkeeping を snapshot してから
                        //     一旦剥がす (新旧 slot の衝突を避けるため)。
                        // (slot, plugin_id, loaded_id, loaded_meta(id, name))
                        type SlotBookkeeping =
                            (PluginSlot, u32, Option<String>, Option<(String, String)>);
                        let mut remapped: Vec<SlotBookkeeping> = Vec::new();
                        for old in lo..=hi {
                            let old_slot = make_slot(old);
                            if let Some(&pid) = plugin_lookup.get(&(track, old_slot)) {
                                let lid = loaded_id_for_slot.remove(&(track, old_slot));
                                let meta = loaded_meta_for_slot.remove(&(track, old_slot));
                                plugin_lookup.remove(&(track, old_slot));
                                let new_slot = make_slot(new_index(old));
                                remapped.push((new_slot, pid, lid, meta));
                            }
                        }
                        // (2) 新 slot で貼り直し + registry entry の slot を補正。
                        for (new_slot, pid, lid, meta) in remapped {
                            plugin_lookup.insert((track, new_slot), pid);
                            if let Some(lid) = lid {
                                loaded_id_for_slot.insert((track, new_slot), lid);
                            }
                            if let Some(meta) = meta {
                                loaded_meta_for_slot.insert((track, new_slot), meta);
                            }
                            republish_entry_slot(&plugin_registry, pid, track, new_slot);
                        }
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
                        for mfx in chain.midi_fx_chain.drain(..) {
                            teardown_plugin(mfx);
                        }
                        if let Some(inst) = chain.instrument.take() {
                            teardown_plugin(inst);
                        }
                        for fx in chain.fx_chain.drain(..) {
                            teardown_plugin(fx);
                        }
                    }

                    plugin_lookup.retain(|&(t, _), _| t != track);
                    loaded_id_for_slot.retain(|&(t, _), _| t != track);
                    loaded_meta_for_slot.retain(|&(t, _), _| t != track);
                    for pid in removed_pids {
                        plugin_shmems.remove(&pid);
                        // registry は (2) で既に None 化済み。
                        let _ = evt_tx.send(PluginEvent::SlotPluginUnloaded { plugin_id: pid });
                    }
                }
                PluginCommand::RequestSlotState { track, slot } => {
                    let data = match tracks.plugin_at_mut(track, slot) {
                        Some(plugin) => match plugin.state_save() {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!(error = ?e, "state_save failed");
                                None
                            }
                        },
                        None => None,
                    };
                    let _ = evt_tx.send(PluginEvent::SlotPluginState { track, slot, data });
                }
                PluginCommand::RequestAllStates => {
                    let entries = collect_all_states(&mut tracks);
                    let _ = evt_tx.send(PluginEvent::AllPluginStates { entries });
                }
                PluginCommand::OpenSlotGui { track, slot, host_hwnd } => {
                    match open_gui(&mut tracks, track, slot, host_hwnd) {
                        Ok(Some((w, h))) => {
                            let _ = evt_tx.send(PluginEvent::SlotGuiOpened {
                                track,
                                slot,
                                width: w,
                                height: h,
                            });
                        }
                        Ok(None) => {
                            let _ = evt_tx.send(PluginEvent::SlotGuiClosed { track, slot });
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, track, ?slot, "failed to open GUI");
                            close_gui(&mut tracks, track, slot);
                            let _ = evt_tx.send(PluginEvent::SlotGuiClosed { track, slot });
                        }
                    }
                }
                PluginCommand::CloseSlotGui { track, slot } => {
                    close_gui(&mut tracks, track, slot);
                    let _ = evt_tx.send(PluginEvent::SlotGuiClosed { track, slot });
                }
                PluginCommand::ResizeSlotGui {
                    track,
                    slot,
                    width,
                    height,
                } => {
                    resize_gui(&mut tracks, track, slot, width, height);
                }
                PluginCommand::SetBuiltinPluginNoteMetadata {
                    plugin_id,
                    bpm,
                    entries,
                } => {
                    // plugin_lookup は `(track, slot) -> plugin_id` の
                    // forward map。 逆引きは O(n) walk。 同 frame に
                    // 数件しか呼ばれない ので n ≤ 数十で問題なし。
                    let target =
                        plugin_lookup.iter().find_map(|(k, v)| {
                            (*v == plugin_id).then_some(*k)
                        });
                    let Some((track, slot)) = target else {
                        tracing::warn!(
                            plugin_id,
                            "SetBuiltinPluginNoteMetadata: plugin_id not found"
                        );
                        continue;
                    };
                    tracks.mutate(|t| {
                        if let Some(plugin) = t.plugin_at_mut(track, slot) {
                            plugin.set_note_metadata(bpm, &entries);
                        }
                    });
                }
            }
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
    tracks.shutdown();
    tracing::info!("plugin-main thread exiting (WM_QUIT)");
}

/// `slot` に `plugin` を挿入する。 同じ slot に既存 plugin がある場合は
/// 呼び出し側で先に [`detach_plugin`] で抜き取り、 `WorkerPool::quiesce`
/// で in-flight な `process()` を排出してから [`teardown_plugin`] に
/// 渡す責務がある。 ここでは新 plugin の `activate` / `start_processing`
/// と Vec への挿入のみを行う。
fn install_plugin(
    chain: &mut Chain,
    slot: PluginSlot,
    mut plugin: Box<dyn LoadedPlugin>,
    sample_rate: u32,
    max_frames: u32,
) {
    // A2: 旧実装は audio thread 側の prologue で activate /
    // start_processing を呼んでいた。 worker pool 経由の process()
    // dispatch に切り替えた現在は、 chain に置く時点で started 状態に
    // しておく必要がある。
    if let Err(e) = plugin.activate(f64::from(sample_rate), 64, max_frames) {
        tracing::error!(error = ?e, ?slot, "plugin.activate failed");
    }
    if let Err(e) = plugin.start_processing() {
        tracing::error!(error = ?e, ?slot, "start_processing failed; plugin may be silent");
    }
    match slot {
        PluginSlot::Instrument => {
            chain.instrument = Some(plugin);
        }
        PluginSlot::Fx(i) => {
            let i = i as usize;
            if i <= chain.fx_chain.len() {
                chain.fx_chain.insert(i, plugin);
            } else {
                chain.fx_chain.push(plugin);
            }
        }
        PluginSlot::MidiFx(i) => {
            let i = i as usize;
            if i <= chain.midi_fx_chain.len() {
                chain.midi_fx_chain.insert(i, plugin);
            } else {
                chain.midi_fx_chain.push(plugin);
            }
        }
    }
}

/// `slot` の plugin を chain から **取り出すだけ** (DLL call なし)。
/// 戻り値の `Box<dyn LoadedPlugin>` はまだ active / processing 状態の
/// ことがあるため、 呼び出し側は registry から `None` を publish し
/// `WorkerPool::quiesce` で in-flight `process()` を排出した後、
/// [`teardown_plugin`] で破棄する。
///
/// 「detach → registry-None → quiesce → teardown」 の順序を分けて持つ
/// のは、 plugin-main thread が `Box` を drop する瞬間に worker thread
/// が `unsafe { &mut *entry.plugin.0 }` で deref していると UAF に
/// なるため。 詳細は `process_server.rs` の module-level docs。
fn detach_plugin(chain: &mut Chain, slot: PluginSlot) -> Option<Box<dyn LoadedPlugin>> {
    match slot {
        PluginSlot::Instrument => chain.instrument.take(),
        PluginSlot::Fx(i) => {
            let i = i as usize;
            (i < chain.fx_chain.len()).then(|| chain.fx_chain.remove(i))
        }
        PluginSlot::MidiFx(i) => {
            let i = i as usize;
            (i < chain.midi_fx_chain.len()).then(|| chain.midi_fx_chain.remove(i))
        }
    }
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

/// `move_plugin` の戻り値。 実際に reorder が起きたときだけ `Some` を返し、
/// 影響を受けた chain 種別 (`is_midi`) と元の `(a, b)` index を伝える。
/// 呼び出し側はこれを使って `plugin_lookup` / `loaded_*_for_slot` /
/// `PluginEntry.slot` を新順序に貼り直す。
struct MovedRange {
    is_midi: bool,
    a: usize,
    b: usize,
}

fn move_plugin(chain: &mut Chain, from: PluginSlot, to: PluginSlot) -> Option<MovedRange> {
    // Only Fx↔Fx and MidiFx↔MidiFx reorders are supported for MVP.
    match (from, to) {
        (PluginSlot::Fx(a), PluginSlot::Fx(b)) => {
            let a = a as usize;
            let b = b as usize;
            if a < chain.fx_chain.len() && b < chain.fx_chain.len() && a != b {
                let plugin = chain.fx_chain.remove(a);
                chain.fx_chain.insert(b, plugin);
                return Some(MovedRange { is_midi: false, a, b });
            }
            None
        }
        (PluginSlot::MidiFx(a), PluginSlot::MidiFx(b)) => {
            let a = a as usize;
            let b = b as usize;
            if a < chain.midi_fx_chain.len() && b < chain.midi_fx_chain.len() && a != b {
                let plugin = chain.midi_fx_chain.remove(a);
                chain.midi_fx_chain.insert(b, plugin);
                return Some(MovedRange { is_midi: true, a, b });
            }
            None
        }
        _ => None,
    }
}

/// Wrap `plugin.state_save()` with explicit error logging + error string
/// extraction. Mirror of the inline pattern in `RequestSlotState` handler
/// (line 935-941) so silent corruption (= `Err → None`) is surfaced both
/// in logs (= plugin name + track/slot で trace) と IPC (= `SlotState
/// .error` で daw_gui status_message へ伝播) で見えるようになる。
fn save_plugin_state_with_err(
    plugin: &mut dyn crate::plugin_instance::LoadedPlugin,
    track_id: u32,
    slot: PluginSlot,
) -> (Option<Vec<u8>>, Option<String>) {
    match plugin.state_save() {
        Ok(s) => (s, None),
        Err(e) => {
            let reason = format!("{e:#}");
            tracing::error!(
                track = track_id,
                ?slot,
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
        let (mfx_count, has_inst, fx_count) = {
            let Some(chain) = handle.tracks.chains.get(&track_id) else {
                continue;
            };
            (
                chain.midi_fx_chain.len(),
                chain.instrument.is_some(),
                chain.fx_chain.len(),
            )
        };
        for i in 0..mfx_count {
            let slot = PluginSlot::MidiFx(i as u32);
            if let Some(plugin) = handle.plugin_at_mut(track_id, slot) {
                let (data, error) =
                    save_plugin_state_with_err(plugin, track_id, slot);
                out.push(SlotState {
                    track: track_id,
                    slot,
                    data,
                    error,
                });
            }
        }
        if has_inst {
            let slot = PluginSlot::Instrument;
            if let Some(plugin) = handle.plugin_at_mut(track_id, slot) {
                let (data, error) =
                    save_plugin_state_with_err(plugin, track_id, slot);
                out.push(SlotState {
                    track: track_id,
                    slot,
                    data,
                    error,
                });
            }
        }
        for i in 0..fx_count {
            let slot = PluginSlot::Fx(i as u32);
            if let Some(plugin) = handle.plugin_at_mut(track_id, slot) {
                let (data, error) =
                    save_plugin_state_with_err(plugin, track_id, slot);
                out.push(SlotState {
                    track: track_id,
                    slot,
                    data,
                    error,
                });
            }
        }
    }
    out
}

fn open_gui(
    handle: &mut TracksHandle,
    track: u32,
    slot: PluginSlot,
    host_hwnd: u64,
) -> Result<Option<(u32, u32)>> {
    let Some(plugin) = handle.plugin_at_mut(track, slot) else {
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
    let size = plugin.gui_get_size().unwrap_or((800, 600));
    tracing::info!(
        plugin = %plugin.name(),
        resizable,
        width = size.0,
        height = size.1,
        "plugin gui initial size"
    );

    plugin.gui_set_parent_hwnd(host_hwnd)?;

    // Some plugins post themselves an internal "finish init" message from
    // inside set_parent. Drain whatever the plugin queued before calling
    // show so it can complete initialization on the current thread.
    pump_pending_messages();

    let shown = plugin.gui_show()?;
    if !shown {
        // VCV Rack 2 returns false here even though its GUI is actually
        // visible in our container. Since create + set_parent succeeded,
        // keep the GUI alive and just log — tearing down on a false return
        // from `show` destroys a working editor for these plugins.
        tracing::warn!(
            plugin = %plugin.name(),
            "gui.show returned false; keeping GUI alive (plugin may have already shown itself)"
        );
    }
    tracing::info!(plugin = %plugin.name(), width = size.0, height = size.1, "plugin gui opened");
    Ok(Some(size))
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

fn close_gui(handle: &mut TracksHandle, track: u32, slot: PluginSlot) {
    let Some(plugin) = handle.plugin_at_mut(track, slot) else {
        return;
    };
    let _ = plugin.gui_hide();
    plugin.gui_destroy();
}

fn resize_gui(
    handle: &mut TracksHandle,
    track: u32,
    slot: PluginSlot,
    width: u32,
    height: u32,
) {
    let Some(plugin) = handle.plugin_at_mut(track, slot) else {
        return;
    };
    if let Err(e) = plugin.gui_set_size(width, height) {
        tracing::warn!(error = ?e, width, height, track, ?slot, "gui.set_size failed");
    }
}

// --- pipe_loop: multiplex read (commands) + write (events) ---------------

async fn pipe_loop(
    mut pipe: NamedPipeClient,
    plugin: PluginThreadSender,
    mut evt_rx: tmpsc::UnboundedReceiver<PluginEvent>,
) {
    loop {
        tokio::select! {
            msg = read_msg::<_, MainToChild>(&mut pipe) => {
                match msg {
                    Ok(m) => handle_main_to_child(m, &plugin),
                    Err(e) => {
                        tracing::info!(error = ?e, "pipe ended");
                        return;
                    }
                }
            }
            evt = evt_rx.recv() => {
                let Some(evt) = evt else { return };
                let child_msg = ChildToMain::from(evt);
                if let Err(e) = write_msg(&mut pipe, &child_msg).await {
                    tracing::error!(error = ?e, ?child_msg, "failed to forward plugin event");
                    return;
                }
            }
        }
    }
}

fn handle_main_to_child(msg: MainToChild, plugin: &PluginThreadSender) {
    match msg {
        MainToChild::SetSlotPlugin {
            track,
            slot,
            format,
            path,
            plugin_id,
            initial_state,
        } => {
            tracing::info!(
                track,
                ?slot,
                ?format,
                path = %path.display(),
                id = %plugin_id,
                has_state = initial_state.is_some(),
                "received SetSlotPlugin"
            );
            plugin.send(PluginCommand::SetSlotPlugin {
                track,
                slot,
                format,
                path,
                plugin_id,
                initial_state,
            });
        }
        MainToChild::RemoveSlotPlugin { track, slot } => {
            tracing::info!(track, ?slot, "received RemoveSlotPlugin");
            plugin.send(PluginCommand::RemoveSlotPlugin { track, slot });
        }
        MainToChild::MoveSlot { track, from, to } => {
            tracing::info!(track, ?from, ?to, "received MoveSlot");
            plugin.send(PluginCommand::MoveSlot { track, from, to });
        }
        MainToChild::RemoveTrack { track } => {
            tracing::info!(track, "received RemoveTrack");
            plugin.send(PluginCommand::RemoveTrack { track });
        }
        MainToChild::RequestSlotState { track, slot } => {
            tracing::info!(track, ?slot, "received RequestSlotState");
            plugin.send(PluginCommand::RequestSlotState { track, slot });
        }
        MainToChild::RequestAllStates => {
            tracing::info!("received RequestAllStates");
            plugin.send(PluginCommand::RequestAllStates);
        }
        MainToChild::OpenSlotGuiEmbedded {
            track,
            slot,
            host_hwnd,
        } => {
            tracing::info!(track, ?slot, host_hwnd, "received OpenSlotGuiEmbedded");
            plugin.send(PluginCommand::OpenSlotGui { track, slot, host_hwnd });
        }
        MainToChild::CloseSlotGui { track, slot } => {
            tracing::info!(track, ?slot, "received CloseSlotGui");
            plugin.send(PluginCommand::CloseSlotGui { track, slot });
        }
        MainToChild::ResizeSlotGui {
            track,
            slot,
            width,
            height,
        } => {
            tracing::info!(track, ?slot, width, height, "received ResizeSlotGui");
            plugin.send(PluginCommand::ResizeSlotGui {
                track,
                slot,
                width,
                height,
            });
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
        MainToChild::SetBuiltinPluginNoteMetadata { plugin_id, bpm, entries } => {
            tracing::debug!(
                plugin_id,
                bpm,
                count = entries.len(),
                "received SetBuiltinPluginNoteMetadata"
            );
            plugin.send(PluginCommand::SetBuiltinPluginNoteMetadata {
                plugin_id,
                bpm,
                entries,
            });
        }
        MainToChild::ExportWav { .. } => {
            // ExportWav is consumed by daw_audio (which freewheels the
            // song through its existing AudioWorker pool). The plugin
            // host doesn't drive the render any more — it only switches
            // render mode on `MainToChild::SetRenderMode`.
        }
        // OpenPluginShmem / ClosePluginShmem flow daw_gui → daw_audio,
        // not into the plugin host (the plugin host is the *creator* of
        // the shmem and already owns the handle in `plugin_shmems`).
        // We log if these arrive here just to flag a routing bug.
        MainToChild::OpenPluginShmem { plugin_id, shmem_id, track, slot } => {
            tracing::warn!(
                plugin_id,
                shmem = %shmem_id,
                track,
                ?slot,
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
    pub slot: PluginSlot,
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
/// Each slot holds a `Box<dyn LoadedPlugin>` so CLAP (`ClapPlugin`) and
/// VST3 (`Vst3Plugin`) implementations can coexist on the same chain.
/// Boxing keeps the plugin pinned on the heap so the raw pointers stored
/// in `PluginEntry` remain valid across `Vec` reallocations.
#[derive(Default)]
struct Chain {
    /// Note-effect plugins executed before the instrument (e.g. arpeggiators).
    /// Events flow left-to-right, with each plugin's emitted notes feeding
    /// the next.
    midi_fx_chain: Vec<Box<dyn LoadedPlugin>>,
    /// Instrument slot (note→audio). `None` = no instrument loaded on the
    /// track; audio thread produces silence at the instrument stage.
    instrument: Option<Box<dyn LoadedPlugin>>,
    /// Audio effects applied in order after the instrument.
    fx_chain: Vec<Box<dyn LoadedPlugin>>,
}

impl Chain {
    fn plugin_at_mut(&mut self, slot: PluginSlot) -> Option<&mut (dyn LoadedPlugin + '_)> {
        match slot {
            PluginSlot::MidiFx(i) => self
                .midi_fx_chain
                .get_mut(i as usize)
                .map(|b| &mut **b as &mut dyn LoadedPlugin),
            PluginSlot::Instrument => self
                .instrument
                .as_mut()
                .map(|b| &mut **b as &mut dyn LoadedPlugin),
            PluginSlot::Fx(i) => self
                .fx_chain
                .get_mut(i as usize)
                .map(|b| &mut **b as &mut dyn LoadedPlugin),
        }
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
        slot: PluginSlot,
    ) -> Option<&mut (dyn LoadedPlugin + '_)> {
        self.chains.get_mut(&track_id).and_then(|c| c.plugin_at_mut(slot))
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
        slot: PluginSlot,
    ) -> Option<&mut (dyn LoadedPlugin + '_)> {
        self.tracks.plugin_at_mut(track, slot)
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
            for mfx in &mut chain.midi_fx_chain {
                mfx.gui_destroy();
            }
            if let Some(inst) = chain.instrument.as_mut() {
                inst.gui_destroy();
            }
            for fx in &mut chain.fx_chain {
                fx.gui_destroy();
            }
        }
        // ここで `Plugin::drop` が main thread で走る。
    }
}


