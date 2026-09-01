//! Generator modulator evaluation (LFO / Random / MSEG / Steps).
//!
//! `docs/plan_fixme_56_modulators.md`. これらは envelope follower と違い **audio
//! 入力に依存せず `song_beat` (と Free Hz 用の `song_secs`) の純粋関数** で出力する。
//! よって RT preview / 音声書き出し / video export の全経路で同一関数を呼べば
//! **drift ゼロ・bounce 完全再現**になる。状態を持たず alloc/lock もしないので
//! audio callback から直接呼んでよい。
//!
//! 出力は常に unipolar `0.0..=1.0`。極性 (Uni/Bipolar) は後段の
//! [`crate::model::ModRouting`] が `depth*(2s-1)` 等で担う (SSoT、 follower と同契約)。

use crate::model::{
    LfoConfig, LfoShape, ModRate, ModSourceKind, MsegConfig, MsegPlayMode, MsegPoint, RandomConfig,
    RetriggerMode, StepsConfig, StepsDirection,
};

use std::f64::consts::TAU;

/// tick 時点で解決済みの生成器パラメータ。RT で [`ModSourceKind`] を clone しないために
/// **`Copy` なオーバーライドだけ**を運ぶ (`MsegConfig.points` / `StepsConfig.values` は
/// `Vec` なので RT clone は heap 確保になる)。
///
/// `cycle_pos` は「未ラップの周期位置」で、rate が未変調なら [`cycle_pos`] の閉形式、
/// 変調されていれば [`crate::mod_graph`] の積分位相が入る。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GenParams {
    pub cycle_pos: f64,
    /// LFO の開始位相 (0..=1)。
    pub lfo_phase: f32,
    /// LFO Pulse の duty (0..=1)。
    pub pulse_width: f32,
    /// Random の Stepped↔Smoothed モーフ (0..=1)。
    pub random_smooth: f32,
    /// Steps の slew (0..=1)。
    pub steps_slew: f32,
}

impl GenParams {
    /// 変調が無いときの値 (config そのまま)。
    #[must_use]
    pub fn from_config(kind: &ModSourceKind, cycle_pos: f64) -> Self {
        let (lfo_phase, pulse_width) = match kind {
            ModSourceKind::Lfo(c) => (
                c.phase,
                match c.shape {
                    LfoShape::Pulse { width } => width,
                    _ => 0.5,
                },
            ),
            _ => (0.0, 0.5),
        };
        Self {
            cycle_pos,
            lfo_phase,
            pulse_width,
            random_smooth: match kind {
                ModSourceKind::Random(c) => c.smooth,
                _ => 0.0,
            },
            steps_slew: match kind {
                ModSourceKind::Steps(c) => c.slew,
                _ => 0.0,
            },
        }
    }
}

/// 解決済みパラメータで generator を評価する (unipolar 0..=1)。 envelope follower は
/// engine ring が算出するので `None`。 **クロス変調の唯一の評価点**
/// ([`crate::mod_graph::tick`] が呼ぶ)。
#[inline]
pub fn eval_generator(kind: &ModSourceKind, p: GenParams) -> Option<f32> {
    match kind {
        ModSourceKind::EnvelopeFollower { .. } => None,
        ModSourceKind::Lfo(c) => Some(eval_lfo(c, p)),
        ModSourceKind::Random(c) => Some(eval_random(c, p)),
        ModSourceKind::Mseg(c) => Some(eval_mseg(c, p)),
        ModSourceKind::Steps(c) => Some(eval_steps(c, p)),
    }
}

/// 変調が無い generator の出力スカラー (unipolar 0..=1)。閉形式なので O(1)。
/// envelope follower は `None`。
#[inline]
pub fn generator_scalar(kind: &ModSourceKind, t: ModTime) -> Option<f32> {
    let (rate, retrig) = match (kind.rate(), kind.retrigger()) {
        (Some(r), Some(rt)) => (r, rt),
        _ => return None,
    };
    let cp = cycle_pos(&rate, t, &retrig);
    eval_generator(kind, GenParams::from_config(kind, cp))
}

/// 生成器を評価する時刻。`anchor_secs` は [`RetriggerMode::FromBeat`] の
/// `anchor_beat` を **秒へ換算した値** で、テンポマップが要るので off-RT の
/// 呼び出し側 (plan 構築 / GUI プレビュー) が解決して渡す。
///
/// r.md #88: 旧実装は Free のとき retrigger を丸ごと無視していたため、
/// `⟲here` が音にも波形にも効かない silent no-op だった
/// (`docs/plan_fixme_56_modulators.md` が要求していた beat→secs 換算が未実装)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModTime {
    pub beat: f64,
    pub secs: f64,
    /// `FromBeat { anchor_beat }` を秒へ換算したもの。`FreeRun` では使わない。
    pub anchor_secs: f64,
}

impl ModTime {
    /// テンポ一定 (または Sync のみ使う) 文脈の簡易構築。
    #[must_use]
    pub fn new(beat: f64, secs: f64) -> Self {
        Self { beat, secs, anchor_secs: 0.0 }
    }
}

/// `rate` に応じた **未ラップの周期位置** (= 何周したか、 1.0 = 1 周) の **閉形式**。
/// Sync は song_beat、 Free は song_secs の関数。 どちらも transport の関数なので
/// 決定論的 (壁時計を使わない)。
///
/// **rate が変調されていないときだけ正しい。** 変調されているときは瞬時周波数の
/// 積分でしか位相が定まらないので [`crate::mod_graph`] の位相アキュムレータを使う
/// (`docs/plan_rmd_88_89_cross_modulation.md` §2)。未変調ならこの閉形式と積分は
/// **厳密に一致する**ので、既存曲の音は 1 サンプルも変わらない。
#[inline]
pub fn cycle_pos(rate: &ModRate, t: ModTime, retrigger: &RetriggerMode) -> f64 {
    match rate.mode {
        crate::model::ModRateMode::Sync => {
            let beat = match retrigger {
                RetriggerMode::FreeRun => t.beat,
                RetriggerMode::FromBeat { anchor_beat } => t.beat - anchor_beat,
            };
            beat / rate.period_beats()
        }
        crate::model::ModRateMode::Free => {
            let secs = match retrigger {
                RetriggerMode::FreeRun => t.secs,
                RetriggerMode::FromBeat { .. } => t.secs - t.anchor_secs,
            };
            secs * f64::from(rate.hz.clamp(
                crate::model::MOD_RATE_HZ_MIN,
                crate::model::MOD_RATE_HZ_MAX,
            ))
        }
    }
}

/// LFO 波形 (phase 0..=1 → unipolar 0..=1)。
#[inline]
pub fn lfo_shape_value(shape: LfoShape, p: f64) -> f32 {
    let p = p.rem_euclid(1.0);
    let v = match shape {
        LfoShape::Sine => 0.5 + 0.5 * (TAU * p).sin(),
        LfoShape::Triangle => 1.0 - (2.0 * p - 1.0).abs(),
        LfoShape::SawUp => p,
        LfoShape::SawDown => 1.0 - p,
        LfoShape::Square => {
            if p < 0.5 {
                1.0
            } else {
                0.0
            }
        }
        LfoShape::Pulse { width } => {
            if p < width.clamp(0.0, 1.0) as f64 {
                1.0
            } else {
                0.0
            }
        }
    };
    v as f32
}

#[inline]
fn eval_lfo(c: &LfoConfig, g: GenParams) -> f32 {
    let p = g.cycle_pos + f64::from(g.lfo_phase);
    // Pulse の duty は変調されうるので `GenParams` 側を使う (config の値は base)。
    let shape = match c.shape {
        LfoShape::Pulse { .. } => LfoShape::Pulse { width: g.pulse_width },
        other => other,
    };
    lfo_shape_value(shape, p)
}

/// SplitMix64: seed から step ごとに決定論的な乱数を引く (依存追加なし)。
#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// step インデックスの決定論的乱数 0..=1。 `step` 負値も許容 (FromBeat 由来)。
#[inline]
pub fn random_unit(seed: u64, step: i64) -> f32 {
    let h = splitmix64(seed ^ (step as u64));
    ((h >> 11) as f64 * (1.0 / (1u64 << 53) as f64)) as f32
}

/// 既存 seed から決定論的に別 seed を派生する (UI の re-roll 用、 壁時計/RNG なし)。
#[inline]
pub fn reseed(prev: u64) -> u64 {
    splitmix64(prev ^ 0xD1B5_4A32_D192_ED03)
}

#[inline]
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[inline]
fn eval_random(c: &RandomConfig, g: GenParams) -> f32 {
    let cp = g.cycle_pos;
    let step = cp.floor();
    let frac = (cp - step) as f32;
    let a = random_unit(c.seed, step as i64);
    // Bitwig 流 Stepped↔Smoothed 連続モーフ: smooth=0 で完全階段 (S&H = `a`)、
    // smooth=1 で隣接 step を smoothstep 補間、 中間は両者を lerp。
    let smooth = g.random_smooth.clamp(0.0, 1.0);
    let v = if smooth <= 0.0 {
        a
    } else {
        let b = random_unit(c.seed, step as i64 + 1);
        let interp = lerp(a, b, smoothstep(frac));
        lerp(a, interp, smooth)
    };
    v.clamp(0.0, 1.0)
}

/// forward カウンタ `k` を direction に応じた step index に写す。
#[inline]
fn step_index(direction: StepsDirection, k: i64, n: usize) -> usize {
    let n = n.max(1);
    match direction {
        StepsDirection::Forward => k.rem_euclid(n as i64) as usize,
        StepsDirection::Backward => (n - 1) - k.rem_euclid(n as i64) as usize,
        StepsDirection::PingPong => {
            if n == 1 {
                return 0;
            }
            let period = (2 * n - 2) as i64;
            let kk = k.rem_euclid(period) as usize;
            if kk < n { kk } else { period as usize - kk }
        }
    }
}

/// Steps の現在アクティブな step index (UI の走査ハイライト用)。 `eval_steps` の
/// index 計算と同一ロジック (direction / PingPong period を反映)。
#[inline]
pub fn steps_active_index(c: &StepsConfig, cycle_pos: f64) -> usize {
    let n = c.values.len();
    if n == 0 {
        return 0;
    }
    let count = match c.direction {
        StepsDirection::PingPong if n > 1 => 2 * n - 2,
        _ => n,
    };
    let pos = cycle_pos.rem_euclid(1.0);
    let k = (pos * count as f64).floor() as i64;
    step_index(c.direction, k, n)
}

#[inline]
fn eval_steps(c: &StepsConfig, g: GenParams) -> f32 {
    let n = c.values.len();
    if n == 0 {
        return 0.0;
    }
    let count = match c.direction {
        StepsDirection::PingPong if n > 1 => 2 * n - 2,
        _ => n,
    };
    let pos = g.cycle_pos.rem_euclid(1.0);
    let fidx = pos * count as f64;
    let k = fidx.floor() as i64;
    let frac = fidx.fract() as f32;
    let cur = c.values[step_index(c.direction, k, n)];
    let slew = g.steps_slew.clamp(0.0, 1.0);
    let v = if slew <= 0.0 {
        cur
    } else {
        let next = c.values[step_index(c.direction, k + 1, n)];
        let smoothed = lerp(cur, next, smoothstep(frac));
        lerp(cur, smoothed, slew)
    };
    v.clamp(0.0, 1.0)
}

/// セグメントの tension (-1..=1) で `t` (0..=1) を歪ませる。 0=linear、
/// +=凸(ease-out)、 -=凹(ease-in)。 単調・端点固定 (0→0, 1→1)。
#[inline]
fn apply_tension(t: f32, curve: f32) -> f32 {
    if curve.abs() < 1e-6 {
        return t;
    }
    // curve= -0.25 → exponent 2 (t^2)、 +0.25 → 0.5 (sqrt)。
    let k = 2.0_f32.powf(-curve * 4.0);
    t.clamp(0.0, 1.0).powf(k)
}

/// MSEG を 1 周内の正規化位置 `q` (0..=1) でサンプル。 points は時刻昇順前提。
#[inline]
pub fn mseg_sample(points: &[MsegPoint], q: f32) -> f32 {
    match points {
        [] => 0.0,
        [only] => only.value.clamp(0.0, 1.0),
        _ => {
            let q = q.clamp(0.0, 1.0);
            if q <= points[0].time {
                return points[0].value.clamp(0.0, 1.0);
            }
            if q >= points[points.len() - 1].time {
                return points[points.len() - 1].value.clamp(0.0, 1.0);
            }
            // bracket: points[i].time <= q < points[i+1].time。
            let i = points
                .windows(2)
                .position(|w| q >= w[0].time && q < w[1].time)
                .unwrap_or(points.len() - 2);
            let p0 = points[i];
            let p1 = points[i + 1];
            let span = (p1.time - p0.time).max(f32::MIN_POSITIVE);
            let t = ((q - p0.time) / span).clamp(0.0, 1.0);
            lerp(p0.value, p1.value, apply_tension(t, p0.curve)).clamp(0.0, 1.0)
        }
    }
}

#[inline]
fn eval_mseg(c: &MsegConfig, g: GenParams) -> f32 {
    let cp = g.cycle_pos;
    let q = match c.play_mode {
        MsegPlayMode::OneShot => cp.clamp(0.0, 1.0),
        MsegPlayMode::Loop => cp.rem_euclid(1.0),
        MsegPlayMode::PingPong => {
            let t = cp.rem_euclid(2.0);
            if t <= 1.0 { t } else { 2.0 - t }
        }
    };
    mseg_sample(&c.points, q as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModRate;

    // Sync 1/4 note: period = 1 beat。 secs は無関係 (0 を渡す)。
    fn sync_quarter() -> ModRate {
        ModRate::default()
    }

    #[test]
    fn lfo_各shapeが既知点で正しい値を返す() {
        // (shape, phase p, expected) — p は cycle 位置 (Sync では beat/period)。
        let cases = [
            (LfoShape::Sine, 0.0, 0.5),
            (LfoShape::Sine, 0.25, 1.0),
            (LfoShape::Sine, 0.5, 0.5),
            (LfoShape::Sine, 0.75, 0.0),
            (LfoShape::Triangle, 0.0, 0.0),
            (LfoShape::Triangle, 0.5, 1.0),
            (LfoShape::SawUp, 0.25, 0.25),
            (LfoShape::SawDown, 0.25, 0.75),
            (LfoShape::Square, 0.0, 1.0),
            (LfoShape::Square, 0.6, 0.0),
            (LfoShape::Pulse { width: 0.25 }, 0.1, 1.0),
            (LfoShape::Pulse { width: 0.25 }, 0.3, 0.0),
        ];
        for (shape, p, expected) in cases {
            let got = lfo_shape_value(shape, p);
            assert!(
                (got - expected).abs() < 1e-6,
                "shape={shape:?} p={p} got={got} expected={expected}"
            );
        }
    }

    #[test]
    fn lfo_sync_phaseはbeatの関数で1拍で1周() {
        let c = LfoConfig {
            shape: LfoShape::SawUp,
            rate: sync_quarter(),
            phase: 0.0,
            retrigger: RetriggerMode::FreeRun,
        };
        // SawUp なので scalar == phase。 1/4 note 周期 = 1 beat。
        let cases = [(0.0, 0.0), (0.25, 0.25), (0.5, 0.5), (1.0, 0.0), (2.5, 0.5)];
        for (beat, expected) in cases {
            let got = generator_scalar(&ModSourceKind::Lfo(c), ModTime::new(beat, 0.0)).unwrap();
            assert!(
                (got - expected).abs() < 1e-6,
                "beat={beat} got={got} expected={expected}"
            );
        }
    }

    #[test]
    fn lfo_phaseオフセットが波形をずらす() {
        let c = LfoConfig {
            shape: LfoShape::SawUp,
            rate: sync_quarter(),
            phase: 0.25,
            retrigger: RetriggerMode::FreeRun,
        };
        // beat=0 で phase=0.25。
        let got = generator_scalar(&ModSourceKind::Lfo(c), ModTime::new(0.0, 0.0)).unwrap();
        assert!((got - 0.25).abs() < 1e-6, "got={got}");
    }

    #[test]
    fn lfo_free_hzは秒の関数() {
        let c = LfoConfig {
            shape: LfoShape::SawUp,
            rate: ModRate { mode: crate::model::ModRateMode::Free, hz: 2.0, ..ModRate::default() },
            phase: 0.0,
            retrigger: RetriggerMode::FreeRun,
        };
        // 2 Hz: 0.25 秒で半周 → SawUp=0.5。
        let got = generator_scalar(&ModSourceKind::Lfo(c), ModTime::new(0.0, 0.25)).unwrap();
        assert!((got - 0.5).abs() < 1e-6, "got={got}");
    }

    #[test]
    fn random_は同beatで再現し別seedで別値() {
        let mk = |seed| RandomConfig {
            rate: sync_quarter(),
            smooth: 0.0,
            seed,
            retrigger: RetriggerMode::FreeRun,
        };
        let a1 = generator_scalar(&ModSourceKind::Random(mk(42)), ModTime::new(3.2, 0.0)).unwrap();
        let a2 = generator_scalar(&ModSourceKind::Random(mk(42)), ModTime::new(3.2, 0.0)).unwrap();
        assert_eq!(a1, a2, "同 seed・同 beat は bit 再現");
        let b = generator_scalar(&ModSourceKind::Random(mk(43)), ModTime::new(3.2, 0.0)).unwrap();
        assert!(a1 != b, "別 seed は別値 (a={a1} b={b})");
    }

    #[test]
    fn random_steppedはstep内一定smoothedは補間() {
        // smooth=0 (完全 stepped = S&H)。
        let sh = RandomConfig {
            rate: sync_quarter(),
            smooth: 0.0,
            seed: 7,
            retrigger: RetriggerMode::FreeRun,
        };
        // 同じ step (beat 0.1 と 0.9 は period=1 beat の step 0) → 同値。
        let v1 = generator_scalar(&ModSourceKind::Random(sh), ModTime::new(0.1, 0.0)).unwrap();
        let v2 = generator_scalar(&ModSourceKind::Random(sh), ModTime::new(0.9, 0.0)).unwrap();
        assert_eq!(v1, v2, "stepped (smooth=0) は step 内一定");
        // step 境界の値そのもの (frac=0)。
        let edge = generator_scalar(&ModSourceKind::Random(sh), ModTime::new(0.0, 0.0)).unwrap();
        assert_eq!(edge, random_unit(7, 0));
        // smooth=1 は step 始点で a、 次 step 始点で b。
        let smooth = RandomConfig { smooth: 1.0, ..sh };
        let s0 = generator_scalar(&ModSourceKind::Random(smooth), ModTime::new(0.0, 0.0)).unwrap();
        let s1 = generator_scalar(&ModSourceKind::Random(smooth), ModTime::new(1.0, 0.0)).unwrap();
        assert!((s0 - random_unit(7, 0)).abs() < 1e-6);
        assert!((s1 - random_unit(7, 1)).abs() < 1e-6);
    }

    #[test]
    fn random_smoothは0と1の中間で按分() {
        let seed = 7;
        let base = RandomConfig {
            rate: sync_quarter(),
            smooth: 0.0,
            seed,
            retrigger: RetriggerMode::FreeRun,
        };
        // step 中央 (frac=0.5) で stepped=a、 fully-smoothed=lerp(a,b,smoothstep(0.5))。
        let beat = 0.5;
        let stepped = generator_scalar(&ModSourceKind::Random(base), ModTime::new(beat, 0.0)).unwrap();
        let smoothed =
            generator_scalar(&ModSourceKind::Random(RandomConfig { smooth: 1.0, ..base }), ModTime::new(beat, 0.0))
                .unwrap();
        let mid =
            generator_scalar(&ModSourceKind::Random(RandomConfig { smooth: 0.5, ..base }), ModTime::new(beat, 0.0))
                .unwrap();
        // 中間 morph は両端の中点 (lerp(stepped, smoothed, 0.5))。
        assert!(
            (mid - 0.5 * (stepped + smoothed)).abs() < 1e-6,
            "smooth=0.5 は stepped と smoothed の中点 (stepped={stepped} smoothed={smoothed} mid={mid})"
        );
    }

    #[test]
    fn steps_各方向のインデックスと値() {
        let values = vec![0.0, 0.25, 0.5, 1.0]; // n=4
        let mk = |direction| StepsConfig {
            values: values.clone(),
            rate: sync_quarter(), // 1 周 = 1 beat
            direction,
            slew: 0.0,
            retrigger: RetriggerMode::FreeRun,
        };
        // Forward: beat 0,0.25,0.5,0.75 → step 0,1,2,3。
        let fwd = mk(StepsDirection::Forward);
        for (i, beat) in [0.0, 0.25, 0.5, 0.75].into_iter().enumerate() {
            let got = generator_scalar(&ModSourceKind::Steps(fwd.clone()), ModTime::new(beat, 0.0)).unwrap();
            assert!((got - values[i]).abs() < 1e-6, "fwd beat={beat} got={got}");
        }
        // Backward: step 3,2,1,0。
        let bwd = mk(StepsDirection::Backward);
        for (i, beat) in [0.0, 0.25, 0.5, 0.75].into_iter().enumerate() {
            let got = generator_scalar(&ModSourceKind::Steps(bwd.clone()), ModTime::new(beat, 0.0)).unwrap();
            assert!(
                (got - values[3 - i]).abs() < 1e-6,
                "bwd beat={beat} got={got}"
            );
        }
        // PingPong: period = 2n-2 = 6 step、 idx 0,1,2,3,2,1。
        let pp = mk(StepsDirection::PingPong);
        let expect_idx = [0usize, 1, 2, 3, 2, 1];
        for (k, ei) in expect_idx.into_iter().enumerate() {
            let beat = k as f64 / 6.0;
            let got = generator_scalar(&ModSourceKind::Steps(pp.clone()), ModTime::new(beat, 0.0)).unwrap();
            assert!(
                (got - values[ei]).abs() < 1e-6,
                "pp k={k} beat={beat} got={got} expect_idx={ei}"
            );
        }
    }

    #[test]
    fn mseg_既定三角を位置でサンプル() {
        let c = MsegConfig::default(); // (0,0)-(0.5,1)-(1,0) linear, Loop, 1/4 note
        // q == cycle_pos の小数部 (period 1 beat)。
        let cases = [(0.0, 0.0), (0.25, 0.5), (0.5, 1.0), (0.75, 0.5), (1.0, 0.0)];
        for (beat, expected) in cases {
            let got = generator_scalar(&ModSourceKind::Mseg(c.clone()), ModTime::new(beat, 0.0)).unwrap();
            assert!(
                (got - expected).abs() < 1e-6,
                "beat={beat} got={got} expected={expected}"
            );
        }
    }

    #[test]
    fn mseg_oneshotはclampしloopはラップ() {
        let pts = vec![
            MsegPoint {
                time: 0.0,
                value: 0.0,
                curve: 0.0,
            },
            MsegPoint {
                time: 1.0,
                value: 1.0,
                curve: 0.0,
            },
        ];
        let one = MsegConfig {
            points: pts.clone(),
            rate: sync_quarter(),
            play_mode: MsegPlayMode::OneShot,
            retrigger: RetriggerMode::FreeRun,
        };
        // OneShot: beat 1.5 (cp=1.5) → clamp 1.0 → value 1.0。
        let got = generator_scalar(&ModSourceKind::Mseg(one.clone()), ModTime::new(1.5, 0.0)).unwrap();
        assert!((got - 1.0).abs() < 1e-6, "oneshot got={got}");
        // Loop: beat 1.5 → frac 0.5 → 0.5。
        let lp = MsegConfig {
            points: pts,
            play_mode: MsegPlayMode::Loop,
            ..one
        };
        let got = generator_scalar(&ModSourceKind::Mseg(lp), ModTime::new(1.5, 0.0)).unwrap();
        assert!((got - 0.5).abs() < 1e-6, "loop got={got}");
    }

    #[test]
    fn mseg_tensionが補間を歪ませる() {
        // curve=-0.25 → exponent 2 → t^2。 t=0.5 → 0.25。
        assert!((apply_tension(0.5, -0.25) - 0.25).abs() < 1e-6);
        // curve=0 → linear。
        assert!((apply_tension(0.5, 0.0) - 0.5).abs() < 1e-6);
        // 端点は常に固定。
        assert!((apply_tension(0.0, 0.7) - 0.0).abs() < 1e-6);
        assert!((apply_tension(1.0, -0.7) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn follower種別はnoneを返す() {
        let f = ModSourceKind::default(); // EnvelopeFollower
        assert!(generator_scalar(&f, ModTime::new(1.0, 1.0)).is_none());
    }
}
