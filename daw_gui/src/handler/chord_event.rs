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
    /// 中身は帯の見出しで選ぶ表記 (`ProjectView::chord_band`、`ViewState` に保存)。
    #[must_use]
    pub(crate) fn chord_notation(&self) -> ChordNotation {
        self.cur.view.chord_band.notation
    }

    /// 表示名 1 つ (`chord_notation` と、その位置のキーの綴り)。表示面はこれを呼ぶ。
    #[must_use]
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
            ChordTrackEvent::SetSpans { spans } => set_chord_spans(song, &spans),
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

/// 箱の範囲をまとめて変える (境界の連動 / 比例伸縮)。対象を先に全部外してから置き直すので、
/// 新しい範囲どうしが互いを削らない (外さずに 1 つずつ置くと、先に伸ばした箱が後の箱を覆って消す)。
/// かかった他の箱は上書き規則で削られる。変わったら `true`。
fn set_chord_spans(song: &mut common::model::Song, spans: &[(u32, f64, f64)]) -> bool {
    let old: Vec<ChordEvent> = spans.iter().filter_map(|(id, _, _)| song.chord_by_id(*id).cloned()).collect();
    let unchanged = old.iter().all(|c| {
        spans.iter().any(|&(id, s, l)| id == c.id && s == c.start_beat && l == c.len_beats)
    });
    if old.is_empty() || unchanged {
        return false;
    }
    let ids: Vec<u32> = old.iter().map(|c| c.id).collect();
    song.delete_chords(&ids);
    for c in old {
        let Some(&(_, start, len)) = spans.iter().find(|(id, _, _)| *id == c.id) else { continue };
        song.place_chord(ChordEvent { start_beat: start, len_beats: len, ..c });
    }
    true
}
