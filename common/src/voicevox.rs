//! VOICEVOX — HTTP を持たない共有部分 (config const / Phoneme 型 / sing query builder /
//! urlencoding / 歌詞のモーラ分割)。
//!
//! arch-refactor S5-2 で reqwest を要する HTTP を 2 プロセスへ分離した:
//! - `/singers` `/speakers` fetch + phoneme query (口パク) = **daw_gui**
//!   (`daw_gui::voicevox_client`)
//! - `frame_synthesis` / `synthesis` による音声合成 = **daw_plugin_host**
//!   (`daw_plugin_host::builtin::voicevox_synth`)
//!
//! ここに残るのは reqwest 非依存の純粋部分だけ。`build_sing_query` / `urlencoding_encode` /
//! `QUERY_SPEAKER` / `Phoneme` / `FRAME_RATE` は client (daw_gui) と synth (plugin_host) の
//! **双方が共用** するため common に置く (lipsync / project も参照)。

use crate::model::Note;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Engine REST API endpoint。voicevox_engine module (daw_gui) からも参照する。
pub const VOICEVOX_URL: &str = "http://localhost:50021";

/// sing_frame_audio_query の query 生成に使う speaker (query generation only — 実 singer は
/// frame_synthesis で選ぶ)。6000 = 波音リツ、REAPER 参照と同じ。client (query_phonemes) と
/// synth (sing_query_to_wav) が共用。
pub const QUERY_SPEAKER: u32 = 6000;
/// frame_synthesis の既定 singer (声未指定時)。3061 = 中国うさぎ ノーマル。
pub const DEFAULT_SINGER_ID: u32 = 3061;
/// (talk) `/audio_query` + `/synthesis` の既定 talk speaker (Text clip 声未指定時)。
/// 歌唱 sing style とは別 id 空間 (`/speakers`)。3 = ずんだもん ノーマル。
pub const DEFAULT_TALK_SPEAKER_ID: u32 = 3;
/// `DEFAULT_SINGER_ID` の表示用キャラ名 / スタイル名 (声未設定・旧 project migration 用)。
pub const DEFAULT_SINGER_NAME: &str = "中国うさぎ";
pub const DEFAULT_STYLE_NAME: &str = "ノーマル";
/// VOICEVOX frame rate (24000 Hz / 256 samples)。口パク (`crate::lipsync`) の frame_length →
/// beat 変換、talk phoneme の秒 → frame 変換 (daw_gui client) でも参照する。
pub const FRAME_RATE: f64 = 93.75;
/// Silence frames prepended/appended to every sing query so the synth engine has room for
/// attack/release envelopes. 口パクの先頭 pau lead-in オフセットでも同じ値を使い、音声と口を揃える。
pub const REST_FRAMES: u32 = 10;

// ---------------------------------------------------------------------------
// Phoneme (lip-sync 用、docs/plan_pakupaku.md §5)
// ---------------------------------------------------------------------------

/// VOICEVOX が返す 1 phoneme とその長さ (VOICEVOX frame 単位、`FRAME_RATE` = 93.75fps)。
/// `phoneme` は `"a"`/`"i"`/.../`"N"`/`"cl"`/`"pau"` や子音記号。client (daw_gui) が
/// `sing_frame_audio_query` / `audio_query` 応答から組み、`crate::lipsync` が beat へ配置する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phoneme {
    pub phoneme: String,
    pub frame_length: u32,
}

// ---------------------------------------------------------------------------
// Query builder (sing) — client (query_phonemes) と synth (sing_query_to_wav) が共用
// ---------------------------------------------------------------------------

/// Builds the JSON body for `POST /sing_frame_audio_query`.
///
/// Notes are converted to a flat sequence of `{key, frame_length, lyric}` entries with
/// `key=null` rests inserted between any two notes that don't touch. The first note's
/// `start_beat` becomes `frame 0` of the query — VOICEVOX renders relative to the first
/// non-rest entry, not relative to the song timeline, so anything before the first note is
/// ignored.
pub fn build_sing_query(notes: &[Note], bpm: f32) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Leading rest (gives the synth a moment of silence for the attack).
    parts.push(format!(
        r#"{{"id":"rest_start","key":null,"frame_length":{},"lyric":""}}"#,
        REST_FRAMES
    ));

    // Sort notes by start_beat — `ClipContent.notes` is unordered by contract, and this
    // builder requires monotonic timing.
    let mut sorted: Vec<&Note> = notes
        .iter()
        .filter(|n| n.duration_beats > 0.0 && n.pitch > 0)
        .collect();
    sorted.sort_by(|a, b| {
        a.start_beat
            .partial_cmp(&b.start_beat)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if sorted.is_empty() {
        parts.push(format!(
            r#"{{"id":"rest_end","key":null,"frame_length":{},"lyric":""}}"#,
            REST_FRAMES
        ));
        return format!(r#"{{"notes":[{}]}}"#, parts.join(","));
    }

    let seconds_per_beat = 60.0 / f64::from(bpm);
    let base_beat = sorted[0].start_beat;
    let mut prev_end_frame: i64 = 0;

    for (i, note) in sorted.iter().enumerate() {
        let start_sec = (note.start_beat - base_beat) * seconds_per_beat;
        let end_sec =
            (note.start_beat + note.duration_beats - base_beat) * seconds_per_beat;
        let start_frame = seconds_to_frames(start_sec);
        let end_frame = seconds_to_frames(end_sec);

        // Gap between previous note's end and this note's start → rest.
        if i > 0 {
            let gap = start_frame - prev_end_frame;
            if gap > 0 {
                parts.push(format!(
                    r#"{{"id":"rest{}","key":null,"frame_length":{},"lyric":""}}"#,
                    i, gap
                ));
            }
        }

        let note_frames = (end_frame - start_frame).max(1);
        let lyric = note.lyric.as_deref().unwrap_or("ら");
        let escaped = lyric.replace('\\', "\\\\").replace('"', "\\\"");
        parts.push(format!(
            r#"{{"id":"note{}","key":{},"frame_length":{},"lyric":"{}"}}"#,
            i, note.pitch, note_frames, escaped
        ));

        prev_end_frame = end_frame;
    }

    // Trailing rest
    parts.push(format!(
        r#"{{"id":"rest_end","key":null,"frame_length":{},"lyric":""}}"#,
        REST_FRAMES
    ));

    format!(r#"{{"notes":[{}]}}"#, parts.join(","))
}

fn seconds_to_frames(s: f64) -> i64 {
    (s * FRAME_RATE).round().max(1.0) as i64
}

// ---------------------------------------------------------------------------
// Helpers (client / synth 共用)
// ---------------------------------------------------------------------------

/// Minimal URL-encoding for query parameters (talk の `/audio_query?text=` で使う)。
/// client (query_talk_phonemes) と synth (synthesize_talk) が共用。
pub fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 歌詞のモーラ分割
// ---------------------------------------------------------------------------

/// 歌詞テキストをモーラ単位の Vec に分割する。
///
/// VOICEVOX sing API は「1 note = 1 モーラ」 を前提とするため、 ユーザー入力
/// 「あいうえ」 を 4 note に分配する用途や、 入力検証用途に使う。
///
/// **ルール**:
///
/// - 基本: 1 char = 1 モーラ
/// - **小書き仮名** (ぁぃぅぇぉ ゃゅょ っ ァィゥェォ ャュョ ッ) は **直前の char と結合**
///   して 1 モーラ。 例: "きゃ" は 1 モーラ、 "しゅんかん" は 4 モーラ ("しゅ" / "ん"
///   / "か" / "ん")
/// - 行頭が小書き仮名 (= 結合先が無い) の場合は単独で 1 モーラ
/// - ASCII / 漢字 / 空白は 1 char = 1 モーラ (拗音判定はせず passthrough)
pub fn split_into_morae(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ch in text.chars() {
        if is_small_kana(ch) && !out.is_empty() {
            out.last_mut().unwrap().push(ch);
        } else {
            out.push(ch.to_string());
        }
    }
    out
}

fn is_small_kana(ch: char) -> bool {
    matches!(
        ch,
        // ひらがな小書き
        'ぁ' | 'ぃ' | 'ぅ' | 'ぇ' | 'ぉ' | 'ゃ' | 'ゅ' | 'ょ' | 'っ' | 'ゎ'
        // カタカナ小書き
        | 'ァ' | 'ィ' | 'ゥ' | 'ェ' | 'ォ' | 'ャ' | 'ュ' | 'ョ' | 'ッ' | 'ヮ'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Note;
    use serde_json::Value;

    fn parse_query(json: &str) -> Vec<(Option<i64>, i64, String)> {
        let v: Value = serde_json::from_str(json).expect("query is not valid JSON");
        v["notes"]
            .as_array()
            .expect("notes is not an array")
            .iter()
            .map(|n| {
                let key = n["key"].as_i64();
                let frame = n["frame_length"].as_i64().unwrap_or(0);
                let lyric = n["lyric"].as_str().unwrap_or("").to_string();
                (key, frame, lyric)
            })
            .collect()
    }

    #[test]
    fn empty_clip_yields_two_rests() {
        let notes: Vec<Note> = Vec::new();
        let q = build_sing_query(&notes, 120.0);
        let entries = parse_query(&q);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.0.is_none()));
    }

    #[test]
    fn single_note_emits_rest_note_rest() {
        let notes = vec![Note {
            id: 0,
            start_beat: 0.0,
            duration_beats: 1.0,
            pitch: 60,
            velocity: 100,
            lyric: Some("ら".into()),
            muted: false,
        }];
        let q = build_sing_query(&notes, 120.0);
        let entries = parse_query(&q);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].0, None);
        assert_eq!(entries[1].0, Some(60));
        assert_eq!(entries[1].2, "ら");
        assert_eq!(entries[2].0, None);
    }

    #[test]
    fn gap_between_notes_emits_rest_in_between() {
        let notes = vec![
            Note {
                id: 0,
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 60,
                velocity: 100,
                lyric: Some("こ".into()),
                muted: false,
            },
            Note {
                id: 0,
                start_beat: 2.0,
                duration_beats: 1.0,
                pitch: 62,
                velocity: 100,
                lyric: Some("ん".into()),
                muted: false,
            },
        ];
        let q = build_sing_query(&notes, 120.0);
        let entries = parse_query(&q);
        // rest_start, note0, gap_rest, note1, rest_end
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[1].0, Some(60));
        assert_eq!(entries[2].0, None);
        assert!(entries[2].1 > 0, "gap rest must have non-zero frame_length");
        assert_eq!(entries[3].0, Some(62));
    }

    #[test]
    fn touching_notes_emit_no_extra_rest() {
        let notes = vec![
            Note {
                id: 0,
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 60,
                velocity: 100,
                lyric: Some("こ".into()),
                muted: false,
            },
            Note {
                id: 0,
                start_beat: 1.0,
                duration_beats: 1.0,
                pitch: 62,
                velocity: 100,
                lyric: Some("ん".into()),
                muted: false,
            },
        ];
        let q = build_sing_query(&notes, 120.0);
        let entries = parse_query(&q);
        // rest_start, note0, note1, rest_end — no rest between notes.
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[1].0, Some(60));
        assert_eq!(entries[2].0, Some(62));
    }

    #[test]
    fn lyric_with_quotes_is_escaped() {
        let notes = vec![Note {
            id: 0,
            start_beat: 0.0,
            duration_beats: 1.0,
            pitch: 60,
            velocity: 100,
            lyric: Some("\"a\"".into()),
            muted: false,
        }];
        let q = build_sing_query(&notes, 120.0);
        // Must remain valid JSON despite embedded quotes.
        let _: Value = serde_json::from_str(&q).expect("invalid JSON output");
    }

    #[test]
    fn unsorted_notes_are_sorted_before_emitting() {
        let notes = vec![
            Note {
                id: 0,
                start_beat: 2.0,
                duration_beats: 1.0,
                pitch: 64,
                velocity: 100,
                lyric: Some("に".into()),
                muted: false,
            },
            Note {
                id: 0,
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 60,
                velocity: 100,
                lyric: Some("こ".into()),
                muted: false,
            },
        ];
        let q = build_sing_query(&notes, 120.0);
        let entries = parse_query(&q);
        // After sort: rest, note(60,こ), gap_rest, note(64,に), rest
        assert_eq!(entries[1].0, Some(60));
        assert_eq!(entries[3].0, Some(64));
    }

    // ---- split_into_morae --------------------------------------------------

    #[test]
    fn split_kana_basic() {
        assert_eq!(split_into_morae("あいうえお"), vec!["あ", "い", "う", "え", "お"]);
    }

    #[test]
    fn split_combines_small_kana_with_previous() {
        assert_eq!(split_into_morae("きゃ"), vec!["きゃ"]);
        assert_eq!(split_into_morae("しゅんかん"), vec!["しゅ", "ん", "か", "ん"]);
        assert_eq!(split_into_morae("ちょこ"), vec!["ちょ", "こ"]);
    }

    #[test]
    fn split_combines_small_katakana() {
        assert_eq!(split_into_morae("キャラ"), vec!["キャ", "ラ"]);
    }

    #[test]
    fn split_handles_sokuon() {
        assert_eq!(split_into_morae("ばった"), vec!["ばっ", "た"]);
    }

    #[test]
    fn split_empty_string_returns_empty() {
        assert!(split_into_morae("").is_empty());
    }

    #[test]
    fn split_starts_with_small_kana() {
        assert_eq!(split_into_morae("ぁい"), vec!["ぁ", "い"]);
    }

    #[test]
    fn split_handles_ascii_passthrough() {
        assert_eq!(split_into_morae("ab漢字"), vec!["a", "b", "漢", "字"]);
    }
}
