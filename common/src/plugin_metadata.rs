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

/// One per-note metadata entry. Carries the **complete sing-mode note
/// description** (= start_beat / duration_beats / pitch / velocity) plus
/// the lyric character. Builtin plugins use this to drive their own
/// synthesis (= VOICEVOX builds a `singing_query` payload from these);
/// CLAP / VST3 plugins ignore the entire metadata flush via the
/// `LoadedPlugin::set_note_metadata` default no-op.
///
/// Fields mirror `common::model::Note` deliberately rather than
/// embedding it: `plugin_metadata` is meant to stay model-agnostic so
/// it can be hosted out-of-tree as a public plugin SDK in PR-V5
/// (external VST3 build).
#[derive(
    Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize, Default,
)]
pub struct NoteMetadata {
    /// MIDI note identifier shared between the audio engine's
    /// `TimedNoteEvent` stream and the host-side metadata flush.
    /// daw_gui uses the clip-internal note index (= position in
    /// `Clip.notes` *of the same content*) as the value, so an audio
    /// engine event with `note_id == 7` is the 8th note of whichever
    /// clip is currently playing on this track.
    pub note_id: u32,
    /// Note start in beats relative to the song timeline (NOT clip-
    /// relative). Builtins use this to compute frame offsets for the
    /// synthesis output buffer.
    pub start_beat: f64,
    /// Note length in beats.
    pub duration_beats: f64,
    /// MIDI pitch (0–127). `0` = unpitched (= talk-mode lyric).
    pub pitch: u8,
    /// MIDI velocity (0–127).
    pub velocity: u8,
    /// Lyric character for VOICEVOX `singing_query`. Typically one
    /// mora ("あ", "い", "っ"). Empty string = "use the previous
    /// note's lyric" (= sustained tail) or "no lyric" depending on
    /// context. Other builtins ignore this.
    pub lyric: String,
    /// (FIXME #36) Stable `Clip::id` (track 内一意) of the clip this note
    /// belongs to. The VOICEVOX builtin groups the flushed metadata by
    /// `clip_id` so each clip is synthesised with its own `speaker_id`
    /// (= per-clip voice), then concatenates the per-clip WAVs into one
    /// buffer. Other builtins ignore this.
    #[serde(default)]
    pub clip_id: u32,
    /// (FIXME #36) Per-clip VOICEVOX singing voice = `/frame_synthesis`
    /// style id (from `/singers`). `0` = unset → the builtin falls back to
    /// `common::voicevox::DEFAULT_SINGER_ID`. All notes of one clip carry
    /// the same value. Other builtins ignore this.
    #[serde(default)]
    pub speaker_id: u32,
}

/// (talk) 1 件の読み上げ (= `ClipContent::Text` の 1 `TextEvent`) を builtin VOICEVOX
/// に渡すメタデータ (`docs/plan_voicevox_talk.md` §3.2)。`NoteMetadata` の talk 版。
/// builtin はこれを `event_id` ごとに talk 合成 (`/audio_query` → `/synthesis`) し、
/// `start_beat` の song-absolute 位置へ配置する。再生は sequencer が `event_id` を
/// note_id とする合成 note_on を `start_beat` で発火してトリガする。
#[derive(Debug, Clone, PartialEq, Encode, Decode, Serialize, Deserialize, Default)]
pub struct TalkMetadata {
    /// `talk_event_id(clip_id, event_index)`。sing の `note_id` 空間 (= 通し index、
    /// 小さい値) と衝突しない high band に置く。sequencer の note_on と builtin の
    /// `note_offsets` がこの id で対応する。
    pub event_id: u32,
    /// 読み上げ開始位置 (song-absolute beat = `clip.start_beat +
    /// event_start_in_clip_beats`)。
    pub start_beat: f64,
    /// 読み上げるテキスト (= `TextEvent.text`、表示字幕と同一文字列)。
    pub text: String,
    /// talk style speaker id (`/speakers`)。`0` = 未設定 → builtin が既定 talk
    /// speaker にフォールバック。
    pub speaker_id: u32,
    /// 全体スケール (`TalkParams`)。`audio_query` 応答に patch される。
    pub speed_scale: f32,
    pub pitch_scale: f32,
    pub intonation_scale: f32,
    pub volume_scale: f32,
}

/// (talk) `event_id` の high band 起点。sing の `note_id` (= track 内 note 通し
/// index、現実的に < 数万) と衝突しないよう十分高くする。
pub const TALK_EVENT_ID_BASE: u32 = 1 << 28;
/// (talk) 1 clip あたりの最大 TextEvent 数 (`event_id` 導出の基数)。
pub const MAX_TEXT_EVENTS_PER_CLIP: u32 = 4096;

/// (talk) `(clip_id, event_index)` から決定論的に `event_id` を導出する。flush
/// (daw_gui) と再生トリガ (daw_audio sequencer) が**同じ式**で計算するので、running
/// counter の skip 計数を同期させる必要がない (= §8 の id-space リスクを構造的に解消)。
/// `clip_id` は track 内一意で安定 (`Clip::id`)、`event_index` は clip 内の TextEvent
/// 位置。`saturating_*` で overflow を防ぐ (現実的な範囲では衝突しない)。
#[must_use]
pub fn talk_event_id(clip_id: u32, event_index: u32) -> u32 {
    TALK_EVENT_ID_BASE
        .saturating_add(clip_id.saturating_mul(MAX_TEXT_EVENTS_PER_CLIP))
        .saturating_add(event_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn talk_event_ids_are_unique_and_above_base() {
        let a = talk_event_id(1, 0);
        let b = talk_event_id(1, 1);
        let c = talk_event_id(2, 0);
        assert!(a >= TALK_EVENT_ID_BASE);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        // sing note_id (= 小さい通し index) とは重ならない。
        assert!(talk_event_id(0, 0) > 100_000);
    }

    #[test]
    fn talk_metadata_bincode_roundtrip() {
        let m = TalkMetadata {
            event_id: talk_event_id(3, 2),
            start_beat: 8.0,
            text: "こんにちは".to_string(),
            speaker_id: 3,
            speed_scale: 1.2,
            pitch_scale: 0.0,
            intonation_scale: 1.0,
            volume_scale: 1.0,
        };
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&m, cfg).unwrap();
        let (decoded, _): (TalkMetadata, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(decoded, m);
    }

    #[test]
    fn bincode_roundtrip() {
        let m = NoteMetadata {
            note_id: 42,
            start_beat: 4.0,
            duration_beats: 0.25,
            pitch: 60,
            velocity: 100,
            lyric: "あ".to_string(),
            clip_id: 7,
            speaker_id: 3061,
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
        assert_eq!(m.start_beat, 0.0);
        assert_eq!(m.duration_beats, 0.0);
        assert_eq!(m.pitch, 0);
        assert_eq!(m.velocity, 0);
        assert!(m.lyric.is_empty());
        assert_eq!(m.clip_id, 0);
        assert_eq!(m.speaker_id, 0);
    }
}
