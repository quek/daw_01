use std::path::PathBuf;

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

pub const CURRENT_VERSION: u32 = 1;

/// Serde adapter for `Option<Vec<u8>>` that writes binary data as base64 in
/// JSON (and other human-readable formats). Bincode bypasses this and uses
/// native length-prefixed bytes via the `Encode`/`Decode` derives.
pub mod base64_opt {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        bytes: &Option<Vec<u8>>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(b) => ser.serialize_some(&STANDARD.encode(b)),
            None => ser.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        de: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        let s: Option<String> = Option::deserialize(de)?;
        match s {
            Some(s) => STANDARD
                .decode(s.as_bytes())
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectFile {
    pub version: u32,
    pub song: Song,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Song {
    pub bpm: f32,
    pub time_sig: (u8, u8),
    pub length_beats: f64,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

impl Default for Song {
    fn default() -> Self {
        Self {
            bpm: 120.0,
            time_sig: (4, 4),
            length_beats: 64.0,
            tracks: Vec::new(),
        }
    }
}

/// A track owns a full CLAP signal chain in three sections:
///
/// 1. `midi_fx_chain` — note-effect plugins (arpeggiator / quantizer / ...)
///    processed in order, piping out_events into the next plugin's in_events.
/// 2. `instrument` — the note→audio plugin (receives the MIDI FX output).
///    `None` when the track has no instrument yet.
/// 3. `fx_chain` — audio-effect plugins (compressor / reverb / ...) applied
///    to the instrument's audio output in order.
///
/// Clips on the track feed the MIDI FX chain at the top of the buffer. The
/// final audio is mixed into the master bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Track {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument: Option<PluginInstance>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub midi_fx_chain: Vec<PluginInstance>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fx_chain: Vec<PluginInstance>,
    pub volume: f32,
    pub pan: f32,
    /// Future use: VOICEVOX speaker / style etc. Kept distinct from the
    /// `instrument` slot because it selects a rendering backend, not a CLAP
    /// plugin.
    #[serde(default)]
    pub source: InstrumentSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<Clip>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum InstrumentSource {
    #[default]
    None,
    Vocal { speaker_id: u32, style_name: String },
    Vst3 { path: PathBuf },
    BuiltinSynth,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            name: String::new(),
            instrument: None,
            midi_fx_chain: Vec::new(),
            fx_chain: Vec::new(),
            volume: 1.0,
            pan: 0.0,
            source: InstrumentSource::None,
            clips: Vec::new(),
        }
    }
}

/// Reference to a CLAP plugin loaded on a track, with the state blob the
/// plugin itself produced via `clap_plugin_state.save`. Paths are NOT
/// stored — the plugin-id is resolved to a path through
/// `plugin_db::PluginDatabase` at load time, keeping projects portable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct PluginInstance {
    pub plugin_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "base64_opt"
    )]
    pub state: Option<Vec<u8>>,
}

impl PluginInstance {
    pub fn new(plugin_id: String) -> Self {
        Self {
            plugin_id,
            state: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Clip {
    pub name: String,
    pub start_beat: f64,
    pub length_beats: f64,
    pub rows_per_beat: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct Row {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<NoteEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fx: Vec<FxCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lyric: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum NoteEvent {
    On(Note),
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct Note {
    pub key: u8,
    pub velocity: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct FxCommand {
    pub cmd: u8,
    pub value: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_roundtrip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(value).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn song_default_roundtrip() {
        let song = Song::default();
        assert_eq!(json_roundtrip(&song), song);
    }

    #[test]
    fn project_file_roundtrip() {
        let pf = ProjectFile {
            version: CURRENT_VERSION,
            song: Song::default(),
        };
        assert_eq!(json_roundtrip(&pf), pf);
    }

    #[test]
    fn empty_row_serializes_as_empty_object() {
        assert_eq!(serde_json::to_string(&Row::default()).unwrap(), "{}");
    }

    #[test]
    fn note_serializes_compactly() {
        let note = Note {
            key: 60,
            velocity: 100,
        };
        assert_eq!(
            serde_json::to_string(&note).unwrap(),
            r#"{"key":60,"velocity":100}"#
        );
    }

    #[test]
    fn note_event_on_roundtrip() {
        let event = NoteEvent::On(Note {
            key: 60,
            velocity: 100,
        });
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"On":{"key":60,"velocity":100}}"#
        );
        assert_eq!(json_roundtrip(&event), event);
    }

    #[test]
    fn note_event_off_roundtrip() {
        let event = NoteEvent::Off;
        assert_eq!(serde_json::to_string(&event).unwrap(), r#""Off""#);
        assert_eq!(json_roundtrip(&event), event);
    }

    #[test]
    fn vocal_clip_roundtrip() {
        let song = Song {
            tracks: vec![Track {
                name: "Vocal".into(),
                source: InstrumentSource::Vocal {
                    speaker_id: 3,
                    style_name: "ノーマル".into(),
                },
                clips: vec![Clip {
                    name: "こんにちは".into(),
                    start_beat: 0.0,
                    length_beats: 16.0,
                    rows_per_beat: 4,
                    rows: vec![
                        Row {
                            note: Some(NoteEvent::On(Note {
                                key: 60,
                                velocity: 100,
                            })),
                            lyric: Some("こ".into()),
                            ..Default::default()
                        },
                        Row::default(),
                        Row {
                            note: Some(NoteEvent::Off),
                            ..Default::default()
                        },
                    ],
                }],
                ..Track::default()
            }],
            ..Song::default()
        };
        assert_eq!(json_roundtrip(&song), song);
    }
}
