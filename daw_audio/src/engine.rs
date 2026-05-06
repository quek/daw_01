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

use crate::audio_worker::AudioWorkerPool;
use crate::graph::{BufRef, NodeOp, Schedule, compile_schedule};
use crate::mixer::TrackScratch;
use crate::sequencer::{NoteTransition, TimedNoteEvent, collect_events_for_buffer};
use crate::vocal::VocalAudio;

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
    /// graph at the matching position.
    OpenPluginShmem {
        plugin_id: u32,
        plugin_ref: PluginRef,
        track: u32,
        slot: PluginSlot,
    },
    /// Drop a previously-opened plugin shmem mapping. Triggered on
    /// RemoveSlotPlugin / RemoveTrack from the GUI side.
    ClosePluginShmem { plugin_id: u32 },
    /// Pre-rendered vocal audio for a single clip on a track. Hot-swapped
    /// via `ArcSwapOption` so the audio thread picks up new samples on
    /// the next buffer without restarting.
    SetVocalAudio {
        track: u32,
        clip_start_samples: u64,
        samples: Vec<f32>,
    },
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
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            song: ArcSwapOption::empty(),
            playback: AtomicU8::new(PlaybackCommand::Stop as u8),
            looping: AtomicBool::new(false),
            playhead: AtomicU64::new(0),
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
    /// Per-track pre-rendered vocal audio (VOICEVOX results). The outer
    /// map is snapshotted on add/remove; the inner `ArcSwapOption` is
    /// hot-swapped on `SetVocalAudio` so re-synthesis lands on the next
    /// buffer.
    pub vocal_store: ArcSwap<HashMap<u32, Arc<ArcSwapOption<VocalAudio>>>>,
    /// Worker pool that fans per-track work across N audio-engine
    /// workers. `None` until `OpenWorkerPool` arrives.
    pub worker_pool: ArcSwapOption<AudioWorkerPool>,
    /// Set by A3's export thread while it owns the audio path. CPAL
    /// callback skips its `process_buffer` and writes silence so the
    /// export render can drive `plugin.process()` exclusively.
    pub export_running: AtomicBool,
}

impl EngineShared {
    pub fn new() -> Self {
        Self {
            worker_bridge: ArcSwapOption::empty(),
            worker_syncs: ArcSwap::from_pointee(Vec::new()),
            plugin_refs: ArcSwap::from_pointee(HashMap::new()),
            slot_to_plugin_id: ArcSwap::from_pointee(HashMap::new()),
            vocal_store: ArcSwap::from_pointee(HashMap::new()),
            worker_pool: ArcSwapOption::empty(),
            export_running: AtomicBool::new(false),
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
            cmd_rx,
            shared,
            cached_schedule: Schedule::empty(),
            cached_song: None,
            #[cfg(debug_assertions)]
            last_heartbeat_playhead: 0,
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
                AudioCommand::SetVocalAudio {
                    track,
                    clip_start_samples,
                    samples,
                } => {
                    let new_audio = Arc::new(VocalAudio {
                        clip_start_samples,
                        samples,
                    });
                    // Reuse an existing per-track ArcSwapOption if we
                    // have one (so any clone the audio path is holding
                    // sees the new sample), else mint one and publish a
                    // new outer map snapshot.
                    let cur = self.shared.vocal_store.load();
                    if let Some(slot) = cur.get(&track) {
                        slot.store(Some(new_audio));
                    } else {
                        let mut new_map: HashMap<u32, Arc<ArcSwapOption<VocalAudio>>> =
                            (**cur).clone();
                        let slot = Arc::new(ArcSwapOption::empty());
                        slot.store(Some(new_audio));
                        new_map.insert(track, slot);
                        self.shared.vocal_store.store(Arc::new(new_map));
                    }
                    tracing::info!(track, "vocal audio updated");
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
                shared.playhead.store(0, Ordering::Release);
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

        if let Some(song) = song_ref {
            let any_solo = song.tracks.iter().any(|t| t.solo);
            let n_tracks = song.tracks.len().min(MAX_TRACKS);

            // Snapshot the wait-free shared state once for this buffer.
            // Guards stay live until the end of the call so the workers
            // can safely deref them via the publish pointers.
            let plugin_refs_g = self.shared.plugin_refs.load();
            let slot_map_g = self.shared.slot_to_plugin_id.load();
            let vocal_store_g = self.shared.vocal_store.load();
            let worker_syncs_g = self.shared.worker_syncs.load();
            let pool_g = self.shared.worker_pool.load();

            // Fan the per-track work out across the audio worker pool
            // when one is bound; otherwise fall back to serial dispatch
            // through `worker_syncs[0]` (still correct, just slower).
            if let Some(pool) = pool_g.as_deref() {
                pool.dispatch_and_wait(
                    Some(song),
                    &mut self.scratch[..n_tracks],
                    &plugin_refs_g,
                    &slot_map_g,
                    &vocal_store_g,
                    &worker_syncs_g,
                    &mut self.master_l[..n],
                    &mut self.master_r[..n],
                    sample_rate,
                    playhead,
                    n as u32,
                    playing,
                    any_solo,
                );
            } else {
                let worker_sync = worker_syncs_g.first();
                for track_idx in 0..n_tracks {
                    let song_track = &song.tracks[track_idx];
                    let scratch = &mut self.scratch[track_idx];
                    let track_id = song_track.id;
                    let vocal = vocal_store_g.get(&track_id);
                    process_track_owned(
                        track_idx as u32,
                        song_track,
                        scratch,
                        &plugin_refs_g,
                        &slot_map_g,
                        vocal,
                        worker_sync,
                        sample_rate,
                        playhead,
                        n as u32,
                        playing,
                        Some(song),
                        any_solo,
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
                &self.cached_schedule,
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
                    let track_peaks: Vec<(f32, f32, bool)> = self
                        .scratch
                        .iter()
                        .take(n_tracks)
                        .map(|s| (s.peak_l, s.peak_r, s.effective_mute))
                        .collect();
                    let plugin_ids: Vec<u32> = plugin_refs_g.keys().copied().collect();
                    let slot_keys: Vec<((u32, PluginSlot), u32)> =
                        slot_map_g.iter().map(|(k, v)| (*k, *v)).collect();
                    tracing::info!(
                        playing,
                        playhead,
                        master_peak,
                        ?track_peaks,
                        ?plugin_ids,
                        ?slot_keys,
                        n_workers = worker_syncs_g.len(),
                        worker_pool = pool_g.is_some(),
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
                } else {
                    self.playing = false;
                    shared
                        .playback
                        .store(PlaybackCommand::Stop as u8, Ordering::Release);
                }
            }
            shared.playhead.store(new_ph, Ordering::Release);
        }
    }
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
#[allow(clippy::too_many_arguments)]
pub fn process_track_owned(
    track_idx: u32,
    song_track: &Track,
    scratch: &mut TrackScratch,
    plugin_refs: &HashMap<u32, PluginRef>,
    slot_to_plugin_id: &HashMap<(u32, PluginSlot), u32>,
    vocal: Option<&Arc<ArcSwapOption<VocalAudio>>>,
    worker_sync: Option<&WorkerSyncRef>,
    sample_rate: u32,
    playhead: u64,
    frames: u32,
    playing: bool,
    song: Option<&Song>,
    any_solo: bool,
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
        scratch.midi_bus_a.push(TimedNoteEvent {
            time: 0,
            event: NoteTransition::Off { key: k },
        });
    }
    scratch.state.pending_offs.clear();
    if playing {
        collect_events_for_buffer(
            song,
            track_idx,
            sample_rate,
            playhead,
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
        for ev in &scratch.midi_bus_a {
            match ev.event {
                NoteTransition::On { key, velocity } => {
                    pd.push_note_on(ev.time, key, velocity, 0)
                }
                NoteTransition::Off { key } => pd.push_note_off(ev.time, key, 0),
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
                        key: ev.key,
                        velocity: ev.velocity,
                    },
                },
                EventKind::NoteOff => TimedNoteEvent {
                    time: ev.time,
                    event: NoteTransition::Off { key: ev.key },
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

    // ---- Vocal: pre-rendered samples for tracks without an instrument ----
    // VOICEVOX-style sample playback. The audio engine reads the
    // hot-swapped buffer from `vocal_store` directly into the track
    // scratch. Tracks that have an instrument plugin skip this — the
    // instrument is the audio source.
    if song_track.instrument.is_none() && playing
        && let Some(slot) = vocal
        && let Some(vocal_audio) = slot.load().as_deref()
        && !vocal_audio.samples.is_empty()
    {
        let buf_start = playhead;
        for i in 0..n {
            let abs_sample = buf_start + i as u64;
            if abs_sample >= vocal_audio.clip_start_samples {
                let v_idx = (abs_sample - vocal_audio.clip_start_samples) as usize;
                if v_idx < vocal_audio.samples.len() {
                    let s = vocal_audio.samples[v_idx];
                    scratch.track_l[i] = s;
                    scratch.track_r[i] = s;
                }
            }
        }
    }

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
            for ev in &scratch.midi_bus_a {
                match ev.event {
                    NoteTransition::On { key, velocity } => {
                        pd.push_note_on(ev.time, key, velocity, 0)
                    }
                    NoteTransition::Off { key } => pd.push_note_off(ev.time, key, 0),
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
    let volume = song_track.volume;
    let pan = song_track.pan.clamp(-1.0, 1.0);
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
        let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        let gain_l = angle.cos() * volume;
        let gain_r = angle.sin() * volume;
        let mut peak_l = 0.0_f32;
        let mut peak_r = 0.0_f32;
        // Apply the strip in-place so the master reducer can accumulate
        // raw track samples. No global write here — race-free.
        for i in 0..n {
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
fn execute_schedule_post_dispatch(
    schedule: &Schedule,
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
) {
    for op in &schedule.nodes {
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
                    track,
                    song,
                    target,
                    plugin_refs,
                    slot_to_plugin_id,
                    worker_sync,
                    sample_rate,
                    frames,
                    playing,
                    any_solo,
                );
            }
            NodeOp::ApplyDelay { .. } => {
                // PR3.
            }
            NodeOp::SidechainTap { .. } => {
                // PR4.
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
    song_track: &Track,
    song: &Song,
    scratch: &mut TrackScratch,
    plugin_refs: &HashMap<u32, PluginRef>,
    slot_to_plugin_id: &HashMap<(u32, PluginSlot), u32>,
    worker_sync: Option<&WorkerSyncRef>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    any_solo: bool,
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
        pd.buffer_in[0][..n].copy_from_slice(&scratch.track_l[..n]);
        pd.buffer_in[1][..n].copy_from_slice(&scratch.track_r[..n]);
        if let Err(e) = ws.dispatch(plugin_id) {
            tracing::error!(error = ?e, plugin_id, "group fx dispatch failed");
            continue;
        }
        scratch.track_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
        scratch.track_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
    }

    let volume = song_track.volume;
    let pan = song_track.pan.clamp(-1.0, 1.0);
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
        let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        let gain_l = angle.cos() * volume;
        let gain_r = angle.sin() * volume;
        let mut peak_l = 0.0_f32;
        let mut peak_r = 0.0_f32;
        for i in 0..n {
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
