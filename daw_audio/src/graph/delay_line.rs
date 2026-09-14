//! Sample-accurate stereo delay line for plugin-delay compensation
//! (PR3). Capacity is fixed at construction time; the audio thread
//! never resizes the underlying ring buffer.

#![allow(dead_code)]

/// Stereo ring buffer used by `NodeOp::ApplyDelay`.
///
/// `step` writes the input samples into the ring and reads back samples
/// `delay` slots earlier — i.e. the output for sample `i` is the input
/// for sample `i - delay` (or zero before the ring fills).
pub struct DelayLine {
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    write: usize,
    capacity: usize,
}

impl DelayLine {
    /// Allocate a delay line with `capacity` samples per channel.
    /// `capacity == 0` produces a no-op pass-through.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf_l: vec![0.0; capacity],
            buf_r: vec![0.0; capacity],
            write: 0,
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Push `n = min(len of all four slices)` samples through the ring,
    /// reading back each output sample with `delay` samples of latency.
    /// Caller is responsible for keeping `delay <= capacity - 1`; values
    /// beyond that are clamped (logged once at compile time, not here).
    pub fn step(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
        delay: usize,
    ) {
        if self.capacity == 0 {
            let n = in_l.len().min(in_r.len()).min(out_l.len()).min(out_r.len());
            out_l[..n].copy_from_slice(&in_l[..n]);
            out_r[..n].copy_from_slice(&in_r[..n]);
            return;
        }
        let n = in_l.len().min(in_r.len()).min(out_l.len()).min(out_r.len());
        let cap = self.capacity;
        let d = delay.min(cap - 1);
        for i in 0..n {
            self.buf_l[self.write] = in_l[i];
            self.buf_r[self.write] = in_r[i];
            let read = (self.write + cap - d) % cap;
            out_l[i] = self.buf_l[read];
            out_r[i] = self.buf_r[read];
            self.write = (self.write + 1) % cap;
        }
    }

    pub fn reset(&mut self) {
        self.buf_l.fill(0.0);
        self.buf_r.fill(0.0);
        self.write = 0;
    }

    /// Schedule 再 compile 間の状態移送 (`Schedule::adopt_state_from`)。`self` は compile 直後 (ゼロ)。
    /// capacity (= 補償 delay 長 + 1) が一致すれば ring の内容と write cursor を `old` と交換する
    /// (`mem::swap` のポインタ交換なので RT スレッド上で alloc/free が発生しない)。delay 長が変わっていれば
    /// [`Self::carry_history_from`] で手元の過去を写す (ゼロのまま始めると、新しい遅延の長さぶん無音が挟まる)。
    pub fn adopt(&mut self, old: &mut DelayLine) {
        if self.capacity != old.capacity {
            self.carry_history_from(old);
            return;
        }
        std::mem::swap(&mut self.buf_l, &mut old.buf_l);
        std::mem::swap(&mut self.buf_r, &mut old.buf_r);
        std::mem::swap(&mut self.write, &mut old.write);
    }

    /// 容量の違う `old` が持っている過去のうち新しい `min(容量)` サンプルを (古い順に) この line の頭へ写し、
    /// 書き込み位置をその直後に置く。遅延が変わって line を差し替えても、手元にある過去はそのまま読み出せる
    /// (写さないと新しい遅延の長さぶん無音が挟まる)。`old` は **走っていた** line であること (止まっていた line の
    /// 中身は古い音)。`self` は確保直後 (ゼロ) であること。RT で呼ぶ: 確保せず高々 `self.capacity` 回ぶん写すだけ。
    pub fn carry_history_from(&mut self, old: &DelayLine) {
        let n = old.capacity.min(self.capacity);
        if n == 0 {
            return;
        }
        // `old.write` が最古、その 1 つ前が最新。新しい `n` サンプルは `old.write + (old.capacity - n)` から。
        let start = (old.write + old.capacity - n) % old.capacity;
        for (dst, src) in [(&mut self.buf_l, &old.buf_l), (&mut self.buf_r, &old.buf_r)] {
            let first = (old.capacity - start).min(n);
            dst[..first].copy_from_slice(&src[start..start + first]);
            dst[first..n].copy_from_slice(&src[..n - first]);
        }
        self.write = n % self.capacity;
    }

    /// `step` の in-place 版:`l` / `r` 1 組のスライスを入力でも出力でも
    /// 兼用する。 audio engine の post-dispatch では track の scratch
    /// (`TrackScratch::track_l/r`) を **そのまま** 遅延線に通したいので、
    /// 別バッファを毎呼出しで確保するわけにはいかず in-place が必要。
    ///
    /// 各 sample で「ring に書き込む → ring の `delay` サンプル前を読む
    /// → `l[i]` / `r[i]` に書き戻す」 順なので、 同じスライスを使っても
    /// 当該サンプル以外の入力を破壊しない。
    pub fn step_in_place(&mut self, l: &mut [f32], r: &mut [f32], delay: usize) {
        if self.capacity == 0 {
            return;
        }
        let n = l.len().min(r.len());
        let cap = self.capacity;
        let d = delay.min(cap - 1);
        for i in 0..n {
            self.buf_l[self.write] = l[i];
            self.buf_r[self.write] = r[i];
            let read = (self.write + cap - d) % cap;
            l[i] = self.buf_l[read];
            r[i] = self.buf_r[read];
            self.write = (self.write + 1) % cap;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_capacity_passes_through() {
        let mut dl = DelayLine::with_capacity(0);
        let in_l = [1.0, 2.0, 3.0];
        let in_r = [4.0, 5.0, 6.0];
        let mut out_l = [0.0; 3];
        let mut out_r = [0.0; 3];
        dl.step(&in_l, &in_r, &mut out_l, &mut out_r, 0);
        assert_eq!(out_l, in_l);
        assert_eq!(out_r, in_r);
    }

    #[test]
    fn delay_one_buffers_input_by_one_sample() {
        let mut dl = DelayLine::with_capacity(8);
        let in_l = [1.0, 2.0, 3.0, 4.0];
        let in_r = [-1.0, -2.0, -3.0, -4.0];
        let mut out_l = [0.0; 4];
        let mut out_r = [0.0; 4];
        dl.step(&in_l, &in_r, &mut out_l, &mut out_r, 1);
        // First output is from the not-yet-written ring slot (zero);
        // subsequent outputs lag by one.
        assert_eq!(out_l, [0.0, 1.0, 2.0, 3.0]);
        assert_eq!(out_r, [0.0, -1.0, -2.0, -3.0]);
    }

    #[test]
    fn delay_clamps_to_capacity_minus_one() {
        let mut dl = DelayLine::with_capacity(2);
        let mut out_l = [0.0; 4];
        let mut out_r = [0.0; 4];
        // Asking for 99 samples of delay clamps to capacity - 1 = 1.
        dl.step(
            &[1.0, 2.0, 3.0, 4.0],
            &[1.0, 2.0, 3.0, 4.0],
            &mut out_l,
            &mut out_r,
            99,
        );
        assert_eq!(out_l, [0.0, 1.0, 2.0, 3.0]);
    }

    /// 遅延 2 で流している途中に line を容量 8 へ差し替え、遅延を 3 に伸ばす: 直前の出力 (4 サンプル前の入力) の
    /// 続きは、差し替え前の line に残っている過去から出る。
    #[test]
    fn 大きい_line_へ差し替えても手元の過去を読み出せる() {
        let mut old = DelayLine::with_capacity(3);
        let (mut l, mut r) = ([1.0, 2.0, 3.0, 4.0, 5.0], [0.0; 5]);
        old.step_in_place(&mut l, &mut r, 2);
        assert_eq!(l, [0.0, 0.0, 1.0, 2.0, 3.0]);

        let mut grown = DelayLine::with_capacity(8);
        grown.carry_history_from(&old);
        let (mut l, mut r) = ([6.0, 7.0, 8.0], [0.0; 3]);
        grown.step_in_place(&mut l, &mut r, 3);
        assert_eq!(l, [3.0, 4.0, 5.0], "入力 6 の 3 サンプル前 = 3 から続く (無音を挟まない)");

        // 縮めるときは新しい側だけが残る: 遅延 1 (容量 2) では入力 9 の 1 つ前 = 8。
        let mut shrunk = DelayLine::with_capacity(2);
        shrunk.carry_history_from(&grown);
        let (mut l, mut r) = ([9.0, 10.0], [0.0; 2]);
        shrunk.step_in_place(&mut l, &mut r, 1);
        assert_eq!(l, [8.0, 9.0]);
    }
}
