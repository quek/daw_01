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
//!   書き換えで動いていた素材の上に箱を置く・貼る・複製する・抽出する、または別の箱を移動 / 伸縮で重ねると、その
//!   部分は書いた音に戻る (同じコードでも — 箱の系統が変わるため)。
//! - **新しく基準を決めて書くのは書き換えだけ** ([`Song::set_chord`] / [`Song::transpose_chords`])。書き換える箱の
//!   下にある素材のうち、**その系統でまだ記録の無いもの** に、書き換える **前** のコードを書く。同じ `edit_song` の
//!   中なので undo は 1 段。記録は鳴らし方ではなく素材についての事実なので、追従しないトラック (Off) と曲全体の
//!   スイッチが切れている間の書き換えも記録する (後で ON にすると、ずっと ON だったときと同じ音で鳴る — A/B で
//!   比べられる)。ほかに記録を書くのは、元の素材の基準を新しい素材へ移す経路 (下の引き継ぎ・時間ごとの貼り付け・
//!   焼き込み・Glue) だけ。1 件は (ノート, 系統) の組で bincode 約 7〜12 B (Note 1 個の約 4 割)、ノートと同じく
//!   毎回の LoadSong と undo の snapshot に載る。
//!   - MIDI: ノートの頭 (`Clip::content_to_song_beat`) が書き換える箱にあるもの (ミュートしたノート / 窓の外へ
//!     隠したノートも書く — 後で出したときに、そこだけ動かないまま残らない)。
//!   - オーディオ: 見えている event ∩ 箱の範囲を、波形と再生の写像 (`crate::audio_render::event_wave_spans`) で
//!     音源フレームの範囲へ写し、`(take, 系統)` で記録の無い部分にだけ書く (`AudioEvent::take_key` は分割の片が
//!     継ぐので、分割しても引き継ぎが要らない)。箱を伸ばして新しく覆った部分は記録が無いので動かない。
//! - **系統** (`chord_ops.rs`): 新しく置く箱は新しい系統、同じ箱の変形 (移動 / 伸縮 / 範囲の変更) は保つ、割れた片
//!   (上書きで削られた右側 / 時間操作の境界) は継ぐ。時間ごと貼った箱 (Paste / Duplicate Time、セクションの複製) は
//!   写しの系統ごとに新しい系統で、貼ったクリップの記録も同じ表で付け替える ([`ClipChordBasis::relineaged`] —
//!   箱と記録が組で運ばれるので、別のプロジェクトからでも残す。範囲の外の箱の系統の記録だけを捨てる)。
//! - 書き換えを重ねても記録は最初のコードのまま (C → Dm → Em なら C → Em へ直接写す。差は積み重ならない)。
//!   元のコードへ書き戻せば動かない。
//! - 置いている最中の箱 (ダブルクリックで作ってポップアップでコードを決める) のコードを決めるのは書き換えではない
//!   (`ChordTrackEvent::Recast` → [`Song::recast_chords`]、記録を書かない)。
//! - **引き継ぎ**: クリップの移動 / 分割 / 複製 / 同じプロジェクトへのコピーは `Clip` ごと運ぶ (note id と take が
//!   保たれる。行き先が同じ系統の箱の下なら同じ音、別の箱の下なら書いた音)。ノートを割ると後ろの片が新しい id に
//!   なるので、元の id の記録を写す ([`Song::copy_note_basis`])。ノートの複製・貼り付け・追加・録音は新しい素材
//!   (記録なし = 書いた音)。クリップ / セル / トラックの貼り付けは [`ClipChordBasis::pasted`] を通す: 同じ番号の
//!   空間 (同じプロジェクトの、同じ開き方のコピー) からなら運び、それ以外 (別のプロジェクト / 保存せずに閉じた・
//!   Revert した後 / Save As で分けた版) からは捨てる (箱が付いてこないので、系統の番号が別の箱を指しうる)。
//! - **焼き込み** (Bounce In Place / オーディオの Glue): [`Song::baked_chord_basis`] / [`Song::baked_window_bases`] が、
//!   元の素材が箱ごとに動いていた基準 (`chord_follow::effective_basis` — 和音のパートは自分の和音、ほかは記録) を、
//!   焼いた音源の take の記録にその箱の系統で書く。元の記録はまるごと置き換え (take 番号が重なりうるため)、セルは空。
//! - **MIDI の Glue**: 元のクリップの記録を、振り直した note id へ系統ごと写す (`daw_gui` の `handler/glue.rs`)。
//! - **鳴る音で確定** したノートの記録は消す ([`Song::forget_note_basis`]、確定した音 = 今のコードに対して書いた音)。
//! - ランチャーのセルは記録を読まない (曲の位置を持たない)。記録は残す (アレンジへ戻すと元どおり鳴る)。
//! - 読み込みと保存の前に [`Song::normalize_chord_basis`] で並びと値域を揃え、もう無いノート / take / 系統の記録を
//!   捨てる (系統の番号は再利用しないので、消えた系統の記録は二度と効かない。undo は Song ごと戻すので記録も戻る)。
//!   開いている間も、書き換えで記録を足すクリップからはもう無い系統の記録を落とす (積み増さない)。

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

/// 1 回の書き換え ([`Song::remember_chord_basis`]) の箱の組。
struct RewriteSet<'a> {
    /// `start` 昇順。
    boxes: &'a [Rewritten],
    by_id: &'a HashMap<u32, Rewritten>,
    /// ノートの頭がこの外なら、どの書き換える箱にも従わない (`[lo, hi)`)。
    onset_range: (f64, f64),
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

/// 記録の端を箱の端へ戻す幅の上限 (拍)。1 フレームを保持する span (warp の退化) は 1 フレームが長い拍になるので、
/// 遠い箱の端へ寄せない。
const SNAP_MAX_BEATS: f64 = 1e-3;
/// 浮動小数の丸めの分の余白 (拍)。
const SNAP_MARGIN_BEATS: f64 = 1e-9;

/// span `s` の音源 1 フレームの拍の幅 (音源を持たない span は 0)。
fn beats_per_frame(s: &WaveSpan) -> f64 {
    let frames = s.source_end.saturating_sub(s.source_start);
    #[allow(clippy::cast_precision_loss)]
    if frames == 0 { 0.0 } else { (s.end_beat - s.start_beat).abs() / frames as f64 }
}

/// event の波形の span を音源フレームで引く索引 (記録ごとに span を全件なめない)。span は warp marker の逆行 /
/// 1 フレームの保持で音源の上で重なりうるので、`source_end` は単調でない — 開始順に並べ、`source_end` の累積最大で
/// 「ここより前は全部 `f0` 以前に終わる」位置を二分探索する。
struct SpansBySource {
    spans: Vec<WaveSpan>,
    /// `spans` の添字を `source_start` 昇順に。
    order: Vec<usize>,
    /// `order` の順の `source_end` の累積最大。
    max_end: Vec<u64>,
}

impl SpansBySource {
    fn new(spans: Vec<WaveSpan>) -> Self {
        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_by_key(|&i| spans[i].source_start);
        let max_end = order
            .iter()
            .scan(0u64, |m, &i| {
                *m = (*m).max(spans[i].source_end);
                Some(*m)
            })
            .collect();
        Self { spans, order, max_end }
    }

    /// 音源フレームの範囲 `[f0, f1)` に掛かりうる span (掛からないものも混ざる — `span_beats` が落とす)。
    fn touching(&self, f0: u64, f1: u64) -> impl Iterator<Item = &WaveSpan> {
        let hi = self.order.partition_point(|&i| self.spans[i].source_start < f1);
        let lo = self.max_end[..hi].partition_point(|&e| e <= f0);
        self.order[lo..hi].iter().map(|&i| &self.spans[i])
    }
}

/// 系統ごとの箱の端 (曲の拍、昇順)。
struct LineageEdges(HashMap<u32, Vec<f64>>);

impl LineageEdges {
    fn of(chords: &[ChordEvent]) -> Self {
        let mut map: HashMap<u32, Vec<f64>> = HashMap::new();
        for c in chords {
            map.entry(c.lineage).or_default().extend([c.start_beat, c.end_beat()]);
        }
        for edges in map.values_mut() {
            edges.sort_by(f64::total_cmp);
        }
        Self(map)
    }

    /// `beat` から `tol` 以内に系統 `lineage` の箱の端があればその端、無ければ `beat`。
    fn snap(&self, lineage: u32, beat: f64, tol: f64) -> f64 {
        let Some(edges) = self.0.get(&lineage) else { return beat };
        let i = edges.partition_point(|&e| e < beat);
        let near = [i.checked_sub(1), Some(i)].into_iter().flatten().filter_map(|j| edges.get(j));
        near.copied().filter(|e| (e - beat).abs() <= tol).min_by(|a, b| (a - beat).abs().total_cmp(&(b - beat).abs())).unwrap_or(beat)
    }
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
    ///
    /// 記録が増える唯一の口なので、記録を足すクリップからは、もう効かない系統 (どの箱も持たない — 系統の番号は再利用
    /// しない) の記録を落とす: 「箱を置き直す → 書き換える」を繰り返しても、開いている間の記録が「ノート × 今ある
    /// 系統」で頭打ちになる (LoadSong と undo の snapshot に死んだ記録を積み増さない)。消えたノート / take の記録は
    /// 積み増されないので、保存と読み込みの [`Self::normalize_chord_basis`] に任せる (書き換えのたびに曲の全ノートの
    /// 集合を作らない)。
    fn remember_chord_basis(&mut self, boxes: &[Rewritten]) {
        if boxes.is_empty() {
            return;
        }
        let tempo = TempoMap::from_song(self);
        let by_id: HashMap<u32, Rewritten> = boxes.iter().map(|b| (b.id, *b)).collect();
        // 書き換える箱の外枠 (`chord_at_onset` が箱を返す範囲 `[start - eps, end)` を全部含む)。ノートの頭がこの外に
        // あれば箱を引かずに飛ばす (書き換えのたびに曲の全ノートで箱を二分探索しない)。
        let lo = boxes.iter().map(|b| b.start).fold(f64::INFINITY, f64::min) - super::chord_ops::CHORD_ONSET_EPS_BEATS;
        let hi = boxes.iter().map(|b| b.end).fold(f64::NEG_INFINITY, f64::max);
        let rewrite = RewriteSet { boxes, by_id: &by_id, onset_range: (lo, hi) };
        // (track 位置, clip 位置, 足す記録)。先に全部決めてから書く (Song を読みながら書かない)。
        let mut adds: Vec<(usize, usize, ClipChordBasis)> = Vec::new();
        for (ti, track) in self.tracks.iter().enumerate() {
            for (ci, clip) in track.clips.iter().enumerate() {
                let add = self.records_to_add(&tempo, &rewrite, clip);
                if !add.is_empty() {
                    adds.push((ti, ci, add));
                }
            }
        }
        let mut live: Vec<u32> = self.chords.iter().map(|c| c.lineage).collect();
        live.sort_unstable();
        live.dedup();
        let alive = |lineage: u32| live.binary_search(&lineage).is_ok();
        for (ti, ci, add) in adds {
            let basis = &mut self.tracks[ti].clips[ci].chord_basis;
            basis.notes.retain(|b| alive(b.lineage));
            basis.audio.retain(|b| alive(b.lineage));
            basis.notes.extend(add.notes);
            basis.audio.extend(add.audio);
            *basis = std::mem::take(basis).normalized();
        }
    }

    /// クリップ `clip` の素材のうち、書き換える箱 `rewrite` の下にあって記録の無いものへ足す記録。
    fn records_to_add(&self, tempo: &TempoMap<'_>, rewrite: &RewriteSet<'_>, clip: &Clip) -> ClipChordBasis {
        match self.clip_contents.get(&clip.content_id) {
            Some(ClipContent::Midi(m)) => {
                ClipChordBasis { notes: self.note_records_to_add(rewrite, clip, &m.notes), audio: Vec::new() }
            }
            Some(ClipContent::Audio(content)) => {
                let audio =
                    content.events.iter().flat_map(|e| self.audio_records_to_add(tempo, rewrite.boxes, clip, e)).collect();
                ClipChordBasis { notes: Vec::new(), audio }
            }
            _ => ClipChordBasis::default(),
        }
    }

    /// [`Self::records_to_add`] の MIDI: ノートの頭が書き換える箱にあるもの (再生と同じ `chord_at_onset` で箱を引く)。
    /// クリップの窓では絞らない (窓の外へ隠したノートにも書く、module doc)。ノートの並びは順不同なので全件を見るが、
    /// 外枠の外のノートは比較 1 つで飛ばす。
    fn note_records_to_add(&self, rewrite: &RewriteSet<'_>, clip: &Clip, notes: &[Note]) -> Vec<NoteChordBasis> {
        let (lo, hi) = rewrite.onset_range;
        notes
            .iter()
            .filter_map(|n| {
                let at = clip.content_to_song_beat(n.start_beat);
                if !(lo..hi).contains(&at) {
                    return None;
                }
                let b = rewrite.by_id.get(&self.chord_at_onset(at)?.id)?;
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
                let mut copies: Vec<NoteChordBasis> = pairs
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
                // 写す先に同じ `(note, 系統)` の古い記録があれば写しで置き換える (写す先は新しく採番した id なので通常は
                // 何も消えないが、残すと二分探索で引く区間に同じ鍵が 2 件入る)。`to` は一意 (`split_notes` が保証)。
                // 引くのは二分探索 (グリッドで割ると片も記録も数千になる — 総当たりは二乗)。
                let key = |b: &NoteChordBasis| (b.note_id, b.lineage);
                copies.sort_by_key(key);
                basis.retain(|b| copies.binary_search_by_key(&key(b), key).is_err());
                basis.extend(copies);
                basis.sort_by_key(key);
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
        let spans = SpansBySource::new(spans);
        let edges = LineageEdges::of(&self.chords);
        let (lo, hi) = (event.source_start_frames, event.source_end_frames);
        let mut placed = Vec::new();
        let mut i = 0;
        while i < records.len() {
            let g = group(records, records[i].take, records[i].lineage);
            i += g.len().max(1);
            for r in overlapping(g, lo, hi) {
                for s in spans.touching(r.start_frame, r.end_frame) {
                    let Some((b0, b1)) = span_beats(s, r.start_frame, r.end_frame) else { continue };
                    let (b0, b1) = ((b0 + visible).max(window.0), (b1 + visible).min(window.1));
                    // 記録の端は書いたときに箱の端を音源フレームへ丸めた (`span_frames` が外へ広げる) ので、箱の端から
                    // 1 フレーム未満ずれている。同じ系統の箱の端へ戻す — 時間操作で割れた同じ系統の片が隣り合うと、
                    // 1 フレームぶん隣の片の記録を引いて、その位置から始まる buffer だけ音程が跳ねる。
                    let tol = beats_per_frame(s).min(SNAP_MAX_BEATS) + SNAP_MARGIN_BEATS;
                    let (b0, b1) = (edges.snap(r.lineage, b0, tol), edges.snap(r.lineage, b1, tol));
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
        // 自分の和音は素材 (トラックのアレンジ / セル) ごとに 1 回だけ検出する (同じトラックのクリップを束ねる Glue)。
        let mut own: HashMap<crate::chord_follow::OwnScope, Vec<SourceChord>> = HashMap::new();
        for key in keys {
            if let Some(src) = self.clip_by_key(*key) {
                self.add_moved_basis(&tempo, *key, src, &mut own, &mut weights);
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

    /// クリップ `clip` (`key`) の素材が箱ごとに動いていた元のコードを、鳴る長さ (MIDI はノートの長さ、オーディオは
    /// 音源フレーム) で `weights` (箱の id ごと) に足す。動いていない素材は足さない。`own` は素材ごとの自分の和音の
    /// 置き場 (無ければここで検出して入れる)。
    fn add_moved_basis(
        &self,
        tempo: &TempoMap<'_>,
        key: ClipKey,
        clip: &Clip,
        own: &mut HashMap<crate::chord_follow::OwnScope, Vec<SourceChord>>,
        weights: &mut HashMap<u32, Vec<(Chord, f64)>>,
    ) {
        let moved: Vec<(u32, Chord, f64)> = match self.clip_contents.get(&clip.content_id) {
            Some(ClipContent::Midi(m)) => {
                let scope = self.own_scope_of(key);
                let own = own.entry(scope).or_insert_with(|| crate::chord_follow::own_chords_in(self, scope, None));
                self.moved_note_basis(key.track_id, clip, &m.notes, own)
            }
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
    fn moved_note_basis(&self, track_id: u32, clip: &Clip, notes: &[Note], own: &[SourceChord]) -> Vec<(u32, Chord, f64)> {
        let follow = crate::chord_follow::TrackChordFollow::of(self, track_id);
        notes
            .iter()
            .filter(|n| !n.muted && clip.window_has_onset(n.start_beat))
            .filter_map(|n| {
                let src = crate::chord_follow::NoteSource::of(own, &clip.chord_basis.notes, clip, n);
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

    /// 貼るクリップ (クリップ / セル / トラックの貼り付け) の記録。同じ番号の空間 (`same_space` = 同じプロジェクトの
    /// 同じ開き方のコピー) なら並びと値域を揃えて運び (外の JSON は並びが崩れていうる)、違えば捨てる (箱が付いて
    /// こないので、系統の番号が別の箱を指す)。
    #[must_use]
    pub fn pasted(self, same_space: bool) -> Self {
        if same_space { self.normalized() } else { Self::default() }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 音源フレームの索引は、warp の逆行 / 1 フレームの保持で音源の上で重なる span も取りこぼさない (総当たりと同じ組)。
    #[test]
    fn spans_by_source_finds_every_overlapping_span() {
        let span = |source_start, source_end| WaveSpan {
            start_beat: 0.0,
            end_beat: 1.0,
            source_start,
            source_end,
            reversed: false,
            head: false,
        };
        // 長い span の後に、その途中へ戻る短い span (逆行) と、1 フレームの保持。
        let spans = vec![span(0, 1000), span(1000, 2000), span(200, 300), span(1500, 1501), span(2000, 2400), span(50, 3000)];
        let index = SpansBySource::new(spans.clone());
        let hits = |s: &&WaveSpan, f0: u64, f1: u64| s.source_start < f1 && s.source_end > f0;
        for (f0, f1) in [(0, 10), (250, 260), (900, 1100), (1500, 1501), (2100, 2200), (2999, 3500), (3000, 4000)] {
            let mut got: Vec<(u64, u64)> =
                index.touching(f0, f1).filter(|s| hits(s, f0, f1)).map(|s| (s.source_start, s.source_end)).collect();
            let mut want: Vec<(u64, u64)> =
                spans.iter().filter(|s| hits(s, f0, f1)).map(|s| (s.source_start, s.source_end)).collect();
            got.sort_unstable();
            want.sort_unstable();
            assert_eq!(got, want, "[{f0}, {f1})");
        }
    }
}
