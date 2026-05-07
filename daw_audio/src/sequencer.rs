//! Walks the song's clips/notes and emits MIDI transitions for the next
//! audio buffer. Owned by daw_audio; called from each track's worker
//! before handing events off to the plugin host.
//!
//! Migrated from `daw_plugin_host` as part of A2 (audio-engine refactor).

#![allow(dead_code)]

use common::model::{Note, Song};

#[derive(Debug, Clone, Copy)]
pub enum NoteTransition {
    On { key: u8, velocity: f64 },
    Off { key: u8 },
}

#[derive(Debug, Clone, Copy)]
pub struct TimedNoteEvent {
    pub time: u32,
    pub event: NoteTransition,
}

/// Per-track state owned exclusively by the audio worker that processes
/// the track. Survives across buffers so notes don't get cut on Stop /
/// loop-wrap.
#[derive(Default)]
pub struct PerTrackState {
    /// Pitches currently sounding on this track. Used to flush stuck notes
    /// on Stop / loop wrap.
    pub active_notes: Vec<u8>,
    /// NoteOffs that must fire at frame 0 of the *next* buffer (after
    /// Stop / clip-end) so notes don't hang.
    pub pending_offs: Vec<u8>,
}

impl PerTrackState {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            active_notes: Vec::with_capacity(cap),
            pending_offs: Vec::with_capacity(cap),
        }
    }
}

/// Walk every clip on `track_idx` and emit `On` / `Off` events that fall
/// inside the half-open buffer `[playhead, playhead + frames)`.
///
/// `active_notes` is the audio worker's running set of pitches currently
/// sounding for this track — the caller maintains it across buffers so it
/// can flush stuck notes on Stop / loop wrap.
///
/// RT-safe: pushes into the caller-provided `out` (pre-allocated capacity)
/// and uses `sort_unstable_by_key` (in-place pdqsort).
pub fn collect_events_for_buffer(
    song: Option<&Song>,
    track_idx: u32,
    sample_rate: u32,
    playhead: u64,
    frames: u32,
    out: &mut Vec<TimedNoteEvent>,
    active_notes: &mut Vec<u8>,
) {
    let Some(song) = song else { return };
    let Some(track) = song.tracks.get(track_idx as usize) else {
        return;
    };
    if song.bpm <= 0.0 {
        return;
    }

    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(song.bpm);
    let buf_end = playhead + u64::from(frames);

    for clip in &track.clips {
        if clip.length_beats <= 0.0 {
            continue;
        }
        let clip_start_samples = (clip.start_beat * samples_per_beat).max(0.0) as u64;
        let clip_end_samples =
            clip_start_samples + (clip.length_beats * samples_per_beat) as u64;
        if clip_end_samples <= playhead || clip_start_samples >= buf_end {
            continue;
        }

        // v6 linked clip: notes は Song.clip_contents から取り出す。
        // 共有 clip 群は同じ content から同じ notes を見るので、 別々の
        // 配置位置 (clip.start_beat) で同じ内容が再生される。
        let notes: &[Note] = song
            .clip_contents
            .get(&clip.content_id)
            .map(|c| c.notes.as_slice())
            .unwrap_or(&[]);
        for note in notes {
            if note.duration_beats <= 0.0 {
                continue;
            }
            // Skip notes whose On is outside the clip — otherwise we could
            // emit On but lose Off to clamping, leaving a stuck note.
            if note.start_beat < 0.0 || note.start_beat >= clip.length_beats {
                continue;
            }
            let on_offset = (note.start_beat * samples_per_beat).max(0.0) as u64;
            let raw_off_offset =
                ((note.start_beat + note.duration_beats) * samples_per_beat).max(0.0) as u64;
            let on_sample = clip_start_samples + on_offset;
            // Notes extending past the clip end are clamped to the boundary.
            let off_sample = (clip_start_samples + raw_off_offset).min(clip_end_samples);

            if on_sample >= playhead && on_sample < buf_end {
                out.push(TimedNoteEvent {
                    time: (on_sample - playhead) as u32,
                    event: NoteTransition::On {
                        key: note.pitch,
                        velocity: f64::from(note.velocity) / 127.0,
                    },
                });
                active_notes.push(note.pitch);
            }
            if off_sample > on_sample && off_sample >= playhead && off_sample < buf_end {
                out.push(TimedNoteEvent {
                    time: (off_sample - playhead) as u32,
                    event: NoteTransition::Off { key: note.pitch },
                });
                if let Some(pos) = active_notes.iter().position(|&k| k == note.pitch) {
                    active_notes.swap_remove(pos);
                }
            }
        }
    }

    // CLAP requires in-events sorted by time. At equal times, Off must come
    // before On so a re-attack at the same frame doesn't drop because the
    // synth saw On→Off in the same buffer.
    out.sort_unstable_by_key(|e| {
        let priority: u8 = match e.event {
            NoteTransition::Off { .. } => 0,
            NoteTransition::On { .. } => 1,
        };
        (e.time, priority)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{Clip, ClipContent, Track};

    fn one_note_song(start_beat: f64, duration_beats: f64, pitch: u8) -> Song {
        // v6: notes は Song.clip_contents に置く。 inline の `notes:` は
        // legacy field (空) のままで、 ensure_clip_contents が migrate する
        // 想定だが、 ここでは直接 clip_contents を構築して migrate を挟まず
        // production と同形にする。
        let mut song = Song::default();
        song.bpm = 120.0;
        let content_id = song.alloc_content_id();
        song.clip_contents.insert(
            content_id,
            ClipContent {
                notes: vec![Note {
                    start_beat,
                    duration_beats,
                    pitch,
                    velocity: 100,
                    lyric: None,
                }],
            },
        );
        song.tracks.push(Track {
            id: 1,
            name: "T".into(),
            clips: vec![Clip {
                id: 1,
                name: "C".into(),
                start_beat: 0.0,
                length_beats: 8.0,
                content_id,
                notes: Vec::new(),
            }],
            ..Track::default()
        });
        song
    }

    /// 120 BPM, 48 kHz: samples_per_beat = 24000.
    const SR: u32 = 48000;
    const SPB: u64 = 24_000;

    #[test]
    fn note_starting_at_buffer_zero_emits_on_at_time_zero() {
        let song = one_note_song(0.0, 1.0, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(Some(&song), 0, SR, 0, 1024, &mut out, &mut active);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].time, 0);
        assert!(matches!(out[0].event, NoteTransition::On { key: 60, .. }));
        assert_eq!(active, vec![60]);
    }

    #[test]
    fn note_off_emitted_in_buffer_containing_end() {
        let song = one_note_song(0.0, 1.0, 60);
        let mut out = Vec::new();
        let mut active = vec![60u8];
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            SPB - 100,
            200,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].event, NoteTransition::Off { key: 60 }));
        assert!(active.is_empty(), "active set must drop the off note");
    }

    #[test]
    fn note_entirely_inside_buffer_emits_on_then_off() {
        let song = one_note_song(0.0, 0.01, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(Some(&song), 0, SR, 0, 1024, &mut out, &mut active);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0].event, NoteTransition::On { key: 60, .. }));
        assert!(matches!(out[1].event, NoteTransition::Off { key: 60 }));
        assert!(out[0].time < out[1].time);
        assert!(active.is_empty());
    }

    #[test]
    fn chord_emits_two_ons_at_same_time() {
        let mut song = one_note_song(0.0, 1.0, 60);
        let cid = song.tracks[0].clips[0].content_id;
        song.clip_contents.get_mut(&cid).unwrap().notes.push(Note {
            start_beat: 0.0,
            duration_beats: 1.0,
            pitch: 64,
            velocity: 100,
            lyric: None,
        });
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(Some(&song), 0, SR, 0, 1024, &mut out, &mut active);
        assert_eq!(out.len(), 2);
        for e in &out {
            assert_eq!(e.time, 0);
            assert!(matches!(e.event, NoteTransition::On { .. }));
        }
        active.sort_unstable();
        assert_eq!(active, vec![60, 64]);
    }

    #[test]
    fn no_song_returns_empty() {
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(None, 0, SR, 0, 1024, &mut out, &mut active);
        assert!(out.is_empty());
        assert!(active.is_empty());
    }

    #[test]
    fn note_outside_buffer_emits_nothing() {
        let song = one_note_song(2.0, 1.0, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(Some(&song), 0, SR, 0, 1000, &mut out, &mut active);
        assert!(out.is_empty());
        assert!(active.is_empty());
    }

    #[test]
    fn note_extending_past_clip_end_is_clamped() {
        let mut song = one_note_song(7.0, 4.0, 60);
        song.tracks[0].clips[0].length_beats = 8.0;
        let playhead = 8 * SPB - 100;
        let frames = 200u32;
        let mut out = Vec::new();
        let mut active = vec![60u8];
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            playhead,
            frames,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].event, NoteTransition::Off { key: 60 }));
        assert!(active.is_empty());
    }

    #[test]
    fn note_past_clip_end_is_skipped_entirely() {
        let mut song = one_note_song(10.0, 1.0, 60);
        song.tracks[0].clips[0].length_beats = 4.0;
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            10 * SPB - 100,
            200,
            &mut out,
            &mut active,
        );
        assert!(out.is_empty());
        assert!(active.is_empty());
    }

    #[test]
    fn output_is_sorted_with_off_before_on_at_same_time() {
        let mut song = Song::default();
        song.bpm = 120.0;
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent {
                notes: vec![
                    Note {
                        start_beat: 0.0,
                        duration_beats: 1.0,
                        pitch: 60,
                        velocity: 100,
                        lyric: None,
                    },
                    Note {
                        start_beat: 1.0,
                        duration_beats: 1.0,
                        pitch: 60,
                        velocity: 100,
                        lyric: None,
                    },
                ],
            },
        );
        song.tracks.push(Track {
            id: 1,
            name: "T".into(),
            clips: vec![Clip {
                id: 1,
                name: "C".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                notes: Vec::new(),
            }],
            ..Track::default()
        });
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            0,
            (2 * SPB) as u32,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0].event, NoteTransition::On { .. }));
        assert_eq!(out[0].time, 0);
        assert!(matches!(out[1].event, NoteTransition::Off { .. }));
        assert!(matches!(out[2].event, NoteTransition::On { .. }));
        assert_eq!(out[1].time, out[2].time);
    }
}
