//! 再生中のコードの試聴 (r.md #157 Q7「再生中も鳴らす」、`docs/plan_chord_track.md` §3.4)。
//!
//! コードトラックを **試聴トラックのノート源** にする。試聴トラックと voicing は「聴き方の都合」なので
//! Song に載せず `AudioCommand::SetChordAudition` で受け、計画 ([`ChordAuditionPlan`]) は bundle の
//! snapshot として RT へ運び、live の [`crate::graph::native::NativeIo`] にだけ載せる — 書き出し / bounce /
//! ラウドネス解析は `NativeIo::default()` (計画なし) なので試聴は焼き込まれない (不変条件 6)。
//!
//! - **off-RT** ([`ChordAuditionPlan::build`]): Song のコードを `common::chord_voicing::voice_progression`
//!   (GUI の試聴・ノート化と同じ関数) で鍵盤にした列。
//! - **RT** ([`emit`]): 試聴トラックの行で、buffer の窓に掛かるコードの On / Off を MIDI バスへ足す。鳴っている
//!   鍵盤は track scratch の [`ChordAuditionVoices`] (固定長、確保なし) が持つ。止める口は 3 つ:
//!   - 箱の終わり / 次の箱の頭 ([`emit`])。
//!   - 一括消音 ([`ChordAuditionVoices::release_all`]): 停止 / ループ / seek / 行の無効化 (`mixer::queue_notes_off`)。
//!     **再生位置の飛びは必ずこれを通る** (`ProjectRt::queue_all_notes_off` の doc) ので、窓の頭を予測して飛びを
//!     推し量らない — テンポのランプ / 段差で予測が外れ、同じ箱を毎 buffer 鳴らし直していた。
//!   - 計画の差し替え ([`hand_over`]): 試聴トラックを替えた / 無くした (前のトラックで止める)、トラックの並びが
//!     変わった (行は位置で持つので、鳴っている状態を移った先の行へ移す)。
//!
//! 止める Off は行の MIDI バスの頭 ([`drain_release`]) で出し、バスが満杯なら次の buffer へ持ち越す (容量で捨てない)。
//!
//! 鍵盤は **書いた音** (追従しない) に曲の移調 (r.md #130、呼び出し側が解いた `transpose`) だけ足す。

use common::model::{ChordEvent, Song, Voicing};
use common::process_data::MAX_EVENTS;
use common::song_index::SongIndex;

use crate::sequencer::{NoteTransition, TimedNoteEvent};

/// 1 コードで鳴らす鍵盤の上限 (どの voicing もこれを超えない — `common::chord_voicing` のテストが保証する)。
pub const MAX_CHORD_VOICES: usize = common::chord_voicing::MAX_CHORD_KEYS;

// 止める予約は slot の bit 集合 (`ChordAuditionVoices::release`) で持つ。
const _: () = assert!(MAX_CHORD_VOICES <= u16::BITS as usize);

/// 試聴の voice id の帯。sequencer の台帳 (`sing_note_id`、`[0, 1 << 28)`) と talk の読み上げ
/// (`TALK_EVENT_ID_BASE` = `1 << 28` から `1 << 26` 幅) の上、`i32` の非負の範囲。
const VOICE_ID_BASE: u32 = (1 << 28) | (1 << 27);

/// 試聴の発音の強さ (GUI の `PREVIEW_VELOCITY` = 100 と同じ)。
const VELOCITY: f64 = 100.0 / 127.0;

/// コード 1 つぶんの鍵盤 (書いた音の空間)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlannedChord {
    pub start_beat: f64,
    pub end_beat: f64,
    pub keys: [u8; MAX_CHORD_VOICES],
    pub n: u8,
}

impl PlannedChord {
    fn keys(&self) -> &[u8] {
        &self.keys[..usize::from(self.n)]
    }
}

/// 試聴の計画 (off-RT で組み、bundle で RT へ運ぶ)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChordAuditionPlan {
    /// 試聴トラックの安定 id (計画が差し替わったとき、前の試聴トラックが今どの行に居るかを引く。[`hand_over`])。
    pub track_id: Option<u32>,
    /// 試聴トラックの index (`song.tracks` の位置。同じ便の Song と組でしか使えない)。`None` = 鳴らさない。
    pub track_idx: Option<u32>,
    /// `start_beat` 昇順・非交差。
    pub chords: Vec<PlannedChord>,
}

impl ChordAuditionPlan {
    /// Song のコードを `voicing` で鍵盤にした計画。試聴トラック `track_id` が居なければ空。
    #[must_use]
    pub fn build(song: Option<&Song>, track_id: Option<u32>, voicing: Voicing) -> ChordAuditionPlan {
        let (Some(song), Some(track_idx)) = (song, audition_row(song, track_id)) else {
            return ChordAuditionPlan::default();
        };
        let voiced = common::chord_voicing::voice_progression(
            song.chords.iter().map(|c| &c.chord),
            voicing,
            common::chord_voicing::DEFAULT_VOICING_BASE,
        );
        let chords = song
            .chords
            .iter()
            .zip(voiced)
            .filter(|(c, _)| c.len_beats > 0.0 && c.start_beat.is_finite())
            .map(|(c, pitches)| {
                // voicing は `MAX_CHORD_KEYS` を超えない (common のテスト)。`min` は配列の境界の防御だけ。
                debug_assert!(pitches.len() <= MAX_CHORD_VOICES, "{pitches:?}");
                let mut keys = [0u8; MAX_CHORD_VOICES];
                let n = pitches.len().min(MAX_CHORD_VOICES);
                keys[..n].copy_from_slice(&pitches[..n]);
                #[allow(clippy::cast_possible_truncation)]
                PlannedChord { start_beat: c.start_beat, end_beat: c.end_beat(), keys, n: n as u8 }
            })
            .collect();
        ChordAuditionPlan { track_id, track_idx: Some(track_idx), chords }
    }
}

/// 試聴トラック `track_id` の `song.tracks` の位置 (居なければ `None`)。
fn audition_row(song: Option<&Song>, track_id: Option<u32>) -> Option<u32> {
    let (song, id) = (song?, track_id?);
    song.tracks.iter().position(|t| t.id == id).and_then(|i| u32::try_from(i).ok())
}

/// recv loop (off-RT) が持つ試聴の設定と、組んだ計画のキャッシュ (`ProjectCtl::chord_audition`)。
#[derive(Debug, Default)]
pub struct ChordAuditionCtl {
    track_id: Option<u32>,
    voicing: Voicing,
    /// 今の計画を組んだ入力 (試聴トラックの位置と Song のコード列)。`None` = まだ組んでいない / 設定が変わった。
    /// 計画はこの 2 つと設定だけで決まるので、Song の `Arc` が替わっても入力が同じなら同じ計画 (同じ `Arc`) を送る
    /// — 値だけの便 (ミキサーのスライダー、`ProjectCtl::update_song_values`) は毎回 Song を複製する。
    built_from: Option<(Option<u32>, Vec<ChordEvent>)>,
    plan: std::sync::Arc<ChordAuditionPlan>,
}

impl ChordAuditionCtl {
    /// `AudioCommand::SetChordAudition`。次の便 ([`Self::plan_for`]) で組み直す。
    pub fn set(&mut self, track_id: Option<u32>, voicing: Voicing) {
        if (self.track_id, self.voicing) != (track_id, voicing) {
            self.track_id = track_id;
            self.voicing = voicing;
            self.built_from = None;
        }
    }

    /// 便に載せる計画。設定か入力 (試聴トラックの位置 / コード列) が変わったときだけ組み直す。
    pub fn plan_for(&mut self, song: Option<&std::sync::Arc<Song>>) -> std::sync::Arc<ChordAuditionPlan> {
        let song = song.map(|s| &**s);
        let row = audition_row(song, self.track_id);
        // 試聴トラックが居なければ計画はコードに依らず空。
        let chords = song.filter(|_| row.is_some()).map_or(&[][..], |s| s.chords.as_slice());
        let same = self.built_from.as_ref().is_some_and(|(r, c)| *r == row && c.as_slice() == chords);
        if !same {
            self.plan = std::sync::Arc::new(ChordAuditionPlan::build(song, self.track_id, self.voicing));
            self.built_from = Some((row, chords.to_vec()));
        }
        std::sync::Arc::clone(&self.plan)
    }
}

/// sequencer の出力と同じ並び (時刻昇順、同じ時刻は Off → On。`sequencer::collect_events_for_buffer` 末尾の契約)。
pub fn sort_bus(out: &mut [TimedNoteEvent]) {
    out.sort_unstable_by_key(|e| (e.time, u8::from(matches!(e.event, NoteTransition::On { .. }))));
}

/// 試聴トラックの行で鳴らしているコード (track scratch に置く。固定長、RT で確保しない)。
#[derive(Debug, Clone, Copy, Default)]
pub struct ChordAuditionVoices {
    /// 送った鍵盤 (移調込み)。voice id は `VOICE_ID_BASE + slot`。slot `0..n` が鳴っている。
    sounding: [u8; MAX_CHORD_VOICES],
    n: u8,
    /// Off をまだ出せていない slot (bit) と、その鍵盤。一括消音の予約と、バスが満杯で出せなかった Off。
    /// 出し切るまで同じ voice id の On は出さない (`on`)。
    release: u16,
    release_keys: [u8; MAX_CHORD_VOICES],
    /// 鳴らしているコードの開始拍と、書いた音の鍵盤 (計画が差し替わっても同じコードなら鳴らし直さない)。
    chord_start: f64,
    chord_keys: [u8; MAX_CHORD_VOICES],
    chord_n: u8,
    transpose: i32,
}

/// バスにもう 1 つ積めるか (`MAX_EVENTS` と確保量の小さい方。RT で再確保しない)。
fn has_room(out: &[TimedNoteEvent], capacity: usize) -> bool {
    out.len() < capacity.min(MAX_EVENTS)
}

#[allow(clippy::cast_possible_truncation)]
fn voice_id(slot: usize) -> u32 {
    VOICE_ID_BASE + slot as u32
}

impl ChordAuditionVoices {
    fn is_sounding(&self) -> bool {
        self.n > 0
    }

    fn same_chord(&self, c: &PlannedChord, transpose: i32) -> bool {
        self.is_sounding()
            && self.transpose == transpose
            && (self.chord_start - c.start_beat).abs() < 1e-9
            && self.chord_keys[..usize::from(self.chord_n)] == *c.keys()
    }

    /// 鳴っている音を全部止める予約にする (Off は次の [`drain_release`] が frame 0 で出す)。一括消音
    /// (`mixer::queue_notes_off`: 停止 / ループ / seek / 行の無効化) と [`hand_over`] から。RT 安全。
    pub fn release_all(&mut self) {
        for slot in 0..usize::from(self.n) {
            self.release |= 1 << slot;
            self.release_keys[slot] = self.sounding[slot];
        }
        self.n = 0;
    }

    /// 鳴っている音を `time` で止める。バスに積めない Off は予約に回す (次の buffer の頭で出す)。
    fn off_all(&mut self, time: u32, out: &mut Vec<TimedNoteEvent>) {
        let cap = out.capacity();
        for slot in 0..usize::from(self.n) {
            let key = self.sounding[slot];
            if has_room(out, cap) {
                out.push(TimedNoteEvent { time, event: NoteTransition::Off { note_id: voice_id(slot), key } });
            } else {
                self.release |= 1 << slot;
                self.release_keys[slot] = key;
            }
        }
        self.n = 0;
    }

    fn on(&mut self, c: &PlannedChord, transpose: i32, time: u32, out: &mut Vec<TimedNoteEvent>) {
        // 止め切っていない voice id があるうちは鳴らさない (同じ id の On が Off より先に届く)。予約が残るのは
        // バスが満杯の buffer だけなので、次の buffer で出し切ってから鳴る。
        if self.release != 0 {
            self.n = 0;
            return;
        }
        let cap = out.capacity();
        // 和音の全部を積めないなら、この buffer では鳴らさない。途中まで鳴らすと「同じ箱が鳴っている」と判定され、
        // 残りの鍵盤が箱の終わりまで鳴らない。鳴っていない状態のままにして、次の buffer の窓の頭で鳴らす。
        let wanted = c.keys().iter().filter(|&&p| common::transpose::sounding_key(p, transpose).is_some()).count();
        if cap.min(MAX_EVENTS).saturating_sub(out.len()) < wanted {
            self.n = 0;
            return;
        }
        let mut n = 0usize;
        for &p in c.keys() {
            let Some(key) = common::transpose::sounding_key(p, transpose) else { continue };
            if !has_room(out, cap) {
                break;
            }
            out.push(TimedNoteEvent { time, event: NoteTransition::On { note_id: voice_id(n), key, velocity: VELOCITY } });
            self.sounding[n] = key;
            n += 1;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            self.n = n as u8;
        }
        self.chord_start = c.start_beat;
        self.chord_keys = c.keys;
        self.chord_n = c.n;
        self.transpose = transpose;
    }
}

/// 試聴トラックの行の、この buffer の窓 `[playhead_beats, +frames)` のコードの On / Off を `out` へ足す
/// (再生中だけ呼ぶ)。同じ時刻は Off → On の順に積む (呼び出し側が sequencer の出力と合わせて並べ直す)。
///
/// 再生位置の飛び (seek / ループ / 停止) は見ない — 飛びは必ず一括消音 ([`ChordAuditionVoices::release_all`]) を
/// 通るので、ここへは鳴っていない状態で来て、窓の頭を覆う箱を鳴らし直す。
///
/// RT 安全: 確保なし (`out` は `MAX_EVENTS` で打ち切り、積めない Off は予約に回す。計画は二分探索)。
pub fn emit(
    plan: &ChordAuditionPlan,
    voices: &mut ChordAuditionVoices,
    playhead_beats: f64,
    samples_per_beat: f64,
    frames: u32,
    transpose: i32,
    out: &mut Vec<TimedNoteEvent>,
) {
    if !(samples_per_beat.is_finite() && samples_per_beat > 0.0) {
        return;
    }
    let one_sample = 1.0 / samples_per_beat;
    let end_beat = playhead_beats + f64::from(frames) * one_sample;
    let frame_of = |beat: f64| -> u32 {
        let f = common::timing::boundary_frame(beat - playhead_beats, samples_per_beat).max(0.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        (f as u32).min(frames.saturating_sub(1))
    };
    let first = plan.chords.partition_point(|c| c.end_beat <= playhead_beats + 1e-9);
    // 窓の先頭で鳴っているべきコード (窓より前に始まったもの)。
    let covering = plan
        .chords
        .get(first)
        .filter(|c| c.start_beat <= playhead_beats + 1e-9 && (c.end_beat - playhead_beats) >= one_sample);
    match covering {
        Some(c) if voices.same_chord(c, transpose) => {}
        Some(c) => {
            voices.off_all(0, out);
            voices.on(c, transpose, 0, out);
        }
        None => voices.off_all(0, out),
    }
    for c in plan.chords.iter().skip(first) {
        if c.start_beat >= end_beat {
            break;
        }
        if c.start_beat > playhead_beats + 1e-9 {
            let t = frame_of(c.start_beat);
            voices.off_all(t, out);
            voices.on(c, transpose, t, out);
        }
        if c.end_beat < end_beat && voices.same_chord(c, transpose) {
            // 次のコードが同じ拍で始まるならそちらの On の前の Off が止めるが、隙間なら箱の終わりで止める。
            voices.off_all(frame_of(c.end_beat), out);
        }
    }
}

/// [`emit`] を track scratch の MIDI バスへ (`graph::execute::process_track_owned` の 1 か所から、sequencer の
/// 後)。バスは sequencer が並べた後なので、足したら並べ直す。RT 安全 (バスの容量内、in-place sort)。
pub fn emit_into_bus(
    plan: &ChordAuditionPlan,
    scratch: &mut crate::mixer::TrackScratch,
    playhead_beats: f64,
    current_bpm: f32,
    sample_rate: u32,
    frames: u32,
    transpose: i32,
) {
    if current_bpm <= 0.0 {
        return;
    }
    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(current_bpm);
    let before = scratch.midi_bus_a.len();
    emit(plan, &mut scratch.chord_audition, playhead_beats, samples_per_beat, frames, transpose, &mut scratch.midi_bus_a);
    if scratch.midi_bus_a.len() != before {
        sort_bus(&mut scratch.midi_bus_a);
    }
}

/// 止める予約 (一括消音 / バスが満杯で出せなかった Off) を、行の MIDI バスの頭に frame 0 の Off として出す
/// (`graph::execute::process_track_owned`、`pending_offs` の直後 — 再生中かに依らない)。バスに積めない分は予約に
/// 残して次の buffer で出す (容量で捨てない)。RT 安全: バスの容量内の push だけ。
pub fn drain_release(voices: &mut ChordAuditionVoices, bus: &mut Vec<TimedNoteEvent>) {
    let cap = bus.capacity();
    for slot in 0..MAX_CHORD_VOICES {
        if voices.release & (1 << slot) == 0 {
            continue;
        }
        if !has_room(bus, cap) {
            return;
        }
        bus.push(TimedNoteEvent { time: 0, event: NoteTransition::Off { note_id: voice_id(slot), key: voices.release_keys[slot] } });
        voices.release &= !(1 << slot);
    }
}

/// 計画の差し替え (`ProjectRt::refresh_bundle`、新しい Song の索引と scratch の成長便を差し込んだ後): 前の試聴トラック
/// (`old`) の行で鳴っている状態を始末する。鳴っている状態は行 (位置) が持ち、行の楽器はトラックについて動くので:
/// - 前の試聴トラックが今も居る → 状態を今の行へ移す。試聴トラックのままなら鳴り続け (同じ箱を鳴らし直さない)、
///   試聴トラックでなくなったなら止める予約にする (その行の楽器へ Off が届く)。
/// - 消えた (または `carry == false` = 別のファイルを読み込んだ便。id は別の曲の名前) → 元の行で止める予約にする。
///   楽器はもう居ないはずだが、黙って捨てると同じ楽器が残る経路で鳴りっぱなしになる。知らない voice への Off は無害
///   (`mixer::silence_disabled_rows` と同じ約束。試聴の voice id は sequencer の台帳の帯と重ならない)。
///
/// RT 安全: `Copy` の状態の移動と索引の参照だけ。
pub fn hand_over(
    old: &ChordAuditionPlan,
    new: &ChordAuditionPlan,
    index: &SongIndex,
    scratch: &mut [crate::mixer::TrackScratch],
    carry: bool,
) {
    let (Some(from), Some(id)) = (old.track_idx, old.track_id) else { return };
    if carry && (old.track_id, old.track_idx) == (new.track_id, new.track_idx) {
        return;
    }
    let Some(row) = scratch.get_mut(from as usize) else { return };
    let mut voices = std::mem::take(&mut row.chord_audition);
    let now_at = index.track_pos(id).filter(|_| carry);
    if now_at.is_none() || new.track_id != Some(id) {
        voices.release_all();
    }
    if let Some(row) = scratch.get_mut(now_at.unwrap_or(from as usize)) {
        row.chord_audition = voices;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{Chord, ChordEvent, ChordKind, Track};

    fn song() -> Song {
        let mut s = Song { tracks: vec![Track { id: 7, ..Track::default() }], ..Song::default() };
        for (start, len, root) in [(0.0, 4.0, 0), (4.0, 2.0, 9), (8.0, 4.0, 7)] {
            s.place_chord(ChordEvent { id: 0, start_beat: start, len_beats: len, chord: Chord::new(root, ChordKind::Major), color: None });
        }
        s
    }

    fn transitions(out: &[TimedNoteEvent]) -> Vec<(u32, bool, u8)> {
        out.iter()
            .map(|e| match e.event {
                NoteTransition::On { key, .. } => (e.time, true, key),
                NoteTransition::Off { key, .. } => (e.time, false, key),
            })
            .collect()
    }

    fn render(plan: &ChordAuditionPlan, voices: &mut ChordAuditionVoices, beat: f64, frames: u32) -> Vec<(u32, bool, u8)> {
        let mut out = Vec::with_capacity(MAX_EVENTS);
        emit(plan, voices, beat, 100.0, frames, 0, &mut out);
        transitions(&out)
    }

    /// 箱の頭で鳴らし、次の箱の頭で鳴らし直し、隙間の手前で止める。途中から再生すれば鳴っている箱を拾い、
    /// 一括消音 (停止 / ループ / seek) の後は次の buffer の頭で止めてから鳴らし直す。試聴トラックが居なければ計画は空。
    #[test]
    fn chords_sound_on_their_boxes_and_stop_at_gaps_and_jumps() {
        let s = song();
        assert_eq!(ChordAuditionPlan::build(Some(&s), Some(99), Voicing::Basic), ChordAuditionPlan::default());
        let plan = ChordAuditionPlan::build(Some(&s), Some(7), Voicing::Basic);
        assert_eq!((plan.track_id, plan.track_idx), (Some(7), Some(0)));
        let mut v = ChordAuditionVoices::default();
        // 拍 0..5 (100 sample / 拍): C の On (4 音)、拍 4 で C の Off → A の On。
        let ev = render(&plan, &mut v, 0.0, 500);
        assert_eq!(ev.iter().filter(|e| e.0 == 0 && e.1).count(), 4);
        assert_eq!(ev.iter().filter(|e| e.0 == 400).map(|e| e.1).collect::<Vec<_>>(), vec![false, false, false, false, true, true, true, true]);
        // 拍 5..7: A は拍 6 で終わり、次は拍 8 → 拍 6 で止める。
        let ev = render(&plan, &mut v, 5.0, 200);
        assert!(ev.iter().all(|e| e.0 == 100 && !e.1) && ev.len() == 4, "{ev:?}");
        // 拍 1 から (鳴っていない状態): 窓を覆う C を先頭で拾う。続けて拍 2 から: 同じ C は鳴らし直さない。
        let ev = render(&plan, &mut v, 1.0, 100);
        assert_eq!(ev.iter().filter(|e| e.0 == 0 && e.1).count(), 4);
        assert!(render(&plan, &mut v, 2.0, 100).is_empty());
        // 一括消音: 次の buffer の頭で C の Off を出し、その窓を覆う C を鳴らし直す (Off → On)。
        v.release_all();
        let mut bus = Vec::with_capacity(MAX_EVENTS);
        drain_release(&mut v, &mut bus);
        emit(&plan, &mut v, 2.5, 100.0, 100, 0, &mut bus);
        let ev = transitions(&bus);
        assert_eq!(ev.iter().map(|e| e.1).collect::<Vec<_>>(), [false, false, false, false, true, true, true, true]);
        assert!(bus.iter().all(|e| matches!(e.event, NoteTransition::Off { note_id, .. } | NoteTransition::On { note_id, .. } if note_id >= VOICE_ID_BASE)));
    }

    /// バスの空きが和音の音数に足りない buffer では 1 音も鳴らさず、空いた次の buffer で全部鳴らす
    /// (途中まで鳴らすと「同じ箱が鳴っている」と判定され、残りの鍵盤が箱の終わりまで鳴らない)。
    #[test]
    fn a_chord_that_does_not_fit_in_the_bus_waits_for_the_next_buffer() {
        let plan = ChordAuditionPlan::build(Some(&song()), Some(7), Voicing::Basic);
        let mut v = ChordAuditionVoices::default();
        let mut bus = Vec::with_capacity(MAX_EVENTS);
        bus.extend((0..MAX_EVENTS - 2).map(|_| TimedNoteEvent { time: 0, event: NoteTransition::Off { note_id: 1, key: 1 } }));
        emit(&plan, &mut v, 0.0, 100.0, 100, 0, &mut bus);
        assert_eq!(bus.len(), MAX_EVENTS - 2, "空き 2 に 4 音の和音は積まない");
        let ev = render(&plan, &mut v, 1.0, 100);
        assert_eq!(ev.iter().filter(|e| e.0 == 0 && e.1).count(), 4, "{ev:?}");
    }

    /// テンポのランプ / 段差では、窓の頭の拍は「前の窓 + frames を窓の頭のテンポで」からずれる (刻みが積分した拍)。
    /// 同じ箱の中では鳴らし直さない (飛びの検出に予測を使っていたときは、1 BPM / buffer のランプで 448 回鳴り直した)。
    #[test]
    fn tempo_ramps_and_steps_do_not_retrigger_the_chord() {
        let plan = ChordAuditionPlan::build(Some(&song()), Some(7), Voicing::Basic);
        let (sr, frames) = (48_000.0f64, 512u32);
        // (buffer 番号, 今の BPM) → この buffer の間に進む拍と、次の buffer 頭の BPM。
        let ramp = |_: u32, bpm: f64| (f64::from(frames) * (bpm + 0.5) / (60.0 * sr), bpm + 1.0);
        let step = |n: u32, bpm: f64| match n {
            // buffer の途中 (200 frame 目) で 120 → 180 BPM。
            40 => ((200.0 * bpm + 312.0 * 180.0) / (60.0 * sr), 180.0),
            _ => (f64::from(frames) * bpm / (60.0 * sr), bpm),
        };
        let tempo: [&dyn Fn(u32, f64) -> (f64, f64); 2] = [&ramp, &step];
        for (label, advance) in ["ramp", "step"].into_iter().zip(tempo) {
            let mut v = ChordAuditionVoices::default();
            let (mut beat, mut bpm, mut ons, mut n) = (0.0f64, 120.0f64, 0, 0u32);
            while beat < 3.5 {
                let mut out = Vec::with_capacity(MAX_EVENTS);
                emit(&plan, &mut v, beat, sr * 60.0 / bpm, frames, 0, &mut out);
                ons += out.iter().filter(|e| matches!(e.event, NoteTransition::On { .. })).count();
                let (d, next) = advance(n, bpm);
                (beat, bpm, n) = (beat + d, next, n + 1);
            }
            assert_eq!(ons, 4, "{label}: C の 4 音を 1 回だけ鳴らす");
        }
    }

    /// テンションを全部載せたコードも、鍵盤を切り詰めずに鳴らす (上限は `common::chord_voicing::MAX_CHORD_KEYS`)。
    #[test]
    fn plan_keeps_every_key_of_a_full_chord() {
        use common::model::Tensions;
        let mut s = Song { tracks: vec![Track { id: 7, ..Track::default() }], ..Song::default() };
        let full = Chord { tensions: Tensions(0x1FF), ..Chord::new(0, ChordKind::Dominant7) };
        s.place_chord(ChordEvent { id: 0, start_beat: 0.0, len_beats: 4.0, chord: full, color: None });
        let plan = ChordAuditionPlan::build(Some(&s), Some(7), Voicing::Basic);
        let want = common::chord_voicing::chord_pitches(&full, Voicing::Basic, common::chord_voicing::DEFAULT_VOICING_BASE);
        assert!(want.len() > 12, "{want:?}");
        assert_eq!(plan.chords[0].keys(), want.as_slice());
        let mut v = ChordAuditionVoices::default();
        assert_eq!(render(&plan, &mut v, 0.0, 100).len(), want.len());
    }

    /// 値だけの便 (ミキサーのスライダー: Song を複製して値を変える) では組み直さず同じ計画を送る。コード / 試聴トラックの
    /// 位置 / 設定が変われば組み直す。
    #[test]
    fn value_only_songs_keep_the_same_plan() {
        use std::sync::Arc;
        let mut ctl = ChordAuditionCtl::default();
        ctl.set(Some(7), Voicing::Basic);
        let a = Arc::new(song());
        let p1 = ctl.plan_for(Some(&a));
        let mut b = (*a).clone();
        b.tracks[0].volume = 0.5;
        assert!(Arc::ptr_eq(&p1, &ctl.plan_for(Some(&Arc::new(b.clone())))), "値だけ変えた Song");
        b.chords[0].chord.root = 2;
        let p2 = ctl.plan_for(Some(&Arc::new(b.clone())));
        assert!(!Arc::ptr_eq(&p1, &p2), "コードを変えた");
        b.tracks.insert(0, Track { id: 1, ..Track::default() });
        let p3 = ctl.plan_for(Some(&Arc::new(b.clone())));
        assert_eq!(p3.track_idx, Some(1), "試聴トラックの位置が変わった");
        ctl.set(Some(7), Voicing::Piano);
        assert!(!Arc::ptr_eq(&p3, &ctl.plan_for(Some(&Arc::new(b)))), "voicing を変えた");
    }

    /// 一括消音の予約は台帳が満杯でも捨てない: `pending_offs` (台帳の全件で満杯) とは別に持ち、バスに積めない間は
    /// 持ち越して、空いた buffer の頭で出す。出し切るまでは同じ voice id の On を出さない。
    #[test]
    fn audition_offs_survive_a_full_ledger_and_a_full_bus() {
        let mut s = crate::mixer::TrackScratch::new();
        for i in 0..MAX_EVENTS {
            #[allow(clippy::cast_possible_truncation)]
            s.state.active_notes.note_on(1, i as u32, (i % 128) as u8, 0);
        }
        let plan = ChordAuditionPlan::build(Some(&song()), Some(7), Voicing::Basic);
        let mut bus = Vec::with_capacity(MAX_EVENTS);
        emit(&plan, &mut s.chord_audition, 0.0, 100.0, 100, 0, &mut bus);
        assert_eq!(bus.len(), 4);
        crate::mixer::queue_all_notes_off(std::slice::from_mut(&mut s));
        assert_eq!(s.state.pending_offs.len(), MAX_EVENTS, "台帳の Off で満杯");
        // 次の buffer: 台帳の Off でバスが満杯 → 試聴の Off は持ち越し、On も出さない。
        bus.clear();
        bus.extend(s.state.pending_offs.drain(..).map(|(note_id, key)| TimedNoteEvent { time: 0, event: NoteTransition::Off { note_id, key } }));
        drain_release(&mut s.chord_audition, &mut bus);
        emit(&plan, &mut s.chord_audition, 1.0, 100.0, 100, 0, &mut bus);
        assert_eq!(bus.len(), MAX_EVENTS, "満杯のバスには積まない (再確保しない)");
        // その次の buffer: 持ち越した Off を出してから鳴らし直す。
        bus.clear();
        drain_release(&mut s.chord_audition, &mut bus);
        emit(&plan, &mut s.chord_audition, 2.0, 100.0, 100, 0, &mut bus);
        assert_eq!(transitions(&bus).iter().map(|e| e.1).collect::<Vec<_>>(), [false, false, false, false, true, true, true, true]);
    }
}
