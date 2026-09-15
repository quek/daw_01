//! L / R 2 本のバイクワッドを 1 本のレジスタで同時に進める (内蔵 EQ / Tone EQ / Comp の検出フィルタ /
//! Parallel の帯域分割)。
//!
//! 1 サンプルの計算と丸めは [`BiquadState::process`] と**ビット単位で同じ**: 差分方程式の演算順をそのまま写し、
//! 「非有限か `|y| <= 1e-25` なら 0」を分岐なしの比較マスクで行う。サンプルごとの分岐と、L と R を交互に
//! 回す往復が消えるぶん速い。x86_64 は SSE (baseline)、それ以外は同じ規則の scalar。

use super::{Biquad, BiquadState};

/// L / R の 1 サンプル。
#[derive(Debug, Clone, Copy)]
pub struct Stereo(imp::Lanes);

impl Stereo {
    #[inline]
    #[must_use]
    pub fn new(l: f32, r: f32) -> Self {
        Self(imp::Lanes::new(l, r))
    }

    #[inline]
    #[must_use]
    pub fn l(self) -> f32 {
        self.0.l()
    }

    #[inline]
    #[must_use]
    pub fn r(self) -> f32 {
        self.0.r()
    }
}

/// L / R 2 本ぶんの遅延と係数をレジスタに載せたもの。buffer の頭で [`Self::load`]、サンプルごとに
/// [`Self::tick`]、終わりに [`Self::store`] で [`BiquadState`] へ書き戻す。
#[derive(Debug, Clone, Copy)]
pub struct StereoBiquad {
    x1: imp::Lanes,
    x2: imp::Lanes,
    y1: imp::Lanes,
    y2: imp::Lanes,
    b0: imp::Lanes,
    b1: imp::Lanes,
    b2: imp::Lanes,
    a1: imp::Lanes,
    a2: imp::Lanes,
}

impl StereoBiquad {
    #[inline]
    #[must_use]
    pub fn load(l: &BiquadState, r: &BiquadState, c: &Biquad) -> Self {
        Self {
            x1: imp::Lanes::new(l.x1, r.x1),
            x2: imp::Lanes::new(l.x2, r.x2),
            y1: imp::Lanes::new(l.y1, r.y1),
            y2: imp::Lanes::new(l.y2, r.y2),
            b0: imp::Lanes::splat(c.b0),
            b1: imp::Lanes::splat(c.b1),
            b2: imp::Lanes::splat(c.b2),
            a1: imp::Lanes::splat(c.a1),
            a2: imp::Lanes::splat(c.a2),
        }
    }

    /// 1 サンプル進める ([`BiquadState::process`] を L / R に 1 回ずつ掛けたのと同じ値)。
    #[inline]
    #[must_use]
    pub fn tick(&mut self, x: Stereo) -> Stereo {
        let x = x.0;
        let ff = self.b0.mul(x).add(self.b1.mul(self.x1)).add(self.b2.mul(self.x2));
        let y = ff.sub(self.a1.mul(self.y1)).sub(self.a2.mul(self.y2)).snap();
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        Stereo(y)
    }

    #[inline]
    pub fn store(&self, l: &mut BiquadState, r: &mut BiquadState) {
        (l.x1, r.x1) = (self.x1.l(), self.x1.r());
        (l.x2, r.x2) = (self.x2.l(), self.x2.r());
        (l.y1, r.y1) = (self.y1.l(), self.y1.r());
        (l.y2, r.y2) = (self.y2.l(), self.y2.r());
    }

    /// `l[..n]` / `r[..n]` を in-place で 1 段通す。
    pub fn run(state: &mut [BiquadState; 2], c: &Biquad, l: &mut [f32], r: &mut [f32], n: usize) {
        let n = n.min(l.len()).min(r.len());
        let [sl, sr] = state;
        let mut s = Self::load(sl, sr, c);
        for (a, b) in l[..n].iter_mut().zip(&mut r[..n]) {
            let y = s.tick(Stereo::new(*a, *b));
            (*a, *b) = (y.l(), y.r());
        }
        s.store(sl, sr);
    }
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use core::arch::x86_64::{
        __m128, _mm_add_ps, _mm_and_ps, _mm_andnot_ps, _mm_cmpgt_ps, _mm_cmplt_ps, _mm_cvtss_f32, _mm_mul_ps,
        _mm_set_ps, _mm_set1_ps, _mm_shuffle_ps, _mm_sub_ps,
    };

    // SAFETY (この module の `unsafe` 全部): 呼ぶのは SSE 命令だけで、SSE は x86_64 の baseline なので
    // どの x86_64 CPU でも存在する。どれもポインタを取らない値演算。

    /// レーン 0 = L、1 = R (2 / 3 は使わない)。
    #[derive(Debug, Clone, Copy)]
    pub struct Lanes(__m128);

    impl Lanes {
        #[inline]
        pub fn new(l: f32, r: f32) -> Self {
            Self(unsafe { _mm_set_ps(0.0, 0.0, r, l) })
        }

        #[inline]
        pub fn splat(v: f32) -> Self {
            Self(unsafe { _mm_set1_ps(v) })
        }

        #[inline]
        pub fn l(self) -> f32 {
            unsafe { _mm_cvtss_f32(self.0) }
        }

        #[inline]
        pub fn r(self) -> f32 {
            unsafe { _mm_cvtss_f32(_mm_shuffle_ps(self.0, self.0, 0b01_01_01_01)) }
        }

        #[inline]
        pub fn add(self, o: Self) -> Self {
            Self(unsafe { _mm_add_ps(self.0, o.0) })
        }

        #[inline]
        pub fn sub(self, o: Self) -> Self {
            Self(unsafe { _mm_sub_ps(self.0, o.0) })
        }

        #[inline]
        pub fn mul(self, o: Self) -> Self {
            Self(unsafe { _mm_mul_ps(self.0, o.0) })
        }

        /// 有限で `|v| > 1e-25` のレーンだけ残し、それ以外は +0 (NaN は比較が偽になるので落ちる)。
        #[inline]
        pub fn snap(self) -> Self {
            unsafe {
                let abs = _mm_andnot_ps(_mm_set1_ps(-0.0), self.0);
                let keep =
                    _mm_and_ps(_mm_cmpgt_ps(abs, _mm_set1_ps(1e-25)), _mm_cmplt_ps(abs, _mm_set1_ps(f32::INFINITY)));
                Self(_mm_and_ps(self.0, keep))
            }
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod imp {
    #[derive(Debug, Clone, Copy)]
    pub struct Lanes([f32; 2]);

    impl Lanes {
        #[inline]
        pub fn new(l: f32, r: f32) -> Self {
            Self([l, r])
        }

        #[inline]
        pub fn splat(v: f32) -> Self {
            Self([v, v])
        }

        #[inline]
        pub fn l(self) -> f32 {
            self.0[0]
        }

        #[inline]
        pub fn r(self) -> f32 {
            self.0[1]
        }

        #[inline]
        pub fn add(self, o: Self) -> Self {
            Self([self.0[0] + o.0[0], self.0[1] + o.0[1]])
        }

        #[inline]
        pub fn sub(self, o: Self) -> Self {
            Self([self.0[0] - o.0[0], self.0[1] - o.0[1]])
        }

        #[inline]
        pub fn mul(self, o: Self) -> Self {
            Self([self.0[0] * o.0[0], self.0[1] * o.0[1]])
        }

        #[inline]
        pub fn snap(self) -> Self {
            let s = |y: f32| if y.is_finite() && y.abs() > 1e-25 { y } else { 0.0 };
            Self([s(self.0[0]), s(self.0[1])])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L / R 同時の段は、L と R に [`BiquadState::process`] を 1 サンプルずつ掛けたのと全サンプル・全状態でビット一致する
    /// (極小値へ減衰する区間 / ±inf / NaN / -0.0 を含む)。
    #[test]
    fn 同時に進めた段は_1_本ずつ回したのとビット一致する() {
        let coeffs = [
            Biquad::peaking(48_000.0, 1_000.0, 0.7, 9.0),
            Biquad::high_pass(48_000.0, 80.0, 0.707),
            Biquad::low_shelf(48_000.0, 200.0, 0.707, -12.0),
        ];
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        };
        let mut l: Vec<f32> = (0..4_000).map(|_| noise()).collect();
        let mut r: Vec<f32> = (0..4_000).map(|_| noise() * 0.5).collect();
        // 無音への減衰 (極小値の丸めを踏む)、非有限、負のゼロ。
        l[1_000..3_000].fill(0.0);
        r[1_500..3_500].fill(0.0);
        (l[3_100], r[3_200], l[3_300], r[3_301]) = (f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.0);
        for c in &coeffs {
            let mut scalar = [BiquadState::default(); 2];
            let want_l: Vec<f32> = l.iter().map(|&x| scalar[0].process(c, x)).collect();
            let want_r: Vec<f32> = r.iter().map(|&x| scalar[1].process(c, x)).collect();
            let mut stereo = [BiquadState::default(); 2];
            let (mut got_l, mut got_r) = (l.clone(), r.clone());
            // buffer を跨いで状態を持ち越す。
            for (cl, cr) in got_l.chunks_mut(384).zip(got_r.chunks_mut(384)) {
                let n = cl.len();
                StereoBiquad::run(&mut stereo, c, cl, cr, n);
            }
            let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&got_l), bits(&want_l));
            assert_eq!(bits(&got_r), bits(&want_r));
            let state_bits = |s: &BiquadState| [s.x1, s.x2, s.y1, s.y2].map(f32::to_bits);
            assert_eq!(stereo.map(|s| state_bits(&s)), scalar.map(|s| state_bits(&s)));
        }
    }
}
