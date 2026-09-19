//! 内蔵 Delay。設計の正本は `docs/plan_rmd_134_135_reverb_delay.md` §7。
//!
//! 遅延メモリは [`DelayState::new`] (compile 時 = off-RT) で [`MAX_DELAY_SEC`] ぶん確保し、
//! 以後は再確保しない。[`DelayState::reset`] も 0 埋めだけで容量を保つ (RT で呼ばれる)。
//!
//! フィルタは**読み出しの直後に 1 組だけ**置き、その出力を出力段とフィードバックの両方へ配る
//! (Surge XT `sst/effects/Delay.h:415-428` と同じ)。出力段だけに置くと繰り返しが暗くならない。
//! 飽和 ([`DelayDrive`]) は書き込み直前に 1 回 — Feedback 100% で発散しない唯一の保証。

use common::dsp::{Biquad, BiquadState};
use common::model::{DelayDrive, DelayMode, DelayPattern, DelaySettings, MAX_DELAY_SEC};

use super::ring::Ring;
use super::{NativeBlock, block_len};

/// 4 点補間に要る前後の余白 (サンプル)。
const GUARD: usize = 4;
/// `Mode::Repitch` が目標へ寄る半減期 (秒)。Vital `kDelayHalfLife = 0.02f` と同値。
const REPITCH_HALF_LIFE_S: f32 = 0.02;
/// `Mode::Fade` のクロスフェード長 (秒)。**固定 ms** — Ardour ACE Delay は
/// `xfade += 1/n_samples` なのでホストの buffer size で音が変わる (`a-delay.c:481`)。
const FADE_SECS: f32 = 0.02;
/// `ModDepth` 100% の揺れ幅 (ms、片側)。コーラス〜フランジャ相当。
const MAX_MOD_MS: f32 = 3.0;
/// `Freeze` 中のフィードバック。
const FREEZE_FEEDBACK: f32 = 1.0;

/// 片チャンネルの読み出しヘッドと、ループ内フィルタの状態。
#[derive(Default, Clone, Copy)]
struct Head {
    /// 現在の読み出し遅延 (サンプル)。
    pos: f32,
    /// `Fade` 中の旧ヘッド。
    prev: f32,
    /// `Fade` の進み (1.0 = 完了)。
    xfade: f32,
    hp: BiquadState,
    lp: BiquadState,
}

impl Head {
    fn reset(&mut self) {
        let pos = self.pos;
        *self = Self { pos, prev: pos, xfade: 1.0, ..Self::default() };
    }

    /// 目標遅延へヘッドを進める。戻り値 = このサンプルで読むべき値。
    #[inline]
    fn read(&mut self, ring: &Ring, target: f32, mode: DelayMode, repitch_a: f32, fade_inc: f32) -> f32 {
        match mode {
            DelayMode::Jump => {
                self.pos = target;
                self.xfade = 1.0;
                ring.peek_frac(self.pos)
            }
            DelayMode::Repitch => {
                self.pos += (target - self.pos) * repitch_a;
                self.xfade = 1.0;
                ring.peek_frac(self.pos)
            }
            DelayMode::Fade => {
                // 目標が動いたらクロスフェードを張り直す (進行中なら現在位置から)。
                if (target - self.pos).abs() > 0.5 {
                    self.prev = self.pos;
                    self.pos = target;
                    self.xfade = 0.0;
                }
                if self.xfade >= 1.0 {
                    return ring.peek_frac(self.pos);
                }
                self.xfade = (self.xfade + fade_inc).min(1.0);
                // 等電力クロスフェード (無相関な 2 点なので振幅和ではなく電力和で揃える)。
                let (s, c) = (self.xfade * std::f32::consts::FRAC_PI_2).sin_cos();
                ring.peek_frac(self.pos) * s + ring.peek_frac(self.prev) * c
            }
        }
    }

    /// 読んだ値にループ内フィルタを当てる。
    #[inline]
    fn filter(&mut self, x: f32, hp: &Biquad, lp: &Biquad) -> f32 {
        let y = self.hp.process(hp, x);
        self.lp.process(lp, y)
    }
}

/// 1 buffer ぶんの係数。
#[derive(Clone, Copy, PartialEq)]
struct Coeffs {
    target_l: f32,
    target_r: f32,
    feedback: f32,
    cross: f32,
    pattern: DelayPattern,
    mode: DelayMode,
    drive: DelayDrive,
    hp: Biquad,
    lp: Biquad,
    mod_inc: f32,
    mod_depth: f32,
    width: f32,
    mix: f32,
    freeze: bool,
    repitch_a: f32,
    fade_inc: f32,
}

pub struct DelayState {
    ring_l: Ring,
    ring_r: Ring,
    head_l: Head,
    head_r: Head,
    lfo_phase: f32,
    cached: Option<(DelaySettings, f32, f32)>,
    coeffs: Coeffs,
}

impl DelayState {
    /// **off-RT 専用** — ここでだけ確保する。
    #[must_use]
    pub fn new(sample_rate: f32) -> Self {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cap = (MAX_DELAY_SEC * sample_rate.max(1.0)).ceil() as usize + GUARD;
        Self {
            ring_l: Ring::new(cap),
            ring_r: Ring::new(cap),
            head_l: Head::default(),
            head_r: Head::default(),
            lfo_phase: 0.0,
            cached: None,
            coeffs: Coeffs::silent(),
        }
    }

    /// 無音の状態に戻す (容量は保つ = 確保しない)。
    pub fn reset(&mut self) {
        self.ring_l.reset();
        self.ring_r.reset();
        self.head_l.reset();
        self.head_r.reset();
        self.lfo_phase = 0.0;
        self.cached = None;
    }

    /// 遅延メモリの容量 (引き継ぎの可否判定 = サンプルレートが同じか)。
    #[must_use]
    pub fn capacity_key(&self) -> usize {
        self.ring_l.capacity()
    }

    pub(super) fn process(&mut self, s: &DelaySettings, b: NativeBlock<'_>) -> f32 {
        let n = block_len(b.l, b.r, b.n);
        if n == 0 || b.sample_rate <= 0.0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let max_pos = (self.ring_l.capacity() - GUARD) as f32;
        if self.cached != Some((*s, b.sample_rate, b.bpm)) {
            self.coeffs = Coeffs::build(s, b.sample_rate, b.bpm, max_pos);
            self.cached = Some((*s, b.sample_rate, b.bpm));
            // 初回はヘッドを目標に置く (0 から寄せると最初の 20 ms がピッチシフトする)。
            if self.head_l.pos == 0.0 {
                self.head_l.pos = self.coeffs.target_l;
                self.head_l.prev = self.coeffs.target_l;
                self.head_l.xfade = 1.0;
                self.head_r.pos = self.coeffs.target_r;
                self.head_r.prev = self.coeffs.target_r;
                self.head_r.xfade = 1.0;
            }
        }
        let c = self.coeffs;
        let ping = c.pattern.is_ping_pong();
        // Ping R は L / R を入れ替えただけ — 内側は Ping L として書き、入出力で swap する。
        let swap = c.pattern == DelayPattern::PingR;
        for i in 0..n {
            let (dry_l, dry_r) = (b.l[i], b.r[i]);
            let (in_l, in_r) = if swap { (dry_r, dry_l) } else { (dry_l, dry_r) };

            let wob = if c.mod_depth > 0.0 {
                (self.lfo_phase * std::f32::consts::TAU).sin() * c.mod_depth
            } else {
                0.0
            };
            self.lfo_phase += c.mod_inc;
            if self.lfo_phase >= 1.0 {
                self.lfo_phase -= 1.0;
            }
            let (tl, tr) = ((c.target_l + wob).clamp(2.0, max_pos), (c.target_r - wob).clamp(2.0, max_pos));

            let raw_l = self.head_l.read(&self.ring_l, tl, c.mode, c.repitch_a, c.fade_inc);
            let raw_r = self.head_r.read(&self.ring_r, tr, c.mode, c.repitch_a, c.fade_inc);
            let wet_l = self.head_l.filter(raw_l, &c.hp, &c.lp);
            let wet_r = self.head_r.filter(raw_r, &c.hp, &c.lp);

            let fb = if c.freeze { FREEZE_FEEDBACK } else { c.feedback };
            let (src_l, src_r) = if c.freeze {
                (0.0, 0.0)
            } else if ping {
                // 入力をモノ化して L のラインにだけ入れる (Vital `kPingPong`)。
                let mono = (in_l + in_r) * std::f32::consts::FRAC_1_SQRT_2;
                (mono, 0.0)
            } else {
                (in_l, in_r)
            };
            let (w_l, w_r) = if ping {
                // **往路は unity、復路にだけ Feedback**。両方に掛けると 1 往復で 2 回掛かり、
                // 「Feedback 50%」が実際には 25% になる (Vital `delay.cpp:84-88`)。
                (src_l + fb * wet_r, wet_l)
            } else {
                (src_l + fb * wet_l + c.cross * wet_r, src_r + fb * wet_r + c.cross * wet_l)
            };
            let drive = if c.freeze { DelayDrive::Off } else { c.drive };
            self.ring_l.push(saturate(w_l, drive));
            self.ring_r.push(saturate(w_r, drive));

            // M/S で幅を決める (100% = 素通し、0% = mono、200% = 誇張)。
            let (out_l, out_r) = if swap { (wet_r, wet_l) } else { (wet_l, wet_r) };
            let mid = 0.5 * (out_l + out_r);
            let side = 0.5 * (out_l - out_r) * c.width;
            b.l[i] = dry_l * (1.0 - c.mix) + (mid + side) * c.mix;
            b.r[i] = dry_r * (1.0 - c.mix) + (mid - side) * c.mix;
        }
        0.0
    }
}

/// フィードバックループの飽和。`Off` 以外は必ず有界なので Feedback 100% でも発散しない。
#[inline]
fn saturate(x: f32, drive: DelayDrive) -> f32 {
    match drive {
        DelayDrive::Off => x,
        // 3 次のソフトクリップ (|x| < 1 では ほぼ線形、±1.5 で飽和)。
        DelayDrive::Soft => {
            let v = (x * (2.0 / 3.0)).clamp(-1.0, 1.0);
            1.5 * (v - v * v * v / 3.0)
        }
        DelayDrive::Tanh => x.tanh(),
        DelayDrive::Hard => x.clamp(-1.0, 1.0),
    }
}

impl Coeffs {
    fn silent() -> Self {
        Self {
            target_l: 2.0,
            target_r: 2.0,
            feedback: 0.0,
            cross: 0.0,
            pattern: DelayPattern::Stereo,
            mode: DelayMode::Repitch,
            drive: DelayDrive::Off,
            hp: Biquad::IDENTITY,
            lp: Biquad::IDENTITY,
            mod_inc: 0.0,
            mod_depth: 0.0,
            width: 1.0,
            mix: 0.0,
            freeze: false,
            repitch_a: 1.0,
            fade_inc: 1.0,
        }
    }

    fn build(s: &DelaySettings, sample_rate: f32, bpm: f32, max_pos: f32) -> Self {
        let samples = |right: bool| (s.effective_secs(right, bpm) * sample_rate).clamp(2.0, max_pos);
        let hp = if s.hp_hz > 0.0 { Biquad::high_pass(sample_rate, s.hp_hz, 0.707) } else { Biquad::IDENTITY };
        let lp = if s.lp_hz < sample_rate * 0.49 {
            Biquad::low_pass(sample_rate, s.lp_hz, 0.707)
        } else {
            Biquad::IDENTITY
        };
        Self {
            target_l: samples(false),
            target_r: samples(true),
            feedback: (s.feedback_pct / 100.0).clamp(0.0, 1.0),
            cross: if s.pattern == DelayPattern::Stereo { (s.cross_pct / 100.0).clamp(0.0, 1.0) } else { 0.0 },
            pattern: s.pattern,
            mode: s.mode,
            drive: s.drive,
            hp,
            lp,
            mod_inc: (s.mod_rate_hz / sample_rate).clamp(0.0, 0.5),
            mod_depth: MAX_MOD_MS / 1000.0 * sample_rate * (s.mod_depth_pct / 100.0).clamp(0.0, 1.0),
            width: (s.width_pct / 100.0).clamp(0.0, 2.0),
            mix: (s.mix_pct / 100.0).clamp(0.0, 1.0),
            freeze: s.freeze,
            // 半減期 h の 1 極: a = 1 - 2^(-1/(h*sr))。
            repitch_a: 1.0 - 0.5_f32.powf(1.0 / (REPITCH_HALF_LIFE_S * sample_rate).max(1.0)),
            fade_inc: 1.0 / (FADE_SECS * sample_rate).max(1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{DelayDiv, NativeKind, NativeParams};

    const SR: f32 = 48_000.0;

    /// フィルタを素通しにした設定 (位置とゲインを正確に見るため)。
    fn transparent() -> DelaySettings {
        DelaySettings {
            sync: false,
            time_l_ms: 10.0,
            time_r_ms: 10.0,
            link: true,
            feedback_pct: 0.0,
            pattern: DelayPattern::Mono,
            drive: DelayDrive::Off,
            hp_hz: 0.0,
            // Nyquist より上 = 係数が IDENTITY になる (範囲外だが構造体直書きなので通る)。
            lp_hz: 30_000.0,
            mod_depth_pct: 0.0,
            width_pct: 100.0,
            mix_pct: 100.0,
            ..DelaySettings::default()
        }
    }

    /// 先頭にインパルスを入れて `frames` サンプル流し、(L, R) を返す。
    fn impulse(st: &mut DelayState, s: &DelaySettings, frames: usize, into_right: bool) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r) = (vec![0.0_f32; frames], vec![0.0_f32; frames]);
        if into_right { r[0] = 1.0 } else { l[0] = 1.0 }
        // 1 buffer で流す (block 境界の影響を入れない)。
        st.process(s, NativeBlock { l: &mut l, r: &mut r, n: frames, sample_rate: SR, bpm: 120.0, sidechain: None, listen_out: None });
        (l, r)
    }

    fn peak_at(v: &[f32], from: usize, to: usize) -> (usize, f32) {
        let mut best = (from, 0.0_f32);
        for (i, x) in v.iter().enumerate().take(to.min(v.len())).skip(from) {
            if x.abs() > best.1 {
                best = (i, x.abs());
            }
        }
        best
    }

    #[test]
    fn エコーは指定した遅延サンプルに出る() {
        let s = transparent();
        let mut st = DelayState::new(SR);
        // 10 ms @48k = 480 サンプル。
        let (l, _) = impulse(&mut st, &s, 2_000, false);
        let (idx, amp) = peak_at(&l, 1, 2_000);
        assert!((idx as i32 - 480).abs() <= 2, "エコーの位置 {idx} (期待 480)");
        assert!(amp > 0.9, "エコーの振幅 {amp}");
    }

    /// **往路 unity の回帰** — ping-pong の 1 往復ゲインはノブの Feedback そのもの。
    /// 往路にも fb を掛けると 1 往復で 2 回掛かり、50% が 25% になる。
    #[test]
    fn ping_pong_の_1_往復ゲインは_feedback_ノブの値と一致する() {
        let s = DelaySettings { pattern: DelayPattern::PingL, feedback_pct: 50.0, ..transparent() };
        let mut st = DelayState::new(SR);
        // L 入力 → R に 1 回目 (t=960)、R に 2 回目 (t=1920)。その比が 1 往復ゲイン。
        let (_, r) = impulse(&mut st, &s, 2_400, false);
        let (i1, a1) = peak_at(&r, 700, 1_200);
        let (i2, a2) = peak_at(&r, 1_700, 2_200);
        assert!((i1 as i32 - 960).abs() <= 2 && (i2 as i32 - 1_920).abs() <= 2, "位置 {i1} / {i2}");
        let round_trip = a2 / a1;
        assert!((round_trip - 0.5).abs() < 0.02, "1 往復ゲイン {round_trip} (期待 0.5)");
    }

    /// Ping R は Ping L の L / R を入れ替えただけ。
    #[test]
    fn ping_r_は_ping_l_の左右反転() {
        let left = DelaySettings { pattern: DelayPattern::PingL, feedback_pct: 50.0, ..transparent() };
        let right = DelaySettings { pattern: DelayPattern::PingR, ..left };
        let (mut a, mut b) = (DelayState::new(SR), DelayState::new(SR));
        let (_, r_of_l) = impulse(&mut a, &left, 2_400, false);
        let (l_of_r, _) = impulse(&mut b, &right, 2_400, true);
        for (x, y) in r_of_l.iter().zip(&l_of_r) {
            assert!((x - y).abs() < 1e-5, "反転が一致しない {x} vs {y}");
        }
    }

    /// `Drive` が Off 以外なら Feedback 100% でも発散しない。
    #[test]
    fn feedback_100_パーセントでも_drive_が発散を止める() {
        for drive in [DelayDrive::Soft, DelayDrive::Tanh, DelayDrive::Hard] {
            let s = DelaySettings { feedback_pct: 100.0, drive, time_l_ms: 1.0, time_r_ms: 1.0, ..transparent() };
            let mut st = DelayState::new(SR);
            let (mut l, mut r) = (vec![0.0_f32; 4_096], vec![0.0_f32; 4_096]);
            for i in 0..4_096 {
                // 連続的に強い入力を入れ続ける (最悪ケース)。
                l[i] = 1.0;
                r[i] = 1.0;
            }
            for _ in 0..40 {
                st.process(&s, NativeBlock { l: &mut l, r: &mut r, n: 4_096, sample_rate: SR, bpm: 120.0, sidechain: None, listen_out: None });
                for v in l.iter_mut().chain(r.iter_mut()) {
                    assert!(v.is_finite() && v.abs() < 8.0, "{drive:?}: 発散 {v}");
                    *v = 1.0;
                }
            }
        }
    }

    /// tempo sync は BPM から遅延を決め、上限を超えたら頭打ちになる。
    #[test]
    fn tempo_sync_は_bpm_に追従し上限で頭打ちになる() {
        let s = DelaySettings { sync: true, div_l: DelayDiv::D8, ..transparent() };
        let mut st = DelayState::new(SR);
        // 1/8 @120BPM = 250 ms = 12000 サンプル。
        let (mut l, mut r) = (vec![0.0_f32; 16_000], vec![0.0_f32; 16_000]);
        l[0] = 1.0;
        st.process(&s, NativeBlock { l: &mut l, r: &mut r, n: 16_000, sample_rate: SR, bpm: 120.0, sidechain: None, listen_out: None });
        let (idx, _) = peak_at(&l, 1, 16_000);
        assert!((idx as i32 - 12_000).abs() <= 2, "1/8 @120BPM の位置 {idx}");
        // BPM 1 の 1/1. は 24 秒ぶんだが、確保は 5 秒なのでそこで頭打ち (panic せず有限)。
        let slow = DelaySettings { div_l: DelayDiv::D1D, ..s };
        assert_eq!(slow.effective_secs(false, 1.0), MAX_DELAY_SEC);
    }

    /// **RT 規約の回帰** — `reset` は容量を保つ (= 再生スレッドで確保しない)。
    #[test]
    fn reset_と引き継ぎは容量を保つ() {
        let mut a = super::super::NativeDsp::new(NativeKind::Delay, SR);
        let before = match &a {
            super::super::NativeDsp::Delay(d) => d.capacity_key(),
            _ => unreachable!(),
        };
        a.reset();
        let after = match &a {
            super::super::NativeDsp::Delay(d) => d.capacity_key(),
            _ => unreachable!(),
        };
        assert_eq!(before, after);

        // 同じサンプルレート同士は引き継ぐ、違えば引き継がない。
        let mut same = super::super::NativeDsp::new(NativeKind::Delay, SR);
        let mut other = super::super::NativeDsp::new(NativeKind::Delay, SR);
        assert!(same.adopt_state_from(&mut other));
        let mut diff = super::super::NativeDsp::new(NativeKind::Delay, SR * 2.0);
        assert!(!same.adopt_state_from(&mut diff), "サンプルレートが違えば長さの意味が変わる");
        // 種類違いも引き継がない。
        let mut comp = super::super::NativeDsp::new(NativeKind::Comp, SR);
        assert!(!same.adopt_state_from(&mut comp));
    }

    /// 引き継ぎで繰り返しが切れない (swap でリングの中身が移る)。
    #[test]
    fn 再_compile_を跨いでも繰り返しが残る() {
        let s = transparent();
        let s = DelaySettings { feedback_pct: 80.0, ..s };
        let mut old = super::super::NativeDsp::new(NativeKind::Delay, SR);
        let params = NativeParams::Delay(s);
        let (mut l, mut r) = (vec![0.0_f32; 480], vec![0.0_f32; 480]);
        l[0] = 1.0;
        old.process(&params, NativeBlock { l: &mut l, r: &mut r, n: 480, sample_rate: SR, bpm: 120.0, sidechain: None, listen_out: None });

        let mut fresh = super::super::NativeDsp::new(NativeKind::Delay, SR);
        assert!(fresh.adopt_state_from(&mut old));
        let (mut l2, mut r2) = (vec![0.0_f32; 480], vec![0.0_f32; 480]);
        fresh.process(&params, NativeBlock { l: &mut l2, r: &mut r2, n: 480, sample_rate: SR, bpm: 120.0, sidechain: None, listen_out: None });
        let (_, amp) = peak_at(&l2, 0, 480);
        assert!(amp > 0.9, "引き継いだ側にエコーが出ない ({amp})");
    }
}
