//! 内蔵 Reverb (Dattorro プレート)。
//!
//! Jon Dattorro, "Effect Design, Part 1: Reverberator and Other Filters",
//! *J. Audio Eng. Soc.* 45(9), 1997, pp.660-684。Fig.1 のトポロジ、Table 1 の係数、
//! Table 2 の出力タップをそのまま使う。設計の正本は
//! `docs/plan_rmd_134_135_reverb_delay.md` §5。
//!
//! 遅延メモリは [`ReverbState::new`] (compile 時 = off-RT) で **Size 最大 (200%)** の長さに
//! 確保し、Size を動かしても再確保しない — 実効の長さを index で短くするだけ。
//! [`ReverbState::reset`] も 0 埋めだけで容量を保つ (RT で呼ばれる)。

use common::dsp::{Biquad, BiquadState};
use common::model::{
    REVERB_BASE_SR, REVERB_DECAY_DIFFUSION_1, REVERB_INPUT_DIFFUSION, REVERB_MAX_PREDELAY_MS, REVERB_MAX_SIZE_PCT,
    REVERB_OUTPUT_GAIN, ReverbSettings,
};

use super::ring::{Ring, flush_denormal, one_pole, one_pole_coeff};
use super::{NativeBlock, block_len};

/// Fig.1 の遅延長 (基準 29761 Hz)。入力ディフューザ 4 段。
const IN_APF: [f32; 4] = [142.0, 107.0, 379.0, 277.0];
/// タンク左枝: 変調 APF / delay1 / APF2 / delay2。
const TANK_L: [f32; 4] = [672.0, 4453.0, 1800.0, 3720.0];
/// タンク右枝。
const TANK_R: [f32; 4] = [908.0, 4217.0, 2656.0, 3163.0];

/// Table 2 の出力タップ。`(枝, 位置)` で、枝は [`Branch`] の順。
/// 左出力が主に右枝を、右出力が主に左枝を読むのが synthetic stereo の正体 (§1.3.6)。
const TAPS_L: [(Branch, f32, f32); 7] = [
    (Branch::RDelay1, 266.0, 1.0),
    (Branch::RDelay1, 2974.0, 1.0),
    (Branch::RApf2, 1913.0, -1.0),
    (Branch::RDelay2, 1996.0, 1.0),
    (Branch::LDelay1, 1990.0, -1.0),
    (Branch::LApf2, 187.0, -1.0),
    (Branch::LDelay2, 1066.0, -1.0),
];
const TAPS_R: [(Branch, f32, f32); 7] = [
    (Branch::LDelay1, 353.0, 1.0),
    (Branch::LDelay1, 3627.0, 1.0),
    (Branch::LApf2, 1228.0, -1.0),
    (Branch::LDelay2, 2673.0, 1.0),
    (Branch::RDelay1, 2111.0, -1.0),
    (Branch::RApf2, 335.0, -1.0),
    (Branch::RDelay2, 121.0, -1.0),
];

/// 出力タップが読むライン。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Branch {
    LDelay1,
    LApf2,
    LDelay2,
    RDelay1,
    RApf2,
    RDelay2,
}

/// 変調 APF の振れ幅 (Table 1 の EXCURSION = 16 は peak-to-peak、footnote 14 の
/// 「peak excursion of about 8 samples」= 片側)。基準 29761 Hz でのサンプル数。
const EXCURSION_PEAK: f32 = 8.0;
/// 低域ダンピングのクロスオーバー (Hz)。論文には無い追加 (zita-rev1 / Surge Reverb2 が持つ
/// 帯域別減衰の最小形)。この周波数より下だけを `LfDamp` の割合で減らす。
const LF_DAMP_XOVER_HZ: f32 = 200.0;
/// ダンピング係数の上限 (1.0 にすると 1 極が閉じて音が止まる)。
const MAX_DAMPING: f32 = 0.95;

/// タンク片枝の状態。
struct Branch2 {
    mod_apf: Ring,
    delay1: Ring,
    apf2: Ring,
    delay2: Ring,
    /// ダンピングの 1 極 (高域) と低域抽出の 1 極。
    damp_state: f32,
    lo_state: f32,
    /// 直前の出力 (図八の相手枝へ渡す)。
    out: f32,
}

impl Branch2 {
    /// `base` = Fig.1 の 4 本の長さ (29761 Hz 基準)、`max_scale` = 確保に使う最大倍率。
    fn new(base: [f32; 4], max_scale: f32) -> Self {
        // 変調 APF は excursion のぶん余分に要る。
        let margin = (EXCURSION_PEAK * max_scale).ceil() as usize + 8;
        Self {
            mod_apf: Ring::new(scaled(base[0], max_scale) + margin),
            delay1: Ring::new(scaled(base[1], max_scale) + 4),
            apf2: Ring::new(scaled(base[2], max_scale) + 4),
            delay2: Ring::new(scaled(base[3], max_scale) + 4),
            damp_state: 0.0,
            lo_state: 0.0,
            out: 0.0,
        }
    }

    fn reset(&mut self) {
        self.mod_apf.reset();
        self.delay1.reset();
        self.apf2.reset();
        self.delay2.reset();
        self.damp_state = 0.0;
        self.lo_state = 0.0;
        self.out = 0.0;
    }
}

/// 1 buffer ぶんの係数 (値が変わった buffer だけ組み直す)。
#[derive(Clone, Copy, PartialEq)]
struct Coeffs {
    /// 各ラインの実効長 (Size 倍率込み、サンプル)。
    tank_l: [f32; 4],
    tank_r: [f32; 4],
    in_apf: [usize; 4],
    predelay: usize,
    excursion: f32,
    mod_inc: f32,
    decay: f32,
    dd2: f32,
    id1: f32,
    id2: f32,
    damping: f32,
    lf_damp: f32,
    lo_coeff: f32,
    bandwidth: f32,
    low_cut: Biquad,
    scale: f32,
    freeze: bool,
    width: f32,
    mix: f32,
}

pub struct ReverbState {
    pre: Ring,
    in_apf: [Ring; 4],
    left: Branch2,
    right: Branch2,
    /// 入力段の bandwidth (1 極 LP) と LowCut (バイクワッド HP)。
    bw_state: f32,
    low_cut_state: BiquadState,
    /// 変調 LFO の位相 (0..1)。L / R は quadrature (sin / cos) で相関を落とす。
    lfo_phase: f32,
    cached: Option<(ReverbSettings, f32)>,
    coeffs: Coeffs,
}

impl ReverbState {
    /// **off-RT 専用** — ここでだけ確保する。`sample_rate` はセッションのもの。
    #[must_use]
    pub fn new(sample_rate: f32) -> Self {
        let max_scale = scale_of(sample_rate, REVERB_MAX_SIZE_PCT);
        let pre_cap = (REVERB_MAX_PREDELAY_MS / 1000.0 * sample_rate.max(1.0)).ceil() as usize + 8;
        Self {
            pre: Ring::new(pre_cap),
            in_apf: [
                Ring::new(scaled(IN_APF[0], max_scale) + 4),
                Ring::new(scaled(IN_APF[1], max_scale) + 4),
                Ring::new(scaled(IN_APF[2], max_scale) + 4),
                Ring::new(scaled(IN_APF[3], max_scale) + 4),
            ],
            left: Branch2::new(TANK_L, max_scale),
            right: Branch2::new(TANK_R, max_scale),
            bw_state: 0.0,
            low_cut_state: BiquadState::default(),
            lfo_phase: 0.0,
            cached: None,
            coeffs: Coeffs::silent(),
        }
    }

    /// 無音の状態に戻す (容量は保つ = 確保しない)。
    pub fn reset(&mut self) {
        self.pre.reset();
        for a in &mut self.in_apf {
            a.reset();
        }
        self.left.reset();
        self.right.reset();
        self.bw_state = 0.0;
        self.low_cut_state = BiquadState::default();
        self.lfo_phase = 0.0;
        self.cached = None;
    }

    /// 遅延メモリの容量 (引き継ぎの可否判定 = サンプルレートが同じか)。
    #[must_use]
    pub fn capacity_key(&self) -> usize {
        self.pre.capacity()
    }

    pub(super) fn process(&mut self, s: &ReverbSettings, b: NativeBlock<'_>) -> f32 {
        let n = block_len(b.l, b.r, b.n);
        if n == 0 || b.sample_rate <= 0.0 {
            return 0.0;
        }
        if self.cached != Some((*s, b.sample_rate)) {
            self.coeffs = Coeffs::build(s, b.sample_rate);
            self.cached = Some((*s, b.sample_rate));
        }
        let c = self.coeffs;
        for i in 0..n {
            let (dry_l, dry_r) = (b.l[i], b.r[i]);
            // 入力は mono 化する (§1.3.6: このトポロジは mono 入力を前提に組まれている)。
            let mono = if c.freeze { 0.0 } else { 0.5 * (dry_l + dry_r) };
            let cut = self.low_cut_state.process(&c.low_cut, mono);
            let band = one_pole(&mut self.bw_state, cut, c.bandwidth);
            let pre = self.pre.step(band, c.predelay);

            // 入力ディフューザ 4 段 (前 2 段が id1、後ろ 2 段が id2)。
            let mut d = pre;
            for (k, apf) in self.in_apf.iter_mut().enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let len = c.in_apf[k] as f32;
                let coeff = if k < 2 { c.id1 } else { c.id2 };
                d = apf.step_allpass(d, len, coeff);
            }

            // 変調: L / R は quadrature (§1.3.7 の footnote 14)。
            let (sin_p, cos_p) = (self.lfo_phase * std::f32::consts::TAU).sin_cos();
            self.lfo_phase += c.mod_inc;
            if self.lfo_phase >= 1.0 {
                self.lfo_phase -= 1.0;
            }

            let right_out = self.right.out;
            let left_out = self.left.out;
            let l = run_branch(&mut self.left, d + right_out, &c, c.tank_l, sin_p * c.excursion);
            let r = run_branch(&mut self.right, d + left_out, &c, c.tank_r, cos_p * c.excursion);
            self.left.out = l;
            self.right.out = r;

            let wet_l = self.tap(&TAPS_L, c.scale) * REVERB_OUTPUT_GAIN;
            let wet_r = self.tap(&TAPS_R, c.scale) * REVERB_OUTPUT_GAIN;
            // M/S で幅を決める (0% = mono)。
            let mid = 0.5 * (wet_l + wet_r);
            let side = 0.5 * (wet_l - wet_r) * c.width;
            b.l[i] = dry_l * (1.0 - c.mix) + (mid + side) * c.mix;
            b.r[i] = dry_r * (1.0 - c.mix) + (mid - side) * c.mix;
        }
        0.0
    }

    /// Table 2 のタップ 7 本を足す。位置はライン長と同じ倍率でスケールする。
    fn tap(&self, taps: &[(Branch, f32, f32); 7], scale: f32) -> f32 {
        let mut sum = 0.0;
        for &(branch, pos, sign) in taps {
            let ring = match branch {
                Branch::LDelay1 => &self.left.delay1,
                Branch::LApf2 => &self.left.apf2,
                Branch::LDelay2 => &self.left.delay2,
                Branch::RDelay1 => &self.right.delay1,
                Branch::RApf2 => &self.right.apf2,
                Branch::RDelay2 => &self.right.delay2,
            };
            sum += sign * ring.peek(scaled(pos, scale).max(1));
        }
        sum
    }
}

/// タンク片枝の 1 サンプル。`x` = 入力 (ディフューザ出力 + 相手枝の出力)。
/// 戻り値 = この枝の出力 (相手枝の入力になる)。
#[inline]
fn run_branch(br: &mut Branch2, x: f32, c: &Coeffs, len: [f32; 4], excursion: f32) -> f32 {
    // 変調 APF。decay diffusion 1 は Fig.1 の "note sign" により負で使う。
    let y = br.mod_apf.step_allpass(x, (len[0] + excursion).max(2.0), -REVERB_DECAY_DIFFUSION_1);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let y = br.delay1.step(y, (len[1] as usize).max(1));
    // 高域ダンピング (論文の damping) → 低域ダンピング (論文には無い追加、§200 Hz 以下を減らす)。
    let damped = one_pole(&mut br.damp_state, y, 1.0 - c.damping);
    let lo = one_pole(&mut br.lo_state, damped, c.lo_coeff);
    let y = damped - lo * c.lf_damp;
    let y = y * c.decay;
    let y = br.apf2.step_allpass(y, len[2], c.dd2);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let y = br.delay2.step(y, (len[3] as usize).max(1));
    // 枝の出力はサンプルを跨いで保持されるので、ここでも非正規化数を落とす
    // (リングへの書き込みだけでは、相手枝へ渡る値が非正規化のまま残る)。
    flush_denormal(y * c.decay)
}

impl Coeffs {
    fn silent() -> Self {
        Self {
            tank_l: [1.0; 4],
            tank_r: [1.0; 4],
            in_apf: [1; 4],
            predelay: 1,
            excursion: 0.0,
            mod_inc: 0.0,
            decay: 0.0,
            dd2: 0.5,
            id1: 0.0,
            id2: 0.0,
            damping: 0.0,
            lf_damp: 0.0,
            lo_coeff: 1.0,
            bandwidth: 1.0,
            low_cut: Biquad::IDENTITY,
            scale: 1.0,
            freeze: false,
            width: 1.0,
            mix: 0.0,
        }
    }

    fn build(s: &ReverbSettings, sample_rate: f32) -> Self {
        let scale = scale_of(sample_rate, s.size_pct);
        let diff = (s.diffusion_pct / 100.0).clamp(0.0, 1.0);
        let damping = if s.freeze { 0.0 } else { (s.damp_pct / 100.0).clamp(0.0, 1.0) * MAX_DAMPING };
        let lf_damp = if s.freeze { 0.0 } else { (s.lf_damp_pct / 100.0).clamp(0.0, 1.0) };
        let predelay = if s.predelay_ms > 0.0 {
            (s.predelay_ms / 1000.0 * sample_rate).round().max(1.0) as usize
        } else {
            1
        };
        let low_cut =
            if s.low_cut_hz > 0.0 { Biquad::high_pass(sample_rate, s.low_cut_hz, 0.707) } else { Biquad::IDENTITY };
        Self {
            tank_l: TANK_L.map(|v| v * scale),
            tank_r: TANK_R.map(|v| v * scale),
            in_apf: IN_APF.map(|v| scaled(v, scale).max(2)),
            predelay,
            excursion: EXCURSION_PEAK * scale * (s.mod_depth_pct / 100.0).clamp(0.0, 1.0),
            mod_inc: (s.mod_rate_hz / sample_rate).clamp(0.0, 0.5),
            decay: s.decay_coeff(),
            dd2: s.decay_diffusion_2(),
            id1: REVERB_INPUT_DIFFUSION.0 * diff,
            id2: REVERB_INPUT_DIFFUSION.1 * diff,
            damping,
            lf_damp,
            lo_coeff: one_pole_coeff(LF_DAMP_XOVER_HZ, sample_rate),
            bandwidth: one_pole_coeff(s.high_cut_hz, sample_rate),
            low_cut,
            scale,
            freeze: s.freeze,
            width: (s.width_pct / 100.0).clamp(0.0, 1.0),
            mix: (s.mix_pct / 100.0).clamp(0.0, 1.0),
        }
    }
}

/// 論文の基準 (29761 Hz) からの長さ倍率。
fn scale_of(sample_rate: f32, size_pct: f32) -> f32 {
    sample_rate.max(1.0) / REVERB_BASE_SR * (size_pct / 100.0).clamp(0.01, REVERB_MAX_SIZE_PCT / 100.0)
}

/// 基準長 × 倍率を整数サンプルへ。
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn scaled(base: f32, scale: f32) -> usize {
    (base * scale).round().max(1.0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::NativeKind;

    const SR: f32 = 48_000.0;

    /// インパルスを入れて `frames` サンプル流し、(L, R) を返す。
    fn impulse(st: &mut ReverbState, s: &ReverbSettings, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r) = (vec![0.0_f32; frames], vec![0.0_f32; frames]);
        (l[0], r[0]) = (1.0, 1.0);
        st.process(s, NativeBlock { l: &mut l, r: &mut r, n: frames, sample_rate: SR, bpm: 120.0, sidechain: None, listen_out: None });
        (l, r)
    }

    fn rms(v: &[f32], from: usize, to: usize) -> f32 {
        let slice = &v[from.min(v.len())..to.min(v.len())];
        if slice.is_empty() {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let n = slice.len() as f32;
        (slice.iter().map(|x| x * x).sum::<f32>() / n).sqrt()
    }

    #[test]
    fn インパルスから減衰する残響が出る() {
        let s = ReverbSettings { mix_pct: 100.0, decay_s: 2.0, ..ReverbSettings::default() };
        let mut st = ReverbState::new(SR);
        let (l, r) = impulse(&mut st, &s, 96_000);
        assert!(l.iter().chain(&r).all(|x| x.is_finite() && x.abs() < 4.0), "発散または非有限");
        // predelay 10 ms のあと残響が立ち上がり、時間とともに減る。
        let early = rms(&l, 480, 12_000);
        let mid = rms(&l, 24_000, 36_000);
        let late = rms(&l, 72_000, 96_000);
        assert!(early > 1e-4, "残響が出ていない ({early})");
        assert!(mid < early && late < mid, "減衰していない early={early} mid={mid} late={late}");
        // 左右は同じにならない (Table 2 のタップが非対称 = mono 入力から stereo 像を作る)。
        let diff = l.iter().zip(&r).map(|(a, b)| (a - b).abs()).sum::<f32>();
        assert!(diff > 1e-3, "左右が同一 ({diff})");
    }

    #[test]
    fn decay_time_が長いほど尾が残る() {
        let base = ReverbSettings { mix_pct: 100.0, ..ReverbSettings::default() };
        let tail = |t60: f32| {
            let s = ReverbSettings { decay_s: t60, ..base };
            let mut st = ReverbState::new(SR);
            let (l, _) = impulse(&mut st, &s, 96_000);
            rms(&l, 72_000, 96_000)
        };
        let (short, long) = (tail(0.5), tail(8.0));
        assert!(long > short * 4.0, "T60 8s の尾 {long} が 0.5s の尾 {short} より十分大きくない");
    }

    #[test]
    fn freeze_は減衰を止める() {
        let s = ReverbSettings { mix_pct: 100.0, decay_s: 1.0, freeze: false, ..ReverbSettings::default() };
        let mut st = ReverbState::new(SR);
        // まず残響を溜める。
        let _ = impulse(&mut st, &s, 24_000);
        // Freeze に入れて入力を止めると、以後ほぼ減らない。
        let frozen = ReverbSettings { freeze: true, ..s };
        let run = |st: &mut ReverbState| {
            let (mut l, mut r) = (vec![0.0_f32; 48_000], vec![0.0_f32; 48_000]);
            st.process(&frozen, NativeBlock { l: &mut l, r: &mut r, n: 48_000, sample_rate: SR, bpm: 120.0, sidechain: None, listen_out: None });
            (rms(&l, 0, 4_000), rms(&l, 44_000, 48_000))
        };
        let (head, tail) = run(&mut st);
        assert!(head > 1e-5, "Freeze 前の残響が無い ({head})");
        assert!(tail > head * 0.5, "Freeze しているのに 1 秒で半減した head={head} tail={tail}");
    }

    #[test]
    fn mix_0_パーセントは素通し() {
        let s = ReverbSettings { mix_pct: 0.0, ..ReverbSettings::default() };
        let mut st = ReverbState::new(SR);
        let (mut l, mut r) = (vec![0.5_f32; 512], vec![-0.25_f32; 512]);
        st.process(&s, NativeBlock { l: &mut l, r: &mut r, n: 512, sample_rate: SR, bpm: 120.0, sidechain: None, listen_out: None });
        assert!(l.iter().all(|x| (x - 0.5).abs() < 1e-6) && r.iter().all(|x| (x + 0.25).abs() < 1e-6));
    }

    /// **RT 規約の回帰** — `reset` は容量を保つ (= 再生スレッドで確保しない)。
    #[test]
    fn reset_と引き継ぎは容量を保つ() {
        let mut a = super::super::NativeDsp::new(NativeKind::Reverb, SR);
        let cap = |d: &super::super::NativeDsp| match d {
            super::super::NativeDsp::Reverb(r) => r.capacity_key(),
            _ => unreachable!(),
        };
        let before = cap(&a);
        a.reset();
        assert_eq!(before, cap(&a));

        let mut other = super::super::NativeDsp::new(NativeKind::Reverb, SR);
        assert!(a.adopt_state_from(&mut other));
        let mut diff = super::super::NativeDsp::new(NativeKind::Reverb, SR * 2.0);
        assert!(!a.adopt_state_from(&mut diff), "サンプルレートが違えば長さの意味が変わる");
    }
}
