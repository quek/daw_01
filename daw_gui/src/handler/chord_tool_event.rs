//! handler::chord_tool_event — コードの検出 / 変換 / 取り込みの入口 (`AppEvent::ChordTool`、r.md #157)。

use common::chord_detect::DetectOptions;
use common::chord_voicing::Voicing;

use crate::app_types::PREVIEW_VELOCITY;
use crate::event_chord::ChordToolEvent;
use crate::state::AppData;

impl AppData {
    /// [`ChordToolEvent`] の処理 (`AppEvent::ChordTool` の 1 arm)。
    pub(crate) fn handle_chord_tool_event(&mut self, ev: ChordToolEvent) {
        match ev {
            ChordToolEvent::SetAudioChords { source_id, mut spans } => {
                // 外 (検出スレッド / script) から来る値なので、昇順・非交差・長さ > 0 に揃えてから置く。
                spans.retain(|s| s.end_frame > s.start_frame);
                spans.sort_by_key(|s| s.start_frame);
                let mut prev_end = 0u64;
                for s in &mut spans {
                    s.start_frame = s.start_frame.max(prev_end);
                    s.chord = s.chord.sanitized();
                    prev_end = prev_end.max(s.end_frame);
                }
                spans.retain(|s| s.end_frame > s.start_frame);
                self.edit_song_checked(|song| {
                    if !song.media.audio_sources.contains_key(&source_id) {
                        return false;
                    }
                    let old = song.audio_chords.get(&source_id).map_or(&[][..], Vec::as_slice);
                    if old == spans.as_slice() {
                        return false;
                    }
                    if spans.is_empty() {
                        song.audio_chords.remove(&source_id);
                    } else {
                        song.audio_chords.insert(source_id, spans);
                    }
                    true
                });
            }
            ChordToolEvent::ChordsToClip { chord_ids, track_id } => {
                self.edit_song_checked(|song| {
                    song.chords_to_clip(&chord_ids, track_id, Voicing::Basic, PREVIEW_VELOCITY).is_some()
                });
            }
            ChordToolEvent::ExtractFromClips { clips, window_beats } => {
                let opts = DetectOptions { window_beats, ..DetectOptions::default() };
                self.edit_song_checked(|song| {
                    let found = song.chords_from_clips(&clips, &opts);
                    let placed = !found.is_empty();
                    for c in found {
                        song.place_chord(c);
                    }
                    placed
                });
            }
        }
    }
}
