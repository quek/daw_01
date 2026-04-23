use bincode::{Decode, Encode};

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
    /// Plugin-host confirmed `SetSlotPlugin` and reported the stable id /
    /// display name of the descriptor that actually loaded.
    SlotPluginLoaded {
        track: u32,
        slot: PluginSlot,
        id: String,
        name: String,
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
    // --- Per-track plugin slot management -----------------------------
    /// Load / replace the plugin in `(track, slot)`. Empty `plugin_id`
    /// picks the first descriptor in `path`; non-empty selects by id.
    /// `initial_state`, when `Some`, is applied via
    /// `clap_plugin_state.load` right after activate.
    SetSlotPlugin {
        track: u32,
        slot: PluginSlot,
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
