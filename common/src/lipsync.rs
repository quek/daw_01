//! 口パク (lip-sync) 生成ロジック。docs/plan_pakupaku.md §6。
//!
//! VOICEVOX の phoneme 列 (各 `frame_length` 付き) を、口形状画像の `ImageEvent`
//! 列へ変換する純粋関数。REAPER `pakupaku.lua` の挙動を移植する:
//!   - 子音は次の母音の口形状を借用 (pau / cl で打ち切り、無ければ閉口)
//!   - cl (促音) / pau (ポーズ) は閉口
//!   - 連続同形はマージ (1 つの長い `ImageEvent` にまとめる)
//!   - 先頭 pau の `REST_FRAMES` 分手前から配置し、実 phoneme を最初の note 位置へ
//!   - clip 範囲外はクランプ、前後の隙間は閉口で埋める
//!
//! 副作用なし・純粋関数なので unit test しやすい。実際の HTTP 取得は
//! `crate::voicevox::query_phonemes`、生成先 clip への適用は daw_gui 側。

use crate::model::{ImageEvent, MouthMap, MouthShape};
use crate::voicevox::{FRAME_RATE, Phoneme, REST_FRAMES};

/// VOICEVOX frame 数 → clip-local beats。
/// `beats = frames / FRAME_RATE / (60 / bpm)`。
fn frames_to_beats(frames: f64, bpm: f32) -> f64 {
    frames / FRAME_RATE * (bpm as f64) / 60.0
}

/// phoneme 文字列が母音 (a/i/u/e/o/N) なら対応する口形状。子音 / cl / pau は
/// `None`。REAPER の VOWELS セット (撥音 N を含む) と同じ判定。
fn vowel_shape(phoneme: &str) -> Option<MouthShape> {
    match phoneme {
        "a" => Some(MouthShape::A),
        "i" => Some(MouthShape::I),
        "u" => Some(MouthShape::U),
        "e" => Some(MouthShape::E),
        "o" => Some(MouthShape::O),
        "N" => Some(MouthShape::N),
        _ => None,
    }
}

fn is_pause(phoneme: &str) -> bool {
    phoneme == "pau" || phoneme == "cl"
}

/// `phonemes[i]` が実際に表示すべき口形状。母音/撥音はそのまま、cl/pau は閉口、
/// 子音は次の母音を先読みして借用する (pau/cl に当たったら打ち切り = 閉口、
/// 末尾まで母音が無ければ閉口)。REAPER `pakupaku.lua` の effective_phoneme 同等。
fn effective_shape(phonemes: &[Phoneme], i: usize) -> MouthShape {
    let p = phonemes[i].phoneme.as_str();
    if let Some(s) = vowel_shape(p) {
        return s;
    }
    if is_pause(p) {
        return MouthShape::Closed;
    }
    // 子音: 次の母音の口形状を借用。
    for next in &phonemes[i + 1..] {
        if let Some(s) = vowel_shape(&next.phoneme) {
            return s;
        }
        if is_pause(&next.phoneme) {
            break;
        }
    }
    MouthShape::Closed
}

/// phoneme 列 → 口画像 `ImageEvent` 列 (clip-local beats)。
///
/// 引数:
/// - `phonemes`: `query_phonemes` の生出力 (先頭/末尾の pau 込み、frame 0 起点)。
/// - `mouth_map`: 口形状 → `ImageSourceId`。未割当 slot は閉口へ fallback。
/// - `bpm`: 曲の BPM (frame → beat 変換に使用)。
/// - `first_note_local_beat`: 生成先 clip 内で earliest note が始まる clip-local
///   beat。VOICEVOX frame `REST_FRAMES` (= 先頭 pau の直後) がこの位置に対応する。
/// - `clip_len_beats`: 生成先 clip の長さ。範囲外の event はクランプ/破棄する。
///
/// 戻り値の `ImageEvent` は `source_id` / `event_start_in_clip_beats` /
/// `event_length_beats` のみ設定し、rect は全画面・opacity 1・fade 0
/// (`ImageEvent::default()`)。
pub fn build_mouth_events(
    phonemes: &[Phoneme],
    mouth_map: &MouthMap,
    bpm: f32,
    first_note_local_beat: f64,
    clip_len_beats: f64,
) -> Vec<ImageEvent> {
    if phonemes.is_empty() || clip_len_beats <= 0.0 || bpm <= 0.0 {
        return Vec::new();
    }

    let rest_beats = frames_to_beats(REST_FRAMES as f64, bpm);
    // 先頭 pau の手前から配置 → 実 phoneme が first_note 位置へ来る (音声と同期)。
    let mut cursor = first_note_local_beat - rest_beats;

    // 1) phoneme ごとの raw 区間 (source_id 付き、まだクランプ/マージ前)。
    struct Raw {
        start: f64,
        end: f64,
        source_id: u32,
    }
    let mut raw: Vec<Raw> = Vec::with_capacity(phonemes.len() + 2);
    for (i, p) in phonemes.iter().enumerate() {
        let dur = frames_to_beats(p.frame_length as f64, bpm);
        let source_id = mouth_map.resolve(effective_shape(phonemes, i));
        raw.push(Raw {
            start: cursor,
            end: cursor + dur,
            source_id,
        });
        cursor += dur;
    }

    // 2) phoneme 列が覆わない前後の隙間を閉口で埋める。
    let closed_id = mouth_map.resolve(MouthShape::Closed);
    let span_start = raw.first().map(|r| r.start).unwrap_or(0.0);
    let span_end = raw.last().map(|r| r.end).unwrap_or(0.0);
    if span_start > 0.0 {
        raw.insert(
            0,
            Raw {
                start: 0.0,
                end: span_start,
                source_id: closed_id,
            },
        );
    }
    if span_end < clip_len_beats {
        raw.push(Raw {
            start: span_end,
            end: clip_len_beats,
            source_id: closed_id,
        });
    }

    // 3) [0, clip_len] にクランプ → 連続同形マージ → ImageEvent 化。
    let mut events: Vec<ImageEvent> = Vec::new();
    for r in raw {
        let start = r.start.max(0.0);
        let end = r.end.min(clip_len_beats);
        if end - start <= 1e-9 {
            continue;
        }
        if let Some(last) = events.last_mut() {
            let last_end = last.event_start_in_clip_beats + last.event_length_beats;
            if last.source_id == r.source_id && (start - last_end).abs() <= 1e-6 {
                // 連続する同形 → 直前 event を延長してマージ。
                last.event_length_beats = end - last.event_start_in_clip_beats;
                continue;
            }
        }
        events.push(ImageEvent {
            source_id: r.source_id,
            event_start_in_clip_beats: start,
            event_length_beats: end - start,
            ..ImageEvent::default()
        });
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ph(p: &str, f: u32) -> Phoneme {
        Phoneme {
            phoneme: p.into(),
            frame_length: f,
        }
    }

    fn full_map() -> MouthMap {
        MouthMap {
            a: 1,
            i: 2,
            u: 3,
            e: 4,
            o: 5,
            n: 6,
            closed: 7,
        }
    }

    /// frame → beat (テスト用、本体と同式)。
    fn b(frames: f64, bpm: f32) -> f64 {
        frames / FRAME_RATE * (bpm as f64) / 60.0
    }

    fn ids(events: &[ImageEvent]) -> Vec<u32> {
        events.iter().map(|e| e.source_id).collect()
    }

    /// event 列が [0, clip_len] を隙間なく連続で覆うこと。
    fn assert_contiguous(events: &[ImageEvent], clip_len: f64) {
        assert!(!events.is_empty());
        assert!((events[0].event_start_in_clip_beats).abs() < 1e-6, "starts at 0");
        let mut cur = 0.0;
        for e in events {
            assert!(
                (e.event_start_in_clip_beats - cur).abs() < 1e-6,
                "event starts where previous ended"
            );
            cur = e.event_start_in_clip_beats + e.event_length_beats;
        }
        assert!((cur - clip_len).abs() < 1e-6, "covers up to clip_len");
    }

    #[test]
    fn consonant_borrows_next_vowel_and_merges() {
        let bpm = 120.0;
        // 先頭 pau(10) → k(5) → a(20) → 末尾 pau(10)。
        let phs = vec![ph("pau", 10), ph("k", 5), ph("a", 20), ph("pau", 10)];
        // first_note を rest_beats に置く → cursor 開始 = 0。
        let first_note = b(REST_FRAMES as f64, bpm);
        let clip_len = b(45.0, bpm); // 10+5+20+10
        let events = build_mouth_events(&phs, &full_map(), bpm, first_note, clip_len);
        // k(子音)→a を借用し id=1、a も id=1 → マージ。前後 pau は閉口 id=7。
        assert_eq!(ids(&events), vec![7, 1, 7]);
        assert_contiguous(&events, clip_len);
        // 借用+マージ区間は frame [10,35] = b(10)..b(35)。
        assert!((events[1].event_start_in_clip_beats - b(10.0, bpm)).abs() < 1e-6);
        assert!((events[1].event_length_beats - b(25.0, bpm)).abs() < 1e-6);
    }

    #[test]
    fn distinct_vowels_are_separate_events() {
        let bpm = 100.0;
        let phs = vec![ph("pau", 10), ph("a", 10), ph("i", 10), ph("o", 10), ph("pau", 10)];
        let first_note = b(REST_FRAMES as f64, bpm);
        let clip_len = b(50.0, bpm);
        let events = build_mouth_events(&phs, &full_map(), bpm, first_note, clip_len);
        assert_eq!(ids(&events), vec![7, 1, 2, 5, 7]);
        assert_contiguous(&events, clip_len);
    }

    #[test]
    fn rest_offset_places_first_vowel_at_note() {
        let bpm = 120.0;
        let phs = vec![ph("pau", 10), ph("a", 20), ph("pau", 10)];
        // first_note を clip 内 beat 2.0 に。
        let first_note = 2.0;
        let clip_len = 8.0;
        let events = build_mouth_events(&phs, &full_map(), bpm, first_note, clip_len);
        // 最初の母音 'a' (id=1) は first_note=2.0 に始まる。
        let a_event = events.iter().find(|e| e.source_id == 1).expect("has 'a' event");
        assert!(
            (a_event.event_start_in_clip_beats - 2.0).abs() < 1e-6,
            "first vowel starts at first_note_local_beat, got {}",
            a_event.event_start_in_clip_beats
        );
        // 先頭は [0, 2.0] の閉口 fill。
        assert_eq!(events[0].source_id, 7);
        assert!((events[0].event_start_in_clip_beats).abs() < 1e-6);
        assert!((events[0].event_length_beats - 2.0).abs() < 1e-6);
        assert_contiguous(&events, clip_len);
    }

    #[test]
    fn trailing_gap_filled_with_closed() {
        let bpm = 120.0;
        let phs = vec![ph("pau", 10), ph("a", 20), ph("pau", 10)];
        let first_note = b(REST_FRAMES as f64, bpm);
        // clip_len を phoneme 終端 (b(40)) より長く。
        let clip_len = b(40.0, bpm) + 1.0;
        let events = build_mouth_events(&phs, &full_map(), bpm, first_note, clip_len);
        assert_contiguous(&events, clip_len);
        // 末尾は閉口。
        assert_eq!(events.last().unwrap().source_id, 7);
    }

    #[test]
    fn unmapped_shape_falls_back_to_closed() {
        let bpm = 120.0;
        // closed のみ割当、母音は未割当 (0)。
        let map = MouthMap {
            a: 0,
            i: 0,
            u: 0,
            e: 0,
            o: 0,
            n: 0,
            closed: 9,
        };
        let phs = vec![ph("pau", 10), ph("a", 20), ph("pau", 10)];
        let first_note = b(REST_FRAMES as f64, bpm);
        let clip_len = b(40.0, bpm);
        let events = build_mouth_events(&phs, &map, bpm, first_note, clip_len);
        // 全部 closed(9) に解決 → 1 つにマージ。
        assert_eq!(ids(&events), vec![9]);
    }

    #[test]
    fn empty_or_invalid_inputs_yield_nothing() {
        assert!(build_mouth_events(&[], &full_map(), 120.0, 0.0, 4.0).is_empty());
        assert!(build_mouth_events(&[ph("a", 10)], &full_map(), 120.0, 0.0, 0.0).is_empty());
        assert!(build_mouth_events(&[ph("a", 10)], &full_map(), 0.0, 0.0, 4.0).is_empty());
    }
}
