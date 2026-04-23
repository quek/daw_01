//! Sample / beat / row conversion helpers shared by `daw_plugin_host`
//! (audio thread) and `daw_gui` (playhead-row highlight).

use crate::model::Song;

/// Returns `(clip_start_samples, clip_end_samples)` for the MVP playback
/// target (track 0 / clip 0), or `None` if there is nothing playable:
/// no song, empty track list, missing clip, `bpm <= 0`, or zero-length clip.
/// The `None` case means "do not loop, do not play": downstream code should
/// treat it as end-of-song.
pub fn clip_bounds_samples(song: Option<&Song>, sample_rate: u32) -> Option<(u64, u64)> {
    let song = song?;
    let track = song.tracks.first()?;
    let clip = track.clips.first()?;
    if song.bpm <= 0.0 || clip.length_beats <= 0.0 {
        return None;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let start = (clip.start_beat * samples_per_beat).max(0.0) as u64;
    let length = (clip.length_beats * samples_per_beat) as u64;
    if length == 0 {
        return None;
    }
    Some((start, start + length))
}

/// True when `playhead` has advanced past the clip end (or there is nothing
/// playable).
pub fn song_ended(song: Option<&Song>, sample_rate: u32, playhead: u64) -> bool {
    match clip_bounds_samples(song, sample_rate) {
        Some((_, end)) => playhead >= end,
        None => false,
    }
}

/// Maps an absolute playhead (samples from song 0) to the tracker row index
/// within clip 0. Returns `None` when:
/// - there is no playable clip (`clip_bounds_samples` is `None`),
/// - `rows_per_beat == 0`,
/// - `playhead` is before the clip start, or
/// - `playhead` is past the last row.
///
/// The computed row is clamped to `[0, rows.len())` when the clip has a row
/// buffer; otherwise the raw index is returned (capped by the clip length).
pub fn playhead_to_row(song: Option<&Song>, sample_rate: u32, playhead: u64) -> Option<u32> {
    let song = song?;
    let track = song.tracks.first()?;
    let clip = track.clips.first()?;
    if clip.rows_per_beat == 0 {
        return None;
    }
    let (start, end) = clip_bounds_samples(Some(song), sample_rate)?;
    if playhead < start || playhead >= end {
        return None;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
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
                fx_chain: vec![],
                volume: 1.0,
                pan: 0.0,
                clips: vec![Clip {
                    name: "C".into(),
                    start_beat,
                    length_beats,
                    rows_per_beat: 4,
                    rows: vec![Row::default(); row_count],
                }],
            }],
            ..Song::default()
        }
    }

    #[test]
    fn clip_bounds_none_for_no_song() {
        assert_eq!(clip_bounds_samples(None, 48000), None);
    }

    #[test]
    fn clip_bounds_none_for_empty_tracks() {
        assert_eq!(clip_bounds_samples(Some(&Song::default()), 48000), None);
    }

    #[test]
    fn clip_bounds_none_for_zero_bpm() {
        let song = song_with_clip(0.0, 0.0, 4.0);
        assert_eq!(clip_bounds_samples(Some(&song), 48000), None);
    }

    #[test]
    fn clip_bounds_none_for_zero_length_clip() {
        let song = song_with_clip(120.0, 0.0, 0.0);
        assert_eq!(clip_bounds_samples(Some(&song), 48000), None);
    }

    #[test]
    fn clip_bounds_standard_clip() {
        // 120 BPM, 48 kHz: 1 beat = 24000 samples; 4 beats = 96000 samples.
        let song = song_with_clip(120.0, 0.0, 4.0);
        assert_eq!(clip_bounds_samples(Some(&song), 48000), Some((0, 96_000)));
    }

    #[test]
    fn clip_bounds_with_offset() {
        // Start at beat 2 → 48000 samples in; length 4 beats → end at 144000.
        let song = song_with_clip(120.0, 2.0, 4.0);
        assert_eq!(
            clip_bounds_samples(Some(&song), 48000),
            Some((48_000, 144_000))
        );
    }

    #[test]
    fn song_ended_never_triggers_with_no_song() {
        assert!(!song_ended(None, 48000, 0));
        assert!(!song_ended(None, 48000, u64::MAX));
    }

    #[test]
    fn song_ended_after_clip_end() {
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
        assert_eq!(playhead_to_row(None, 48000, 0), None);
    }

    #[test]
    fn playhead_to_row_none_when_clip_missing() {
        assert_eq!(playhead_to_row(Some(&Song::default()), 48000, 0), None);
    }

    #[test]
    fn playhead_to_row_zero_at_clip_start() {
        // 120 BPM, rows_per_beat 4: samples_per_row = 6000.
        let song = song_with_clip_rows(120.0, 0.0, 4.0, 16);
        assert_eq!(playhead_to_row(Some(&song), 48000, 0), Some(0));
    }

    #[test]
    fn playhead_to_row_advances_with_samples() {
        let song = song_with_clip_rows(120.0, 0.0, 4.0, 16);
        // samples_per_row = 48000*60/120/4 = 6000
        assert_eq!(playhead_to_row(Some(&song), 48000, 5_999), Some(0));
        assert_eq!(playhead_to_row(Some(&song), 48000, 6_000), Some(1));
        assert_eq!(playhead_to_row(Some(&song), 48000, 12_000), Some(2));
        // Row 15 is the last row; samples_per_row = 6000, so 15*6000 = 90000.
        assert_eq!(playhead_to_row(Some(&song), 48000, 90_000), Some(15));
    }

    #[test]
    fn playhead_to_row_none_before_clip_start() {
        // Clip starts at beat 2 (48_000 samples in). Playhead earlier → None.
        let song = song_with_clip_rows(120.0, 2.0, 4.0, 16);
        assert_eq!(playhead_to_row(Some(&song), 48000, 0), None);
        assert_eq!(playhead_to_row(Some(&song), 48000, 47_999), None);
        assert_eq!(playhead_to_row(Some(&song), 48000, 48_000), Some(0));
    }

    #[test]
    fn playhead_to_row_none_past_clip_end() {
        // Clip 0..4 beats → 0..96_000 samples.
        let song = song_with_clip_rows(120.0, 0.0, 4.0, 16);
        assert_eq!(playhead_to_row(Some(&song), 48000, 96_000), None);
        assert_eq!(playhead_to_row(Some(&song), 48000, 200_000), None);
    }

    #[test]
    fn playhead_to_row_clamps_to_available_rows() {
        // rows_per_beat=4, length=4 beats → 16 slots of 6000 samples each,
        // but the clip only defines 8 rows. Row 10 (sample 60_000) should
        // clamp to the last stored row (index 7).
        let song = song_with_clip_rows(120.0, 0.0, 4.0, 8);
        assert_eq!(playhead_to_row(Some(&song), 48000, 60_000), Some(7));
    }
}
