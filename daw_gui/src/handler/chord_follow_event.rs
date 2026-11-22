//! handler::chord_follow_event — コードトラックへの追従の設定の入口 (`AppEvent::ChordFollow`、r.md #157)。
//!
//! 追従の評価は `common::chord_follow` が SSoT。ここは設定 (`Song::chord_follow_enabled` /
//! `Track::follow_chords`) を `edit_song` へ通すだけで、engine / VOICEVOX / 印はフレーム末の同期と描画が
//! Song から読み直す。

use crate::event_chord::ChordFollowEvent;
use crate::state::AppData;

impl AppData {
    /// [`ChordFollowEvent`] の処理 (`AppEvent::ChordFollow` の 1 arm)。変化が無ければ undo を積まない。
    pub(crate) fn handle_chord_follow_event(&mut self, ev: ChordFollowEvent) {
        self.edit_song_checked(|song| match ev {
            ChordFollowEvent::SetEnabled(on) => {
                let changed = song.chord_follow_enabled != on;
                song.chord_follow_enabled = on;
                changed
            }
            ChordFollowEvent::SetTracks { track_ids, mode } => {
                let mut changed = false;
                for t in song.tracks.iter_mut().filter(|t| track_ids.contains(&t.id)) {
                    changed |= t.follow_chords != mode;
                    t.follow_chords = mode;
                }
                changed
            }
        });
    }
}
