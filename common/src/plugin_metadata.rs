//! Builtin plugin (`PluginFormat::Builtin`) 用の per-note metadata。
//!
//! 外部 CLAP / VST3 plugin の規格には「note ごとの追加情報」 (= 歌詞、
//! phoneme 等) を載せる方法が公式には無い (CLAP の `NoteExpression` /
//! VST3 の `IEvent NoteExpressionType` は数値専用)。 daw_01 内蔵 plugin
//! (= VOICEVOX) はこれを必要とするので、 `LoadedPlugin::set_note_metadata`
//! の引数として「note_id → metadata」 を渡す**専用経路**を builtin
//! plugin だけ持たせる (CLAP / VST3 plugin は default no-op)。
//!
//! `note_id` は audio engine が plugin に渡す MIDI events の note 識別子
//! と一致させる必要がある (= clip 内 note index を共通鍵にする)。 PR-V2
//! 完了時点では daw_audio の MIDI events に note_id field がまだ無い
//! ので、 PR-V2.4 で daw_audio 側に note_id を伝播する経路を追加する。
//!
//! `lyric` は VOICEVOX の `singing_query` API が要求する「1 note = 1
//! 音節」 の歌詞。 通常 1 文字 (例: `あ`)、 `っ` など促音は前 note の
//! lyric に内包する。 詳しくは
//! `%APPDATA%\\REAPER\\Scripts\\yoshino\\voicevox\\` 参照。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// One per-note metadata entry. Currently only carries `lyric` for
/// VOICEVOX; future builtins (formant shifters, vibrato controllers,
/// etc.) can add fields here without changing the IPC variant — bincode
/// struct expansion is backward compatible as long as new fields have
/// `#[serde(default)]` and a meaningful `Default` impl.
#[derive(
    Debug, Clone, PartialEq, Eq, Encode, Decode, Serialize, Deserialize, Default,
)]
pub struct NoteMetadata {
    /// MIDI note identifier shared between the audio engine's
    /// `TimedNoteEvent` stream and the host-side metadata flush. PR-V2.1
    /// uses the clip-internal note index (= position in `Clip.notes`)
    /// as the value; PR-V2.4 wires this through the audio path.
    pub note_id: u32,
    /// Lyric character for VOICEVOX `singing_query`. Empty string =
    /// "use the previous note's lyric" (= sustained tail) or "no
    /// lyric" depending on context. Other builtins ignore this.
    pub lyric: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bincode_roundtrip() {
        let m = NoteMetadata {
            note_id: 42,
            lyric: "あ".to_string(),
        };
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&m, cfg).unwrap();
        let (decoded, _): (NoteMetadata, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(decoded, m);
    }

    #[test]
    fn default_is_empty() {
        let m = NoteMetadata::default();
        assert_eq!(m.note_id, 0);
        assert!(m.lyric.is_empty());
    }
}
