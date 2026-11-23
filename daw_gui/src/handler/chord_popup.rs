//! handler::chord_popup — コードのポップアップ (Q4、Studio One の Chord Selector 型) の操作。
//!
//! 変更は即反映 (`ChordTrackEvent::SetChord` を 1 回ずつ = 1 undo step) し、試聴の口
//! (`audition_chord_box`) を鳴らす。描画は `view::chord_popup`、状態は `ProjectEphemeral::chord_band.popup`。

use common::chord_name::parse_chord;
use common::chord_suggest::suggest_next;
use common::model::Chord;

use crate::app::AppEvent;
use crate::event_chord::ChordTrackEvent;
use crate::state::AppData;
use crate::state::chord_band::ChordPopup;

/// 隣の箱と「接している」とみなす差 (拍)。
const ADJACENT_EPS: f64 = 1e-9;

impl AppData {
    /// ポップアップが開いている箱の id。
    #[must_use]
    pub(crate) fn chord_popup_id(&self) -> Option<u32> {
        self.cur.peph.chord_band.popup.as_ref().map(|p| p.chord_id)
    }

    /// 次のコードの候補 (直前の箱とその位置のキーから)。描画と上下キーの巡回がこれを共有する。
    #[must_use]
    pub(crate) fn chord_popup_candidates(&self) -> Vec<Chord> {
        let Some(id) = self.chord_popup_id() else { return Vec::new() };
        let song = self.cur.song_doc.song();
        let Some(i) = song.chords.iter().position(|c| c.id == id) else { return Vec::new() };
        let start = song.chords[i].start_beat;
        let prev = i.checked_sub(1).map(|j| &song.chords[j].chord);
        suggest_next(prev, song.scale_at(start).map(|k| (k.root, k.scale)))
    }

    /// 入力欄の打鍵 (`on_change`)。箱のコードは Enter / Tab まで変えない。
    pub(crate) fn chord_popup_typed(&mut self, text: String) {
        if let Some(p) = self.cur.peph.chord_band.popup.as_mut() {
            p.typed = true;
            p.text = text;
        }
    }

    /// ポップアップの箱のコードを差し替えて試聴し、入力欄を今の名前で出し直す。このポップアップで作った箱なら
    /// 置いている最中 (`ChordTrackEvent::Recast`、メロディーは動かない)、それ以外は書き換え (r.md #159)。
    pub(crate) fn chord_popup_set(&mut self, chord: Chord) {
        let Some(p) = self.cur.peph.chord_band.popup.as_ref() else { return };
        let (id, placing) = (p.chord_id, p.placing.contains(&p.chord_id));
        let ids = vec![id];
        let ev = if placing { ChordTrackEvent::Recast { ids, chord } } else { ChordTrackEvent::SetChord { ids, chord } };
        self.handle_event(AppEvent::Chord(ev));
        self.audition_chord_box(id);
        self.refresh_chord_popup_text();
    }

    /// 入力欄の文字列を読んで確定する。読めなければ何もしない (`false`)。
    pub(crate) fn chord_popup_commit_text(&mut self) -> bool {
        let Some(chord) = self.cur.peph.chord_band.popup.as_ref().and_then(|p| parse_chord(&p.text)) else {
            return false;
        };
        self.chord_popup_set(chord);
        true
    }

    /// Enter: 読めれば確定して閉じる (読めない間は開いたまま)。
    pub(crate) fn chord_popup_enter(&mut self) {
        let typed = self.cur.peph.chord_band.popup.as_ref().is_some_and(|p| p.typed);
        if !typed || self.chord_popup_commit_text() {
            self.close_chord_popup();
        }
    }

    /// ←→: 隣の箱へ移る (`dir` = -1 / +1)。端なら何もしない。
    pub(crate) fn chord_popup_step(&mut self, dir: i32) {
        let Some(id) = self.chord_popup_id() else { return };
        let song = self.cur.song_doc.song();
        let Some(i) = song.chords.iter().position(|c| c.id == id) else { return };
        let j = if dir < 0 { i.checked_sub(1) } else { Some(i + 1) };
        let Some(next) = j.and_then(|j| song.chords.get(j)).map(|c| c.id) else { return };
        self.focus_chord_box(next);
    }

    /// Tab: 打ち換えていれば確定してから、次の箱へ移る。接した次の箱が無ければ、同じ長さ
    /// (次の箱までの隙間に収まる長さ) の箱を作る。中身は候補の先頭 (無ければ今のコード)。
    pub(crate) fn chord_popup_tab(&mut self) {
        if self.cur.peph.chord_band.popup.as_ref().is_some_and(|p| p.typed) {
            self.chord_popup_commit_text();
        }
        let Some(id) = self.chord_popup_id() else { return };
        let song = self.cur.song_doc.song();
        let Some(i) = song.chords.iter().position(|c| c.id == id) else { return };
        let cur = song.chords[i].clone();
        let end = cur.end_beat();
        let next = song.chords.get(i + 1);
        if let Some(n) = next
            && n.start_beat - end < ADJACENT_EPS
        {
            let nid = n.id;
            self.focus_chord_box(nid);
            return;
        }
        let len = next.map_or(cur.len_beats, |n| cur.len_beats.min(n.start_beat - end));
        // 次の箱の候補は「今の箱の次」なので、今の箱を直前として引く。
        let key = song.scale_at(end).map(|k| (k.root, k.scale));
        let chord = suggest_next(Some(&cur.chord), key).first().copied().unwrap_or(cur.chord);
        self.handle_event(AppEvent::Chord(ChordTrackEvent::Place { start: end, len, chord }));
        let Some(new_id) = self.cur.song_doc.song().chord_at(end).filter(|c| (c.start_beat - end).abs() < ADJACENT_EPS).map(|c| c.id)
        else {
            return;
        };
        self.focus_chord_box(new_id);
        self.mark_chord_popup_placing(new_id);
    }

    /// 箱 `id` をこのポップアップで作った箱にする ([`ChordPopup::placing`])。
    pub(crate) fn mark_chord_popup_placing(&mut self, id: u32) {
        if let Some(p) = self.cur.peph.chord_band.popup.as_mut()
            && !p.placing.contains(&id)
        {
            p.placing.push(id);
        }
    }

    /// ↑↓: 候補を巡回し、選んだ候補をそのまま箱に入れる (`delta` = -1 / +1)。
    pub(crate) fn chord_popup_cycle(&mut self, delta: i32) {
        let candidates = self.chord_popup_candidates();
        if candidates.is_empty() {
            return;
        }
        let n = candidates.len() as i32;
        let cur = self.cur.peph.chord_band.popup.as_ref().and_then(|p| p.candidate);
        let next = match cur {
            Some(c) => (c as i32 + delta).rem_euclid(n),
            None if delta > 0 => 0,
            None => n - 1,
        } as usize;
        self.chord_popup_set(candidates[next]);
        if let Some(p) = self.cur.peph.chord_band.popup.as_mut() {
            p.candidate = Some(next);
        }
    }

    /// 箱 `id` を選び、ポップアップをそこへ移して試聴する (←→ / Tab の行き先)。
    fn focus_chord_box(&mut self, id: u32) {
        self.set_chord_selection(vec![id]);
        self.cur.selection.chord_anchor = Some(id);
        self.open_chord_popup(id);
        self.audition_chord_box(id);
    }

    /// 入力欄を今のコード名で出し直す (世代を進めて欄を作り直す)。
    fn refresh_chord_popup_text(&mut self) {
        let Some(id) = self.chord_popup_id() else { return };
        let Some(c) = self.cur.song_doc.song().chord_by_id(id).cloned() else { return };
        let text = self.chord_entry_text(&c.chord, c.start_beat);
        if let Some(p) = self.cur.peph.chord_band.popup.as_mut() {
            p.text = text;
            p.typed = false;
            p.rev = p.rev.wrapping_add(1);
        }
    }

    /// 箱のポップアップを開く (開いていれば箱を差し替え、入力欄を作り直す。このポップアップで作った箱の集合は
    /// 閉じるまで持ち越す)。
    pub(crate) fn open_chord_popup(&mut self, id: u32) {
        let Some(c) = self.cur.song_doc.song().chord_by_id(id).cloned() else { return };
        let text = self.chord_entry_text(&c.chord, c.start_beat);
        let prev = self.cur.peph.chord_band.popup.take();
        let rev = prev.as_ref().map_or(0, |p| p.rev.wrapping_add(1));
        let placing = prev.map(|p| p.placing).unwrap_or_default();
        let played_seen = self.cur.chord_tools.played.map_or(0, |(_, seq)| seq);
        self.cur.peph.chord_band.popup = Some(ChordPopup { rev, played_seen, placing, ..ChordPopup::new(id, text) });
    }

    /// Instrument Input: ポップアップを開いている間に MIDI 鍵盤で弾き終えた和音を、編集中の箱に入れる
    /// (`ChordToolsState::played` の通し番号が進んだときだけ。試聴も鳴らす)。
    pub(crate) fn chord_popup_take_played(&mut self) {
        let Some((chord, seq)) = self.cur.chord_tools.played else { return };
        let Some(p) = self.cur.peph.chord_band.popup.as_mut() else { return };
        if p.played_seen == seq {
            return;
        }
        p.played_seen = seq;
        self.chord_popup_set(chord);
    }

    pub(crate) fn close_chord_popup(&mut self) {
        self.cur.peph.chord_band.popup = None;
    }
}
