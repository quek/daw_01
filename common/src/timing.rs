//! Sample / beat conversion helpers shared by `daw_plugin_host`
//! (audio thread) and `daw_gui` (playhead rendering).

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

/// Returns the sample range the audio engine should treat as the active
/// playback loop. Prefers the user-defined `Song::loop_*_beat` range when
/// it is non-empty; otherwise falls back to the full song-content
/// envelope from [`song_bounds_samples`].
pub fn effective_loop_bounds(song: Option<&Song>, sample_rate: u32) -> Option<(u64, u64)> {
    let song_ref = song?;
    if song_ref.bpm > 0.0 && song_ref.loop_end_beat > song_ref.loop_start_beat {
        let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song_ref.bpm);
        let start = (song_ref.loop_start_beat * samples_per_beat).max(0.0) as u64;
        let end = (song_ref.loop_end_beat * samples_per_beat).max(0.0) as u64;
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
                    name: "C".into(),
                    start_beat,
                    length_beats,
                    content_id: 0,
                    notes: Vec::new(),
                    color: None,
                    auto_lipsync: false,
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
                        name: "A0".into(),
                        start_beat: 0.0,
                        length_beats: 4.0,
                        content_id: 0,
                        notes: Vec::new(),
                        color: None,
                        auto_lipsync: false,
                    }],
                    ..Track::default()
                },
                Track {
                    name: "B".into(),
                    clips: vec![Clip {
                        id: 1,
                        name: "B0".into(),
                        start_beat: 6.0,
                        length_beats: 2.0,
                        content_id: 0,
                        notes: Vec::new(),
                        color: None,
                        auto_lipsync: false,
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
}
