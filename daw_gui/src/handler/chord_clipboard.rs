//! handler::chord_clipboard — コードトラックの箱の Copy / Cut / Paste / 複製 (r.md #157 §3.1)。
//!
//! OS クリップボードには他の面と同じ envelope (`ClipboardPayload::Chords`) で載せ、貼るのは
//! `ChordTrackEvent::PlaceMany` (上書き規則で置く、1 undo step)。貼り先はポインタが帯の上にあるときの
//! その拍 (`view::clipboard_ops::paste_from_clipboard` が決める)。

use common::model::ChordEvent;

use crate::app::AppEvent;
use crate::clipboard::{ClipboardEnvelope, ClipboardPayload};
use crate::event_chord::ChordTrackEvent;
use crate::state::AppData;

/// 貼った箱を見つけるときの位置の許容差 (拍)。
const EPS: f64 = 1e-9;

impl AppData {
    /// 選んでいる箱の写し (最も早い箱を 0 とした相対、id 0)。選択が無ければ `None`。
    #[must_use]
    pub(crate) fn selected_chords_relative(&self) -> Option<Vec<ChordEvent>> {
        let ids = self.live_chord_ids();
        let song = self.cur.song_doc.song();
        let picked: Vec<&ChordEvent> = song.chords.iter().filter(|c| ids.contains(&c.id)).collect();
        let origin = picked.iter().map(|c| c.start_beat).fold(f64::INFINITY, f64::min);
        if !origin.is_finite() {
            return None;
        }
        Some(picked.into_iter().map(|c| ChordEvent { id: 0, start_beat: c.start_beat - origin, ..c.clone() }).collect())
    }

    /// Ctrl+C: 選んでいる箱を clipboard envelope (JSON) にする。`(json, 個数)`。
    #[must_use]
    pub(crate) fn copy_chords_clip(&self) -> Option<(String, usize)> {
        let chords = self.selected_chords_relative()?;
        let n = chords.len();
        let json = ClipboardEnvelope::new(self.cur.song_doc.song().project_id, ClipboardPayload::Chords(chords)).to_json()?;
        Some((json, n))
    }

    /// `at` を基準に箱の列を置き (上書き規則)、置いた箱を選ぶ。貼り付け / プリセットの挿入が使う。
    /// 置かなかったとき (上限超え / 全部不正) は選択を変えない (同じ位置にある既存の箱を選ばない)。
    pub(crate) fn place_chords_at(&mut self, at: f64, chords: Vec<ChordEvent>) {
        let mut starts: Vec<f64> = chords.iter().map(|c| (at + c.start_beat).max(0.0)).collect();
        starts.sort_by(f64::total_cmp);
        let epoch = self.cur.song_doc.edit_epoch();
        self.handle_event(AppEvent::Chord(ChordTrackEvent::PlaceMany { at, chords }));
        if self.cur.song_doc.edit_epoch() == epoch {
            return;
        }
        // 置いた箱 = 頭が置いた位置にある箱 (貼った頭を並べておき二分探索 — 大量の貼り付けで O(n²) にしない)。
        let placed: Vec<u32> = self
            .cur
            .song_doc
            .song()
            .chords
            .iter()
            .filter(|c| {
                let i = starts.partition_point(|&s| s < c.start_beat - EPS);
                starts.get(i).is_some_and(|&s| (s - c.start_beat).abs() < EPS)
            })
            .map(|c| c.id)
            .collect();
        self.set_chord_selection(placed);
    }

    /// D / Alt+D: 選んでいる箱を、選択の範囲 (最初の箱の頭〜最後の箱の終わり) の直後へ複製し、
    /// 複製を選ぶ (連打で後ろへ続く。クリップの D と同じ)。
    pub(crate) fn duplicate_selected_chords(&mut self) {
        let Some(chords) = self.selected_chords_relative() else { return };
        let ids = self.live_chord_ids();
        let song = self.cur.song_doc.song();
        let picked = song.chords.iter().filter(|c| ids.contains(&c.id));
        let (lo, hi) = picked.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), c| (lo.min(c.start_beat), hi.max(c.end_beat())));
        if hi > lo {
            self.place_chords_at(hi, chords);
        }
    }
}
