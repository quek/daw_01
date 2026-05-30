//! VOICEVOX HTTP API client + query builder.
//!
//! Mirrors the REAPER Lua reference implementation:
//!   - sing: `POST /sing_frame_audio_query` → `POST /frame_synthesis` → WAV
//!   - talk: `POST /audio_query` → `POST /synthesis` → WAV
//!
//! All calls are **blocking** (meant to run inside a `cx.spawn` worker or
//! a background thread). The returned WAV bytes can be decoded with
//! `decode_wav_to_f32` into planar samples ready for the audio thread.

use std::io::Cursor;

use anyhow::{Context, Result};

use crate::model::{Note, Song};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Engine REST API endpoint.  voicevox_engine の公開 URL は voicevox_engine
/// module からも参照する。
pub const VOICEVOX_URL: &str = "http://localhost:50021";

/// `/singers` レスポンスの 1 entry。 1 キャラクターと、 そのスタイル (= sing
/// 用 style id 群)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoiceVoxSinger {
    pub name: String,
    pub styles: Vec<VoiceVoxStyle>,
}

/// 各キャラクターのスタイル (= 表情 / 歌唱モード)。 `id` が `synthesize_song`
/// に渡す singer_id。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoiceVoxStyle {
    pub id: u32,
    pub name: String,
}

/// VOICEVOX engine の `/singers` を叩いて全キャラクター + スタイル一覧を取得。
/// blocking、 5 秒 timeout。 engine 未起動なら `Err`。 起動直後 (= まだ ready
/// でない) なら 5 秒 timeout 内で接続エラー、 リトライ可能。
pub fn fetch_singers() -> anyhow::Result<Vec<VoiceVoxSinger>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client.get(format!("{VOICEVOX_URL}/singers")).send()?;
    let body = resp.text()?;
    let json: serde_json::Value = serde_json::from_str(&body)?;
    let arr = json
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("/singers response is not a JSON array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let name = item["name"].as_str().unwrap_or("").to_string();
        let styles = item["styles"]
            .as_array()
            .map(|sa| {
                sa.iter()
                    .filter_map(|s| {
                        Some(VoiceVoxStyle {
                            id: s["id"].as_u64()? as u32,
                            name: s["name"].as_str()?.to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        out.push(VoiceVoxSinger { name, styles });
    }
    Ok(out)
}

/// Speaker id used for the sing_frame_audio_query step (query generation
/// only — the actual singer voice is selected at frame_synthesis time).
/// 6000 = 波音リツ, same as the REAPER reference script.
const QUERY_SPEAKER: u32 = 6000;
/// Default singer for frame_synthesis when no explicit override is given.
/// 3061 = 中国うさぎ ノーマル.
pub const DEFAULT_SINGER_ID: u32 = 3061;
const FRAME_RATE: f64 = 93.75; // 24000 Hz / 256 samples
const OUTPUT_SAMPLE_RATE: u32 = 48000;
/// Silence frames prepended/appended to every sing query so the synth
/// engine has room for attack/release envelopes.
const REST_FRAMES: u32 = 10;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Synthesize every vocal clip on every vocal track in `song`, returning
/// one `SynthResult` per clip processed (success or failure). Non-vocal
/// tracks are skipped.
///
/// `default_singer_id` is the fallback for sing mode when the track's
/// `InstrumentSource::Vocal { speaker_id }` is 0 (uninitialised). Each
/// vocal track may override with its own `speaker_id`.
///
/// `default_talk_speaker_id` is the same fallback for talk mode (clips
/// with no pitched notes).
pub fn synthesize_song(
    song: &Song,
    default_singer_id: u32,
    default_talk_speaker_id: u32,
    cache: &mut crate::voicevox_cache::VoiceVoxCache,
) -> Vec<SynthResult> {
    let client = reqwest::blocking::Client::new();
    let mut results = Vec::new();

    for (track_idx, track) in song.tracks.iter().enumerate() {
        let crate::model::InstrumentSource::Vocal {
            speaker_id: model_speaker,
            ..
        } = &track.source
        else {
            continue;
        };
        // sing/talk 両方とも、 track の speaker_id != 0 が指定されていれば
        // それを優先 (UI で設定された singer)、 0 なら caller の default。
        let track_speaker = if *model_speaker != 0 { *model_speaker } else { 0 };
        let singer_id = if track_speaker != 0 { track_speaker } else { default_singer_id };

        for (clip_idx, clip) in track.clips.iter().enumerate() {
            // v6 linked clip: notes は Song.clip_contents に。 共有 clip /
            // 独立 clip を区別せず、 同じ content_id の clip は同じ notes
            // で 1 回だけ合成 (cache key も notes ベース)。
            let notes: &[Note] = song
                .clip_contents
                .get(&clip.content_id)
                .and_then(|c| c.notes())
                .unwrap_or(&[]);

            // Cache lookup — notes 内容 + singer_id が同じなら HTTP call
            // を skip。 talk mode も sing mode も同じ key 体系で hit。
            let cache_key =
                crate::voicevox_cache::VoiceVoxCache::key_for_notes(notes, singer_id);
            if let Some(cached) = cache.get(cache_key) {
                tracing::info!(
                    track = track_idx,
                    clip = clip_idx,
                    cache_key,
                    "VOICEVOX cache hit"
                );
                results.push(SynthResult {
                    track: track_idx as u32,
                    clip: clip_idx as u32,
                    samples: cached.samples.clone(),
                    sample_rate: cached.sample_rate,
                    error: None,
                });
                continue;
            }

            // A clip with at least one note that has any pitch goes into
            // sing mode; otherwise we fall back to talk mode using
            // whatever lyrics are attached (used for spoken intros, etc.).
            let has_pitched_notes = notes.iter().any(|n| n.pitch > 0);
            let wav_bytes = if has_pitched_notes {
                match synthesize_sing_clip(&client, notes, song.bpm, singer_id) {
                    Ok(b) => b,
                    Err(e) => {
                        let msg = format!("{e:#}");
                        tracing::error!(error = ?e, track = track_idx, clip = clip_idx, "sing synthesis failed");
                        results.push(SynthResult {
                            track: track_idx as u32,
                            clip: clip_idx as u32,
                            samples: Vec::new(),
                            sample_rate: 0,
                            error: Some(msg),
                        });
                        continue;
                    }
                }
            } else {
                // Talk mode: concatenate lyrics in time order.
                let mut sorted: Vec<&Note> = notes.iter().collect();
                sorted.sort_by(|a, b| {
                    a.start_beat
                        .partial_cmp(&b.start_beat)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                let text: String = sorted
                    .iter()
                    .filter_map(|n| n.lyric.as_deref())
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() {
                    continue;
                }
                let sid = if track_speaker != 0 { track_speaker } else { default_talk_speaker_id };
                match synthesize_talk(&client, &text, sid) {
                    Ok(b) => b,
                    Err(e) => {
                        let msg = format!("{e:#}");
                        tracing::error!(error = ?e, track = track_idx, clip = clip_idx, "talk synthesis failed");
                        results.push(SynthResult {
                            track: track_idx as u32,
                            clip: clip_idx as u32,
                            samples: Vec::new(),
                            sample_rate: 0,
                            error: Some(msg),
                        });
                        continue;
                    }
                }
            };

            match decode_wav_to_f32(&wav_bytes) {
                Ok((samples, sr)) => {
                    // Cache に store (次回同 clip 同 singer で hit)
                    cache.insert(
                        cache_key,
                        crate::voicevox_cache::CachedClip {
                            samples: samples.clone(),
                            sample_rate: sr,
                        },
                    );
                    results.push(SynthResult {
                        track: track_idx as u32,
                        clip: clip_idx as u32,
                        samples,
                        sample_rate: sr,
                        error: None,
                    });
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::error!(error = ?e, track = track_idx, clip = clip_idx, "WAV decode failed");
                    results.push(SynthResult {
                        track: track_idx as u32,
                        clip: clip_idx as u32,
                        samples: Vec::new(),
                        sample_rate: 0,
                        error: Some(msg),
                    });
                }
            }
        }
    }

    results
}

#[derive(Debug, Clone)]
pub struct SynthResult {
    pub track: u32,
    pub clip: u32,
    /// Mono f32 samples, −1..+1. Empty when `error` is `Some`.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    /// Non-None when synthesis failed for this clip.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Sing
// ---------------------------------------------------------------------------

fn synthesize_sing_clip(
    client: &reqwest::blocking::Client,
    notes: &[Note],
    bpm: f32,
    singer_id: u32,
) -> Result<Vec<u8>> {
    let query_json = build_sing_query(notes, bpm);
    tracing::info!(json_len = query_json.len(), "sing_frame_audio_query");

    // Step 1: sing_frame_audio_query
    let url = format!(
        "{}/sing_frame_audio_query?speaker={}",
        VOICEVOX_URL, QUERY_SPEAKER
    );
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(query_json)
        .send()
        .context("sing_frame_audio_query request failed")?;
    let status = resp.status();
    let body = resp.text().context("reading sing query response")?;
    anyhow::ensure!(
        status.is_success(),
        "sing_frame_audio_query returned {}: {}",
        status,
        &body[..body.len().min(200)]
    );

    // Patch outputSamplingRate
    let patched = body.replace(
        &find_sample_rate_field(&body),
        &format!("\"outputSamplingRate\":{}", OUTPUT_SAMPLE_RATE),
    );

    // Step 2: frame_synthesis
    let url = format!(
        "{}/frame_synthesis?speaker={}",
        VOICEVOX_URL, singer_id
    );
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(patched)
        .send()
        .context("frame_synthesis request failed")?;
    let status = resp.status();
    let wav = resp.bytes().context("reading frame_synthesis response")?;
    if !status.is_success() {
        let preview = String::from_utf8_lossy(&wav[..wav.len().min(300)]);
        anyhow::bail!("frame_synthesis returned {}: {}", status, preview);
    }

    Ok(wav.to_vec())
}

// ---------------------------------------------------------------------------
// Talk
// ---------------------------------------------------------------------------

fn synthesize_talk(
    client: &reqwest::blocking::Client,
    text: &str,
    speaker_id: u32,
) -> Result<Vec<u8>> {
    // Step 1: audio_query
    let url = format!(
        "{}/audio_query?speaker={}&text={}",
        VOICEVOX_URL,
        speaker_id,
        urlencoding_encode(text)
    );
    let resp = client
        .post(&url)
        .send()
        .context("audio_query request failed")?;
    let status = resp.status();
    let body = resp.text().context("reading audio_query response")?;
    anyhow::ensure!(
        status.is_success(),
        "audio_query returned {}: {}",
        status,
        &body[..body.len().min(200)]
    );

    let patched = body.replace(
        &find_sample_rate_field(&body),
        &format!("\"outputSamplingRate\":{}", OUTPUT_SAMPLE_RATE),
    );

    // Step 2: synthesis
    let url = format!("{}/synthesis?speaker={}", VOICEVOX_URL, speaker_id);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(patched)
        .send()
        .context("synthesis request failed")?;
    let status = resp.status();
    let wav = resp.bytes().context("reading synthesis response")?;
    if !status.is_success() {
        let preview = String::from_utf8_lossy(&wav[..wav.len().min(300)]);
        anyhow::bail!("synthesis returned {}: {}", status, preview);
    }

    Ok(wav.to_vec())
}

// ---------------------------------------------------------------------------
// Query builder (sing)
// ---------------------------------------------------------------------------

/// Builds the JSON body for `POST /sing_frame_audio_query`.
///
/// Notes are converted to a flat sequence of `{key, frame_length, lyric}`
/// entries with `key=null` rests inserted between any two notes that don't
/// touch. The first note's `start_beat` becomes `frame 0` of the query —
/// VOICEVOX renders relative to the first non-rest entry, not relative to
/// the song timeline, so anything before the first note is ignored.
fn build_sing_query(notes: &[Note], bpm: f32) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Leading rest (gives the synth a moment of silence for the attack).
    parts.push(format!(
        r#"{{"id":"rest_start","key":null,"frame_length":{},"lyric":""}}"#,
        REST_FRAMES
    ));

    // Sort notes by start_beat — `ClipContent.notes` is unordered by
    // contract, and this builder requires monotonic timing.
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
// WAV decode
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Builtin plugin wrapper (PR-V2.3)
// ---------------------------------------------------------------------------

/// Synthesise a single track's worth of notes for the VOICEVOX builtin
/// plugin (`docs/plan_voicevox_synth.md` PR-V2.3). Wraps `synthesize_sing
/// _clip` with the plumbing the builtin needs:
///
/// - `notes` is a flat list of all notes the plugin should sing; the
///   plugin builds this from its `NoteMetadata` buffer (filled by
///   `LoadedPlugin::set_note_metadata`).
/// - returns the **mono** PCM samples + sample rate + per-note frame
///   offsets within the synthesised buffer (= `note_id → start frame`).
///
/// The frame offset table lets `process()` look up where a freshly-
/// arrived `note_on` event should start streaming from. Offsets are
/// computed from `(note.start_beat - earliest_start_beat) ×
/// samples_per_beat`, mirroring how the VOICEVOX engine renders a
/// `sing_frame_audio_query` payload (frame 0 = first non-rest entry).
///
/// `notes` must NOT be empty — VOICEVOX rejects empty queries; callers
/// should bail before reaching this function.
pub fn synthesize_notes_for_builtin(
    notes: &[BuiltinNoteSpec],
    bpm: f32,
    speaker_id: u32,
) -> Result<BuiltinSynthOutput> {
    anyhow::ensure!(
        !notes.is_empty(),
        "synthesize_notes_for_builtin called with no notes"
    );
    let model_notes: Vec<Note> = notes.iter().map(|n| n.to_model_note()).collect();

    let client = reqwest::blocking::Client::new();
    let wav_bytes = synthesize_sing_clip(&client, &model_notes, bpm, speaker_id)?;
    let (samples, sample_rate) = decode_wav_to_f32(&wav_bytes)?;

    // Note frame offsets relative to frame 0 of the rendered buffer.
    //
    // `build_sing_query` は wav 先頭に必ず `REST_FRAMES` の leading rest
    // (= attack 用の無音) を入れる。 よって「最初の non-rest entry」 は
    // frame 0 ではなく frame REST_FRAMES から始まる。 各 note の音声開始
    // 位置 = leading rest + (start_beat - earliest) × samples_per_beat。
    // この lead-in を足さないと、 cursor が無音区間を指したまま再生開始
    // → 全 note が一律 REST_FRAMES 分 (≈107ms @48kHz) 遅れて聞こえる。
    let lead_in_samples =
        (f64::from(REST_FRAMES) / FRAME_RATE * f64::from(sample_rate)).round() as u64;
    let earliest = notes
        .iter()
        .map(|n| n.start_beat)
        .fold(f64::INFINITY, f64::min);
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(bpm.max(0.001));
    let mut note_offsets: std::collections::HashMap<u32, u64> =
        std::collections::HashMap::with_capacity(notes.len());
    for n in notes {
        let frame = lead_in_samples
            + (((n.start_beat - earliest) * samples_per_beat).max(0.0)) as u64;
        note_offsets.insert(n.note_id, frame);
    }

    Ok(BuiltinSynthOutput {
        samples,
        sample_rate,
        note_offsets,
    })
}

/// One note in a `synthesize_notes_for_builtin` request. Kept distinct
/// from `crate::model::Note` (which carries DAW-internal IDs and clip-
/// scoped fields) so the plugin SDK boundary stays minimal — only the
/// fields VOICEVOX actually needs.
#[derive(Debug, Clone)]
pub struct BuiltinNoteSpec {
    pub note_id: u32,
    pub start_beat: f64,
    pub duration_beats: f64,
    pub pitch: u8,
    pub velocity: u8,
    pub lyric: String,
}

impl BuiltinNoteSpec {
    fn to_model_note(&self) -> Note {
        Note {
            start_beat: self.start_beat,
            duration_beats: self.duration_beats,
            pitch: self.pitch,
            velocity: self.velocity,
            lyric: if self.lyric.is_empty() {
                None
            } else {
                Some(self.lyric.clone())
            },
        }
    }
}

/// Result of `synthesize_notes_for_builtin`. `samples` is mono f32
/// audio at `sample_rate` Hz; `note_offsets` lets `process()` look up
/// where a `note_on` event for a given `note_id` starts streaming.
#[derive(Debug, Clone)]
pub struct BuiltinSynthOutput {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub note_offsets: std::collections::HashMap<u32, u64>,
}

/// Decodes WAV bytes (as returned by VOICEVOX) into mono f32 samples.
/// Supports PCM 16-bit and IEEE float 32-bit.
pub fn decode_wav_to_f32(data: &[u8]) -> Result<(Vec<f32>, u32)> {
    let cursor = Cursor::new(data);
    let mut reader =
        hound::WavReader::new(cursor).context("failed to parse WAV header")?;
    let spec = reader.spec();
    let sr = spec.sample_rate;

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()
            .context("reading PCM16 samples")?,
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .context("reading f32 samples")?,
    };

    // If stereo, mixdown to mono by averaging channels.
    let mono = if spec.channels > 1 {
        let ch = spec.channels as usize;
        samples
            .chunks(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect()
    } else {
        samples
    };

    Ok((mono, sr))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Finds the `"outputSamplingRate":<number>` substring in `json` so it can
/// be replaced. Returns the full match including the key name.
fn find_sample_rate_field(json: &str) -> String {
    if let Some(start) = json.find("\"outputSamplingRate\":") {
        let after_key = start + "\"outputSamplingRate\":".len();
        let end = json[after_key..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| after_key + i)
            .unwrap_or(json.len());
        json[start..end].to_string()
    } else {
        // Fallback: return something that won't match so replace is a no-op.
        String::new()
    }
}

/// Minimal URL-encoding for query parameters.
fn urlencoding_encode(s: &str) -> String {
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

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::model::Clip;
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
        let clip = Clip {
            id: 1,
            name: "c".into(),
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: 0,
            notes: Vec::new(),
            color: None,
        };
        let q = build_sing_query(&clip.notes, 120.0);
        let entries = parse_query(&q);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.0.is_none()));
    }

    #[test]
    fn single_note_emits_rest_note_rest() {
        let clip = Clip {
            id: 1,
            name: "c".into(),
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: 0,
            notes: vec![Note {
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 60,
                velocity: 100,
                lyric: Some("ら".into()),
            }],
            color: None,
        };
        let q = build_sing_query(&clip.notes, 120.0);
        let entries = parse_query(&q);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].0, None);
        assert_eq!(entries[1].0, Some(60));
        assert_eq!(entries[1].2, "ら");
        assert_eq!(entries[2].0, None);
    }

    #[test]
    fn gap_between_notes_emits_rest_in_between() {
        let clip = Clip {
            id: 1,
            name: "c".into(),
            start_beat: 0.0,
            length_beats: 8.0,
            content_id: 0,
            notes: vec![
                Note {
                    start_beat: 0.0,
                    duration_beats: 1.0,
                    pitch: 60,
                    velocity: 100,
                    lyric: Some("こ".into()),
                },
                Note {
                    start_beat: 2.0,
                    duration_beats: 1.0,
                    pitch: 62,
                    velocity: 100,
                    lyric: Some("ん".into()),
                },
            ],
            color: None,
        };
        let q = build_sing_query(&clip.notes, 120.0);
        let entries = parse_query(&q);
        // rest_start, note0, gap_rest, note1, rest_end
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[1].0, Some(60));
        assert_eq!(entries[2].0, None);
        assert!(
            entries[2].1 > 0,
            "gap rest must have non-zero frame_length"
        );
        assert_eq!(entries[3].0, Some(62));
    }

    #[test]
    fn touching_notes_emit_no_extra_rest() {
        // Notes that end exactly where the next one starts shouldn't get
        // a 0-frame rest stuffed between them.
        let clip = Clip {
            id: 1,
            name: "c".into(),
            start_beat: 0.0,
            length_beats: 8.0,
            content_id: 0,
            notes: vec![
                Note {
                    start_beat: 0.0,
                    duration_beats: 1.0,
                    pitch: 60,
                    velocity: 100,
                    lyric: Some("こ".into()),
                },
                Note {
                    start_beat: 1.0,
                    duration_beats: 1.0,
                    pitch: 62,
                    velocity: 100,
                    lyric: Some("ん".into()),
                },
            ],
            color: None,
        };
        let q = build_sing_query(&clip.notes, 120.0);
        let entries = parse_query(&q);
        // rest_start, note0, note1, rest_end — no rest between notes.
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[1].0, Some(60));
        assert_eq!(entries[2].0, Some(62));
    }

    #[test]
    fn lyric_with_quotes_is_escaped() {
        let clip = Clip {
            id: 1,
            name: "c".into(),
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: 0,
            notes: vec![Note {
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 60,
                velocity: 100,
                lyric: Some("\"a\"".into()),
            }],
            color: None,
        };
        let q = build_sing_query(&clip.notes, 120.0);
        // Must remain valid JSON despite embedded quotes.
        let _: Value = serde_json::from_str(&q).expect("invalid JSON output");
    }

    #[test]
    fn unsorted_notes_are_sorted_before_emitting() {
        let clip = Clip {
            id: 1,
            name: "c".into(),
            start_beat: 0.0,
            length_beats: 8.0,
            content_id: 0,
            notes: vec![
                Note {
                    start_beat: 2.0,
                    duration_beats: 1.0,
                    pitch: 64,
                    velocity: 100,
                    lyric: Some("に".into()),
                },
                Note {
                    start_beat: 0.0,
                    duration_beats: 1.0,
                    pitch: 60,
                    velocity: 100,
                    lyric: Some("こ".into()),
                },
            ],
            color: None,
        };
        let q = build_sing_query(&clip.notes, 120.0);
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
        // "きゃ" は 1 モーラ
        assert_eq!(split_into_morae("きゃ"), vec!["きゃ"]);
        // "しゅんかん" → "しゅ" / "ん" / "か" / "ん"
        assert_eq!(split_into_morae("しゅんかん"), vec!["しゅ", "ん", "か", "ん"]);
        // "ちょこ" → "ちょ" / "こ"
        assert_eq!(split_into_morae("ちょこ"), vec!["ちょ", "こ"]);
    }

    #[test]
    fn split_combines_small_katakana() {
        assert_eq!(split_into_morae("キャラ"), vec!["キャ", "ラ"]);
    }

    #[test]
    fn split_handles_sokuon() {
        // 促音 "っ" は 1 モーラとして扱う前モーラの長音化用、 ただしモーラ単位
        // としては VOICEVOX 仕様上 "っ" 単体で 1 entry として扱う方が正確
        // — ここでは「小書き仮名は前と結合」 ルールに従い "ばっ" は 1 モーラ
        assert_eq!(split_into_morae("ばった"), vec!["ばっ", "た"]);
    }

    #[test]
    fn split_empty_string_returns_empty() {
        assert!(split_into_morae("").is_empty());
    }

    #[test]
    fn split_starts_with_small_kana() {
        // 行頭が小書き仮名の場合は単独で 1 モーラ (前に結合先がない)
        assert_eq!(split_into_morae("ぁい"), vec!["ぁ", "い"]);
    }

    #[test]
    fn split_handles_ascii_passthrough() {
        // ASCII / 漢字等は 1 char = 1 モーラ
        assert_eq!(split_into_morae("ab漢字"), vec!["a", "b", "漢", "字"]);
    }
}

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
