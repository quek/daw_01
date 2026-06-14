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
    /// `track` / `index` let the engine slot the plugin into its routing
    /// graph at the matching position in the single device chain. `handle`
    /// keeps the daw_audio-side shmem mapping alive — without it,
    /// `plugin_ref.process_data` would be a dangling pointer once
    /// `handle_open_plugin_shmem` returns and drops its local
    /// `ProcessDataHandle`.
    OpenPluginShmem {
        plugin_id: u32,
        plugin_ref: PluginRef,
        handle: common::process_data::ProcessDataHandle,
        track: u32,
        index: u32,
    },
    /// Drop a previously-opened plugin shmem mapping. Triggered on
    /// RemoveSlotPlugin / RemoveTrack from the GUI side.
    ClosePluginShmem { plugin_id: u32 },
    /// FIXME #32: atomically re-key `slot_to_plugin_id` for a chain reorder.
    /// `moves` is the complete `(old_index, new_index)` permutation of
    /// `track`'s loaded plugins (see `MainToChild::ReorderChain`). Only the
    /// device-index KEYS move — the plugin ids and their `plugin_refs` are
    /// untouched, so no plugin is ever briefly dropped (unlike a sequence of
    /// `OpenPluginShmem` at swapping indices). The processing ORDER follows
    /// the matching `LoadSong`; this just makes each index resolve to its
    /// moved plugin.
    ReorderChain {
        track: u32,
        moves: Vec<(u32, u32)>,
    },
    /// 鍵盤レーン click のプレビュー note-on (gui_01 #055)。 `track` は
    /// song.tracks の Vec index (= main.rs が `track_id` から現 song snapshot
    /// で解決済)、 `velocity` は normalized 0..=1。 `pump_commands` が該当
    /// track の `pending_preview` に積み、 `process_track_owned` が次の
    /// dispatch で frame 0 に注入する。
    PreviewNoteOn {
        track: usize,
        pitch: u8,
        velocity: f64,
    },
    /// 鍵盤プレビューの note-off (gui_01 #055)。 `track` は note-on と同じ
    /// Vec index。
    PreviewNoteOff { track: usize, pitch: u8 },
}

/// 鍵盤プレビュー note の `note_id`。 sequencer が振る通し index (= 0.. の
/// 小さい値) と衝突しない sentinel。 CLAP/VST3 は `note_id` を無視し、 builtin
/// は key 一致で発音/停止するので、 on/off で同値であれば voice 対応が取れる。
const PREVIEW_NOTE_ID: u32 = u32::MAX;

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

/// `pending_seek` の sentinel = 「seek 要求なし」。playhead はサンプル単位で、
/// この値 (u64::MAX サンプル ≈ 数百万年) に達することは現実的に無いので
/// 「要求なし」を表す番兵に使える。
pub const NO_PENDING_SEEK: u64 = u64::MAX;

/// State shared between the audio thread and the IPC receive loop /
/// future GUI commands. Every field is wait-free: the audio thread reads
/// these on every buffer; the IPC side writes them on each command.
pub struct SharedState {
    pub song: ArcSwapOption<Song>,
    pub playback: AtomicU8,
    pub looping: AtomicBool,
    /// Last published playhead in samples. Mirrored to shmem for the GUI
    /// playhead cursor. **書き込みは audio thread (`process_buffer`) 単独**。
    /// IPC スレッドは seek を `pending_seek` に積むだけで、ここを直接書かない
    /// (FIXME #41: 直接書くと buffer 末の advance store と race して、Stop 直後
    /// に停止位置へ巻き戻る = 開始位置に戻らないバグになる)。
    pub playhead: AtomicU64,
    /// FIXME #41: GUI からの `SeekTo` 要求を audio thread に渡す single-writer
    /// チャネル。IPC 受信スレッドが目標サンプル位置を `store`、audio thread が
    /// `process_buffer` 冒頭で `swap` 消費して `playhead` に反映する。これにより
    /// `playhead` の writer を audio thread 単独に保ち、停止/seek の競合を排除する。
    /// `NO_PENDING_SEEK` = 要求なし。多重要求は last-wins。
    pub pending_seek: AtomicU64,
    /// Phase 4 Step C-2 (`docs/plan_automation.md` §6): currently recording
    /// lane set (= GUI が `SetRecordingLanes` で更新)。 audio thread は
    /// 各 buffer の頭で `load()` し、 `fill_track_param_ramps` で該当 lane
    /// の curve eval を bypass する。 `(track_id, AutomationTarget)` の
    /// 2 つ組で identify (lane_id を使わないのは GUI 側で lane を削除して
    /// から audio に通知が届くまでの race を避けるため = target 一致なら
    /// bypass で済む)。 起動時は空。
    pub recording_lanes:
        arc_swap::ArcSwap<std::collections::HashSet<(u32, common::model::AutomationTarget)>>,
    /// Phase 7 B3 (2026-05-13): メトロノーム on/off。 GUI が
    /// `MainToChild::SetMetronomeEnabled(bool)` で更新、 audio thread が
    /// `render_metronome` で読む。 false なら click 生成を skip (= 無音)。
    /// 起動時 default false。
    pub metronome_enabled: AtomicBool,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            song: ArcSwapOption::empty(),
            playback: AtomicU8::new(PlaybackCommand::Stop as u8),
            looping: AtomicBool::new(false),
            playhead: AtomicU64::new(0),
            pending_seek: AtomicU64::new(NO_PENDING_SEEK),
            recording_lanes: arc_swap::ArcSwap::from_pointee(
                std::collections::HashSet::new(),
            ),
            metronome_enabled: AtomicBool::new(false),
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
    /// `(track, device_index)` → `plugin_id`. New snapshot in lock-step with
    /// `plugin_refs`. v23 single-chain: addressed by the device's position in
    /// `Track.devices` (was per-section `PluginSlot`).
    pub slot_to_plugin_id: ArcSwap<HashMap<(u32, u32), u32>>,
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
    /// Phase 7 B4 Step C (2026-05-13): count-in 用 preroll の合計 samples
    /// (= count-in 開始時に GUI が `StartCountIn { samples }` で立てた値の
    /// snapshot)。 `process_buffer` で `elapsed = total - remaining` を
    /// 計算して metronome の click trigger 用 playhead として使う。 0 で
    /// count-in 中ではない。
    pub preroll_total_samples: AtomicU64,
    /// Phase 7 B4 Step C: count-in 残り samples。 audio thread が毎 buffer
    /// `frames` だけ deduct + audio_bridge mirror 経由で GUI に publish。
    /// 0 到達で通常再生に戻る (= dispatch / clip render 復帰)。
    pub preroll_remaining_samples: AtomicU64,
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
            preroll_total_samples: AtomicU64::new(0),
            preroll_remaining_samples: AtomicU64::new(0),
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
/// Phase 7 B3 (2026-05-13): メトロノーム click voice (sine + linear envelope
/// decay)。 1 voice mono。 beat 境界で trigger され、 `samples_remaining` が
/// 0 になるまで sine を mix。 連続 beat で前 voice が decay 中なら新 voice
/// で overwrite (= 短 decay の業界標準 idiom)。
///
/// パラメータ default: decay 40 ms / amplitude peak 0.25 (-12 dB) / freq
/// downbeat 880 Hz 他 440 Hz。 すべて `render_metronome` で hardcode。
pub struct ClickVoice {
    /// remaining samples until envelope reaches 0 (= voice expires)
    pub samples_remaining: u32,
    /// total decay length in samples (envelope = remaining / decay_total)
    pub decay_samples: u32,
    /// oscillator frequency (Hz)
    pub freq: f32,
    /// phase accumulator (radians, 0..=TAU)
    pub phase: f32,
    /// 次 buffer 内で voice を再開する sample offset (= trigger frame)。
    /// 0 なら buffer の最初から再開 (= 前 buffer から続いている voice)。
    pub start_offset: u32,
}

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
    /// Phase 7 B3 (2026-05-13): metronome click voice 状態 (mono single-voice、
    /// per-buffer で beat 境界を検出して trigger、 sine + linear envelope decay)。
    /// `Some` なら active (= まだ decay 中)、 `None` なら idle。 連続 beat で
    /// 前 voice が decay 中なら overwrite (= 短 decay の 1 voice のみ持続、
    /// 業界標準の click と同 idiom)。 master mix 後に `render_metronome` で
    /// 反映。 export 中 / playing でない / `metronome_enabled = false` のいずれ
    /// でも render skip。
    pub metronome_voice: Option<ClickVoice>,
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
    /// docs/plan_modulation.md §5: reusable per-buffer snapshot of follower
    /// scalars (slot = `ModSource` position), filled from
    /// `cached_schedule.follower_slots` before dispatch (= the previous
    /// buffer's envelopes) and published to the audio workers so volume / pan /
    /// plugin-param lanes with `mod_routings` modulate. Reused across buffers
    /// (no per-buffer allocation once warmed).
    pub mod_scalars_snapshot: Vec<f32>,
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
    pub heartbeat_slot_keys: Vec<((u32, u32), u32)>,
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
            metronome_voice: None,
            cmd_rx,
            shared,
            cached_schedule: Schedule::empty(),
            mod_scalars_snapshot: Vec::with_capacity(common::audio_bridge::MAX_MOD_SOURCES),
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
                    index,
                } => {
                    // Snapshot-copy-mutate-publish so RT readers either
                    // see the old map or the fully-populated new one,
                    // never a partial state.
                    let mut new_refs: HashMap<u32, PluginRef> =
                        (**self.shared.plugin_refs.load()).clone();
                    let mut new_slot: HashMap<(u32, u32), u32> =
                        (**self.shared.slot_to_plugin_id.load()).clone();
                    if let Some(stale) = new_slot.insert((track, index), plugin_id)
                        && stale != plugin_id
                        // FIXME #31: only drop the displaced plugin from
                        // plugin_refs if it isn't still mapped at ANOTHER index.
                        // On a live move the displaced plugin keeps being
                        // processed at its new device index (its OpenPluginShmem
                        // at the new index arrives first); the new plugin then
                        // displaces it here, but it must stay in plugin_refs.
                        && !new_slot.values().any(|&pid| pid == stale)
                    {
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
                    tracing::info!(plugin_id, track, index, "plugin shmem registered");
                }
                AudioCommand::ClosePluginShmem { plugin_id } => {
                    let mut new_refs: HashMap<u32, PluginRef> =
                        (**self.shared.plugin_refs.load()).clone();
                    let cur_slot = self.shared.slot_to_plugin_id.load();
                    // Find the (track_id, device_index) that pointed at this
                    // plugin_id BEFORE we remove the entry, so we can shift the
                    // remaining higher device indices on the same track down by
                    // one (mirrors the `Vec::remove` daw_gui / daw_plugin_host
                    // did on `Track.devices`). v23 single-chain: the index
                    // space is unified, so removing device i leaves device i+1
                    // stranded under its old key; without this shift,
                    // `process_track_owned` would look up index i → no hit →
                    // silent (chain device dropped on deletion).
                    let removed_key = cur_slot
                        .iter()
                        .find_map(|(k, v)| if *v == plugin_id { Some(*k) } else { None });
                    let mut new_slot: HashMap<(u32, u32), u32> = (**cur_slot).clone();
                    new_slot.retain(|_, pid| *pid != plugin_id);
                    if let Some((track_id, removed_idx)) = removed_key {
                        let entries: Vec<((u32, u32), u32)> = new_slot.drain().collect();
                        for ((tid, idx), pid) in entries {
                            let new_idx = if tid == track_id && idx > removed_idx {
                                idx - 1
                            } else {
                                idx
                            };
                            new_slot.insert((tid, new_idx), pid);
                        }
                    }
                    new_refs.remove(&plugin_id);
                    self.shared.plugin_refs.store(Arc::new(new_refs));
                    self.shared.slot_to_plugin_id.store(Arc::new(new_slot));
                    tracing::info!(plugin_id, "plugin shmem dropped + index shifted");
                }
                AudioCommand::ReorderChain { track, moves } => {
                    // Re-key slot_to_plugin_id from old→new in one atomic
                    // publish. Remove every old key FIRST (snapshot the pids),
                    // then re-insert at the new keys, so a swap (0↔1) can't
                    // clobber the second pid. `plugin_refs` is left untouched:
                    // the plugins themselves don't move, only the
                    // (track, device_index) → plugin_id addressing does — so no
                    // plugin is ever transiently missing from the graph.
                    let mut new_slot: HashMap<(u32, u32), u32> =
                        (**self.shared.slot_to_plugin_id.load()).clone();
                    // Pass 1: detach every moved plugin from its old key,
                    // keeping the pids index-aligned with `moves`.
                    let pids: Vec<Option<u32>> = moves
                        .iter()
                        .map(|&(from, _)| new_slot.remove(&(track, from)))
                        .collect();
                    // Pass 2: re-attach each at its new key.
                    for (&(_, to), pid) in moves.iter().zip(pids) {
                        if let Some(pid) = pid {
                            new_slot.insert((track, to), pid);
                        }
                    }
                    self.shared.slot_to_plugin_id.store(Arc::new(new_slot));
                    tracing::info!(track, n = moves.len(), "slot_to_plugin_id reordered");
                }
                AudioCommand::PreviewNoteOn {
                    track,
                    pitch,
                    velocity,
                } => {
                    // 鍵盤プレビュー: 該当 track の pending_preview に積む。
                    // process_track_owned が次の dispatch で frame 0 に注入する。
                    // capacity 上限で guard し RT での realloc を避ける
                    // (push_note_on と同じ「溢れたら drop」 方針)。
                    if let Some(s) = self.scratch.get_mut(track) {
                        let pp = &mut s.state.pending_preview;
                        if pp.len() < pp.capacity() {
                            pp.push(NoteTransition::On {
                                note_id: PREVIEW_NOTE_ID,
                                key: pitch,
                                velocity,
                            });
                        }
                    }
                }
                AudioCommand::PreviewNoteOff { track, pitch } => {
                    if let Some(s) = self.scratch.get_mut(track) {
                        let pp = &mut s.state.pending_preview;
                        if pp.len() < pp.capacity() {
                            pp.push(NoteTransition::Off {
                                note_id: PREVIEW_NOTE_ID,
                                key: pitch,
                            });
                        }
                    }
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

        // Snapshot the transport-state atomics once for the whole buffer so
        // every step below sees a single consistent view (export gate /
        // loop wrap / metronome gate). Loading each atomic at multiple call
        // sites could otherwise observe a mid-buffer flip and produce an
        // internally inconsistent buffer.
        let export_running = self.shared.export_running.load(Ordering::Acquire);
        let looping = shared.looping.load(Ordering::Acquire);
        let metronome_enabled = shared.metronome_enabled.load(Ordering::Acquire);

        // A3 freewheel: while the export thread holds the audio
        // resources, write silence and skip dispatch so the worker pool
        // and plugin instances are exclusively driven by the export
        // render loop.
        if export_running {
            return;
        }

        // Phase 7 B4 Step C (2026-05-13): count-in モード — preroll > 0 なら
        // 通常 dispatch / clip render を skip し、 metronome のみ render +
        // preroll counter を deduct + audio_bridge に mirror。 0 到達で通常
        // 再生に戻る (= 次 buffer で本 if が false になり、 既存 dispatch
        // 経路に進む)。 GUI 側 on_tick が audio_bridge の preroll mirror を
        // poll、 0 検出で midi_recording_pending → midi_recording 遷移。
        let preroll =
            self.shared.preroll_remaining_samples.load(Ordering::Acquire);
        if preroll > 0 {
            let total =
                self.shared.preroll_total_samples.load(Ordering::Acquire);
            let elapsed = total.saturating_sub(preroll);
            let bpm = song_snapshot
                .as_ref()
                .map(|s| s.bpm)
                .unwrap_or(120.0)
                .max(1.0);
            let tsig_num = i64::from(
                song_snapshot
                    .as_ref()
                    .map(|s| s.time_sig.0)
                    .unwrap_or(4)
                    .max(1),
            );
            if metronome_enabled {
                render_metronome(
                    &mut self.metronome_voice,
                    &mut self.master_l[..n],
                    &mut self.master_r[..n],
                    n,
                    elapsed,
                    sample_rate,
                    bpm,
                    tsig_num,
                );
            }
            let new_preroll = preroll.saturating_sub(n as u64);
            self.shared
                .preroll_remaining_samples
                .store(new_preroll, Ordering::Release);
            bridge.set_preroll_remaining(new_preroll);
            return;
        }

        // FIXME #41: GUI からの seek 要求を audio thread 単独 writer として
        // `playhead` に反映する。IPC スレッドが `playhead` を直接書くと、下の
        // buffer 末 advance store と同一 atomic を別スレッドから書く race になり、
        // Stop 直後 (in-flight buffer がまだ playing で advance する瞬間) に開始
        // 位置への巻き戻しが上書きされて停止位置から再生されてしまう。`swap` で
        // 消費する (多重要求は last-wins)。ここで `playhead` を書き換えておけば、
        // 下の `playhead` load → seek 検出 (`playhead != last_known_playhead`) が
        // `playhead_beats` を再同期する。
        let pending_seek = shared.pending_seek.swap(NO_PENDING_SEEK, Ordering::AcqRel);
        if pending_seek != NO_PENDING_SEEK {
            shared.playhead.store(pending_seek, Ordering::Release);
        }

        // Play / Stop edge handling. On Play, restart playhead and clear
        // active notes. On Stop, queue offs at frame 0 of the next buffer
        // so plugins drain cleanly.
        let desired = PlaybackCommand::from_u8(shared.playback.load(Ordering::Acquire));
        match (self.playing, desired) {
            (false, PlaybackCommand::Play) => {
                self.playing = true;
                // Play は **現在の playhead からそのまま再生する** (頭出しは
                // しない)。「どこから再生するか」「停止でどこへ戻すか」は GUI 側
                // が所有する (FIXME #41 のモデル A = Pro Tools / Ableton 流の
                // 「停止すると再生を押した位置に戻る」)。GUI は play() 時の
                // playhead を origin として記録し、stop() で SeekTo を送って
                // engine カーソルを origin に揃える。engine はその SeekTo
                // (= pending_seek 経由で playhead に反映済み) をそのまま使う。
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
        // user の loop button 実状態 (= engine が実際に wrap する条件)。
        // plugin transport の IS_LOOP_ACTIVE / looping field に渡す。
        // (buffer 冒頭で snapshot 済の `looping` ローカルを使う。)

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

            // docs/plan_modulation.md §5: snapshot the previous buffer's
            // follower envelopes (slot order = `ModSource` position) for audio-
            // param modulation, reusing the buffer (no per-buffer alloc). The
            // EnvelopeFollow nodes for THIS buffer run post-dispatch, so param
            // events see the prior buffer's env — a ~1-buffer (block-rate) lag.
            // FIXME #56: follower の env (= 前 buffer 値、 上記の lag) に加え、
            // generator (LFO/Random/MSEG/Steps) は `song_beat`/`song_secs` から
            // この buffer の値を直接算出する (状態レス・lag なし、 決定論)。
            self.mod_scalars_snapshot.clear();
            let song_secs = playhead as f64 / sample_rate as f64;
            for (fs, kind) in self
                .cached_schedule
                .follower_slots
                .iter()
                .zip(self.cached_schedule.mod_kinds.iter())
            {
                let v = common::modulators::generator_scalar(kind, self.playhead_beats, song_secs)
                    .unwrap_or(fs.env);
                self.mod_scalars_snapshot.push(v);
            }

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
                    looping,
                    &self.mod_scalars_snapshot,
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
                        looping,
                        &self.mod_scalars_snapshot,
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
                self.playhead_beats,
                looping,
            );

            // master bus fx chain。 全 track mix 後・metronome 前に直列 process
            // する (= metronome guide は master fx を通さない、 track fx と同じ
            // worker dispatch idiom)。 master_fx_chain が空なら即 return で CPU 0。
            process_master_fx_chain(
                &song.master_fx_chain,
                &mut self.master_l[..n],
                &mut self.master_r[..n],
                &plugin_refs_g,
                &slot_map_g,
                worker_syncs_g.first(),
                sample_rate,
                n as u32,
                playing,
                Some(song),
                current_bpm,
                self.playhead_beats,
                looping,
            );

            // Phase 7 B3 (2026-05-13): metronome click を master mix に追加。
            // export 中 / playing でない / metronome_enabled false のいずれ
            // でも skip (= mix 自体が走らないので CPU 0)。 enable 時は beat
            // 境界 (current_bpm + tsig 由来) ごとに voice を trigger、 既存
            // voice があれば overwrite (= 短 decay 1 voice だけ持続、 業界
            // 標準の click)。 master mix の最後に重ねる (= track の mute /
            // solo / volume の影響を受けない、 「常に聞こえる guide」 が
            // metronome の規範動作)。
            if !export_running && playing && metronome_enabled {
                let tsig_num = i64::from(song.time_sig.0.max(1));
                render_metronome(
                    &mut self.metronome_voice,
                    &mut self.master_l[..n],
                    &mut self.master_r[..n],
                    n,
                    playhead,
                    sample_rate,
                    current_bpm,
                    tsig_num,
                );
            }

            // Publish per-track peak meters into the shared AudioBridge
            // so the GUI mixer strips animate. Atomic stores, RT-safe.
            // Tracks with effective_mute already have peak_l/r == 0.
            for (i, tr) in self.scratch.iter().take(n_tracks).enumerate() {
                bridge.set_track_peak(i, tr.peak_l, tr.peak_r);
            }

            // docs/plan_modulation.md §4.2: publish each ModSource's envelope
            // follower scalar (block-rate, `env` after this buffer) so the GUI
            // poller can apply visual/param modulation. Atomic stores, RT-safe.
            // FIXME #56: follower は env、 generator は song 位置から直接算出して publish。
            let pub_song_secs = playhead as f64 / sample_rate as f64;
            for (slot, (fs, kind)) in self
                .cached_schedule
                .follower_slots
                .iter()
                .zip(self.cached_schedule.mod_kinds.iter())
                .enumerate()
            {
                let v = common::modulators::generator_scalar(kind, self.playhead_beats, pub_song_secs)
                    .unwrap_or(fs.env);
                bridge.set_mod_scalar(slot, v);
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
            let active_end = if looping {
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
                let wrap_to = if looping {
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
            // Stop 中は audio thread が playhead を advance しない。GUI からの
            // SeekTo は process_buffer 冒頭の pending_seek consume で (audio
            // thread 自身が) shared.playhead に反映済みなので、その値で
            // last_known_playhead を同期し、次 Play 開始時の seek 検出を
            // 誤発火させない (= stop 中の seek は位置を変えるだけで
            // playhead_beats 再計算が必要)。
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
    // 積分済みの真の拍位置 (tempo automation を考慮)。 plugin host が一定
    // テンポ逆算する代わりにこれを直接 song_pos_beats として使う。
    song_pos_beats: f64,
    // user の loop button 実状態 (= `shared.looping`)。 region 有無の
    // heuristic ではなく engine が実際に wrap している条件を渡す。
    looping: bool,
) {
    let Some(song) = song else { return };
    pd.bpm = effective_bpm.max(1.0);
    pd.tsig_num = song.time_sig.0 as u16;
    pd.tsig_denom = song.time_sig.1 as u16;
    pd.loop_start_beats = song.loop_start_beat;
    pd.loop_end_beats = song.loop_end_beat;
    pd.song_pos_beats = song_pos_beats;
    // 実 loop トグル状態を渡す。 plugin host は IS_LOOP_ACTIVE 判定で
    // 別途 `loop_end_beats > loop_start_beats` (= region 定義済) と AND する。
    pd.looping = if looping { 1 } else { 0 };
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
/// has set to `max(path_latency(src) for src in fx_chain[*].aux_inputs[*].tap)`.
/// 0 = no delay (the common case).
#[allow(clippy::too_many_arguments)]
pub fn process_track_owned(
    track_idx: u32,
    song_track: &Track,
    scratch: &mut TrackScratch,
    plugin_refs: &HashMap<u32, PluginRef>,
    slot_to_plugin_id: &HashMap<(u32, u32), u32>,
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
    // user の loop button 実状態 (= `shared.looping`)。 set_pd_transport に渡す。
    looping: bool,
    // docs/plan_modulation.md §5: per-`ModSource` follower scalars (block-rate
    // snapshot, slot = `Song::mod_sources` position). fill_track_param_ramps /
    // fill_pd_param_events に渡して volume/pan/plugin param を follower 変調する。
    // 空なら変調なし (= 既存挙動と byte 同一)。
    mod_scalars: &[f32],
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
    // 鍵盤レーン click のプレビュー note (engine の pump_commands が該当 track の
    // pending_preview に積む)。 transport に関係なく frame 0 で 1 回注入する
    // (instrument dispatch は playing で gate されないので停止中でも発音する)。
    // collect_events_for_buffer より前に push し、 playing 時は同 buffer の
    // sort (CLAP の time 昇順 / 同 time は Off→On) に乗せる。
    for &ev in &scratch.state.pending_preview {
        scratch.midi_bus_a.push(TimedNoteEvent { time: 0, event: ev });
    }
    scratch.state.pending_preview.clear();
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

    // ---- Track audio output (cleared every buffer) ----
    // 毎 buffer ゼロから組み立てる。直後に audio clip を加算し、その後 device chain が
    // port 構成に従って audio を上書き / 加算していく。
    scratch.track_l[..n].fill(0.0);
    scratch.track_r[..n].fill(0.0);

    // PR-V4: 旧 VOICEVOX 専用 vocal block を削除。 vocal track は単一チェーン
    // 中の builtin VOICEVOX plugin (audio_out を持つ音源) として処理される。
    // daw_gui の migration が project load 時に旧 vocal tracks を builtin path
    // に移行する。

    // ---- v23 single-chain: serial port connection (Reaper 流) -----------
    // 役割判定はしない。track の MIDI (notes, midi_bus_a) と audio (clips) を
    // 起点に、各 device を順に処理し、device の port 構成に従って MIDI / audio を
    // 接続する。先に audio source (audio clip + sidechain alignment delay) を
    // track_l/r に入れてからチェーンを通す (clips → エフェクトで処理 / 音源出力に
    // 加算される)。playing == false では audio clip を mix しない (Stop で鳴り
    // 続けるバグ防止)。
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
    // PR4.5 sidechain plugin-internal alignment: main 信号を遅延させて sidechain
    // source と musical time を揃える。capacity は edit-time 確保済 (RT で再確保なし)。
    if input_delay_samples > 0 {
        scratch.input_delay_line.step_in_place(
            &mut scratch.track_l[..n],
            &mut scratch.track_r[..n],
            input_delay_samples as usize,
        );
    }

    // docs/plan_modulation_followups.md §1: snapshot the **pre-FX** signal (the
    // raw audio clip / input before the device chain) for any PreFx tap / mod
    // source. Guarded so untouched tracks skip the memcpy — RT-safe.
    if song.is_some_and(|s| track_needs_prefx_snapshot(s, track_id)) {
        scratch.pre_fx_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fx_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }

    for i in 0..song_track.devices.len() {
        // chain map の key は (track_id, device_index)。 song.tracks の Vec
        // position に依存しないので、 group 化や drag&drop reorder で track
        // index が shift しても plugin lookup が壊れない。
        let key = (track_id, i as u32);
        let ports = song_track.devices[i].ports;
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
        set_pd_transport(pd, song, current_bpm, playhead_beats, looping);
        if let Some(song) = song {
            crate::automation::fill_pd_param_events(
                pd,
                song,
                track_id,
                i as u32,
                sample_rate,
                song.bpm,
                playhead,
                frames,
                recording_lanes,
                mod_scalars,
            );
        }
        // ---- inputs: device の port を持つものだけ現在のバスを渡す ----
        if ports.has_note_input {
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
        }
        if ports.has_audio_input {
            pd.buffer_in[0][..n].copy_from_slice(&scratch.track_l[..n]);
            pd.buffer_in[1][..n].copy_from_slice(&scratch.track_r[..n]);
        }
        if let Err(_e) = ws.dispatch(plugin_id) {
            // RT path: skip on dispatch failure without per-buffer I/O.
            #[cfg(debug_assertions)]
            tracing::error!(error = ?_e, plugin_id, "device dispatch failed");
            continue;
        }
        // ---- outputs ----
        // note 出力を持つなら出力 MIDI で次段のバスを置き換える (無ければ素通し)。
        if ports.has_note_output {
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
                    EventKind::ParamValue | EventKind::ParamMod => continue,
                };
                scratch.midi_bus_b.push(timed);
            }
            scratch.midi_bus_b.sort_unstable_by_key(|e| e.time);
            std::mem::swap(&mut scratch.midi_bus_a, &mut scratch.midi_bus_b);
        }
        // audio 出力を持つなら: audio 入力も持つ機 (= エフェクト) は処理結果で
        // 置き換え、入力を持たない機 (= 音源/生成器) はソースとして加算する。
        if ports.has_audio_output {
            if ports.has_audio_input {
                scratch.track_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
                scratch.track_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
            } else {
                for j in 0..n {
                    scratch.track_l[j] += pd.buffer_out[0][j];
                    scratch.track_r[j] += pd.buffer_out[1][j];
                }
            }
        }
    }

    // ---- Pre-fader send tap ----
    // A pre-fader send reads the post-fx, pre-strip signal. Snapshot it
    // before the strip overwrites `track_l/r` in place. docs/plan_modulation.md
    // §6: a PostFx aux-input route or mod source also reads this snapshot, so
    // capture it for those too. Only copied when something actually needs it
    // (cheap check; skips the memcpy otherwise).
    let has_prefader_send = song_track
        .sends
        .iter()
        .any(|s| s.mode == common::model::SendMode::PreFader);
    if has_prefader_send
        || song.is_some_and(|s| track_needs_prefader_snapshot(s, song_track.id))
    {
        scratch.pre_fader_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fader_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }

    // ---- Mixer strip + master accumulate ----
    let muted = song_track.muted;
    let solo = song_track.solo;
    // Folder solo: グループを solo したらその子も鳴る (Ableton / Reaper 準拠)。
    // 祖先 group のいずれかが solo なら、 この track 自身が非 solo でも透過させる。
    let ancestor_soloed = song.is_some_and(|s| s.ancestor_soloed(song_track.id));
    let effective_mute = muted || (any_solo && !solo && !ancestor_soloed);
    scratch.effective_mute = effective_mute;

    // Always apply the strip so `track_l/r` carries this track's
    // post-fader signal — even for an excluded track. The master / group
    // mixes drop `effective_mute` tracks via the flag (so the dry is never
    // heard), but keeping the signal lets aux sends into a SOLOED return
    // and sidechain taps still read it: soloing a return then auditions
    // the sends feeding it (Ableton). RT-safe: in-place writes only.
    // Phase 5 Step 5.2: master が当該 buffer の effective bpm を current_bpm
    // として渡す (song = None なら process_buffer 側で 120.0)。
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
        mod_scalars,
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

    // Explicit mute zeroes the output entirely (no dry, no send, no
    // sidechain). Solo-exclusion does NOT zero — the flag already keeps it
    // out of the master / group mix, while the signal stays available for
    // sends / sidechain. Either way an excluded track meters dark.
    if muted {
        scratch.track_l[..n].fill(0.0);
        scratch.track_r[..n].fill(0.0);
    }
    if effective_mute {
        scratch.peak_l = 0.0;
        scratch.peak_r = 0.0;
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

/// master bus の audio fx chain を直列 process する。 全 track が
/// `master_l/r` に mix され終わった後・metronome を重ねる前に呼ばれる。
/// track fx (`process_track_owned` の Audio FX chain 部) と同じ buffer io
/// idiom: plugin は `(MASTER_TRACK_ID, device_index)` keying で worker pool
/// 経由 dispatch し、 in-place で `master_l/r` を上書きする。
///
/// master fx param automation は現状 song に target lane が無いので
/// `fill_pd_param_events` は呼ばない (= 呼んでも no-op、 将来機能)。
/// RT 規約: ヒープ確保 / lock / I/O なし。 buffer は呼び出し側が事前確保した
/// `master_l/r` と plugin 側 ProcessData shmem のみを使う。
#[allow(clippy::too_many_arguments)]
pub fn process_master_fx_chain(
    master_fx_chain: &[common::model::PluginInstance],
    master_l: &mut [f32],
    master_r: &mut [f32],
    plugin_refs: &HashMap<u32, PluginRef>,
    slot_to_plugin_id: &HashMap<(u32, u32), u32>,
    worker_sync: Option<&WorkerSyncRef>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    song: Option<&Song>,
    current_bpm: f32,
    playhead_beats: f64,
    looping: bool,
) {
    let n = frames as usize;
    let Some(ws) = worker_sync else { return };
    for i in 0..master_fx_chain.len() {
        // master は音源境界のない単一 audio FX Vec なので、 device_index は
        // そのまま Vec position。
        let key = (common::model::MASTER_TRACK_ID, i as u32);
        let Some(&plugin_id) = slot_to_plugin_id.get(&key) else {
            continue;
        };
        let Some(plugin_ref) = plugin_refs.get(&plugin_id) else {
            continue;
        };
        let pd = plugin_ref.data_mut();
        pd.prepare();
        pd.frames = frames;
        pd.playing = if playing { 1 } else { 0 };
        pd.sample_rate = sample_rate;
        set_pd_transport(pd, song, current_bpm, playhead_beats, looping);
        pd.buffer_in[0][..n].copy_from_slice(&master_l[..n]);
        pd.buffer_in[1][..n].copy_from_slice(&master_r[..n]);
        if let Err(_e) = ws.dispatch(plugin_id) {
            // RT path: skip on dispatch failure without per-buffer I/O.
            #[cfg(debug_assertions)]
            tracing::error!(error = ?_e, plugin_id, "master fx dispatch failed");
            continue;
        }
        master_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
        master_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
    }
}

/// Resolve a tap `BufRef` (PostFader / PostFx / PreFx) to the source track's
/// `(L, R)` buffers. Returns `None` for a non-tap `BufRef` or out-of-range
/// track. docs/plan_modulation_followups.md §1. RT-safe (pure slicing).
fn resolve_tap_buffers(scratch: &[TrackScratch], src: BufRef) -> Option<(&[f32], &[f32])> {
    Some(match src {
        BufRef::TrackScratch(i) => {
            let s = scratch.get(i as usize)?;
            (s.track_l.as_slice(), s.track_r.as_slice())
        }
        BufRef::PreFaderScratch(i) => {
            let s = scratch.get(i as usize)?;
            (s.pre_fader_l.as_slice(), s.pre_fader_r.as_slice())
        }
        BufRef::PreFxScratch(i) => {
            let s = scratch.get(i as usize)?;
            (s.pre_fx_l.as_slice(), s.pre_fx_r.as_slice())
        }
        _ => return None,
    })
}

/// docs/plan_modulation.md §6 / docs/plan_modulation_followups.md §1: does any
/// aux-input route or mod source tap `track_id` exactly at `want`? Read-only
/// scan, no alloc — RT-safe.
fn any_tap_at(song: &Song, track_id: u32, want: common::model::TapPoint) -> bool {
    let hit = |t: &common::model::AudioTap| t.source_track == track_id && t.tap_point == want;
    song.tracks
        .iter()
        .flat_map(|tr| tr.devices.iter())
        .chain(song.master_fx_chain.iter())
        .flat_map(|p| p.aux_inputs.iter().flatten())
        .any(|r| hit(&r.tap))
        // FIXME #56: generator (LFO/Random/MSEG/Steps) は tap を持たない。 follower のみ走査。
        || song
            .mod_sources
            .iter()
            .filter_map(|m| m.follower())
            .any(|(tap, _)| hit(tap))
}

/// A `PostFx` tap reads the track's `pre_fader_l/r` snapshot (post-fx,
/// pre-strip), so the per-track render must capture it.
fn track_needs_prefader_snapshot(song: &Song, track_id: u32) -> bool {
    any_tap_at(song, track_id, common::model::TapPoint::PostFx)
}

/// A `PreFx` tap reads the track's `pre_fx_l/r` snapshot (the raw signal
/// before the device chain), so the per-track render must capture it.
fn track_needs_prefx_snapshot(song: &Song, track_id: u32) -> bool {
    any_tap_at(song, track_id, common::model::TapPoint::PreFx)
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
    slot_to_plugin_id: &HashMap<(u32, u32), u32>,
    worker_sync: Option<&WorkerSyncRef>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    any_solo: bool,
    playhead: u64,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    current_bpm: f32,
    // group fx の transport snapshot 用 (= 積分済み拍位置 + 実 loop トグル)。
    playhead_beats: f64,
    looping: bool,
) {
    // `nodes` の不変参照と `delay_lines` の可変参照を同時に取りたい
    // (ApplyDelay で line を引きながら nodes を回すため)。 `Schedule`
    // を split borrow で 2 つの参照に分解する。
    let Schedule {
        nodes,
        delay_lines,
        port_buffers: _,
        input_delay_per_track: _,
        follower_slots,
        mod_kinds: _,
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
                dst:
                    BufRef::Pooled(_)
                    | BufRef::PluginAuxOut { .. }
                    | BufRef::PreFaderScratch(_)
                    | BufRef::PreFxScratch(_),
                ..
            } => {
                // PR4: pooled targets and plugin aux-out routing land
                // here once parallel-out support arrives. A Mix into a
                // Pre*Scratch is never emitted (those are written by
                // ProcessTrack), but the arm keeps the match exhaustive.
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
                    playhead_beats,
                    looping,
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
                dst_index,
                aux_in_port,
            } => {
                // PR4 sidechain: copy the source track's scratch L/R into the
                // destination plugin's `pd.buffer_aux_in[port]` shmem region,
                // marking the port active so `daw_plugin_host` forwards it as a
                // CLAP `clap_audio_buffer` / VST3 aux bus on the next
                // `process()`. docs/plan_modulation.md §6: the tap point picks
                // the buffer — `TrackScratch` = post-fader, `PreFaderScratch` =
                // post-fx / pre-fader. Other `BufRef`s are ignored (PR4.4/PR5).
                // RT path: skip silently on any miss (no per-buffer tracing).
                // docs/plan_modulation_followups.md §1: the tap point picks the
                // source buffer — PostFader / PostFx (pre-fader) / PreFx.
                let Some((src_l, src_r)) = resolve_tap_buffers(scratch, *src) else {
                    continue;
                };
                let port = *aux_in_port as usize;
                if port >= common::process_data::MAX_AUX_IN {
                    continue;
                }
                // Resolve the runtime plugin_id for (dst_track, dst_index).
                // v23: the chain map is keyed by (track_id, device_index).
                let key = (*dst_track, *dst_index);
                let Some(&plugin_id) = slot_to_plugin_id.get(&key) else {
                    continue;
                };
                let Some(plugin_ref) = plugin_refs.get(&plugin_id) else {
                    continue;
                };
                let pd = plugin_ref.data_mut();
                let copy_n = n.min(src_l.len()).min(src_r.len());
                pd.buffer_aux_in[port][0][..copy_n].copy_from_slice(&src_l[..copy_n]);
                pd.buffer_aux_in[port][1][..copy_n].copy_from_slice(&src_r[..copy_n]);
                pd.aux_in_active[port] = 1;
            }

            NodeOp::MixSend {
                src,
                dst,
                src_track_idx,
                send_idx,
            } => {
                // PR4 aux send: accumulate the source's post- or pre-fader
                // buffer into the destination return / bus scratch, scaled
                // by the live (optionally automated) send gain.
                let BufRef::TrackScratch(dst_idx) = *dst else {
                    continue;
                };
                let (src_idx, pre_fader) = match *src {
                    BufRef::TrackScratch(i) => (i, false),
                    BufRef::PreFaderScratch(i) => (i, true),
                    _ => continue,
                };
                mix_send_into_track_scratch(
                    scratch,
                    dst_idx as usize,
                    src_idx as usize,
                    pre_fader,
                    song,
                    *src_track_idx,
                    *send_idx,
                    sample_rate,
                    current_bpm,
                    playhead,
                    any_solo,
                    recording_lanes,
                    n,
                );
            }

            NodeOp::EnvelopeFollow { src, slot } => {
                // docs/plan_modulation.md §3/§6: advance this source's envelope
                // follower over its (settled) scratch, picking the buffer by
                // tap point (`TrackScratch` = post-fader, `PreFaderScratch` =
                // post-fx / pre-fader). The smoothed envelope lands in
                // `follower_slots[slot].env`; `process_buffer` publishes it to
                // `AudioBridge::mod_scalars` after this walk. RT-safe: pure
                // arithmetic, no alloc / lock.
                let Some((src_l, src_r)) = resolve_tap_buffers(scratch, *src) else {
                    continue;
                };
                let Some(fs) = follower_slots.get_mut(*slot as usize) else {
                    continue;
                };
                fs.process_block(src_l, src_r, n);
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

/// Accumulate one aux send into a return / bus scratch.
///
/// Reads `scratch[src_idx]`'s post-fader (`track_l/r`) or pre-fader
/// (`pre_fader_l/r`) buffer, scales it by the **live** send gain of
/// `song.tracks[src_track_idx].sends[send_idx]` — sampled per-sample from
/// a `SendGain` automation lane when present (and not being recorded),
/// otherwise the constant `send.gain` — and adds it into
/// `scratch[dst_idx].track_l/r` (`+=`, no clear). A disabled send or a
/// muted source contributes nothing (Ableton: mute silences sends). The
/// gain is read live, never baked into the schedule, so knob drags and
/// `SendGain` automation apply without recompiling.
#[allow(clippy::too_many_arguments)]
fn mix_send_into_track_scratch(
    scratch: &mut [TrackScratch],
    dst_idx: usize,
    src_idx: usize,
    pre_fader: bool,
    song: &Song,
    src_track_idx: u32,
    send_idx: u8,
    sample_rate: u32,
    bpm: f32,
    playhead: u64,
    any_solo: bool,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    n: usize,
) {
    use common::model::{AutomationTarget, TrackBuiltinParam};

    if src_idx == dst_idx || src_idx >= scratch.len() || dst_idx >= scratch.len() {
        return;
    }
    let Some(track) = song.tracks.get(src_track_idx as usize) else {
        return;
    };
    let Some(send) = track.sends.get(send_idx as usize) else {
        return;
    };
    if !send.enabled {
        return;
    }
    // An explicit mute on the source always silences its sends.
    if track.muted {
        return;
    }
    // Solo handling. Soloing a track should let you hear ONLY it and its
    // sends — other tracks' sends must NOT leak into a shared return. So
    // under solo a send flows only if its SOURCE is solo-audible (soloed,
    // or kept alive by a soloed child / send), OR the DESTINATION return is
    // itself explicitly soloed (you soloed the return to audition
    // everything routed to it). The source keeps its signal (see
    // process_track_owned), so the soloed-return audition still works.
    if any_solo {
        let dest_soloed = song.tracks.get(dst_idx).is_some_and(|d| d.solo);
        if !dest_soloed && !track.solo && !has_soloed_contributor(song, track.id) {
            return;
        }
    }

    // Pick this send's `SendGain` automation lane, unless it is currently
    // being recorded (then the live knob value is heard, mirroring the
    // volume / pan recording bypass).
    let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_idx });
    let lane = if recording_lanes.contains(&(track.id, target.clone())) {
        None
    } else {
        track
            .automation_lanes
            .iter()
            .find(|l| l.enabled && l.target == target)
    };
    let samples_per_beat = if bpm > 0.0 && sample_rate > 0 {
        f64::from(sample_rate) * 60.0 / f64::from(bpm)
    } else {
        0.0
    };
    let const_gain = send.gain;

    // Borrow the source immutably and the destination mutably without
    // overlap (`src_idx != dst_idx` checked above).
    let (src_scratch, dst_scratch): (&TrackScratch, &mut TrackScratch) = if src_idx < dst_idx {
        let (left, right) = scratch.split_at_mut(dst_idx);
        (&left[src_idx], &mut right[0])
    } else {
        let (left, right) = scratch.split_at_mut(src_idx);
        (&right[0], &mut left[dst_idx])
    };
    let (src_l, src_r) = if pre_fader {
        (&src_scratch.pre_fader_l, &src_scratch.pre_fader_r)
    } else {
        (&src_scratch.track_l, &src_scratch.track_r)
    };
    let n = n
        .min(src_l.len())
        .min(src_r.len())
        .min(dst_scratch.track_l.len())
        .min(dst_scratch.track_r.len());

    if let (Some(lane), true) = (lane, samples_per_beat > 0.0) {
        for i in 0..n {
            let beat = (playhead + i as u64) as f64 / samples_per_beat;
            let g = common::automation::lane_value_at(lane, &song.clip_contents, beat) as f32;
            dst_scratch.track_l[i] += src_l[i] * g;
            dst_scratch.track_r[i] += src_r[i] * g;
        }
    } else {
        for i in 0..n {
            dst_scratch.track_l[i] += src_l[i] * const_gain;
            dst_scratch.track_r[i] += src_r[i] * const_gain;
        }
    }
}

/// `track_id` に流れ込む (= contribute する) track のいずれかが
/// `solo == true` なら true。 寄与エッジは「子 (`parent_group_id == node`、
/// group の soloed-via-children)」 と「`node` 宛ての aux send を持つ track
/// (= send 元、 return の solo-safe)」 の 2 種。 これで「あるトラックを
/// solo すると、 そのトラックが送っている reverb / delay の **リターン** も
/// 生かす」 Ableton 準拠の挙動になる (リターンを solo-safe にしないと、
/// ソロしたトラックの send 先が solo 規則で無音化され、 ソロ中はセンド
/// エフェクトが聞こえない)。 routing graph は DAG (`compile_schedule` が
/// cycle を弾く) なので BFS は停止する。 `hops` 上限は child + send の
/// fan-in を見込んで広めに取る。
fn has_soloed_contributor(song: &Song, track_id: u32) -> bool {
    // RT-safe non-allocating BFS: this runs on the audio dispatch path, so
    // the frontier and the visited set must live on the stack rather than
    // heap-allocated `Vec`s. `MAX_TRACKS` (= 32) caps the number of distinct
    // nodes; the stack is sized to comfortably hold them. Track ids are not
    // dense indices, so the visited set stores ids directly.
    let mut frontier = [0u32; MAX_TRACKS * 2];
    let mut frontier_len = 0usize;
    let mut visited = [0u32; MAX_TRACKS];
    let mut visited_len = 0usize;

    // Seed with the starting node, marked visited so it is never re-pushed.
    frontier[frontier_len] = track_id;
    frontier_len += 1;
    visited[visited_len] = track_id;
    visited_len += 1;

    while frontier_len > 0 {
        frontier_len -= 1;
        let node = frontier[frontier_len];
        for t in &song.tracks {
            let feeds_node = t.parent_group_id == Some(node)
                || t.sends.iter().any(|s| s.dest_track_id == node);
            if feeds_node {
                if t.solo {
                    return true;
                }
                // Skip already-visited nodes so the fixed-length frontier
                // can never overflow (each distinct node is pushed once).
                if visited[..visited_len].contains(&t.id) {
                    continue;
                }
                if visited_len < visited.len() && frontier_len < frontier.len() {
                    visited[visited_len] = t.id;
                    visited_len += 1;
                    frontier[frontier_len] = t.id;
                    frontier_len += 1;
                }
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
    slot_to_plugin_id: &HashMap<(u32, u32), u32>,
    worker_sync: Option<&WorkerSyncRef>,
    sample_rate: u32,
    playhead: u64,
    frames: u32,
    playing: bool,
    any_solo: bool,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    current_bpm: f32,
    // group fx の transport snapshot (= 積分済み拍位置 + 実 loop トグル)。
    playhead_beats: f64,
    looping: bool,
) {
    let n = frames as usize;
    let track_id = song_track.id;

    // docs/plan_modulation_followups.md §1: a group's pre-FX signal = the summed
    // children before its own device chain. Capture for any PreFx tap / mod
    // source (guarded — untouched groups skip the memcpy).
    if track_needs_prefx_snapshot(song, track_id) {
        scratch.pre_fx_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fx_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }

    // v23 single-chain: a group / return bus has a summed audio input (no
    // sequencer notes), so the chain runs entirely in the audio domain. Walk
    // `devices` once and connect audio ports serially (Reaper 流) — feed the
    // bus signal into any device that takes audio in, dispatch, then write the
    // audio out back (replace when the device has an audio input = effect, add
    // when it has none = pure source). MIDI ports are irrelevant on a bus.
    for i in 0..song_track.devices.len() {
        let ports = song_track.devices[i].ports;
        if !ports.has_audio_output {
            // No audio output (e.g. a pure MIDI effect) — nothing to contribute
            // to a bus signal; skip.
            continue;
        }
        // id ベースの key で lookup (chain は track_id + device_index で識別、
        // song.tracks の Vec position に依存しない)。
        let key = (track_id, i as u32);
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
        set_pd_transport(pd, Some(song), current_bpm, playhead_beats, looping);
        // Phase 2b: group fx 宛 PluginParam automation。
        crate::automation::fill_pd_param_events(
            pd,
            song,
            track_id,
            i as u32,
            sample_rate,
            song.bpm,
            playhead,
            frames,
            recording_lanes,
            // group/master fx の param follower 変調は follow-up (post-dispatch
            // 段でのスナップショット plumbing 未配線)。 track param 変調が主用途。
            &[],
        );
        if ports.has_audio_input {
            pd.buffer_in[0][..n].copy_from_slice(&scratch.track_l[..n]);
            pd.buffer_in[1][..n].copy_from_slice(&scratch.track_r[..n]);
        }
        if let Err(_e) = ws.dispatch(plugin_id) {
            // RT path: skip on dispatch failure without per-buffer I/O.
            #[cfg(debug_assertions)]
            tracing::error!(error = ?_e, plugin_id, "group fx dispatch failed");
            continue;
        }
        if ports.has_audio_input {
            // effect: 入力を処理した結果で bus を置換。
            scratch.track_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
            scratch.track_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
        } else {
            // source: 入力を取らず生成する機 → bus に加算。
            for j in 0..n {
                scratch.track_l[j] += pd.buffer_out[0][j];
                scratch.track_r[j] += pd.buffer_out[1][j];
            }
        }
    }

    // ---- Pre-fader send tap (bus / return source) ----
    // A pre-fader send from this bus reads its post-fx, pre-strip signal.
    if song_track
        .sends
        .iter()
        .any(|s| s.mode == common::model::SendMode::PreFader)
    {
        scratch.pre_fader_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fader_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }

    let muted = song_track.muted;
    let solo = song_track.solo;
    // Live 互換: 子 / send 元のいずれかが solo されていれば、 この bus 自身は
    // solo フラグが無くても透過させる (has_soloed_contributor)。 さらに folder
    // solo: 祖先 group が solo なら、 このネストした group bus 自身も透過させる。
    let effective_mute = muted
        || (any_solo
            && !solo
            && !song.ancestor_soloed(song_track.id)
            && !has_soloed_contributor(song, song_track.id));
    scratch.effective_mute = effective_mute;

    // Always apply the strip (mirrors process_track_owned): keep the signal
    // in `track_l/r` for onward sends / sidechain even when excluded; only
    // an explicit mute zeroes it, and the flag handles master / group
    // exclusion.
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
        // group/master bus の volume/pan follower 変調は follow-up。
        &[],
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
    if muted {
        scratch.track_l[..n].fill(0.0);
        scratch.track_r[..n].fill(0.0);
    }
    if effective_mute {
        scratch.peak_l = 0.0;
        scratch.peak_r = 0.0;
    }
}

/// Phase 7 B3 (2026-05-13): メトロノーム click を 1 buffer 分 master_l/r に
/// 重ねる。 buffer 範囲内の全 beat 境界 (current_bpm + sample_rate から算出)
/// で click voice を trigger、 既存 voice が decay 中なら overwrite (= 短
/// decay 1 voice の業界標準 idiom)。 voice の sample 生成は sine + linear
/// envelope decay で hardcode (decay 40 ms / amp peak 0.25 = -12 dB / freq
/// downbeat 880 Hz, 他 440 Hz)。 stereo は同 sample を L/R に均等 mix
/// (= mono click)。
///
/// RT 安全: heap 確保なし、 浮動小数演算と sin() 呼び出しのみ。 bpm = 0 /
/// sample_rate = 0 / tsig_num < 1 で no-op (defensive)。
///
/// 同 buffer 内に 2 個以上 beat 境界が含まれる場合 (= 高速 tempo / 大 buffer)、
/// 後の trigger が voice を overwrite し前の voice の残響は失われる。 通常
/// 使用範囲 (~600 BPM @ 11.6 ms buffer = 1.16 beat/buffer) では起きない。
#[allow(clippy::too_many_arguments)]
fn render_metronome(
    voice: &mut Option<ClickVoice>,
    master_l: &mut [f32],
    master_r: &mut [f32],
    frames: usize,
    playhead_samples: u64,
    sample_rate: u32,
    bpm: f32,
    tsig_num: i64,
) {
    if frames == 0 || sample_rate == 0 || bpm <= 0.0 || tsig_num < 1 {
        return;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(bpm);
    if samples_per_beat <= 0.0 {
        return;
    }
    let buffer_start = playhead_samples as f64;
    let buffer_end = buffer_start + frames as f64;
    // この buffer 内に含まれる beat 境界 (= sample 位置 = beat_index *
    // samples_per_beat) を順次 trigger。 連続なら最後の trigger が voice を
    // overwrite (KISS: 同 buffer 多重 voice なし)。
    let first_beat_in_buf = (buffer_start / samples_per_beat).ceil() as i64;
    let mut beat_index = first_beat_in_buf.max(0);
    loop {
        let boundary_sample = beat_index as f64 * samples_per_beat;
        if boundary_sample >= buffer_end {
            break;
        }
        if boundary_sample >= buffer_start {
            let buf_offset = (boundary_sample - buffer_start).floor() as u32;
            if (buf_offset as usize) < frames {
                let downbeat = beat_index.rem_euclid(tsig_num) == 0;
                let decay = ((sample_rate as f32) * 0.04) as u32;
                let freq = if downbeat { 880.0 } else { 440.0 };
                *voice = Some(ClickVoice {
                    samples_remaining: decay.max(1),
                    decay_samples: decay.max(1),
                    freq,
                    phase: 0.0,
                    start_offset: buf_offset,
                });
            }
        }
        beat_index += 1;
    }
    // active voice の sample 生成 + mix。 start_offset から frames 末まで
    // sine + linear envelope decay。 voice 終端で None に戻す。
    if let Some(v) = voice.as_mut() {
        let mut i = v.start_offset as usize;
        v.start_offset = 0;
        let two_pi = std::f32::consts::TAU;
        let amp_peak: f32 = 0.25;
        let freq_per_sr = v.freq / sample_rate as f32;
        while i < frames && v.samples_remaining > 0 {
            let env = v.samples_remaining as f32 / v.decay_samples as f32;
            let s = v.phase.sin() * env * amp_peak;
            master_l[i] += s;
            master_r[i] += s;
            v.phase += two_pi * freq_per_sr;
            if v.phase > two_pi {
                v.phase -= two_pi;
            }
            v.samples_remaining -= 1;
            i += 1;
        }
        if v.samples_remaining == 0 {
            *voice = None;
        }
    }
}

#[cfg(test)]
mod sidechain_tests {
    use super::*;
    use crate::graph::compile_schedule;
    use common::model::{PluginInstance, Song, Track};
    use common::plugin_format::PluginFormat;

    /// v23 single-chain: `Track::default()` を mutator で埋める helper。 downstream
    /// crate (daw_audio) の test で `Track { .., ..Track::default() }` を書くと、
    /// `common` 内の `pub(crate)` legacy migration fields が見えず E0451 になる
    /// ため、 private field に触れない default + mutate で回避する。
    fn track(f: impl FnOnce(&mut Track)) -> Track {
        let mut t = Track::default();
        f(&mut t);
        t
    }

    #[test]
    fn set_pd_transport_uses_real_beats_and_loop_toggle() {
        // SSoT 回帰防止: pd.song_pos_beats は daw_audio が渡す積分済み拍位置を
        // そのまま使い (= samples × bpm の逆算ではない)、 pd.looping は実 loop
        // トグルを反映する (= region 有無 heuristic ではない)。
        let song = Song {
            time_sig: (3, 4),
            loop_start_beat: 4.0,
            loop_end_beat: 8.0,
            ..Song::default()
        };
        let mut pd = common::process_data::ProcessData::empty();
        // playhead_beats = 12.5 は constant-tempo 逆算とは無関係な「真の拍」。
        set_pd_transport(&mut pd, Some(&song), 90.0, 12.5, true);
        assert_eq!(pd.song_pos_beats, 12.5);
        assert_eq!(pd.bpm, 90.0);
        assert_eq!(pd.tsig_num, 3);
        assert_eq!(pd.tsig_denom, 4);
        assert_eq!(pd.loop_start_beats, 4.0);
        assert_eq!(pd.loop_end_beats, 8.0);
        assert_eq!(pd.looping, 1);
        // loop region は定義済のまま looping=false を渡すと pd.looping=0
        // (= region heuristic を使っていれば 1 のままになる、 という回帰検出)。
        set_pd_transport(&mut pd, Some(&song), 90.0, 12.5, false);
        assert_eq!(pd.looping, 0);
    }

    /// PR4 Sidechain engine-handler test: 実 plugin を立てなくても、
    /// `execute_schedule_post_dispatch` の `NodeOp::SidechainTap` ハンドラ
    /// が source TrackScratch の signal を `pd.buffer_aux_in[port]` に正しく
    /// copy することを直接検証する。 ProcessData は heap に Box で置き、
    /// `PluginRef` を手書きして plugin_refs / slot_to_plugin_id に登録する。
    #[test]
    fn sidechain_tap_copies_source_track_into_plugin_aux_in_buffer() {
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Source".into();
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Dest".into();
                    // v23 single-chain: an audio-FX device (audio_output only,
                    // no note input) → derives as AudioEffect at device 0.
                    t.devices = vec![PluginInstance {
                        aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                        ..PluginInstance::with_ports(
                            "test.scc".into(),
                            PluginFormat::Vst3,
                            common::port_config::PortConfig {
                                has_note_input: false,
                                has_note_output: false,
                                has_audio_output: true,
                                // audio-FX device: audio を加工する → audio 入力あり。
                                has_audio_input: true,
                            },
                        )
                    }];
                }),
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

        let mut slot_to_plugin_id: HashMap<(u32, u32), u32> = HashMap::new();
        slot_to_plugin_id.insert((2, 0), plugin_id);

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
            0.0,
            false,
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

#[cfg(test)]
mod send_tests {
    use super::*;
    use common::model::{Send, SendMode, Song, Track};

    /// v23 single-chain: `Track::default()` を mutator で埋める helper
    /// (`sidechain_tests::track` と同趣旨、 E0451 回避)。
    fn track(f: impl FnOnce(&mut Track)) -> Track {
        let mut t = Track::default();
        f(&mut t);
        t
    }

    const FRAMES: usize = 64;

    fn song_with_send(gain: f32, mode: SendMode, enabled: bool) -> Song {
        Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Vocal".into();
                    t.sends = vec![Send {
                        dest_track_id: 2,
                        gain,
                        mode,
                        enabled,
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Reverb".into();
                }),
            ],
            ..Song::default()
        }
    }

    fn empty_lanes() -> std::collections::HashSet<(u32, common::model::AutomationTarget)> {
        std::collections::HashSet::new()
    }

    /// A post-fader send accumulates `src * gain` into the return scratch
    /// **on top of** whatever is already there (the prior clearing Mix is
    /// a separate op), reading the source's post-fader `track_l/r`.
    #[test]
    fn post_fader_send_accumulates_src_times_gain() {
        let song = song_with_send(0.5, SendMode::PostFader, true);
        let mut scratch: Vec<TrackScratch> = (0..4).map(|_| TrackScratch::new()).collect();
        for i in 0..FRAMES {
            scratch[0].track_l[i] = (i as f32) * 0.1;
            scratch[0].track_r[i] = -(i as f32) * 0.1;
            scratch[1].track_l[i] = 1.0; // pre-existing return content
            scratch[1].track_r[i] = 2.0;
        }
        let empty = empty_lanes();
        mix_send_into_track_scratch(
            &mut scratch, 1, 0, false, &song, 0, 0, 48_000, 120.0, 0, false, &empty, FRAMES,
        );
        for i in 0..FRAMES {
            let want_l = 1.0 + (i as f32) * 0.1 * 0.5;
            let want_r = 2.0 + (-(i as f32) * 0.1) * 0.5;
            assert!((scratch[1].track_l[i] - want_l).abs() < 1e-6, "l[{i}]");
            assert!((scratch[1].track_r[i] - want_r).abs() < 1e-6, "r[{i}]");
        }
    }

    /// A disabled send contributes nothing (per-send mute).
    #[test]
    fn disabled_send_contributes_silence() {
        let song = song_with_send(0.5, SendMode::PostFader, false);
        let mut scratch: Vec<TrackScratch> = (0..4).map(|_| TrackScratch::new()).collect();
        for i in 0..FRAMES {
            scratch[0].track_l[i] = 1.0;
            scratch[1].track_l[i] = 3.0;
        }
        let empty = empty_lanes();
        mix_send_into_track_scratch(
            &mut scratch, 1, 0, false, &song, 0, 0, 48_000, 120.0, 0, false, &empty, FRAMES,
        );
        for i in 0..FRAMES {
            assert_eq!(scratch[1].track_l[i], 3.0, "disabled send must not change dst");
        }
    }

    /// An *explicitly* muted source silences its sends.
    #[test]
    fn explicitly_muted_source_send_contributes_silence() {
        let mut song = song_with_send(1.0, SendMode::PostFader, true);
        song.tracks[0].muted = true; // explicit mute kills the send
        let mut scratch: Vec<TrackScratch> = (0..4).map(|_| TrackScratch::new()).collect();
        for i in 0..FRAMES {
            scratch[0].track_l[i] = 1.0;
            scratch[1].track_l[i] = 3.0;
        }
        let empty = empty_lanes();
        mix_send_into_track_scratch(
            &mut scratch, 1, 0, false, &song, 0, 0, 48_000, 120.0, 0, false, &empty, FRAMES,
        );
        for i in 0..FRAMES {
            assert_eq!(
                scratch[1].track_l[i], 3.0,
                "explicitly muted source must not feed its send"
            );
        }
    }

    /// Under solo, a send must respect BOTH the source's and the
    /// destination's solo state: soloing one source must not leak other
    /// tracks' sends into a shared return, but soloing the return itself
    /// auditions everything routed to it.
    #[test]
    fn send_under_solo_respects_source_and_return_solo() {
        let render = |solo_src: bool, solo_dest: bool| -> f32 {
            let mut song = song_with_send(1.0, SendMode::PostFader, true);
            song.tracks[0].solo = solo_src; // Vocal (source)
            song.tracks[1].solo = solo_dest; // Reverb return (dest)
            let mut scratch: Vec<TrackScratch> =
                (0..4).map(|_| TrackScratch::new()).collect();
            scratch[0].track_l[0] = 0.5;
            let empty = empty_lanes();
            mix_send_into_track_scratch(
                &mut scratch, 1, 0, false, &song, 0, 0, 48_000, 120.0, 0, true, &empty, FRAMES,
            );
            scratch[1].track_l[0]
        };
        // A soloed source still feeds its own send.
        assert!(
            (render(true, false) - 0.5).abs() < 1e-6,
            "a soloed source still feeds its send"
        );
        // Neither the source audible nor the return soloed → blocked, so
        // soloing one track does not leak other tracks' sends.
        assert_eq!(
            render(false, false),
            0.0,
            "a non-audible source must not leak into the return"
        );
        // Return explicitly soloed → audition: the send flows even from a
        // non-soloed source.
        assert!(
            (render(false, true) - 0.5).abs() < 1e-6,
            "soloing the return auditions the sends feeding it"
        );
    }

    /// A pre-fader send reads the source's `pre_fader_l/r`, not its
    /// post-fader `track_l/r`.
    #[test]
    fn pre_fader_send_reads_pre_fader_buffer() {
        let song = song_with_send(1.0, SendMode::PreFader, true);
        let mut scratch: Vec<TrackScratch> = (0..4).map(|_| TrackScratch::new()).collect();
        for i in 0..FRAMES {
            scratch[0].track_l[i] = 9.0; // post-fader — must be ignored
            scratch[0].track_r[i] = 9.0;
            scratch[0].pre_fader_l[i] = 0.25; // pre-fader — must be used
            scratch[0].pre_fader_r[i] = 0.5;
        }
        let empty = empty_lanes();
        mix_send_into_track_scratch(
            &mut scratch, 1, 0, true, &song, 0, 0, 48_000, 120.0, 0, false, &empty, FRAMES,
        );
        for i in 0..FRAMES {
            assert!(
                (scratch[1].track_l[i] - 0.25).abs() < 1e-6,
                "pre-fader send must read pre_fader_l"
            );
            assert!((scratch[1].track_r[i] - 0.5).abs() < 1e-6);
        }
    }

    /// Solo-safe returns: when a track that aux-sends into a return is
    /// soloed, the return must count as having a soloed contributor so the
    /// solo rule keeps it audible instead of muting it. Regression for the
    /// user-reported "soloed track's send reaches the FX, but the return
    /// fader meter is dead and there is no sound".
    #[test]
    fn soloed_send_source_keeps_return_solo_safe() {
        // song_with_send: Vocal (id 1) post-fader sends to Reverb (id 2).
        let mut song = song_with_send(1.0, SendMode::PostFader, true);
        song.tracks[0].solo = true; // solo the send SOURCE (Vocal)
        assert!(
            has_soloed_contributor(&song, 2),
            "Reverb return must be solo-safe when its send source is soloed"
        );
        // Nothing soloed → the return has no soloed contributor.
        song.tracks[0].solo = false;
        assert!(
            !has_soloed_contributor(&song, 2),
            "with nothing soloed, the return has no soloed contributor"
        );
    }

    /// Folder solo: soloing a GROUP must keep its children audible (Ableton /
    /// Reaper folder behavior). The leaf strip rule excludes a non-soloed
    /// track under solo only when no ancestor group is soloed, so a child of
    /// a soloed group is NOT effective-muted. Guards the `ancestor_soloed`
    /// condition added to the effective-mute formula.
    #[test]
    fn soloed_group_keeps_children_audible() {
        // id 10 = group, id 11 = child of 10, id 12 = unrelated.
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 10;
                    t.solo = true;
                }), // solo the group
                track(|t| {
                    t.id = 11;
                    t.parent_group_id = Some(10);
                }),
                track(|t| t.id = 12),
            ],
            ..Default::default()
        };

        let any_solo = song.tracks.iter().any(|t| t.solo);
        assert!(any_solo);
        // child: not soloed itself, but its ancestor group is → audible.
        assert!(song.ancestor_soloed(11), "child sees the soloed ancestor group");
        let child = &song.tracks[1];
        let child_excluded = any_solo && !child.solo && !song.ancestor_soloed(child.id);
        assert!(!child_excluded, "child of a soloed group must not be solo-excluded");
        // unrelated track: no soloed ancestor → excluded (silent) under solo.
        let other = &song.tracks[2];
        let other_excluded = any_solo && !other.solo && !song.ancestor_soloed(other.id);
        assert!(other_excluded, "unrelated track is silenced while a group is soloed");
    }
}
