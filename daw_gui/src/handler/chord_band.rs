//! handler::chord_band — コードトラックの帯の選択 / 作成 / ポップアップ / メニューの口 (r.md #157 §3.1)。
//!
//! 箱の編集そのものは `AppEvent::Chord` (`handler::chord_event`) を通す — ここから Song を直接
//! 書かない (undo / dirty / 子プロセス sync は `edit_song` の唯一の口)。ここが持つのは選択
//! (`SelectionState::selected_chord_ids`) と GUI の一時状態 (`ProjectEphemeral::chord_band`) だけ。

use common::model::{Chord, ChordKind};

use crate::app::{AppEvent, EditSurface};
use crate::event_chord::ChordTrackEvent;
use crate::state::AppData;
use crate::widgets::select_modifier::{SelectModifier, range_ordered};

/// 空きのドラッグで選ぶときの合わせ方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RangeSelectMode {
    /// 範囲にかかる箱だけにする。
    Replace,
    /// 今の選択に足す (Shift)。
    Add,
    /// 範囲にかかる箱の選択を反転する (Ctrl)。
    Toggle,
}

impl RangeSelectMode {
    #[must_use]
    pub(crate) fn from_modifiers(shift: bool, ctrl: bool) -> Self {
        if shift {
            Self::Add
        } else if ctrl {
            Self::Toggle
        } else {
            Self::Replace
        }
    }
}

impl AppData {
    /// 選択のうち今も在る箱 (undo / 削除で消えた id を落とす)。Delete / Copy / 移調の対象はこれ。
    #[must_use]
    pub(crate) fn live_chord_ids(&self) -> Vec<u32> {
        let song = self.cur.song_doc.song();
        self.cur.selection.selected_chord_ids.iter().copied().filter(|id| song.chord_by_id(*id).is_some()).collect()
    }

    /// 選択を差し替え、「最後に選んだ面」を帯にする (空になったらタグを降ろす — 他の面と同じ規則)。
    pub(crate) fn set_chord_selection(&mut self, ids: Vec<u32>) {
        let empty = ids.is_empty();
        self.cur.selection.selected_chord_ids = ids;
        if empty {
            if self.cur.selection.last_edit_select == Some(EditSurface::Chords) {
                self.cur.selection.last_edit_select = None;
            }
        } else {
            self.cur.selection.last_edit_select = Some(EditSurface::Chords);
        }
    }

    /// 箱のクリック (Shift = アンカーからの範囲 / Ctrl = 切替)。
    pub(crate) fn select_chord(&mut self, id: u32, modifier: SelectModifier) {
        let prev = self.live_chord_ids();
        let anchor = self.cur.selection.chord_anchor;
        let order: Vec<u32> = self.cur.song_doc.song().chords.iter().map(|c| c.id).collect();
        let next = modifier.resolve(&prev, id, || range_ordered(&order, anchor?, id));
        if modifier.updates_anchor() {
            self.cur.selection.chord_anchor = Some(id);
        }
        self.set_chord_selection(next);
    }

    /// 空きのドラッグで範囲にかかった箱を選ぶ (短クリックは `ids` 空 = 素なら選択を外す)。
    pub(crate) fn select_chords_in_range(&mut self, ids: Vec<u32>, mode: RangeSelectMode) {
        let prev = self.live_chord_ids();
        let next = match mode {
            RangeSelectMode::Replace => ids,
            RangeSelectMode::Add => {
                let mut v = prev;
                v.extend(ids.into_iter().filter(|id| !v.contains(id)).collect::<Vec<_>>());
                v
            }
            RangeSelectMode::Toggle => {
                let mut v: Vec<u32> = prev.iter().copied().filter(|id| !ids.contains(id)).collect();
                v.extend(ids.into_iter().filter(|id| !prev.contains(id)));
                v
            }
        };
        self.set_chord_selection(next);
    }

    /// 同じコードの箱を全部選ぶ (右クリックメニュー)。
    pub(crate) fn select_same_chords(&mut self, id: u32) {
        let song = self.cur.song_doc.song();
        let Some(chord) = song.chord_by_id(id).map(|c| c.chord) else { return };
        let ids = song.chords.iter().filter(|c| c.chord == chord).map(|c| c.id).collect();
        self.set_chord_selection(ids);
    }

    /// 空きのダブルクリック: `[start, start + len)` に箱を作り、選んでポップアップを開く。
    /// 中身はその位置のキーのトニックの 3 和音 (キーが無ければ C) — 開いたポップアップでコードを決める前提で、
    /// 決めるのは「置く」(r.md #159、メロディーは動かない。`ChordPopup::placing`)。
    pub(crate) fn create_chord_box(&mut self, start: f64, len: f64) {
        let chord = self.default_chord_at(start);
        self.handle_event(AppEvent::Chord(ChordTrackEvent::Place { start, len, chord }));
        let Some(id) = self.cur.song_doc.song().chord_at(start).filter(|c| c.start_beat == start).map(|c| c.id) else {
            return;
        };
        self.set_chord_selection(vec![id]);
        self.cur.selection.chord_anchor = Some(id);
        // ダブルクリックで作るたびに、作った箱だけを「置いている最中」とするポップアップとして開き直す。
        self.close_chord_popup();
        self.open_chord_popup(id);
        self.mark_chord_popup_placing(id);
    }

    /// `beat` の位置のキーのトニックの 3 和音 (短 3 度を持つスケールなら minor)。
    #[must_use]
    pub(crate) fn default_chord_at(&self, beat: f64) -> Chord {
        self.cur.song_doc.song().scale_at(beat).map_or(Chord::new(0, ChordKind::Major), |k| {
            let mask = k.scale.pitch_class_mask();
            let minor = mask & (1 << 3) != 0 && mask & (1 << 4) == 0;
            Chord::new(k.root, if minor { ChordKind::Minor } else { ChordKind::Major })
        })
    }

    /// 帯の右クリック: 箱の上なら (選択の外なら) その箱を選んでからメニューを開く。
    pub(crate) fn open_chord_menu(&mut self, id: Option<u32>, pos: (f32, f32)) {
        if let Some(id) = id
            && !self.live_chord_ids().contains(&id)
        {
            self.set_chord_selection(vec![id]);
            self.cur.selection.chord_anchor = Some(id);
        }
        self.cur.peph.chord_band.menu = Some((id, pos));
        self.cur.peph.chord_band.menu_open = true;
    }

    /// 箱 1 つを試聴する (クリック / ポップアップでの変更 / 候補の選択)。本体は
    /// `AppData::audition_chord` (`handler/chord_audition.rs`、試聴トラックが無ければ無音)。
    pub(crate) fn audition_chord_box(&mut self, id: u32) {
        let Some(chord) = self.cur.song_doc.song().chord_by_id(id).map(|c| c.chord) else { return };
        self.audition_chord(&chord);
    }
}

#[cfg(test)]
mod tests {
    use common::model::{Chord, ChordEvent, ChordKind, LoopRegion};

    use crate::event::AppEvent;
    use crate::event_chord::ChordToolEvent;

    fn ev(start: f64) -> ChordEvent {
        ChordEvent { id: 0, start_beat: start, len_beats: 4.0, chord: Chord::new(0, ChordKind::Major), color: None }
    }

    /// 曲を差し替えると、箱の選択・ポップアップ・メニューは前の曲の id なので捨てる (id は曲ごとに 1 から振るので、
    /// 残すと新しい曲の別の箱に当たる)。ViewState を持たない旧ファイルでは、試聴トラック (track id) と帯の表示も
    /// 前のプロジェクトのものを持ち越さない。
    #[test]
    fn replacing_the_song_drops_chord_references_and_tools_view() {
        let mut app = crate::test_support::headless_app();
        let mut id = 0;
        app.edit_song(|song| id = song.place_chord(ev(0.0)).expect("置ける"));
        app.set_chord_selection(vec![id]);
        app.cur.selection.chord_anchor = Some(id);
        app.open_chord_popup(id);
        app.open_chord_menu(Some(id), (0.0, 0.0));
        let t1 = app.cur.song_doc.song().tracks[0].id;
        app.handle_event(AppEvent::ChordTool(ChordToolEvent::SetAuditionTrack(Some(t1))));
        app.cur.view.chord_band.visible = false;

        // 新しい曲: 別の位置に、同じ id の箱がある。
        let mut song = app.cur.song_doc.song().clone();
        song.chords.clear();
        song.ids.next_chord_id = 1;
        assert_eq!(song.place_chord(ev(8.0)), Some(id));
        app.cur.song_doc.replace_song(song);
        app.after_song_replaced();
        app.restore_view_state(None, LoopRegion::default(), Vec::new());

        assert!(app.cur.selection.selected_chord_ids.is_empty());
        assert_eq!(app.cur.selection.chord_anchor, None);
        assert_eq!(app.chord_popup_id(), None);
        assert_eq!(app.cur.peph.chord_band.menu, None);
        assert_eq!(app.cur.chord_tools.view.audition_track, None, "旧ファイルは試聴トラックを持ち越さない");
        assert!(app.cur.view.chord_band.visible, "旧ファイルは帯を既定 (表示) で出す");
    }
}
