//! **分割の SSoT** — note / audio event / content を「切る位置の集合」で割る。
//!
//! 分割を伴う経路は切る位置の決め方だけが違い、片の作り方 (id / 歌詞 / fade / source の
//! 範囲) と「短すぎる片は隣の片にくっつける」規則 ([`split_boundaries`]) はここだけが持つ:
//!
//! - ピアノロールの `E` (カーソルの 1 点) / `Shift+E` (グリッド線) → [`MidiContent::split_notes`]
//! - オーディオエディタの `E` / `Shift+E` → [`AudioContent::split_events`]
//! - クリップの分割 (`E` / `Shift+E`) / セクションの境界 (`Song::split_clips_at`) /
//!   範囲操作の両端 (`handler::range_ops`) → [`Song::split_content_at_points`]
//!
//! クリップの分割で content も切るのは、窓 (クリップ) を割っただけでは「跨いだ note の後半」を
//! 後半の窓が鳴らせないから (再生側は「発音開始が窓内」の note しか鳴らさない)。 共有されて
//! いる content は先に fork するので linked clip は無傷。

use crate::model::{AudioContent, AudioEvent, ClipContent, ContentId, MidiContent, Note, Song, VideoEvent};

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
    /// 片ごとに source の範囲を**拍の比**で配り (逆再生は source の末尾側から)、切り口の
    /// 側の fade は 0 にする (外側の端の fade は端の片が継ぐ)。 先頭の片が元の `id`、
    /// 後ろの片は新しい `id`。 gain / pan / pitch / stretch / onset / warp marker は全片が継ぐ。
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
            let ev = self.events[i].clone();
            let (e0, len) = (ev.event_start_in_clip_beats, ev.event_length_beats);
            let kept = split_boundaries(e0, e0 + len, cuts(&ev), min_piece);
            if kept.is_empty() {
                continue;
            }
            let bounds: Vec<f64> =
                std::iter::once(e0).chain(kept).chain(std::iter::once(e0 + len)).collect();
            let span = ev.source_end_frames.saturating_sub(ev.source_start_frames);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
            let frame_at = |beat: f64| -> u64 {
                ((beat - e0) / len * span as f64).round().clamp(0.0, span as f64) as u64
            };
            let last = bounds.len() - 2;
            for (j, w) in bounds.windows(2).enumerate() {
                let (da, db) = (frame_at(w[0]), frame_at(w[1]));
                let mut piece = ev.clone();
                piece.event_start_in_clip_beats = w[0];
                piece.event_length_beats = w[1] - w[0];
                (piece.source_start_frames, piece.source_end_frames) = if ev.reversed {
                    // 逆再生: event の頭は source の末尾を読む。
                    (ev.source_end_frames.saturating_sub(db), ev.source_end_frames.saturating_sub(da))
                } else {
                    (ev.source_start_frames.saturating_add(da), ev.source_start_frames.saturating_add(db))
                };
                if j > 0 {
                    piece.fade_in_beats = 0.0;
                }
                if j < last {
                    piece.fade_out_beats = 0.0;
                }
                if j == 0 {
                    ids.push(ev.id);
                    self.events[i] = piece;
                } else {
                    piece.id = self.alloc_event_id();
                    ids.push(piece.id);
                    extra.push(piece);
                }
            }
        }
        if !extra.is_empty() {
            self.events.extend(extra);
            self.events.sort_by(|x, y| {
                x.event_start_in_clip_beats.total_cmp(&y.event_start_in_clip_beats)
            });
        }
        ids
    }
}

impl Song {
    /// content を **content-local 拍 `at`** で切る ([`Self::split_content_at_points`] の 1 点版)。
    pub fn split_content_at(&mut self, content_id: ContentId, at: f64) -> ContentId {
        self.split_content_at_points(content_id, &[at])
    }

    /// content を **content-local 拍の集合 `ats`** で切る。 どれかを跨ぐ note / event を割り、
    /// 切り口 (内側) の fade は 0 にする。 **位置は動かさない** — 窓モデルなので、切った 1 つの
    /// content の上に分割後のクリップの窓が並ぶ。 切り口は構造上の境界なので短い片も作る
    /// (`min_piece = 0`: 窓の境界ちょうどで切れていないと後ろの窓が鳴らせない)。
    ///
    /// content が複数の clip から共有されていれば**先に 1 回だけ fork する** (copy-on-write)
    /// ので、linked clip の中身は変わらない。 返り値は「切り終えた content の id」= 分割後の
    /// 全片が使う id。 跨ぐ要素が無ければ fork もせず `content_id` をそのまま返す。
    pub fn split_content_at_points(&mut self, content_id: ContentId, ats: &[f64]) -> ContentId {
        let crosses = self
            .clip_contents
            .get(&content_id)
            .is_some_and(|c| ats.iter().any(|&at| Self::content_crosses(c, at)));
        if !crosses {
            return content_id;
        }
        let target = if self.clip_content_refcount(content_id) > 1 {
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
    fn content_crosses(content: &ClipContent, at: f64) -> bool {
        let crosses = |start: f64, len: f64| start < at - EPS && start + len > at + EPS;
        match content {
            ClipContent::Midi(m) => {
                m.notes.iter().any(|n| crosses(n.start_beat, n.duration_beats))
            }
            ClipContent::Audio(a) => a
                .events
                .iter()
                .any(|e| crosses(e.event_start_in_clip_beats, e.event_length_beats)),
            ClipContent::Video(v) => v
                .events
                .iter()
                .any(|e| crosses(e.event_start_in_clip_beats, e.event_length_beats)),
            ClipContent::Image(i) => i
                .events
                .iter()
                .any(|e| crosses(e.event_start_in_clip_beats, e.event_length_beats)),
            ClipContent::Text(t) => t
                .events
                .iter()
                .any(|e| crosses(e.event_start_in_clip_beats, e.event_length_beats)),
            // automation point は幅を持たないので切る対象が無い (窓の外の point も
            // 補間には効くので、境界に point を挿す必要も無い)。
            ClipContent::Automation(_) => false,
        }
    }

    /// `ats` を跨ぐ要素を割る (位置は動かさない)。
    fn cut_content_at(content: &mut ClipContent, ats: &[f64]) {
        /// 時間軸 source を持たない overlay (image / text) 用。
        macro_rules! cut_overlay {
            ($events:expr, $at:expr) => {{
                let at = $at;
                let mut extra = Vec::new();
                for ev in $events.iter_mut() {
                    let e0 = ev.event_start_in_clip_beats;
                    let e1 = e0 + ev.event_length_beats;
                    if !(e0 < at - EPS && e1 > at + EPS) {
                        continue;
                    }
                    let mut tail = ev.clone();
                    tail.event_start_in_clip_beats = at;
                    tail.event_length_beats = e1 - at;
                    tail.fade_in_beats = 0.0;
                    extra.push(tail);
                    ev.event_length_beats = at - e0;
                    ev.fade_out_beats = 0.0;
                }
                $events.extend(extra);
                $events.sort_by(|a, b| {
                    a.event_start_in_clip_beats.total_cmp(&b.event_start_in_clip_beats)
                });
            }};
        }
        match content {
            ClipContent::Midi(m) => {
                m.split_notes(|_| ats.to_vec(), 0.0);
            }
            ClipContent::Audio(a) => {
                a.split_events(|_| ats.to_vec(), 0.0);
            }
            ClipContent::Video(v) => {
                for &at in ats {
                    Self::cut_video_at(&mut v.events, at);
                }
            }
            ClipContent::Image(i) => {
                for &at in ats {
                    cut_overlay!(i.events, at);
                }
            }
            ClipContent::Text(t) => {
                for &at in ats {
                    cut_overlay!(t.events, at);
                }
            }
            ClipContent::Automation(_) => {}
        }
    }

    /// video event を `at` で 2 つに割る (source の範囲は拍の比で配る)。
    fn cut_video_at(events: &mut Vec<VideoEvent>, at: f64) {
        let mut extra: Vec<VideoEvent> = Vec::new();
        for ev in events.iter_mut() {
            let e0 = ev.event_start_in_clip_beats;
            let e1 = e0 + ev.event_length_beats;
            if !(e0 < at - EPS && e1 > at + EPS) {
                continue;
            }
            let frac = (at - e0) / ev.event_length_beats;
            let span = ev.source_end_micros.saturating_sub(ev.source_start_micros);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
            let delta = (span as f64 * frac).round().max(0.0) as u64;
            let mid = ev.source_start_micros.saturating_add(delta);
            let mut tail = ev.clone();
            tail.event_start_in_clip_beats = at;
            tail.event_length_beats = e1 - at;
            tail.source_start_micros = mid.min(ev.source_end_micros);
            tail.fade_in_beats = 0.0;
            extra.push(tail);
            ev.event_length_beats = at - e0;
            ev.source_end_micros = mid.max(ev.source_start_micros);
            ev.fade_out_beats = 0.0;
        }
        events.extend(extra);
        events.sort_by(|x, y| x.event_start_in_clip_beats.total_cmp(&y.event_start_in_clip_beats));
    }
}
