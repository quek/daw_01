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
    /// Offline WAV export finished (or failed).
    ExportWavComplete {
        error: Option<String>,
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
    /// Per-track mixer parameter changes. plugin_host applies each to the
    /// matching audio-thread-visible `TrackAudioParams` atomic without
    /// stopping the audio thread.
    /// Pre-rendered vocal audio for a single clip on a track. `samples`
    /// is mono f32, `sample_rate` matches `AudioSession::sample_rate` (or
    /// is resampled by the host). `clip_start_samples` is the absolute
    /// sample offset within the song where this clip begins.
    /// Offline-render the entire song to a WAV file. plugin_host runs the
    /// render on the plugin-main thread (audio thread stopped), then
    /// replies with `ChildToMain::ExportWavComplete`.
    ExportWav {
        path: std::path::PathBuf,
    },
    SetVocalAudio {
        track: u32,
        clip: u32,
        clip_start_samples: u64,
        sample_rate: u32,
        samples: Vec<f32>,
    },
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
    /// slot), tearing down each plugin's GUI first. Sent when the user
    /// removes a whole track so the audio thread stops rendering it.
    RemoveTrack { track: u32 },
    /// Reorder the chains map: swap the plugin chains / mixer params /
    /// vocal audio between `a` and `b`. The audio thread sees the new
    /// arrangement on the next buffer.
    SwapTracks { a: u32, b: u32 },
    /// Apply a multi-track reorder in **one** `tracks.mutate` call (i.e. one
    /// audio-thread stop/start cycle). `order[i]` is the previous index of
    /// the track that should end up at the new position `i`. Used by the
    /// arrangement widget's drag-and-drop reorder so multiple `SwapTracks`
    /// commands don't thrash the audio thread.
    ReorderTracks(Vec<u32>),
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
