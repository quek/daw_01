//! handler::chord_midi_import — MIDI ファイル → コードトラック (r.md #157 §3.4)。
//!
//! エクスプローラから `.mid` を帯へドロップすると、全トラックのノート (ドラムの ch 10 を除く) から
//! コードを検出して置く。SMF の読みは `crate::midi_import` (トラックへの取り込みと同じ解析、
//! `docs/plan_midi_import.md`)、検出は `common::chord_detect::extract_chords` (クリップからの抽出と同じ)。
//! SMF の tick 0 がドロップ位置に来る。テンポは採らない (帯はコードだけを受ける)。

use std::path::Path;

use common::chord_detect::{DetectNote, extract_chords};
use common::model::ChordEvent;

use crate::state::AppData;

/// GM のドラム (ch 10、0 起点で 9)。コードの材料にしない。
const DRUM_CHANNEL: u8 = 9;

impl AppData {
    /// `path` の SMF からコードを検出し、拍 `at` を tick 0 として帯に置く (1 undo step)。置いた数を返す。
    pub(crate) fn import_midi_chords(&mut self, path: &Path, at: f64) -> usize {
        let tempo_map = common::tempo_map::TempoMap::from_song(self.cur.song_doc.song());
        let parsed = match crate::midi_import::read_and_parse(path, &|s| tempo_map.seconds_to_beat(s)) {
            Ok(p) => p,
            Err(e) => {
                self.ui_ephemeral.status_message = format!("MIDI からコードを読めません: {}: {e}", path.display());
                return 0;
            }
        };
        let notes: Vec<DetectNote> = parsed
            .tracks
            .iter()
            .filter(|t| t.channel != DRUM_CHANNEL)
            .flat_map(|t| t.notes.iter().filter(|n| !n.muted))
            .map(|n| DetectNote { start_beat: n.start_beat, len_beats: n.duration_beats, pitch: n.pitch })
            .collect();
        let found = extract_chords(&notes, &self.cur.chord_tools.view.detect);
        if found.is_empty() {
            self.ui_ephemeral.status_message = format!("{}: コードが見つかりません", path.display());
            return 0;
        }
        let at = if at.is_finite() { at.max(0.0) } else { 0.0 };
        let n = found.len();
        self.edit_song(|song| {
            song.place_chords(found.into_iter().map(|s| ChordEvent::new(at + s.start_beat, s.len_beats, s.chord)));
        });
        self.ui_ephemeral.status_message = format!("MIDI からコードを {n} 個置きました");
        n
    }
}

#[cfg(test)]
mod tests {
    use common::chord_voicing::Voicing;
    use common::model::{Chord, ChordEvent, ChordKind};

    /// 帯 → ノート化 → SMF 書き出し → 別の曲の帯へ取り込み で、同じ進行がドロップ位置からずれて戻る。
    #[test]
    fn a_progression_survives_an_smf_round_trip() {
        let mut app = crate::test_support::headless_app();
        let track = app.cur.song_doc.song().tracks[0].id;
        let prog = [(0, ChordKind::Major), (9, ChordKind::Minor7), (5, ChordKind::Major), (7, ChordKind::Dominant7)];
        app.edit_song(|song| {
            for (i, (root, kind)) in prog.iter().enumerate() {
                song.place_chord(ChordEvent::new(i as f64 * 4.0, 4.0, Chord::new(*root, *kind)));
            }
            song.chords_to_clip(&[], track, Voicing::Piano, 100);
            song.chords.clear();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prog.mid");
        crate::midi_export::export_midi(app.cur.song_doc.song(), &path).unwrap();
        app.handle_event(crate::event::AppEvent::ChordTool(crate::event_chord::ChordToolEvent::SetDetectOptions(
            common::chord_detect::DetectOptions { window_beats: 4.0, ..Default::default() },
        )));
        assert_eq!(app.import_midi_chords(&path, 8.0), 4);
        let got: Vec<(f64, u8, ChordKind)> =
            app.cur.song_doc.song().chords.iter().map(|c| (c.start_beat, c.chord.root, c.chord.kind)).collect();
        let want: Vec<(f64, u8, ChordKind)> = prog.iter().enumerate().map(|(i, (r, k))| (8.0 + i as f64 * 4.0, *r, *k)).collect();
        assert_eq!(got, want);
    }
}
