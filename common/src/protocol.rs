use bincode::{Decode, Encode};

use crate::plugin_format::PluginFormat;

/// Addresses a single plugin slot inside a track. A track has:
/// - MIDI FX chain: `MidiFx(0)`, `MidiFx(1)`, ...
/// - one Instrument slot: `Instrument`
/// - audio FX chain: `Fx(0)`, `Fx(1)`, ...
///
/// Indices within `MidiFx` / `Fx` are stable while the chain is unchanged;
/// explicit `MoveSlot` messages rewrite them after a reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub enum PluginSlot {
    MidiFx(u32),
    Instrument,
    Fx(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub enum ChildKind {
    Audio,
    PluginHost,
}

/// CLAP `render` extension mode. Sent to the plugin host via
/// `MainToChild::SetRenderMode` so it can call
/// `clap_plugin_render.set` on every loaded plugin.
///
/// `Realtime` is the default — plugins should optimise for low latency.
/// `Offline` is set during WAV export so plugins free to use higher
/// quality / non-realtime algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub enum RenderMode {
    Realtime,
    Offline,
}

impl ChildKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChildKind::Audio => "audio",
            ChildKind::PluginHost => "plugin_host",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum ChildToMain {
    Hello {
        kind: ChildKind,
        pid: u32,
    },
    /// Offline WAV export finished (or failed). Sent by daw_audio when
    /// the export thread finalises (or hits an error). `error == None`
    /// means the WAV file at the requested path is fully written.
    ExportWavComplete {
        error: Option<String>,
    },
    /// Offline plugin-FX bounce finished (or failed). Mirror of
    /// `ExportWavComplete` but for `MainToChild::BounceClipFxOnline`.
    /// `frames` is the actual number of frames written to the WAV
    /// (= `end_frame - start_frame` plus tail silence kept).
    /// `source_track` / `source_clip` echo the request so the host can
    /// look up which clip the result belongs to.
    BounceClipFxComplete {
        path: std::path::PathBuf,
        source_track: u32,
        source_clip: u32,
        error: Option<String>,
        frames: u64,
    },
    /// Plugin-host confirmed `SetSlotPlugin` and reported the stable id /
    /// display name of the descriptor that actually loaded.
    /// `plugin_id` is the host's session-unique identifier for this
    /// instance; `shmem_id` names the `ProcessData` shared memory the
    /// host created so daw_audio can `OpenShared` it and use the
    /// worker-pool dispatch to drive `plugin.process()`.
    SlotPluginLoaded {
        track: u32,
        slot: PluginSlot,
        id: String,
        name: String,
        plugin_id: u32,
        shmem_id: String,
    },
    /// Reply to `RequestSlotState`. `None` = plugin unavailable or state
    /// extension missing.
    SlotPluginState {
        track: u32,
        slot: PluginSlot,
        data: Option<Vec<u8>>,
    },
    /// Reply to `RequestAllStates`: one entry per slot that had a plugin
    /// loaded at request time. Makes project save a single round-trip.
    AllPluginStates {
        entries: Vec<SlotState>,
    },
    /// GUI opened at the requested size.
    SlotGuiOpened {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    /// Plugin-initiated resize via `clap_host_gui.request_resize`.
    SlotGuiRequestResize {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    /// Plugin-initiated close (X button handled by plugin, or `closed`).
    SlotGuiClosed {
        track: u32,
        slot: PluginSlot,
    },
    /// Plugin host destroyed a plugin instance (RemoveSlotPlugin /
    /// RemoveTrack 経由)。 daw_gui はこれを受け取って
    /// `MainToChild::ClosePluginShmem { plugin_id }` を daw_audio に
    /// 転送し、 audio engine の `plugin_refs` / `slot_to_plugin_id`
    /// から stale entry を消す。 これを送らないと audio thread が
    /// destroy 済 plugin に process() を呼んで「VST3 plugin not
    /// processing」 エラー / deadlock を起こす。
    SlotPluginUnloaded {
        plugin_id: u32,
    },
    /// `SetSlotPlugin` の load が失敗した。 daw_gui は
    /// `pending_plugin_loads` から該当 entry を解放し、 `pending_play`
    /// が立っていれば flush する (= 「失敗 = 完了」 と等価扱いで Play
    /// queue を解放)。 song の slot は touch せず、 旧 plugin が居れば
    /// 継続。 reason は plugin host 側 `tracing::error!` 相当の文字列
    /// (例: "library load failed: ABI mismatch")。
    SlotPluginLoadFailed {
        track: u32,
        slot: PluginSlot,
        plugin_id: String,
        reason: String,
    },
    /// Plugin が報告した自身の processing latency (samples 単位、 host
    /// sample_rate)。 PR3 PDC pipeline の最終段で、 plugin が active 化
    /// した直後 (CLAP `activate` 完了直後 / VST3 `setActive(true)` 完了
    /// 直後) もしくは plugin が `host->request_restart()` /
    /// `IComponentHandler::restartComponent(kLatencyChanged)` で再 query
    /// を要求して deactivate→activate→get の往復を完了した直後に発火。
    /// daw_gui は plugin_id から (track_id, slot) を逆引きして
    /// `Track::reported_latency_samples` を更新し、 daw_audio に
    /// `LoadSong` を再送して compile_schedule に PDC を再計算させる。
    PluginLatencyChanged {
        plugin_id: u32,
        samples: u32,
    },
}

/// Single entry in the `AllPluginStates` reply.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub struct SlotState {
    pub track: u32,
    pub slot: PluginSlot,
    pub data: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct AudioSession {
    pub shmem_id: String,
    pub request_sem_id: String,
    pub ready_sem_id: String,
    pub sample_rate: u32,
    pub max_frames: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum MainToChild {
    Ack,
    Play,
    Stop,
    Session(AudioSession),
    LoadSong(crate::model::Song),
    SetLoop(bool),
    SetMasterGain(f32),
    /// Offline-render the entire song to a WAV file. Sent to daw_audio,
    /// which freewheels through the song using its existing AudioWorker
    /// pool + plugin handshake, then replies with
    /// `ChildToMain::ExportWavComplete`.
    ExportWav {
        path: std::path::PathBuf,
    },
    /// Offline-render a clip range with the **full plugin chain** (= post-FX)
    /// to a WAV file. Used by `Bounce (with FX)` (`docs/plan_audio_clip
    /// .md` §3.8). Same freewheel pipeline as `ExportWav` but the WAV
    /// captures only frames in `[start_frame, end_frame)` (plus tail
    /// silence cutoff). The render walks the song from frame 0 so plugin
    /// state at `start_frame` is fully accumulated (= reverb tails /
    /// parameter ramps / sidechain history are correct). The host sends
    /// this with `SetRenderMode(Offline)` bookended around it. The audio
    /// engine replies with `ChildToMain::BounceClipFxComplete`.
    ///
    /// `source_track` / `source_clip` are echoed back in the completion
    /// reply so the host can resolve which clip the freshly-written WAV
    /// belongs to (= multiple bounces in flight not supported in M1, but
    /// the field structure leaves room for it).
    BounceClipFxOnline {
        path: std::path::PathBuf,
        source_track: u32,
        source_clip: u32,
        start_frame: u64,
        end_frame: u64,
    },
    /// Builtin plugin (`PluginFormat::Builtin`) に per-note metadata を
    /// flush する。 daw_gui が plugin_id 別に、 「note_id → metadata」 の
    /// 一括 vector を送る。 daw_plugin_host だけが consume (= daw_audio
    /// は ignore)。 CLAP / VST3 plugin に対する flush は trait の default
    /// no-op 実装で吸収するので、 plugin format をホスト側で気にする必要
    /// はない (`docs/plan_voicevox_synth.md` PR-V2.2)。
    SetBuiltinPluginNoteMetadata {
        plugin_id: u32,
        bpm: f32,
        entries: Vec<crate::plugin_metadata::NoteMetadata>,
    },
    /// Tell the plugin host to switch every loaded plugin's CLAP render
    /// mode (Realtime ↔ Offline). The audio side bookends an export
    /// with `Offline` / `Realtime` so plugins that implement the CLAP
    /// `render` extension can pick higher-quality algorithms during
    /// export and revert afterwards.
    SetRenderMode(RenderMode),
    /// Reposition the audio engine's playhead. Sent by daw_gui when the
    /// user clicks/drags the arrangement ruler. `samples` is the absolute
    /// frame offset from the start of the song at the engine sample rate.
    /// Takes effect on the next audio buffer regardless of `playing`
    /// state, so click-to-seek works both while stopped and during
    /// playback.
    SeekTo { samples: u64 },
    // PR-V4: `MainToChild::SetGeneratedAudio` 削除済。 旧 path で
    // VOICEVOX 合成結果を audio engine に流していたが、 builtin
    // instrument plugin (`PluginFormat::Builtin`) が plugin host 内で
    // 合成を完結させるため不要に。
    /// Tell the audio engine the current project directory so it can
    /// resolve `AudioSourcePath::ProjectRelative` entries against
    /// `<project_dir>/samples/<...>`. `None` for unsaved projects —
    /// in that state any `ProjectRelative` AudioSource fails to load
    /// and the corresponding clip plays silence with a "missing
    /// source" badge in the GUI. Spec: §9.2.
    SetProjectDir(Option<std::path::PathBuf>),
    SetTrackVolume { track: u32, volume: f32 },
    SetTrackPan { track: u32, pan: f32 },
    SetTrackMuted { track: u32, muted: bool },
    SetTrackSolo { track: u32, solo: bool },
    // --- Per-track plugin slot management -----------------------------
    /// Load / replace the plugin in `(track, slot)`. `format` routes the
    /// request to the CLAP or VST3 backend. Empty `plugin_id` picks the
    /// first descriptor in `path`; non-empty selects by id (CLAP stable id
    /// or VST3 FUID as hex). `initial_state`, when `Some`, is applied via
    /// the backend's state-restore entry right after activate.
    SetSlotPlugin {
        track: u32,
        slot: PluginSlot,
        format: PluginFormat,
        path: std::path::PathBuf,
        plugin_id: String,
        initial_state: Option<Vec<u8>>,
    },
    /// Remove the plugin at `(track, slot)` if any.
    RemoveSlotPlugin {
        track: u32,
        slot: PluginSlot,
    },
    /// Reorder: move the plugin at `(track, from)` to `(track, to)`. Only
    /// valid within the same section (`MidiFx → MidiFx`, `Fx → Fx`).
    MoveSlot {
        track: u32,
        from: PluginSlot,
        to: PluginSlot,
    },
    /// Drop the entire chain for `track` (every MIDI FX / Instrument / FX
    /// slot), tearing down each plugin's GUI first. `track` is a stable
    /// `Track::id` (since PR2.1 the plugin host's chain map is keyed by
    /// id, not Vec position). Sent when the user removes a whole track
    /// so the audio thread stops rendering it.
    RemoveTrack { track: u32 },
    /// Ask the plugin_host to capture state for one slot. Reply is
    /// `ChildToMain::SlotPluginState`.
    RequestSlotState {
        track: u32,
        slot: PluginSlot,
    },
    /// Ask the plugin_host to capture state for every slot at once.
    /// Reply is `ChildToMain::AllPluginStates` containing one entry per
    /// loaded plugin. Used for project save.
    RequestAllStates,
    // --- GUI management ----------------------------------------------
    OpenSlotGuiEmbedded {
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
    // --- A2 audio engine refactor -------------------------------------
    /// Stand up the per-buffer plugin process worker pool. `n_workers`
    /// audio-engine workers will pair 1:1 with `n_workers` plugin-host
    /// workers via the named events listed (one wake + one done per
    /// pair). The `WorkerBridge` shmem published under
    /// `worker_bridge_shmem_id` carries the per-worker `worker_task`
    /// (plugin id) the audio side writes before each dispatch.
    OpenWorkerPool {
        n_workers: u32,
        worker_bridge_shmem_id: String,
        wake_event_names: Vec<String>,
        done_event_names: Vec<String>,
    },
    /// Tear down the worker pool started by `OpenWorkerPool`. Plugin-host
    /// workers exit and the IDs they held become invalid.
    CloseWorkerPool,
    /// Map a `ProcessData` shmem region into the consumer (daw_audio).
    /// `track` and `slot` let the audio engine slot the plugin into the
    /// right place in its routing graph; `plugin_id` is the host's
    /// session-unique id and `shmem_id` names the shmem the host
    /// already created.
    OpenPluginShmem {
        plugin_id: u32,
        shmem_id: String,
        track: u32,
        slot: PluginSlot,
    },
    /// Drop the `ProcessData` mapping for `plugin_id` after the plugin
    /// instance is being torn down.
    ClosePluginShmem { plugin_id: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(msg: &T) -> T
    where
        T: Encode + Decode<()>,
    {
        let config = bincode::config::standard();
        let bytes = bincode::encode_to_vec(msg, config).unwrap();
        let (decoded, _) = bincode::decode_from_slice(&bytes, config).unwrap();
        decoded
    }

    #[test]
    fn child_kind_as_str() {
        assert_eq!(ChildKind::Audio.as_str(), "audio");
        assert_eq!(ChildKind::PluginHost.as_str(), "plugin_host");
    }

    #[test]
    fn child_to_main_hello_roundtrip() {
        let msg = ChildToMain::Hello {
            kind: ChildKind::Audio,
            pid: 12345,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn main_to_child_ack_roundtrip() {
        let msg = MainToChild::Ack;
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn main_to_child_play_stop_roundtrip() {
        assert_eq!(roundtrip(&MainToChild::Play), MainToChild::Play);
        assert_eq!(roundtrip(&MainToChild::Stop), MainToChild::Stop);
    }
}
