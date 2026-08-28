//! VOICEVOX 音声 **合成** — HTTP (`sing_frame_audio_query` / `frame_synthesis` /
//! `audio_query` / `synthesis`) と WAV decode、FrameAudioQuery のスライス。
//!
//! arch-refactor S5-2 で common::voicevox から分離した (合成は builtin plugin = plugin-host
//! プロセスが唯一の実行場所、reqwest を要する)。`/singers` fetch や口パク phoneme query
//! (= GUI 側の責務) は `daw_gui::voicevox_client`。共有の純粋部分 (query builder /
//! フレーズ分割 / 各 const / Note 型 / ディスクキャッシュ) は `common::voicevox` /
//! `common::voicevox_phrase` / `common::voicevox_cache` / `common::model`。
//!
//! r.md #75 で歌唱は **2 段**に割れている:
//! 1. **塊クエリ** (`fetch_sing_frame_query`) — 60 秒ぶんの楽譜を 1 回投げて
//!    FrameAudioQuery を得る。応答は非決定的なので、キャッシュキーは**入力の楽譜**から作る。
//! 2. **フレーズ合成** (`slice_frame_query` → `frame_synthesis`) — 1 フレーズ ±0.5 秒だけ
//!    切り出して投げる。こちらは決定的なので WAV キャッシュが正しく効く。
//!
//! オーケストレーション (キャッシュ / 継ぎ目 / mix / publish) は
//! [`super::voicevox_render`]。ここは HTTP と純粋な JSON 操作だけを持つ。
//!
//! すべて blocking (background synth thread で呼ぶ)。

use std::io::Cursor;

use anyhow::{Context, Result};

use common::model::TalkParams;
use common::voicevox::{
    OUTPUT_SAMPLE_RATE, QUERY_SPEAKER, TALK_PRE_PHONEME_LENGTH, VOICEVOX_URL,
    normalize_frame_query, urlencoding_encode,
};
use common::voicevox_cache::{CacheKind, VoiceVoxDiskCache, key_for_talk, key_for_talk_query};

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

/// 音声合成 HTTP の timeout。
///
/// r.md #75 以降、合成は **フレーズ単位** (実測 平均 145 ms / 最大 546 ms)、クエリは
/// **塊単位** (60 秒 ≈ 0.55 s、上限 300 秒でも 15 s 程度)。120 秒は engine の
/// コールドスタートと極端に長いフレーズ (休符が無い長大な区間 = 1 フレーズ) のための余裕。
const SYNTH_HTTP_TIMEOUT_SECS: u64 = 120;

/// 合成 HTTP client (timeout 付き) を作る。塊クエリ / フレーズ合成 / talk が共用する。
pub fn synth_client() -> Result<reqwest::blocking::Client, SynthError> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(SYNTH_HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| unreachable(e, "building HTTP client"))
}

// ---------------------------------------------------------------------------
// Sing — 塊クエリ / フレーズ合成
// ---------------------------------------------------------------------------

/// `POST /sing_frame_audio_query` (塊 1 回)。応答を
/// [`common::voicevox::normalize_frame_query`] に通した `FrameAudioQuery` JSON を返す。
///
/// `outputSamplingRate` は**ここで 1 回だけ** [`OUTPUT_SAMPLE_RATE`] に差し替える。
/// 以降の全スライスがこの値を継承するのでフレーズごとに sample rate がぶれず、
/// **キャッシュへ入るのも正規形だけ**になる (daw_gui の口パク query と鍵空間を共有する
/// ので、片方が生 body を put すると他方が 24 kHz の WAV を掴む)。
pub fn fetch_sing_frame_query(
    client: &reqwest::blocking::Client,
    score_json: &str,
) -> Result<String, SynthError> {
    let url = format!("{VOICEVOX_URL}/sing_frame_audio_query?speaker={QUERY_SPEAKER}");
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(score_json.to_owned())
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
    Ok(normalize_frame_query(&body))
}

/// `POST /frame_synthesis` (フレーズ 1 回)。WAV bytes を返す。
pub fn frame_synthesis(
    client: &reqwest::blocking::Client,
    frame_query_json: &str,
    singer_id: u32,
) -> Result<Vec<u8>, SynthError> {
    let url = format!("{VOICEVOX_URL}/frame_synthesis?speaker={singer_id}");
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(frame_query_json.to_owned())
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

/// FrameAudioQuery の frame 総数 (= `f0` 配列長)。
pub fn frame_query_len(fq: &serde_json::Value) -> Result<usize> {
    let arr = fq
        .get("f0")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("FrameAudioQuery has no `f0` array"))?;
    Ok(arr.len())
}

/// FrameAudioQuery を frame 範囲 `[a, b)` で切り出す (純粋関数)。
///
/// - `f0` / `volume` は `[a, b)` をそのままスライス。
/// - `phonemes` は先頭から `frame_length` を積んで区間を出し、`[a, b)` と重なるものだけを
///   境界で切り詰めて残す (完全に外のものは落とす)。
/// - それ以外の field (`volumeScale` / `outputSamplingRate` 等) はそのまま引き継ぐ。
///
/// **phoneme の長さ field は `frame_length` (snake_case)**。engine の `FramePhoneme` が
/// そう定義しており (`voicevox_engine/tts_pipeline/song_engine.py` の
/// `phoneme.frame_length`)、本番の `daw_gui::voicevox_client` も `frame_length` だけを
/// 読んで動いている。同じ応答の中で `outputSamplingRate` / `volumeScale` は camelCase と
/// いう混在があるが、**phoneme 側が camelCase で返った観測は無い**ので推測で両対応を
/// 書かない。欠けていたら `Err` にして表に出す (黙って 0 扱いにすると全 phoneme が
/// 落ちて無音になる)。
pub fn slice_frame_query(fq: &serde_json::Value, a: usize, b: usize) -> Result<String> {
    anyhow::ensure!(a <= b, "slice_frame_query: 逆転した範囲 {a}..{b}");
    let obj = fq
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("FrameAudioQuery is not a JSON object"))?;
    let mut out = obj.clone();

    for key in ["f0", "volume"] {
        let arr = obj
            .get(key)
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("FrameAudioQuery has no `{key}` array"))?;
        let hi = b.min(arr.len());
        let lo = a.min(hi);
        out.insert(key.to_string(), serde_json::Value::Array(arr[lo..hi].to_vec()));
    }

    let phonemes = obj
        .get("phonemes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("FrameAudioQuery has no `phonemes` array"))?;
    let mut kept: Vec<serde_json::Value> = Vec::with_capacity(phonemes.len());
    let mut cursor = 0usize;
    for p in phonemes {
        let len = p
            .get("frame_length")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("FramePhoneme has no `frame_length`"))?
            as usize;
        let start = cursor;
        let end = cursor.saturating_add(len);
        cursor = end;
        let s = start.max(a);
        let e = end.min(b);
        if e <= s {
            continue;
        }
        let mut q = p.clone();
        if let Some(o) = q.as_object_mut() {
            o.insert("frame_length".into(), serde_json::json!(e - s));
        }
        kept.push(q);
    }
    out.insert("phonemes".to_string(), serde_json::Value::Array(kept));

    serde_json::to_string(&serde_json::Value::Object(out))
        .context("serializing sliced FrameAudioQuery")
}

// ---------------------------------------------------------------------------
// Talk
// ---------------------------------------------------------------------------

/// `/audio_query` 応答 JSON を取る (ディスクキャッシュ付き)。
///
/// 応答は `(text, speaker)` の純粋関数で、speed / pitch 等は後から patch するので
/// 鍵に混ぜない。**同じ鍵空間を daw_gui の口パク (`query_talk_phonemes`) と共有する**
/// ので、両者とも生の応答 body をそのまま置く (patch は読み手が行う)。
fn fetch_talk_query(
    client: &reqwest::blocking::Client,
    text: &str,
    speaker_id: u32,
) -> Result<String, SynthError> {
    let cache = VoiceVoxDiskCache::production();
    let key = key_for_talk_query(text, speaker_id);
    if let Some(hit) = cache.as_ref().and_then(|c| c.get(key, CacheKind::Json))
        && let Ok(text) = String::from_utf8(hit)
    {
        return Ok(text);
    }
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
    if let Some(c) = cache.as_ref() {
        c.put(key, CacheKind::Json, body.as_bytes());
    }
    Ok(body)
}

/// `text` を talk 合成して WAV bytes を返す。`/audio_query` → (TalkParams patch) → `/synthesis`。
/// `scales` の話速/音高/抑揚/音量 を audio_query 応答に適用してから synthesis する。blocking。
fn synthesize_talk(
    client: &reqwest::blocking::Client,
    text: &str,
    speaker_id: u32,
    scales: &TalkParams,
) -> Result<Vec<u8>, SynthError> {
    let body = fetch_talk_query(client, text, speaker_id)?;

    let patched =
        apply_talk_params(&body, scales).map_err(|e| SynthError::Rejected(format!("{e:#}")))?;

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
    let wav = if let Some(hit) = cache.as_ref().and_then(|c| c.get(cache_key, CacheKind::Wav)) {
        tracing::info!(cache_key, "VOICEVOX talk cache hit (HTTP skip)");
        hit
    } else {
        let client = synth_client()?;
        let wav = synthesize_talk(&client, text, speaker_id, scales)?;
        if let Some(c) = cache.as_ref() {
            c.put(cache_key, CacheKind::Wav, &wav);
        }
        wav
    };
    decode_wav_to_f32(&wav).map_err(|e| SynthError::Rejected(format!("{e:#}")))
}

/// (talk) `TalkParams` を `/audio_query` 応答 JSON に適用して再シリアライズする。
/// `outputSamplingRate` も 48000 に揃える。
///
/// r.md #39: `prePhonemeLength` を [`TALK_PRE_PHONEME_LENGTH`] (= 0) で **必ず上書き**
/// する。engine 既定 (0.1s) のままだと wav 先頭に話速依存の無音が入り、「クリップ位置 =
/// 発話開始」が話速で ±100ms 動く。無音を推定して差し引くのではなく、そもそも作らない。
/// `postPhonemeLength` は語尾の余韻なので触らない。
fn apply_talk_params(audio_query_json: &str, scales: &TalkParams) -> Result<String> {
    let mut v: serde_json::Value =
        serde_json::from_str(audio_query_json).context("parsing audio_query response JSON")?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("audio_query response is not a JSON object"))?;
    obj.insert(
        "prePhonemeLength".into(),
        serde_json::json!(TALK_PRE_PHONEME_LENGTH),
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn apply_talk_params_sets_all_scales_and_keeps_query() {
        let query = r#"{"accent_phrases":[{"moras":[]}],"speedScale":1.0,"pitchScale":0.0,"intonationScale":1.0,"volumeScale":1.0,"prePhonemeLength":0.1,"postPhonemeLength":0.1,"outputSamplingRate":24000}"#;
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
        // r.md #39: 先頭無音は必ず 0 に上書き (話速で伸縮する engine 既定 0.1s を消す)。
        assert!(v["prePhonemeLength"].as_f64().unwrap().abs() < 1e-12);
        // 語尾の余韻 (postPhonemeLength) は engine 既定のまま。
        assert!((v["postPhonemeLength"].as_f64().unwrap() - 0.1).abs() < 1e-12);
    }

    /// frames = 10 の合成 FrameAudioQuery (phoneme は 3/4/3)。
    fn fq_fixture() -> Value {
        serde_json::json!({
            "f0": (0..10).map(f64::from).collect::<Vec<f64>>(),
            "volume": (0..10).map(|i| f64::from(i) * 0.1).collect::<Vec<f64>>(),
            "phonemes": [
                {"phoneme": "pau", "frame_length": 3},
                {"phoneme": "r",   "frame_length": 4},
                {"phoneme": "a",   "frame_length": 3},
            ],
            "volumeScale": 1.0,
            "outputSamplingRate": 48000,
        })
    }

    #[test]
    fn frame_query_len_reads_f0() {
        assert_eq!(frame_query_len(&fq_fixture()).unwrap(), 10);
        let bad = serde_json::json!({"volume": []});
        assert!(frame_query_len(&bad).is_err());
    }

    #[test]
    fn slice_frame_query_cuts_arrays_and_phonemes() {
        let fq = fq_fixture();
        let out: Value = serde_json::from_str(&slice_frame_query(&fq, 2, 8).unwrap()).unwrap();
        // f0 / volume は [a, b) をそのまま。
        assert_eq!(out["f0"].as_array().unwrap().len(), 6);
        assert!((out["f0"][0].as_f64().unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(out["volume"].as_array().unwrap().len(), 6);
        // phoneme の合計 frame_length は b - a。
        let ph = out["phonemes"].as_array().unwrap();
        let total: u64 = ph
            .iter()
            .map(|p| p["frame_length"].as_u64().unwrap())
            .sum();
        assert_eq!(total, 6);
        // 境界で切り詰められる: pau 3→1、r 4→4、a 3→1。
        assert_eq!(ph.len(), 3);
        assert_eq!(ph[0]["frame_length"].as_u64().unwrap(), 1);
        assert_eq!(ph[1]["frame_length"].as_u64().unwrap(), 4);
        assert_eq!(ph[2]["frame_length"].as_u64().unwrap(), 1);
        // 範囲外の phoneme は落ちる。
        let head: Value = serde_json::from_str(&slice_frame_query(&fq, 0, 3).unwrap()).unwrap();
        assert_eq!(head["phonemes"].as_array().unwrap().len(), 1);
        // 他 field はそのまま引き継ぐ (= 48 kHz 指定が保たれる)。
        assert_eq!(out["outputSamplingRate"].as_u64().unwrap(), 48_000);
        assert!((out["volumeScale"].as_f64().unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn slice_frame_query_rejects_phonemes_without_frame_length() {
        // 黙って 0 扱いにすると全 phoneme が落ちて無音になるので、必ず Err にする。
        let fq = serde_json::json!({
            "f0": [0.0, 1.0],
            "volume": [0.0, 0.1],
            "phonemes": [{"phoneme": "a", "frameLength": 2}],
        });
        assert!(slice_frame_query(&fq, 0, 2).is_err());
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
