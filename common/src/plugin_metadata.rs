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
//! と一致させる必要がある。両者は [`sing_note_id`] という **同じ式** で
//! `(clip_id, note.id)` から決定論的に導出する (r.md #75。旧「track 内 note
//! 通し index」は片方だけ数え方がずれると壊れる欠陥があった)。
//!
//! `lyric` は VOICEVOX の `singing_query` API が要求する「1 note = 1
//! 音節」 の歌詞。 通常 1 文字 (例: `あ`)、 `っ` など促音は前 note の
//! lyric に内包する。 詳しくは
//! `%APPDATA%\\REAPER\\Scripts\\<user>\\voicevox\\` (作者ローカルの REAPER
//! 参照実装) を参照。

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
    ///
    /// 値は [`sing_note_id`]`(clip_id, note.id)` — **安定 id** (アーキ不変条件 1)。
    /// daw_gui の `sync_vocal_metadata` と daw_audio の `sequencer` が
    /// **同じ関数**で同じ値を作るので、clip の追加 / 削除 / 並べ替え / muted で
    /// 番号がずれない。旧実装は「track 内 note 通し index」で、両者が独立に
    /// 数え直していたため、クリップ先頭に 1 音足すと以降の全 note_id がずれた。
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
    /// Stable `Clip::id` (track 内一意) of the clip this note belongs to。
    ///
    /// 用途は 2 つ: (1) [`sing_note_id`] の導出元、(2) builtin が合成進捗を
    /// **クリップ単位で報告**するための帰属情報
    /// (`protocol::VocalSynthProgress::pending_clips`)。
    /// **グルーピングには使わない** — 合成の単位はフレーズ (= 隙間ゼロで続く note の
    /// 極大列) で、クリップ境界では切らない。Other builtins ignore this.
    #[serde(default)]
    pub clip_id: u32,
    /// Per-clip VOICEVOX singing voice = `/frame_synthesis`
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
    /// `talk_event_id(clip_id, event_index)`。sing の `note_id` 空間
    /// (= [`sing_note_id`]、`[0, TALK_EVENT_ID_BASE)`) と衝突しない high band に置く。
    /// sequencer の note_on と builtin の `note_offsets` がこの id で対応する。
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
    /// Stable `Clip::id` (track 内一意) of the Text clip this utterance belongs to。
    /// `event_id` の導出元 ([`talk_event_id`]) であり、合成進捗のクリップ帰属
    /// (`protocol::VocalSynthProgress::pending_clips`) にも使う。
    /// `event_id` から割り算で復元はしない (`talk_event_id` は `saturating_*` を
    /// 含むので逆関数が全域では正しくない。持っている情報を捨てない)。
    #[serde(default)]
    pub clip_id: u32,
}

/// (talk) `event_id` の high band 起点。sing の `note_id`
/// (= [`sing_note_id`] の値域 `[0, 1 << 28)`) の**直上**。両者は定義上ちょうど
/// 接する (`MAX_CLIPS_PER_TRACK_FOR_NOTE_ID * MAX_NOTES_PER_CLIP == 1 << 28`)。
pub const TALK_EVENT_ID_BASE: u32 = 1 << 28;
/// (talk) 1 clip あたりの最大 TextEvent 数 (`event_id` 導出の基数)。
pub const MAX_TEXT_EVENTS_PER_CLIP: u32 = 4096;

/// (sing) 1 clip あたりの最大 note 数 ([`sing_note_id`] の基数)。
pub const MAX_NOTES_PER_CLIP: u32 = 16_384;
/// (sing) 1 track あたりの最大 clip 数 ([`sing_note_id`] の基数)。
/// 積が [`TALK_EVENT_ID_BASE`] にちょうど収まる値。
pub const MAX_CLIPS_PER_TRACK_FOR_NOTE_ID: u32 = 16_384;

/// (sing) `(clip_id, note.id)` から決定論的に `note_id` を導出する。
///
/// flush (daw_gui `sync_vocal_metadata`) と再生トリガ (daw_audio `sequencer`) が
/// **同じ式**で計算するので、「クリップ先頭に 1 音足すと以降の全 note_id がずれる」
/// という旧「トラック内通し index」の欠陥が構造的に消える (アーキ不変条件 1)。
///
/// 値域は `[0, TALK_EVENT_ID_BASE)` に**必ず**収まる (= talk の high band を侵さない)。
/// clip / note が基数を超えた場合は剰余で畳むので、極端な project では 2 note が同じ
/// id を共有し得る (= 停止中プレビューがもう一方の位置から鳴る)。再生・書き出しには
/// 影響しない縮退で、現実的な曲では起きない。
#[must_use]
pub fn sing_note_id(clip_id: u32, note_id: u32) -> u32 {
    (clip_id % MAX_CLIPS_PER_TRACK_FOR_NOTE_ID) * MAX_NOTES_PER_CLIP
        + (note_id % MAX_NOTES_PER_CLIP)
}

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
        // sing note_id (= `sing_note_id` の値域) とは重ならない。
        assert!(talk_event_id(0, 0) > 100_000);
    }

    #[test]
    fn sing_note_ids_stay_below_talk_band() {
        // 値域の上界が talk band の直下に収まる (= 剰余で必ず畳まれる)。
        assert!(sing_note_id(u32::MAX, u32::MAX) < TALK_EVENT_ID_BASE);
        assert!(sing_note_id(0, 0) < TALK_EVENT_ID_BASE);
        // 定義上ちょうど接する。
        assert_eq!(
            MAX_CLIPS_PER_TRACK_FOR_NOTE_ID * MAX_NOTES_PER_CLIP,
            TALK_EVENT_ID_BASE
        );
    }

    #[test]
    fn sing_note_id_is_stable_against_sibling_insertion() {
        // 同じ `(clip_id, note.id)` なら他 note の増減に依らず同値
        // (旧「通し index」はここで全部ずれていた)。
        assert_eq!(sing_note_id(3, 7), sing_note_id(3, 7));
        assert_ne!(sing_note_id(3, 7), sing_note_id(3, 8));
        assert_ne!(sing_note_id(3, 7), sing_note_id(4, 7));
        // clip が違えば note.id が同じでも別 id。
        assert_eq!(sing_note_id(0, 1), 1);
        assert_eq!(sing_note_id(1, 1), MAX_NOTES_PER_CLIP + 1);
    }

    #[test]
    fn sing_and_talk_id_spaces_do_not_overlap() {
        for clip in [0u32, 1, 5, 4095, 16_383] {
            for note in [0u32, 1, 16_383] {
                assert!(sing_note_id(clip, note) < TALK_EVENT_ID_BASE);
            }
            for ev in [0u32, 1, 4095] {
                assert!(talk_event_id(clip, ev) >= TALK_EVENT_ID_BASE);
            }
        }
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
            clip_id: 3,
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
