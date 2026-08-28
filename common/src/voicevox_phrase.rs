//! 歌唱合成の**分割単位** — フレーズ (合成の単位) と 塊 (クエリの単位)。
//!
//! - フレーズ = 隙間ゼロで連続する note の極大列 (本家 `extractPhraseNotes` と同一定義)、
//!   かつ声 (speaker) が変わる位置でも切る。**クリップ境界では切らない**。
//! - 塊 = 連続する複数フレーズ。`/sing_frame_audio_query` を 1 回投げる単位。
//!   既定 60 秒 (docs/plan_rmd_75_voicevox_phrase.md §0 (C) の実測)。
//!   切れ目は**窓の中で最も長い休符**に置く。
//!
//! ここで定義する型はすべて**プロセス内の計算専用**で IPC を渡らない
//! (アーキ不変条件 7 / `common/build.rs` の `WIRE_SOURCES` に足さない)。
//!
//! 分割の一次情報と実測は docs/plan_rmd_75_voicevox_phrase.md を参照。

use std::ops::Range;

use crate::model::Note;
use crate::plugin_metadata::NoteMetadata;
use crate::voicevox::{self, NotePlacement};

/// 塊 (= 1 クエリ) の既定の長さ (秒)。実測で 30 秒はばらつきが倍、120 秒は改善せず
/// クエリ時間だけ 38% 増える。**60 秒が曲がり角**。
pub const DEFAULT_CHUNK_SECS: f32 = 60.0;
/// 設定で受け付ける下限。
pub const MIN_CHUNK_SECS: f32 = 15.0;
/// 設定で受け付ける上限。300 秒を超えると engine の RAM / GPU が持たない
/// (361 秒でクエリ 18.4 秒、480 秒で engine が落ちる)。
pub const MAX_CHUNK_SECS: f32 = 300.0;

/// フレーズ切り出しのパディング (前後、秒)。実測で 0.5 s が最良
/// (0.12 s → 2.88 dB / **0.5 s → 0.67 dB** / 1.0 s → 1.60 dB / 4.0 s → 0.56 dB。非単調なので
/// 「長ければ良い」ではない)。
pub const PHRASE_PAD_SECS: f64 = 0.5;
/// 同 frame 数 = `round(PHRASE_PAD_SECS * FRAME_RATE)` = 47。
/// **塊 query の端 rest でもある** — 端 rest をこの値にしておくと、先頭フレーズの
/// `[origin - PAD, ..)` と末尾フレーズの `[.., origin + len + PAD)` がちょうど塊の
/// frame 範囲の端に一致し、どのフレーズでも切り出し窓がクランプ不要になる。
pub const PHRASE_PAD_FRAMES: i64 = 47;

/// 合成 1 単位。プロセス内の計算専用 (IPC を渡らない)。
#[derive(Debug, Clone)]
pub struct Phrase {
    /// 解決済みの歌唱 style id (`0` は呼び出し前に `DEFAULT_SINGER_ID` へ潰してある)。
    pub speaker_id: u32,
    /// このフレーズの note (song-absolute beat、start 昇順)。`build_sing_query*` に
    /// そのまま渡せる。
    ///
    /// **不変条件: ここに入る note は必ず query に載る** —
    /// `place_sing_notes(&notes, bpm, _)` が 1 件も落とさない。
    /// 落ちる note を残すと [`crate::voicevox::carry_vowel_after`] が
    /// query に載らない note の母音まで数えてしまい、`emit_sing_query` 側の
    /// 持ち越し母音の連鎖とずれる (= キャッシュキーの楽譜と実際に歌われた歌詞が食い違う)。
    pub notes: Vec<Note>,
    /// `notes` と同じ index の安定 note_id (`plugin_metadata::sing_note_id`)。
    pub note_ids: Vec<u32>,
    /// このフレーズに note を持つ clip の id (昇順・重複なし)。進捗表示のクリップ帰属。
    pub clip_ids: Vec<u32>,
    /// フレーズ先頭で有効な持ち越し母音 (長音符「ー」の解決)。
    pub carry_in: Option<char>,
    /// フレーズ先頭 note の `start_beat` (song-absolute)。
    pub start_beat: f64,
    /// フレーズ末尾 note の終端 beat (song-absolute)。
    pub end_beat: f64,
}

/// クエリ 1 単位。`phrases[range]` を 1 本の query にまとめる。
/// プロセス内の計算専用 (IPC を渡らない)。
#[derive(Debug, Clone)]
pub struct Chunk {
    pub speaker_id: u32,
    /// [`split_into_phrases`] の戻り値に対する index 範囲 (連続、同一 speaker)。
    pub phrases: Range<usize>,
    /// 塊先頭の持ち越し母音 (= `phrases.start` のフレーズの `carry_in`)。
    pub carry_in: Option<char>,
}

/// 塊 1 個ぶんの `/sing_frame_audio_query` body と、各フレーズが塊 frame 空間で
/// 占める範囲。プロセス内の計算専用 (IPC を渡らない)。
#[derive(Debug, Clone)]
pub struct ChunkQuery {
    /// `POST /sing_frame_audio_query` に渡す JSON body。
    pub json: String,
    /// `phrases` と同じ index。`[origin, origin + len)` = そのフレーズの
    /// **先頭 note の開始 frame 〜 末尾 note の終端 frame** (塊 query の絶対 frame)。
    /// 切り出し窓は `[origin - PHRASE_PAD_FRAMES, origin + len + PHRASE_PAD_FRAMES)`。
    pub phrase_windows: Vec<Range<i64>>,
    /// 塊 query の総 frame 数 (= `phrase_windows` 末尾 + [`PHRASE_PAD_FRAMES`])。
    /// engine 応答の frame 数と一致するはず。
    pub total_frames: i64,
}

/// `a_beat` → `b_beat` の秒数。
fn seconds(a_beat: f64, b_beat: f64, bpm: f32) -> f64 {
    (b_beat - a_beat) * 60.0 / f64::from(bpm.max(0.001))
}

/// `entries` をフレーズへ割る。戻り値は **speaker id 昇順 → start_beat 昇順**の
/// 決定論的な順序。
///
/// フレーズは「隙間ゼロで続く note の極大列」。`clip_id` は**切れ目にしない**
/// (クリップは content への窓にすぎず、音楽的な切れ目と一致しない)。
#[must_use]
pub fn split_into_phrases(entries: &[NoteMetadata], bpm: f32) -> Vec<Phrase> {
    // speaker (解決済み) でグルーピング。BTreeMap で speaker id 昇順 = 決定論的順序。
    let mut by_speaker: std::collections::BTreeMap<u32, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        let speaker = if e.speaker_id != 0 {
            e.speaker_id
        } else {
            voicevox::DEFAULT_SINGER_ID
        };
        by_speaker.entry(speaker).or_default().push(i);
    }

    let mut out: Vec<Phrase> = Vec::new();
    for (speaker_id, idxs) in by_speaker {
        let notes: Vec<Note> = idxs
            .iter()
            .map(|&i| {
                let e = &entries[i];
                Note {
                    id: 0,
                    start_beat: e.start_beat,
                    duration_beats: e.duration_beats,
                    pitch: e.pitch,
                    velocity: e.velocity,
                    lyric: (!e.lyric.is_empty()).then(|| e.lyric.clone()),
                    muted: false,
                }
            })
            .collect();

        // 「どの note が載るか」「どこで隙間ゼロか」の SSoT。端 rest は全 placement に
        // 一律で足されるだけなのでフレーズ境界には影響しない
        // (`edge_rest_frames_shifts_every_placement_uniformly` が保証)。既定値を渡して
        // 「分割は口パクと同じ既定 query 空間で見る」を明示する。
        let placements =
            voicevox::place_sing_notes(&notes, bpm, i64::from(voicevox::REST_FRAMES));
        if placements.is_empty() {
            continue;
        }

        // 隙間 (end != 次の start) で切る。1 グループは単一 speaker なので声による
        // 追加の切断は不要 (グルーピングが先に効いている)。
        let mut carry: Option<char> = None;
        let mut seg_start = 0usize;
        for k in 0..placements.len() {
            let is_last = k + 1 == placements.len();
            let breaks =
                is_last || placements[k].end_frame != placements[k + 1].start_frame;
            if !breaks {
                continue;
            }
            let seg = &placements[seg_start..=k];
            let mut ph_notes: Vec<Note> = seg.iter().map(|p| notes[p.index].clone()).collect();
            let mut note_ids: Vec<u32> =
                seg.iter().map(|p| entries[idxs[p.index]].note_id).collect();
            let mut clip_src: Vec<u32> =
                seg.iter().map(|p| entries[idxs[p.index]].clip_id).collect();
            seg_start = k + 1;

            // **フレーズローカル格子の不動点まで絞る。**
            //
            // 上の `placements` は「グループ全体」を基準に丸めているが、実際に query を
            // 組むのは常に**フレーズ先頭 note を基準**にした格子
            // (`build_sing_query_with` も [`build_chunk_query`] も
            // `place_sing_notes(&ph.notes, bpm, _)`)。基準が違うと、ごく短い note が
            // 片方だけで「切り詰めで 1 frame 未満」として落ちることがある。
            //
            // 落ちた note を `Phrase::notes` に残したままにすると、
            // [`crate::voicevox::carry_vowel_after`] が **query に載らない note の母音まで
            // 数えて** carry を進めてしまい、`emit_sing_query` 側の連鎖とずれる
            // = キャッシュキーの楽譜と実際に歌われた歌詞が食い違う。
            // ここで揃えておけば「`Phrase::notes` は必ず query に載る」が不変条件になる。
            //
            // 絞るたびに基準 note が変わり得るので不動点まで回す (各回で必ず 1 件以上
            // 減るので停止する。現実の入力では 1 回目で確定する)。
            loop {
                let local = voicevox::place_sing_notes(&ph_notes, bpm, 0);
                if local.len() == ph_notes.len() {
                    break;
                }
                let keep: Vec<usize> = local.iter().map(|p| p.index).collect();
                ph_notes = keep.iter().map(|&i| ph_notes[i].clone()).collect();
                note_ids = keep.iter().map(|&i| note_ids[i]).collect();
                clip_src = keep.iter().map(|&i| clip_src[i]).collect();
            }
            if ph_notes.is_empty() {
                continue;
            }

            let mut clip_ids = clip_src;
            clip_ids.sort_unstable();
            clip_ids.dedup();
            let start_beat = ph_notes[0].start_beat;
            let last = &ph_notes[ph_notes.len() - 1];
            let end_beat = last.start_beat + last.duration_beats;
            let carry_in = carry;
            carry = voicevox::carry_vowel_after(&ph_notes, carry);
            out.push(Phrase {
                speaker_id,
                notes: ph_notes,
                note_ids,
                clip_ids,
                carry_in,
                start_beat,
                end_beat,
            });
        }
    }
    out
}

/// フレーズ列を塊へまとめる。`chunk_secs` は呼び出し側で
/// `MIN_CHUNK_SECS..=MAX_CHUNK_SECS` にクランプ済みであること。
///
/// **フレーズは絶対に割らない** — `chunk_secs` より長い単一フレーズはそれだけで 1 塊。
/// 切れ目は「窓の中で最も長い休符」に置く (同点なら長い塊を選ぶ)。
#[must_use]
pub fn group_into_chunks(phrases: &[Phrase], bpm: f32, chunk_secs: f32) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let limit = f64::from(chunk_secs);
    let mut g_start = 0usize;
    while g_start < phrases.len() {
        // 同一 speaker の連続列 [g_start, g_end)。
        let speaker_id = phrases[g_start].speaker_id;
        let mut g_end = g_start;
        while g_end < phrases.len() && phrases[g_end].speaker_id == speaker_id {
            g_end += 1;
        }

        let mut s = g_start;
        while s < g_end {
            // (1) `[s, k)` の長さが chunk_secs 以下になる最大の k (最低 s + 1)。
            let mut k_max = s + 1;
            let mut k = s + 2;
            while k <= g_end {
                if seconds(phrases[s].start_beat, phrases[k - 1].end_beat, bpm) <= limit {
                    k_max = k;
                    k += 1;
                } else {
                    break;
                }
            }
            // (2) グループ末尾まで入るならそれで終わり。
            if k_max >= g_end {
                chunks.push(Chunk {
                    speaker_id,
                    phrases: s..g_end,
                    carry_in: phrases[s].carry_in,
                });
                s = g_end;
                continue;
            }
            // (3) 長さが chunk_secs の半分以上になる最小の k (無ければ k_max)。
            let mut k_lo = k_max;
            for k in (s + 1)..=k_max {
                if seconds(phrases[s].start_beat, phrases[k - 1].end_beat, bpm) >= limit * 0.5 {
                    k_lo = k;
                    break;
                }
            }
            // その範囲で「休符が最長」の位置を選ぶ (同点なら大きい k = 長い塊)。
            let mut best_k = k_lo;
            let mut best_rest = f64::NEG_INFINITY;
            for k in k_lo..=k_max {
                let rest = phrases[k].start_beat - phrases[k - 1].end_beat;
                if rest >= best_rest {
                    best_rest = rest;
                    best_k = k;
                }
            }
            chunks.push(Chunk {
                speaker_id,
                phrases: s..best_k,
                carry_in: phrases[s].carry_in,
            });
            s = best_k;
        }
        g_start = g_end;
    }
    chunks
}

/// 塊 query を「**各フレーズの単体 query と 1 frame もずれない格子**」で組む。
///
/// 素朴に `build_sing_query(塊の全 note)` を投げると、丸めの基準 (`base_beat`) が
/// フレーズ単体 query と違うため (1) フレーズ内 2 音目以降が最大 ±1 frame ずれ、
/// (2) 1 frame 長の note が塊側だけで落ちてフレーズが無音になり得る。
/// ここでは **フレーズごとに `place_sing_notes(&ph.notes, bpm, 0)` を掛け**、その
/// 相対格子を平行移動して連結するので、どちらも原理的に起きない。
///
/// `phrases` は同一 speaker の連続列 (= [`group_into_chunks`] が返した
/// [`Chunk::phrases`] のスライス)。`carry_in` は [`Chunk::carry_in`]。
#[must_use]
pub fn build_chunk_query(phrases: &[Phrase], carry_in: Option<char>, bpm: f32) -> ChunkQuery {
    let pad = PHRASE_PAD_FRAMES;
    // 1 拍あたりの frame 数。
    let frames_per_beat = 60.0 / f64::from(bpm.max(0.001)) * voicevox::FRAME_RATE;
    let base = phrases.first().map_or(0.0, |p| p.start_beat);

    let mut chunk_notes: Vec<Note> = Vec::new();
    let mut placements: Vec<NotePlacement> = Vec::new();
    let mut phrase_windows: Vec<Range<i64>> = Vec::with_capacity(phrases.len());
    let mut note_base = 0usize;
    let mut prev_end = pad;

    for (i, ph) in phrases.iter().enumerate() {
        // フレーズローカル配置 (端 rest 0 = 先頭 note が frame 0)。
        let local = voicevox::place_sing_notes(&ph.notes, bpm, 0);
        debug_assert!(
            !local.is_empty(),
            "phrase notes come from place_sing_notes so the first note is never dropped"
        );
        if local.is_empty() {
            // 防御 (到達しない): この塊では歌われない = 空窓。
            phrase_windows.push(prev_end..prev_end);
            chunk_notes.extend(ph.notes.iter().cloned());
            note_base += ph.notes.len();
            continue;
        }
        let len_i = local[local.len() - 1].end_frame;
        let natural = pad + ((ph.start_beat - base) * frames_per_beat).round() as i64;
        // フレーズ間には必ず休符があるが、フレーズローカル配置に組み替えた結果その
        // 隙間が 0 frame に丸まることがある。0 だと `emit_sing_query` が rest を挿入せず
        // **隣接フレーズが engine から見て 1 本に融合**してしまうので、最低 1 frame
        // 空ける。ずれるのは塊内の休符長だけで、各フレーズの音の中身にも、曲上の配置
        // にも影響しない (配置は曲 sample 空間で拍から直接決める)。
        let origin = if i == 0 { pad } else { natural.max(prev_end + 1) };

        for p in &local {
            placements.push(NotePlacement {
                index: note_base + p.index,
                start_frame: origin + p.start_frame,
                end_frame: origin + p.end_frame,
            });
        }
        chunk_notes.extend(ph.notes.iter().cloned());
        note_base += ph.notes.len();
        phrase_windows.push(origin..origin + len_i);
        prev_end = origin + len_i;
    }

    let q = voicevox::emit_sing_query(&chunk_notes, &placements, pad, carry_in);
    ChunkQuery {
        json: q.json,
        phrase_windows,
        total_frames: prev_end + pad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn meta(note_id: u32, clip_id: u32, speaker_id: u32, start: f64, dur: f64, lyric: &str) -> NoteMetadata {
        NoteMetadata {
            note_id,
            start_beat: start,
            duration_beats: dur,
            pitch: 60,
            velocity: 100,
            lyric: lyric.to_string(),
            clip_id,
            speaker_id,
        }
    }

    fn note(start: f64, dur: f64, lyric: &str) -> Note {
        Note {
            id: 0,
            start_beat: start,
            duration_beats: dur,
            pitch: 60,
            velocity: 100,
            lyric: Some(lyric.to_string()),
            muted: false,
        }
    }

    /// 手組みの 1 note フレーズ (build_chunk_query の入力を直接作るとき用)。
    fn phrase_at(start: f64, dur: f64) -> Phrase {
        Phrase {
            speaker_id: 3061,
            notes: vec![note(start, dur, "ら")],
            note_ids: vec![0],
            clip_ids: vec![0],
            carry_in: None,
            start_beat: start,
            end_beat: start + dur,
        }
    }

    /// 塊 query JSON を `(key, frame_length)` 列へ。
    fn parse_entries(json: &str) -> Vec<(Option<i64>, i64)> {
        let v: Value = serde_json::from_str(json).expect("chunk query is not valid JSON");
        v["notes"]
            .as_array()
            .expect("notes array")
            .iter()
            .map(|n| (n["key"].as_i64(), n["frame_length"].as_i64().unwrap_or(0)))
            .collect()
    }

    /// 塊 query JSON を独立に走査して、note エントリの **絶対開始 frame** 列を得る
    /// (= 本番の placements とは別経路)。
    fn note_starts_from_json(json: &str) -> Vec<i64> {
        let mut cursor = 0i64;
        let mut out = Vec::new();
        for (key, len) in parse_entries(json) {
            if key.is_some() {
                out.push(cursor);
            }
            cursor += len;
        }
        out
    }

    #[test]
    fn phrases_break_only_on_gaps() {
        // 隙間ゼロで続く 4 note は 1 フレーズ。
        let entries: Vec<NoteMetadata> = (0..4)
            .map(|i| meta(i, 1, 3061, f64::from(i), 1.0, "ら"))
            .collect();
        let ph = split_into_phrases(&entries, 120.0);
        assert_eq!(ph.len(), 1);
        assert_eq!(ph[0].notes.len(), 4);

        // 間に休符を挟むと 2 つ。
        let entries = vec![
            meta(0, 1, 3061, 0.0, 1.0, "ら"),
            meta(1, 1, 3061, 1.0, 1.0, "ら"),
            meta(2, 1, 3061, 4.0, 1.0, "ら"),
            meta(3, 1, 3061, 5.0, 1.0, "ら"),
        ];
        let ph = split_into_phrases(&entries, 120.0);
        assert_eq!(ph.len(), 2);
        assert_eq!(ph[0].notes.len(), 2);
        assert_eq!(ph[1].notes.len(), 2);
        assert_eq!(ph[0].start_beat, 0.0);
        assert_eq!(ph[0].end_beat, 2.0);
        assert_eq!(ph[1].start_beat, 4.0);
        assert_eq!(ph[1].end_beat, 6.0);
        assert_eq!(ph[0].note_ids, vec![0, 1]);
        assert_eq!(ph[1].note_ids, vec![2, 3]);
    }

    #[test]
    fn phrases_never_cross_speakers() {
        // 同じ時刻に別 speaker の note があっても混ざらない (声ごとに独立の列)。
        let entries = vec![
            meta(0, 1, 3061, 0.0, 1.0, "ら"),
            meta(1, 2, 6000, 1.0, 1.0, "ら"),
            meta(2, 1, 3061, 1.0, 1.0, "ら"),
        ];
        let ph = split_into_phrases(&entries, 120.0);
        assert_eq!(ph.len(), 2, "speaker ごとに 1 フレーズ");
        // speaker id 昇順 (決定論的)。
        assert_eq!(ph[0].speaker_id, 3061);
        assert_eq!(ph[1].speaker_id, 6000);
        assert_eq!(ph[0].notes.len(), 2);
        assert_eq!(ph[1].notes.len(), 1);
    }

    #[test]
    fn phrases_ignore_clip_boundaries() {
        // clip_id が途中で変わっても、隙間ゼロなら 1 フレーズ。両方の clip が
        // `clip_ids` に入る (= クリップ上スピナーが両方点く根拠)。
        let entries = vec![
            meta(0, 7, 3061, 0.0, 1.0, "ら"),
            meta(1, 9, 3061, 1.0, 1.0, "ら"),
        ];
        let ph = split_into_phrases(&entries, 120.0);
        assert_eq!(ph.len(), 1);
        assert_eq!(ph[0].clip_ids, vec![7, 9]);
    }

    /// `Phrase::notes` は **必ず query に載る集合**であること。
    ///
    /// フレーズ分割はグループ全体を基準にした格子で行うが、query を組むのは常に
    /// フレーズ先頭 note を基準にした格子なので、ごく短い note が片方だけで
    /// 「切り詰めで 1 frame 未満」として落ちることがある。落ちた note を残すと
    /// `carry_vowel_after` が query に載らない母音まで数え、`emit_sing_query` 側の
    /// 連鎖とずれる = **キャッシュキーの楽譜と実際に歌われた歌詞が食い違う**。
    #[test]
    fn phrase_notes_all_survive_the_phrase_local_grid() {
        let bpm = 93.7f32;
        // グループ格子では 4 件残るが、フレーズローカル格子では 1 件落ちる並び。
        let raw = [
            (0.0, 0.5),
            (1.622_921_656_887_116_5, 0.010_992_623_340_540_112),
            (1.633_914_280_227_656_7, 0.007_942_691_540_579_139),
            (1.641_856_971_768_235_9, 0.5),
        ];
        let entries: Vec<NoteMetadata> = raw
            .iter()
            .enumerate()
            .map(|(i, &(s, d))| meta(i as u32, 1, 3061, s, d, "ら"))
            .collect();
        let ph = split_into_phrases(&entries, bpm);
        assert!(!ph.is_empty());
        let kept: usize = ph.iter().map(|p| p.notes.len()).sum();
        assert!(kept < raw.len(), "この入力は実際に 1 件落ちる (test の前提): {kept}");
        for p in &ph {
            let local = voicevox::place_sing_notes(&p.notes, bpm, 0);
            assert_eq!(local.len(), p.notes.len(), "notes は全部 query に載る");
            assert_eq!(p.note_ids.len(), p.notes.len(), "平行配列が揃っている");
            // 単体 query も同じ集合を残す = carry の連鎖が split 側と一致する。
            let solo = voicevox::build_sing_query_with(&p.notes, bpm, p.carry_in);
            assert_eq!(solo.notes.len(), p.notes.len());
            assert_eq!(
                solo.carry_out,
                voicevox::carry_vowel_after(&p.notes, p.carry_in),
                "carry の SSoT が割れていない"
            );
        }
    }

    #[test]
    fn carry_in_flows_between_phrases() {
        // 「ら」で終わるフレーズの次のフレーズの carry_in が Some('あ')。
        let entries = vec![
            meta(0, 1, 3061, 0.0, 1.0, "ら"),
            meta(1, 1, 3061, 4.0, 1.0, "ー"),
        ];
        let ph = split_into_phrases(&entries, 120.0);
        assert_eq!(ph.len(), 2);
        assert_eq!(ph[0].carry_in, None);
        assert_eq!(ph[1].carry_in, Some('あ'));
    }

    #[test]
    fn chunks_cut_at_the_longest_rest() {
        // bpm 120 → 1 拍 0.5 秒。1 拍の note を 0/2/4/…/38 拍に置く (= 20 フレーズ)。
        // うち 1 か所だけ休符を長くしておくと、その位置で切れる。
        let mut entries: Vec<NoteMetadata> = Vec::new();
        let mut beat = 0.0;
        for i in 0..20u32 {
            entries.push(meta(i, 1, 3061, beat, 1.0, "ら"));
            // 12 番目の後だけ長い休符 (それ以外は 1 拍の休符)。
            beat += if i == 11 { 9.0 } else { 2.0 };
        }
        let ph = split_into_phrases(&entries, 120.0);
        assert_eq!(ph.len(), 20);
        // 全長は 20 フレーズ ≈ 24 秒。chunk_secs 15 秒 → 半分 (7.5 秒) 以上で
        // 最長休符 = phrase 12 の手前。
        let chunks = group_into_chunks(&ph, 120.0, 15.0);
        assert!(chunks.len() >= 2, "分割される: {chunks:?}");
        assert_eq!(chunks[0].phrases, 0..12, "最長休符の位置で切れる");
        assert_eq!(chunks[1].phrases.start, 12);
    }

    #[test]
    fn chunk_never_splits_a_phrase() {
        // chunk_secs より長い単一フレーズが 1 塊になる (フレーズは絶対に割らない)。
        let entries: Vec<NoteMetadata> = (0..80)
            .map(|i| meta(i, 1, 3061, f64::from(i), 1.0, "ら"))
            .collect();
        let ph = split_into_phrases(&entries, 120.0);
        assert_eq!(ph.len(), 1, "隙間ゼロなので 1 フレーズ (40 秒)");
        let chunks = group_into_chunks(&ph, 120.0, 15.0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].phrases, 0..1);
    }

    #[test]
    fn chunks_are_deterministic() {
        // 同じ入力で 2 回呼んで完全一致 (キャッシュキーの前提)。
        let mut entries: Vec<NoteMetadata> = Vec::new();
        let mut beat = 0.0;
        for i in 0..30u32 {
            entries.push(meta(i, 1, if i % 3 == 0 { 6000 } else { 3061 }, beat, 1.0, "ら"));
            beat += 2.0;
        }
        let a = split_into_phrases(&entries, 120.0);
        let b = split_into_phrases(&entries, 120.0);
        let key = |v: &[Phrase]| -> Vec<(u32, f64, f64, Vec<u32>)> {
            v.iter()
                .map(|p| (p.speaker_id, p.start_beat, p.end_beat, p.note_ids.clone()))
                .collect()
        };
        assert_eq!(key(&a), key(&b));
        let ca = group_into_chunks(&a, 120.0, 15.0);
        let cb = group_into_chunks(&b, 120.0, 15.0);
        let ckey = |v: &[Chunk]| -> Vec<(u32, Range<usize>)> {
            v.iter().map(|c| (c.speaker_id, c.phrases.clone())).collect()
        };
        assert_eq!(ckey(&ca), ckey(&cb));
        // 塊は phrases を過不足なく覆う。
        let covered: usize = ca.iter().map(|c| c.phrases.len()).sum();
        assert_eq!(covered, a.len());
    }

    #[test]
    fn chunk_query_grid_matches_each_phrase_solo_query() {
        // **この計画の核**: 塊 query の frame 格子が、各フレーズの単体 query と
        // 1 frame もずれないこと。突き合わせは 2 つの独立な経路 (塊 JSON を走査した
        // 絶対 frame 列 と `build_sing_query_with` の placement) で行う。
        let mut entries: Vec<NoteMetadata> = Vec::new();
        let mut id = 0u32;
        for (start, n) in [(0.0, 3usize), (5.0, 4), (11.5, 2)] {
            for k in 0..n {
                entries.push(meta(id, 1, 3061, start + k as f64 * 0.75, 0.75, "ら"));
                id += 1;
            }
        }
        let ph = split_into_phrases(&entries, 132.0);
        assert_eq!(ph.len(), 3, "3 フレーズ");
        let cq = build_chunk_query(&ph, None, 132.0);
        let chunk_starts = note_starts_from_json(&cq.json);

        let mut cursor = 0usize;
        for (i, p) in ph.iter().enumerate() {
            let solo = voicevox::build_sing_query_with(&p.notes, 132.0, p.carry_in);
            let n = solo.notes.len();
            let chunk_rel: Vec<i64> = chunk_starts[cursor..cursor + n]
                .iter()
                .map(|f| f - cq.phrase_windows[i].start)
                .collect();
            let solo_rel: Vec<i64> = solo
                .notes
                .iter()
                .map(|p| p.start_frame - i64::from(voicevox::REST_FRAMES))
                .collect();
            assert_eq!(chunk_rel, solo_rel, "phrase {i} の相対 frame 列");
            cursor += n;
        }
        assert_eq!(cursor, chunk_starts.len(), "塊 note 数 = 各フレーズの合計");
    }

    #[test]
    fn chunk_query_keeps_every_note_the_solo_query_keeps() {
        // 1 frame 長の note を含むフレーズでも、単体 query に載った note が塊 query から
        // 落ちないこと (= 「フレーズが無音になる」の回帰)。塊が自分の base_beat で
        // 丸め直すと、この 1 frame の note が「切り詰めで 1 frame 未満」として落ちる側に
        // 転び得る — `build_chunk_query` はフレーズローカル配置を平行移動するので起きない。
        //
        // 1 frame 未満の note の直後 (= ちょうど 1 frame 後) に次の note を置くと、
        // 隙間ゼロ = 同一フレーズになる。
        let f = voicevox::frames_to_beats(1.0, 120.0);
        let entries = vec![
            meta(0, 1, 3061, 0.0, 0.004, "ら"), // 1 frame 未満 → 1 frame に丸められる
            meta(1, 1, 3061, f, 1.0, "ら"),
            meta(2, 1, 3061, 6.0, 0.004, "ら"),
            meta(3, 1, 3061, 6.0 + f, 1.0, "ら"),
        ];
        let ph = split_into_phrases(&entries, 120.0);
        assert_eq!(ph.len(), 2, "1 frame の note とその直後の note は同一フレーズ");
        assert_eq!(ph[0].notes.len(), 2);
        assert_eq!(ph[1].notes.len(), 2);
        let cq = build_chunk_query(&ph, None, 120.0);
        let chunk_note_count = parse_entries(&cq.json)
            .iter()
            .filter(|(key, _)| key.is_some())
            .count();
        let solo_total: usize = ph
            .iter()
            .map(|p| voicevox::build_sing_query_with(&p.notes, 120.0, p.carry_in).notes.len())
            .sum();
        assert_eq!(chunk_note_count, solo_total);
        assert!(solo_total >= 4, "1 frame の note も落ちない: {solo_total}");
    }

    #[test]
    fn chunk_query_windows_need_no_clamping() {
        // 先頭 / 末尾を含む全フレーズで `0 <= origin - PAD` かつ
        // `origin + len + PAD <= total_frames` (= 切り出し窓のクランプが不要)。
        let entries = vec![
            meta(0, 1, 3061, 0.0, 1.0, "ら"),
            meta(1, 1, 3061, 1.0, 1.0, "ら"),
            meta(2, 1, 3061, 6.0, 1.0, "ら"),
            meta(3, 1, 3061, 12.0, 1.0, "ら"),
        ];
        let ph = split_into_phrases(&entries, 120.0);
        assert_eq!(ph.len(), 3);
        let cq = build_chunk_query(&ph, None, 120.0);
        for (i, w) in cq.phrase_windows.iter().enumerate() {
            assert!(w.start - PHRASE_PAD_FRAMES >= 0, "phrase {i} の下端");
            assert!(
                w.end + PHRASE_PAD_FRAMES <= cq.total_frames,
                "phrase {i} の上端: {} + {PHRASE_PAD_FRAMES} > {}",
                w.end,
                cq.total_frames
            );
        }
        // total_frames は JSON の frame_length 総和と一致する (= engine の frame 数)。
        let total: i64 = parse_entries(&cq.json).iter().map(|(_, len)| len).sum();
        assert_eq!(total, cq.total_frames);
    }

    #[test]
    fn chunk_query_separates_phrases_by_at_least_one_frame() {
        // フレーズローカル配置で隙間が 0 frame に丸まる入力でも、必ず 1 frame 空ける
        // (0 だと engine から見て隣接フレーズが 1 本に融合する)。
        // 入力は合成的 — 拍上の休符が 1 frame 未満に丸まる配置を直接組む。
        let phrases = vec![phrase_at(0.0, 1.0), phrase_at(1.005, 1.0)];
        let cq = build_chunk_query(&phrases, None, 120.0);
        assert_eq!(cq.phrase_windows.len(), 2);
        assert!(
            cq.phrase_windows[1].start > cq.phrase_windows[0].end,
            "最低 1 frame 空ける: windows={:?}",
            cq.phrase_windows
        );
        // 融合していないこと = JSON に rest エントリが 3 つ (start / 間 / end) ある。
        let rests = parse_entries(&cq.json)
            .iter()
            .filter(|(key, _)| key.is_none())
            .count();
        assert_eq!(rests, 3, "フレーズ間に rest が入る");
    }
}
