use bincode::{Decode, Encode};

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
    /// Sent after `create` + `show` succeeds. Carries the initial size the
    /// plugin wants for its embedded window so daw_gui can resize the host
    /// container.
    GuiOpened {
        width: u32,
        height: u32,
    },
    /// Plugin-initiated resize via `clap_host_gui.request_resize`. daw_gui
    /// should resize the container and ack with `MainToChild::ResizeGui`.
    GuiRequestResize {
        width: u32,
        height: u32,
    },
    /// Plugin-initiated close (window X button handled by the plugin, or
    /// `clap_host_gui.closed`). daw_gui should drop its embed HWND.
    GuiClosed,
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
    SetClapPlugin(std::path::PathBuf),
    SetLoop(bool),
    SetMasterGain(f32),
    /// Request the plugin to create + embed + show its GUI as a child of the
    /// given Win32 HWND (serialized as `u64` since HWND isn't directly
    /// `Encode`-able). daw_plugin_host replies with `ChildToMain::GuiOpened`.
    OpenGuiEmbedded {
        host_hwnd: u64,
    },
    /// Tear down the plugin GUI (hide + destroy).
    CloseGui,
    /// Tell the plugin "the container was resized to W×H, update your UI".
    /// Sent after daw_gui resizes the host container in response to
    /// `GuiRequestResize` or a user drag.
    ResizeGui {
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
