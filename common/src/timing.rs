//! Sample / beat / row conversion helpers shared by `daw_plugin_host`
//! (audio thread) and `daw_gui` (playhead-row highlight).

use crate::model::Song;

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
    for track in &song.tracks {
        for clip in &track.clips {
            if clip.length_beats <= 0.0 {
                continue;
            }
            let start = (clip.start_beat * samples_per_beat).max(0.0) as u64;
            let length = (clip.length_beats * samples_per_beat) as u64;
            if length == 0 {
                continue;
            }
            min_start = Some(min_start.map_or(start, |m| m.min(start)));
            max_end = max_end.max(start + length);
        }
    }
    let start = min_start?;
    if max_end <= start {
        return None;
    }
    Some((start, max_end))
}

/// True when `playhead` has advanced past the last clip's end (or there is
/// nothing playable across the whole song).
pub fn song_ended(song: Option<&Song>, sample_rate: u32, playhead: u64) -> bool {
    match song_bounds_samples(song, sample_rate) {
        Some((_, end)) => playhead >= end,
        None => false,
    }
}

/// Maps an absolute playhead (samples from song 0) to the tracker row index
/// within the specified track's clip 0. Returns `None` when:
/// - there is no playable clip on that track,
/// - `rows_per_beat == 0`,
/// - `playhead` is before the clip start, or
/// - `playhead` is past the clip's last row.
///
/// The computed row is clamped to `[0, rows.len())` when the clip has a row
/// buffer; otherwise the raw index is returned (capped by the clip length).
pub fn playhead_to_row(
    song: Option<&Song>,
    track_idx: u32,
    sample_rate: u32,
    playhead: u64,
) -> Option<u32> {
    let song = song?;
    if song.bpm <= 0.0 {
        return None;
    }
    let track = song.tracks.get(track_idx as usize)?;
    let clip = track.clips.first()?;
    if clip.rows_per_beat == 0 || clip.length_beats <= 0.0 {
        return None;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let start = (clip.start_beat * samples_per_beat).max(0.0) as u64;
    let length = (clip.length_beats * samples_per_beat) as u64;
    if length == 0 {
        return None;
    }
    let end = start + length;
    if playhead < start || playhead >= end {
        return None;
    }
    let samples_per_row = samples_per_beat / f64::from(clip.rows_per_beat);
    if samples_per_row <= 0.0 {
        return None;
    }
    let offset = (playhead - start) as f64;
    let row = (offset / samples_per_row).floor() as u32;
    if !clip.rows.is_empty() {
        let max = (clip.rows.len() as u32).saturating_sub(1);
        Some(row.min(max))
    } else {
        Some(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Clip, InstrumentSource, Row, Track};

    fn song_with_clip(bpm: f32, start_beat: f64, length_beats: f64) -> Song {
        song_with_clip_rows(bpm, start_beat, length_beats, 0)
    }

    fn song_with_clip_rows(
        bpm: f32,
        start_beat: f64,
        length_beats: f64,
        row_count: usize,
    ) -> Song {
        Song {
            bpm,
            tracks: vec![Track {
                name: "T".into(),
                source: InstrumentSource::BuiltinSynth,
                clips: vec![Clip {
                    name: "C".into(),
                    start_beat,
                    length_beats,
                    rows_per_beat: 4,
                    rows: vec![Row::default(); row_count],
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
                        name: "A0".into(),
                        start_beat: 0.0,
                        length_beats: 4.0,
                        rows_per_beat: 4,
                        rows: Vec::new(),
                    }],
                    ..Track::default()
                },
                Track {
                    name: "B".into(),
                    clips: vec![Clip {
                        name: "B0".into(),
                        start_beat: 6.0,
                        length_beats: 2.0,
                        rows_per_beat: 4,
                        rows: Vec::new(),
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
    fn playhead_to_row_none_when_no_song() {
        assert_eq!(playhead_to_row(None, 0, 48000, 0), None);
    }

    #[test]
    fn playhead_to_row_none_when_track_missing() {
        let song = song_with_clip_rows(120.0, 0.0, 4.0, 16);
        assert_eq!(playhead_to_row(Some(&song), 5, 48000, 0), None);
    }

    #[test]
    fn playhead_to_row_none_when_clip_missing() {
        assert_eq!(playhead_to_row(Some(&Song::default()), 0, 48000, 0), None);
    }

    #[test]
    fn playhead_to_row_zero_at_clip_start() {
        // 120 BPM, rows_per_beat 4: samples_per_row = 6000.
        let song = song_with_clip_rows(120.0, 0.0, 4.0, 16);
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 0), Some(0));
    }

    #[test]
    fn playhead_to_row_advances_with_samples() {
        let song = song_with_clip_rows(120.0, 0.0, 4.0, 16);
        // samples_per_row = 48000*60/120/4 = 6000
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 5_999), Some(0));
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 6_000), Some(1));
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 12_000), Some(2));
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 90_000), Some(15));
    }

    #[test]
    fn playhead_to_row_none_before_clip_start() {
        let song = song_with_clip_rows(120.0, 2.0, 4.0, 16);
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 0), None);
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 47_999), None);
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 48_000), Some(0));
    }

    #[test]
    fn playhead_to_row_none_past_clip_end() {
        let song = song_with_clip_rows(120.0, 0.0, 4.0, 16);
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 96_000), None);
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 200_000), None);
    }

    #[test]
    fn playhead_to_row_clamps_to_available_rows() {
        let song = song_with_clip_rows(120.0, 0.0, 4.0, 8);
        assert_eq!(playhead_to_row(Some(&song), 0, 48000, 60_000), Some(7));
    }
}
