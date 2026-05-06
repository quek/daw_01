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

    /// `step` の in-place 版: `l` / `r` 1 組のスライスを入力でも出力でも
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
}
