// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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
/// attack/release envelopes. 合成 WAV の配置 (plugin_host) も口パクの配置 (daw_gui) も
/// [`sing_head_beat`] 経由でこの値を差し引くので、音声と口が同じ位置に揃う。
pub const REST_FRAMES: u32 = 10;

/// (talk) `/audio_query` 応答の `prePhonemeLength` に **必ず上書きする** 値 (秒)。
///
/// engine 既定 (0.1s) のままだと合成 WAV の先頭に無音が入り、しかもその長さは
/// `speedScale` で割られる (話速 0.5 → 202.7ms / 1.0 → 96.0ms / 1.5 → 64.0ms)。
/// 「クリップ位置 = 発話開始」を話速に依らず保つため 0 にして無音を **推定せず消す**。
/// 合成側 (`daw_plugin_host::builtin::voicevox_synth::apply_talk_params`) と口パク側
/// (`daw_gui::voicevox_client::parse_talk_phonemes`) が同じ値を見る SSoT
/// (r.md #39)。`postPhonemeLength` は余韻なので engine 既定のまま。
pub const TALK_PRE_PHONEME_LENGTH: f64 = 0.0;

// ---------------------------------------------------------------------------
// frame ↔ beat / sample 変換 (FRAME_RATE を持つここが SSoT)
// ---------------------------------------------------------------------------

/// VOICEVOX frame 数 → beats。`beats = frames / FRAME_RATE * bpm / 60`。
#[must_use]
pub fn frames_to_beats(frames: f64, bpm: f32) -> f64 {
    frames / FRAME_RATE * f64::from(bpm) / 60.0
}

/// VOICEVOX frame 数 → 合成 WAV の sample 数 (1 frame = `sample_rate / FRAME_RATE` sample)。
#[must_use]
pub fn frames_to_samples(frames: f64, sample_rate: u32) -> f64 {
    frames / FRAME_RATE * f64::from(sample_rate)
}

/// (talk) `prePhonemeLength` 由来の先頭無音の frame 数。engine は秒を frame に
/// 丸めてから合成するので、実際の無音長は `round(pre / speed * FRAME_RATE)` frame。
/// 配置 (plugin_host) と口パク (daw_gui) が同じ量を差し引くための SSoT。
/// [`TALK_PRE_PHONEME_LENGTH`] が 0 の現行設定では常に 0。
#[must_use]
pub fn talk_pre_silence_frames(speed_scale: f32) -> u32 {
    let speed = f64::from(speed_scale).max(0.01);
    (TALK_PRE_PHONEME_LENGTH / speed * FRAME_RATE).round().max(0.0) as u32
}

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

/// [`build_sing_query`] の出力。
///
/// `note_frames` は「入力 `notes` 内の index → その note の音声が始まる **絶対 frame
/// 位置**」(query 先頭 = frame 0、先頭 rest 込み)。合成 WAV / phoneme 列の実位置その
/// もので、sample 位置は [`frames_to_samples`] で得られる。歌詞・長さ・重なりの都合で
/// query に載らなかった note は含まれない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingQuery {
    /// `POST /sing_frame_audio_query` に渡す JSON body。
    pub json: String,
    /// `(notes 内 index, 絶対 frame 位置)`、query 内の出現順。
    pub note_frames: Vec<(usize, i64)>,
}

/// query に載る (= 実際に歌われる) note か。長さ 0 / pitch 0 は VOICEVOX に渡せない。
fn is_sung(n: &Note) -> bool {
    n.duration_beats > 0.0 && n.pitch > 0
}

/// [`build_sing_query`] が query の基準 (= frame [`REST_FRAMES`]) に置く note の
/// `start_beat`。query に載る note が 1 つも無ければ `None`。
///
/// 音声配置 (daw_plugin_host) と口パク配置 (daw_gui) が **同じ基準** を使うための SSoT。
#[must_use]
pub fn sing_base_beat(notes: &[Note]) -> Option<f64> {
    let m = notes
        .iter()
        .filter(|n| is_sung(n))
        .map(|n| n.start_beat)
        .fold(f64::INFINITY, f64::min);
    m.is_finite().then_some(m)
}

/// 歌唱 WAV / phoneme 列の **frame 0** が来る beat。[`build_sing_query`] は基準 note
/// ([`sing_base_beat`]) の [`REST_FRAMES`] 手前から書き出すので、音声も口パクもこの
/// beat を先頭に置けば揃う (r.md #39: 「buffer index = 曲位置」の単一契約)。
#[must_use]
pub fn sing_head_beat(base_beat: f64, bpm: f32) -> f64 {
    base_beat - frames_to_beats(f64::from(REST_FRAMES), bpm)
}

/// 基準 note からの拍オフセット → VOICEVOX frame **位置**。
///
/// 位置変換なので **下限クランプを持たない**。旧 `seconds_to_frames` は長さ用の
/// `.max(1.0)` を位置変換にも適用しており、先頭 note の位置 0 が 1 に化けて
/// 2 音目以降が丸ごと 1 frame (= 10.7ms) 早まっていた (r.md #39 原因 1)。
/// 長さの下限 1 frame は下の正規化が担う。
fn beat_offset_to_frame(beat_offset: f64, seconds_per_beat: f64) -> i64 {
    (beat_offset * seconds_per_beat * FRAME_RATE).round() as i64
}

/// `key=null` の rest エントリ。
fn rest_entry(id: &str, frames: i64) -> String {
    format!(r#"{{"id":"{id}","key":null,"frame_length":{frames},"lyric":""}}"#)
}

/// Builds the JSON body for `POST /sing_frame_audio_query`.
///
/// **絶対 frame 位置ベース**: まず各 note の位置 / 終端を「基準 note からの拍オフセット
/// を frame へ丸めた絶対値」で確定し、その後に frame_length 列 (rest + note の並び) へ
/// 落とす。これにより「重なった note」「1 frame 未満の note」があっても **以降の note
/// 位置が押し出されない** (旧実装は telescoping が壊れて累積ずれを起こしていた)。
///
/// 重なりは Reaper 流に **前の note を次の開始で切り詰める**。切り詰めで 1 frame 未満に
/// なった note は落とす (VOICEVOX は `frame_length >= 1` を要求し、押し出さずに 1 frame
/// を確保する方法が他に無いため)。
///
/// 残差は VOICEVOX の 93.75fps 分解能そのもの (±0.5 frame = ±5.33ms、バイアス 0) で、
/// これは VOICEVOX 本体エディタと同じ精度。
pub fn build_sing_query(notes: &[Note], bpm: f32) -> SingQuery {
    let mut parts: Vec<String> = Vec::new();
    let rest = i64::from(REST_FRAMES);

    // Leading rest (gives the synth a moment of silence for the attack).
    parts.push(rest_entry("rest_start", rest));

    // Sort notes by start_beat — `ClipContent.notes` is unordered by contract, and this
    // builder requires monotonic timing. 安定ソートなので同時刻は入力順を保つ。
    let mut sorted: Vec<(usize, &Note)> = notes
        .iter()
        .enumerate()
        .filter(|(_, n)| is_sung(n))
        .collect();
    sorted.sort_by(|a, b| {
        a.1.start_beat
            .partial_cmp(&b.1.start_beat)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let Some(&(_, first)) = sorted.first() else {
        parts.push(rest_entry("rest_end", rest));
        return SingQuery {
            json: format!(r#"{{"notes":[{}]}}"#, parts.join(",")),
            note_frames: Vec::new(),
        };
    };

    let seconds_per_beat = 60.0 / f64::from(bpm);
    let base_beat = first.start_beat;
    // bpm <= 0 の壊れた song では `seconds_per_beat` が inf になり frame が飽和するので、
    // 先頭 rest の加算も saturating で回す (panic させない)。
    let pos_of = |beat: f64| {
        rest.saturating_add(beat_offset_to_frame(beat - base_beat, seconds_per_beat))
    };

    // 絶対位置 → 重なり解決 (前を切り詰め / 潰れたら落とす)。押し出しは一切しない。
    let mut kept: Vec<(usize, i64, i64)> = Vec::with_capacity(sorted.len());
    for (idx, note) in sorted {
        let pos = pos_of(note.start_beat);
        let end = pos_of(note.start_beat + note.duration_beats);
        while let Some(&(_, prev_pos, prev_end)) = kept.last() {
            let truncated = prev_end.min(pos);
            // 位置は f64 → i64 の飽和キャストなので、壊れた project の巨大な
            // start_beat でも panic しないよう saturating で回す。
            if truncated.saturating_sub(prev_pos) >= 1 {
                if let Some(prev) = kept.last_mut() {
                    prev.2 = truncated;
                }
                break;
            }
            // 切り詰めで 1 frame 未満に潰れた → 落とす (押し出さない)。
            kept.pop();
        }
        kept.push((idx, pos, end.max(pos.saturating_add(1))));
    }

    // 長音「ー」を「直前の母音を伸ばす」で解決するため、直前ノートの母音を持ち越す。
    let mut carried_vowel: Option<char> = None;
    let mut note_frames: Vec<(usize, i64)> = Vec::with_capacity(kept.len());
    let mut cursor = rest;
    for (i, &(idx, pos, end)) in kept.iter().enumerate() {
        if pos > cursor {
            parts.push(rest_entry(&format!("rest{i}"), pos.saturating_sub(cursor)));
        }
        // 長音符「ー」は VOICEVOX の sing 合成が単独歌詞として弾く (400
        // `lyricが不正です: ー`)。直前の母音へ解決してから流す。
        let raw_lyric = notes[idx].lyric.as_deref().unwrap_or("ら");
        let lyric = resolve_sing_lyric(raw_lyric, &mut carried_vowel);
        let escaped = lyric.replace('\\', "\\\\").replace('"', "\\\"");
        parts.push(format!(
            r#"{{"id":"note{}","key":{},"frame_length":{},"lyric":"{}"}}"#,
            i,
            notes[idx].pitch,
            end.saturating_sub(pos),
            escaped
        ));
        note_frames.push((idx, pos));
        cursor = end;
    }

    // Trailing rest
    parts.push(rest_entry("rest_end", rest));

    SingQuery {
        json: format!(r#"{{"notes":[{}]}}"#, parts.join(",")),
        note_frames,
    }
}

/// 長音符 (prolonged sound mark)。全角 `ー` (U+30FC) と半角 `ｰ` (U+FF70)。
fn is_prolongation(ch: char) -> bool {
    matches!(ch, 'ー' | 'ｰ')
}

/// 仮名 1 文字の母音を **ひらがな母音** (あ/い/う/え/お) で返す。母音を持たない
/// 文字 (ん/っ/長音符/非仮名) は `None`。長音「ー」を直前の母音へ解決するために使う。
/// 濁点・半濁点・小書き・カタカナも同じ母音へ畳む (例: `ぎゃ`→`あ`, `シュ`→`う`)。
pub fn kana_vowel(ch: char) -> Option<char> {
    match ch {
        'あ' | 'か' | 'さ' | 'た' | 'な' | 'は' | 'ま' | 'や' | 'ら' | 'わ' | 'が' | 'ざ' | 'だ'
        | 'ば' | 'ぱ' | 'ぁ' | 'ゃ' | 'ゎ' | 'ア' | 'カ' | 'サ' | 'タ' | 'ナ' | 'ハ' | 'マ' | 'ヤ'
        | 'ラ' | 'ワ' | 'ガ' | 'ザ' | 'ダ' | 'バ' | 'パ' | 'ァ' | 'ャ' | 'ヮ' => Some('あ'),
        'い' | 'き' | 'し' | 'ち' | 'に' | 'ひ' | 'み' | 'り' | 'ぎ' | 'じ' | 'ぢ' | 'び' | 'ぴ'
        | 'ぃ' | 'ゐ' | 'イ' | 'キ' | 'シ' | 'チ' | 'ニ' | 'ヒ' | 'ミ' | 'リ' | 'ギ' | 'ジ' | 'ヂ'
        | 'ビ' | 'ピ' | 'ィ' | 'ヰ' => Some('い'),
        'う' | 'く' | 'す' | 'つ' | 'ぬ' | 'ふ' | 'む' | 'ゆ' | 'る' | 'ぐ' | 'ず' | 'づ' | 'ぶ'
        | 'ぷ' | 'ぅ' | 'ゅ' | 'ゔ' | 'ウ' | 'ク' | 'ス' | 'ツ' | 'ヌ' | 'フ' | 'ム' | 'ユ' | 'ル'
        | 'グ' | 'ズ' | 'ヅ' | 'ブ' | 'プ' | 'ゥ' | 'ュ' | 'ヴ' => Some('う'),
        'え' | 'け' | 'せ' | 'て' | 'ね' | 'へ' | 'め' | 'れ' | 'げ' | 'ぜ' | 'で' | 'べ' | 'ぺ'
        | 'ぇ' | 'ゑ' | 'エ' | 'ケ' | 'セ' | 'テ' | 'ネ' | 'ヘ' | 'メ' | 'レ' | 'ゲ' | 'ゼ' | 'デ'
        | 'ベ' | 'ペ' | 'ェ' | 'ヱ' => Some('え'),
        'お' | 'こ' | 'そ' | 'と' | 'の' | 'ほ' | 'も' | 'よ' | 'ろ' | 'を' | 'ご' | 'ぞ' | 'ど'
        | 'ぼ' | 'ぽ' | 'ぉ' | 'ょ' | 'オ' | 'コ' | 'ソ' | 'ト' | 'ノ' | 'ホ' | 'モ' | 'ヨ' | 'ロ'
        | 'ヲ' | 'ゴ' | 'ゾ' | 'ド' | 'ボ' | 'ポ' | 'ォ' | 'ョ' => Some('お'),
        _ => None,
    }
}

/// 歌唱クエリ用に 1 note の歌詞を解決する。VOICEVOX の `sing_frame_audio_query` は
/// 各 note の歌詞に有効な 1 モーラを要求し、裸の長音符「ー」を 400 で弾く。歌唱では
/// 長音は日常的な記法なので、ここで「ー」を発声可能な仮名へ変換する:
///
/// - 「ー」単独 (= 音を伸ばす継続 note) → 直前ノートの母音 (`ら`→`あ`, `き`→`い`, …)。
///   直前が無い / 母音を取れない場合は `あ` にフォールバック。
/// - 通常の歌詞 → そのまま流し、末尾仮名の母音を「直前の母音」として記憶する。
/// - 歌詞中に「ー」が混じる 1 note (例: `らー`) → 長音符を落として実仮名部分を返す
///   (その note 自身の frame_length が伸ばしを表現するので、母音の再掲は不要)。
fn resolve_sing_lyric(raw: &str, carried: &mut Option<char>) -> String {
    if !raw.chars().any(is_prolongation) {
        // 通常経路: 末尾の母音持ち仮名から carried を更新し、そのまま返す。
        for ch in raw.chars() {
            if let Some(v) = kana_vowel(ch) {
                *carried = Some(v);
            }
        }
        return raw.to_string();
    }
    // 「ー」を含む。実仮名 (長音符以外) を抽出。
    let real: String = raw.chars().filter(|c| !is_prolongation(*c)).collect();
    if !real.is_empty() {
        // 例: `らー` → `ら` (伸ばしは frame_length が担う)。
        for ch in real.chars() {
            if let Some(v) = kana_vowel(ch) {
                *carried = Some(v);
            }
        }
        return real;
    }
    // 純粋な長音 (`ー` / `ーー`) → 直前の母音 1 文字。
    carried.unwrap_or('あ').to_string()
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

    fn parse_query(q: &SingQuery) -> Vec<(Option<i64>, i64, String)> {
        let v: Value = serde_json::from_str(&q.json).expect("query is not valid JSON");
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
        let _: Value = serde_json::from_str(&q.json).expect("invalid JSON output");
    }

    /// 歌詞ヘルパ: pitch/duration は固定で lyric だけ変えて query を作る。
    fn note_with_lyric(start_beat: f64, lyric: &str) -> Note {
        Note {
            id: 0,
            start_beat,
            duration_beats: 1.0,
            pitch: 60,
            velocity: 100,
            lyric: Some(lyric.into()),
            muted: false,
        }
    }

    #[test]
    fn lone_prolongation_resolves_to_previous_vowel() {
        // 「ら」「ー」「ー」→ ら / あ / あ (裸の「ー」を弾かせない)。
        let notes = vec![
            note_with_lyric(0.0, "ら"),
            note_with_lyric(1.0, "ー"),
            note_with_lyric(2.0, "ー"),
        ];
        let entries = parse_query(&build_sing_query(&notes, 120.0));
        let lyrics: Vec<&str> = entries.iter().map(|e| e.2.as_str()).collect();
        assert_eq!(lyrics, vec!["", "ら", "あ", "あ", ""]);
    }

    #[test]
    fn prolongation_uses_consonant_row_vowel() {
        // 「き」→い, 「く」→う, 「け」→え, 「こ」→お の各行を伸ばす。
        for (base, vowel) in [("き", "い"), ("く", "う"), ("け", "え"), ("こ", "お")] {
            let notes = vec![note_with_lyric(0.0, base), note_with_lyric(1.0, "ー")];
            let entries = parse_query(&build_sing_query(&notes, 120.0));
            assert_eq!(entries[2].2, vowel, "base={base} should hold vowel {vowel}");
        }
    }

    #[test]
    fn leading_prolongation_falls_back_to_a() {
        // 直前が無い先頭「ー」→ あ (フォールバック)。合成を止めないための保険。
        let notes = vec![note_with_lyric(0.0, "ー")];
        let entries = parse_query(&build_sing_query(&notes, 120.0));
        assert_eq!(entries[1].2, "あ");
    }

    #[test]
    fn small_kana_mora_vowel_and_inline_prolongation() {
        // 拗音末尾の母音で伸ばす: 「きゃ」→あ。1 note 内混在「らー」→「ら」。
        let notes = vec![note_with_lyric(0.0, "きゃ"), note_with_lyric(1.0, "ー")];
        let entries = parse_query(&build_sing_query(&notes, 120.0));
        assert_eq!(entries[1].2, "きゃ");
        assert_eq!(entries[2].2, "あ");

        let notes = vec![note_with_lyric(0.0, "らー")];
        let entries = parse_query(&build_sing_query(&notes, 120.0));
        assert_eq!(entries[1].2, "ら");
    }

    #[test]
    fn no_bare_prolongation_survives_in_query() {
        // どんな並びでも出力 JSON に裸の「ー」を残さない (= 400 の根絶)。
        let notes = vec![
            note_with_lyric(0.0, "ん"), // 母音なし → carried 変わらず
            note_with_lyric(1.0, "ー"), // 直前 carried が無い → あ
            note_with_lyric(2.0, "そ"),
            note_with_lyric(3.0, "ー"), // → お
        ];
        let entries = parse_query(&build_sing_query(&notes, 120.0));
        assert!(
            entries.iter().all(|e| !e.2.contains('ー')),
            "query still contains a bare prolongation: {entries:?}"
        );
        assert_eq!(entries.last().unwrap().0, None); // trailing rest
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

    // ---- 絶対 frame 位置 (r.md #39) -----------------------------------------

    /// pitch/lyric 固定で位置と長さだけ変える helper。
    fn note_at(start_beat: f64, duration_beats: f64) -> Note {
        Note {
            id: 0,
            start_beat,
            duration_beats,
            pitch: 60,
            velocity: 100,
            lyric: Some("ら".into()),
            muted: false,
        }
    }

    /// 各 note の理論 frame 位置 (丸め前)。`REST_FRAMES + 拍オフセット × 秒 × 93.75`。
    fn ideal_frame(start_beat: f64, base_beat: f64, bpm: f64) -> f64 {
        f64::from(REST_FRAMES) + (start_beat - base_beat) * (60.0 / bpm) * FRAME_RATE
    }

    #[test]
    fn note_positions_are_absolute_with_no_systematic_bias() {
        // 旧実装は `seconds_to_frames` の `.max(1.0)` が先頭 note の位置 0 を 1 に
        // 化けさせ、2 音目以降が丸ごと 1 frame (10.7ms) 早まっていた (r.md #39 原因 1)。
        // 位置は絶対 frame へ丸めるだけなので、誤差は ±0.5 frame・バイアス 0 になる。
        let notes: Vec<Note> = (0..4).map(|i| note_at(f64::from(i), 1.0)).collect();
        let q = build_sing_query(&notes, 120.0);
        assert_eq!(q.note_frames, vec![(0, 10), (1, 57), (2, 104), (3, 151)]);
        for (idx, frame) in &q.note_frames {
            let ideal = ideal_frame(notes[*idx].start_beat, 0.0, 120.0);
            assert!(
                (*frame as f64 - ideal).abs() <= 0.5,
                "note {idx}: frame={frame} ideal={ideal}"
            );
        }
    }

    #[test]
    fn rests_between_notes_do_not_accumulate_error() {
        // 休符を挟んでも誤差が積み上がらない (旧実装は休符ごとに更にずれた)。
        let starts = [0.0, 1.5, 3.0, 4.5];
        let notes: Vec<Note> = starts.iter().map(|&s| note_at(s, 1.0)).collect();
        let q = build_sing_query(&notes, 120.0);
        for (idx, frame) in &q.note_frames {
            let ideal = ideal_frame(starts[*idx], 0.0, 120.0);
            assert!(
                (*frame as f64 - ideal).abs() <= 0.5,
                "note {idx}: frame={frame} ideal={ideal}"
            );
        }
        // 先頭 note は必ず REST_FRAMES ちょうど。
        assert_eq!(q.note_frames[0], (0, i64::from(REST_FRAMES)));
    }

    #[test]
    fn overlapping_notes_truncate_the_previous_without_pushing_later_ones() {
        // note0 [0,2) に note1 [1.5,2.5) が食い込む。前を切り詰めるだけで、
        // note1 の位置は絶対値のまま (旧実装は rest を入れず telescoping が壊れ、
        // 重なった分だけ以降が後ろへずれていた = r.md #39 付随 (b))。
        let notes = vec![note_at(0.0, 2.0), note_at(1.5, 1.0)];
        let q = build_sing_query(&notes, 120.0);
        assert_eq!(q.note_frames, vec![(0, 10), (1, 80)]);
        let entries = parse_query(&q);
        // rest_start(10), note0(70 = 80-10), note1(47), rest_end(10)。gap rest 無し。
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[1].1, 70, "前 note が次の開始で切り詰められる");
        assert_eq!(entries[2].1, 47);
    }

    #[test]
    fn sub_frame_note_keeps_one_frame_and_does_not_delay_the_next() {
        // 1 frame (10.7ms) 未満の note も VOICEVOX 要求どおり 1 frame は確保するが、
        // 次の note の位置は押し出さない。
        let notes = vec![note_at(0.0, 0.005), note_at(1.0, 1.0)];
        let q = build_sing_query(&notes, 120.0);
        assert_eq!(q.note_frames, vec![(0, 10), (1, 57)]);
        let entries = parse_query(&q);
        assert_eq!(entries[1].1, 1, "最小 1 frame");
        assert_eq!(entries[2].1, 46, "gap rest = 57 - 11");
    }

    #[test]
    fn notes_starting_at_the_same_frame_drop_the_collapsed_one() {
        // 同時刻 2 note (VOICEVOX sing は単声)。切り詰めで 0 frame になる方を落とし、
        // 後続位置は保つ (押し出すと以降が全部ずれるため)。
        let notes = vec![note_at(0.0, 1.0), note_at(0.0, 2.0), note_at(2.0, 1.0)];
        let q = build_sing_query(&notes, 120.0);
        assert_eq!(q.note_frames, vec![(1, 10), (2, 104)]);
    }

    #[test]
    fn sing_base_beat_ignores_unsingable_notes() {
        // 長さ 0 / pitch 0 は query に載らないので基準にもならない。
        let mut zero_len = note_at(0.0, 0.0);
        zero_len.pitch = 60;
        let mut no_pitch = note_at(0.5, 1.0);
        no_pitch.pitch = 0;
        let notes = vec![zero_len, no_pitch, note_at(2.0, 1.0)];
        assert_eq!(sing_base_beat(&notes), Some(2.0));
        assert_eq!(build_sing_query(&notes, 120.0).note_frames, vec![(2, 10)]);
        assert_eq!(sing_base_beat(&[]), None);
    }

    #[test]
    fn sing_head_beat_is_rest_frames_before_the_base_note() {
        // 120 BPM で REST_FRAMES(10) = 0.10667s = 0.21333 拍。
        let head = sing_head_beat(4.0, 120.0);
        assert!((head - (4.0 - 10.0 / FRAME_RATE * 2.0)).abs() < 1e-12, "head={head}");
        // frames_to_beats / frames_to_samples の整合 (1 frame = sr/93.75 sample)。
        assert!((frames_to_samples(10.0, 48_000) - 5120.0).abs() < 1e-9);
    }

    #[test]
    fn talk_pre_silence_is_zero_at_every_speed() {
        // 先頭無音は「推定せず消す」= prePhonemeLength 0 (r.md #39 原因 2)。
        for speed in [0.5, 0.8, 1.0, 1.2, 1.5, 2.0] {
            assert_eq!(talk_pre_silence_frames(speed), 0, "speed={speed}");
        }
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
