//! Sample / beat conversion helpers shared by `daw_audio` (audio thread) and
//! `daw_gui`.

use crate::model::{Clip, LoopRegion, Song};
use crate::tempo_map::TempoMap;

/// Every clip that contributes to the song's playable envelope: finite,
/// positive-length, finite-start clips across all tracks. Single source of
/// truth for the clip filter shared by [`song_bounds_samples`] and
/// [`content_bounds_beats`] — a change to the filtering rule (e.g. excluding
/// muted tracks) lives here, not in several copies. Non-finite (NaN / ±Inf) or
/// non-positive geometry is skipped: it slips past a plain `<= 0.0` guard and
/// would saturate the u64 sample cast. `start + length` is checked as a sum
/// because two finite values can still overflow to infinity.
fn playable_clips(song: &Song) -> impl Iterator<Item = &Clip> {
    song.tracks
        .iter()
        .flat_map(|t| t.clips.iter())
        .filter(|c| c.length_beats > 0.0 && (c.start_beat + c.length_beats).is_finite())
}

/// Returns `(earliest_clip_start_samples, latest_clip_end_samples)` across
/// every clip on every track in `song`, or `None` if there is nothing
/// playable: no song, no non-empty clip, `bpm <= 0`, or all clips have zero
/// length.
///
/// The `None` case means "do not loop, do not play": downstream code should
/// treat it as end-of-song. The bounds are the outer envelope of all
/// material; playhead iterates over this span once per loop cycle.
///
/// `tempo` は `song` から作った表。テンポカーブのある曲は、engine が sample を拍へ戻すときと同じ表で換算する。
pub fn song_bounds_samples(song: Option<&Song>, sample_rate: u32, tempo: &TempoMap) -> Option<(u64, u64)> {
    let song = song?;
    if song.bpm <= 0.0 {
        return None;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let mut min_start: Option<u64> = None;
    let mut max_end: u64 = 0;
    for clip in playable_clips(song) {
        let (start, length) = if tempo.is_automated() {
            let start = tempo.beat_to_samples(clip.start_beat, sample_rate);
            let end = tempo.beat_to_samples(clip.start_beat + clip.length_beats, sample_rate);
            (start, end.saturating_sub(start))
        } else {
            ((clip.start_beat * samples_per_beat).max(0.0) as u64, (clip.length_beats * samples_per_beat) as u64)
        };
        // A clip shorter than one sample at this rate contributes nothing.
        if length == 0 {
            continue;
        }
        min_start = Some(min_start.map_or(start, |m| m.min(start)));
        max_end = max_end.max(start.saturating_add(length));
    }
    let start = min_start?;
    if max_end <= start {
        return None;
    }
    Some((start, max_end))
}

/// The beat-domain content envelope `(earliest_clip_start, content_end)` =
/// `(min start_beat, max start_beat + length_beats)` over every playable clip
/// on every track, or `None` when the song has no playable clip (same
/// filtering as [`song_bounds_samples`]: finite, positive-length clips only).
/// The beat-domain sibling of [`song_bounds_samples`]'s `(min_start,
/// max_end)`. r.md #10: the Home key seeks to `.0` (the head of the first /
/// earliest clip; a second Home press then returns to bar 1) and the End key
/// seeks to `.1` (just after the last clip).
pub fn content_bounds_beats(song: &Song) -> Option<(f64, f64)> {
    let mut bounds: Option<(f64, f64)> = None;
    for clip in playable_clips(song) {
        let start = clip.start_beat;
        let end = clip.start_beat + clip.length_beats;
        bounds = Some(match bounds {
            Some((lo, hi)) => (lo.min(start), hi.max(end)),
            None => (start, end),
        });
    }
    bounds
}

/// 曲の長さの上限 [拍]。壊れた / 細工された project の巨大な値で、曲末まで回るループや表の確保が
/// 終わらなくなるのを防ぐ (1M 拍 ≒ 200 BPM で約 83 時間で、現実の曲の遥か上)。
pub const MAX_SONG_BEATS: f64 = 1_000_000.0;

/// 曲末 [拍]。曲全体を指す処理 (書き出しの全曲 / テンポ表 / 変調表 …) はこれを読む。
///
/// `Song::length_beats` はクリップを置いても伸びない (MIDI の取り込み / サンプラー / 時間の挿入・削除だけが
/// 更新する) ので、単独で曲末として読むと、それより後ろのクリップが欠ける。クリップ終端と大きい方を取る。
/// 値は有限で `0.0..=MAX_SONG_BEATS` に収まる。全クリップを舐めるので RT からは呼ばない。
#[must_use]
pub fn song_end_beat(song: &Song) -> f64 {
    let content_end = content_bounds_beats(song).map_or(0.0, |(_, end)| end);
    let length = if song.length_beats.is_finite() { song.length_beats } else { 0.0 };
    length.max(content_end).clamp(0.0, MAX_SONG_BEATS)
}

/// `playhead` が最後のクリップの終端に達したか。曲の範囲 ([`song_bounds_samples`]) は求め済みのものを渡す
/// (RT は song と同じ便で届いた値を使い、buffer ごとに全 clip を舐めない)。
#[must_use]
pub fn ended_at(bounds: Option<(u64, u64)>, playhead: u64) -> bool {
    bounds.is_some_and(|(_, end)| playhead >= end)
}

/// 曲の範囲 ([`song_bounds_samples`]) を求めたときの sample rate とその値 (song と同じ便で RT へ届ける)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SongBounds {
    pub sample_rate: u32,
    pub samples: Option<(u64, u64)>,
    /// 最後のクリップの終端 [拍] (`samples` が `None` なら `None`)。
    pub end_beat: Option<f64>,
}

impl SongBounds {
    #[must_use]
    pub fn of(song: Option<&Song>, sample_rate: u32, tempo: &TempoMap) -> Self {
        let samples = song_bounds_samples(song, sample_rate, tempo);
        let end_beat = samples.and(song).and_then(content_bounds_beats).map(|(_, end)| end);
        Self { sample_rate, samples, end_beat }
    }

    /// 鳴っている拍 `beat` で見た `(ループの端に達したか, 曲末に達したか)`。テンポが動く曲の判定に使う。
    /// 前者は `loop_active` (ループが効いている) のときだけ `Some`。`loop_region` に範囲が無ければ曲全体がループ区間。
    #[must_use]
    pub fn reached_at_beat(&self, beat: f64, loop_region: LoopRegion, loop_active: bool) -> (Option<bool>, bool) {
        let song_ended = self.end_beat.is_some_and(|end| beat >= end);
        let loop_end_reached = loop_active.then(|| match loop_region.range() {
            Some((_, end)) => beat >= end,
            None => song_ended,
        });
        (loop_end_reached, song_ended)
    }

    /// `sample_rate` での曲の範囲。求めたときと rate が違えば (device の rate が session と食い違う間) その rate で
    /// 求め直して持ち替える — 次の buffer からは求め直さない。`song` と `tempo` は求めたときと同じ snapshot。
    pub fn at(&mut self, song: Option<&Song>, sample_rate: u32, tempo: &TempoMap) -> Option<(u64, u64)> {
        if self.sample_rate != sample_rate {
            *self = Self::of(song, sample_rate, tempo);
        }
        self.samples
    }
}

/// Converts a beat-domain position to `(bar, beat_in_bar)`, both **1-based**,
/// matching the bar/beat numbering the arrangement / piano-roll ruler uses
/// (gui_01 `TimeMapping::samples_to_bar_beat`). `beats_per_bar` is derived
/// from the time signature as `num * 4 / den` (= 4 for 4/4, 6 for 6/8). The
/// transport readout and the rulers therefore agree on which bar a beat is in.
pub fn beat_to_bar_beat(beat: f64, time_sig: (u8, u8)) -> (u32, f64) {
    let bpb = beats_per_bar(time_sig);
    if bpb <= 0.0 || !beat.is_finite() {
        return (1, 1.0);
    }
    let bar = (beat / bpb).floor().max(0.0) as u32 + 1;
    let beat_in_bar = beat - (f64::from(bar) - 1.0) * bpb + 1.0;
    (bar, beat_in_bar)
}

/// Quarter-note beats per bar for a `(num, den)` time signature: `num * 4 /
/// den` (= 4 for 4/4, 3 for 6/8, 6 for 6/4). Single source of truth for the
/// bar/beat math shared by [`beat_to_bar_beat`], the rulers, and the export
/// range picker's bar.beat field.
pub fn beats_per_bar(time_sig: (u8, u8)) -> f64 {
    f64::from(time_sig.0) * 4.0 / f64::from(time_sig.1.max(1))
}

/// buffer の先頭から `delta_beats` 拍の位置にある境界 (note の発音 / 音の始まりと終わり) を、その位置以降で最初の
/// frame に写す (`ceil`)。浮動小数の誤差で整数のすぐそばに来た値は整数へ吸着する。
///
/// 吸着しないと、buffer の境界ちょうどにある境界が buffer ごとに前後へぶれる: 切り捨てると
/// `(playhead + frames/spb − playhead) * spb = 479.99999999998977` が 479 になり、音の続く buffer の最後の 1 sample が
/// 書かれずに 0 のまま残る (buffer 周期のクリック = ブザー)。同じ規則で両端を写せば、buffer ごとに独立に計算しても
/// 同じ絶対位置は必ず同じ frame に落ち、隣の buffer と重ならず隙間も空かない。
#[must_use]
#[inline]
pub fn boundary_frame(delta_beats: f64, samples_per_beat: f64) -> f64 {
    let x = delta_beats * samples_per_beat;
    let nearest = x.round();
    if (x - nearest).abs() < 1e-6 { nearest } else { x.ceil() }
}

/// Returns the sample range the audio engine should treat as the active
/// playback loop. Prefers the user-defined [`LoopRegion`] range when it is
/// non-empty; otherwise falls back to `song_bounds`, the full song-content
/// envelope from [`song_bounds_samples`] (RT は song と同じ便で届いた値を渡す).
///
/// The loop region is **session state**, not part of `Song` (see
/// [`LoopRegion`]): the engine receives it via `AudioCommand::SetLoop` and
/// passes it in here. テンポカーブのある曲は、engine が sample を拍へ戻すときと同じ `tempo` の表で換算する
/// (巻き戻した先の sample が指定した拍に戻る)。表の引きは alloc なしで RT から呼べる。
#[must_use]
pub fn effective_loop_bounds(
    song: Option<&Song>,
    loop_region: LoopRegion,
    sample_rate: u32,
    tempo: &TempoMap,
    song_bounds: Option<(u64, u64)>,
) -> Option<(u64, u64)> {
    let song_ref = song?;
    if song_ref.bpm > 0.0 && loop_region.has_range() {
        let (start, end) = if tempo.is_automated() {
            (tempo.beat_to_samples(loop_region.start_beat, sample_rate), tempo.beat_to_samples(loop_region.end_beat, sample_rate))
        } else {
            let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song_ref.bpm);
            (
                (loop_region.start_beat * samples_per_beat).max(0.0) as u64,
                (loop_region.end_beat * samples_per_beat).max(0.0) as u64,
            )
        };
        if end > start {
            return Some((start, end));
        }
    }
    song_bounds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Clip, InstrumentSource, Track};

    fn song_with_clip(bpm: f32, start_beat: f64, length_beats: f64) -> Song {
        Song {
            bpm,
            tracks: vec![Track {
                name: "T".into(),
                source: InstrumentSource::BuiltinSynth,
                clips: vec![Clip {
                    id: 1,
                    start_beat,
                    length_beats,
                    content_id: 0,
                    color: None,
                    auto_lipsync: false,
                    ..Default::default()
                }],
                ..Track::default()
            }],
            ..Song::default()
        }
    }

    /// 48 kHz での曲の範囲 (本番と同じく `song` から作った表で求める)。
    fn bounds(song: Option<&Song>) -> Option<(u64, u64)> {
        let default = Song::default();
        song_bounds_samples(song, 48000, &TempoMap::from_song(song.unwrap_or(&default)))
    }

    #[test]
    fn bounds_none_for_no_song() {
        assert_eq!(bounds(None), None);
    }

    #[test]
    fn bounds_none_for_empty_tracks() {
        assert_eq!(bounds(Some(&Song::default())), None);
    }

    #[test]
    fn bounds_none_for_zero_bpm() {
        let song = song_with_clip(0.0, 0.0, 4.0);
        assert_eq!(bounds(Some(&song)), None);
    }

    #[test]
    fn bounds_none_for_zero_length_clip() {
        let song = song_with_clip(120.0, 0.0, 0.0);
        assert_eq!(bounds(Some(&song)), None);
    }

    #[test]
    fn bounds_standard_clip() {
        // 120 BPM, 48 kHz: 1 beat = 24000 samples; 4 beats = 96000 samples.
        let song = song_with_clip(120.0, 0.0, 4.0);
        assert_eq!(bounds(Some(&song)), Some((0, 96_000)));
    }

    #[test]
    fn bounds_with_offset() {
        // Start at beat 2 → 48000 samples in; length 4 beats → end at 144000.
        let song = song_with_clip(120.0, 2.0, 4.0);
        assert_eq!(
            bounds(Some(&song)),
            Some((48_000, 144_000))
        );
    }

    #[test]
    fn bounds_spans_multiple_tracks() {
        // Track 0 has a 4-beat clip at 0; track 1 has a 2-beat clip at beat 6.
        // Expected span: earliest start (0) to latest end (beat 8 = 192_000).
        let song = Song {
            bpm: 120.0,
            tracks: vec![
                Track {
                    name: "A".into(),
                    clips: vec![Clip {
                        id: 1,
                        start_beat: 0.0,
                        length_beats: 4.0,
                        content_id: 0,
                        color: None,
                        auto_lipsync: false,
                        ..Default::default()
                    }],
                    ..Track::default()
                },
                Track {
                    name: "B".into(),
                    clips: vec![Clip {
                        id: 1,
                        start_beat: 6.0,
                        length_beats: 2.0,
                        content_id: 0,
                        color: None,
                        auto_lipsync: false,
                        ..Default::default()
                    }],
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        assert_eq!(bounds(Some(&song)), Some((0, 192_000)));
    }

    #[test]
    fn song_end_beat_is_bounded_for_corrupt_values() {
        // 有限どうしの和が無限へ溢れるクリップは無視する。
        let overflow = Song { length_beats: 8.0, ..song_with_clip(120.0, f64::MAX, f64::MAX) };
        assert_eq!(song_end_beat(&overflow), 8.0);
        // 巨大だが有限のクリップは上限で止める。
        assert_eq!(song_end_beat(&song_with_clip(120.0, 1e15, 4.0)), MAX_SONG_BEATS);
        // 無限の曲長は無いものとして扱う。
        let infinite = Song { length_beats: f64::INFINITY, ..song_with_clip(120.0, 4.0, 4.0) };
        assert_eq!(song_end_beat(&infinite), 8.0);
    }

    #[test]
    fn loop_bounds_without_tempo_curve_use_the_song_bpm() {
        let song = song_with_clip(120.0, 0.0, 4.0);
        let tempo = TempoMap::from_song(&song);
        let region = LoopRegion { enabled: true, start_beat: 8.0, end_beat: 24.0 };
        assert_eq!(effective_loop_bounds(Some(&song), region, 48000, &tempo, None), Some((192_000, 576_000)));
    }

    #[test]
    fn reached_at_beat_uses_the_loop_end_or_the_song_end() {
        let song = song_with_clip(120.0, 0.0, 4.0);
        let b = SongBounds::of(Some(&song), 48000, &TempoMap::from_song(&song));
        let region = LoopRegion { enabled: true, start_beat: 1.0, end_beat: 2.0 };
        assert_eq!(b.reached_at_beat(1.9, region, true), (Some(false), false));
        assert_eq!(b.reached_at_beat(2.0, region, true), (Some(true), false));
        assert_eq!(b.reached_at_beat(4.0, region, false), (None, true));
        // 範囲が無いループは曲全体を回す。
        assert_eq!(b.reached_at_beat(4.0, LoopRegion::default(), true), (Some(true), true));
    }

    #[test]
    fn ended_at_never_triggers_with_no_song() {
        assert!(!ended_at(bounds(None), 0));
        assert!(!ended_at(bounds(None), u64::MAX));
    }

    #[test]
    fn ended_at_after_final_clip_end() {
        let song = song_with_clip(120.0, 0.0, 4.0);
        assert!(!ended_at(bounds(Some(&song)), 95_999));
        assert!(ended_at(bounds(Some(&song)), 96_000));
        assert!(ended_at(bounds(Some(&song)), 96_001));
    }

    #[test]
    fn ended_at_treats_zero_length_as_not_ended() {
        let song = song_with_clip(120.0, 0.0, 0.0);
        assert!(!ended_at(bounds(Some(&song)), 1_000_000));
    }

    #[test]
    fn beat_to_bar_beat_4_4() {
        // 4/4: 4 beats per bar, 1-based.
        assert_eq!(beat_to_bar_beat(0.0, (4, 4)), (1, 1.0));
        assert_eq!(beat_to_bar_beat(3.0, (4, 4)), (1, 4.0));
        assert_eq!(beat_to_bar_beat(4.0, (4, 4)), (2, 1.0));
        // bar 9 starts at beat 32 (8 bars * 4).
        assert_eq!(beat_to_bar_beat(32.0, (4, 4)), (9, 1.0));
        let (bar, b) = beat_to_bar_beat(33.5, (4, 4));
        assert_eq!(bar, 9);
        assert!((b - 2.5).abs() < 1e-9);
    }

    #[test]
    fn beat_to_bar_beat_6_8() {
        // 6/8: beats_per_bar = 6 * 4 / 8 = 3 quarter-note beats per bar.
        assert_eq!(beat_to_bar_beat(0.0, (6, 8)), (1, 1.0));
        assert_eq!(beat_to_bar_beat(3.0, (6, 8)), (2, 1.0));
    }

    #[test]
    fn beat_to_bar_beat_handles_degenerate() {
        assert_eq!(beat_to_bar_beat(f64::NAN, (4, 4)), (1, 1.0));
        assert_eq!(beat_to_bar_beat(5.0, (0, 4)), (1, 1.0));
    }

    fn clip(start_beat: f64, length_beats: f64) -> Clip {
        Clip { id: 1, start_beat, length_beats, content_id: 0, ..Default::default() }
    }

    fn song_with_clips(clips: Vec<Clip>) -> Song {
        Song {
            bpm: 120.0,
            tracks: vec![Track { name: "T".into(), clips, ..Track::default() }],
            ..Song::default()
        }
    }

    // ---- r.md #10: Home (= earliest clip start) / End (= content end) ----

    #[test]
    fn content_bounds_none_when_empty() {
        assert_eq!(content_bounds_beats(&Song::default()), None);
    }

    #[test]
    fn content_bounds_are_min_start_and_max_end() {
        // earliest-starting clip = beat 4 (Home target); last-ending = 8+2 = 10.
        let s = song_with_clips(vec![clip(4.0, 4.0), clip(8.0, 2.0), clip(6.0, 1.0)]);
        assert_eq!(content_bounds_beats(&s), Some((4.0, 10.0)));
    }

    #[test]
    fn content_bounds_skip_zero_length_and_nonfinite() {
        // The zero-length (100) and NaN-length (50) clips are skipped, so the
        // earliest playable clip starts at 2 and content ends at 6.
        let s = song_with_clips(vec![clip(2.0, 4.0), clip(100.0, 0.0), clip(50.0, f64::NAN)]);
        assert_eq!(content_bounds_beats(&s), Some((2.0, 6.0)));
    }

    #[test]
    fn content_bounds_span_tracks() {
        // clips live on separate tracks; the envelope is the outer (min, max).
        let mut s = song_with_clips(vec![clip(5.0, 4.0)]);
        s.tracks.push(Track { name: "T2".into(), clips: vec![clip(2.0, 20.0)], ..Track::default() });
        assert_eq!(content_bounds_beats(&s), Some((2.0, 22.0)));
    }
}
