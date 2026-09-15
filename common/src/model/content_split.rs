//! **分割の SSoT** — note / audio event / content を「切る位置の集合」で割る。
//!
//! 分割を伴う経路は切る位置の決め方だけが違い、片の作り方 (id / 歌詞 / 窓 / fade / 読み上げ) と
//! 「短すぎる片は隣の片にくっつける」規則 ([`split_boundaries`]) はここだけが持つ:
//!
//! - ピアノロールの `E` (カーソルの 1 点) / `Shift+E` (グリッド線) → [`MidiContent::split_notes`]
//! - オーディオエディタの `E` / `Shift+E` → [`AudioContent::split_events`]
//! - クリップの分割 (`E` / `Shift+E`) / セクションの境界 (`Song::split_clips_at`) /
//!   範囲操作の両端 (`handler::range_ops`) → [`Song::split_content_at_points`]
//!
//! **分割は切れ目を入れるだけ** — 分割直後の再生・書き出し・描画は分割前と同じ (r.md #132 残件)。
//! note は後ろの片を長音「ー」にし、時間軸を持つ event (audio / video / image / text) は元の event の
//! 窓を切り出す ([`super::event_piece`]: source の写像・fade のランプ・読み上げは切り口を跨いで続く)。
//!
//! クリップの分割で content も切るのは、窓 (クリップ) を割っただけでは「跨いだ note の後半」を
//! 後半の窓が鳴らせないから (再生側は「発音開始が窓内」の note しか鳴らさない)。 共有されて
//! いる MIDI content は先に fork するので linked clip は無傷 (時間軸を持つ event は切っても鳴り方が
//! 変わらないので fork しない、[`Song::split_content_at_points`])。

use crate::model::{
    AudioContent, AudioEvent, ClipContent, ContentId, MidiContent, Note, Song, TimedEvent, split_pieces,
};

/// 拍の同一視許容量。
const EPS: f64 = 1e-9;

/// 区間 `[start, end)` を `cuts` で割るときに**実際に使う切り口**を昇順で返す。
///
/// 切り口は順不同・重複可で、区間の外 / 端ちょうどは無視する。 どの片も `min_piece` 以上
/// (かつ長さ 0 を超える) になるように、短い片を作る切り口を捨てる = その片は隣の片に
/// くっつく (先頭の短い片は次の片へ、末尾の短い片は前の片へ、グリッドが `min_piece` より
/// 細かいときは後ろへ順に)。 空なら割らない。
#[must_use]
pub fn split_boundaries(
    start: f64,
    end: f64,
    cuts: impl IntoIterator<Item = f64>,
    min_piece: f64,
) -> Vec<f64> {
    let fits = |len: f64| len > EPS && len >= min_piece - EPS;
    let mut sorted: Vec<f64> = cuts.into_iter().filter(|c| c.is_finite()).collect();
    sorted.sort_by(f64::total_cmp);
    let mut kept = Vec::new();
    let mut last = start;
    for c in sorted {
        if fits(c - last) && fits(end - c) {
            kept.push(c);
            last = c;
        }
    }
    kept
}

impl MidiContent {
    /// ノートを切る。 `cuts(note)` がそのノートを切る位置 (content-local 拍、順不同・重複可・
    /// ノートの外は無視) を返し、空なら触らない。 `min_piece` より短い片は作らず隣の片に
    /// くっつける ([`split_boundaries`])。
    ///
    /// - 先頭の片が元の `id` と歌詞を持つ。 後ろの片は新しい `id` で、歌詞のあるノートなら
    ///   長音「ー」 ([`crate::voicevox::PROLONGED_SOUND_MARK`]、音節を歌い直さず伸ばす)、
    ///   歌詞の無いノートは無いまま。
    /// - pitch / velocity / muted は全片が継ぐ。 片同士は隣接するだけなので同じ音程の
    ///   重なりは作らない (重なり解消は要らない)。
    ///
    /// 返り値は実際に割ったノートの数。
    pub fn split_notes(&mut self, mut cuts: impl FnMut(&Note) -> Vec<f64>, min_piece: f64) -> usize {
        let mut pieces: Vec<Note> = Vec::new();
        let mut split = 0;
        for i in 0..self.notes.len() {
            let note = self.notes[i].clone();
            let end = note.start_beat + note.duration_beats;
            let kept = split_boundaries(note.start_beat, end, cuts(&note), min_piece);
            let Some(&first) = kept.first() else {
                continue;
            };
            split += 1;
            self.notes[i].duration_beats = first - note.start_beat;
            let tail_lyric = note
                .lyric
                .as_ref()
                .map(|_| crate::voicevox::PROLONGED_SOUND_MARK.to_string());
            for (j, &at) in kept.iter().enumerate() {
                let to = kept.get(j + 1).copied().unwrap_or(end);
                pieces.push(Note {
                    id: self.alloc_note_id(),
                    start_beat: at,
                    duration_beats: to - at,
                    lyric: tail_lyric.clone(),
                    ..note.clone()
                });
            }
        }
        if split > 0 {
            self.notes.extend(pieces);
            self.notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
        }
        split
    }
}

impl AudioContent {
    /// audio event を切る ([`MidiContent::split_notes`] の event 版、切り口の規則は同じ)。
    ///
    /// 片は元の event の窓 ([`super::event_piece`]): source の範囲・伸縮・onset・warp marker は全片が
    /// そのまま継ぎ、take の窓 (`take_head_beats` / `take_tail_beats`) と fade のランプだけが片ごとに
    /// 変わる。 先頭の片が元の `id`、後ろの片は新しい `id` で、全片が元の take (`take_id`) を継ぐ。
    ///
    /// 返り値は割った event の**全片の id** (event ごとに先頭の片から)。
    pub fn split_events(
        &mut self,
        mut cuts: impl FnMut(&AudioEvent) -> Vec<f64>,
        min_piece: f64,
    ) -> Vec<u32> {
        let mut extra: Vec<AudioEvent> = Vec::new();
        let mut ids = Vec::new();
        for i in 0..self.events.len() {
            let mut pieces = split_pieces(&self.events[i], cuts(&self.events[i]), min_piece).into_iter();
            let Some(head) = pieces.next() else {
                continue;
            };
            ids.push(head.id);
            let take = head.take_key();
            self.events[i] = head;
            for mut piece in pieces {
                piece.id = self.alloc_event_id();
                piece.take_id = take;
                ids.push(piece.id);
                extra.push(piece);
            }
        }
        if !extra.is_empty() {
            self.events.extend(extra);
            sort_by_start(&mut self.events);
        }
        ids
    }
}

/// 時間軸を持つ event 列を開始拍順に並べる (安定)。
fn sort_by_start<E: TimedEvent>(events: &mut [E]) {
    events.sort_by(|a, b| a.start().total_cmp(&b.start()));
}

/// id を持たない event 列 (video / image / text) を切り口 `ats` で割る。
fn split_timed<E: TimedEvent>(events: &mut Vec<E>, ats: &[f64]) {
    let mut out: Vec<E> = Vec::with_capacity(events.len());
    let mut split = false;
    for ev in events.drain(..) {
        let pieces = split_pieces(&ev, ats.iter().copied(), 0.0);
        if pieces.is_empty() {
            out.push(ev);
        } else {
            split = true;
            out.extend(pieces);
        }
    }
    if split {
        sort_by_start(&mut out);
    }
    *events = out;
}

impl Song {
    /// content を **content-local 拍 `at`** で切る ([`Self::split_content_at_points`] の 1 点版)。
    pub fn split_content_at(&mut self, content_id: ContentId, at: f64) -> ContentId {
        self.split_content_at_points(content_id, &[at])
    }

    /// content を **content-local 拍の集合 `ats`** で切る。 どれかを跨ぐ note / event を割る
    /// (片の作り方は [`MidiContent::split_notes`] / [`super::event_piece`])。 **位置は動かさない** —
    /// 窓モデルなので、切った 1 つの content の上に分割後のクリップの窓が並ぶ。 切り口は構造上の
    /// 境界なので短い片も作る (`min_piece = 0`: 窓の境界ちょうどで切れていないと後ろの窓が鳴らせない)。
    ///
    /// **切ると中身の意味が変わる content (MIDI) だけ**、複数の clip から共有されていれば先に 1 回だけ
    /// fork する (copy-on-write) ので、linked clip の鳴り方は変わらない — ノートを「あ」+「ー」に割ると
    /// 切り口で発音し直すので、共有したまま切ると linked clip まで変わる。 時間軸を持つ event (audio /
    /// video / image / text) の切り口は **切れ目を入れるだけ** ([`super::event_piece`]) で、共有している
    /// clip の再生・描画・読み上げは 1 sample も変わらないので fork しない (リンクを切らない。 audio は
    /// ARA の編集 = content と take ごとの audio modification も共有したまま続く)。 返り値は「切り終えた
    /// content の id」= 分割後の全片が使う id。 跨ぐ要素が無ければ `content_id` をそのまま返す。
    pub fn split_content_at_points(&mut self, content_id: ContentId, ats: &[f64]) -> ContentId {
        let Some(content) = self.clip_contents.get(&content_id) else {
            return content_id;
        };
        if !ats.iter().any(|&at| Self::content_crosses(content, at)) {
            return content_id;
        }
        let cut_changes_meaning = matches!(content, ClipContent::Midi(_));
        let target = if cut_changes_meaning && self.clip_content_refcount(content_id) > 1 {
            self.fork_content(content_id)
        } else {
            content_id
        };
        if let Some(content) = self.clip_contents.get_mut(&target) {
            Self::cut_content_at(content, ats);
        }
        target
    }

    /// `at` を厳密に跨ぐ要素があるか (`split_content_at_points` の早期 return 判定)。
    pub(super) fn content_crosses(content: &ClipContent, at: f64) -> bool {
        fn any_crosses<E: TimedEvent>(events: &[E], at: f64) -> bool {
            events.iter().any(|e| e.start() < at - EPS && e.start() + e.len_beats() > at + EPS)
        }
        match content {
            ClipContent::Midi(m) => m
                .notes
                .iter()
                .any(|n| n.start_beat < at - EPS && n.start_beat + n.duration_beats > at + EPS),
            ClipContent::Audio(a) => any_crosses(&a.events, at),
            ClipContent::Video(v) => any_crosses(&v.events, at),
            ClipContent::Image(i) => any_crosses(&i.events, at),
            ClipContent::Text(t) => any_crosses(&t.events, at),
            // automation point は幅を持たないので切る対象が無い (窓の外の point も
            // 補間には効くので、境界に point を挿す必要も無い)。
            ClipContent::Automation(_) => false,
        }
    }

    /// `ats` を跨ぐ要素を割る (位置は動かさない)。
    fn cut_content_at(content: &mut ClipContent, ats: &[f64]) {
        match content {
            ClipContent::Midi(m) => {
                m.split_notes(|_| ats.to_vec(), 0.0);
            }
            ClipContent::Audio(a) => {
                a.split_events(|_| ats.to_vec(), 0.0);
            }
            ClipContent::Video(v) => split_timed(&mut v.events, ats),
            ClipContent::Image(i) => split_timed(&mut i.events, ats),
            ClipContent::Text(t) => split_timed(&mut t.events, ats),
            ClipContent::Automation(_) => {}
        }
    }
}
