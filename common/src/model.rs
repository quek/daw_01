use std::path::PathBuf;

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

pub const CURRENT_VERSION: u32 = 1;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Track {
    pub name: String,
    pub source: InstrumentSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fx_chain: Vec<PluginInstance>,
    pub volume: f32,
    pub pan: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<Clip>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum InstrumentSource {
    Vocal { speaker_id: u32, style_name: String },
    Clap { path: PathBuf, plugin_id: String },
    Vst3 { path: PathBuf },
    BuiltinSynth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct PluginInstance {
    pub path: PathBuf,
    pub plugin_id: String,
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
                fx_chain: vec![],
                volume: 1.0,
                pan: 0.0,
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
            }],
            ..Song::default()
        };
        assert_eq!(json_roundtrip(&song), song);
    }
}
