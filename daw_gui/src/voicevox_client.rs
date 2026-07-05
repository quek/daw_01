//! VOICEVOX HTTP client — `/singers` `/speakers` fetch + 口パク (lip-sync) phoneme query。
//!
//! arch-refactor S5-2 で common::voicevox から分離した (reqwest を要する GUI 側の責務)。
//! 音声 **合成** (frame_synthesis / synthesis) は daw_plugin_host の builtin
//! (`daw_plugin_host::builtin::voicevox_synth`) が持つ。共有の純粋部分 (query builder /
//! Phoneme / 各種 const / urlencoding) は `common::voicevox`。
//!
//! すべて blocking (background thread / `cx.spawn` worker で呼ぶ前提)。

use anyhow::{Context, Result};

use common::model::{Note, TalkParams};
use common::voicevox::{
    FRAME_RATE, Phoneme, QUERY_SPEAKER, VOICEVOX_URL, build_sing_query, urlencoding_encode,
};

/// `/singers` レスポンスの 1 entry。 1 キャラクターと、 そのスタイル (= sing 用 style id 群)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoiceVoxSinger {
    pub name: String,
    pub styles: Vec<VoiceVoxStyle>,
}

/// 各キャラクターのスタイル (= 表情 / 歌唱モード)。 `id` が `synthesize_song` に渡す singer_id。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoiceVoxStyle {
    pub id: u32,
    pub name: String,
}

/// VOICEVOX engine の `/singers` を叩いて全キャラクター + スタイル一覧を取得。blocking、5 秒
/// timeout。engine 未起動なら `Err`。起動直後 (= まだ ready でない) なら 5 秒 timeout 内で接続
/// エラー、リトライ可能。
pub fn fetch_singers() -> anyhow::Result<Vec<VoiceVoxSinger>> {
    fetch_voices("/singers")
}

/// (talk) VOICEVOX engine の `/speakers` を叩いて全 talk キャラクター + スタイル一覧を取得する
/// (`docs/plan_voicevox_talk.md` §4)。レスポンス構造は `/singers` と同型。各 style の `id` が
/// `/audio_query` + `/synthesis` に渡す talk speaker id。
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

/// 口パク (lip-sync) 用: `sing_frame_audio_query` **だけ** を叩いて phoneme 列を取得する。
/// `frame_synthesis` は呼ばない (口パクに音声 WAV は不要、phoneme タイミングだけ要る = 軽い)。
/// `build_sing_query` を音声合成と共用するので、得られる phoneme は実際に鳴る歌唱と完全に一致する
/// (= 口と音声が同期)。
///
/// 戻り値は先頭/末尾の `REST_FRAMES` 分の `pau` も含む VOICEVOX 生の phoneme 列 (frame 0 起点)。
/// beat への配置は `common::lipsync` 側で行う。
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

/// (talk) `text` の `/audio_query` を叩き、phoneme 列を返す (口パク用、`/synthesis` は呼ばない)。
/// 歌唱の `query_phonemes` と同じ `Vec<Phoneme>` を返すので、生成先 clip への配置は
/// `common::lipsync::build_mouth_events` をそのまま再利用できる。先頭/末尾の `pau` を含む。
/// `scales.speed_scale` で frame_length を割り、実際に鳴る (= speed 適用後の) 音声と口を揃える。
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

/// `/audio_query` 応答 JSON (`accent_phrases[].moras[]` + pre/post phoneme length + pause_mora)
/// を `Vec<Phoneme>` へ変換する純粋関数。各モーラは子音 (あれば) + 母音の 2 phoneme に展開し、
/// 長さ (秒) を `FRAME_RATE` × `1/speed` で frame へ。先頭に `prePhonemeLength`、末尾に
/// `postPhonemeLength` 由来の `pau` を置く。
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
