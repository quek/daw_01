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

use crate::model::{Note, TalkParams};

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
    fetch_voices("/singers")
}

/// (talk) VOICEVOX engine の `/speakers` を叩いて全 talk キャラクター + スタイル
/// 一覧を取得する (`docs/plan_voicevox_talk.md` §4)。レスポンス構造は `/singers` と
/// 同型 (`[{name, styles:[{id, name}]}]`、 talk は別途 `speaker_uuid` を持つが無視)。
/// 各 style の `id` が `/audio_query` + `/synthesis` に渡す talk speaker id。
pub fn fetch_speakers() -> anyhow::Result<Vec<VoiceVoxSinger>> {
    fetch_voices("/speakers")
}

/// `/singers` (sing) / `/speakers` (talk) 共通の取得 + パース。両 endpoint は
/// `[{name, styles:[{id, name}]}]` の同型レスポンスを返す。blocking、5 秒 timeout。
fn fetch_voices(path: &str) -> anyhow::Result<Vec<VoiceVoxSinger>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client.get(format!("{VOICEVOX_URL}{path}")).send()?;
    let body = resp.text()?;
    let json: serde_json::Value = serde_json::from_str(&body)?;
    let arr = json
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("{path} response is not a JSON array"))?;
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
/// (talk) Default talk speaker for `/audio_query` + `/synthesis` when a Text clip
/// has no explicit talk speaker (`Clip::speaker_id == 0`)。歌唱の sing style とは別
/// id 空間 (`/speakers`)。3 = ずんだもん ノーマル (VOICEVOX 標準の代表 talk voice)。
pub const DEFAULT_TALK_SPEAKER_ID: u32 = 3;

/// 音声 **合成** (`frame_synthesis` / `synthesis`) HTTP の timeout。歌唱は曲全体の
/// 全 note を 1 query にまとめて `frame_synthesis` する (json 数 KB) ため、合成は
/// 数十秒かかり得る。旧 5 秒では曲が大きいと毎回 timeout して「歌が鳴らない」
/// (2026-06-20、 実機 27 トラック曲で発覚)。engine 不在 (= 接続拒否) は timeout を
/// 待たず即 Err になるので、 長め設定でも engine-down 時のリトライは遅くならない。
const SYNTH_HTTP_TIMEOUT_SECS: u64 = 120;
/// (FIXME #36) `DEFAULT_SINGER_ID` の表示用キャラ名 / スタイル名。新規 vocal
/// clip で声が未設定のときの既定表示、 旧プロジェクト migration で名前が
/// 欠落しているときのフォールバックに使う。
pub const DEFAULT_SINGER_NAME: &str = "中国うさぎ";
pub const DEFAULT_STYLE_NAME: &str = "ノーマル";
/// VOICEVOX frame rate (24000 Hz / 256 samples). 口パク (`crate::lipsync`) の
/// frame_length → beat 変換でも参照する。
pub(crate) const FRAME_RATE: f64 = 93.75;
const OUTPUT_SAMPLE_RATE: u32 = 48000;
/// Silence frames prepended/appended to every sing query so the synth
/// engine has room for attack/release envelopes. 口パクの先頭 pau lead-in
/// オフセットでも同じ値を使い、音声と口を揃える。
pub(crate) const REST_FRAMES: u32 = 10;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

// NOTE (FIXME #77): 旧 `synthesize_song` + in-memory `VoiceVoxCache` は撤去した。
// 合成は `daw_plugin_host` の builtin plugin が `synthesize_notes_for_builtin` /
// `synthesize_talk_for_builtin` 経由で行い、 結果は `voicevox_cache`
// (`VoiceVoxDiskCache`) で per-user global に永続化する。

// ---------------------------------------------------------------------------
// Sing
// ---------------------------------------------------------------------------

/// 既に組み立て済みの sing query JSON を `frame_synthesis` に流して WAV bytes を
/// 得る (`build_sing_query` → 本関数 の 2 段)。 caller が query を先に作るのは
/// キャッシュキー (= query 内容 + singer) を HTTP 前に計算するため (FIXME #77)。
fn sing_query_to_wav(
    client: &reqwest::blocking::Client,
    query_json: &str,
    singer_id: u32,
) -> Result<Vec<u8>> {
    tracing::info!(json_len = query_json.len(), "sing_frame_audio_query");

    // Step 1: sing_frame_audio_query
    let url = format!(
        "{}/sing_frame_audio_query?speaker={}",
        VOICEVOX_URL, QUERY_SPEAKER
    );
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(query_json.to_owned())
        .send()
        .context("sing_frame_audio_query request failed")?;
    let status = resp.status();
    let body = resp.text().context("reading sing query response")?;
    if !status.is_success() {
        let preview: String = body.chars().take(200).collect();
        anyhow::bail!("sing_frame_audio_query returned {}: {}", status, preview);
    }

    // Patch outputSamplingRate
    let patched = if let Some(field) = find_sample_rate_field(&body) {
        body.replace(
            &field,
            &format!("\"outputSamplingRate\":{}", OUTPUT_SAMPLE_RATE),
        )
    } else {
        body
    };

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
// Lip-sync phoneme query (口パク, docs/plan_pakupaku.md §5)
// ---------------------------------------------------------------------------

/// VOICEVOX が返す 1 phoneme とその長さ (VOICEVOX frame 単位、`FRAME_RATE`
/// = 93.75fps)。`phoneme` は `"a"`/`"i"`/.../`"N"`/`"cl"`/`"pau"` や子音記号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phoneme {
    pub phoneme: String,
    pub frame_length: u32,
}

/// 口パク (lip-sync) 用: `sing_frame_audio_query` **だけ** を叩いて phoneme 列を
/// 取得する。`frame_synthesis` は呼ばない (口パクに音声 WAV は不要、phoneme
/// タイミングだけ要る = 軽い)。`build_sing_query` を音声合成と共用するので、
/// 得られる phoneme は実際に鳴る歌唱と完全に一致する (= 口と音声が同期)。
///
/// 戻り値は先頭/末尾の `REST_FRAMES` 分の `pau` も含む VOICEVOX 生の phoneme 列
/// (frame 0 起点)。beat への配置は `crate::lipsync` 側で行う。
pub fn query_phonemes(notes: &[Note], bpm: f32) -> Result<Vec<Phoneme>> {
    let client = reqwest::blocking::Client::new();
    let query_json = build_sing_query(notes, bpm);
    let url = format!("{VOICEVOX_URL}/sing_frame_audio_query?speaker={QUERY_SPEAKER}");
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(query_json)
        .send()
        .context("sing_frame_audio_query request failed")?;
    let status = resp.status();
    let body = resp.text().context("reading sing query response")?;
    if !status.is_success() {
        let preview: String = body.chars().take(200).collect();
        anyhow::bail!("sing_frame_audio_query returned {}: {}", status, preview);
    }
    parse_phonemes(&body)
}

/// `sing_frame_audio_query` 応答 JSON から `phonemes` 配列を抽出する。各要素は
/// `{ "phoneme": "...", "frame_length": N }` (VOICEVOX FrameAudioQuery)。
/// REAPER `pakupaku.lua` の `parse_phonemes` と同構造。
fn parse_phonemes(body: &str) -> Result<Vec<Phoneme>> {
    let json: serde_json::Value =
        serde_json::from_str(body).context("parsing sing_frame_audio_query JSON")?;
    let arr = json["phonemes"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("sing_frame_audio_query response has no phonemes array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for p in arr {
        let phoneme = p["phoneme"].as_str().unwrap_or("").to_string();
        let frame_length = p["frame_length"].as_u64().unwrap_or(0) as u32;
        if !phoneme.is_empty() {
            out.push(Phoneme {
                phoneme,
                frame_length,
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Talk
// ---------------------------------------------------------------------------

/// `text` を talk 合成して WAV bytes を返す。`/audio_query` → (TalkParams patch) →
/// `/synthesis`。`scales` の話速/音高/抑揚/音量 を audio_query 応答に適用してから
/// synthesis する (`docs/plan_voicevox_talk.md` §3.1)。blocking。
fn synthesize_talk(
    client: &reqwest::blocking::Client,
    text: &str,
    speaker_id: u32,
    scales: &TalkParams,
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
    if !status.is_success() {
        let preview: String = body.chars().take(200).collect();
        anyhow::bail!("audio_query returned {}: {}", status, preview);
    }

    let patched = apply_talk_params(&body, scales)?;

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

/// talk builtin 向けラッパ: `text` を talk 合成して **mono f32 + sample_rate** を返す
/// (`docs/plan_voicevox_talk.md` §3.1)。builtin (plugin host) が 1 TextEvent につき
/// 1 回呼び、結果を song-absolute 位置へ配置する。`note_offsets` は builtin 側で
/// event_id と placement から組むのでここでは返さない (sing の
/// `synthesize_notes_for_builtin` と違い talk は単一発話 = 単一 voice)。
pub fn synthesize_talk_for_builtin(
    text: &str,
    speaker_id: u32,
    scales: &TalkParams,
) -> Result<(Vec<f32>, u32)> {
    anyhow::ensure!(!text.is_empty(), "synthesize_talk_for_builtin called with empty text");
    // FIXME #77: 永続キャッシュ (text + talk speaker + scales)。 再オープンで
    // 読み上げを再合成しない。
    let cache = crate::voicevox_cache::VoiceVoxDiskCache::production();
    let cache_key = crate::voicevox_cache::key_for_talk(text, speaker_id, scales);
    let wav = if let Some(hit) = cache.as_ref().and_then(|c| c.get(cache_key)) {
        tracing::info!(cache_key, "VOICEVOX talk cache hit (HTTP skip)");
        hit
    } else {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(SYNTH_HTTP_TIMEOUT_SECS))
            .build()?;
        let wav = synthesize_talk(&client, text, speaker_id, scales)?;
        if let Some(c) = cache.as_ref() {
            c.put(cache_key, &wav);
        }
        wav
    };
    decode_wav_to_f32(&wav)
}

/// (talk) `TalkParams` を `/audio_query` 応答 JSON に適用して再シリアライズする。
/// RT 外 (background synth thread) なので serde_json で素直に parse → 値設定 →
/// 再シリアライズ (sing の string-replace patch と違い複数 scale field を確実に上書き)。
/// `outputSamplingRate` も 48000 に揃える。
fn apply_talk_params(audio_query_json: &str, scales: &TalkParams) -> Result<String> {
    let mut v: serde_json::Value = serde_json::from_str(audio_query_json)
        .context("parsing audio_query response JSON")?;
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
// Talk lip-sync phoneme query (口パク, docs/plan_voicevox_talk.md §5)
// ---------------------------------------------------------------------------

/// (talk) `text` の `/audio_query` を叩き、phoneme 列を返す (口パク用、`/synthesis`
/// は呼ばない)。歌唱の `query_phonemes` と同じ `Vec<Phoneme>` を返すので、生成先
/// clip への配置は `crate::lipsync::build_mouth_events` をそのまま再利用できる。
/// 先頭/末尾の `pau` (pre/post phoneme length) を含む。`scales.speed_scale` で
/// frame_length を割り、実際に鳴る (= speed 適用後の) 音声と口を揃える。
pub fn query_talk_phonemes(
    text: &str,
    speaker_id: u32,
    scales: &TalkParams,
) -> Result<Vec<Phoneme>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
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
    if !status.is_success() {
        let preview: String = body.chars().take(200).collect();
        anyhow::bail!("audio_query returned {}: {}", status, preview);
    }
    parse_talk_phonemes(&body, scales.speed_scale)
}

/// `/audio_query` 応答 JSON (`accent_phrases[].moras[]` + pre/post phoneme length +
/// pause_mora) を `Vec<Phoneme>` へ変換する純粋関数。各モーラは子音 (あれば) +
/// 母音の 2 phoneme に展開し、長さ (秒) を `FRAME_RATE` × `1/speed` で frame へ。
/// 先頭に `prePhonemeLength`、末尾に `postPhonemeLength` 由来の `pau` を置く
/// (歌唱の leading/trailing rest に相当)。`build_mouth_events` の lead-in と整合する。
fn parse_talk_phonemes(body: &str, speed_scale: f32) -> Result<Vec<Phoneme>> {
    let json: serde_json::Value =
        serde_json::from_str(body).context("parsing audio_query JSON")?;
    let speed = f64::from(speed_scale).max(0.01);
    let sec_to_frames = |s: f64| -> u32 { (s / speed * FRAME_RATE).round().max(0.0) as u32 };

    let mut out: Vec<Phoneme> = Vec::new();
    // 先頭 pau (prePhonemeLength、無ければ 0.1s)。
    let pre = json["prePhonemeLength"].as_f64().unwrap_or(0.1);
    out.push(Phoneme { phoneme: "pau".into(), frame_length: sec_to_frames(pre) });

    let phrases = json["accent_phrases"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("audio_query response has no accent_phrases array"))?;
    for phrase in phrases {
        if let Some(moras) = phrase["moras"].as_array() {
            for mora in moras {
                if let Some(cons) = mora["consonant"].as_str() {
                    let len = mora["consonant_length"].as_f64().unwrap_or(0.0);
                    out.push(Phoneme { phoneme: cons.to_string(), frame_length: sec_to_frames(len) });
                }
                let vowel = mora["vowel"].as_str().unwrap_or("");
                if !vowel.is_empty() {
                    let len = mora["vowel_length"].as_f64().unwrap_or(0.0);
                    out.push(Phoneme { phoneme: vowel.to_string(), frame_length: sec_to_frames(len) });
                }
            }
        }
        // accent_phrase 間のポーズ (pause_mora、vowel は通常 "pau")。
        if let Some(pause) = phrase["pause_mora"].as_object() {
            let len = pause
                .get("vowel_length")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            out.push(Phoneme { phoneme: "pau".into(), frame_length: sec_to_frames(len) });
        }
    }

    // 末尾 pau (postPhonemeLength)。
    let post = json["postPhonemeLength"].as_f64().unwrap_or(0.1);
    out.push(Phoneme { phoneme: "pau".into(), frame_length: sec_to_frames(post) });
    Ok(out)
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
    // (FIXME #36) speaker_id 0 = 未設定 → DEFAULT_SINGER_ID (歌唱可能 style) へ
    // フォールバック。 旧プロジェクトの clip は声未焼き込み (0) で来るため、 0 を
    // そのまま frame_synthesis に渡すと 500 Internal Server Error になる。
    let speaker_id = if speaker_id != 0 {
        speaker_id
    } else {
        DEFAULT_SINGER_ID
    };
    let model_notes: Vec<Note> = notes.iter().map(|n| n.to_model_note()).collect();
    let query_json = build_sing_query(&model_notes, bpm);

    // FIXME #77: 永続コンテンツアドレスキャッシュ。 query 内容 (= 歌詞 / pitch /
    // frame / bpm が畳み込み済) + singer が同じなら、 HTTP 合成を丸ごと skip して
    // 保存済 WAV を返す。 プロジェクト再オープンで全曲を再合成しないための要。
    let cache = crate::voicevox_cache::VoiceVoxDiskCache::production();
    let cache_key = crate::voicevox_cache::key_for_sing(&query_json, speaker_id);
    let wav_bytes = if let Some(hit) = cache.as_ref().and_then(|c| c.get(cache_key)) {
        tracing::info!(cache_key, "VOICEVOX sing cache hit (HTTP skip)");
        hit
    } else {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(SYNTH_HTTP_TIMEOUT_SECS))
            .build()?;
        let wav = sing_query_to_wav(&client, &query_json, speaker_id)?;
        if let Some(c) = cache.as_ref() {
            c.put(cache_key, &wav);
        }
        wav
    };
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
            muted: false,
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Finds the `"outputSamplingRate":<number>` substring in `json` so it can
/// be replaced. Returns the full match including the key name, or `None`
/// when the key is absent (an empty match would make `str::replace` splice
/// the replacement between every character, corrupting the JSON).
fn find_sample_rate_field(json: &str) -> Option<String> {
    let start = json.find("\"outputSamplingRate\":")?;
    let after_key = start + "\"outputSamplingRate\":".len();
    let end = json[after_key..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| after_key + i)
        .unwrap_or(json.len());
    Some(json[start..end].to_string())
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
    use crate::model::{Clip, TalkParams};
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
            auto_lipsync: false,
            ..Default::default()
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
                muted: false,
            }],
            color: None,
            auto_lipsync: false,
            ..Default::default()
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
                    muted: false,
                },
                Note {
                    start_beat: 2.0,
                    duration_beats: 1.0,
                    pitch: 62,
                    velocity: 100,
                    lyric: Some("ん".into()),
                    muted: false,
                },
            ],
            color: None,
            auto_lipsync: false,
            ..Default::default()
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
                    muted: false,
                },
                Note {
                    start_beat: 1.0,
                    duration_beats: 1.0,
                    pitch: 62,
                    velocity: 100,
                    lyric: Some("ん".into()),
                    muted: false,
                },
            ],
            color: None,
            auto_lipsync: false,
            ..Default::default()
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
                muted: false,
            }],
            color: None,
            auto_lipsync: false,
            ..Default::default()
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
                    muted: false,
                },
                Note {
                    start_beat: 0.0,
                    duration_beats: 1.0,
                    pitch: 60,
                    velocity: 100,
                    lyric: Some("こ".into()),
                    muted: false,
                },
            ],
            color: None,
            auto_lipsync: false,
            ..Default::default()
        };
        let q = build_sing_query(&clip.notes, 120.0);
        let entries = parse_query(&q);
        // After sort: rest, note(60,こ), gap_rest, note(64,に), rest
        assert_eq!(entries[1].0, Some(60));
        assert_eq!(entries[3].0, Some(64));
    }

    // ---- talk (docs/plan_voicevox_talk.md §3) ------------------------------

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
        // outputSamplingRate は 48000 に揃う。
        assert_eq!(v["outputSamplingRate"].as_u64().unwrap(), u64::from(OUTPUT_SAMPLE_RATE));
        // 既存の query 構造 (accent_phrases) は保持される。
        assert!(v["accent_phrases"].is_array());
    }

    #[test]
    fn parse_talk_phonemes_expands_moras_with_pauses() {
        // 「コ」(k+o) + 句間ポーズ + 「ン」(N)。前後 0.1s の pau 付き。
        let body = r#"{
          "accent_phrases":[
            {"moras":[{"text":"コ","consonant":"k","consonant_length":0.05,"vowel":"o","vowel_length":0.1,"pitch":5.5}],
             "accent":1,
             "pause_mora":{"text":"、","consonant":null,"consonant_length":null,"vowel":"pau","vowel_length":0.3,"pitch":0.0}},
            {"moras":[{"text":"ン","consonant":null,"consonant_length":null,"vowel":"N","vowel_length":0.15,"pitch":5.0}],
             "accent":1,"pause_mora":null}
          ],
          "prePhonemeLength":0.1,"postPhonemeLength":0.1
        }"#;
        let ph = parse_talk_phonemes(body, 1.0).unwrap();
        let syms: Vec<&str> = ph.iter().map(|p| p.phoneme.as_str()).collect();
        assert_eq!(syms, vec!["pau", "k", "o", "pau", "N", "pau"]);
        // frame_length = 秒 × FRAME_RATE。
        assert_eq!(ph[1].frame_length, (0.05 * FRAME_RATE).round() as u32);
        assert_eq!(ph[2].frame_length, (0.1 * FRAME_RATE).round() as u32);
        assert_eq!(ph[3].frame_length, (0.3 * FRAME_RATE).round() as u32);
    }

    #[test]
    fn parse_talk_phonemes_divides_length_by_speed() {
        let body = r#"{"accent_phrases":[{"moras":[{"text":"ア","consonant":null,"consonant_length":null,"vowel":"a","vowel_length":0.2,"pitch":5.0}],"accent":1,"pause_mora":null}],"prePhonemeLength":0.0,"postPhonemeLength":0.0}"#;
        let ph = parse_talk_phonemes(body, 2.0).unwrap();
        let a = ph.iter().find(|p| p.phoneme == "a").expect("vowel a present");
        // speed 2.0 → 長さ半分。
        assert_eq!(a.frame_length, (0.2 / 2.0 * FRAME_RATE).round() as u32);
    }

    /// 実 VOICEVOX engine に対する talk 合成の統合テスト (`docs/plan_voicevox_talk.md`)。
    /// engine (localhost:50021) が要るので通常 `cargo test` では無視。実機検証で
    /// `cargo test -p common -- --ignored talk_synth_against_real_engine` で走らせる。
    #[test]
    #[ignore = "requires a running VOICEVOX engine at localhost:50021"]
    fn talk_synth_against_real_engine_produces_audio_and_phonemes() {
        let scales = TalkParams::default();
        // 1) 読み上げ合成: 非無音の mono PCM が返る (= 実際に喋っている)。
        let (samples, sr) = synthesize_talk_for_builtin(
            "こんにちは。テストです。",
            DEFAULT_TALK_SPEAKER_ID,
            &scales,
        )
        .expect("talk synth against real engine");
        assert!(sr >= 24000, "sample_rate looks valid: {sr}");
        assert!(!samples.is_empty(), "got samples");
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        assert!(rms > 0.001, "synthesized talk audio is non-silent (rms={rms})");
        // 2) 口パク phoneme: 母音を含む列が返る (= build_mouth_events に流せる)。
        let phonemes =
            query_talk_phonemes("こんにちは", DEFAULT_TALK_SPEAKER_ID, &scales).expect("talk phonemes");
        let syms: Vec<&str> = phonemes.iter().map(|p| p.phoneme.as_str()).collect();
        assert!(
            phonemes
                .iter()
                .any(|p| matches!(p.phoneme.as_str(), "a" | "i" | "u" | "e" | "o")),
            "phoneme list has vowels: {syms:?}"
        );
        // 先頭・末尾は pau (pre/post phoneme length)。
        assert_eq!(phonemes.first().map(|p| p.phoneme.as_str()), Some("pau"));
        assert_eq!(phonemes.last().map(|p| p.phoneme.as_str()), Some("pau"));
    }

    /// 実 engine で `/speakers` (talk 声一覧) が取れること。
    #[test]
    #[ignore = "requires a running VOICEVOX engine at localhost:50021"]
    fn fetch_speakers_against_real_engine() {
        let speakers = fetch_speakers().expect("fetch /speakers");
        assert!(!speakers.is_empty(), "engine returns talk speakers");
        assert!(
            speakers.iter().any(|s| !s.styles.is_empty()),
            "at least one speaker has styles"
        );
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
