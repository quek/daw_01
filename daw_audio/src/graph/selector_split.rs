//! r.md #114: Parallel の `Split::Selector` (Bitwig Instrument / FX Selector) の RT 部。
//!
//! 入力 (audio + MIDI) を **アクティブな 1 chain だけ** に配る。 chain `k` の出力 = 入力 × 重み
//! `w_k`、 `w_k` は「選ばれている = 1 / 他 = 0」へ **1 sample あたり `1 / fade_samples`** の
//! 傾きで追う (線形クロスフェード)。 全 chain が同じ傾きで動くので、 切替の途中でも
//! `Σ w_k = 1` (= 空 chain を並べれば和は入力そのまま)。 どの chain がアクティブかは
//! **sample ごと** に `pos_ramp` (automation + 変調の per-sample 値) から決める
//! (`Split::select_index` が GUI と共通の写像)。
//!
//! MIDI: note-on はその時刻に選ばれている chain へ、 note-off は **その note の note-on を受けた
//! chain** へ (Bitwig: "each sounding note continues until its output is silent")。 宛先は
//! `ParallelBegin` で event ごとに 1 回だけ決め ([`Self::midi_dest`])、 `ChainBegin` は自分宛の
//! event を写すだけ ([`Self::copy_midi_for`])。 鳴っている note の表は固定長
//! ([`MAX_HELD`]、 溢れたら最古を捨てる = その note-off は現在の chain へ)。
//!
//! RT 規約: 確保・ロック・I/O なし。 buffer は compile 時に chain 数ぶん確保する。 状態
//! (重み / 鳴っている note) は再 compile を跨いで `adopt_state_from` で移送する (捨てると
//! 編集のたびにフェードし直し + note-off が迷子になる)。

use common::model::Split;

use crate::mixer::{MAX_EVENTS, MAX_FRAMES};
use crate::sequencer::{NoteTransition, TimedNoteEvent};

/// 同時に鳴っている note の表の容量 (note-on を受けた chain を覚える)。
const MAX_HELD: usize = 256;

/// 鳴っている note 1 つ: どの chain が note-on を受けたか。
#[derive(Clone, Copy, Default)]
struct HeldNote {
    note_id: u32,
    key: u8,
    chain: u8,
}

/// Parallel 1 つぶんの Selector。 出力は [`Self::output`] で読む。
pub struct SelectorSplit {
    n_chains: usize,
    /// chain ごとの現在の重み (`0..=1`)。
    weights: Vec<f32>,
    /// `[chain][ch]` の出力 (MAX_FRAMES)。
    out: Vec<[Vec<f32>; 2]>,
    /// アクティブ chain の位置 (`0..=1`) の per-sample ramp (automation + 変調)。 呼び側が
    /// `process` の前に埋める。
    pub pos_ramp: Vec<f32>,
    /// sample ごとの選ばれている chain の index。
    sel: Vec<u8>,
    /// 鳴っている note → 受けた chain (`held[..held_len]` が有効)。
    held: [HeldNote; MAX_HELD],
    held_len: usize,
    /// 入力 MIDI の event ごとの宛先 chain (`in_midi` と同じ並び)。
    midi_dest: Vec<u8>,
}

impl SelectorSplit {
    pub fn new(n_chains: usize) -> Self {
        let n_chains = n_chains.min(usize::from(u8::MAX));
        Self {
            n_chains,
            weights: vec![0.0; n_chains],
            out: (0..n_chains)
                .map(|_| [vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES]])
                .collect(),
            pos_ramp: vec![0.5; MAX_FRAMES],
            sel: vec![0; MAX_FRAMES],
            held: [HeldNote::default(); MAX_HELD],
            held_len: 0,
            midi_dest: Vec::with_capacity(MAX_EVENTS),
        }
    }

    /// chain ごとの現在の重み (テスト用)。
    #[cfg(test)]
    pub fn weights(&self) -> &[f32] {
        &self.weights
    }

    /// `in_l/r[..n]` を chain ごとの出力へ配り、 `in_midi` の宛先を決める。 位置は `pos_ramp`
    /// (呼び側が先に埋める)、 クロスフェードは `fade_ms` (0 = 1 sample で切替)。
    pub fn process(
        &mut self,
        sample_rate: u32,
        fade_ms: f32,
        in_l: &[f32],
        in_r: &[f32],
        in_midi: &[TimedNoteEvent],
        n: usize,
    ) {
        let n = n.min(MAX_FRAMES).min(in_l.len()).min(in_r.len());
        if n == 0 || self.n_chains == 0 {
            return;
        }
        let fade_samples = if fade_ms.is_finite() && fade_ms > 0.0 {
            fade_ms * 1e-3 * sample_rate.max(1) as f32
        } else {
            0.0
        };
        let step = if fade_samples >= 1.0 { 1.0 / fade_samples } else { 1.0 };
        for i in 0..n {
            self.sel[i] = Split::select_index(self.pos_ramp[i], self.n_chains) as u8;
        }
        for i in 0..n {
            let mut sum = 0.0f32;
            for (k, w) in self.weights.iter_mut().enumerate() {
                let target = if usize::from(self.sel[i]) == k { 1.0 } else { 0.0 };
                *w += (target - *w).clamp(-step, step);
                sum += *w;
            }
            // 重みは各 chain が独立に目標を追うので、 3 chain 以上をフェードの途中で乗り換えると
            // 和が 1 を割る (A=0.5, B=0.5 → C: A, B が下がる間 C はまだ小さい)。 sample ごとに
            // Σw で正規化して和を常に 1 に保つ。 compile 直後 (全部 0) も最初の sample で選択
            // chain が 1 になる (無音からのフェードインにしない)。
            let norm = if sum > 1e-6 { 1.0 / sum } else { 0.0 };
            for (k, out) in self.out.iter_mut().enumerate() {
                let w = self.weights[k] * norm;
                out[0][i] = in_l[i] * w;
                out[1][i] = in_r[i] * w;
            }
        }
        self.route_midi(in_midi, n);
    }

    /// event ごとの宛先 chain を決める (note-on = その時刻の選択、 note-off = note-on を受けた chain)。
    fn route_midi(&mut self, in_midi: &[TimedNoteEvent], n: usize) {
        self.midi_dest.clear();
        for ev in in_midi.iter().take(self.midi_dest.capacity()) {
            let t = (ev.time as usize).min(n - 1);
            let current = self.sel[t];
            let dest = match ev.event {
                NoteTransition::On { note_id, key, .. } => {
                    self.remember_held(HeldNote { note_id, key, chain: current });
                    current
                }
                NoteTransition::Off { note_id, key } => self.take_held(note_id, key).unwrap_or(current),
            };
            self.midi_dest.push(dest);
        }
    }

    /// 鳴っている note を覚える。 同じ `(note_id, key)` が居れば上書き (再トリガ)、 満杯なら最古を
    /// 捨てる。
    fn remember_held(&mut self, h: HeldNote) {
        if let Some(slot) = self.held[..self.held_len]
            .iter_mut()
            .find(|x| x.note_id == h.note_id && x.key == h.key)
        {
            *slot = h;
            return;
        }
        if self.held_len == MAX_HELD {
            self.held.copy_within(1..MAX_HELD, 0);
            self.held_len -= 1;
        }
        self.held[self.held_len] = h;
        self.held_len += 1;
    }

    /// note-off の相手を表から外して、 note-on を受けた chain を返す。 `(note_id, key)` で引き、
    /// 無ければ `key` だけで引く (id を知らない送り手への保険。 停止時の flush も note-on と同じ id)。
    fn take_held(&mut self, note_id: u32, key: u8) -> Option<u8> {
        let held = &self.held[..self.held_len];
        let idx = held
            .iter()
            .position(|x| x.note_id == note_id && x.key == key)
            .or_else(|| held.iter().position(|x| x.key == key))?;
        let chain = self.held[idx].chain;
        self.held.copy_within(idx + 1..self.held_len, idx);
        self.held_len -= 1;
        Some(chain)
    }

    /// chain `k` 宛の MIDI を `dst` に写す (`in_midi` は直前の `process` に渡したもの)。
    pub fn copy_midi_for(&self, k: u8, in_midi: &[TimedNoteEvent], dst: &mut Vec<TimedNoteEvent>) {
        dst.clear();
        let cap = dst.capacity();
        for (ev, dest) in in_midi.iter().zip(&self.midi_dest) {
            if *dest == k && dst.len() < cap {
                dst.push(*ev);
            }
        }
    }

    /// k 番目の出力 (L, R)。 chain 数を超える k は `None`。
    pub fn output(&self, k: u8) -> Option<(&[f32], &[f32])> {
        let o = self.out.get(usize::from(k))?;
        Some((o[0].as_slice(), o[1].as_slice()))
    }

    /// 再 compile 跨ぎの状態移送 (RT 上、 コピーのみ)。 chain 数が変わっていれば重なる範囲だけ
    /// (増えた chain は重み 0 から、 選ばれていればフェードインする)。
    pub fn adopt_state_from(&mut self, old: &Self) {
        let n = self.n_chains.min(old.n_chains);
        self.weights[..n].copy_from_slice(&old.weights[..n]);
        for k in 0..n {
            for ch in 0..2 {
                self.out[k][ch].copy_from_slice(&old.out[k][ch]);
            }
        }
        // 消えた chain が受けていた note の note-off は現在の chain へ (`take_held` の fallback
        // ではなく表から外す: 存在しない chain 番号を宛先にすると誰にも届かない)。
        self.held_len = 0;
        for h in &old.held[..old.held_len] {
            if usize::from(h.chain) < self.n_chains {
                self.held[self.held_len] = *h;
                self.held_len += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(time: u32, note_id: u32, key: u8) -> TimedNoteEvent {
        TimedNoteEvent { time, event: NoteTransition::On { note_id, key, velocity: 1.0 } }
    }
    fn off(time: u32, note_id: u32, key: u8) -> TimedNoteEvent {
        TimedNoteEvent { time, event: NoteTransition::Off { note_id, key } }
    }
    /// event ごとの宛先 chain (`copy_midi_for` を chain ごとに引いて元の並びへ戻す)。
    fn dests(s: &SelectorSplit, midi: &[TimedNoteEvent]) -> Vec<u8> {
        let mut by_event = vec![u8::MAX; midi.len()];
        let mut total = 0;
        for k in 0..s.n_chains as u8 {
            let mut dst = Vec::with_capacity(MAX_EVENTS);
            s.copy_midi_for(k, midi, &mut dst);
            total += dst.len();
            for ev in dst {
                let i = midi.iter().position(|e| e.time == ev.time).unwrap();
                by_event[i] = k;
            }
        }
        assert_eq!(total, midi.len(), "全 event がどこか 1 つの chain へ");
        by_event
    }

    /// 切替の途中でも重みの和は 1 (空 chain の和 = 入力)、 フェード後は選ばれた chain だけが
    /// 入力を受ける。 fade 0 は 1 sample で切り替わる。
    #[test]
    fn crossfade_weights_are_complementary_and_settle_on_the_selected_chain() {
        let mut s = SelectorSplit::new(3);
        let one = vec![1.0f32; 16];
        // 最初は chain 1 (pos 0.5 → index 1)。 fade 0: 1 sample で確定。
        s.process(48_000, 0.0, &one, &one, &[], 16);
        assert_eq!(s.weights(), &[0.0, 1.0, 0.0]);
        // chain 2 へ、 8 sample のフェード (8 / 48 ms)。
        s.pos_ramp[..16].fill(0.9);
        s.process(48_000, 8.0 / 48.0, &one, &one, &[], 16);
        let (o1, _) = s.output(1).unwrap();
        let (o2, _) = s.output(2).unwrap();
        for i in 0..16 {
            let sum: f32 = (0..3).map(|k| s.output(k).unwrap().0[i]).sum();
            assert!((sum - 1.0).abs() < 1e-5, "i={i} sum={sum}");
        }
        assert!((o2[3] - 0.5).abs() < 1e-5, "4 sample 目で半分: {}", o2[3]);
        assert!((o1[7]).abs() < 1e-5 && (o2[7] - 1.0).abs() < 1e-5, "8 sample で完了");
        assert_eq!(s.weights(), &[0.0, 0.0, 1.0]);
        // sample 途中の切替 (pos ramp が buffer 内で変わる) も sample 単位で追う。
        s.pos_ramp[..8].fill(0.9);
        s.pos_ramp[8..16].fill(0.1);
        s.process(48_000, 0.0, &one, &one, &[], 16);
        let (o0, _) = s.output(0).unwrap();
        assert_eq!((o0[7], o0[8]), (0.0, 1.0));
    }

    /// note-on はその時刻に選ばれている chain へ、 note-off は note-on を受けた chain へ。
    /// id が違う note-off は key で引く。 知らない note-off は現在の chain へ。
    #[test]
    fn note_off_follows_the_chain_that_received_the_note_on() {
        let mut s = SelectorSplit::new(2);
        let z = vec![0.0f32; 16];
        s.pos_ramp[..16].fill(0.1); // chain 0
        let midi = [on(0, 100, 60), on(2, 101, 64)];
        s.process(48_000, 0.0, &z, &z, &midi, 16);
        assert_eq!(dests(&s, &midi), vec![0, 0]);

        s.pos_ramp[..16].fill(0.9); // chain 1
        let midi = [on(0, 102, 67), off(4, 100, 60), off(5, 0, 64), off(6, 999, 72)];
        s.process(48_000, 0.0, &z, &z, &midi, 16);
        assert_eq!(dests(&s, &midi), vec![1, 0, 0, 1], "off は on を受けた chain 0 へ、 未知の off は現在の chain 1 へ");
        assert_eq!(s.held_len, 1, "鳴っているのは 102 だけ");

        // buffer 内で切り替わる: 時刻ごとの選択で配る。
        s.pos_ramp[..8].fill(0.1);
        s.pos_ramp[8..16].fill(0.9);
        let midi = [on(3, 103, 60), on(12, 104, 61), off(15, 102, 67)];
        s.process(48_000, 0.0, &z, &z, &midi, 16);
        assert_eq!(dests(&s, &midi), vec![0, 1, 1]);
    }

    /// 再 compile 跨ぎ: 重みと鳴っている note を引き継ぐ。 消えた chain 宛の note は表から外す。
    #[test]
    fn adopt_state_keeps_weights_and_held_notes_within_the_new_chain_count() {
        let mut old = SelectorSplit::new(3);
        let z = vec![0.0f32; 4];
        old.pos_ramp[..4].fill(0.9); // chain 2
        old.process(48_000, 0.0, &z, &z, &[on(0, 1, 60)], 4);
        old.pos_ramp[..4].fill(0.1); // chain 0
        old.process(48_000, 0.0, &z, &z, &[on(0, 2, 61)], 4);
        assert_eq!(old.held_len, 2);

        let mut new = SelectorSplit::new(2);
        new.adopt_state_from(&old);
        assert_eq!(new.weights(), &[1.0, 0.0]);
        assert_eq!(new.held_len, 1, "chain 2 宛の note 1 は落ちる");
        new.pos_ramp[..4].fill(0.9); // chain 1
        let midi = [off(0, 2, 61), off(1, 1, 60)];
        new.process(48_000, 0.0, &z, &z, &midi, 4);
        assert_eq!(dests(&new, &midi), vec![0, 1]);
    }
}
