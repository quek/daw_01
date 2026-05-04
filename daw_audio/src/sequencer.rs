//! Walks the song's clips/notes and emits MIDI transitions for the next
//! audio buffer. Owned by daw_audio; called from each track's worker
//! before handing events off to the plugin host.
//!
//! Migrated from `daw_plugin_host` as part of A2 (audio-engine refactor).

#![allow(dead_code)]

use common::model::Song;

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

        for note in &clip.notes {
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
