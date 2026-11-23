//! 素材の **置いた時点のコード** の記録 (`Clip::chord_basis`、r.md #159) の書き込み・引き継ぎ・正規化。
//!
//! 型 ([`ClipChordBasis`]) は wire を渡るので `chords.rs` に置き、ここは wire に載らないロジックだけを持つ
//! (`common/build.rs` の `WIRE_SOURCES` 対象外)。評価 (記録から鳴る音を決める式) は `crate::chord_follow`。
//!
//! ## 規則 (確定仕様: 2026-11-23 ユーザー確定、`docs/plan_chord_track.md` §1 Q8)
//!
//! コードの帯にコードを **置いた時点** のコードがメロディーの基準で、メロディーが動くのは後からそのコードを
//! **書き換えた** ときだけ。和音のパートは今どおり、自分の和音から帯のコードへ合わせる (`crate::chord_follow`)。
//!
//! - **記録 1 件 = (素材, 箱の系統) の組**: 「この素材は、系統 L の箱の下で、このコードのときに置かれていた」。
//!   系統 ([`ChordEvent::lineage`]) は箱の安定 id で、記録は **同じ系統の箱の下でだけ** 効く。記録の無い組は
//!   今のコードが基準 = どのモードでも動かない。だから箱を置く・貼る・複製する・抽出する (新しい系統) と、その下の
//!   素材は動かず、素材を別の箱の下へ動かす / 写す (系統が違う) と書いた音で鳴る。元の箱の下へ戻せば元どおり。
//! - **記録を書くのは書き換えだけ** ([`Song::set_chord`] / [`Song::transpose_chords`])。書き換える箱の下にある
//!   素材のうち、**その系統でまだ記録の無いもの** に、書き換える **前** のコードを書く。同じ `edit_song` の中なので
//!   undo は 1 段。記録は鳴らし方ではなく素材についての事実なので、追従しないトラック (Off) と曲全体のスイッチが
//!   切れている間の書き換えも記録する (後で ON にすると、ずっと ON だったときと同じ音で鳴る — A/B で比べられる)。
//!   - MIDI: ノートの頭 (`Clip::content_to_song_beat`) が書き換える箱にあるもの (ミュートしたノート / 窓の外へ
//!     隠したノートも書く — 後で出したときに、そこだけ動かないまま残らない)。
//!   - オーディオ: 見えている event ∩ 箱の範囲を、波形と再生の写像 (`crate::audio_render::event_wave_spans`) で
//!     音源フレームの範囲へ写し、`(take, 系統)` で記録の無い部分にだけ書く (`AudioEvent::take_key` は分割の片が
//!     継ぐので、分割しても引き継ぎが要らない)。箱を伸ばして新しく覆った部分は記録が無いので動かない。
//! - **系統** (`chord_ops.rs`): 新しく置く箱は新しい系統、同じ箱の変形 (移動 / 伸縮 / 範囲の変更) は保つ、割れた片
//!   (上書きで削られた右側 / 時間操作の境界) は継ぐ。時間ごと貼った箱 (Paste / Duplicate Time、セクションの複製) は
//!   写しの系統ごとに新しい系統で、貼ったクリップの記録も同じ表で付け替える ([`ClipChordBasis::relineaged`])。
//! - 書き換えを重ねても記録は最初のコードのまま (C → Dm → Em なら C → Em へ直接写す。差は積み重ならない)。
//!   元のコードへ書き戻せば動かない。
//! - 置いている最中の箱 (ダブルクリックで作ってポップアップでコードを決める) のコードを決めるのは書き換えではない
//!   (`ChordTrackEvent::Recast` → [`Song::recast_chords`]、記録を書かない)。
//! - **引き継ぎ**: クリップの移動 / 分割 / 複製 / 同じプロジェクトへのコピーは `Clip` ごと運ぶ (note id と take が
//!   保たれる。行き先が同じ系統の箱の下なら同じ音、別の箱の下なら書いた音)。ノートを割ると後ろの片が新しい id に
//!   なるので、元の id の記録を写す ([`Song::copy_note_basis`])。ノートの複製・貼り付け・追加・録音は新しい素材
//!   (記録なし = 書いた音)。別のプロジェクトへ貼るクリップは記録を捨てる (系統の番号の空間が違う)。
//! - **鳴る音で確定** したノートの記録は消す ([`Song::forget_note_basis`]、確定した音 = 今のコードに対して書いた音)。
//! - ランチャーのセルは記録を読まない (曲の位置を持たない)。記録は残す (アレンジへ戻すと元どおり鳴る)。
//! - 読み込みと保存の前に [`Song::normalize_chord_basis`] で並びと値域を揃え、もう無いノート / take / 系統の記録を
//!   捨てる (系統の番号は再利用しないので、消えた系統の記録は二度と効かない。undo は Song ごと戻すので記録も戻る)。

use super::*;
use crate::audio_render::{TempoMap, WaveSpan, event_wave_spans};

/// 書き換える箱 1 つ (書き換える前の値)。
#[derive(Debug, Clone, Copy)]
struct Rewritten {
    id: u32,
    lineage: u32,
    start: f64,
    end: f64,
    chord: Chord,
}

/// オーディオ event 1 つの記録を **曲の拍** へ写したもの (engine の compile が event ごとに 1 回作り
/// ([`Song::audio_event_basis`])、RT は曲の拍と箱の系統で二分探索するだけ)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioEventBasis {
    /// 追従を評価する曲の拍の範囲 (見えている event ∩ クリップの窓)。クロスフェードの張り出しはこの範囲の端の値で
    /// 鳴らす (窓の外は記録を引けないので、そのまま引くと張り出しの間だけ動かなくなる)。
    pub window: (f64, f64),
    /// `(lineage, start_beat)` 昇順。
    pub placed: Vec<AudioBasisSpan>,
}

/// [`AudioEventBasis`] の 1 区間: 曲の拍 `[start_beat, end_beat)` の音源は、系統 `lineage` の箱の下で `chord` のときに
/// 置かれていた。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioBasisSpan {
    pub lineage: u32,
    pub start_beat: f64,
    pub end_beat: f64,
    pub chord: Chord,
}

impl AudioEventBasis {
    /// 評価する拍: `beat` を [`Self::window`] の中へ寄せたもの (窓が空なら `beat` のまま)。
    #[must_use]
    pub fn clamp(&self, beat: f64) -> f64 {
        let (a, b) = self.window;
        if b.partial_cmp(&a) != Some(std::cmp::Ordering::Greater) {
            return beat;
        }
        // 終端は排他なので、終端の 1 ulp 手前ではなく最小の箱より短い幅だけ手前へ (浮動小数の丸めで外へ出さない)。
        beat.max(a).min(b - (b - a).min(1e-7))
    }

    /// 曲の拍 `beat` の、系統 `lineage` の記録のコード。RT 安全 (二分探索)。
    #[must_use]
    pub fn placed_at(&self, lineage: u32, beat: f64) -> Option<&Chord> {
        let i = self.placed.partition_point(|s| (s.lineage, s.start_beat) <= (lineage, beat));
        let s = self.placed.get(i.checked_sub(1)?)?;
        (s.lineage == lineage && beat < s.end_beat).then_some(&s.chord)
    }
}

/// 拍の範囲 `[b0, b1)` (span と同じ event-local 拍) が読む音源フレームの範囲 (端は丸めの分だけ外へ広げる)。
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

/// [`span_frames`] の逆: 音源フレームの範囲 `[f0, f1)` を span `s` が鳴らす event-local 拍の範囲。
fn span_beats(s: &WaveSpan, f0: u64, f1: u64) -> Option<(f64, f64)> {
    let (f0, f1) = (f0.max(s.source_start), f1.min(s.source_end));
    let frames = s.source_end.saturating_sub(s.source_start);
    if f1 <= f0 || frames == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let at = |f: u64| (f - s.source_start) as f64 / frames as f64;
    let beats = s.end_beat - s.start_beat;
    let (fa, fb) = if s.reversed { (1.0 - at(f1), 1.0 - at(f0)) } else { (at(f0), at(f1)) };
    Some((s.start_beat + fa * beats, s.start_beat + fb * beats))
}

/// `records` (`(take, lineage, start_frame)` 昇順) のうち take `take`・系統 `lineage` の区間。RT 安全。
fn group(records: &[AudioChordBasis], take: u32, lineage: u32) -> &[AudioChordBasis] {
    let lo = records.partition_point(|r| (r.take, r.lineage) < (take, lineage));
    let hi = records.partition_point(|r| (r.take, r.lineage) <= (take, lineage));
    &records[lo..hi]
}

/// `records` のうち take `take` の区間 (系統を問わない)。
fn take_group(records: &[AudioChordBasis], take: u32) -> &[AudioChordBasis] {
    let lo = records.partition_point(|r| r.take < take);
    let hi = records.partition_point(|r| r.take <= take);
    &records[lo..hi]
}

/// 同じ `(take, lineage)` の記録 `group` (非交差・昇順) のうち `[f0, f1)` に掛かるもの。
fn overlapping(group: &[AudioChordBasis], f0: u64, f1: u64) -> &[AudioChordBasis] {
    let lo = group.partition_point(|r| r.end_frame <= f0);
    let hi = lo + group[lo..].partition_point(|r| r.start_frame < f1);
    &group[lo..hi]
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

    /// 書き換える箱 `boxes` (`start` 昇順) の下にある、その系統で記録の無い素材へ書き換える前のコードを記録する
    /// (module doc の規則)。
    fn remember_chord_basis(&mut self, boxes: &[Rewritten]) {
        if boxes.is_empty() {
            return;
        }
        let tempo = TempoMap::from_song(self);
        let by_id: HashMap<u32, Rewritten> = boxes.iter().map(|b| (b.id, *b)).collect();
        // (track 位置, clip 位置, 足す記録)。先に全部決めてから書く (Song を読みながら書かない)。
        let mut adds: Vec<(usize, usize, ClipChordBasis)> = Vec::new();
        for (ti, track) in self.tracks.iter().enumerate() {
            for (ci, clip) in track.clips.iter().enumerate() {
                let add = self.records_to_add(&tempo, boxes, &by_id, clip);
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

    /// クリップ `clip` の素材のうち、書き換える箱 `boxes` (`by_id` は id で引く表) の下にあって記録の無いものへ足す記録。
    fn records_to_add(
        &self,
        tempo: &TempoMap<'_>,
        boxes: &[Rewritten],
        by_id: &HashMap<u32, Rewritten>,
        clip: &Clip,
    ) -> ClipChordBasis {
        match self.clip_contents.get(&clip.content_id) {
            Some(ClipContent::Midi(m)) => ClipChordBasis { notes: self.note_records_to_add(by_id, clip, &m.notes), audio: Vec::new() },
            Some(ClipContent::Audio(content)) => {
                let audio = content.events.iter().flat_map(|e| self.audio_records_to_add(tempo, boxes, clip, e)).collect();
                ClipChordBasis { notes: Vec::new(), audio }
            }
            _ => ClipChordBasis::default(),
        }
    }

    /// [`Self::records_to_add`] の MIDI: ノートの頭が書き換える箱にあるもの (再生と同じ `chord_at_onset` で箱を引く)。
    fn note_records_to_add(&self, by_id: &HashMap<u32, Rewritten>, clip: &Clip, notes: &[Note]) -> Vec<NoteChordBasis> {
        notes
            .iter()
            .filter_map(|n| {
                let b = by_id.get(&self.chord_at_onset(clip.content_to_song_beat(n.start_beat))?.id)?;
                crate::chord_follow::note_basis_at(&clip.chord_basis.notes, n.id, b.lineage).is_none().then_some(
                    NoteChordBasis { note_id: n.id, lineage: b.lineage, chord: b.chord },
                )
            })
            .collect()
    }

    /// [`Self::records_to_add`] のオーディオ event 1 つ: 箱ごとの音源フレームの範囲のうち、その `(take, 系統)` で記録の
    /// 無い部分。
    fn audio_records_to_add(&self, tempo: &TempoMap<'_>, boxes: &[Rewritten], clip: &Clip, event: &AudioEvent) -> Vec<AudioChordBasis> {
        let take = event.take_key();
        let visible = clip.content_to_song_beat(event.event_start_in_clip_beats);
        let (w0, w1) = clip.song_window();
        let (va, vb) = (visible.max(w0), (visible + event.event_length_beats).min(w1));
        let first = boxes.partition_point(|r| r.end <= va);
        let mut out = Vec::new();
        for b in boxes[first..].iter().take_while(|r| r.start < vb) {
            let existing = group(&clip.chord_basis.audio, take, b.lineage);
            for (f0, f1) in self.event_frames(tempo, clip, event, b.start, b.end) {
                let pieces = uncovered(overlapping(existing, f0, f1), f0, f1);
                out.extend(pieces.into_iter().map(|(s, e)| AudioChordBasis {
                    take,
                    lineage: b.lineage,
                    start_frame: s,
                    end_frame: e,
                    chord: b.chord,
                }));
            }
        }
        out
    }

    /// 書き換える前の箱の値 (`ids` が空なら全部、`changes` がコードを変えるものだけ)。
    fn rewritten_boxes(&self, ids: &[u32], changes: impl Fn(&Chord) -> bool) -> Vec<Rewritten> {
        self.chords
            .iter()
            .filter(|c| (ids.is_empty() || ids.contains(&c.id)) && changes(&c.chord))
            .map(|c| Rewritten { id: c.id, lineage: c.lineage, start: c.start_beat, end: c.end_beat(), chord: c.chord })
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

    /// トラック `track_id` のクリップ `clip_id` (アレンジ / セル) から、ノート `note_ids` (空なら全部) の記録を
    /// 系統を問わず消す (鳴る音で確定したノート)。消したら `true`。
    pub fn forget_note_basis(&mut self, track_id: u32, clip_id: u32, note_ids: &[u32]) -> bool {
        let Some(clip) = self.track_by_id_mut(track_id).and_then(|t| t.clip_by_id_mut(clip_id)) else { return false };
        let ids: std::collections::HashSet<u32> = note_ids.iter().copied().collect();
        let before = clip.chord_basis.notes.len();
        clip.chord_basis.notes.retain(|b| !ids.is_empty() && !ids.contains(&b.note_id));
        clip.chord_basis.notes.len() != before
    }

    /// content `content_id` を見る全クリップ (アレンジとセル) で、ノートの記録を `pairs` の `(元の id, 写す先の id)`
    /// へ系統ごと写す (ノートを割った後ろの片)。元に記録が無ければ何もしない。共有している content を割る前に
    /// fork した場合は、張り替えた **後** で呼ぶ (張り替える前は fork 先を見るクリップが無い)。
    pub fn copy_note_basis(&mut self, content_id: ContentId, pairs: &[(u32, u32)]) {
        if pairs.is_empty() {
            return;
        }
        for track in &mut self.tracks {
            let clips = track.clips.iter_mut().chain(track.session_clips.iter_mut().map(|c| &mut c.clip));
            for clip in clips.filter(|c| c.content_id == content_id && !c.chord_basis.notes.is_empty()) {
                let basis = &mut clip.chord_basis.notes;
                let copies: Vec<NoteChordBasis> = pairs
                    .iter()
                    .flat_map(|&(from, to)| {
                        let lo = basis.partition_point(|b| b.note_id < from);
                        let hi = basis.partition_point(|b| b.note_id <= from);
                        basis[lo..hi].iter().map(move |b| NoteChordBasis { note_id: to, ..*b })
                    })
                    .collect();
                if copies.is_empty() {
                    continue;
                }
                basis.retain(|b| !copies.iter().any(|c| c.note_id == b.note_id && c.lineage == b.lineage));
                basis.extend(copies);
                basis.sort_by_key(|b| (b.note_id, b.lineage));
            }
        }
    }

    /// 全クリップ (アレンジとセル) の記録の不変条件を回復する: 並び・重複なし・同じ `(take, 系統)` の中で非交差・
    /// コードの値域、もう content に無いノート / take の記録と、どの箱も持たない系統の記録を捨てる。
    /// 読み込みと保存の前に通す (`normalize_chords` の後)。idempotent。
    pub fn normalize_chord_basis(&mut self) {
        let contents = &self.clip_contents;
        let lineages: std::collections::HashSet<u32> = self.chords.iter().map(|c| c.lineage).collect();
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
                        basis.notes.retain(|b| ids.contains(&b.note_id) && lineages.contains(&b.lineage));
                        basis.audio.clear();
                    }
                    Some(ClipContent::Audio(a)) => {
                        let takes: std::collections::HashSet<u32> = a.events.iter().map(AudioEvent::take_key).collect();
                        basis.audio.retain(|b| takes.contains(&b.take) && lineages.contains(&b.lineage));
                        basis.notes.clear();
                    }
                    _ => *basis = ClipChordBasis::default(),
                }
                *basis = std::mem::take(basis).normalized();
            }
        }
    }

    /// クリップ `clip` のオーディオ event `event` の記録を曲の拍へ写したもの (engine の compile が event ごとに 1 回だけ
    /// 作る。RT は曲の拍で引くだけ)。`tempo` は [`TempoMap::from_song`] (compile で 1 回作ったもの)。
    #[must_use]
    pub fn audio_event_basis(&self, tempo: &TempoMap<'_>, clip: &Clip, event: &AudioEvent) -> AudioEventBasis {
        let visible = clip.content_to_song_beat(event.event_start_in_clip_beats);
        let (w0, w1) = clip.song_window();
        let window = (visible.max(w0), (visible + event.event_length_beats).min(w1));
        let records = take_group(&clip.chord_basis.audio, event.take_key());
        let Some(source) = self.media.audio_sources.get(&event.source_id).filter(|_| !records.is_empty()) else {
            return AudioEventBasis { window, placed: Vec::new() };
        };
        let mut spans = Vec::new();
        event_wave_spans(event, source.sample_rate, tempo, visible, &mut spans);
        let (lo, hi) = (event.source_start_frames, event.source_end_frames);
        let mut placed = Vec::new();
        let mut i = 0;
        while i < records.len() {
            let g = group(records, records[i].take, records[i].lineage);
            i += g.len().max(1);
            for r in overlapping(g, lo, hi) {
                for s in &spans {
                    let Some((b0, b1)) = span_beats(s, r.start_frame, r.end_frame) else { continue };
                    let (b0, b1) = ((b0 + visible).max(window.0), (b1 + visible).min(window.1));
                    if b1 > b0 {
                        placed.push(AudioBasisSpan { lineage: r.lineage, start_beat: b0, end_beat: b1, chord: r.chord });
                    }
                }
            }
        }
        placed.sort_by(|a, b| a.lineage.cmp(&b.lineage).then(a.start_beat.total_cmp(&b.start_beat)));
        AudioEventBasis { window, placed }
    }

    /// 焼き込み (バウンス / オーディオの Glue) の写し: クリップ `keys` の素材が箱ごとに動いていた元のコード
    /// (`chord_follow::effective_basis` — 和音のパートは自分の和音、和音を持たない素材は記録。動いていなかった素材は
    /// 写さない) を、焼いた音源のクリップ `baked` の event `event` の音源フレームへ、その箱の系統で写した記録。
    ///
    /// 焼いた音は書いた音なので、写した記録から動かすと元の素材と同じ量だけ動く。ただしオーディオは全体を平行移動する
    /// だけなので、焼く前と後で鳴りが揃うのは平行移動 / ベースだけ (最寄りのコード音 / スケールで構成音だけが寄って
    /// いた旋律は、焼いた後は全体が根音の差で動く)。箱の下で鳴っていた基準が複数あれば、鳴る長さがいちばん長いもの。
    /// 元を置き換える前に呼ぶ。
    #[must_use]
    pub fn baked_chord_basis(&self, keys: &[ClipKey], baked: &Clip, event: &AudioEvent) -> ClipChordBasis {
        let tempo = TempoMap::from_song(self);
        // 箱の id → (元のコード, 鳴る長さ) の重み。素材を 1 回ずつなめて足す (箱ごとに素材をなめ直さない)。
        let mut weights: HashMap<u32, Vec<(Chord, f64)>> = HashMap::new();
        for key in keys {
            if let Some(src) = self.clip_by_key(*key) {
                self.add_moved_basis(&tempo, key.track_id, src, &mut weights);
            }
        }
        let mut out = ClipChordBasis::default();
        if weights.is_empty() {
            return out;
        }
        let (w0, w1) = baked.song_window();
        let first = self.chords.partition_point(|c| c.end_beat() <= w0);
        for c in self.chords[first..].iter().take_while(|c| c.start_beat < w1) {
            let Some(chord) = weights.remove(&c.id).and_then(|w| w.into_iter().max_by(|a, b| a.1.total_cmp(&b.1))).map(|w| w.0)
            else {
                continue;
            };
            for (f0, f1) in self.event_frames(&tempo, baked, event, c.start_beat, c.end_beat()) {
                out.audio.push(AudioChordBasis { take: event.take_key(), lineage: c.lineage, start_frame: f0, end_frame: f1, chord });
            }
        }
        out.normalized()
    }

    /// Bounce In Place の記録: content `content_id` を窓 `window` で見る全クリップ (`Song::replace_window_content` が
    /// 一緒に置き換える linked clip) ごとの、焼いた event `event` で鳴らす記録 `(track id, clip id, 記録)`。アレンジの
    /// クリップはそれぞれの位置で [`Self::baked_chord_basis`]、セルは空 (記録を読まない)。どれも元の記録をまるごと
    /// 置き換える (焼いた event の take は元の take の番号と重なりうるので、元の記録を残すと別の音源に当たる)。
    /// 元を置き換える前に呼ぶ。
    #[must_use]
    pub fn baked_window_bases(&self, content_id: ContentId, window: (f64, f64), event: &AudioEvent) -> Vec<(u32, u32, ClipChordBasis)> {
        let mut out = Vec::new();
        for track in &self.tracks {
            for clip in track.clips.iter().filter(|c| super::bounce_ops::shows_window(c, content_id, window)) {
                let key = ClipKey { track_id: track.id, clip_id: clip.id };
                out.push((track.id, clip.id, self.baked_chord_basis(&[key], clip, event)));
            }
            let cells = track.session_clips.iter().map(|c| &c.clip);
            for clip in cells.filter(|c| super::bounce_ops::shows_window(c, content_id, window)) {
                out.push((track.id, clip.id, ClipChordBasis::default()));
            }
        }
        out
    }

    /// クリップ `clip` の素材が箱ごとに動いていた元のコードを、鳴る長さ (MIDI はノートの長さ、オーディオは音源
    /// フレーム) で `weights` (箱の id ごと) に足す。動いていない素材は足さない。
    fn add_moved_basis(&self, tempo: &TempoMap<'_>, track_id: u32, clip: &Clip, weights: &mut HashMap<u32, Vec<(Chord, f64)>>) {
        let moved: Vec<(u32, Chord, f64)> = match self.clip_contents.get(&clip.content_id) {
            Some(ClipContent::Midi(m)) => self.moved_note_basis(track_id, clip, &m.notes),
            Some(ClipContent::Audio(a)) => {
                let (w0, w1) = clip.song_window();
                let first = self.chords.partition_point(|c| c.end_beat() <= w0);
                let boxes = self.chords[first..].iter().take_while(|c| c.start_beat < w1);
                boxes
                    .flat_map(|c| a.events.iter().filter_map(move |e| Some((c.id, self.moved_audio_basis(tempo, clip, e, c)?))))
                    .map(|(id, (chord, w))| (id, chord, w))
                    .collect()
            }
            _ => Vec::new(),
        };
        for (id, chord, w) in moved {
            let list = weights.entry(id).or_default();
            match list.iter_mut().find(|(k, _)| *k == chord) {
                Some((_, sum)) => *sum += w,
                None => list.push((chord, w)),
            }
        }
    }

    /// [`Self::add_moved_basis`] の MIDI: ノートごとの (従う箱の id, 動いていた元のコード, ノートの長さ)。モードは問わない
    /// (記録と同じく素材の事実。焼いた音源は元のトラックのモードを継ぐ)。
    fn moved_note_basis(&self, track_id: u32, clip: &Clip, notes: &[Note]) -> Vec<(u32, Chord, f64)> {
        let follow = crate::chord_follow::TrackChordFollow::of(self, track_id);
        let own = crate::chord_follow::note_own_chords(notes);
        notes
            .iter()
            .filter(|n| !n.muted && clip.window_has_onset(n.start_beat))
            .filter_map(|n| {
                let src = crate::chord_follow::NoteSource::of(&own, &clip.chord_basis.notes, n);
                let (ev, basis) = follow.note_basis(clip.content_to_song_beat(n.start_beat), &src)?;
                Some((ev.id, *crate::chord_follow::effective_basis(basis)?, n.duration_beats))
            })
            .collect()
    }

    /// [`Self::add_moved_basis`] のオーディオ event 1 つ・箱 `c` 1 つ: 動いていた元のコードと、その音源フレームの長さ。
    fn moved_audio_basis(&self, tempo: &TempoMap<'_>, clip: &Clip, event: &AudioEvent, c: &ChordEvent) -> Option<(Chord, f64)> {
        let frames = self.event_frames(tempo, clip, event, c.start_beat, c.end_beat());
        let (own, placed) = self.frames_basis(clip, event, c.lineage, &frames);
        let chord = *crate::chord_follow::effective_basis(crate::chord_follow::Basis { own: own.as_ref(), placed: placed.as_ref() })?;
        #[allow(clippy::cast_precision_loss)]
        Some((chord, frames.iter().map(|(f0, f1)| (f1 - f0) as f64).sum()))
    }

    /// `bases` (`(track id, clip id, 記録)`、[`Self::baked_window_bases`]) の記録をそれぞれのクリップへ入れる
    /// (アレンジ / セル)。
    pub fn set_chord_bases(&mut self, bases: Vec<(u32, u32, ClipChordBasis)>) {
        for (track_id, clip_id, basis) in bases {
            if let Some(clip) = self.track_by_id_mut(track_id).and_then(|t| t.clip_by_id_mut(clip_id)) {
                clip.chord_basis = basis;
            }
        }
    }

    /// 音源フレームの範囲 `frames` (event 1 つ・箱 1 つぶん) の焼き込みの基準: いちばん長く重なる、和音を弾いている区間の
    /// コードと、その箱の系統でいちばん長く重なる記録のコード。
    fn frames_basis(&self, clip: &Clip, event: &AudioEvent, lineage: u32, frames: &[(u64, u64)]) -> (Option<Chord>, Option<Chord>) {
        let spans = self.audio_chords.get(&event.source_id).map_or(&[][..], Vec::as_slice);
        let own = frames
            .iter()
            .filter_map(|&(f0, f1)| crate::chord_detect::audio_longest_own_in(spans, f0, f1))
            .max_by_key(|(_, overlap)| *overlap)
            .map(|(chord, _)| *chord);
        let g = group(&clip.chord_basis.audio, event.take_key(), lineage);
        let placed = frames
            .iter()
            .flat_map(|&(f0, f1)| {
                overlapping(g, f0, f1).iter().map(move |r| (r.chord, r.end_frame.min(f1).saturating_sub(r.start_frame.max(f0))))
            })
            .max_by_key(|(_, o)| *o)
            .map(|(chord, _)| chord);
        (own, placed)
    }
}

impl ClipChordBasis {
    /// 並び (`(note_id, lineage)` 昇順 / `(take, lineage, start_frame)` 昇順)・重複なし・同じ `(take, lineage)` の中で
    /// 非交差・コードの値域を揃えたもの (外 = ファイル / clipboard から来た値。記録は二分探索で引くので並びが崩れると
    /// 黙って外れる)。idempotent。
    #[must_use]
    pub fn normalized(mut self) -> Self {
        for b in &mut self.notes {
            b.chord = b.chord.sanitized();
        }
        self.notes.sort_by_key(|b| (b.note_id, b.lineage));
        self.notes.dedup_by_key(|b| (b.note_id, b.lineage));
        normalize_audio_basis(&mut self.audio);
        self
    }

    /// 系統を `table` (元の系統 → 新しい系統) で付け替えた写し。表に無い系統の記録は捨てる (時間ごと貼ったクリップ:
    /// 貼った箱の下でだけ、元と同じ音で鳴る)。
    #[must_use]
    pub fn relineaged(&self, table: &HashMap<u32, u32>) -> Self {
        let notes = self
            .notes
            .iter()
            .filter_map(|b| Some(NoteChordBasis { lineage: *table.get(&b.lineage)?, ..*b }))
            .collect();
        let audio = self
            .audio
            .iter()
            .filter_map(|b| Some(AudioChordBasis { lineage: *table.get(&b.lineage)?, ..*b }))
            .collect();
        Self { notes, audio }.normalized()
    }
}

/// 同じ `(take, lineage)` の記録 `covered` (昇順・非交差) が覆っていない、`[f0, f1)` の部分。
fn uncovered(covered: &[AudioChordBasis], f0: u64, f1: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut at = f0;
    for r in covered {
        if r.start_frame > at {
            out.push((at, r.start_frame.min(f1)));
        }
        at = at.max(r.end_frame);
    }
    if at < f1 {
        out.push((at, f1));
    }
    out
}

/// オーディオの記録の並び (`(take, lineage, start_frame)` 昇順) と、同じ `(take, lineage)` の中の非交差 (先に始まる方を
/// 残して後発の頭を削る)、隣り合う同じコードの結合、コードの値域。idempotent。
fn normalize_audio_basis(records: &mut Vec<AudioChordBasis>) {
    records.retain(|r| r.end_frame > r.start_frame);
    records.sort_by_key(|r| (r.take, r.lineage, r.start_frame));
    let mut out: Vec<AudioChordBasis> = Vec::with_capacity(records.len());
    for mut r in records.drain(..) {
        r.chord = r.chord.sanitized();
        if let Some(last) = out.last_mut().filter(|l| (l.take, l.lineage) == (r.take, r.lineage)) {
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
