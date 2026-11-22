//! handler::chord_band — コードトラックの帯の選択 / 作成 / ポップアップ / メニューの口 (r.md #157 §3.1)。
//!
//! 箱の編集そのものは `AppEvent::Chord` (`handler::chord_event`) を通す — ここから Song を直接
//! 書かない (undo / dirty / 子プロセス sync は `edit_song` の唯一の口)。ここが持つのは選択
//! (`SelectionState::selected_chord_ids`) と GUI の一時状態 (`ProjectEphemeral::chord_band`) だけ。

use common::model::{Chord, ChordKind};

use crate::app::{AppEvent, EditSurface};
use crate::event_chord::ChordTrackEvent;
use crate::state::AppData;
use crate::state::chord_band::ChordPopup;
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
    /// 中身はその位置のキーのトニックの 3 和音 (キーが無ければ C) — 開いたポップアップで書き換える前提。
    pub(crate) fn create_chord_box(&mut self, start: f64, len: f64) {
        let chord = self.default_chord_at(start);
        self.handle_event(AppEvent::Chord(ChordTrackEvent::Place { start, len, chord }));
        let Some(id) = self.cur.song_doc.song().chord_at(start).filter(|c| c.start_beat == start).map(|c| c.id) else {
            return;
        };
        self.set_chord_selection(vec![id]);
        self.cur.selection.chord_anchor = Some(id);
        self.open_chord_popup(id);
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

    /// 箱のポップアップを開く (開いていれば箱を差し替える)。
    pub(crate) fn open_chord_popup(&mut self, id: u32) {
        let Some(c) = self.cur.song_doc.song().chord_by_id(id).cloned() else { return };
        let text = self.chord_label(&c.chord, c.start_beat);
        self.cur.peph.chord_band.popup = Some(ChordPopup::new(id, text));
    }

    pub(crate) fn close_chord_popup(&mut self) {
        self.cur.peph.chord_band.popup = None;
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

    /// 箱 1 つを試聴する (クリック / ポップアップでの変更)。試聴の本体は c157-tools の
    /// `AppData::audition_chord` (`handler/chord_audition.rs`、計画書 §4) で、統合時にこの関数の中身を
    /// `self.audition_chord(&chord)` の 1 行でつなぐ。帯の呼び出し側はすべてここを通す。
    pub(crate) fn audition_chord_box(&mut self, id: u32) {
        let Some(_chord) = self.cur.song_doc.song().chord_by_id(id).map(|c| c.chord) else { return };
    }
}
