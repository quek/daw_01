//! handler::chord_event — コードトラックの箱の編集の入口 (`AppEvent::Chord`、r.md #157)。
//!
//! 編集の規則 (上書き / 正規化 / 採番) は `common::model` の `chord_ops.rs` が SSoT で、ここは
//! `edit_song` チョークポイントへ通すだけ (undo / dirty / 子プロセス sync はそこが担う)。

use common::chord_name::{ChordNotation, format_chord};
use common::model::{Chord, ChordEvent};

use crate::event_chord::ChordTrackEvent;
use crate::state::AppData;

/// 1 回の [`ChordTrackEvent::PlaceMany`] で置く箱の上限。曲 1 曲ぶん (1 小節 4 個 × 2,000 小節) を十分に
/// 超える数で、外から来る列 (OS のクリップボード / プリセットのファイル / script) の暴走だけを止める。
const MAX_PLACE_CHORDS: usize = 8192;

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

    /// 文字入力欄に入れるコード名: 表記の設定によらず音名 (`parse_chord` が読めるのは音名だけ — 度数表記の
    /// `vi7` を欄に入れると、少し書き足して確定しただけで「読めない」になる)。
    #[must_use]
    pub(crate) fn chord_entry_text(&self, chord: &Chord, song_beat: f64) -> String {
        format_chord(chord, ChordNotation::Regular, self.cur.song_doc.song().scale_at(song_beat))
    }

    /// [`ChordTrackEvent`] の処理 (`AppEvent::Chord` の 1 arm)。変化が無ければ undo を積まない。
    pub(crate) fn handle_chord_event(&mut self, ev: ChordTrackEvent) {
        if let ChordTrackEvent::PlaceMany { chords, .. } = &ev
            && chords.len() > MAX_PLACE_CHORDS
        {
            // 貼り付け (OS のクリップボード) / プリセット / script から来る列は外の入力。上限を超えたら置かない。
            self.ui_ephemeral.status_message =
                format!("コードが多すぎるので置きません ({} 個、上限 {MAX_PLACE_CHORDS} 個)", chords.len());
            return;
        }
        self.edit_song_checked(|song| match ev {
            ChordTrackEvent::Place { start, len, chord } => song
                .place_chord(ChordEvent { id: 0, start_beat: start, len_beats: len, chord, color: None })
                .is_some(),
            // 正規化は 1 回 (`Song::place_chords`)。1 箱ずつ `place_chord` すると大量の貼り付けで固まる。
            ChordTrackEvent::PlaceMany { at, chords } => {
                song.place_chords(chords.into_iter().map(|c| ChordEvent { id: 0, start_beat: at + c.start_beat, ..c }))
            }
            ChordTrackEvent::Move { ids, delta } => song.move_chords(&ids, delta),
            ChordTrackEvent::Resize { id, start, len } => song.resize_chord(id, start, len),
            ChordTrackEvent::SetSpans { spans } => set_chord_spans(song, &spans),
            ChordTrackEvent::Duplicate { ids, delta } => !song.duplicate_chords(&ids, delta).is_empty(),
            ChordTrackEvent::Delete { ids } => song.delete_chords(&ids),
            ChordTrackEvent::SetChord { ids, chord } => song.set_chord(&ids, chord),
            ChordTrackEvent::Recast { ids, chord } => song.recast_chords(&ids, chord),
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
///
/// **全部置けたときだけ確定する**: 1 つでも `place_chord` に断られたら (長さ 0 以下 / 非有限) 編集前に戻して
/// `false` (外した箱が置き直されずに黙って消えるのを防ぐ。widget の解決は長さ 0 以下を出さないが、script /
/// 将来の口からも同じ規則で守る)。
fn set_chord_spans(song: &mut common::model::Song, spans: &[(u32, f64, f64)]) -> bool {
    let old: Vec<ChordEvent> = spans.iter().filter_map(|(id, _, _)| song.chord_by_id(*id).cloned()).collect();
    let unchanged = old.iter().all(|c| {
        spans.iter().any(|&(id, s, l)| id == c.id && s == c.start_beat && l == c.len_beats)
    });
    if old.is_empty() || unchanged {
        return false;
    }
    let before = song.chords.clone();
    let ids: Vec<u32> = old.iter().map(|c| c.id).collect();
    song.delete_chords(&ids);
    for c in old {
        let Some(&(_, start, len)) = spans.iter().find(|(id, _, _)| *id == c.id) else { continue };
        if song.place_chord(ChordEvent { start_beat: start, len_beats: len, ..c }).is_none() {
            // 採番が進んでいても (割れた箱の右側) 戻さない — id は再利用しない (不変条件 1)。
            song.chords = before;
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use common::model::{Chord, ChordEvent, ChordKind};

    use crate::event::AppEvent;
    use crate::event_chord::ChordTrackEvent;
    use crate::state::AppData;

    fn place(app: &mut AppData, start: f64, len: f64) -> u32 {
        let mut id = 0;
        app.edit_song(|song| {
            let ev = ChordEvent { id: 0, start_beat: start, len_beats: len, chord: Chord::new(0, ChordKind::Major), color: None };
            id = song.place_chord(ev).expect("置ける");
        });
        id
    }

    fn layout(app: &AppData) -> Vec<(u32, f64, f64)> {
        app.cur.song_doc.song().chords.iter().map(|c| (c.id, c.start_beat, c.len_beats)).collect()
    }

    /// 範囲をまとめて変えるとき、置けない範囲 (長さ 0 以下) が 1 つでもあれば何も変えない (対象を外してから
    /// 置き直すので、断られた箱が黙って消えていた)。
    #[test]
    fn set_spans_with_an_unplaceable_span_changes_nothing() {
        let mut app = crate::test_support::headless_app();
        let a = place(&mut app, 0.0, 4.0);
        let b = place(&mut app, 4.0, 4.0);
        let before = layout(&app);
        let depth = app.cur.song_doc.undo_depth();
        app.handle_event(AppEvent::Chord(ChordTrackEvent::SetSpans { spans: vec![(a, 0.0, 6.0), (b, 6.0, -1.0)] }));
        assert_eq!(layout(&app), before, "箱は消えない");
        assert_eq!(app.cur.song_doc.undo_depth(), depth, "undo も積まない");
    }
}
