//! Sample / beat conversion helpers shared by `daw_plugin_host`
//! (audio thread) and `daw_gui` (playhead rendering).

use crate::model::{Clip, LoopRegion, Song};

/// Every clip that contributes to the song's playable envelope: finite,
/// positive-length, finite-start clips across all tracks. Single source of
/// truth for the clip filter shared by [`song_bounds_samples`],
/// [`latest_clip_start_beat`], and [`content_end_beat`] — a change to the
/// filtering rule (e.g. excluding muted tracks) lives here, not in three
/// copies. Non-finite (NaN / ±Inf) or non-positive geometry is skipped: it
/// slips past a plain `<= 0.0` guard and would saturate the u64 sample cast /
/// overflow `start + length` in the sample-domain caller.
fn playable_clips(song: &Song) -> impl Iterator<Item = &Clip> {
    song.tracks.iter().flat_map(|t| t.clips.iter()).filter(|c| {
        c.length_beats.is_finite() && c.length_beats > 0.0 && c.start_beat.is_finite()
    })
}

/// Returns `(earliest_clip_start_samples, latest_clip_end_samples)` across
/// every clip on every track in `song`, or `None` if there is nothing
/// playable: no song, no non-empty clip, `bpm <= 0`, or all clips have zero
/// length.
///
/// The `None` case means "do not loop, do not play": downstream code should
/// treat it as end-of-song. The bounds are the outer envelope of all
/// material; playhead iterates over this span once per loop cycle.
pub fn song_bounds_samples(song: Option<&Song>, sample_rate: u32) -> Option<(u64, u64)> {
    let song = song?;
    if song.bpm <= 0.0 {
        return None;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let mut min_start: Option<u64> = None;
    let mut max_end: u64 = 0;
    for clip in playable_clips(song) {
        let start = (clip.start_beat * samples_per_beat).max(0.0) as u64;
        let length = (clip.length_beats * samples_per_beat) as u64;
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

/// True when `playhead` has advanced past the last clip's end (or there is
/// nothing playable across the whole song).
pub fn song_ended(song: Option<&Song>, sample_rate: u32, playhead: u64) -> bool {
    match song_bounds_samples(song, sample_rate) {
        Some((_, end)) => playhead >= end,
        None => false,
    }
}

/// Converts a sample-domain playhead to a beat-domain position. `None` when
/// the song has no BPM defined.
pub fn playhead_to_beat(song: Option<&Song>, sample_rate: u32, playhead: u64) -> Option<f64> {
    let song = song?;
    if song.bpm <= 0.0 {
        return None;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    Some(playhead as f64 / samples_per_beat)
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

/// Converts a beat-domain position to seconds using the song's constant
/// tempo. Inverse of the constant-`bpm` mapping `playhead_to_beat` uses, so
/// the displayed time matches the audio engine's playback position.
pub fn beat_to_seconds(beat: f64, bpm: f32) -> f64 {
    if bpm <= 0.0 || !beat.is_finite() {
        return 0.0;
    }
    beat * 60.0 / f64::from(bpm)
}

/// Returns the sample range the audio engine should treat as the active
/// playback loop. Prefers the user-defined [`LoopRegion`] range when it is
/// non-empty; otherwise falls back to the full song-content envelope from
/// [`song_bounds_samples`].
///
/// The loop region is **session state**, not part of `Song` (see
/// [`LoopRegion`]): the engine receives it via `AudioCommand::SetLoop` and
/// passes it in here. `song` still supplies the BPM for the beat → sample
/// conversion and the content-envelope fallback.
pub fn effective_loop_bounds(
    song: Option<&Song>,
    loop_region: LoopRegion,
    sample_rate: u32,
) -> Option<(u64, u64)> {
    let song_ref = song?;
    if song_ref.bpm > 0.0 && loop_region.has_range() {
        let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song_ref.bpm);
        let start = (loop_region.start_beat * samples_per_beat).max(0.0) as u64;
        let end = (loop_region.end_beat * samples_per_beat).max(0.0) as u64;
        if end > start {
            return Some((start, end));
        }
    }
    song_bounds_samples(song, sample_rate)
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

    #[test]
    fn bounds_none_for_no_song() {
        assert_eq!(song_bounds_samples(None, 48000), None);
    }

    #[test]
    fn bounds_none_for_empty_tracks() {
        assert_eq!(song_bounds_samples(Some(&Song::default()), 48000), None);
    }

    #[test]
    fn bounds_none_for_zero_bpm() {
        let song = song_with_clip(0.0, 0.0, 4.0);
        assert_eq!(song_bounds_samples(Some(&song), 48000), None);
    }

    #[test]
    fn bounds_none_for_zero_length_clip() {
        let song = song_with_clip(120.0, 0.0, 0.0);
        assert_eq!(song_bounds_samples(Some(&song), 48000), None);
    }

    #[test]
    fn bounds_standard_clip() {
        // 120 BPM, 48 kHz: 1 beat = 24000 samples; 4 beats = 96000 samples.
        let song = song_with_clip(120.0, 0.0, 4.0);
        assert_eq!(song_bounds_samples(Some(&song), 48000), Some((0, 96_000)));
    }

    #[test]
    fn bounds_with_offset() {
        // Start at beat 2 → 48000 samples in; length 4 beats → end at 144000.
        let song = song_with_clip(120.0, 2.0, 4.0);
        assert_eq!(
            song_bounds_samples(Some(&song), 48000),
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
        assert_eq!(song_bounds_samples(Some(&song), 48000), Some((0, 192_000)));
    }

    #[test]
    fn song_ended_never_triggers_with_no_song() {
        assert!(!song_ended(None, 48000, 0));
        assert!(!song_ended(None, 48000, u64::MAX));
    }

    #[test]
    fn song_ended_after_final_clip_end() {
        let song = song_with_clip(120.0, 0.0, 4.0);
        assert!(!song_ended(Some(&song), 48000, 95_999));
        assert!(song_ended(Some(&song), 48000, 96_000));
        assert!(song_ended(Some(&song), 48000, 96_001));
    }

    #[test]
    fn song_ended_treats_zero_length_as_not_ended() {
        let song = song_with_clip(120.0, 0.0, 0.0);
        assert!(!song_ended(Some(&song), 48000, 1_000_000));
    }

    #[test]
    fn playhead_to_beat_basic() {
        let song = song_with_clip(120.0, 0.0, 4.0);
        // 120 BPM, 48 kHz: samples_per_beat = 24000.
        assert_eq!(playhead_to_beat(Some(&song), 48000, 0), Some(0.0));
        assert_eq!(playhead_to_beat(Some(&song), 48000, 24_000), Some(1.0));
        assert_eq!(playhead_to_beat(Some(&song), 48000, 96_000), Some(4.0));
    }

    #[test]
    fn playhead_to_beat_none_for_invalid_song() {
        assert_eq!(playhead_to_beat(None, 48000, 0), None);
        let song = song_with_clip(0.0, 0.0, 4.0);
        assert_eq!(playhead_to_beat(Some(&song), 48000, 0), None);
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

    #[test]
    fn beat_to_seconds_constant_tempo() {
        // 120 BPM: 1 beat = 0.5 s; matches playhead_to_beat inverse.
        assert!((beat_to_seconds(0.0, 120.0) - 0.0).abs() < 1e-9);
        assert!((beat_to_seconds(1.0, 120.0) - 0.5).abs() < 1e-9);
        assert!((beat_to_seconds(4.0, 120.0) - 2.0).abs() < 1e-9);
        // 140 BPM: 32 beats → 32 * 60/140 ≈ 13.714 s.
        assert!((beat_to_seconds(32.0, 140.0) - 13.714285714).abs() < 1e-6);
        assert_eq!(beat_to_seconds(4.0, 0.0), 0.0);
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
