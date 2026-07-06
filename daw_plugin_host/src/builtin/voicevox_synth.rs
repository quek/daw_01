//! VOICEVOX 音声 **合成** — `frame_synthesis` (sing) / `synthesis` (talk) HTTP + WAV decode。
//!
//! arch-refactor S5-2 で common::voicevox から分離した (合成は builtin plugin = plugin-host
//! プロセスが唯一の実行場所、reqwest を要する)。`/singers` fetch や口パク phoneme query
//! (= GUI 側の責務) は `daw_gui::voicevox_client`。共有の純粋部分 (build_sing_query /
//! urlencoding / 各 const / Note 型) は `common::voicevox` / `common::model`。
//!
//! すべて blocking (background synth thread で呼ぶ)。合成結果は `voicevox_cache`
//! (`VoiceVoxDiskCache`) で per-user global に永続化する。

use std::io::Cursor;

use anyhow::{Context, Result};

use common::model::{Note, TalkParams};
use common::voicevox::{
    DEFAULT_SINGER_ID, QUERY_SPEAKER, VOICEVOX_URL, build_sing_query, urlencoding_encode,
};

use super::voicevox_cache::{VoiceVoxDiskCache, key_for_sing, key_for_talk};

/// 合成失敗の種別。engine への**到達可否**で分ける (呼び出し側の synth thread が
/// retry するか / GUI に「engine 未接続」と出すかを正しく決めるため)。
#[derive(Debug)]
pub enum SynthError {
    /// engine に到達できない (接続拒否 / timeout / 未起動)。transient — retry 対象。
    Unreachable(anyhow::Error),
    /// engine は応答したが入力を拒否した (HTTP 4xx/5xx や壊れた WAV 等)。
    /// `detail` は短い理由 (VOICEVOX が返した `detail` を優先)。同入力での retry は無駄。
    Rejected(String),
}

impl std::fmt::Display for SynthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SynthError::Unreachable(e) => write!(f, "engine unreachable: {e:#}"),
            SynthError::Rejected(d) => write!(f, "rejected: {d}"),
        }
    }
}

/// reqwest の送信/受信エラーを `Unreachable` へ (= engine に届かなかった)。
fn unreachable(e: reqwest::Error, ctx: &'static str) -> SynthError {
    SynthError::Unreachable(anyhow::Error::new(e).context(ctx))
}

/// VOICEVOX のエラー応答 body (通常 `{"detail":"..."}`) から人間可読な理由を取り出す。
/// JSON でなければ先頭 200 文字の preview を返す。UI にそのまま出せる短さに丸める。
fn reject_detail(status: reqwest::StatusCode, body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("detail").and_then(|d| d.as_str()).map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(200).collect());
    // status も添えるが、detail が主 (UI で読みやすい)。全体を 200 字に制限。
    let mut s = if detail.is_empty() {
        status.to_string()
    } else {
        detail
    };
    if s.chars().count() > 200 {
        s = s.chars().take(200).collect();
    }
    s
}

/// 音声 **合成** (`frame_synthesis` / `synthesis`) HTTP の timeout。歌唱は曲全体の全 note を
/// 1 query にまとめて `frame_synthesis` する (json 数 KB) ため、合成は数十秒かかり得る。
const SYNTH_HTTP_TIMEOUT_SECS: u64 = 120;
/// 合成 WAV の出力 sample rate に揃える値 (query の `outputSamplingRate` を上書き)。
const OUTPUT_SAMPLE_RATE: u32 = 48000;

// ---------------------------------------------------------------------------
// Sing
// ---------------------------------------------------------------------------

/// 既に組み立て済みの sing query JSON を `frame_synthesis` に流して WAV bytes を得る
/// (`build_sing_query` → 本関数 の 2 段)。 caller が query を先に作るのはキャッシュキー
/// (= query 内容 + singer) を HTTP 前に計算するため。
fn sing_query_to_wav(
    client: &reqwest::blocking::Client,
    query_json: &str,
    singer_id: u32,
) -> Result<Vec<u8>, SynthError> {
    tracing::info!(json_len = query_json.len(), "sing_frame_audio_query");

    // Step 1: sing_frame_audio_query
    let url = format!("{VOICEVOX_URL}/sing_frame_audio_query?speaker={QUERY_SPEAKER}");
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(query_json.to_owned())
        .send()
        .map_err(|e| unreachable(e, "sing_frame_audio_query request failed"))?;
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|e| unreachable(e, "reading sing query response"))?;
    if !status.is_success() {
        // engine は応答した = 到達済。入力 (歌詞等) が不正 → Rejected。
        return Err(SynthError::Rejected(reject_detail(status, &body)));
    }

    // Patch outputSamplingRate
    let patched = if let Some(field) = find_sample_rate_field(&body) {
        body.replace(&field, &format!("\"outputSamplingRate\":{}", OUTPUT_SAMPLE_RATE))
    } else {
        body
    };

    // Step 2: frame_synthesis
    let url = format!("{VOICEVOX_URL}/frame_synthesis?speaker={singer_id}");
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(patched)
        .send()
        .map_err(|e| unreachable(e, "frame_synthesis request failed"))?;
    let status = resp.status();
    let wav = resp
        .bytes()
        .map_err(|e| unreachable(e, "reading frame_synthesis response"))?;
    if !status.is_success() {
        let preview = String::from_utf8_lossy(&wav[..wav.len().min(300)]);
        return Err(SynthError::Rejected(reject_detail(status, &preview)));
    }

    Ok(wav.to_vec())
}

/// One note in a `synthesize_notes_for_builtin` request. Kept distinct from
/// `common::model::Note` (which carries DAW-internal IDs and clip-scoped fields) so the
/// plugin SDK boundary stays minimal — only the fields VOICEVOX actually needs.
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
            id: 0,
            start_beat: self.start_beat,
            duration_beats: self.duration_beats,
            pitch: self.pitch,
            velocity: self.velocity,
            lyric: if self.lyric.is_empty() {
                None
            } else {
                Some(self.lyric.clone())
            },
            muted: false,
        }
    }
}

/// Result of `synthesize_notes_for_builtin`. `samples` is mono f32 audio at `sample_rate` Hz;
/// `note_offsets` lets `process()` look up where a `note_on` event for a given `note_id`
/// starts streaming.
#[derive(Debug, Clone)]
pub struct BuiltinSynthOutput {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub note_offsets: std::collections::HashMap<u32, u64>,
}

/// `build_sing_query` が sing wav 先頭に必ず入れる leading rest (attack 用の無音) の
/// サンプル数 (fractional)。 note の音声開始 = beat-grid 位置 + この lead-in。 synth 側の
/// note offset 計算と、 audio half 側の連続再生 (拍 → buffer 位置写像) が **同じ値** を
/// 使うため 1 箇所に集約する (r.md #23: 連続再生でこの lead-in を足して拍に合わせる)。
pub(crate) fn lead_in_frames(sample_rate: u32) -> f64 {
    f64::from(common::voicevox::REST_FRAMES) / common::voicevox::FRAME_RATE
        * f64::from(sample_rate)
}

/// Synthesise a single track's worth of notes for the VOICEVOX builtin plugin
/// (`docs/plan_voicevox_synth.md` PR-V2.3). Returns the **mono** PCM samples + sample rate +
/// per-note frame offsets within the synthesised buffer (= `note_id → start frame`).
///
/// `notes` must NOT be empty — VOICEVOX rejects empty queries; callers should bail before
/// reaching this function.
pub fn synthesize_notes_for_builtin(
    notes: &[BuiltinNoteSpec],
    bpm: f32,
    speaker_id: u32,
) -> Result<BuiltinSynthOutput, SynthError> {
    if notes.is_empty() {
        return Err(SynthError::Rejected(
            "synthesize_notes_for_builtin called with no notes".into(),
        ));
    }
    // speaker_id 0 = 未設定 → DEFAULT_SINGER_ID (歌唱可能 style) へフォールバック。旧プロジェクトの
    // clip は声未焼き込み (0) で来るため、0 をそのまま frame_synthesis に渡すと 500 になる。
    let speaker_id = if speaker_id != 0 {
        speaker_id
    } else {
        DEFAULT_SINGER_ID
    };
    let model_notes: Vec<Note> = notes.iter().map(|n| n.to_model_note()).collect();
    let query_json = build_sing_query(&model_notes, bpm);

    // 永続コンテンツアドレスキャッシュ。query 内容 (= 歌詞 / pitch / frame / bpm が畳み込み済) +
    // singer が同じなら、HTTP 合成を丸ごと skip して保存済 WAV を返す。
    let cache = VoiceVoxDiskCache::production();
    let cache_key = key_for_sing(&query_json, speaker_id);
    let wav_bytes = if let Some(hit) = cache.as_ref().and_then(|c| c.get(cache_key)) {
        tracing::info!(cache_key, "VOICEVOX sing cache hit (HTTP skip)");
        hit
    } else {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(SYNTH_HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| unreachable(e, "building HTTP client"))?;
        let wav = sing_query_to_wav(&client, &query_json, speaker_id)?;
        if let Some(c) = cache.as_ref() {
            c.put(cache_key, &wav);
        }
        wav
    };
    let (samples, sample_rate) =
        decode_wav_to_f32(&wav_bytes).map_err(|e| SynthError::Rejected(format!("{e:#}")))?;

    // Note frame offsets relative to frame 0 of the rendered buffer.
    //
    // `build_sing_query` は wav 先頭に必ず `REST_FRAMES` の leading rest (= attack 用の無音) を
    // 入れる。各 note の音声開始位置 = leading rest + (start_beat - earliest) × samples_per_beat。
    let lead_in_samples = lead_in_frames(sample_rate).round() as u64;
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

// ---------------------------------------------------------------------------
// Talk
// ---------------------------------------------------------------------------

/// `text` を talk 合成して WAV bytes を返す。`/audio_query` → (TalkParams patch) → `/synthesis`。
/// `scales` の話速/音高/抑揚/音量 を audio_query 応答に適用してから synthesis する。blocking。
fn synthesize_talk(
    client: &reqwest::blocking::Client,
    text: &str,
    speaker_id: u32,
    scales: &TalkParams,
) -> Result<Vec<u8>, SynthError> {
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
        .map_err(|e| unreachable(e, "audio_query request failed"))?;
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|e| unreachable(e, "reading audio_query response"))?;
    if !status.is_success() {
        return Err(SynthError::Rejected(reject_detail(status, &body)));
    }

    let patched = apply_talk_params(&body, scales)
        .map_err(|e| SynthError::Rejected(format!("{e:#}")))?;

    // Step 2: synthesis
    let url = format!("{VOICEVOX_URL}/synthesis?speaker={speaker_id}");
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(patched)
        .send()
        .map_err(|e| unreachable(e, "synthesis request failed"))?;
    let status = resp.status();
    let wav = resp
        .bytes()
        .map_err(|e| unreachable(e, "reading synthesis response"))?;
    if !status.is_success() {
        let preview = String::from_utf8_lossy(&wav[..wav.len().min(300)]);
        return Err(SynthError::Rejected(reject_detail(status, &preview)));
    }

    Ok(wav.to_vec())
}

/// talk builtin 向けラッパ: `text` を talk 合成して **mono f32 + sample_rate** を返す。
/// builtin (plugin host) が 1 TextEvent につき 1 回呼び、結果を song-absolute 位置へ配置する。
pub fn synthesize_talk_for_builtin(
    text: &str,
    speaker_id: u32,
    scales: &TalkParams,
) -> Result<(Vec<f32>, u32), SynthError> {
    if text.is_empty() {
        return Err(SynthError::Rejected(
            "synthesize_talk_for_builtin called with empty text".into(),
        ));
    }
    // 永続キャッシュ (text + talk speaker + scales)。再オープンで読み上げを再合成しない。
    let cache = VoiceVoxDiskCache::production();
    let cache_key = key_for_talk(text, speaker_id, scales);
    let wav = if let Some(hit) = cache.as_ref().and_then(|c| c.get(cache_key)) {
        tracing::info!(cache_key, "VOICEVOX talk cache hit (HTTP skip)");
        hit
    } else {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(SYNTH_HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| unreachable(e, "building HTTP client"))?;
        let wav = synthesize_talk(&client, text, speaker_id, scales)?;
        if let Some(c) = cache.as_ref() {
            c.put(cache_key, &wav);
        }
        wav
    };
    decode_wav_to_f32(&wav).map_err(|e| SynthError::Rejected(format!("{e:#}")))
}

/// (talk) `TalkParams` を `/audio_query` 応答 JSON に適用して再シリアライズする。
/// `outputSamplingRate` も 48000 に揃える。
fn apply_talk_params(audio_query_json: &str, scales: &TalkParams) -> Result<String> {
    let mut v: serde_json::Value =
        serde_json::from_str(audio_query_json).context("parsing audio_query response JSON")?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("audio_query response is not a JSON object"))?;
    obj.insert("speedScale".into(), serde_json::json!(scales.speed_scale));
    obj.insert("pitchScale".into(), serde_json::json!(scales.pitch_scale));
    obj.insert(
        "intonationScale".into(),
        serde_json::json!(scales.intonation_scale),
    );
    obj.insert("volumeScale".into(), serde_json::json!(scales.volume_scale));
    obj.insert(
        "outputSamplingRate".into(),
        serde_json::json!(OUTPUT_SAMPLE_RATE),
    );
    serde_json::to_string(&v).context("re-serializing patched audio_query")
}

// ---------------------------------------------------------------------------
// WAV decode / helpers
// ---------------------------------------------------------------------------

/// Decodes WAV bytes (as returned by VOICEVOX) into mono f32 samples. Supports PCM 16-bit and
/// IEEE float 32-bit.
pub fn decode_wav_to_f32(data: &[u8]) -> Result<(Vec<f32>, u32)> {
    let cursor = Cursor::new(data);
    let mut reader = hound::WavReader::new(cursor).context("failed to parse WAV header")?;
    let spec = reader.spec();
    let sr = spec.sample_rate;
    anyhow::ensure!(sr > 0, "VOICEVOX returned WAV with sample_rate=0");

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

/// Finds the `"outputSamplingRate":<number>` substring in `json` so it can be replaced.
/// Returns the full match including the key name, or `None` when the key is absent.
fn find_sample_rate_field(json: &str) -> Option<String> {
    let start = json.find("\"outputSamplingRate\":")?;
    let after_key = start + "\"outputSamplingRate\":".len();
    let end = json[after_key..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| after_key + i)
        .unwrap_or(json.len());
    Some(json[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn apply_talk_params_sets_all_scales_and_keeps_query() {
        let query = r#"{"accent_phrases":[{"moras":[]}],"speedScale":1.0,"pitchScale":0.0,"intonationScale":1.0,"volumeScale":1.0,"outputSamplingRate":24000}"#;
        let scales = TalkParams {
            speed_scale: 1.3,
            pitch_scale: 0.05,
            intonation_scale: 0.8,
            volume_scale: 1.5,
        };
        let out = apply_talk_params(query, &scales).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!((v["speedScale"].as_f64().unwrap() as f32 - 1.3).abs() < 1e-6);
        assert!((v["pitchScale"].as_f64().unwrap() as f32 - 0.05).abs() < 1e-6);
        assert!((v["intonationScale"].as_f64().unwrap() as f32 - 0.8).abs() < 1e-6);
        assert!((v["volumeScale"].as_f64().unwrap() as f32 - 1.5).abs() < 1e-6);
        assert_eq!(v["outputSamplingRate"].as_u64().unwrap(), u64::from(OUTPUT_SAMPLE_RATE));
        assert!(v["accent_phrases"].is_array());
    }

    /// 実 VOICEVOX engine に対する talk 合成の統合テスト。engine (localhost:50021) が要るので
    /// 通常 `cargo test` では無視。
    #[test]
    #[ignore = "requires a running VOICEVOX engine at localhost:50021"]
    fn talk_synth_against_real_engine_produces_audio() {
        let scales = TalkParams::default();
        let (samples, sr) = synthesize_talk_for_builtin(
            "こんにちは。テストです。",
            common::voicevox::DEFAULT_TALK_SPEAKER_ID,
            &scales,
        )
        .expect("talk synth against real engine");
        assert!(sr >= 24000, "sample_rate looks valid: {sr}");
        assert!(!samples.is_empty(), "got samples");
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        assert!(rms > 0.001, "synthesized talk audio is non-silent (rms={rms})");
    }
}
