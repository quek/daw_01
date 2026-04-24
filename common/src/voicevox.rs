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

use crate::model::{Clip, NoteEvent, Song};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const VOICEVOX_URL: &str = "http://localhost:50021";
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
/// `(track_index, clip_index, mono_f32_samples, sample_rate)` tuples.
/// Non-vocal tracks and tracks whose `source` is `None` are skipped.
///
/// `singer_id` is the VOICEVOX style id for sing mode (e.g. 6000).
/// `speaker_id` is the VOICEVOX style id for talk mode.
pub fn synthesize_song(
    song: &Song,
    singer_id: u32,
    speaker_id: u32,
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
        // model.speaker_id is for **talk** mode (e.g. ずんだもん=3).
        // For **sing** mode, we always use the caller-provided singer_id
        // (default 6000 = 波音リツ) because VOICEVOX's singer list and
        // speaker list are entirely separate ID spaces.
        let _ = model_speaker;

        for (clip_idx, clip) in track.clips.iter().enumerate() {
            let has_notes = clip.rows.iter().any(|r| matches!(r.note, Some(NoteEvent::On(_))));
            let wav_bytes = if has_notes {
                match synthesize_sing_clip(&client, clip, song.bpm, singer_id) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!(
                            error = ?e,
                            track = track_idx,
                            clip = clip_idx,
                            "sing synthesis failed"
                        );
                        continue;
                    }
                }
            } else {
                // Talk mode: concatenate all lyrics in the clip.
                let text: String = clip
                    .rows
                    .iter()
                    .filter_map(|r| r.lyric.as_deref())
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() {
                    continue;
                }
                let sid = if *model_speaker != 0 { *model_speaker } else { speaker_id };
                match synthesize_talk(&client, &text, sid) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!(
                            error = ?e,
                            track = track_idx,
                            clip = clip_idx,
                            "talk synthesis failed"
                        );
                        continue;
                    }
                }
            };

            match decode_wav_to_f32(&wav_bytes) {
                Ok((samples, sr)) => {
                    results.push(SynthResult {
                        track: track_idx as u32,
                        clip: clip_idx as u32,
                        samples,
                        sample_rate: sr,
                    });
                }
                Err(e) => {
                    tracing::error!(error = ?e, track = track_idx, clip = clip_idx, "WAV decode failed");
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
    /// Mono f32 samples, −1..+1.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

// ---------------------------------------------------------------------------
// Sing
// ---------------------------------------------------------------------------

fn synthesize_sing_clip(
    client: &reqwest::blocking::Client,
    clip: &Clip,
    bpm: f32,
    singer_id: u32,
) -> Result<Vec<u8>> {
    let query_json = build_sing_query(clip, bpm);
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

fn build_sing_query(clip: &Clip, bpm: f32) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Leading rest
    parts.push(format!(
        r#"{{"id":"rest_start","key":null,"frame_length":{},"lyric":""}}"#,
        REST_FRAMES
    ));

    let samples_per_beat = 60.0 / bpm as f64;
    let samples_per_row = samples_per_beat / clip.rows_per_beat.max(1) as f64;

    // Collect (row_index, Note, lyric) for NoteOn rows.
    struct NoteSpan {
        start_row: usize,
        end_row: usize,
        key: u8,
        lyric: String,
    }
    let mut spans: Vec<NoteSpan> = Vec::new();
    let mut current: Option<(usize, u8, String)> = None;

    for (i, row) in clip.rows.iter().enumerate() {
        match &row.note {
            Some(NoteEvent::On(n)) => {
                if let Some((start, key, lyric)) = current.take() {
                    spans.push(NoteSpan {
                        start_row: start,
                        end_row: i,
                        key,
                        lyric,
                    });
                }
                let lyric = row.lyric.clone().unwrap_or_else(|| "ら".into());
                current = Some((i, n.key, lyric));
            }
            Some(NoteEvent::Off) => {
                if let Some((start, key, lyric)) = current.take() {
                    spans.push(NoteSpan {
                        start_row: start,
                        end_row: i,
                        key,
                        lyric,
                    });
                }
            }
            None => {}
        }
    }
    // Close any open note at the end of the clip.
    if let Some((start, key, lyric)) = current.take() {
        spans.push(NoteSpan {
            start_row: start,
            end_row: clip.rows.len(),
            key,
            lyric,
        });
    }

    if spans.is_empty() {
        // No notes → return minimal query
        parts.push(format!(
            r#"{{"id":"rest_end","key":null,"frame_length":{},"lyric":""}}"#,
            REST_FRAMES
        ));
        return format!(r#"{{"notes":[{}]}}"#, parts.join(","));
    }

    let base_row = spans[0].start_row;
    let mut prev_end_frame: i64 = 0;

    for (i, span) in spans.iter().enumerate() {
        let start_sec = (span.start_row - base_row) as f64 * samples_per_row;
        let end_sec = (span.end_row - base_row) as f64 * samples_per_row;
        let start_frame = seconds_to_frames(start_sec);
        let end_frame = seconds_to_frames(end_sec);

        // Gap (rest) between previous note and this one
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
        // Escape the lyric for JSON
        let escaped = span
            .lyric
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        parts.push(format!(
            r#"{{"id":"note{}","key":{},"frame_length":{},"lyric":"{}"}}"#,
            i, span.key, note_frames, escaped
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
    // Simple substring search — no full JSON parse needed.
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
