//! 素材が **書かれたときのコード** の記録 (`Clip::chord_basis`、r.md #159) の書き込み・引き継ぎ・正規化。
//!
//! 型 ([`ClipChordBasis`]) は wire を渡るので `chords.rs` に置き、ここは wire に載らないロジックだけを持つ
//! (`common/build.rs` の `WIRE_SOURCES` 対象外)。評価 (記録から鳴る音を決める式) は `crate::chord_follow`。
//!
//! ## 規則 (確定仕様: 2026-11-23 ユーザー確定、`docs/plan_chord_track.md` §1 Q8)
//!
//! コードの帯にコードを **置いた時点** のコードがメロディーの基準で、メロディーが動くのは後からそのコードを
//! **書き換えた** ときだけ。和音のパートは今どおり、自分の和音から帯のコードへ合わせる (`crate::chord_follow`)。
//!
//! - **記録を書くのは書き換えだけ** ([`Song::set_chord`] / [`Song::transpose_chords`])。書き換える箱の下にある
//!   素材のうち、**まだ記録の無いもの** に、書き換える **前** のコードを書く。同じ `edit_song` の中なので undo は 1 段。
//!   - 書くのは追従するトラック (実効のモードが Off 以外) のアレンジのクリップだけ。Off の間に書き換えた分は、
//!     後で ON にしても動かない (ON にした時点が基準)。曲全体のスイッチも同じ。
//!   - MIDI: ノートの頭 (`Clip::content_to_song_beat`) が書き換える箱にあるもの (ミュートしたノート / 窓の外へ
//!     隠したノートも書く — 後で出したときに、そこだけ動かないまま残らない)。
//!   - オーディオ: 見えている event ∩ 箱の範囲を、波形と再生の写像 (`crate::audio_render::event_wave_spans`) で
//!     音源フレームの範囲へ写し、その take (`AudioEvent::take_key`) で記録の無い部分にだけ書く。
//! - **記録は素材の事実** — 「この素材は R に対して書かれた」。箱には縛らない: 記録のある素材は、どの箱の下へ
//!   動かしても / 写しても「R → 今のコード」で鳴る (参照製品も基準を素材の側に持つ)。記録の無い素材は、どの箱の
//!   下でも書いた音のまま。だから箱を置く / 動かす / 伸ばす / 割る / 時間操作で箱が割れても、記録は触らない。
//! - 書き換えを重ねても記録は最初のコードのまま (C → Dm → Em なら C → Em へ直接写す。差は積み重ならない)。
//!   元のコードへ書き戻せば動かない。
//! - 置いている最中の箱 (ダブルクリックで作ってポップアップでコードを決める) のコードを決めるのは書き換えではない
//!   (`ChordTrackEvent::Recast` → [`Song::recast_chords`]、記録を書かない)。
//! - **引き継ぎ**: クリップの分割 / 複製 / コピーは `Clip` ごと運ぶ (note id と take が保たれる)。ノートを割ると
//!   後ろの片が新しい id になるので、元の id の記録を写す ([`Song::copy_note_basis`])。
//! - **鳴る音で確定** したノートの記録は消す ([`Song::forget_note_basis`]、確定した音 = 今のコードに対して書いた音)。
//! - 読み込みと保存の前に [`Song::normalize_chord_basis`] で並びと値域を揃え、もう無いノート / take の記録を捨てる。
//!   ランチャーのセルの記録は捨てない (セルへ動かしたクリップをアレンジへ戻すと、元どおり鳴る)。

use super::*;
use crate::audio_render::{TempoMap, WaveSpan, event_wave_spans};

/// 書き換える箱 1 つ (書き換える前の値)。
#[derive(Debug, Clone, Copy)]
struct Rewritten {
    id: u32,
    start: f64,
    end: f64,
    chord: Chord,
}

/// コードの箱 1 つの下での、オーディオ event の基準 (engine の compile が箱ごとに 1 回決める。
/// [`Song::audio_event_bases`])。位置は曲の拍。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioBoxBasis {
    pub start_beat: f64,
    pub end_beat: f64,
    /// その範囲でいちばん長く鳴っている、和音を弾いている区間のコード (`Song.audio_chords`)。
    pub own: Option<Chord>,
    /// その範囲でいちばん長い記録 (`Clip::chord_basis.audio`)。
    pub placed: Option<Chord>,
}

impl AudioBoxBasis {
    #[must_use]
    pub fn basis(&self) -> crate::chord_follow::Basis<'_> {
        crate::chord_follow::Basis { own: self.own.as_ref(), placed: self.placed.as_ref() }
    }
}

/// 拍の範囲 `[b0, b1)` (span と同じ event-local 拍) が読む音源フレームの範囲。
fn span_frames(s: &WaveSpan, b0: f64, b1: f64) -> Option<(u64, u64)> {
    let (b0, b1) = (b0.max(s.start_beat), b1.min(s.end_beat));
    let beats = s.end_beat - s.start_beat;
    if b1 <= b0 || beats <= 0.0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let frames = s.source_end.saturating_sub(s.source_start) as f64;
    let (fa, fb) = ((b0 - s.start_beat) / beats, (b1 - s.start_beat) / beats);
    let (lo, hi) = if s.reversed { (frames * (1.0 - fb), frames * (1.0 - fa)) } else { (frames * fa, frames * fb) };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (f0, f1) = (
        s.source_start.saturating_add(lo.floor().max(0.0) as u64),
        s.source_start.saturating_add(hi.ceil().max(0.0) as u64).min(s.source_end),
    );
    (f1 > f0).then_some((f0, f1))
}

impl Song {
    /// オーディオ event `event` (クリップ `clip` の content) の、曲の拍 `[a, b)` が読む音源フレームの範囲の列
    /// (見えている event とクリップの窓で切る)。音源が無ければ空。
    fn event_frames(&self, tempo: &TempoMap<'_>, clip: &Clip, event: &AudioEvent, a: f64, b: f64) -> Vec<(u64, u64)> {
        let Some(source) = self.media.audio_sources.get(&event.source_id) else { return Vec::new() };
        let visible = clip.content_to_song_beat(event.event_start_in_clip_beats);
        let (w0, w1) = clip.song_window();
        let (a, b) = (a.max(visible).max(w0), b.min(visible + event.event_length_beats).min(w1));
        if b.partial_cmp(&a) != Some(std::cmp::Ordering::Greater) {
            return Vec::new();
        }
        let mut spans = Vec::new();
        event_wave_spans(event, source.sample_rate, tempo, visible, &mut spans);
        spans.iter().filter_map(|s| span_frames(s, a - visible, b - visible)).collect()
    }

    /// 書き換える箱 `boxes` の下にある、記録の無い素材へ書き換える前のコードを記録する (module doc の規則)。
    fn remember_chord_basis(&mut self, boxes: &[Rewritten]) {
        if boxes.is_empty() || !self.chord_follow_enabled {
            return;
        }
        let tempo = TempoMap::from_song(self);
        // (track 位置, clip 位置, 足す記録)。先に全部決めてから書く (Song を読みながら書かない)。
        let mut adds: Vec<(usize, usize, ClipChordBasis)> = Vec::new();
        for (ti, track) in self.tracks.iter().enumerate() {
            // 実効のモード (`Song::track_chord_follow`) を手元の値で (id から引き直すと全トラックで O(T²))。
            if track.follow_chords.is_off() {
                continue;
            }
            for (ci, clip) in track.clips.iter().enumerate() {
                let add = self.records_to_add(&tempo, boxes, clip);
                if !add.is_empty() {
                    adds.push((ti, ci, add));
                }
            }
        }
        for (ti, ci, add) in adds {
            let basis = &mut self.tracks[ti].clips[ci].chord_basis;
            basis.notes.extend(add.notes);
            basis.audio.extend(add.audio);
            *basis = std::mem::take(basis).normalized();
        }
    }

    /// クリップ `clip` の素材のうち、書き換える箱 `boxes` の下にあって記録の無いものへ足す記録 (書き換える前のコード)。
    fn records_to_add(&self, tempo: &TempoMap<'_>, boxes: &[Rewritten], clip: &Clip) -> ClipChordBasis {
        match self.clip_contents.get(&clip.content_id) {
            Some(ClipContent::Midi(m)) => ClipChordBasis { notes: self.note_records_to_add(boxes, clip, &m.notes), audio: Vec::new() },
            Some(ClipContent::Audio(content)) => ClipChordBasis {
                notes: Vec::new(),
                audio: content.events.iter().flat_map(|e| self.audio_records_to_add(tempo, boxes, clip, e)).collect(),
            },
            _ => ClipChordBasis::default(),
        }
    }

    /// [`Self::records_to_add`] の MIDI: ノートの頭が書き換える箱にあるもの (再生と同じ `chord_at_onset` で箱を引く)。
    fn note_records_to_add(&self, boxes: &[Rewritten], clip: &Clip, notes: &[Note]) -> Vec<NoteChordBasis> {
        let rewritten_at = |beat: f64| {
            let id = self.chord_at_onset(beat)?.id;
            boxes.iter().find(|b| b.id == id)
        };
        notes
            .iter()
            .filter(|n| crate::chord_follow::note_basis_at(&clip.chord_basis.notes, n.id).is_none())
            .filter_map(|n| {
                let b = rewritten_at(clip.content_to_song_beat(n.start_beat))?;
                Some(NoteChordBasis { note_id: n.id, chord: b.chord })
            })
            .collect()
    }

    /// [`Self::records_to_add`] のオーディオ event 1 つ: 箱ごとの音源フレームの範囲のうち、その take で記録の無い部分。
    fn audio_records_to_add(&self, tempo: &TempoMap<'_>, boxes: &[Rewritten], clip: &Clip, event: &AudioEvent) -> Vec<AudioChordBasis> {
        let take = event.take_key();
        let mut out = Vec::new();
        for b in boxes {
            for (f0, f1) in self.event_frames(tempo, clip, event, b.start, b.end) {
                let pieces = uncovered(&clip.chord_basis.audio, take, f0, f1);
                out.extend(pieces.into_iter().map(|(s, e)| AudioChordBasis { take, start_frame: s, end_frame: e, chord: b.chord }));
            }
        }
        out
    }

    /// 書き換える前の箱の値 (`ids` が空なら全部、`changes` がコードを変えるものだけ)。
    fn rewritten_boxes(&self, ids: &[u32], changes: impl Fn(&Chord) -> bool) -> Vec<Rewritten> {
        self.chords
            .iter()
            .filter(|c| (ids.is_empty() || ids.contains(&c.id)) && changes(&c.chord))
            .map(|c| Rewritten { id: c.id, start: c.start_beat, end: c.end_beat(), chord: c.chord })
            .collect()
    }

    /// 選んだ箱のコードを `chord` に **書き換える** (記録を書く、module doc)。変わったら `true`。
    pub fn set_chord(&mut self, ids: &[u32], chord: Chord) -> bool {
        let chord = chord.sanitized();
        let boxes = self.rewritten_boxes(ids, |c| *c != chord);
        if ids.is_empty() || boxes.is_empty() {
            return false;
        }
        self.remember_chord_basis(&boxes);
        self.recast_chords(ids, chord)
    }

    /// 選んだ箱 (空なら全部) のコードを `semitones` 移調する (書き換え = 記録を書く)。変わったら `true`。
    pub fn transpose_chords(&mut self, ids: &[u32], semitones: i32) -> bool {
        if semitones.rem_euclid(12) == 0 {
            return false;
        }
        let boxes = self.rewritten_boxes(ids, |_| true);
        if boxes.is_empty() {
            return false;
        }
        self.remember_chord_basis(&boxes);
        for c in self.chords.iter_mut().filter(|c| ids.is_empty() || ids.contains(&c.id)) {
            c.chord = c.chord.transposed(semitones);
        }
        true
    }

    /// 選んだ箱のコードを `chord` に **置き直す** — 置いている最中の箱のコードを決める (記録を書かない =
    /// メロディーは動かない、module doc)。変わったら `true`。
    pub fn recast_chords(&mut self, ids: &[u32], chord: Chord) -> bool {
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

    /// トラック `track_id` のクリップ `clip_id` (アレンジ / セル) から、ノート `note_ids` (空なら全部) の記録を消す
    /// (鳴る音で確定したノート)。消したら `true`。
    pub fn forget_note_basis(&mut self, track_id: u32, clip_id: u32, note_ids: &[u32]) -> bool {
        let Some(clip) = self.track_by_id_mut(track_id).and_then(|t| t.clip_by_id_mut(clip_id)) else { return false };
        let before = clip.chord_basis.notes.len();
        clip.chord_basis.notes.retain(|b| !note_ids.is_empty() && !note_ids.contains(&b.note_id));
        clip.chord_basis.notes.len() != before
    }

    /// content `content_id` を見る全クリップ (アレンジとセル) で、ノートの記録を `pairs` の `(元の id, 写す先の id)`
    /// へ写す (ノートを割った後ろの片 / 同じ content の中で複製したノート)。元に記録が無ければ何もしない。
    /// 共有している content を割る前に fork した場合は、張り替えた **後** で呼ぶ (張り替える前は fork 先を見る
    /// クリップが無い)。
    pub fn copy_note_basis(&mut self, content_id: ContentId, pairs: &[(u32, u32)]) {
        if pairs.is_empty() {
            return;
        }
        for track in &mut self.tracks {
            let clips = track.clips.iter_mut().chain(track.session_clips.iter_mut().map(|c| &mut c.clip));
            for clip in clips.filter(|c| c.content_id == content_id) {
                let basis = &mut clip.chord_basis.notes;
                let copies: Vec<NoteChordBasis> = pairs
                    .iter()
                    .filter_map(|&(from, to)| {
                        let chord = *crate::chord_follow::note_basis_at(basis, from)?;
                        Some(NoteChordBasis { note_id: to, chord })
                    })
                    .collect();
                for rec in copies {
                    match basis.binary_search_by_key(&rec.note_id, |b| b.note_id) {
                        Ok(i) => basis[i] = rec,
                        Err(i) => basis.insert(i, rec),
                    }
                }
            }
        }
    }

    /// 全クリップ (アレンジとセル) の記録の不変条件を回復する: 並び (note id 昇順 / `(take, start_frame)` 昇順)・
    /// 重複なし・同じ take の中で非交差・コードの値域、もう content に無いノート / take の記録を捨てる。
    /// 読み込みと保存の前に通す。idempotent。
    pub fn normalize_chord_basis(&mut self) {
        let contents = &self.clip_contents;
        for track in &mut self.tracks {
            let clips = track.clips.iter_mut().chain(track.session_clips.iter_mut().map(|c| &mut c.clip));
            for clip in clips {
                let basis = &mut clip.chord_basis;
                if basis.is_empty() {
                    continue;
                }
                match contents.get(&clip.content_id) {
                    Some(ClipContent::Midi(m)) => {
                        let ids: std::collections::HashSet<u32> = m.notes.iter().map(|n| n.id).collect();
                        basis.notes.retain(|b| ids.contains(&b.note_id));
                        basis.audio.clear();
                    }
                    Some(ClipContent::Audio(a)) => {
                        let takes: std::collections::HashSet<u32> = a.events.iter().map(AudioEvent::take_key).collect();
                        basis.audio.retain(|b| takes.contains(&b.take));
                        basis.notes.clear();
                    }
                    _ => *basis = ClipChordBasis::default(),
                }
                *basis = std::mem::take(basis).normalized();
            }
        }
    }

    /// クリップ `clip` のオーディオ event `event` の、箱ごとの基準 (engine の compile が 1 回だけ作る。RT は曲の拍で
    /// 引くだけ)。箱の下では基準を 1 つに決める — 減衰・休符・単音の窓で音程を跳ねさせない。`audio_chords` は
    /// 音源の検出結果、`tempo` は [`TempoMap::from_song`]。
    #[must_use]
    pub fn audio_event_bases(&self, tempo: &TempoMap<'_>, clip: &Clip, event: &AudioEvent) -> Vec<AudioBoxBasis> {
        let visible = clip.content_to_song_beat(event.event_start_in_clip_beats);
        let (w0, w1) = clip.song_window();
        let (a, b) = (visible.max(w0), (visible + event.event_length_beats).min(w1));
        if b.partial_cmp(&a) != Some(std::cmp::Ordering::Greater) {
            return Vec::new();
        }
        let first = self.chords.partition_point(|c| c.end_beat() <= a);
        self.chords[first..]
            .iter()
            .take_while(|c| c.start_beat < b)
            .map(|c| {
                let frames = self.event_frames(tempo, clip, event, c.start_beat, c.end_beat());
                let (own, placed) = self.frames_basis(clip, event, &frames);
                AudioBoxBasis { start_beat: c.start_beat.max(a), end_beat: c.end_beat().min(b), own, placed }
            })
            .filter(|basis| basis.own.is_some() || basis.placed.is_some())
            .collect()
    }

    /// 焼き込み (バウンス / オーディオの Glue) の写し: クリップ `keys` の素材が箱ごとに動いていた元のコード
    /// (`chord_follow::effective_basis` — 書かれたときのコード、和音のパートは自分の和音。動いていなかった箱は写さない)
    /// を、焼いた音源のクリップ `baked` の event `event` の音源フレームへ写した記録。焼いた音は書いた音なので、写した記録
    /// から動かすと元の素材と同じ量だけ動く (焼く前と後で鳴りが変わらない)。箱の下で鳴っていた基準が複数あれば、
    /// 鳴る長さがいちばん長いもの。追従しないトラックの素材は写さない (記録を書かないのと同じ)。元を置き換える前に呼ぶ。
    #[must_use]
    pub fn baked_chord_basis(&self, keys: &[ClipKey], baked: &Clip, event: &AudioEvent) -> ClipChordBasis {
        let mut out = ClipChordBasis::default();
        let sources: Vec<(u32, &Clip)> = keys
            .iter()
            .filter(|k| !self.track_chord_follow(k.track_id).is_off())
            .filter_map(|k| Some((k.track_id, self.clip_by_key(*k)?)))
            .collect();
        if sources.is_empty() {
            return out;
        }
        let tempo = TempoMap::from_song(self);
        let (w0, w1) = baked.song_window();
        let first = self.chords.partition_point(|c| c.end_beat() <= w0);
        for c in self.chords[first..].iter().take_while(|c| c.start_beat < w1) {
            let mut weights: Vec<(Chord, f64)> = Vec::new();
            for (track_id, src) in &sources {
                self.add_moved_basis(&tempo, *track_id, src, c, &mut weights);
            }
            let Some(chord) = weights.into_iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|(chord, _)| chord) else {
                continue;
            };
            for (f0, f1) in self.event_frames(&tempo, baked, event, c.start_beat, c.end_beat()) {
                out.audio.push(AudioChordBasis { take: event.take_key(), start_frame: f0, end_frame: f1, chord });
            }
        }
        out.normalized()
    }

    /// クリップ `clip` の素材が箱 `c` の下で動いていた元のコードを、鳴る長さ (MIDI はノートの長さ、オーディオは音源
    /// フレーム) で `weights` に足す。動いていない素材は足さない。
    fn add_moved_basis(&self, tempo: &TempoMap<'_>, track_id: u32, clip: &Clip, c: &ChordEvent, weights: &mut Vec<(Chord, f64)>) {
        let mut add = |chord: Chord, w: f64| match weights.iter_mut().find(|(k, _)| *k == chord) {
            Some((_, sum)) => *sum += w,
            None => weights.push((chord, w)),
        };
        match self.clip_contents.get(&clip.content_id) {
            Some(ClipContent::Midi(m)) => {
                let follow = crate::chord_follow::TrackChordFollow::of(self, track_id);
                let own = crate::chord_follow::note_own_chords(&m.notes);
                for n in m.notes.iter().filter(|n| !n.muted && clip.window_has_onset(n.start_beat)) {
                    let beat = clip.content_to_song_beat(n.start_beat);
                    let src = crate::chord_follow::NoteSource { own: &own, basis: &clip.chord_basis.notes, note_id: n.id, start_beat: n.start_beat };
                    let Some((ev, basis)) = follow.note_basis(beat, &src).filter(|(ev, _)| ev.id == c.id) else { continue };
                    if let Some(chord) = crate::chord_follow::effective_basis(basis, &ev.chord) {
                        add(*chord, n.duration_beats);
                    }
                }
            }
            Some(ClipContent::Audio(a)) => {
                for event in &a.events {
                    let frames = self.event_frames(tempo, clip, event, c.start_beat, c.end_beat());
                    let (own, placed) = self.frames_basis(clip, event, &frames);
                    let basis = crate::chord_follow::Basis { own: own.as_ref(), placed: placed.as_ref() };
                    if let Some(chord) = crate::chord_follow::effective_basis(basis, &c.chord) {
                        #[allow(clippy::cast_precision_loss)]
                        add(*chord, frames.iter().map(|(f0, f1)| (f1 - f0) as f64).sum());
                    }
                }
            }
            _ => {}
        }
    }

    /// 音源フレームの範囲 `frames` (event 1 つ・箱 1 つぶん) の基準: いちばん長く重なる、和音を弾いている区間のコードと、
    /// いちばん長く重なる記録のコード。
    fn frames_basis(&self, clip: &Clip, event: &AudioEvent, frames: &[(u64, u64)]) -> (Option<Chord>, Option<Chord>) {
        let spans = self.audio_chords.get(&event.source_id).map_or(&[][..], Vec::as_slice);
        let own = frames
            .iter()
            .filter_map(|&(f0, f1)| crate::chord_detect::audio_longest_own_in(spans, f0, f1))
            .max_by_key(|(_, overlap)| *overlap)
            .map(|(chord, _)| *chord);
        let take = event.take_key();
        let overlap = |r: &AudioChordBasis, (f0, f1): (u64, u64)| r.end_frame.min(f1).saturating_sub(r.start_frame.max(f0));
        let placed = clip
            .chord_basis
            .audio
            .iter()
            .filter(|r| r.take == take)
            .map(|r| (r.chord, frames.iter().map(|&f| overlap(r, f)).sum::<u64>()))
            .filter(|(_, o)| *o > 0)
            .max_by_key(|(_, o)| *o)
            .map(|(chord, _)| chord);
        (own, placed)
    }
}

impl ClipChordBasis {
    /// 並び (note id 昇順 / `(take, start_frame)` 昇順)・重複なし・同じ take の中で非交差・コードの値域を揃えたもの
    /// (外 = ファイル / clipboard から来た値。記録は二分探索で引くので並びが崩れると黙って外れる)。idempotent。
    #[must_use]
    pub fn normalized(mut self) -> Self {
        for b in &mut self.notes {
            b.chord = b.chord.sanitized();
        }
        self.notes.sort_by_key(|b| b.note_id);
        self.notes.dedup_by_key(|b| b.note_id);
        normalize_audio_basis(&mut self.audio);
        self
    }
}

/// `records` (同じクリップの記録) のうち take `take` で、`[f0, f1)` のまだ記録の無い部分。
fn uncovered(records: &[AudioChordBasis], take: u32, f0: u64, f1: u64) -> Vec<(u64, u64)> {
    let mut covered: Vec<(u64, u64)> = records
        .iter()
        .filter(|r| r.take == take && r.end_frame > f0 && r.start_frame < f1)
        .map(|r| (r.start_frame, r.end_frame))
        .collect();
    covered.sort_unstable();
    let mut out = Vec::new();
    let mut at = f0;
    for (s, e) in covered {
        if s > at {
            out.push((at, s.min(f1)));
        }
        at = at.max(e);
    }
    if at < f1 {
        out.push((at, f1));
    }
    out
}

/// オーディオの記録の並び (`(take, start_frame)` 昇順) と、同じ take の中の非交差 (先に始まる方を残して後発の頭を
/// 削る)、隣り合う同じコードの結合、コードの値域。idempotent。
fn normalize_audio_basis(records: &mut Vec<AudioChordBasis>) {
    records.retain(|r| r.end_frame > r.start_frame);
    records.sort_by_key(|r| (r.take, r.start_frame));
    let mut out: Vec<AudioChordBasis> = Vec::with_capacity(records.len());
    for mut r in records.drain(..) {
        r.chord = r.chord.sanitized();
        if let Some(last) = out.last_mut().filter(|l| l.take == r.take) {
            r.start_frame = r.start_frame.max(last.end_frame);
            if r.end_frame <= r.start_frame {
                continue;
            }
            if last.end_frame == r.start_frame && last.chord == r.chord {
                last.end_frame = r.end_frame;
                continue;
            }
        }
        out.push(r);
    }
    *records = out;
}
