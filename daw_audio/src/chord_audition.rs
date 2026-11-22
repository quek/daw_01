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
//!   鍵盤は track scratch の [`ChordAuditionVoices`] (固定長、確保なし) が持ち、停止 / ループ / seek
//!   ([`queue_off`] と窓の不連続) で止める。
//!
//! 鍵盤は **書いた音** (追従しない) に曲の移調 (r.md #130、呼び出し側が解いた `transpose`) だけ足す。

use common::model::{Song, Voicing};
use common::process_data::MAX_EVENTS;

use crate::sequencer::{NoteTransition, TimedNoteEvent};

/// 1 コードで鳴らす鍵盤の上限 (voicing は最大でもベース + 7 和音 + テンション)。
pub const MAX_CHORD_VOICES: usize = 12;

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
    /// 試聴トラックの index (`song.tracks` の位置。同じ便の Song と組でしか使えない)。`None` = 鳴らさない。
    pub track_idx: Option<u32>,
    /// `start_beat` 昇順・非交差。
    pub chords: Vec<PlannedChord>,
}

impl ChordAuditionPlan {
    /// Song のコードを `voicing` で鍵盤にした計画。試聴トラック `track_id` が居なければ空。
    #[must_use]
    pub fn build(song: Option<&Song>, track_id: Option<u32>, voicing: Voicing) -> ChordAuditionPlan {
        let (Some(song), Some(track_id)) = (song, track_id) else { return ChordAuditionPlan::default() };
        let Some(track_idx) = song.tracks.iter().position(|t| t.id == track_id).and_then(|i| u32::try_from(i).ok())
        else {
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
                let mut keys = [0u8; MAX_CHORD_VOICES];
                let n = pitches.len().min(MAX_CHORD_VOICES);
                keys[..n].copy_from_slice(&pitches[..n]);
                #[allow(clippy::cast_possible_truncation)]
                PlannedChord { start_beat: c.start_beat, end_beat: c.end_beat(), keys, n: n as u8 }
            })
            .collect();
        ChordAuditionPlan { track_idx: Some(track_idx), chords }
    }
}

/// recv loop (off-RT) が持つ試聴の設定と、組んだ計画のキャッシュ (`ProjectCtl::chord_audition`)。
#[derive(Debug, Default)]
pub struct ChordAuditionCtl {
    track_id: Option<u32>,
    voicing: Voicing,
    /// 計画を組んだ Song (`Weak` なので、別の Song が同じ番地に来て取り違えることはない)。
    built_for: std::sync::Weak<Song>,
    plan: std::sync::Arc<ChordAuditionPlan>,
}

impl ChordAuditionCtl {
    /// `AudioCommand::SetChordAudition`。次の便 ([`Self::plan_for`]) で組み直す。
    pub fn set(&mut self, track_id: Option<u32>, voicing: Voicing) {
        if (self.track_id, self.voicing) != (track_id, voicing) {
            self.track_id = track_id;
            self.voicing = voicing;
            self.built_for = std::sync::Weak::new();
        }
    }

    /// 便に載せる計画。Song か設定が変わったときだけ組み直す (同じ Song の再配送では同じ `Arc`)。
    pub fn plan_for(&mut self, song: Option<&std::sync::Arc<Song>>) -> std::sync::Arc<ChordAuditionPlan> {
        let same = match song {
            Some(s) => self.built_for.upgrade().is_some_and(|b| std::sync::Arc::ptr_eq(&b, s)),
            None => self.plan.track_idx.is_none(),
        };
        if !same {
            self.plan = std::sync::Arc::new(ChordAuditionPlan::build(song.map(|s| &**s), self.track_id, self.voicing));
            self.built_for = song.map_or_else(std::sync::Weak::new, std::sync::Arc::downgrade);
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
    /// 送った鍵盤 (移調込み)。voice id は `VOICE_ID_BASE + slot`。
    sounding: [u8; MAX_CHORD_VOICES],
    n: u8,
    /// 鳴らしているコードの開始拍と、書いた音の鍵盤 (計画が差し替わっても同じコードなら鳴らし直さない)。
    chord_start: f64,
    chord_keys: [u8; MAX_CHORD_VOICES],
    chord_n: u8,
    transpose: i32,
    /// 次の窓の先頭として期待する拍 (違えばループ / seek で飛んだ = 鳴っている音を止める)。
    next_beat: f64,
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

    fn off_all(&mut self, time: u32, out: &mut Vec<TimedNoteEvent>) {
        for slot in 0..usize::from(self.n) {
            if out.len() < MAX_EVENTS {
                #[allow(clippy::cast_possible_truncation)]
                let note_id = VOICE_ID_BASE + slot as u32;
                out.push(TimedNoteEvent { time, event: NoteTransition::Off { note_id, key: self.sounding[slot] } });
            }
        }
        self.n = 0;
    }

    fn on(&mut self, c: &PlannedChord, transpose: i32, time: u32, out: &mut Vec<TimedNoteEvent>) {
        let mut n = 0usize;
        for &p in c.keys() {
            let Some(key) = common::transpose::sounding_key(p, transpose) else { continue };
            if out.len() >= MAX_EVENTS {
                break;
            }
            #[allow(clippy::cast_possible_truncation)]
            let note_id = VOICE_ID_BASE + n as u32;
            out.push(TimedNoteEvent { time, event: NoteTransition::On { note_id, key, velocity: VELOCITY } });
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
/// RT 安全: 確保なし (`out` は `MAX_EVENTS` で打ち切り、計画は二分探索)。
#[allow(clippy::too_many_arguments)]
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
    // ループの巻き戻し / seek: 期待した位置から 1 sample 以上ずれていたら、鳴っている音を先頭で止める。
    if voices.is_sounding() && (playhead_beats - voices.next_beat).abs() > one_sample {
        voices.off_all(0, out);
    }
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
    voices.next_beat = end_beat;
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

/// 停止 / ループ端などの一括消音: 鳴っている試聴音の Off を次の buffer の先頭に予約する
/// (`crate::mixer::queue_all_notes_off` から)。
pub fn queue_off(voices: &mut ChordAuditionVoices, pending_offs: &mut Vec<(u32, u8)>) {
    for slot in 0..usize::from(voices.n) {
        if pending_offs.len() < pending_offs.capacity() {
            #[allow(clippy::cast_possible_truncation)]
            pending_offs.push((VOICE_ID_BASE + slot as u32, voices.sounding[slot]));
        }
    }
    voices.n = 0;
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

    fn render(plan: &ChordAuditionPlan, voices: &mut ChordAuditionVoices, beat: f64, frames: u32) -> Vec<(u32, bool, u8)> {
        let mut out = Vec::with_capacity(MAX_EVENTS);
        emit(plan, voices, beat, 100.0, frames, 0, &mut out);
        out.iter()
            .map(|e| match e.event {
                NoteTransition::On { key, .. } => (e.time, true, key),
                NoteTransition::Off { key, .. } => (e.time, false, key),
            })
            .collect()
    }

    /// 箱の頭で鳴らし、次の箱の頭で鳴らし直し、隙間の手前で止める。途中から再生すれば鳴っている箱を拾い、
    /// ループで巻き戻れば止めてから鳴らす。試聴トラックが居なければ計画は空。
    #[test]
    fn chords_sound_on_their_boxes_and_stop_at_gaps_and_jumps() {
        let s = song();
        assert_eq!(ChordAuditionPlan::build(Some(&s), Some(99), Voicing::Basic), ChordAuditionPlan::default());
        let plan = ChordAuditionPlan::build(Some(&s), Some(7), Voicing::Basic);
        assert_eq!(plan.track_idx, Some(0));
        let mut v = ChordAuditionVoices::default();
        // 拍 0..5 (100 sample / 拍): C の On (4 音)、拍 4 で C の Off → A の On。
        let ev = render(&plan, &mut v, 0.0, 500);
        assert_eq!(ev.iter().filter(|e| e.0 == 0 && e.1).count(), 4);
        assert_eq!(ev.iter().filter(|e| e.0 == 400).map(|e| e.1).collect::<Vec<_>>(), vec![false, false, false, false, true, true, true, true]);
        // 拍 5..7: A は拍 6 で終わり、次は拍 8 → 拍 6 で止める。
        let ev = render(&plan, &mut v, 5.0, 200);
        assert!(ev.iter().all(|e| e.0 == 100 && !e.1) && ev.len() == 4, "{ev:?}");
        // ループで拍 1 へ戻る: 鳴っていなくても C を拾い直す (先頭で On)。
        let ev = render(&plan, &mut v, 1.0, 100);
        assert_eq!(ev.iter().filter(|e| e.0 == 0 && e.1).count(), 4);
        // 続けて拍 2 から: 同じ C は鳴らし直さない。
        assert!(render(&plan, &mut v, 2.0, 100).is_empty());
        // 停止の一括消音で予約し、鳴っていない状態に戻る。
        let mut offs = Vec::with_capacity(16);
        queue_off(&mut v, &mut offs);
        assert_eq!(offs.len(), 4);
        assert!(offs.iter().all(|(id, _)| *id >= VOICE_ID_BASE));
    }
}
