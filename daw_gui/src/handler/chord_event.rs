//! handler::chord_event — コードトラックの箱の編集の入口 (`AppEvent::Chord`、r.md #157)。
//!
//! 編集の規則 (上書き / 正規化 / 採番) は `common::model` の `chord_ops.rs` が SSoT で、ここは
//! `edit_song` チョークポイントへ通すだけ (undo / dirty / 子プロセス sync はそこが担う)。

use common::chord_name::{ChordNotation, format_chord};
use common::model::{Chord, ChordEvent};

use crate::event_chord::ChordTrackEvent;
use crate::state::AppData;

impl AppData {
    /// コード名の表記 (音名 / 数字 / ローマ数字)。**コード名を出す全ての表示面がこれを引く**
    /// (帯 / ポップアップ / ピアノロール上端 / 今と次のコード)。
    #[must_use]
    #[allow(dead_code)] // 表示面 (帯 / ピアノロール) が載るまで script からしか呼ばれない。載ったら外す。
    pub(crate) fn chord_notation(&self) -> ChordNotation {
        ChordNotation::Regular
    }

    /// 表示名 1 つ (`chord_notation` と、その位置のキーの綴り)。表示面はこれを呼ぶ。
    #[must_use]
    #[allow(dead_code)] // 同上
    pub(crate) fn chord_label(&self, chord: &Chord, song_beat: f64) -> String {
        format_chord(chord, self.chord_notation(), self.cur.song_doc.song().scale_at(song_beat))
    }

    /// [`ChordTrackEvent`] の処理 (`AppEvent::Chord` の 1 arm)。変化が無ければ undo を積まない。
    pub(crate) fn handle_chord_event(&mut self, ev: ChordTrackEvent) {
        self.edit_song_checked(|song| match ev {
            ChordTrackEvent::Place { start, len, chord } => song
                .place_chord(ChordEvent { id: 0, start_beat: start, len_beats: len, chord, color: None })
                .is_some(),
            ChordTrackEvent::PlaceMany { at, chords } => {
                let mut placed = false;
                for c in chords {
                    placed |= song
                        .place_chord(ChordEvent { id: 0, start_beat: at + c.start_beat, ..c })
                        .is_some();
                }
                placed
            }
            ChordTrackEvent::Move { ids, delta } => song.move_chords(&ids, delta),
            ChordTrackEvent::Resize { id, start, len } => song.resize_chord(id, start, len),
            ChordTrackEvent::Duplicate { ids, delta } => !song.duplicate_chords(&ids, delta).is_empty(),
            ChordTrackEvent::Delete { ids } => song.delete_chords(&ids),
            ChordTrackEvent::SetChord { ids, chord } => song.set_chord(&ids, chord),
            ChordTrackEvent::Transpose { ids, semitones } => song.transpose_chords(&ids, semitones),
            ChordTrackEvent::SetColor { ids, color } => song.set_chord_color(&ids, color),
            ChordTrackEvent::Clear => {
                let had = !song.chords.is_empty();
                song.chords.clear();
                had
            }
        });
    }
}
