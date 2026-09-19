//! 内蔵 Reverb / Delay が使う遅延リングと小数遅延の読み出し
//! (`docs/plan_rmd_134_135_reverb_delay.md` §2 / §7.3)。
//!
//! **確保は `new` だけ** (compile 時 = off-RT に呼ばれる)。[`Ring::reset`] は中身を 0 に
//! するだけで容量を保つ — bypass の OFF → ON は RT 上で起きるので、ここで確保すると
//! 再生スレッドがヒープに触る。
//!
//! 読み出しの規約: [`Ring::peek`] は「`d` サンプル前に書いた値」で、**書く前に読む**
//! (`d = 1` が直前に書いた値)。素のディレイ (`y[n] = x[n-L]`) も 2 乗算オールパスの
//! `v[n-L]` も、同じ「読んでから書く」順で書ける。

/// 1 ch ぶんの遅延リング。
pub struct Ring {
    buf: Vec<f32>,
    /// 次に書く位置。
    write: usize,
}

impl Ring {
    /// 容量 `cap` サンプルのリングを確保する (**off-RT 専用**)。`cap` は 4 以上に切り上げる
    /// (4 点補間が常に成立する最小)。
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self { buf: vec![0.0; cap.max(4)], write: 0 }
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// 中身を無音に戻す (容量は保つ = 確保しない)。
    pub fn reset(&mut self) {
        self.buf.fill(0.0);
        self.write = 0;
    }

    /// `d` サンプル前に書いた値 (`d = 1` が直前)。`d` は `1..=capacity` に丸める。
    #[inline]
    #[must_use]
    pub fn peek(&self, d: usize) -> f32 {
        let cap = self.buf.len();
        let d = d.clamp(1, cap);
        self.buf[(self.write + cap - d) % cap]
    }

    /// 小数サンプル遅延の読み出し (Catmull-Rom 4 点 3 次)。
    ///
    /// 線形補間は時変ローパスを持ち込み、フィードバックループで繰り返すたびに累積する。
    /// Thiran オールパスは状態を持つので遅延時間を速く動かせない (JUCE `juce_DelayLine.h`
    /// の DelayLineInterpolationTypes の doc)。状態を持たず低域減衰も小さい 4 点 3 次が
    /// 変調付きディレイの唯一の選択。
    #[inline]
    #[must_use]
    pub fn peek_frac(&self, d: f32) -> f32 {
        let cap = self.buf.len();
        #[allow(clippy::cast_precision_loss)]
        let max = (cap - 2) as f32;
        let d = if d.is_finite() { d.clamp(2.0, max) } else { 2.0 };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let i = d as usize;
        #[allow(clippy::cast_precision_loss)]
        let t = d - i as f32;
        // p1 / p2 が補間区間の両端、p0 / p3 がその外側 (時間をさかのぼる向きに index が増える)。
        let p0 = self.peek(i - 1);
        let p1 = self.peek(i);
        let p2 = self.peek(i + 1);
        let p3 = self.peek(i + 2);
        catmull_rom(p0, p1, p2, p3, t)
    }

    /// 1 サンプル書く。
    #[inline]
    pub fn push(&mut self, x: f32) {
        let cap = self.buf.len();
        self.buf[self.write] = flush_denormal(x);
        self.write += 1;
        if self.write >= cap {
            self.write = 0;
        }
    }

    /// 遅延 `d` (整数) の素通しディレイ 1 サンプル: `y[n] = x[n-d]`。
    #[inline]
    pub fn step(&mut self, x: f32, d: usize) -> f32 {
        let y = self.peek(d);
        self.push(x);
        y
    }

    /// 2 乗算ラティス型オールパス 1 サンプル (Dattorro §1.3.3)。
    /// `v[n] = x[n] - c·v[n-d]` / `y[n] = v[n-d] + c·v[n]`。遅延は小数可。
    #[inline]
    pub fn step_allpass(&mut self, x: f32, d: f32, c: f32) -> f32 {
        let vd = self.peek_frac(d);
        let v = x - c * vd;
        let y = vd + c * v;
        self.push(v);
        y
    }
}

/// Catmull-Rom (= Lagrange 3 次) の 4 点補間。`t` は `p1`→`p2` の位置 (0..=1)。
#[inline]
#[must_use]
fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
    let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
    let c = -0.5 * p0 + 0.5 * p2;
    ((a * t + b) * t + c) * t + p1
}

/// 非正規化数と非有限を 0 に落とす (`common::dsp` と同じ規則)。減衰の尾で必須。
#[inline]
#[must_use]
pub fn flush_denormal(x: f32) -> f32 {
    if x.is_finite() && x.abs() > 1e-25 { x } else { 0.0 }
}

/// 1 極ローパス 1 サンプル: `y += (x - y) * a`。`a = 1` で素通し。
#[inline]
pub fn one_pole(state: &mut f32, x: f32, a: f32) -> f32 {
    *state = flush_denormal(*state + (x - *state) * a);
    *state
}

/// カットオフ周波数 (Hz) → 1 極ローパスの係数 `a`。
#[inline]
#[must_use]
pub fn one_pole_coeff(freq_hz: f32, sample_rate: f32) -> f32 {
    if sample_rate <= 0.0 || !freq_hz.is_finite() || freq_hz <= 0.0 {
        return 1.0;
    }
    let a = 1.0 - (-std::f32::consts::TAU * freq_hz / sample_rate).exp();
    a.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peek_は_d_サンプル前に書いた値を返す() {
        let mut r = Ring::new(8);
        for i in 1..=5 {
            #[allow(clippy::cast_precision_loss)]
            r.push(i as f32);
        }
        assert_eq!(r.peek(1), 5.0);
        assert_eq!(r.peek(3), 3.0);
        // 範囲外は端に丸める (RT で panic しない)。
        assert_eq!(r.peek(0), r.peek(1));
        assert_eq!(r.peek(999), r.peek(8));
    }

    #[test]
    fn step_は_d_サンプルの素通しディレイ() {
        let mut r = Ring::new(16);
        let input = [1.0, 2.0, 3.0, 4.0, 5.0];
        let got: Vec<f32> = input.iter().map(|&x| r.step(x, 2)).collect();
        // 最初の 2 サンプルはリングが空なので 0。
        assert_eq!(got, vec![0.0, 0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn 小数遅延は整数点で整数遅延と一致する() {
        let mut r = Ring::new(64);
        for i in 0..32 {
            #[allow(clippy::cast_precision_loss)]
            r.push((i as f32 * 0.37).sin());
        }
        for d in 4..20 {
            #[allow(clippy::cast_precision_loss)]
            let frac = r.peek_frac(d as f32);
            assert!((frac - r.peek(d)).abs() < 1e-6, "d={d} frac={frac} int={}", r.peek(d));
        }
    }

    #[test]
    fn reset_は容量を保つ() {
        let mut r = Ring::new(1000);
        r.push(1.0);
        let cap = r.capacity();
        r.reset();
        assert_eq!(r.capacity(), cap, "RT 上で呼ぶので確保し直してはいけない");
        assert_eq!(r.peek(1), 0.0);
    }

    #[test]
    fn オールパスは振幅を保ち_遅延だけ与える() {
        // 白色に近い入力を通しても、エネルギーが増えも減りもしない (|H| = 1)。
        let mut r = Ring::new(256);
        let mut state = 12345_u32;
        let (mut ein, mut eout) = (0.0_f64, 0.0_f64);
        for _ in 0..8192 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            #[allow(clippy::cast_precision_loss)]
            let x = (state >> 8) as f32 / 8_388_608.0 - 1.0;
            let y = r.step_allpass(x, 37.0, 0.7);
            ein += f64::from(x) * f64::from(x);
            eout += f64::from(y) * f64::from(y);
        }
        let ratio = eout / ein;
        assert!((ratio - 1.0).abs() < 0.02, "allpass のエネルギー比 {ratio}");
    }
}
