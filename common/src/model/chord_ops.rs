//! コードトラック (`Song.chords`) の編集と参照 (r.md #157、`docs/plan_chord_track.md`)。
//!
//! 型 ([`ChordEvent`]) は wire を渡るので `chords.rs` に置き、ここは wire に載らないロジック
//! だけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。
//!
//! 重なりの規則はクリップと同じ **後から置いた方が勝つ** ([`Song::place_chord`]): 置いた箱の範囲に
//! かかる既存の箱は、かかった部分を削る (中に収まれば 2 つに割れ、完全に覆われれば消える)。
//! 時間操作 (`time_ops.rs` / `section_ops.rs`) では箱もクリップの窓と同じく境界で割って同伴する。

use super::*;

/// 箱として残す最小の長さ (拍)。これ未満に削られた箱は消す。
const MIN_CHORD_LEN_BEATS: f64 = 1e-6;

impl Song {
    /// 新しい `ChordEvent` の安定 id を採番する。`0` は未採番 sentinel なので最低 `1`。
    pub fn alloc_chord_id(&mut self) -> u32 {
        let id = self.ids.next_chord_id.max(1);
        self.ids.next_chord_id = id.saturating_add(1);
        id
    }

    /// `chords` の不変条件を回復する: 値域 (有限・非負・長さ > 0・コードの値域)、`start_beat` 昇順、
    /// 互いに非交差 (先に始まる方を優先して後発の頭を削る)、id の採番と重複解消。idempotent。
    pub fn normalize_chords(&mut self) {
        self.chords.retain(|c| {
            c.start_beat.is_finite() && c.len_beats.is_finite() && c.len_beats > MIN_CHORD_LEN_BEATS
        });
        for c in &mut self.chords {
            if c.start_beat < 0.0 {
                c.len_beats += c.start_beat;
                c.start_beat = 0.0;
            }
            c.chord = c.chord.sanitized();
        }
        self.chords
            .sort_by(|a, b| a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal));
        let mut prev_end = f64::NEG_INFINITY;
        for c in &mut self.chords {
            if c.start_beat < prev_end {
                let end = c.end_beat();
                c.start_beat = prev_end;
                c.len_beats = end - prev_end;
            }
            prev_end = prev_end.max(c.end_beat());
        }
        self.chords.retain(|c| c.len_beats > MIN_CHORD_LEN_BEATS);
        let mut seen = std::collections::HashSet::new();
        let mut next = self.ids.next_chord_id.max(1);
        for c in &self.chords {
            next = next.max(c.id.saturating_add(1));
        }
        for c in &mut self.chords {
            if c.id == 0 || !seen.insert(c.id) {
                c.id = next;
                next = next.saturating_add(1);
                seen.insert(c.id);
            }
        }
        self.ids.next_chord_id = next;
        // 検出結果は音源に付く。音源が消えたら一緒に消す (保存前の GC と load の両方がここを通る)。
        let sources = &self.media.audio_sources;
        self.audio_chords.retain(|id, spans| sources.contains_key(id) && !spans.is_empty());
    }

    /// `beat` を覆う箱 (無ければ `None` = その位置ではどのトラックも追従しない)。
    /// `chords` は昇順・非交差なので二分探索で引く (RT からも呼べる: 確保・ロックなし)。
    #[must_use]
    pub fn chord_at(&self, beat: f64) -> Option<&ChordEvent> {
        let i = self.chords.partition_point(|c| c.start_beat <= beat);
        let c = self.chords.get(i.checked_sub(1)?)?;
        (beat < c.end_beat()).then_some(c)
    }

    /// `beat` より後に始まる最初の箱 (「次のコード」の表示用)。
    #[must_use]
    pub fn next_chord_after(&self, beat: f64) -> Option<&ChordEvent> {
        self.chords.get(self.chords.partition_point(|c| c.start_beat <= beat))
    }

    #[must_use]
    pub fn chord_by_id(&self, id: u32) -> Option<&ChordEvent> {
        self.chords.iter().find(|c| c.id == id)
    }

    /// `[a, b)` にかかる箱を削る (完全に入る箱は消え、またぐ箱は範囲の外側だけ残り、
    /// 範囲を内側に含む箱は 2 つに割れて右側が新 id を得る)。
    fn carve_chords(&mut self, a: f64, b: f64) {
        if b <= a {
            return;
        }
        let mut tails: Vec<ChordEvent> = Vec::new();
        for c in &mut self.chords {
            let (s, e) = (c.start_beat, c.end_beat());
            if e <= a || s >= b {
                continue;
            }
            if s < a && e > b {
                // 範囲を内側に含む: 左を残し、右を新しい箱にする (id は後で振る)。
                tails.push(ChordEvent { id: 0, start_beat: b, len_beats: e - b, ..c.clone() });
                c.len_beats = a - s;
            } else if s < a {
                c.len_beats = a - s;
            } else if e > b {
                c.start_beat = b;
                c.len_beats = e - b;
            } else {
                c.len_beats = 0.0;
            }
        }
        for mut t in tails {
            t.id = self.alloc_chord_id();
            self.chords.push(t);
        }
        self.chords.retain(|c| c.len_beats > MIN_CHORD_LEN_BEATS);
    }

    /// 箱を置く (上書き規則)。置いた箱の id を返す (`id == 0` なら採番する)。
    /// 値域が不正 (非有限 / 長さ 0 以下) なら何もせず `None`。
    pub fn place_chord(&mut self, mut ev: ChordEvent) -> Option<u32> {
        if !ev.start_beat.is_finite() || !ev.len_beats.is_finite() {
            return None;
        }
        if ev.start_beat < 0.0 {
            ev.len_beats += ev.start_beat;
            ev.start_beat = 0.0;
        }
        if ev.len_beats <= MIN_CHORD_LEN_BEATS {
            return None;
        }
        if ev.id == 0 {
            ev.id = self.alloc_chord_id();
        }
        ev.chord = ev.chord.sanitized();
        let id = ev.id;
        self.chords.retain(|c| c.id != id);
        self.carve_chords(ev.start_beat, ev.end_beat());
        self.chords.push(ev);
        self.normalize_chords();
        Some(id)
    }

    /// 箱の列をまとめて置く: [`Self::place_chord`] を列の順に呼んだのと同じ結果 (上書き規則。列の後ろの箱ほど勝ち、
    /// 削られた箱の左側が元の id を残し、割れた右側は新しい id) を、正規化 1 回で出す (O(n log n)。
    /// `place_chord` を n 回呼ぶと 1 回ごとに全体を正規化するので O(n² log n))。id は全部振り直す。
    /// 置けた箱が 1 つでもあれば `true` (値域が不正な箱は `place_chord` と同じく飛ばす)。
    pub fn place_chords(&mut self, evs: impl IntoIterator<Item = ChordEvent>) -> bool {
        // 上塗りの順位 = 並びの位置 (既存の箱は互いに重ならないので先頭に並べるだけでよい)。
        let mut items: Vec<ChordEvent> = std::mem::take(&mut self.chords);
        let existing = items.len();
        for mut ev in evs {
            if !ev.start_beat.is_finite() || !ev.len_beats.is_finite() {
                continue;
            }
            if ev.start_beat < 0.0 {
                ev.len_beats += ev.start_beat;
                ev.start_beat = 0.0;
            }
            if ev.len_beats <= MIN_CHORD_LEN_BEATS {
                continue;
            }
            ev.id = self.alloc_chord_id();
            ev.chord = ev.chord.sanitized();
            items.push(ev);
        }
        if items.len() == existing {
            self.chords = items;
            return false;
        }
        // 境界を掃いて、各区間をそこにかかる箱のうち順位が最大のものに塗る (ヒープは遅延削除)。
        let mut edges: Vec<(f64, usize)> = Vec::with_capacity(items.len() * 2);
        for (i, c) in items.iter().enumerate() {
            edges.push((c.start_beat, i));
            edges.push((c.end_beat(), i));
        }
        edges.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut open = vec![false; items.len()];
        let mut heap = std::collections::BinaryHeap::with_capacity(items.len());
        let mut segs: Vec<(usize, f64, f64)> = Vec::new();
        let mut k = 0;
        while k < edges.len() {
            let x = edges[k].0;
            while k < edges.len() && edges[k].0 == x {
                let i = edges[k].1;
                open[i] = !open[i];
                if open[i] {
                    heap.push(i);
                }
                k += 1;
            }
            while heap.peek().is_some_and(|&i| !open[i]) {
                heap.pop();
            }
            let (Some(&owner), Some(&(next, _))) = (heap.peek(), edges.get(k)) else { continue };
            match segs.last_mut() {
                Some(last) if last.0 == owner && last.2 == x => last.2 = next,
                _ => segs.push((owner, x, next)),
            }
        }
        // 箱ごとに最初の片が元の id を、2 つ目以降 (上に置いた箱で割れた右側) が新しい id を持つ。
        let mut used = vec![false; items.len()];
        let mut out = Vec::with_capacity(segs.len());
        for (owner, a, b) in segs {
            let src = &items[owner];
            let id = if std::mem::replace(&mut used[owner], true) { self.alloc_chord_id() } else { src.id };
            // 削られなかった箱は元の長さのまま (端の引き算で 1 ulp ずれない)。
            let len = if a == src.start_beat && b == src.end_beat() { src.len_beats } else { b - a };
            out.push(ChordEvent { id, start_beat: a, len_beats: len, ..src.clone() });
        }
        self.chords = out;
        self.normalize_chords();
        true
    }

    /// 選んだ箱をまとめて `delta` 拍ずらす (相対位置を保つ)。左端が 0 を割らないよう `delta` を詰める。
    /// 動かした箱どうしは削り合わず、動かなかった箱だけが上書き規則で削られる。動いたら `true`。
    pub fn move_chords(&mut self, ids: &[u32], delta: f64) -> bool {
        if !delta.is_finite() {
            return false;
        }
        let min_start = self
            .chords
            .iter()
            .filter(|c| ids.contains(&c.id))
            .map(|c| c.start_beat)
            .fold(f64::INFINITY, f64::min);
        if !min_start.is_finite() {
            return false;
        }
        let delta = delta.max(-min_start);
        if delta == 0.0 {
            return false;
        }
        let mut moved: Vec<ChordEvent> = Vec::new();
        self.chords.retain(|c| {
            if ids.contains(&c.id) {
                moved.push(ChordEvent { start_beat: c.start_beat + delta, ..c.clone() });
                false
            } else {
                true
            }
        });
        for m in &moved {
            self.carve_chords(m.start_beat, m.end_beat());
        }
        self.chords.extend(moved);
        self.normalize_chords();
        true
    }

    /// 箱 1 つの範囲を `[start, start + len)` に変える (端のドラッグ)。かかった隣の箱は削られる。
    pub fn resize_chord(&mut self, id: u32, start: f64, len: f64) -> bool {
        let Some(old) = self.chord_by_id(id).cloned() else { return false };
        self.place_chord(ChordEvent { start_beat: start, len_beats: len, ..old }).is_some()
    }

    /// 選んだ箱を `delta` 拍ずらした位置に複製する。複製の新 id を元と同じ順で返す。
    pub fn duplicate_chords(&mut self, ids: &[u32], delta: f64) -> Vec<u32> {
        if !delta.is_finite() {
            return Vec::new();
        }
        let copies: Vec<ChordEvent> = self
            .chords
            .iter()
            .filter(|c| ids.contains(&c.id))
            .map(|c| ChordEvent { id: 0, start_beat: c.start_beat + delta, ..c.clone() })
            .collect();
        let mut out = Vec::with_capacity(copies.len());
        let mut placed: Vec<ChordEvent> = Vec::with_capacity(copies.len());
        for mut c in copies {
            c.id = self.alloc_chord_id();
            out.push(c.id);
            placed.push(c);
        }
        for p in &placed {
            self.carve_chords(p.start_beat, p.end_beat());
        }
        self.chords.extend(placed);
        self.normalize_chords();
        out
    }

    /// 選んだ箱を消す。消したら `true`。
    pub fn delete_chords(&mut self, ids: &[u32]) -> bool {
        let before = self.chords.len();
        self.chords.retain(|c| !ids.contains(&c.id));
        self.chords.len() != before
    }

    /// 選んだ箱のコードを差し替える。変わったら `true`。
    pub fn set_chord(&mut self, ids: &[u32], chord: Chord) -> bool {
        let chord = chord.sanitized();
        let mut changed = false;
        for c in self.chords.iter_mut().filter(|c| ids.contains(&c.id)) {
            if c.chord != chord {
                c.chord = chord;
                changed = true;
            }
        }
        changed
    }

    /// 選んだ箱 (空なら全部) のコードを `semitones` 移調する。変わったら `true`。
    pub fn transpose_chords(&mut self, ids: &[u32], semitones: i32) -> bool {
        if semitones.rem_euclid(12) == 0 {
            return false;
        }
        let mut changed = false;
        for c in self.chords.iter_mut().filter(|c| ids.is_empty() || ids.contains(&c.id)) {
            c.chord = c.chord.transposed(semitones);
            changed = true;
        }
        changed
    }

    /// 選んだ箱の色を変える (`None` = テーマの既定色)。変わったら `true`。
    pub fn set_chord_color(&mut self, ids: &[u32], color: Option<[f32; 3]>) -> bool {
        let mut changed = false;
        for c in self.chords.iter_mut().filter(|c| ids.contains(&c.id)) {
            if c.color != color {
                c.color = color;
                changed = true;
            }
        }
        changed
    }

    /// `beat` を厳密にまたぐ箱を 2 つに割る (右側が新 id)。時間操作の境界で、クリップの
    /// [`Song::split_clips_at`] と対で呼ぶ。
    pub fn split_chords_at(&mut self, beat: f64) {
        let Some(i) = self.chords.iter().position(|c| c.start_beat < beat && beat < c.end_beat()) else {
            return;
        };
        let end = self.chords[i].end_beat();
        self.chords[i].len_beats = beat - self.chords[i].start_beat;
        let id = self.alloc_chord_id();
        let right = ChordEvent { id, start_beat: beat, len_beats: end - beat, ..self.chords[i].clone() };
        self.chords.insert(i + 1, right);
    }

    /// `[a, b)` にかかる箱を範囲に詰めた写し (位置は `a` からの相対、`id` は 0)。Song は変えない。
    #[must_use]
    pub fn chords_cropped(&self, a: f64, b: f64) -> Vec<ChordEvent> {
        self.chords
            .iter()
            .filter_map(|c| {
                let (s, e) = (c.start_beat.max(a), c.end_beat().min(b));
                (e > s + MIN_CHORD_LEN_BEATS)
                    .then(|| ChordEvent { id: 0, start_beat: s - a, len_beats: e - s, ..c.clone() })
            })
            .collect()
    }

    /// 写し (`a` からの相対位置) を `at` 基準で置く。**置く範囲は空いている前提** (時間操作が ripple で
    /// 空けた直後に呼ぶ) なので削らず、id だけ採番して足す。呼び出し側が `normalize_chords` を通す。
    pub(crate) fn push_chords_at(&mut self, at: f64, chords: &[ChordEvent]) {
        for c in chords {
            let id = self.alloc_chord_id();
            self.chords.push(ChordEvent { id, start_beat: at + c.start_beat, ..c.clone() });
        }
    }

    /// トラックの**実効の**追従モード: 曲全体のスイッチ ([`Song::chord_follow_enabled`]) が切れていれば
    /// `Off`。評価の口 (`crate::chord_follow`) と表示 (ヘッダの印) がこれを通す。
    #[must_use]
    pub fn track_chord_follow(&self, track_id: u32) -> ChordFollow {
        if !self.chord_follow_enabled {
            return ChordFollow::Off;
        }
        self.track_by_id(track_id).map_or(ChordFollow::Off, |t| t.follow_chords)
    }

    /// どれかのトラックの追従で鳴る音が変わりうるか (箱があり、スイッチが入り、追従するトラックがある)。
    /// engine が「追従の評価を丸ごと飛ばせるか」を 1 回で決めるための述語。
    #[must_use]
    pub fn chord_follow_can_apply(&self) -> bool {
        self.chord_follow_enabled && !self.chords.is_empty() && self.tracks.iter().any(|t| !t.follow_chords.is_off())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(start: f64, len: f64, root: u8) -> ChordEvent {
        ChordEvent { id: 0, start_beat: start, len_beats: len, chord: Chord::new(root, ChordKind::Major), color: None }
    }

    /// `(start, len, root)` の列 (位置順)。
    fn layout(song: &Song) -> Vec<(f64, f64, u8)> {
        song.chords.iter().map(|c| (c.start_beat, c.len_beats, c.chord.root)).collect()
    }

    fn song_with(chords: &[(f64, f64, u8)]) -> Song {
        let mut song = Song::default();
        for (s, l, r) in chords {
            song.place_chord(ev(*s, *l, *r));
        }
        song
    }

    /// 後から置いた箱が勝つ: 覆われた箱は消え、またがれた箱は削られ、内側に置かれた箱は 2 つに割れる。
    #[test]
    fn placing_a_chord_overwrites_what_it_covers() {
        let mut song = song_with(&[(0.0, 4.0, 0), (4.0, 4.0, 5), (8.0, 8.0, 7)]);
        // 拍 2-10 に置く: C は 0-2 に削られ、F は消え、G は 10-16 に削られる。
        song.place_chord(ev(2.0, 8.0, 9));
        assert_eq!(layout(&song), vec![(0.0, 2.0, 0), (2.0, 8.0, 9), (10.0, 6.0, 7)]);
        // G (10-16) の内側 12-13 に置く: G が 10-12 と 13-16 に割れる。
        song.place_chord(ev(12.0, 1.0, 2));
        assert_eq!(
            layout(&song),
            vec![(0.0, 2.0, 0), (2.0, 8.0, 9), (10.0, 2.0, 7), (12.0, 1.0, 2), (13.0, 3.0, 7)]
        );
        // id は全部別 (割れた右側は新 id)。
        let mut ids: Vec<u32> = song.chords.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), song.chords.len());
        // 引く: 箱の中、箱の端 (終端は含まない)、次の箱。
        assert_eq!(song.chord_at(1.0).map(|c| c.chord.root), Some(0));
        assert_eq!(song.chord_at(2.0).map(|c| c.chord.root), Some(9));
        assert_eq!(song.chord_at(16.0), None);
        assert_eq!(song.next_chord_after(12.5).map(|c| c.chord.root), Some(7));
    }

    /// まとめて動かすと相対位置を保ち、動かなかった箱だけが削られる。0 より左へは行かない。
    #[test]
    fn moving_chords_keeps_their_spacing() {
        let mut song = song_with(&[(0.0, 4.0, 0), (4.0, 4.0, 5), (8.0, 4.0, 7)]);
        let ids: Vec<u32> = song.chords.iter().take(2).map(|c| c.id).collect();
        assert!(song.move_chords(&ids, 6.0));
        // C → 6-10、F → 10-14。動かなかった G (8-12) は両方に覆われて消える。
        assert_eq!(layout(&song), vec![(6.0, 4.0, 0), (10.0, 4.0, 5)]);
        assert!(song.move_chords(&ids, -100.0));
        assert_eq!(layout(&song), vec![(0.0, 4.0, 0), (4.0, 4.0, 5)]);
    }

    /// セクションを動かすと、範囲内の箱が (境界で割られて) id を保ったまま一緒に動く。
    #[test]
    fn section_move_carries_the_chords_in_its_range() {
        let mut song = song_with(&[(0.0, 6.0, 0), (6.0, 2.0, 5), (8.0, 8.0, 7)]);
        let f_id = song.chord_at(6.0).map(|c| c.id).unwrap();
        let sid = song.alloc_section_id();
        song.sections.push(Section { id: sid, name: "A".into(), color: [0.5; 3], start_beat: 4.0, len_beats: 4.0 });
        song.length_beats = 16.0;
        // 帯 4-8 を曲の先頭へ。中身 (C の後半 4-6 と F 6-8) が 0-4 へ、C の前半 0-4 は 4-8 へ流れる。
        assert!(!song.move_section(sid, 0.0).is_empty());
        assert_eq!(layout(&song), vec![(0.0, 2.0, 0), (2.0, 2.0, 5), (4.0, 4.0, 0), (8.0, 8.0, 7)]);
        assert_eq!(song.chord_at(2.0).map(|c| c.id), Some(f_id));
    }

    /// 時間の削除 / 複製が箱を同伴する (削除は範囲ぶん詰まり、複製は範囲に詰めた写しが直後に入る)。
    #[test]
    fn time_range_edits_carry_chords() {
        let mut song = song_with(&[(0.0, 4.0, 0), (4.0, 4.0, 5), (8.0, 4.0, 7)]);
        song.length_beats = 12.0;
        // 2-6 を削除: C は 0-2、F は 6-8 だった部分が 2-4 へ、G は 4-8 へ。
        assert!(song.delete_time_range(2.0, 6.0).is_some());
        assert_eq!(layout(&song), vec![(0.0, 2.0, 0), (2.0, 2.0, 5), (4.0, 4.0, 7)]);
        // 0-4 を直後に複製: C / F の並びがもう 1 回入り、G は 8-12 へ押される。
        assert!(song.duplicate_time_range(0.0, 4.0).is_some());
        assert_eq!(
            layout(&song),
            vec![(0.0, 2.0, 0), (2.0, 2.0, 5), (4.0, 2.0, 0), (6.0, 2.0, 5), (8.0, 4.0, 7)]
        );
    }

    /// 読み込みの正規化は冪等で、壊れた値 (重なり / 負の位置 / 重複 id / 範囲外のルート) を直す。
    #[test]
    fn normalize_repairs_broken_chords_and_is_idempotent() {
        let chords = vec![
            ChordEvent { id: 3, start_beat: 4.0, len_beats: 4.0, chord: Chord::new(0, ChordKind::Major), color: None },
            ChordEvent { id: 3, start_beat: 2.0, len_beats: 4.0, chord: Chord { root: 14, ..Chord::default() }, color: None },
            ChordEvent { id: 0, start_beat: -1.0, len_beats: 2.0, chord: Chord::default(), color: None },
            ChordEvent { id: 9, start_beat: f64::NAN, len_beats: 1.0, chord: Chord::default(), color: None },
        ];
        let mut song = Song { chords, ..Song::default() };
        song.normalize_chords();
        assert_eq!(layout(&song), vec![(0.0, 1.0, 0), (2.0, 4.0, 2), (6.0, 2.0, 0)]);
        let ids: std::collections::HashSet<u32> = song.chords.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 3);
        assert!(!ids.contains(&0));
        let once = song.clone();
        song.normalize_chords();
        assert_eq!(song, once);
    }

    /// まとめて置く (`place_chords`) と、`place_chord` を列の順に呼んだのと同じ並びになる (後ろほど勝つ / 割れる /
    /// 覆われて消える / 不正な箱は飛ばす)。削られなかった既存の箱は元の id と値のまま。
    #[test]
    fn placing_many_matches_placing_one_by_one() {
        let base = song_with(&[(0.0, 4.0, 0), (4.0, 4.0, 5), (8.0, 8.0, 7), (20.0, 2.0, 9)]);
        // 疑似乱数 (LCG) の列 + 端のケース (既存の内側 / 既存を覆う / 互いに重なる / 不正値)。
        let mut seed = 12345u64;
        // 0..n の整数を半拍単位にする (2 進で正確な値なので、1 つずつ置いた結果とビット単位で比べられる)。
        let mut half_beats = |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            f64::from(u32::try_from((seed >> 33) % n).unwrap_or(0)) / 2.0
        };
        let mut incoming: Vec<ChordEvent> =
            (0..40u8).map(|i| ev(half_beats(24), half_beats(7), i % 12)).collect();
        incoming.extend([ev(1.0, 1.0, 2), ev(7.0, 10.0, 3), ev(-1.0, 2.0, 4), ev(3.0, 0.0, 5), ev(f64::NAN, 1.0, 6)]);
        let mut one_by_one = base.clone();
        for c in incoming.clone() {
            one_by_one.place_chord(c);
        }
        let mut batch = base.clone();
        assert!(batch.place_chords(incoming));
        assert_eq!(layout(&batch), layout(&one_by_one));
        let ids: std::collections::HashSet<u32> = batch.chords.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), batch.chords.len(), "id は重ならない");

        // 何もかからない既存の箱 (拍 20) は id も値もそのまま。
        let mut far = base.clone();
        assert!(far.place_chords([ev(30.0, 2.0, 1)]));
        let keep = |s: &Song| s.chords.iter().find(|c| c.start_beat == 20.0).cloned();
        assert_eq!(keep(&far), keep(&base));
        assert!(!far.place_chords([ev(3.0, 0.0, 5)]), "置ける箱が無ければ false");
    }
}
