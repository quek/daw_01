//! handler::chord_popup — コードのポップアップ (Q4、Studio One の Chord Selector 型) の操作。
//!
//! 変更は即反映し (1 回ずつ = 1 undo step、変化が無ければ積まない)、試聴の口 (`audition_chord_box`) を鳴らす。
//! 空きのダブルクリック / Tab で開くのは置く前の箱 (`PopupTarget::Draft`、曲にはまだ無い) で、コードを選んだ時点で
//! 置く (`ChordTrackEvent::Place`)。選ばずに閉じる / 離れると何も残さない (r.md #159)。このポップアップで置いた箱は
//! 閉じるまで「置く」(`ChordTrackEvent::Recast`)、ほかの箱は「書き換え」(`ChordTrackEvent::SetChord`) — 使い分けの
//! 規則は `ChordPopup::placing` の doc。描画は `view::chord_popup`、状態は `ProjectEphemeral::chord_band.popup`。

use common::chord_name::parse_chord;
use common::chord_suggest::suggest_next;
use common::model::{Chord, ChordEvent};

use crate::app::AppEvent;
use crate::event_chord::ChordTrackEvent;
use crate::state::AppData;
use crate::state::chord_band::{ChordDraft, ChordPopup, PopupTarget};

/// 隣の箱と「接している」とみなす差 (拍)。
const ADJACENT_EPS: f64 = 1e-9;

impl AppData {
    /// ポップアップが開いている箱の id (置く前の箱なら `None`)。
    #[must_use]
    pub(crate) fn chord_popup_id(&self) -> Option<u32> {
        self.cur.peph.chord_band.popup.as_ref().and_then(ChordPopup::box_id)
    }

    /// ポップアップが編集している箱 (置く前の箱なら、その位置と入力欄に出しているコードで作った値。id は持たない)。
    /// 描画が読む。
    #[must_use]
    pub(crate) fn chord_popup_event(&self) -> Option<ChordEvent> {
        match self.cur.peph.chord_band.popup.as_ref()?.target {
            PopupTarget::Box(id) => self.cur.song_doc.song().chord_by_id(id).cloned(),
            PopupTarget::Draft(d) => Some(ChordEvent::new(d.start_beat, d.len_beats, d.chord)),
        }
    }

    /// 次のコードの候補 (直前の箱とその位置のキーから)。描画と上下キーの巡回がこれを共有する。
    #[must_use]
    pub(crate) fn chord_popup_candidates(&self) -> Vec<Chord> {
        let Some(p) = self.cur.peph.chord_band.popup.as_ref() else { return Vec::new() };
        let song = self.cur.song_doc.song();
        let (start, prev) = match p.target {
            PopupTarget::Box(id) => {
                let Some(i) = song.chords.iter().position(|c| c.id == id) else { return Vec::new() };
                (song.chords[i].start_beat, i.checked_sub(1).map(|j| &song.chords[j].chord))
            }
            // 置く前の箱は空きに居るので、開始より前に始まる最後の箱が直前。
            PopupTarget::Draft(d) => {
                (d.start_beat, song.chords.iter().take_while(|c| c.start_beat < d.start_beat).last().map(|c| &c.chord))
            }
        };
        suggest_next(prev, song.scale_at(start).map(|k| (k.root, k.scale)))
    }

    /// 入力欄の打鍵 (`on_change`)。箱のコードは Enter / Tab まで変えない。
    pub(crate) fn chord_popup_typed(&mut self, text: String) {
        if let Some(p) = self.cur.peph.chord_band.popup.as_mut() {
            p.typed = true;
            p.text = text;
        }
    }

    /// ポップアップの箱のコードを差し替えて試聴し、入力欄を今の名前で出し直す。置く前の箱ならここで置く。
    /// このポップアップで置いた箱なら置いている最中 (`ChordTrackEvent::Recast`、メロディーは動かない)、それ以外は
    /// 書き換え (r.md #159)。
    pub(crate) fn chord_popup_set(&mut self, chord: Chord) {
        let Some(p) = self.cur.peph.chord_band.popup.as_ref() else { return };
        let id = match p.target {
            PopupTarget::Draft(d) => {
                let Some(id) = self.place_chord_draft(d, chord) else { return };
                id
            }
            PopupTarget::Box(id) => {
                let ids = vec![id];
                let ev = if p.placing.contains(&id) {
                    ChordTrackEvent::Recast { ids, chord }
                } else {
                    ChordTrackEvent::SetChord { ids, chord }
                };
                self.handle_event(AppEvent::Chord(ev));
                id
            }
        };
        self.audition_chord_box(id);
        self.refresh_chord_popup_text();
    }

    /// 置く前の箱 `d` を `chord` で曲に置き、選んでポップアップをその箱へ移す (このポップアップで置いた箱にする)。
    /// 置けなかった (書き出し中) ら `None` で、置く前の箱のまま。
    fn place_chord_draft(&mut self, d: ChordDraft, chord: Chord) -> Option<u32> {
        self.handle_event(AppEvent::Chord(ChordTrackEvent::Place { start: d.start_beat, len: d.len_beats, chord }));
        let id = self
            .cur
            .song_doc
            .song()
            .chord_at(d.start_beat)
            .filter(|c| (c.start_beat - d.start_beat).abs() < ADJACENT_EPS)
            .map(|c| c.id)?;
        self.set_chord_selection(vec![id]);
        self.cur.selection.chord_anchor = Some(id);
        if let Some(p) = self.cur.peph.chord_band.popup.as_mut() {
            p.target = PopupTarget::Box(id);
            if !p.placing.contains(&id) {
                p.placing.push(id);
            }
        }
        Some(id)
    }

    /// 入力欄の文字列を読んで確定する。読めなければ何もしない (`false`)。
    pub(crate) fn chord_popup_commit_text(&mut self) -> bool {
        let Some(chord) = self.cur.peph.chord_band.popup.as_ref().and_then(|p| parse_chord(&p.text)) else {
            return false;
        };
        self.chord_popup_set(chord);
        true
    }

    /// 打ち換えていなければ、置く前の箱を入力欄に出しているコードで置く (Enter / Tab の「これで決める」)。
    /// 打ち換えていれば読んで確定する。返り値 = 先へ進んでよいか (読めない名前なら `false`)。
    fn chord_popup_accept(&mut self) -> bool {
        let Some(p) = self.cur.peph.chord_band.popup.as_ref() else { return false };
        if p.typed {
            return self.chord_popup_commit_text();
        }
        if let Some(d) = p.draft() {
            self.chord_popup_set(d.chord);
        }
        true
    }

    /// Enter: 決めて閉じる (読めない名前の間は開いたまま)。
    pub(crate) fn chord_popup_enter(&mut self) {
        if self.chord_popup_accept() {
            self.close_chord_popup();
        }
    }

    /// ←→: 隣の箱へ移る (`dir` = -1 / +1)。端なら何もしない。置く前の箱からは前後の箱へ移り、置く前の箱は残さない。
    pub(crate) fn chord_popup_step(&mut self, dir: i32) {
        let Some(p) = self.cur.peph.chord_band.popup.as_ref() else { return };
        let song = self.cur.song_doc.song();
        let next = match p.target {
            PopupTarget::Box(id) => {
                let Some(i) = song.chords.iter().position(|c| c.id == id) else { return };
                let j = if dir < 0 { i.checked_sub(1) } else { Some(i + 1) };
                j.and_then(|j| song.chords.get(j))
            }
            PopupTarget::Draft(d) if dir < 0 => song.chords.iter().rev().find(|c| c.start_beat < d.start_beat),
            PopupTarget::Draft(d) => song.chords.iter().find(|c| c.start_beat > d.start_beat),
        };
        let Some(next) = next.map(|c| c.id) else { return };
        self.focus_chord_box(next);
    }

    /// Tab: 今の箱を決めてから、次の箱へ移る。接した次の箱が無ければ、同じ長さ (次の箱までの隙間に収まる長さ) の
    /// 置く前の箱を開く。出すコードは候補の先頭 (無ければ今のコード)。読めない名前の間は動かない。
    pub(crate) fn chord_popup_tab(&mut self) {
        if !self.chord_popup_accept() {
            return;
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
        self.open_chord_popup_on(PopupTarget::Draft(ChordDraft { start_beat: end, len_beats: len, chord }));
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

    /// 箱のポップアップを開く (開いていれば箱を差し替え、入力欄を作り直す。このポップアップで置いた箱の集合は
    /// 閉じるまで持ち越す)。
    pub(crate) fn open_chord_popup(&mut self, id: u32) {
        self.open_chord_popup_on(PopupTarget::Box(id));
    }

    /// 空きのダブルクリック: `[start, start + len)` の置く前の箱でポップアップを開く (曲はまだ変えない)。
    /// 出すコードはその位置のキーのトニックの 3 和音 (キーが無ければ C)。開くたびに、このポップアップで置いた箱の
    /// 集合は空から始める。
    pub(crate) fn open_chord_draft(&mut self, start: f64, len: f64) {
        let chord = self.default_chord_at(start);
        self.close_chord_popup();
        self.open_chord_popup_on(PopupTarget::Draft(ChordDraft { start_beat: start, len_beats: len, chord }));
    }

    fn open_chord_popup_on(&mut self, target: PopupTarget) {
        let (chord, start) = match target {
            PopupTarget::Box(id) => {
                let Some(c) = self.cur.song_doc.song().chord_by_id(id) else { return };
                (c.chord, c.start_beat)
            }
            PopupTarget::Draft(d) => (d.chord, d.start_beat),
        };
        let text = self.chord_entry_text(&chord, start);
        let prev = self.cur.peph.chord_band.popup.take();
        let rev = prev.as_ref().map_or(0, |p| p.rev.wrapping_add(1));
        let placing = prev.map(|p| p.placing).unwrap_or_default();
        let played_seen = self.cur.chord_tools.played.map_or(0, |(_, seq)| seq);
        self.cur.peph.chord_band.popup = Some(ChordPopup { rev, played_seen, placing, ..ChordPopup::on(target, text) });
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

    /// 閉じる。置く前の箱は何も残さない (曲に入っていない)。
    pub(crate) fn close_chord_popup(&mut self) {
        self.cur.peph.chord_band.popup = None;
    }
}
