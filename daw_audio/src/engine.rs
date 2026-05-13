//! Audio engine. Drives the per-buffer pipeline (sequencer → mixer →
//! plugin handshake → master) from the CPAL output stream callback.
//!
//! State is split in two:
//! - `SharedState` is `Arc`-clone-able and read by the audio thread (every
//!   buffer) while the IPC receive loop and GUI commands publish into it
//!   wait-free (`Atomic*` / `ArcSwap`).
//! - `LocalState` lives exclusively inside the CPAL closure. It owns the
//!   pre-allocated scratch buffers and the routing snapshot, so the RT
//!   loop never touches a `Mutex`.
//!
//! Plugin handshake is currently a stub: PR3 only wires sequencer + vocal
//! sample playback + mixer + master accumulation. The `routing.tracks`
//! vec stays empty until PR5 builds it from `LoadSong`, so the master bus
//! is silent — that's the expected PR3 behaviour. Plugin process is
//! filled in by PR5 (handshake) and parallelised in PR6 (worker pool).

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use arc_swap::{ArcSwap, ArcSwapOption};
use common::audio_bridge::AudioBridgeHandle;
use common::model::{Song, Track};
use common::plugin_ref::{PluginRef, WorkerSyncRef};
use common::process_data::EventKind;
use common::protocol::PluginSlot;
use common::timing::{effective_loop_bounds, song_ended};
use common::worker_bridge::WorkerBridgeHandle;

use crate::audio_clip_renderer::AudioClipRenderer;
use crate::audio_worker::AudioWorkerPool;
use crate::graph::{BufRef, NodeOp, Schedule, compile_schedule};
use crate::mixer::TrackScratch;
use crate::sequencer::{NoteTransition, TimedNoteEvent, collect_events_for_buffer};

/// VOICEVOX synthesis result key. The GUI sends each freshly-rendered
/// vocal clip via `MainToChild::SetGeneratedAudio` keyed with this id;
/// `process_track_owned` looks the buffer up by the same key when it
/// renders a Vocal track that has no instrument plugin. Encoding both
/// `track_id` and `clip_id` into one `u64` lets a single Vocal track
/// host multiple independent vocal clips without overwriting each
/// other (= the old `vocal_store` was track-keyed only).
#[inline]
pub fn vocal_gen_id(track_id: u32, clip_id: u32) -> u64 {
    ((track_id as u64) << 32) | (clip_id as u64)
}

/// Hard cap on tracks the audio engine can render in a single buffer.
/// Picked to match `audio_bridge::MAX_TRACKS` so the per-track peak
/// meter doesn't fall off the GUI side.
pub const MAX_TRACKS: usize = 32;

/// Commands sent from the IPC receive loop to the audio thread. Pumped
/// at the top of every `process_buffer` so handles land before the
/// dispatch logic uses them.
pub enum AudioCommand {
    /// The plugin host stood up its worker pool. The audio side has now
    /// opened the matching `WorkerBridge` shmem and the per-worker
    /// (wake, done) named events; pass them in so the audio thread can
    /// dispatch via `WorkerSyncRef::dispatch`.
    OpenWorkerPool {
        bridge: WorkerBridgeHandle,
        worker_syncs: Vec<WorkerSyncRef>,
    },
    /// A new plugin instance was loaded; the audio engine has opened
    /// its `ProcessData` shmem and is ready to drive `plugin.process()`.
    /// `track` / `slot` let the engine slot the plugin into its routing
    /// graph at the matching position. `handle` keeps the daw_audio-side
    /// shmem mapping alive — without it, `plugin_ref.process_data` would
    /// be a dangling pointer once `handle_open_plugin_shmem` returns and
    /// drops its local `ProcessDataHandle`.
    OpenPluginShmem {
        plugin_id: u32,
        plugin_ref: PluginRef,
        handle: common::process_data::ProcessDataHandle,
        track: u32,
        slot: PluginSlot,
    },
    /// Drop a previously-opened plugin shmem mapping. Triggered on
    /// RemoveSlotPlugin / RemoveTrack from the GUI side.
    ClosePluginShmem { plugin_id: u32 },
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PlaybackCommand {
    Stop = 0,
    Play = 1,
}

impl PlaybackCommand {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Play,
            _ => Self::Stop,
        }
    }
}

/// State shared between the audio thread and the IPC receive loop /
/// future GUI commands. Every field is wait-free: the audio thread reads
/// these on every buffer; the IPC side writes them on each command.
pub struct SharedState {
    pub song: ArcSwapOption<Song>,
    pub playback: AtomicU8,
    pub looping: AtomicBool,
    /// Last published playhead in samples. Mirrored to shmem for the GUI
    /// playhead cursor.
    pub playhead: AtomicU64,
    /// Phase 4 Step C-2 (`docs/plan_automation.md` §6): currently recording
    /// lane set (= GUI が `SetRecordingLanes` で更新)。 audio thread は
    /// 各 buffer の頭で `load()` し、 `fill_track_param_ramps` で該当 lane
    /// の curve eval を bypass する。 `(track_id, AutomationTarget)` の
    /// 2 つ組で identify (lane_id を使わないのは GUI 側で lane を削除して
    /// から audio に通知が届くまでの race を避けるため = target 一致なら
    /// bypass で済む)。 起動時は空。
    pub recording_lanes:
        arc_swap::ArcSwap<std::collections::HashSet<(u32, common::model::AutomationTarget)>>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            song: ArcSwapOption::empty(),
            playback: AtomicU8::new(PlaybackCommand::Stop as u8),
            looping: AtomicBool::new(false),
            playhead: AtomicU64::new(0),
            recording_lanes: arc_swap::ArcSwap::from_pointee(
                std::collections::HashSet::new(),
            ),
        }
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

/// Engine resources shared between the CPAL audio thread and (in A3) an
/// offline-export worker. All fields are wait-free: the IPC apply path
/// (`pump_commands`) publishes new snapshots via `ArcSwap::store`, RT
/// readers `load()` an immutable snapshot for the duration of one
/// buffer.
///
/// Held by `LocalState::shared` (the CPAL closure) and — once A3 lands
/// — also by the export thread, so both can drive `plugin.process()`
/// without re-syncing per-buffer state.
pub struct EngineShared {
    /// Owned `WorkerBridge` shmem. Populated on `OpenWorkerPool`.
    pub worker_bridge: ArcSwapOption<WorkerBridgeHandle>,
    /// Per-worker handshake handles (one per audio-engine worker), in
    /// the same order the plugin host's `WorkerPool` opened them.
    pub worker_syncs: ArcSwap<Vec<WorkerSyncRef>>,
    /// `plugin_id` → `PluginRef` (process_data shmem ptr). New
    /// snapshot on every plugin load / unload.
    pub plugin_refs: ArcSwap<HashMap<u32, PluginRef>>,
    /// `(track, slot)` → `plugin_id`. New snapshot in lock-step with
    /// `plugin_refs`.
    pub slot_to_plugin_id: ArcSwap<HashMap<(u32, PluginSlot), u32>>,
    /// Worker pool that fans per-track work across N audio-engine
    /// workers. `None` until `OpenWorkerPool` arrives.
    pub worker_pool: ArcSwapOption<AudioWorkerPool>,
    /// Set by A3's export thread while it owns the audio path. CPAL
    /// callback skips its `process_buffer` and writes silence so the
    /// export render can drive `plugin.process()` exclusively.
    pub export_running: AtomicBool,
    /// Audio clip render snapshot. Built off-thread in
    /// `compile_audio_schedule` (PR6) and published via `ArcSwap`. The
    /// audio thread `load()`s once per buffer to find events that
    /// overlap the current playhead range. Empty until imports start
    /// landing.
    pub audio_clip_renderer: ArcSwap<AudioClipRenderer>,
    /// Current project directory, used to resolve
    /// `AudioSourcePath::ProjectRelative`. `None` for unsaved projects
    /// — `ProjectRelative` paths fail to resolve in that state and the
    /// caller is expected to use `Absolute` (import_cache fallback).
    /// Updated by `MainToChild::SetProjectDir`.
    pub project_dir: ArcSwapOption<PathBuf>,
}

impl EngineShared {
    pub fn new() -> Self {
        Self {
            worker_bridge: ArcSwapOption::empty(),
            worker_syncs: ArcSwap::from_pointee(Vec::new()),
            plugin_refs: ArcSwap::from_pointee(HashMap::new()),
            slot_to_plugin_id: ArcSwap::from_pointee(HashMap::new()),
            worker_pool: ArcSwapOption::empty(),
            export_running: AtomicBool::new(false),
            audio_clip_renderer: ArcSwap::from_pointee(AudioClipRenderer::empty()),
            project_dir: ArcSwapOption::empty(),
        }
    }
}

impl Default for EngineShared {
    fn default() -> Self {
        Self::new()
    }
}

/// Audio-thread-private engine state. Lives in the CPAL closure for the
/// whole stream lifetime.
pub struct LocalState {
    /// Pre-allocated scratch buffers (MAX_TRACKS entries). The audio
    /// loop indexes into this with the current Song's track index — no
    /// resize, no allocation in the RT path.
    pub scratch: Vec<TrackScratch>,
    pub master_l: Vec<f32>,
    pub master_r: Vec<f32>,
    /// Whether the transport was rolling on the previous buffer. Used to
    /// detect Play/Stop transitions and reset the playhead / queue
    /// note-offs cleanly.
    pub playing: bool,
    /// Phase 5 Step 5.2 (`docs/plan_automation.md` §10): accumulated
    /// beat-domain playhead。 audio thread が buffer 頭で
    /// `evaluate_song_tempo(song, playhead_beats)` を呼んで current_bpm を
    /// 引き、 buffer 末で `playhead_beats += frames * current_bpm / (60 * SR)`
    /// で advance する。 Play edge / SeekTo IPC では sample-domain playhead +
    /// 現 song.bpm で初期推定 (過去の tempo 履歴を再生できないので average
    /// 線形換算)、 user 体感での timing drift は小さい。
    pub playhead_beats: f64,
    /// Phase 5 Step 5.2: 前 buffer 末の sample-domain playhead。 次 buffer 頭
    /// で `shared.playhead != last_known_playhead` のとき seek が発生したと
    /// 判定し、 `playhead_beats` を再初期化する (= 過去の tempo 履歴を
    /// 再生できないので song.bpm で linear 推定)。 初期値 `u64::MAX` は
    /// 「未確定」 (= 最初の buffer は必ず seek 扱いで初期化される)。
    pub last_known_playhead: u64,
    /// Phase 5 follow-up (granular DSP click 抑制): LP smoothed tempo_ratio
    /// (= current_bpm / song.bpm)。 audio_clip_renderer の granular_sample_at
    /// で grain の source 内 offset を計算するときに使う。 instantaneous な
    /// tempo_ratio を直接使うと、 buffer 越しに tempo が変わったとき
    /// `grain_source_offset = k * HOP * tempo_ratio` が past grain でも更新
    /// されてしまい、 grain 中で source pos が discontinuous → click。
    /// LP smoothing で per-buffer の `Δtempo_ratio` を小さく抑え、 active
    /// grain (= life 2*HOP ~ 1024 samples ≒ 1 buffer @ 512) 内での source
    /// pos jump を低減する。 完全な stateful grain-trigger lock-in は別 phase
    /// (= per-event state 必要、 worker pool に &mut を通す refactor が要)。
    /// 初期値 1.0 (= nominal)、 LP coef は process_buffer で `~50ms TC` 相当。
    pub granular_tempo_smoothed: f64,
    /// Pending IPC commands from the receive loop. Drained at the top
    /// of every `process_buffer` so `EngineShared` snapshots are fresh
    /// before dispatch.
    pub cmd_rx: tokio::sync::mpsc::UnboundedReceiver<AudioCommand>,
    /// Resources shared with the (future) export thread.
    pub shared: Arc<EngineShared>,
    /// Cached routing schedule. Recompiled (heap alloc) only when
    /// `cached_song` is `Arc::ptr_eq`-different from the current song
    /// snapshot, i.e. on user edits — not on every audio buffer. PR3
    /// will move `compile_schedule` off the audio thread entirely and
    /// publish via `ArcSwap` for fully wait-free pickup.
    pub cached_schedule: Schedule,
    /// Last `Arc<Song>` we compiled the schedule from, kept alive so
    /// pointer equality is meaningful across buffers.
    pub cached_song: Option<Arc<Song>>,
    /// Debug-only: playhead at the last heartbeat log. Throttles
    /// `engine heartbeat` to once per second of audio time.
    #[cfg(debug_assertions)]
    pub last_heartbeat_playhead: u64,
    /// Debug-only: pre-allocated scratch for the heartbeat log so the RT
    /// path doesn't allocate when the throttle window opens. Cleared and
    /// re-extended on each emit; capacity is sized at construction.
    #[cfg(debug_assertions)]
    pub heartbeat_track_peaks: Vec<(f32, f32, bool)>,
    #[cfg(debug_assertions)]
    pub heartbeat_plugin_ids: Vec<u32>,
    #[cfg(debug_assertions)]
    pub heartbeat_slot_keys: Vec<((u32, PluginSlot), u32)>,
}

impl LocalState {
    pub fn new(
        max_frames: usize,
        cmd_rx: tokio::sync::mpsc::UnboundedReceiver<AudioCommand>,
        shared: Arc<EngineShared>,
    ) -> Self {
        let scratch = (0..MAX_TRACKS).map(|_| TrackScratch::new()).collect();
        Self {
            scratch,
            master_l: vec![0.0; max_frames],
            master_r: vec![0.0; max_frames],
            playing: false,
            playhead_beats: 0.0,
            last_known_playhead: u64::MAX,
            granular_tempo_smoothed: 1.0,
            cmd_rx,
            shared,
            cached_schedule: Schedule::empty(),
            cached_song: None,
            #[cfg(debug_assertions)]
            last_heartbeat_playhead: 0,
            #[cfg(debug_assertions)]
            heartbeat_track_peaks: Vec::with_capacity(MAX_TRACKS),
            // 上限は実態に合わせた hint。超えても Vec が伸びるだけだが、
            // steady-state で MAX_TRACKS * 4 slot を超えるケースは稀。
            #[cfg(debug_assertions)]
            heartbeat_plugin_ids: Vec::with_capacity(MAX_TRACKS * 4),
            #[cfg(debug_assertions)]
            heartbeat_slot_keys: Vec::with_capacity(MAX_TRACKS * 4),
        }
    }

    /// Refresh `cached_schedule` if the snapshot Arc changed since the
    /// last call. Heap allocation is concentrated here (called only on
    /// edit-time transitions) so the steady-state RT path stays free of
    /// `Vec` growth.
    fn refresh_schedule(&mut self, current_song: Option<&Arc<Song>>) {
        let need_refresh = match (&self.cached_song, current_song) {
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            (None, Some(_)) => true,
            _ => false,
        };
        if !need_refresh {
            return;
        }
        if let Some(song_arc) = current_song {
            match compile_schedule(song_arc.as_ref()) {
                Ok(s) => self.cached_schedule = s,
                Err(e) => {
                    // Routing is broken (cycle / dangling parent_group_id);
                    // fall back to the empty schedule (silent master) so
                    // the user sees the problem rather than mysterious audio.
                    tracing::warn!(?e, "graph compile failed; master goes silent");
                    self.cached_schedule = Schedule::empty();
                }
            }
            self.cached_song = Some(Arc::clone(song_arc));
            // PR4.5 sidechain plugin-internal alignment: ensure each
            // TrackScratch's input_delay_line has enough capacity for the
            // newly compiled schedule. Reallocates only when capacity needs
            // to grow (and only at edit-time, never in the RT buffer
            // dispatch). DelayLine.step_in_place clamps `delay >= cap`
            // requests to `cap - 1`, so capacity must be `delay + 1`.
            for (i, &delay) in self
                .cached_schedule
                .input_delay_per_track
                .iter()
                .enumerate()
            {
                if i >= self.scratch.len() {
                    break;
                }
                let need_cap = delay as usize + 1;
                if delay > 0 && self.scratch[i].input_delay_line.capacity() < need_cap {
                    self.scratch[i].input_delay_line =
                        crate::graph::DelayLine::with_capacity(need_cap);
                }
            }
        }
    }

    /// Drain pending IPC commands. Called at the top of `process_buffer`.
    /// Each command publishes a fresh snapshot into `EngineShared` via
    /// `ArcSwap::store`, so RT readers see the new state on this very
    /// buffer. Allocations only happen here when the daw_gui side
    /// mutates the plugin graph (plugin add/remove) — outside the
    /// steady-state RT path.
    fn pump_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                AudioCommand::OpenWorkerPool {
                    bridge,
                    worker_syncs,
                } => {
                    let n = worker_syncs.len() as u32;
                    self.shared.worker_bridge.store(Some(Arc::new(bridge)));
                    self.shared.worker_syncs.store(Arc::new(worker_syncs));
                    // Spawn the audio-engine worker pool to fan
                    // per-track work out 1:1 against the plugin host.
                    match AudioWorkerPool::new(n) {
                        Ok(pool) => {
                            self.shared.worker_pool.store(Some(Arc::new(pool)));
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, "AudioWorkerPool::new failed");
                            self.shared.worker_pool.store(None);
                        }
                    }
                    tracing::info!(
                        n_workers = self.shared.worker_syncs.load().len(),
                        "audio engine bound to plugin-host worker pool"
                    );
                }
                AudioCommand::OpenPluginShmem {
                    plugin_id,
                    plugin_ref,
                    handle,
                    track,
                    slot,
                } => {
                    // Snapshot-copy-mutate-publish so RT readers either
                    // see the old map or the fully-populated new one,
                    // never a partial state.
                    let mut new_refs: HashMap<u32, PluginRef> =
                        (**self.shared.plugin_refs.load()).clone();
                    let mut new_slot: HashMap<(u32, PluginSlot), u32> =
                        (**self.shared.slot_to_plugin_id.load()).clone();
                    if let Some(stale) = new_slot.insert((track, slot), plugin_id) {
                        new_refs.remove(&stale);
                    }
                    new_refs.insert(plugin_id, plugin_ref);
                    self.shared.plugin_refs.store(Arc::new(new_refs));
                    self.shared.slot_to_plugin_id.store(Arc::new(new_slot));
                    // The `handle` keeps the daw_audio-side shmem
                    // mapping alive for the life of the AudioCommand;
                    // immediately leak it to extend that lifetime to the
                    // end of the process. ClosePluginShmem doesn't
                    // currently reclaim the memory — see
                    // `handle_open_plugin_shmem` for the original
                    // leak-on-open pattern this mirrors.
                    let leaked = Box::leak(Box::new(handle));
                    let _ = leaked;
                    tracing::info!(plugin_id, track, ?slot, "plugin shmem registered");
                }
                AudioCommand::ClosePluginShmem { plugin_id } => {
                    let mut new_refs: HashMap<u32, PluginRef> =
                        (**self.shared.plugin_refs.load()).clone();
                    let cur_slot = self.shared.slot_to_plugin_id.load();
                    // Find the (track_id, slot) that pointed at this
                    // plugin_id BEFORE we remove the entry, so we can
                    // shift remaining same-kind slots in the same
                    // track downwards (mirrors the Vec::remove that
                    // daw_gui / daw_plugin_host did on their fx_chain
                    // / midi_fx_chain). Without this shift, removing
                    // Fx(0) leaves Fx(1) stranded under its old key,
                    // and `process_track_owned` looks up Fx(0) → no
                    // hit → silent (group fx dropped on deletion).
                    let removed_key = cur_slot
                        .iter()
                        .find_map(|(k, v)| if *v == plugin_id { Some(*k) } else { None });
                    let mut new_slot: HashMap<(u32, PluginSlot), u32> = (**cur_slot).clone();
                    new_slot.retain(|_, pid| *pid != plugin_id);
                    if let Some((track_id, removed_slot)) = removed_key {
                        let entries: Vec<((u32, PluginSlot), u32)> = new_slot.drain().collect();
                        for ((tid, slot), pid) in entries {
                            let new_slot_kind = if tid == track_id {
                                match (removed_slot, slot) {
                                    (PluginSlot::Fx(removed_i), PluginSlot::Fx(i))
                                        if i > removed_i =>
                                    {
                                        PluginSlot::Fx(i - 1)
                                    }
                                    (PluginSlot::MidiFx(removed_i), PluginSlot::MidiFx(i))
                                        if i > removed_i =>
                                    {
                                        PluginSlot::MidiFx(i - 1)
                                    }
                                    (_, other) => other,
                                }
                            } else {
                                slot
                            };
                            new_slot.insert((tid, new_slot_kind), pid);
                        }
                    }
                    new_refs.remove(&plugin_id);
                    self.shared.plugin_refs.store(Arc::new(new_refs));
                    self.shared.slot_to_plugin_id.store(Arc::new(new_slot));
                    tracing::info!(plugin_id, "plugin shmem dropped + slot shifted");
                }
            }
        }
    }

    /// Render `frames` of master output into `master_l/r`. Walks the
    /// current `Song`, dispatching every plugin in every track's chain
    /// via the worker pool. Also publishes per-track peak meter values
    /// into the shared `AudioBridge` so the GUI mixer strips animate.
    pub fn process_buffer(
        &mut self,
        shared: &SharedState,
        bridge: &AudioBridgeHandle,
        sample_rate: u32,
        frames: usize,
    ) {
        self.pump_commands();

        // Refresh the cached routing schedule before the dispatch starts
        // so the master mix step sees the right node order. `refresh_schedule`
        // is a no-op when the song Arc hasn't changed.
        let song_snapshot = shared.song.load();
        self.refresh_schedule(song_snapshot.as_ref());

        let n = frames;
        self.master_l[..n].fill(0.0);
        self.master_r[..n].fill(0.0);

        // A3 freewheel: while the export thread holds the audio
        // resources, write silence and skip dispatch so the worker pool
        // and plugin instances are exclusively driven by the export
        // render loop.
        if self.shared.export_running.load(Ordering::Acquire) {
            return;
        }

        // Play / Stop edge handling. On Play, restart playhead and clear
        // active notes. On Stop, queue offs at frame 0 of the next buffer
        // so plugins drain cleanly.
        let desired = PlaybackCommand::from_u8(shared.playback.load(Ordering::Acquire));
        match (self.playing, desired) {
            (false, PlaybackCommand::Play) => {
                self.playing = true;
                // 旧挙動: Play で必ず playhead を 0 に戻す。 これは
                // 「ruler click で位置を変えても Play 押すと頭から
                // 再生される」 という不便さの原因。 業界標準 (REAPER /
                // Ableton / Studio One) は「Play は現在の playhead
                // から再生」、 ホームに戻すのは別 shortcut (= Home キー
                // 等)。 daw_01 もそれに合わせ、 SeekTo IPC で書き込ま
                // れた現在の playhead をそのまま使う。
                for s in self.scratch.iter_mut() {
                    s.state.active_notes.clear();
                    s.state.pending_offs.clear();
                }
            }
            (true, PlaybackCommand::Stop) => {
                self.playing = false;
                for s in self.scratch.iter_mut() {
                    for &k in &s.state.active_notes {
                        s.state.pending_offs.push(k);
                    }
                    s.state.active_notes.clear();
                }
            }
            _ => {}
        }
        let playing = self.playing;

        // Reuse the snapshot we took for `refresh_schedule` so the
        // dispatch sees the same song the schedule was compiled from.
        let song_ref = song_snapshot.as_deref();
        let playhead = shared.playhead.load(Ordering::Acquire);

        // Phase 4 Step C-2: 「現在 recording 中の lane」 を SharedState から
        // 1 buffer 分の lifetime で借りる。 dispatch / process_track_owned /
        // run_group_fx_chain に &HashSet で渡す。 `_recording_lanes_g` の
        // Guard が生きている間は Arc が drop されないので audio thread から
        // 安全に deref できる。
        let recording_lanes_g = shared.recording_lanes.load();
        let recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)> =
            &recording_lanes_g;

        // Phase 5 Step 5.2: seek 検出 + playhead_beats 同期。 前 buffer 末で
        // 記録した `last_known_playhead` と current playhead を比較し、 一致
        // していなければ (= IPC SeekTo / Play edge / loop wrap / 起動直後)
        // 過去の tempo 履歴を再生できないので、 song.bpm を constant とした
        // linear 推定で playhead_beats を再初期化する。
        if playhead != self.last_known_playhead {
            let song_bpm =
                song_ref.map(|s| s.bpm.max(1.0)).unwrap_or(120.0) as f64;
            let sr = sample_rate as f64;
            self.playhead_beats = if sr > 0.0 {
                playhead as f64 * song_bpm / (60.0 * sr)
            } else {
                0.0
            };
        }
        // 今 buffer の effective bpm を SongTempo lane から評価する。
        // song = None なら 120.0 default、 SongTempo lane 無しなら song.bpm。
        // 当該 buffer 内では tempo 定数として扱う (= sub-buffer の tempo
        // change は scope 外、 1 buffer = ~5..20ms なので user 体感には
        // 影響なし)。
        //
        // Phase 5 Step 5.2 follow-up (2026-05-13): SongTempo lane が
        // recording 中 (= GUI 側で transport BPM input が gesture begin、
        // `MainToChild::SetRecordingLanes` で set に追加) なら curve eval を
        // **skip** し、 `song.bpm` constant fallback を維持する。 これで
        // transport BPM input drag が即時に audio に反映され、 直前 frame
        // に curve へ記録された point との二重反映 (= 階段状カクつき / 微小
        // ズレ) を防ぐ。 mixer fader の Volume / Pan と同 idiom
        // (`fill_track_param_ramps` の `recording_lanes.contains(...)` 分岐
        // 参照)。 set は 1 buffer の lifetime で borrow 済みなので RT 安全。
        let tempo_recording = recording_lanes.contains(&(
            common::model::MASTER_TRACK_ID,
            common::model::AutomationTarget::SongTempo,
        ));
        let current_bpm: f32 = match song_ref {
            Some(s) if tempo_recording => s.bpm,
            Some(s) => common::automation::evaluate_song_tempo(s, self.playhead_beats),
            None => 120.0,
        };

        // Phase 5 follow-up (granular DSP click 抑制): tempo_ratio (= current /
        // nominal) を LP smoothing して granular_sample_at に渡す。 nominal =
        // song.bpm (= compile 時に event.nominal_bpm にコピーされる shared
        // 値)。 song = None なら ratio = 1.0 (= 安全な nominal)。 LP coef は
        // ~50ms time constant 相当の固定値 (buffer = 11.6 ms @ 44100/512 で
        // coef ~ 0.3)、 完全な per-event grain-trigger lock-in 無しでも
        // 一般的な tempo curve では click が顕著に低減する。
        let target_granular_ratio = song_ref
            .map(|s| f64::from(current_bpm) / f64::from(s.bpm.max(1.0)))
            .unwrap_or(1.0);
        // Play edge 検出 (= last_known_playhead != playhead で seek) と同 frame
        // で smoothed を target に snap-reset し、 旧 tempo 履歴を持ち越さない。
        // これで Stop → 別位置から Play した直後でも granular が新 tempo に
        // 即座に追随する (= LP lag を抑える)。
        if playhead != self.last_known_playhead {
            self.granular_tempo_smoothed = target_granular_ratio;
        } else {
            const GRANULAR_LP_COEF: f64 = 0.3;
            self.granular_tempo_smoothed +=
                GRANULAR_LP_COEF * (target_granular_ratio - self.granular_tempo_smoothed);
        }

        if let Some(song) = song_ref {
            let any_solo = song.tracks.iter().any(|t| t.solo);
            let n_tracks = song.tracks.len().min(MAX_TRACKS);

            // Snapshot the wait-free shared state once for this buffer.
            // Guards stay live until the end of the call so the workers
            // can safely deref them via the publish pointers.
            let plugin_refs_g = self.shared.plugin_refs.load();
            let slot_map_g = self.shared.slot_to_plugin_id.load();
            let worker_syncs_g = self.shared.worker_syncs.load();
            let pool_g = self.shared.worker_pool.load();
            // PR6: audio clip renderer snapshot for this buffer.
            let audio_renderer_g = self.shared.audio_clip_renderer.load();
            let audio_renderer: &AudioClipRenderer = &audio_renderer_g;

            // Fan the per-track work out across the audio worker pool
            // when one is bound; otherwise fall back to serial dispatch
            // through `worker_syncs[0]` (still correct, just slower).
            if let Some(pool) = pool_g.as_deref() {
                pool.dispatch_and_wait(
                    Some(song),
                    &mut self.scratch[..n_tracks],
                    &plugin_refs_g,
                    &slot_map_g,
                    audio_renderer,
                    &worker_syncs_g,
                    &mut self.master_l[..n],
                    &mut self.master_r[..n],
                    sample_rate,
                    playhead,
                    n as u32,
                    playing,
                    any_solo,
                    &self.cached_schedule.input_delay_per_track,
                    recording_lanes,
                    current_bpm,
                    self.playhead_beats,
                    self.granular_tempo_smoothed,
                );
            } else {
                let worker_sync = worker_syncs_g.first();
                for track_idx in 0..n_tracks {
                    let song_track = &song.tracks[track_idx];
                    let scratch = &mut self.scratch[track_idx];
                    let input_delay = self
                        .cached_schedule
                        .input_delay_per_track
                        .get(track_idx)
                        .copied()
                        .unwrap_or(0);
                    process_track_owned(
                        track_idx as u32,
                        song_track,
                        scratch,
                        &plugin_refs_g,
                        &slot_map_g,
                        Some(audio_renderer),
                        worker_sync,
                        sample_rate,
                        playhead,
                        n as u32,
                        playing,
                        Some(song),
                        any_solo,
                        input_delay,
                        recording_lanes,
                        current_bpm,
                        self.playhead_beats,
                        self.granular_tempo_smoothed,
                    );
                }
            }

            // Walk the cached schedule to (a) mix children → group
            // scratches, (b) run each group's audio fx + strip, (c) sum
            // top-level scratches into the master bus. The legacy
            // `reduce_master` call is replaced by this graph-driven
            // execution so groups + future PDC + sidechain hops can plug
            // in by extending `NodeOp` rather than this function.
            execute_schedule_post_dispatch(
                &mut self.cached_schedule,
                &mut self.scratch[..MAX_TRACKS],
                &mut self.master_l[..n],
                &mut self.master_r[..n],
                n,
                song,
                &plugin_refs_g,
                &slot_map_g,
                worker_syncs_g.first(),
                sample_rate,
                n as u32,
                playing,
                any_solo,
                playhead,
                recording_lanes,
                current_bpm,
            );

            // Publish per-track peak meters into the shared AudioBridge
            // so the GUI mixer strips animate. Atomic stores, RT-safe.
            // Tracks with effective_mute already have peak_l/r == 0.
            for (i, tr) in self.scratch.iter().take(n_tracks).enumerate() {
                bridge.set_track_peak(i, tr.peak_l, tr.peak_r);
            }

            // Debug heartbeat: once per second of audio time, dump the
            // engine's view of the world so we can tell whether the
            // dispatch reached plugin.process(), what came back, and
            // why master might be silent.
            // Debug-only heartbeat. RT 規約上 audio thread での tracing は
            // 望ましくないが、開発時に engine 状態を可視化できる利点が
            // 大きいので debug ビルド限定で残す。release では消える。
            // pre-allocated buffer (`heartbeat_*`) を `clear()+extend()` で
            // 再利用するので heap alloc は (capacity 内なら) 発生しない。
            #[cfg(debug_assertions)]
            {
                let sr = sample_rate as u64;
                if sr > 0
                    && playhead / sr != self.last_heartbeat_playhead / sr
                {
                    self.last_heartbeat_playhead = playhead;
                    let master_peak = self.master_l[..n]
                        .iter()
                        .chain(self.master_r[..n].iter())
                        .fold(0.0_f32, |a, &b| a.max(b.abs()));
                    self.heartbeat_track_peaks.clear();
                    self.heartbeat_track_peaks.extend(
                        self.scratch
                            .iter()
                            .take(n_tracks)
                            .map(|s| (s.peak_l, s.peak_r, s.effective_mute)),
                    );
                    self.heartbeat_plugin_ids.clear();
                    self.heartbeat_plugin_ids
                        .extend(plugin_refs_g.keys().copied());
                    self.heartbeat_slot_keys.clear();
                    self.heartbeat_slot_keys
                        .extend(slot_map_g.iter().map(|(k, v)| (*k, *v)));
                    tracing::info!(
                        playing,
                        playhead,
                        master_peak,
                        track_peaks = ?self.heartbeat_track_peaks,
                        plugin_ids = ?self.heartbeat_plugin_ids,
                        slot_keys = ?self.heartbeat_slot_keys,
                        n_workers = worker_syncs_g.len(),
                        worker_pool = pool_g.is_some(),
                        audio_clip_n_events = audio_renderer.schedule.len(),
                        audio_clip_n_sources = audio_renderer.sources.len(),
                        "engine heartbeat"
                    );
                }
            }
        }

        // Playhead advance + auto-stop / loop wrap.
        if playing {
            let mut new_ph = playhead + n as u64;
            let active_end = if shared.looping.load(Ordering::Acquire) {
                effective_loop_bounds(song_ref, sample_rate).map(|(_, e)| e)
            } else {
                None
            };
            let reached_end = if let Some(end) = active_end {
                new_ph >= end
            } else {
                song_ended(song_ref, sample_rate, new_ph)
            };
            // Phase 5 Step 5.2: playhead_beats を current_bpm で 1 buffer 分
            // advance する。 sub-buffer の tempo 変化は scope 外 (= 1 buffer
            // ~5..20ms 内 constant)。
            let sr = sample_rate as f64;
            if sr > 0.0 {
                self.playhead_beats +=
                    n as f64 * f64::from(current_bpm) / (60.0 * sr);
            }
            if reached_end {
                for s in self.scratch.iter_mut() {
                    for &k in &s.state.active_notes {
                        s.state.pending_offs.push(k);
                    }
                    s.state.active_notes.clear();
                }
                let wrap_to = if shared.looping.load(Ordering::Acquire) {
                    effective_loop_bounds(song_ref, sample_rate).map(|(s, _)| s)
                } else {
                    None
                };
                if let Some(start) = wrap_to {
                    new_ph = start;
                    // Phase 5 Step 5.2 bug fix: loop wrap 時に playhead_beats
                    // を sample-domain new_ph に合わせて再計算する (= 上の
                    // `+=` で advance させた値は loop end 直後の beat 位置
                    // であって、 loop start に rewind されない)。 次 buffer
                    // の seek 検知では `playhead == last_known_playhead` で
                    // 反応しないため、 ここで直接 reset する必要がある。
                    // current_bpm を constant とした linear 推定で OK (= tempo
                    // automation 中の loop boundary は MVP scope 外で、 通常
                    // の constant tempo loop なら精度問題なし)。
                    if sr > 0.0 {
                        self.playhead_beats =
                            new_ph as f64 * f64::from(current_bpm) / (60.0 * sr);
                    }
                } else {
                    self.playing = false;
                    shared
                        .playback
                        .store(PlaybackCommand::Stop as u8, Ordering::Release);
                }
            }
            shared.playhead.store(new_ph, Ordering::Release);
            self.last_known_playhead = new_ph;
        } else {
            // Stop 中は audio thread が playhead を進めないが、 GUI からの
            // SeekTo IPC は shared.playhead を書き換える可能性がある。
            // last_known_playhead を current 値で同期して、 次 Play 開始
            // 時の seek 検出ロジックを誤発火させない (= stop 中の seek は
            // 単に位置を変えるだけで playhead_beats 再計算が必要)。
            self.last_known_playhead = playhead;
        }
    }
}

/// Phase 5 Step 5.3 (`docs/plan_automation.md` §10): populate the
/// transport fields on `ProcessData` from the current `Song` so the
/// plugin host can build a `clap_event_transport` for each
/// `plugin.process()` call. `song = None` (engine init / no song
/// loaded) leaves the default constants set by `ProcessData::empty()`
/// (120 BPM / 4/4 / no loop).
/// Phase 5 Step 5.2: `effective_bpm` is the SongTempo lane evaluated
/// at the buffer-start beat (= what the plugin sees as `clap_event_transport
/// .tempo`)。 引数で受け取るのは song-domain の `song.bpm` (= constant
/// base BPM) と区別するため。
pub fn set_pd_transport(
    pd: &mut common::process_data::ProcessData,
    song: Option<&Song>,
    effective_bpm: f32,
) {
    let Some(song) = song else { return };
    pd.bpm = effective_bpm.max(1.0);
    pd.tsig_num = song.time_sig.0 as u16;
    pd.tsig_denom = song.time_sig.1 as u16;
    pd.loop_start_beats = song.loop_start_beat;
    pd.loop_end_beats = song.loop_end_beat;
    // Loop は user の loop button 状態を別 IPC で渡したいが、 当面は
    // 「loop region が定義済」 (= end > start) を heuristic で IS_LOOPING
    // とする。 Bitwig / Live も同じ (= region 定義時のみ loop active)。
    pd.looping = if song.loop_end_beat > song.loop_start_beat { 1 } else { 0 };
}

/// Render one track's contribution into its `TrackScratch`. Walks the
/// MIDI FX → instrument (or Vocal) → audio FX chain, dispatches every
/// plugin via the assigned worker pair, then applies the mixer strip
/// (equal-power pan + volume + mute/solo). The post-fader audio ends
/// up in `scratch.track_l/r` along with the peak meter info.
///
/// Master accumulation into the bus happens **outside** this function
/// (`reduce_master`) so concurrent workers never race on the same
/// `master_{l,r}[i]`.
///
/// `worker_sync` may be `None` if `OpenWorkerPool` hasn't arrived yet
/// — in that case plugin chains are skipped entirely (silent track).
///
/// `input_delay_samples`: PR4.5 sidechain plugin-internal alignment. If
/// non-zero, the track's main signal (vocal / instrument output) is
/// delayed by that many samples **before** the audio FX chain runs.
/// The caller (engine main loop / export) passes
/// `Schedule::input_delay_per_track[track_idx]`, which compile_schedule
/// has set to `max(path_latency(src) for src in fx_chain[*].sidechain_sources)`.
/// 0 = no delay (the common case).
#[allow(clippy::too_many_arguments)]
pub fn process_track_owned(
    track_idx: u32,
    song_track: &Track,
    scratch: &mut TrackScratch,
    plugin_refs: &HashMap<u32, PluginRef>,
    slot_to_plugin_id: &HashMap<(u32, PluginSlot), u32>,
    audio_renderer: Option<&AudioClipRenderer>,
    worker_sync: Option<&WorkerSyncRef>,
    sample_rate: u32,
    playhead: u64,
    frames: u32,
    playing: bool,
    song: Option<&Song>,
    any_solo: bool,
    input_delay_samples: u32,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    // Phase 5 Step 5.2: 当該 buffer の effective bpm (= SongTempo lane 評価
    // or song.bpm fallback)。 set_pd_transport / fill_track_param_ramps /
    // fill_pd_param_events の sample-to-beat 変換に使う。
    current_bpm: f32,
    // Phase 5 follow-up (MIDI tempo follow): buffer 開始時の累積 beat-domain
    // playhead。 collect_events_for_buffer に渡して beat-domain で note 配置
    // を判定する。 変動 tempo でも note 位置が正しく追随する。
    playhead_beats: f64,
    // Phase 5 follow-up (granular DSP click 抑制): LP smoothed tempo_ratio
    // (= current_bpm / song.bpm)。 audio_clip_renderer::render_audio_events
    // に渡して、 Stretch mode の granular sampler が source 内 offset を
    // 計算するときの ratio として使う。 LocalState 側で per-buffer に
    // 1-pole LP で更新される値。
    granular_tempo_smoothed: f64,
) {
    let n = frames as usize;

    // Tracks that have children (i.e. behave as a "group" / folder)
    // are handled by the post-dispatch schedule walk: the children's
    // outputs are mixed into this track's scratch by a `Mix` op, then
    // `ProcessGroupFx` applies the audio fx_chain and strip. Skip the
    // sequencer / midi_fx / instrument stages here so the dispatch
    // doesn't smear plugin output into a buffer the schedule is about
    // to overwrite. PR2 phase 1 keeps "group ignores its own clips /
    // instrument" semantics; phase 5 will switch to Reaper's folder
    // model where the group's own clips also feed the post-fx mix.
    let has_children = song
        .map(|s| s.tracks.iter().any(|t| t.parent_group_id == Some(song_track.id)))
        .unwrap_or(false);
    if has_children {
        scratch.track_l[..n].fill(0.0);
        scratch.track_r[..n].fill(0.0);
        scratch.peak_l = 0.0;
        scratch.peak_r = 0.0;
        scratch.effective_mute = false;
        return;
    }

    // ---- Sequencer: assemble this buffer's MIDI bus ----
    scratch.midi_bus_a.clear();
    for &k in &scratch.state.pending_offs {
        // pending_offs は stuck note flush 用なので note_id 不明 → 0
        // (= "未指定" 相当)。 builtin plugin は voice cleanup で key 一致
        // で停止するので、 note_id 0 でも実害なし。
        scratch.midi_bus_a.push(TimedNoteEvent {
            time: 0,
            event: NoteTransition::Off { note_id: 0, key: k },
        });
    }
    scratch.state.pending_offs.clear();
    if playing {
        collect_events_for_buffer(
            song,
            track_idx,
            sample_rate,
            playhead_beats,
            current_bpm,
            frames,
            &mut scratch.midi_bus_a,
            &mut scratch.state.active_notes,
        );
    }

    let track_id = song_track.id;
    // ---- MIDI FX chain ----
    for i in 0..song_track.midi_fx_chain.len() {
        // PR2.1: chains map の key は (track_id, slot)。 song.tracks の
        // Vec position に依存しないので、 group 化や drag&drop reorder
        // で index が shift しても plugin lookup が壊れない。
        let key = (track_id, PluginSlot::MidiFx(i as u32));
        let Some(&plugin_id) = slot_to_plugin_id.get(&key) else {
            continue;
        };
        let Some(plugin_ref) = plugin_refs.get(&plugin_id) else {
            continue;
        };
        let Some(ws) = worker_sync else { continue };

        let pd = plugin_ref.data_mut();
        pd.prepare();
        pd.frames = frames;
        pd.playing = if playing { 1 } else { 0 };
        pd.sample_rate = sample_rate;
        set_pd_transport(pd, song, current_bpm);
        // Phase 2b: MIDI FX 宛 PluginParam automation を ParamValue event 化。
        if let Some(song) = song {
            crate::automation::fill_pd_param_events(
                pd,
                song,
                track_id,
                PluginSlot::MidiFx(i as u32),
                sample_rate,
                song.bpm,
                playhead,
                frames,
            );
        }
        for ev in &scratch.midi_bus_a {
            match ev.event {
                NoteTransition::On { note_id, key, velocity } => {
                    pd.push_note_on(ev.time, key, velocity, 0, note_id)
                }
                NoteTransition::Off { note_id, key } => {
                    pd.push_note_off(ev.time, key, 0, note_id)
                }
            }
        }
        if let Err(e) = ws.dispatch(plugin_id) {
            tracing::error!(error = ?e, plugin_id, "midi_fx dispatch failed");
            continue;
        }
        // Drain plugin's output events into midi_bus_b.
        scratch.midi_bus_b.clear();
        let n_out = pd.n_events_out as usize;
        for ev in &pd.events_out[..n_out.min(pd.events_out.len())] {
            let timed = match ev.kind {
                EventKind::NoteOn => TimedNoteEvent {
                    time: ev.time,
                    event: NoteTransition::On {
                        note_id: ev.note_id,
                        key: ev.key,
                        velocity: ev.velocity,
                    },
                },
                EventKind::NoteOff => TimedNoteEvent {
                    time: ev.time,
                    event: NoteTransition::Off {
                        note_id: ev.note_id,
                        key: ev.key,
                    },
                },
                EventKind::ParamValue => continue,
            };
            scratch.midi_bus_b.push(timed);
        }
        scratch.midi_bus_b.sort_unstable_by_key(|e| e.time);
        std::mem::swap(&mut scratch.midi_bus_a, &mut scratch.midi_bus_b);
    }

    // ---- Track audio output (cleared every buffer) ----
    scratch.track_l[..n].fill(0.0);
    scratch.track_r[..n].fill(0.0);

    // PR-V4: 旧 VOICEVOX 専用 vocal block を削除。 vocal track は
    // `track.instrument` に builtin VOICEVOX plugin が居るので、 通常の
    // Instrument 段階 (= 下記) で処理される。 daw_gui の migration
    // (`migrate_legacy_vocal_tracks`) が project load 時に旧 vocal
    // tracks を builtin path に移行する。

    // ---- Instrument ----
    if song_track.instrument.is_some() {
        let key = (track_id, PluginSlot::Instrument);
        if let Some(&plugin_id) = slot_to_plugin_id.get(&key)
            && let Some(plugin_ref) = plugin_refs.get(&plugin_id)
            && let Some(ws) = worker_sync
        {
            let pd = plugin_ref.data_mut();
            pd.prepare();
            pd.frames = frames;
            pd.playing = if playing { 1 } else { 0 };
            pd.sample_rate = sample_rate;
            // Phase 2b: automation lane の PluginParam target で
            // Instrument 宛のものを ParamValue event として push。
            if let Some(song) = song {
                crate::automation::fill_pd_param_events(
                    pd,
                    song,
                    track_id,
                    PluginSlot::Instrument,
                    sample_rate,
                    song.bpm,
                    playhead,
                    frames,
                );
            }
            for ev in &scratch.midi_bus_a {
                match ev.event {
                    NoteTransition::On { note_id, key, velocity } => {
                        pd.push_note_on(ev.time, key, velocity, 0, note_id)
                    }
                    NoteTransition::Off { note_id, key } => {
                        pd.push_note_off(ev.time, key, 0, note_id)
                    }
                }
            }
            if let Err(e) = ws.dispatch(plugin_id) {
                tracing::error!(error = ?e, plugin_id, "instrument dispatch failed");
            } else {
                scratch.track_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
                scratch.track_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
            }
        }
    }

    // ---- Phase 1 PR6: audio clip events ----
    // Bitwig Hybrid Track 流: audio clip 出力は instrument 出力に
    // **加算** されてから fx chain を通る (`docs/plan_audio_clip.md`
    // §13 Q6 / §6.1)。 1 track 内で MIDI clip と Audio clip が混在
    // しても、 audio events は instrument を bypass して effect chain
    // の入口でそのまま合流する。
    //
    // playing == false (Stop / 一時停止) では audio clip を mix しない
    // (= 旧バグ: Stop でも render し続けてブーンと鳴り続けた)。
    if playing && let Some(renderer) = audio_renderer {
        crate::audio_clip_renderer::render_audio_events(
            renderer,
            track_idx as usize,
            &mut scratch.track_l[..n],
            &mut scratch.track_r[..n],
            playhead_beats,
            current_bpm,
            sample_rate,
            frames,
            granular_tempo_smoothed,
        );
    }

    // ---- PR4.5 sidechain plugin-internal alignment ------------------
    // Delay the track's main signal so it lines up musically with any
    // sidechain `aux_in` the fx_chain plugins read from `pd.buffer_aux_in`.
    // Without this delay, a compressor's main_in arrives at musical time t
    // but its aux_in (= source.scratch tapped via SidechainTap) is at
    // musical time t - source.path_latency, so the compressor's gain
    // reduction lags the trigger by source.path_latency. The DelayLine's
    // capacity was sized in `Engine::refresh_schedule` (only ever grows on
    // edit-time, never in the RT path).
    if input_delay_samples > 0 {
        scratch
            .input_delay_line
            .step_in_place(&mut scratch.track_l[..n], &mut scratch.track_r[..n], input_delay_samples as usize);
    }

    // ---- Audio FX chain ----
    for i in 0..song_track.fx_chain.len() {
        let key = (track_id, PluginSlot::Fx(i as u32));
        let Some(&plugin_id) = slot_to_plugin_id.get(&key) else {
            continue;
        };
        let Some(plugin_ref) = plugin_refs.get(&plugin_id) else {
            continue;
        };
        let Some(ws) = worker_sync else { continue };

        let pd = plugin_ref.data_mut();
        pd.prepare();
        pd.frames = frames;
        pd.playing = if playing { 1 } else { 0 };
        pd.sample_rate = sample_rate;
        set_pd_transport(pd, song, current_bpm);
        // Phase 2b: automation の PluginParam target を ParamValue event 化。
        if let Some(song) = song {
            crate::automation::fill_pd_param_events(
                pd,
                song,
                track_id,
                PluginSlot::Fx(i as u32),
                sample_rate,
                song.bpm,
                playhead,
                frames,
            );
        }
        pd.buffer_in[0][..n].copy_from_slice(&scratch.track_l[..n]);
        pd.buffer_in[1][..n].copy_from_slice(&scratch.track_r[..n]);
        if let Err(e) = ws.dispatch(plugin_id) {
            tracing::error!(error = ?e, plugin_id, "fx dispatch failed");
            continue;
        }
        scratch.track_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
        scratch.track_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
    }

    // ---- Mixer strip + master accumulate ----
    let muted = song_track.muted;
    let solo = song_track.solo;
    let effective_mute = muted || (any_solo && !solo);
    scratch.effective_mute = effective_mute;

    if effective_mute {
        // Zero the track output too so a stale buffer from before the
        // user pressed mute doesn't leak into the master bus.
        scratch.track_l[..n].fill(0.0);
        scratch.track_r[..n].fill(0.0);
        scratch.peak_l = 0.0;
        scratch.peak_r = 0.0;
    } else {
        // Fill the per-sample volume / pan ramps. With no automation
        // lane this fills both buffers with the constant track strip
        // values; with a `Volume` / `Pan` lane each sample gets the
        // curve value. RT-safe: in-place writes only.
        // Phase 5 Step 5.2: master が当該 buffer の effective bpm を
        // current_bpm として渡してくる。 song.bpm fallback はもはや不要
        // (= song = None なら process_buffer 側で current_bpm = 120.0)。
        crate::automation::fill_track_param_ramps(
            song,
            track_idx,
            sample_rate,
            current_bpm,
            playhead,
            frames,
            &mut scratch.volume_per_sample,
            &mut scratch.pan_per_sample,
            recording_lanes,
        );
        let mut peak_l = 0.0_f32;
        let mut peak_r = 0.0_f32;
        // Apply the strip per-sample so volume / pan automation lands
        // sample-accurately. No global write — race-free.
        for i in 0..n {
            let pan = scratch.pan_per_sample[i].clamp(-1.0, 1.0);
            let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
            let vol = scratch.volume_per_sample[i];
            let gain_l = angle.cos() * vol;
            let gain_r = angle.sin() * vol;
            let l = scratch.track_l[i] * gain_l;
            let r = scratch.track_r[i] * gain_r;
            scratch.track_l[i] = l;
            scratch.track_r[i] = r;
            if l.abs() > peak_l {
                peak_l = l.abs();
            }
            if r.abs() > peak_r {
                peak_r = r.abs();
            }
        }
        scratch.peak_l = peak_l;
        scratch.peak_r = peak_r;
    }
}

/// Sum every non-muted track's `track_l/r` into `master_l/r`. Runs on
/// the master thread after `dispatch_and_wait` returns, so writers no
/// longer touch `master_{l,r}`. Sequential, but cache-friendly: each
/// scratch is read straight through.
///
/// Used by `export.rs` (the offline freewheel render) which still walks
/// tracks flatly. The realtime engine now goes through
/// `execute_schedule_post_dispatch` so groups + PDC + sidechain hops can
/// plug in by extending `NodeOp`.
pub fn reduce_master(
    scratch: &[TrackScratch],
    n_tracks: usize,
    master_l: &mut [f32],
    master_r: &mut [f32],
    frames: usize,
) {
    let n = frames.min(master_l.len()).min(master_r.len());
    for tr in scratch.iter().take(n_tracks) {
        if tr.effective_mute {
            continue;
        }
        for i in 0..n {
            master_l[i] += tr.track_l[i];
            master_r[i] += tr.track_r[i];
        }
    }
}

/// Replay the post-dispatch portion of the routing schedule:
/// `Mix { dst: TrackScratch }` (children → group bus), `ProcessGroupFx`
/// (group's fx_chain + strip), and `Mix { dst: Master }` (top-level
/// scratches → master). `ProcessTrack` ops are no-ops here because
/// `dispatch_and_wait` has already filled the per-track scratches.
#[allow(clippy::too_many_arguments)]
pub fn execute_schedule_post_dispatch(
    schedule: &mut Schedule,
    scratch: &mut [TrackScratch],
    master_l: &mut [f32],
    master_r: &mut [f32],
    n: usize,
    song: &Song,
    plugin_refs: &HashMap<u32, PluginRef>,
    slot_to_plugin_id: &HashMap<(u32, PluginSlot), u32>,
    worker_sync: Option<&WorkerSyncRef>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    any_solo: bool,
    playhead: u64,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    current_bpm: f32,
) {
    // `nodes` の不変参照と `delay_lines` の可変参照を同時に取りたい
    // (ApplyDelay で line を引きながら nodes を回すため)。 `Schedule`
    // を split borrow で 2 つの参照に分解する。
    let Schedule {
        nodes,
        delay_lines,
        port_buffers: _,
        input_delay_per_track: _,
    } = schedule;
    for op in nodes.iter() {
        match op {
            NodeOp::ProcessTrack { .. } => {
                // Already handled by dispatch_and_wait above.
            }
            NodeOp::Mix {
                srcs,
                dst: BufRef::TrackScratch(target_idx),
            } => {
                mix_into_track_scratch(scratch, *target_idx as usize, srcs, n);
            }
            NodeOp::Mix {
                srcs,
                dst: BufRef::Master,
            } => {
                mix_into_master(scratch, srcs, master_l, master_r, n);
            }
            NodeOp::Mix {
                dst: BufRef::Pooled(_) | BufRef::PluginAuxOut { .. },
                ..
            } => {
                // PR4: pooled targets and plugin aux-out routing land
                // here once parallel-out support arrives.
            }
            NodeOp::ProcessGroupFx { track_idx } => {
                let Some(track) = song.tracks.get(*track_idx as usize) else {
                    continue;
                };
                let Some(target) = scratch.get_mut(*track_idx as usize) else {
                    continue;
                };
                run_group_fx_chain(
                    *track_idx,
                    track,
                    song,
                    target,
                    plugin_refs,
                    slot_to_plugin_id,
                    worker_sync,
                    sample_rate,
                    playhead,
                    frames,
                    playing,
                    any_solo,
                    recording_lanes,
                    current_bpm,
                );
            }
            NodeOp::ApplyDelay {
                buf,
                line_idx,
                frames: delay_frames,
            } => {
                // PR3: `buf` の scratch を in-place で `delay_frames` だけ
                // 遅延させる。 `compile_schedule` は path latency が大きい
                // 側に揃えるため、 小さい side の `BufRef::TrackScratch(i)`
                // を絶対指す前提。 想定外 BufRef は無視。
                let BufRef::TrackScratch(track_idx) = *buf else {
                    continue;
                };
                let Some(s) = scratch.get_mut(track_idx as usize) else {
                    continue;
                };
                let Some(line) = delay_lines.get_mut(*line_idx as usize) else {
                    continue;
                };
                let n = (n).min(s.track_l.len()).min(s.track_r.len());
                line.step_in_place(
                    &mut s.track_l[..n],
                    &mut s.track_r[..n],
                    *delay_frames as usize,
                );
            }
            NodeOp::SidechainTap {
                src,
                dst_track,
                dst_slot,
                aux_in_port,
            } => {
                // PR4 sidechain: copy source track's scratch L/R into the
                // destination plugin's `pd.buffer_aux_in[port]` shmem
                // region, marking the port active so `daw_plugin_host`
                // forwards it as a CLAP `clap_audio_buffer` / VST3
                // aux bus on the next `process()`. Source != TrackScratch
                // (e.g. a future PluginAuxOut output) is ignored — handled
                // by PR4.4 / PR5.
                let BufRef::TrackScratch(src_idx) = *src else {
                    tracing::trace!("sidechain tap: non-TrackScratch src skipped");
                    continue;
                };
                let Some(src_scratch) = scratch.get(src_idx as usize) else {
                    tracing::trace!(src_idx, "sidechain tap: src_idx out of scratch range");
                    continue;
                };
                let port = *aux_in_port as usize;
                if port >= common::process_data::MAX_AUX_IN {
                    tracing::trace!(port, "sidechain tap: port >= MAX_AUX_IN");
                    continue;
                }
                // Resolve the runtime plugin_id for (dst_track, dst_slot).
                // PR2.1: the chains map is keyed by (track_id, slot).
                let key = (*dst_track, *dst_slot);
                let Some(&plugin_id) = slot_to_plugin_id.get(&key) else {
                    tracing::trace!(
                        dst_track = *dst_track,
                        ?dst_slot,
                        "sidechain tap: slot_to_plugin_id miss",
                    );
                    continue;
                };
                let Some(plugin_ref) = plugin_refs.get(&plugin_id) else {
                    tracing::trace!(plugin_id, "sidechain tap: plugin_refs miss");
                    continue;
                };
                let pd = plugin_ref.data_mut();
                let copy_n = n
                    .min(src_scratch.track_l.len())
                    .min(src_scratch.track_r.len());
                pd.buffer_aux_in[port][0][..copy_n]
                    .copy_from_slice(&src_scratch.track_l[..copy_n]);
                pd.buffer_aux_in[port][1][..copy_n]
                    .copy_from_slice(&src_scratch.track_r[..copy_n]);
                pd.aux_in_active[port] = 1;
            }
        }
    }
}

/// Sum the listed source scratches into `scratch[target_idx]` (used to
/// feed group buses with their children). Clears the target first so
/// stale samples from a previous buffer don't leak.
fn mix_into_track_scratch(
    scratch: &mut [TrackScratch],
    target_idx: usize,
    srcs: &[(BufRef, f32)],
    n: usize,
) {
    if target_idx >= scratch.len() {
        return;
    }
    {
        let target = &mut scratch[target_idx];
        target.track_l[..n].fill(0.0);
        target.track_r[..n].fill(0.0);
    }
    let (left, right) = scratch.split_at_mut(target_idx);
    let (target_slot, after) = right.split_first_mut().expect("split bounds checked above");
    for (src, gain) in srcs {
        let BufRef::TrackScratch(s_idx) = src else {
            continue;
        };
        let s = *s_idx as usize;
        if s == target_idx {
            continue;
        }
        let s_scratch = if s < target_idx {
            &left[s]
        } else if s - target_idx - 1 < after.len() {
            &after[s - target_idx - 1]
        } else {
            continue;
        };
        if s_scratch.effective_mute {
            continue;
        }
        let g = *gain;
        for i in 0..n {
            target_slot.track_l[i] += s_scratch.track_l[i] * g;
            target_slot.track_r[i] += s_scratch.track_r[i] * g;
        }
    }
}

/// Sum each non-muted source scratch (with its routing gain) into the
/// master bus. The master buffers are zeroed earlier in `process_buffer`
/// so this is `+= ` style accumulation.
fn mix_into_master(
    scratch: &[TrackScratch],
    srcs: &[(BufRef, f32)],
    master_l: &mut [f32],
    master_r: &mut [f32],
    n: usize,
) {
    let n = n.min(master_l.len()).min(master_r.len());
    for (src, gain) in srcs {
        let BufRef::TrackScratch(s_idx) = src else {
            continue;
        };
        let Some(s_scratch) = scratch.get(*s_idx as usize) else {
            continue;
        };
        if s_scratch.effective_mute {
            continue;
        }
        let g = *gain;
        for i in 0..n {
            master_l[i] += s_scratch.track_l[i] * g;
            master_r[i] += s_scratch.track_r[i] * g;
        }
    }
}

/// `track_id` の子孫 (parent_group_id chain で辿れる) のいずれかが
/// `solo == true` なら true。 Ableton Live の "soloed via children"
/// 挙動 (group track 自身が solo されていなくても、 子が solo なら
/// group も透過させる) のために使う。 cycle-safe: hops 上限 32。
fn has_soloed_descendant(song: &Song, track_id: u32) -> bool {
    let mut frontier: Vec<u32> = vec![track_id];
    let mut hops = 0_usize;
    while let Some(pid) = frontier.pop() {
        hops += 1;
        if hops > song.tracks.len() + 1 {
            return false;
        }
        for t in &song.tracks {
            if t.parent_group_id == Some(pid) {
                if t.solo {
                    return true;
                }
                frontier.push(t.id);
            }
        }
    }
    false
}

/// Run a Group track's audio fx chain on its already-mixed input
/// scratch, then apply the group's mixer strip (volume / pan / mute /
/// solo + peak meter). Mirrors the audio-fx tail of `process_track_owned`,
/// but skips the sequencer / MIDI FX / instrument stages because groups
/// have no clips of their own.
#[allow(clippy::too_many_arguments)]
fn run_group_fx_chain(
    track_idx: u32,
    song_track: &Track,
    song: &Song,
    scratch: &mut TrackScratch,
    plugin_refs: &HashMap<u32, PluginRef>,
    slot_to_plugin_id: &HashMap<(u32, PluginSlot), u32>,
    worker_sync: Option<&WorkerSyncRef>,
    sample_rate: u32,
    playhead: u64,
    frames: u32,
    playing: bool,
    any_solo: bool,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    current_bpm: f32,
) {
    let n = frames as usize;
    let track_id = song_track.id;

    for i in 0..song_track.fx_chain.len() {
        // PR2.1: id ベースの key で lookup (plugin chain は track_id
        // で識別、 song.tracks の Vec position に依存しない)。
        let key = (track_id, PluginSlot::Fx(i as u32));
        let Some(&plugin_id) = slot_to_plugin_id.get(&key) else {
            continue;
        };
        let Some(plugin_ref) = plugin_refs.get(&plugin_id) else {
            continue;
        };
        let Some(ws) = worker_sync else { continue };

        let pd = plugin_ref.data_mut();
        pd.prepare();
        pd.frames = frames;
        pd.playing = if playing { 1 } else { 0 };
        pd.sample_rate = sample_rate;
        set_pd_transport(pd, Some(song), current_bpm);
        // Phase 2b: group fx 宛 PluginParam automation。
        crate::automation::fill_pd_param_events(
            pd,
            song,
            track_id,
            PluginSlot::Fx(i as u32),
            sample_rate,
            song.bpm,
            playhead,
            frames,
        );
        pd.buffer_in[0][..n].copy_from_slice(&scratch.track_l[..n]);
        pd.buffer_in[1][..n].copy_from_slice(&scratch.track_r[..n]);
        if let Err(e) = ws.dispatch(plugin_id) {
            tracing::error!(error = ?e, plugin_id, "group fx dispatch failed");
            continue;
        }
        scratch.track_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
        scratch.track_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
    }

    let muted = song_track.muted;
    let solo = song_track.solo;
    // Live 互換: 子のいずれかが solo されている場合、 group 自身は
    // solo フラグが立っていなくても透過させる ("soloed via children")。
    // → `effective_mute = muted || (any_solo && !solo && !子に solo)`
    let effective_mute = muted
        || (any_solo
            && !solo
            && !has_soloed_descendant(song, song_track.id));
    scratch.effective_mute = effective_mute;
    if effective_mute {
        scratch.track_l[..n].fill(0.0);
        scratch.track_r[..n].fill(0.0);
        scratch.peak_l = 0.0;
        scratch.peak_r = 0.0;
    } else {
        crate::automation::fill_track_param_ramps(
            Some(song),
            track_idx,
            sample_rate,
            song.bpm,
            playhead,
            frames,
            &mut scratch.volume_per_sample,
            &mut scratch.pan_per_sample,
            recording_lanes,
        );
        let mut peak_l = 0.0_f32;
        let mut peak_r = 0.0_f32;
        for i in 0..n {
            let pan = scratch.pan_per_sample[i].clamp(-1.0, 1.0);
            let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
            let vol = scratch.volume_per_sample[i];
            let gain_l = angle.cos() * vol;
            let gain_r = angle.sin() * vol;
            let l = scratch.track_l[i] * gain_l;
            let r = scratch.track_r[i] * gain_r;
            scratch.track_l[i] = l;
            scratch.track_r[i] = r;
            if l.abs() > peak_l {
                peak_l = l.abs();
            }
            if r.abs() > peak_r {
                peak_r = r.abs();
            }
        }
        scratch.peak_l = peak_l;
        scratch.peak_r = peak_r;
    }
}

#[cfg(test)]
mod sidechain_tests {
    use super::*;
    use crate::graph::compile_schedule;
    use common::model::{PluginInstance, Song, Track};
    use common::plugin_format::PluginFormat;

    /// PR4 Sidechain engine-handler test: 実 plugin を立てなくても、
    /// `execute_schedule_post_dispatch` の `NodeOp::SidechainTap` ハンドラ
    /// が source TrackScratch の signal を `pd.buffer_aux_in[port]` に正しく
    /// copy することを直接検証する。 ProcessData は heap に Box で置き、
    /// `PluginRef` を手書きして plugin_refs / slot_to_plugin_id に登録する。
    #[test]
    fn sidechain_tap_copies_source_track_into_plugin_aux_in_buffer() {
        let song = Song {
            tracks: vec![
                Track { id: 1, name: "Source".into(), ..Track::default() },
                Track {
                    id: 2,
                    name: "Dest".into(),
                    fx_chain: vec![PluginInstance {
                        plugin_id: "test.scc".into(),
                        format: PluginFormat::Vst3,
                        state: None,
                        sidechain_sources: vec![Some(1)],
                    }],
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let mut schedule = compile_schedule(&song).unwrap();
        assert!(schedule.nodes.iter().any(|op| matches!(op, NodeOp::SidechainTap { .. })));

        const FRAMES: usize = 64;
        let mut scratch: Vec<TrackScratch> =
            (0..common::audio_bridge::MAX_TRACKS).map(|_| TrackScratch::new()).collect();
        for i in 0..FRAMES {
            scratch[0].track_l[i] = (i as f32) * 0.1;
            scratch[0].track_r[i] = -(i as f32) * 0.1;
        }
        let mut master_l = vec![0.0f32; FRAMES];
        let mut master_r = vec![0.0f32; FRAMES];

        let mut pd = Box::new(common::process_data::ProcessData::empty());
        let pd_ptr: *mut common::process_data::ProcessData = &mut *pd;
        let plugin_id: u32 = 42;
        let plugin_ref = common::plugin_ref::PluginRef { plugin_id, process_data: pd_ptr };
        let mut plugin_refs: HashMap<u32, common::plugin_ref::PluginRef> = HashMap::new();
        plugin_refs.insert(plugin_id, plugin_ref);

        let mut slot_to_plugin_id: HashMap<(u32, common::protocol::PluginSlot), u32> =
            HashMap::new();
        slot_to_plugin_id.insert((2, common::protocol::PluginSlot::Fx(0)), plugin_id);

        execute_schedule_post_dispatch(
            &mut schedule,
            &mut scratch,
            &mut master_l,
            &mut master_r,
            FRAMES,
            &song,
            &plugin_refs,
            &slot_to_plugin_id,
            None,
            48_000,
            FRAMES as u32,
            true,
            false,
            0,
            &std::collections::HashSet::new(),
            song.bpm,
        );

        for i in 0..FRAMES {
            let want_l = (i as f32) * 0.1;
            let want_r = -(i as f32) * 0.1;
            assert!((pd.buffer_aux_in[0][0][i] - want_l).abs() < 1e-6);
            assert!((pd.buffer_aux_in[0][1][i] - want_r).abs() < 1e-6);
        }
        assert_eq!(pd.aux_in_active[0], 1);
    }
}
