//! handler::chord_tool_event — コードの検出 / 変換 / 取り込みと、試聴の設定の入口 (`AppEvent::ChordTool`、r.md #157)。

use common::chord_voicing::Voicing;

use crate::app_types::PREVIEW_VELOCITY;
use crate::event_chord::ChordToolEvent;
use crate::state::AppData;

impl AppData {
    /// [`ChordToolEvent`] の処理 (`AppEvent::ChordTool` の 1 arm)。
    pub(crate) fn handle_chord_tool_event(&mut self, ev: ChordToolEvent) {
        match ev {
            ChordToolEvent::SetAudioChords { source_id, spans, place_on } => {
                self.set_audio_chords(source_id, spans, &place_on);
            }
            ChordToolEvent::ChordsToClip { chord_ids, track_id } => {
                let voicing: Voicing = self.cur.chord_tools.view.voicing;
                self.edit_song_checked(|song| {
                    song.chords_to_clip(&chord_ids, track_id, voicing, PREVIEW_VELOCITY).is_some()
                });
            }
            ChordToolEvent::ExtractFromClips { clips, window_beats } => {
                let opts = common::chord_detect::DetectOptions { window_beats, ..self.cur.chord_tools.view.detect };
                self.edit_song_checked(|song| {
                    let found = song.chords_from_clips(&clips, &opts);
                    let placed = !found.is_empty();
                    for c in found {
                        song.place_chord(c);
                    }
                    placed
                });
            }
            ChordToolEvent::DetectAudioClips { clips, place } => self.detect_audio_clips(&clips, place),
            ChordToolEvent::SetAuditionTrack(track) => {
                self.release_chord_audition();
                self.cur.chord_tools.view.audition_track = track;
            }
            ChordToolEvent::SetAuditionWhilePlaying(on) => self.cur.chord_tools.view.audition_while_playing = on,
            ChordToolEvent::SetVoicing(v) => {
                self.cur.chord_tools.view.voicing = v;
                self.cur.chord_tools.last_voiced.clear();
            }
            ChordToolEvent::SetDetectOptions(o) => self.cur.chord_tools.view.detect = sanitize_detect(o),
            ChordToolEvent::SetRecordArmed(on) => self.cur.chord_tools.record_armed = on,
        }
    }

    /// 保存値 (`ViewState.chord_tools`) を流し込む (`restore_view_state` から)。
    pub(crate) fn restore_chord_tools_view(&mut self, v: common::model::ChordToolsView) {
        self.release_chord_audition();
        self.cur.chord_tools.last_voiced.clear();
        self.cur.chord_tools.view = common::model::ChordToolsView { detect: sanitize_detect(v.detect), ..v };
    }
}

/// 検出の設定を有効域へ (disk / script から来た値)。
fn sanitize_detect(mut o: common::chord_detect::DetectOptions) -> common::chord_detect::DetectOptions {
    let d = common::chord_detect::DetectOptions::default();
    if !(o.window_beats.is_finite() && o.window_beats > 0.0) {
        o.window_beats = d.window_beats;
    }
    o.window_beats = o.window_beats.clamp(0.25, 64.0);
    if !(o.ignore_shorter_than_beats.is_finite() && o.ignore_shorter_than_beats >= 0.0) {
        o.ignore_shorter_than_beats = 0.0;
    }
    o
}
