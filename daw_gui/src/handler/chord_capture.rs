//! handler::chord_capture — MIDI 鍵盤で弾いた和音をコードにする (r.md #157 §3.4)。
//!
//! 和音のまとめ方: 1 つ目の鍵を押してから **全部の鍵を離すまで** に押した鍵を 1 つの和音とし、
//! `common::chord_detect::match_chord` (検出と同じ判定、最低音 = ベース) でコードにする。
//!
//! - **録音** (帯の録音待機 ON + 録音中): 和音の頭 (押し始めの playhead、アレンジの snap) から箱を置く。
//!   箱は次の和音の頭まで伸びる (Studio One の Chord Track の録音と同じ — 弾き直すまでそのコード)。
//!   最後の和音は離した位置で終わる。録音の take の undo step に入る (`GestureOwner::RecordingTake`)。
//! - **Instrument Input** (録音していないとき): 弾いた和音を [`ChordToolsState::played`] に出す。
//!   帯のポップアップは開いている間これを見て、変わったら編集中の箱に設定する (Studio One の Instrument Input)。
//!
//! [`ChordToolsState::played`]: crate::state::chord_tools::ChordToolsState::played

use common::chord_detect::{DetectOptions, match_chord};
use common::model::{Chord, ChordEvent};

use crate::state::AppData;

/// 録音で置く箱の最短の長さ (拍)。離すのが早すぎた和音でも見える箱にする。
const MIN_RECORDED_LEN: f64 = 0.25;

impl AppData {
    /// MIDI 入力の note-on (`handle_midi_note_on` から、ランチャーに飲まれなかったもの)。
    pub(crate) fn chord_capture_note_on(&mut self, pitch: u8) {
        let recording = self.chord_recording_live();
        let beat = recording.then(|| self.chord_capture_beat());
        let cap = &mut self.cur.chord_tools.capture;
        if cap.held.contains(&pitch) {
            return;
        }
        if cap.held.is_empty() {
            // 新しい和音の頭。前の和音を録った箱はここまで伸ばす。
            cap.chord_notes.clear();
            cap.start_beat = beat;
            if let (Some(b), Some(prev)) = (beat, cap.last_recorded) {
                self.extend_recorded_chord_to(prev, b);
            }
        }
        let cap = &mut self.cur.chord_tools.capture;
        cap.held.push(pitch);
        if !cap.chord_notes.contains(&pitch) {
            cap.chord_notes.push(pitch);
        }
    }

    /// MIDI 入力の note-off。全部の鍵が離れたら、押した鍵を 1 つの和音として確定する。
    pub(crate) fn chord_capture_note_off(&mut self, pitch: u8) {
        let cap = &mut self.cur.chord_tools.capture;
        let Some(i) = cap.held.iter().position(|&p| p == pitch) else { return };
        cap.held.swap_remove(i);
        if !cap.held.is_empty() {
            return;
        }
        let notes = std::mem::take(&mut cap.chord_notes);
        let start = cap.start_beat.take();
        let Some(chord) = chord_from_keys(&notes, &self.cur.chord_tools.view.detect) else { return };
        let tools = &mut self.cur.chord_tools;
        tools.played_seq = tools.played_seq.wrapping_add(1);
        tools.played = Some((chord, tools.played_seq));
        if let Some(start) = start.filter(|_| self.chord_recording_live()) {
            let end = self.chord_capture_beat().max(start + MIN_RECORDED_LEN);
            let mut placed = None;
            self.edit_song(|song| {
                placed = song.place_chord(ChordEvent { id: 0, start_beat: start, len_beats: end - start, chord, color: None });
            });
            self.cur.chord_tools.capture.last_recorded = placed;
        }
    }

    /// 帯の録音が記録してよい状態か (録音待機 ON + engine が録音中と publish している)。
    pub(crate) fn chord_recording_live(&self) -> bool {
        self.cur.chord_tools.record_armed && self.cur.recording.live
    }

    /// 録音を閉じたとき (`close_recording_session` から、take の undo step の中): 押したままの和音を
    /// `playhead` までの箱にし、録音の途中状態を捨てる (鍵はまだ押されているので、離したときは
    /// 「弾いたコード」に出るだけ)。
    pub(crate) fn end_chord_recording(&mut self, playhead: f64) {
        let cap = &mut self.cur.chord_tools.capture;
        let start = cap.start_beat.take();
        cap.last_recorded = None;
        if let Some(start) = start.filter(|_| self.cur.chord_tools.record_armed)
            && let Some(chord) = chord_from_keys(&self.cur.chord_tools.capture.chord_notes, &self.cur.chord_tools.view.detect)
        {
            let len = (playhead - start).max(MIN_RECORDED_LEN);
            self.edit_song(|song| song.place_chord(ChordEvent { id: 0, start_beat: start, len_beats: len, chord, color: None }));
        }
    }

    /// 和音の頭 / 終わりの拍: いまの playhead をアレンジの snap に合わせたもの。
    fn chord_capture_beat(&self) -> f64 {
        let raw = self.cur.transport.playhead_beat.map_or(0.0, f64::from).max(0.0);
        crate::view::snap::arrange_snap_config(self).snap_beat(raw, false, self.cur.view.arrange_zoom_x).max(0.0)
    }

    /// 録った箱 `id` の終わりを `end` まで伸ばす (次の和音を弾き始めた位置まで。縮めはしない)。
    fn extend_recorded_chord_to(&mut self, id: u32, end: f64) {
        let Some(c) = self.cur.song_doc.song().chord_by_id(id).cloned() else { return };
        if end > c.end_beat() + 1e-9 {
            self.edit_song(|song| song.resize_chord(id, c.start_beat, end - c.start_beat));
        }
    }
}

/// 押した鍵 `keys` (順不同) のコード。3 音未満や名前の付かない組なら `None`。
#[must_use]
pub(crate) fn chord_from_keys(keys: &[u8], opts: &DetectOptions) -> Option<Chord> {
    let mut weights = [0.0f64; 12];
    for &k in keys {
        weights[usize::from(k % 12)] = 1.0;
    }
    let lowest = keys.iter().min().map(|k| k % 12);
    match_chord(&weights, lowest, opts)
}

#[cfg(test)]
mod tests {
    use common::model::ChordKind;

    use crate::event::AppEvent;
    use crate::event_chord::ChordToolEvent;

    fn play(app: &mut crate::state::AppData, keys: &[u8]) {
        for &k in keys {
            app.chord_capture_note_on(k);
        }
        for &k in keys {
            app.chord_capture_note_off(k);
        }
    }

    /// 録音していないときは弾いた和音を「弾いたコード」に出すだけ (Song は変えない)。録音中は和音の頭から
    /// 箱を置き、次の和音の頭まで伸ばす。
    #[test]
    fn played_chords_become_boxes_while_recording_and_a_mailbox_otherwise() {
        let mut app = crate::test_support::headless_app();
        play(&mut app, &[52, 55, 60]); // C/E
        let (c, _) = app.cur.chord_tools.played.expect("弾いたコード");
        assert_eq!((c.root, c.kind, c.bass), (0, ChordKind::Major, Some(4)));
        assert!(app.cur.song_doc.song().chords.is_empty());
        play(&mut app, &[60, 64]); // 2 音はコードにしない
        assert_eq!(app.cur.chord_tools.played.map(|p| p.1), Some(1));

        app.handle_event(AppEvent::ChordTool(ChordToolEvent::SetRecordArmed(true)));
        app.cur.recording.live = true;
        app.cur.view.arrange_snap_enabled = false;
        app.cur.transport.playhead_beat = Some(0.0);
        for k in [57, 60, 64] {
            app.chord_capture_note_on(k);
        }
        app.cur.transport.playhead_beat = Some(1.5);
        for k in [57, 60, 64] {
            app.chord_capture_note_off(k);
        }
        app.cur.transport.playhead_beat = Some(4.0);
        for k in [53, 57, 60] {
            app.chord_capture_note_on(k);
        }
        app.cur.transport.playhead_beat = Some(6.0);
        for k in [53, 57, 60] {
            app.chord_capture_note_off(k);
        }
        let got: Vec<(f64, f64, u8, ChordKind)> =
            app.cur.song_doc.song().chords.iter().map(|c| (c.start_beat, c.len_beats, c.chord.root, c.chord.kind)).collect();
        assert_eq!(got, vec![(0.0, 4.0, 9, ChordKind::Minor), (4.0, 2.0, 5, ChordKind::Major)]);
    }
}
