//! コードトラックへの追従 (r.md #157 / #159、`docs/plan_chord_track.md`) の評価の SSoT。
//!
//! 追従は **非破壊** — ノートのデータは書き換えず、鳴らす音だけを変える。音程を生む全経路 (engine の
//! ノートとオーディオクリップ、GUI の演奏プレビューとピアノロールの重ね表示、VOICEVOX 歌唱、SMF 書き出し、
//! 「鳴る音で確定」) はここにある 2 つだけを引く (経路ごとに式を写さない):
//!
//! - **ノートの鍵盤**: [`follow_pitch`] (書いた音 → 鳴る音)。
//! - **オーディオの移動量**: [`follow_shift`] (半音。全体を平行移動するだけ)。
//!
//! どちらも **書いた音の空間** の値を返す。グローバルトランスポーズ (r.md #130) は、その後に
//! `crate::transpose::sounding_key` で足す (順序は 追従 → 移調。コードも Key も書いた音の空間にある)。
//!
//! **基準 (r.md #159)** — 何から何へ合わせるか ([`Basis`]):
//!
//! - **自分の和音** (素材がその位置で一緒に弾いている和音、`crate::chord_detect::own_chords`) があれば、
//!   その和音から今のコードへ合わせる (#157 と同じ)。ただし和音が今のコードに収まっていれば動かさない
//!   (帯へ抽出しただけ・合うコードを置いただけでは動かない)。
//! - 無ければ **書かれたときのコード** (`Clip::chord_basis`、箱を書き換えたときだけ記録する) から今のコードへ。
//! - どちらも無ければ動かさない (置いただけでは動かない)。
//!
//! コードはノートの**開始位置**で決まり、伸びているノートは途中のコード変更で鳴り直さない
//! (Studio One と同じ)。オーディオの移動量は buffer ごとの今の位置で決まる。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。RT 安全 (確保・ロックなし)。

use crate::model::{Chord, ChordEvent, ChordFollow, ChordKind, NoteChordBasis, Song, SourceChord};
use crate::scale::{Scale, ScaleChange};

/// pitch class `from` を `to` へ合わせる最短の移動量 (半音、`-6..=5`)。ちょうど 6 半音は下へ。
#[must_use]
pub fn nearest_delta(from: u8, to: u8) -> i32 {
    let up = (i32::from(to % 12) - i32::from(from % 12)).rem_euclid(12);
    if up >= 6 { up - 12 } else { up }
}

/// `pitch` を、12-bit の pitch class 集合 `mask` (bit 0 = C) の最寄りの音へ寄せる。
/// 同じ距離なら上 ([`ScaleChange::snap`] と同じ規則)。`mask` が空、または届く先が 0..=127 に無ければそのまま。
#[must_use]
pub fn snap_to_mask(pitch: u8, mask: u16) -> u8 {
    let has = |p: i32| (0..=127).contains(&p) && (mask >> (p.rem_euclid(12))) & 1 == 1;
    let p = i32::from(pitch);
    if mask & 0x0FFF == 0 || has(p) {
        return pitch;
    }
    for off in 1..=11 {
        if has(p + off) {
            return (p + off) as u8;
        }
        if has(p - off) {
            return (p - off) as u8;
        }
    }
    pitch
}

/// コードに合うスケール (絶対 pitch class の 12-bit 集合)。「スケール」モードの寄せ先。
///
/// その位置にキーがあり、コードの構成音が全部キーのスケールに入っていれば **キーのスケール**
/// (ダイアトニックなコードではキーの音階へ寄せる)。そうでなければコードの種類から決めた既定のスケール
/// (ルート起点) に構成音を足したもの。
#[must_use]
pub fn chord_scale_mask(chord: &Chord, key: Option<&ScaleChange>) -> u16 {
    let tones = chord.pitch_class_mask();
    if let Some(k) = key {
        let key_mask = rotate12(k.scale.pitch_class_mask(), k.root);
        if tones & !key_mask == 0 {
            return key_mask;
        }
    }
    let scale = match chord.kind {
        ChordKind::Major | ChordKind::Major6 | ChordKind::Major7 | ChordKind::Sus2 | ChordKind::Sus4 => Scale::Major,
        ChordKind::Dominant7 | ChordKind::Dominant7Sus4 => Scale::Mixolydian,
        ChordKind::Minor | ChordKind::Minor7 => Scale::NaturalMinor,
        ChordKind::Minor6 | ChordKind::MinorMajor7 => Scale::MelodicMinor,
        ChordKind::Diminished | ChordKind::Diminished7 => Scale::Diminished,
        ChordKind::HalfDiminished7 => Scale::Locrian,
        ChordKind::Augmented | ChordKind::Augmented7 | ChordKind::AugmentedMajor7 => Scale::WholeTone,
        ChordKind::Power => Scale::MinorPentatonic,
    };
    rotate12(scale.pitch_class_mask(), chord.root) | tones
}

/// ルート起点の 12-bit 集合を絶対 pitch class の集合へ回す。
fn rotate12(mask: u16, root: u8) -> u16 {
    let m = u32::from(mask & 0x0FFF);
    let r = u32::from(root % 12);
    (((m << r) | (m >> (12 - r))) & 0x0FFF) as u16
}

/// 追従の基準 (r.md #159、module doc)。`own` = 素材が自分で弾いている和音、`placed` = 書かれたときのコード
/// (`Clip::chord_basis` の記録)。どちらも無ければ動かない。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Basis<'a> {
    pub own: Option<&'a Chord>,
    pub placed: Option<&'a Chord>,
}

impl<'a> Basis<'a> {
    /// 基準なし (動かない)。
    pub const NONE: Basis<'static> = Basis { own: None, placed: None };

    /// 自分の和音だけ。
    #[must_use]
    pub const fn own(chord: &'a Chord) -> Self {
        Basis { own: Some(chord), placed: None }
    }

    /// 書かれたときのコードだけ。
    #[must_use]
    pub const fn placed(chord: &'a Chord) -> Self {
        Basis { own: None, placed: Some(chord) }
    }
}

/// [`Basis`] を今のコード `target` に対して 1 つに決めたもの。
enum Resolved<'a> {
    /// 動かさない。
    Unchanged,
    /// 自分の和音から今のコードへ (#157 と同じ式)。
    Own(&'a Chord),
    /// 書かれたときのコードから今のコードへ (r.md #159 Q1: 平行移動 / ベースは差だけ、最寄りのコード音 /
    /// スケールは元のコードの構成音 / スケールの音だけを寄せる)。
    Placed(&'a Chord),
}

/// `outer` の構成音とベース音が、`inner` の構成音とベース音を全部含むか。
fn covers(outer: &Chord, inner: &Chord) -> bool {
    let tones = |c: &Chord| c.pitch_class_mask() | (1 << c.bass_pc());
    tones(inner) & !tones(outer) == 0
}

/// 自分の和音が今のコードに収まっていれば動かさない (抽出しただけ / 合うコードを置いただけ)。収まっていなくても、
/// 書かれたときのコードに収まっていれば (書き換える前のコードで弾いていた)、書かれたときのコードから動かす。
fn resolve<'a>(basis: Basis<'a>, target: &Chord) -> Resolved<'a> {
    match (basis.own, basis.placed) {
        (Some(own), _) if covers(target, own) => Resolved::Unchanged,
        (Some(own), Some(placed)) if covers(placed, own) => Resolved::Placed(placed),
        (Some(own), _) => Resolved::Own(own),
        (None, Some(placed)) => Resolved::Placed(placed),
        (None, None) => Resolved::Unchanged,
    }
}

/// `basis` を今のコード `target` に対して決めた、実際に動かす元のコード (動かさないなら `None`)。焼き込み (バウンス) が
/// 焼いた音源へ写す基準 (`Song::baked_chord_basis`) — 写した記録から動かすと、元の素材と同じ量だけ動く。
#[must_use]
pub fn effective_basis<'a>(basis: Basis<'a>, target: &Chord) -> Option<&'a Chord> {
    match resolve(basis, target) {
        Resolved::Unchanged => None,
        Resolved::Own(chord) | Resolved::Placed(chord) => Some(chord),
    }
}

/// オーディオクリップ (と、ノートの平行移動 / ベース) の移動量 (半音)。
///
/// `target` = その位置のコードトラックのコード、`basis` = 素材の基準。オーディオは全体を動かすだけなので、
/// `Narrow` / `Scale` は `Parallel` と同じくルートの差で動かす — ただし書かれたときのコードから動かすときは、
/// 寄せ先 (構成音 / スケール) が変わらなければ動かさない (ノートの `Narrow` / `Scale` が動かないのと揃える)。
#[must_use]
pub fn follow_shift(mode: ChordFollow, target: &Chord, basis: Basis<'_>, key: Option<&ScaleChange>) -> i32 {
    match (mode, resolve(basis, target)) {
        (ChordFollow::Off, _) | (_, Resolved::Unchanged) => 0,
        (ChordFollow::Bass, Resolved::Own(own)) => nearest_delta(own.root, target.bass_pc()),
        (_, Resolved::Own(own)) => nearest_delta(own.root, target.root),
        (ChordFollow::Bass, Resolved::Placed(placed)) => nearest_delta(placed.bass_pc(), target.bass_pc()),
        (ChordFollow::Scale, Resolved::Placed(placed)) => {
            if chord_scale_mask(placed, key) == chord_scale_mask(target, key) {
                0
            } else {
                nearest_delta(placed.root, target.root)
            }
        }
        (ChordFollow::Parallel | ChordFollow::Narrow, Resolved::Placed(placed)) => {
            // 呼び名だけを変えた書き換え (C6 → Am7: 構成音が同じ) では動かさない。
            if placed.pitch_class_mask() == target.pitch_class_mask() {
                0
            } else {
                nearest_delta(placed.root, target.root)
            }
        }
    }
}

/// **ノートの追従の本体** (唯一の式): 書いた鍵盤 `pitch` → 鳴らす鍵盤 (書いた音の空間)。
/// 0..=127 を外れたら `None` (= 鳴らさない。端に丸めない — `crate::transpose::sounding_key` と同じ理由)。
#[must_use]
pub fn follow_pitch(
    mode: ChordFollow,
    pitch: u8,
    target: &Chord,
    basis: Basis<'_>,
    key: Option<&ScaleChange>,
) -> Option<u8> {
    match mode {
        ChordFollow::Off => Some(pitch),
        ChordFollow::Parallel | ChordFollow::Bass => {
            u8::try_from(i32::from(pitch) + follow_shift(mode, target, basis, key)).ok().filter(|k| *k <= 127)
        }
        ChordFollow::Narrow => Some(match resolve(basis, target) {
            Resolved::Unchanged => pitch,
            Resolved::Own(_) => snap_to_mask(pitch, target.pitch_class_mask()),
            // 元のコードの構成音だけを寄せる (経過音は動かさない)。
            Resolved::Placed(placed) if placed.contains(pitch) => snap_to_mask(pitch, target.pitch_class_mask()),
            Resolved::Placed(_) => pitch,
        }),
        ChordFollow::Scale => {
            let to = chord_scale_mask(target, key);
            Some(match resolve(basis, target) {
                Resolved::Unchanged => pitch,
                Resolved::Own(_) => snap_to_mask(pitch, to),
                // 元のコードのスケールの音だけを寄せる (スケールの外の音は動かさない)。
                Resolved::Placed(placed) if (chord_scale_mask(placed, key) >> (pitch % 12)) & 1 == 1 => {
                    snap_to_mask(pitch, to)
                }
                Resolved::Placed(_) => pitch,
            })
        }
    }
}

/// **演奏入力のリアルタイム変換** (r.md #157 / #159 Q2、Cubase の Live Input): 弾いた音 `pitch` を今のコード
/// `target` へ寄せた鍵盤。寄せるのは最寄りのコード音 / スケールだけで、平行移動 / ベースは弾いた音のまま
/// (弾いた音に書かれたときのコードは無い = 基準は今のコード)。聴いた音と録れる音はこれで揃える。
#[must_use]
pub fn live_pitch(mode: ChordFollow, pitch: u8, target: &Chord, key: Option<&ScaleChange>) -> u8 {
    match mode {
        ChordFollow::Narrow => snap_to_mask(pitch, target.pitch_class_mask()),
        ChordFollow::Scale => snap_to_mask(pitch, chord_scale_mask(target, key)),
        ChordFollow::Off | ChordFollow::Parallel | ChordFollow::Bass => pitch,
    }
}

/// MIDI の content (ノート列) が **自分で弾いている和音** の区間 (content-local 拍、`crate::chord_detect::own_chords`)。
/// engine の索引 (`SongIndex`) と off-RT の読み口 (VOICEVOX / SMF / GUI) が同じこれを通す。ミュートしたノートは
/// 数えない。
#[must_use]
pub fn note_own_chords(notes: &[crate::model::Note]) -> Vec<SourceChord> {
    let detect: Vec<crate::chord_detect::DetectNote> = notes
        .iter()
        .filter(|n| !n.muted)
        .map(|n| crate::chord_detect::DetectNote { start_beat: n.start_beat, len_beats: n.duration_beats, pitch: n.pitch })
        .collect();
    crate::chord_detect::own_chords(&detect)
}

/// トラック `track` が鳴らす MIDI の content (アレンジのクリップとランチャーのセル) の自分の和音を `out` へ足す
/// (既にある content は検出し直さない)。engine の索引 (`SongIndex`) と off-RT の読み口
/// (`crate::transpose::TrackTranspose`) が同じこれで集める。
pub fn collect_own_chords(
    song: &Song,
    track: &crate::model::Track,
    out: &mut std::collections::HashMap<crate::model::ContentId, Vec<SourceChord>>,
) {
    let clips = track.clips.iter().chain(track.session_clips.iter().map(|c| &c.clip));
    for clip in clips {
        if out.contains_key(&clip.content_id) {
            continue;
        }
        if let Some(notes) = song.clip_contents.get(&clip.content_id).and_then(|c| c.notes()) {
            out.insert(clip.content_id, note_own_chords(notes));
        }
    }
}

/// クリップの記録 `basis` (note id 昇順) のうち note `note_id` が書かれたときのコード。RT 安全 (二分探索)。
#[must_use]
pub fn note_basis_at(basis: &[NoteChordBasis], note_id: u32) -> Option<&Chord> {
    basis.binary_search_by_key(&note_id, |b| b.note_id).ok().map(|i| &basis[i].chord)
}

/// ノート 1 つの基準を引く材料 ([`TrackChordFollow::key`])。
#[derive(Debug, Clone, Copy)]
pub struct NoteSource<'a> {
    /// content の自分の和音 ([`note_own_chords`]、content-local 拍)。
    pub own: &'a [SourceChord],
    /// クリップの記録 (`Clip::chord_basis.notes`)。
    pub basis: &'a [NoteChordBasis],
    pub note_id: u32,
    /// ノートの開始 (content-local 拍)。
    pub start_beat: f64,
}

/// 自分の和音が箱の境界の直前で始まるとき、次の箱の和音として扱う幅 (拍)。食って入る和音 (小節の頭の
/// 半拍前から弾く) を前の箱のコードへ寄せない。
const ANTICIPATION_BEATS: f64 = 0.5;

/// トラック 1 本の追従の読み口 (off-RT と RT の両方)。箱の無い位置と追従しないトラックは書いた音のまま。
///
/// engine は `SongIndex` に焼いた実効モードから [`TrackChordFollow::with_mode`] で作り、オフライン
/// (VOICEVOX / SMF / GUI) は [`TrackChordFollow::of`] で作る。
#[derive(Debug, Clone, Copy)]
pub struct TrackChordFollow<'a> {
    song: &'a Song,
    mode: ChordFollow,
}

impl<'a> TrackChordFollow<'a> {
    /// `track_id` のトラックの読み口 (実効モード = [`Song::track_chord_follow`])。
    #[must_use]
    pub fn of(song: &'a Song, track_id: u32) -> Self {
        Self { song, mode: song.track_chord_follow(track_id) }
    }

    /// 実効モードを渡して作る (engine: 索引に焼いた値)。
    #[must_use]
    pub fn with_mode(song: &'a Song, mode: ChordFollow) -> Self {
        Self { song, mode }
    }

    /// 追従しない読み口 (焼き込み = プロジェクトの中に作るものは書いた音)。
    #[must_use]
    pub fn written(song: &'a Song) -> Self {
        Self { song, mode: ChordFollow::Off }
    }

    #[must_use]
    pub fn mode(&self) -> ChordFollow {
        self.mode
    }

    /// この読み口が音を変えうるか (`false` なら呼び出し側は評価を丸ごと飛ばせる)。
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.mode.is_off() && !self.song.chords.is_empty()
    }

    /// 曲の拍 `song_beat` で始まるノート `pitch` の鳴らす鍵盤 (書いた音の空間)。
    ///
    /// 基準 ([`Basis`]): ノートの頭で鳴っている自分の和音、無ければノートの頭の箱の範囲でいちばん長く鳴っている
    /// 自分の和音 (ベース音のあとに和音を刻む伴奏のベース音も和音のパートとして一緒に動く)、と、クリップの記録。
    #[must_use]
    pub fn key(&self, pitch: u8, song_beat: f64, src: &NoteSource<'_>) -> Option<u8> {
        if self.mode.is_off() {
            return Some(pitch);
        }
        let Some((ev, basis)) = self.note_basis(song_beat, src) else { return Some(pitch) };
        // 境界の直前で次の箱を引いたら、Key もその箱の頭で引く (同じ拍にある Key 変更の前の Key と組ませない)。
        follow_pitch(self.mode, pitch, &ev.chord, basis, self.song.scale_at(song_beat.max(ev.start_beat)))
    }

    /// 曲の拍 `song_beat` で始まるノートが従う箱と基準 ([`Self::key`] の前半)。箱が無ければ `None`。
    #[must_use]
    pub fn note_basis<'s>(&self, song_beat: f64, src: &NoteSource<'s>) -> Option<(&'a ChordEvent, Basis<'s>)> {
        let onset_box = self.song.chord_at_onset(song_beat)?;
        // content-local 拍 + `to_song` = 曲の拍 (このノートの置かれた位置での換算)。
        let to_song = song_beat - src.start_beat;
        let (own, ev) = match crate::chord_detect::own_chord_at(src.own, src.start_beat) {
            Some(span) => {
                let (a, b) = (span.start_beat + to_song, span.start_beat + span.len_beats + to_song);
                (Some(&span.chord), self.anticipated(onset_box, a, b))
            }
            None => {
                let (a, b) = (onset_box.start_beat - to_song, onset_box.end_beat() - to_song);
                (crate::chord_detect::longest_own_in(src.own, a, b).map(|s| &s.chord), onset_box)
            }
        };
        Some((ev, Basis { own, placed: note_basis_at(src.basis, src.note_id) }))
    }

    /// 自分の和音の区間 `[a, b)` (曲の拍) が従う箱: 頭の箱 `onset_box`。ただし区間が次の箱の頭の直前
    /// ([`ANTICIPATION_BEATS`] 以内) で始まり、次の箱に掛かる長さが頭の箱に掛かる長さ以上なら次の箱 (食って入る和音)。
    fn anticipated(&self, onset_box: &'a ChordEvent, a: f64, b: f64) -> &'a ChordEvent {
        let end = onset_box.end_beat();
        if b <= end || end - a > ANTICIPATION_BEATS {
            return onset_box;
        }
        match self.song.chord_at_onset(end) {
            Some(next) if next.id != onset_box.id && b.min(next.end_beat()) - end >= end - a => next,
            _ => onset_box,
        }
    }

    /// 演奏入力 (弾いた音 `pitch`、曲の拍 `song_beat` = playhead) の鳴らす / 録る鍵盤 ([`live_pitch`])。
    #[must_use]
    pub fn live_key(&self, pitch: u8, song_beat: f64) -> u8 {
        if self.mode.is_off() {
            return pitch;
        }
        let Some(ev) = self.song.chord_at_onset(song_beat) else { return pitch };
        live_pitch(self.mode, pitch, &ev.chord, self.song.scale_at(song_beat.max(ev.start_beat)))
    }

    /// 曲の拍 `song_beat` でのオーディオの移動量 (半音)。`basis` は呼び出し側が素材から引いたもの
    /// (engine は箱ごとに compile 時に決めた値、`crate::model::chord_basis`)。
    #[must_use]
    pub fn shift(&self, song_beat: f64, basis: Basis<'_>) -> i32 {
        if self.mode.is_off() {
            return 0;
        }
        let Some(ev) = self.song.chord_at(song_beat) else { return 0 };
        follow_shift(self.mode, &ev.chord, basis, self.song.scale_at(song_beat))
    }
}

#[cfg(test)]
mod tests;
