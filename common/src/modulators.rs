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
    /// r.md #116: retrigger の起点からの経過拍 (`FromBeat` は anchor から、 `FreeRun` は曲頭から、
    /// `Note` は note-on から)。 LFO の Delay / Fade In が読む。 rate の変調とは無関係 (時間そのもの)。
    pub elapsed_beats: f64,
    /// r.md #117: 起点からの経過秒 (ADSR の時間軸)。
    pub elapsed_secs: f64,
    /// r.md #117: note-off からの経過秒 (`None` = 押している / ノート起点でない)。
    pub released_secs: Option<f64>,
    /// r.md #117: ADSR の `[attack_ms, decay_ms, sustain, release_ms]` (変調後の実効値)。
    pub adsr: [f32; 4],
    /// LFO の開始位相 (0..=1)。
    pub lfo_phase: f32,
    /// LFO Pulse の duty (0..=1)。
    pub pulse_width: f32,
    /// r.md #116: LFO の Shape (位相の曲げ、 0.5 = そのまま)。
    pub lfo_shape_amt: f32,
    /// r.md #116: LFO の Jitter (0..=1)。
    pub lfo_jitter: f32,
    /// r.md #116: LFO の Smooth (0..=1)。
    pub lfo_smooth: f32,
    /// Random の Stepped↔Smoothed モーフ (0..=1)。
    pub random_smooth: f32,
    /// Steps の slew (0..=1)。
    pub steps_slew: f32,
}

impl GenParams {
    /// 変調が無いときの値 (config そのまま)。
    #[must_use]
    pub fn from_config(kind: &ModSourceKind, cycle_pos: f64, t: ModTime, retrigger: &RetriggerMode) -> Self {
        let lfo = match kind {
            ModSourceKind::Lfo(c) => Some(c),
            _ => None,
        };
        let released_secs = match retrigger {
            RetriggerMode::Note => t.release_secs.map(|r| t.secs - r).filter(|d| *d >= 0.0),
            _ => None,
        };
        Self {
            cycle_pos,
            elapsed_beats: elapsed_beats(t, retrigger),
            elapsed_secs: elapsed_secs(t, retrigger),
            released_secs,
            adsr: match kind {
                ModSourceKind::Adsr(c) => [c.attack_ms, c.decay_ms, c.sustain, c.release_ms],
                _ => [0.0; 4],
            },
            lfo_phase: lfo.map_or(0.0, |c| c.phase),
            pulse_width: match lfo.map(|c| c.shape) {
                Some(LfoShape::Pulse { width }) => width,
                _ => 0.5,
            },
            lfo_shape_amt: lfo.map_or(0.5, |c| c.shape_amt),
            lfo_jitter: lfo.map_or(0.0, |c| c.jitter),
            lfo_smooth: lfo.map_or(0.0, |c| c.smooth),
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

/// r.md #116: retrigger の起点からの経過拍 (LFO の Delay / Fade In の時間軸)。 `Note` は
/// note-on (`t.anchor_beat`) から。
#[inline]
#[must_use]
pub fn elapsed_beats(t: ModTime, retrigger: &RetriggerMode) -> f64 {
    match retrigger {
        RetriggerMode::FreeRun => t.beat,
        RetriggerMode::FromBeat { anchor_beat } => t.beat - anchor_beat,
        RetriggerMode::Note => t.beat - t.anchor_beat,
    }
}

/// 起点からの経過秒 (ADSR の時間軸)。 `FreeRun` は曲頭から。
#[inline]
#[must_use]
pub fn elapsed_secs(t: ModTime, retrigger: &RetriggerMode) -> f64 {
    match retrigger {
        RetriggerMode::FreeRun => t.secs,
        RetriggerMode::FromBeat { .. } | RetriggerMode::Note => t.secs - t.anchor_secs,
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
        ModSourceKind::Adsr(_) => Some(eval_adsr(p)),
    }
}

/// 変調が無い generator の出力スカラー (unipolar 0..=1)。閉形式なので O(1)。
/// envelope follower は `None`。 `Note` 起点のソースは `t.anchor_*` (note-on) と
/// `t.release_secs` (note-off) を呼び側が埋める (per-note の評価点 = engine の voice 表 /
/// global の最新ノート / GUI プレビュー)。
#[inline]
pub fn generator_scalar(kind: &ModSourceKind, t: ModTime) -> Option<f32> {
    let retrig = kind.retrigger()?;
    let cp = kind.rate().map_or(0.0, |rate| cycle_pos(&rate, t, &retrig));
    eval_generator(kind, GenParams::from_config(kind, cp, t, &retrig))
}

/// r.md #117: ADSR (時間は秒)。 `t_on` = note-on からの秒、 `released` = note-off からの秒
/// (`None` = 押している)。 attack 線形、 decay / release は `exp(-3 t / T)`。
#[inline]
#[must_use]
pub fn adsr_env(attack_ms: f32, decay_ms: f32, sustain: f32, release_ms: f32, t_on: f64, released: Option<f64>) -> f32 {
    if t_on < 0.0 {
        return 0.0;
    }
    let s = f64::from(sustain.clamp(0.0, 1.0));
    let a = f64::from(attack_ms.max(0.0)) * 1e-3;
    let d = f64::from(decay_ms.max(0.0)) * 1e-3;
    let held = |t: f64| -> f64 {
        if t < a {
            if a > 0.0 { t / a } else { 1.0 }
        } else if d > 0.0 {
            s + (1.0 - s) * (-3.0 * (t - a) / d).exp()
        } else {
            s
        }
    };
    let v = match released {
        None => held(t_on),
        Some(tr) if tr <= 0.0 => held(t_on),
        Some(tr) => {
            let r = f64::from(release_ms.max(0.0)) * 1e-3;
            let at_off = held(t_on - tr);
            if r > 0.0 { at_off * (-3.0 * tr / r).exp() } else { 0.0 }
        }
    };
    v.clamp(0.0, 1.0) as f32
}

#[inline]
fn eval_adsr(g: GenParams) -> f32 {
    let [a, d, s, r] = g.adsr;
    adsr_env(a, d, s, r, g.elapsed_secs, g.released_secs)
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
    /// r.md #117: `Note` の起点 (note-on の拍)。 `FromBeat` / `FreeRun` では使わない。
    pub anchor_beat: f64,
    /// r.md #117: `Note` の note-off の絶対秒 (`None` = 押している)。 ADSR の release が読む。
    pub release_secs: Option<f64>,
    /// `FromBeat { anchor_beat }` を秒へ換算したもの (`Note` では note-on の秒)。`FreeRun` では使わない。
    pub anchor_secs: f64,
}

impl ModTime {
    /// テンポ一定 (または Sync のみ使う) 文脈の簡易構築。
    #[must_use]
    pub fn new(beat: f64, secs: f64) -> Self {
        Self { beat, secs, anchor_beat: 0.0, release_secs: None, anchor_secs: 0.0 }
    }

    /// r.md #117: note-on `(anchor_beat, anchor_secs)` を起点にした時刻 (`Note` 用)。
    #[must_use]
    pub fn at_note(beat: f64, secs: f64, anchor_beat: f64, anchor_secs: f64, release_secs: Option<f64>) -> Self {
        Self { beat, secs, anchor_beat, release_secs, anchor_secs }
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
            let beat = elapsed_beats(t, retrigger);
            beat / rate.period_beats()
        }
        crate::model::ModRateMode::Free => {
            let secs = elapsed_secs(t, retrigger);
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

/// r.md #116 Shape: 位相 `p` (0..=1) を `amt` で曲げる区分線形写像。 `amt = 0.5` は恒等
/// (Live "bends or skews")。 継ぎ目 (位相の進む速さが変わる点) は **波形の傾きが 0 の点**に
/// 置き、 出力の傾きが不連続にならないようにする:
/// - Sine (山 p = 0.25 / 谷 p = 0.75): 山を `amt` の位置 `c` (0.01..0.49) へ、 谷を `1 - c` へ寄せる
///   3 区間。 折り返し (p = 0) の前後は同じ速さなので全域で傾きが連続。 `amt → 0` で山が
///   先頭に寄ってなだらかな SawDown、 `→ 1` で SawUp に近づく。
/// - それ以外 (Triangle の山 p = 0.5 / 谷 p = 0、 Saw / Square / Pulse): 中点 (p = 0.5) を `amt`
///   の位置へ寄せる 2 区間。 Triangle は `amt → 0` で SawDown、 `→ 1` で SawUp、 Square は
///   duty が変わる。
#[inline]
fn warp_phase(shape: LfoShape, p: f64, amt: f32) -> f64 {
    let amt = f64::from(amt.clamp(0.0, 1.0));
    match shape {
        LfoShape::Sine => {
            let c = amt * 0.48 + 0.01;
            if p < c {
                0.25 * p / c
            } else if p < 1.0 - c {
                0.25 + 0.5 * (p - c) / (1.0 - 2.0 * c)
            } else {
                0.75 + 0.25 * (p - (1.0 - c)) / c
            }
        }
        _ => {
            let c = amt * 0.96 + 0.02;
            if p < c { 0.5 * p / c } else { 0.5 + 0.5 * (p - c) / (1.0 - c) }
        }
    }
}

/// r.md #116 Jitter の乱数 (unipolar 0..=1): 1 周を [`LFO_JITTER_STEPS_PER_CYCLE`] 段に切り、
/// 段の間は線形補間 (段差クリックを出さない)。 `seed` と周期位置の純関数。
#[inline]
fn jitter_noise(seed: u64, cycle_pos: f64) -> f32 {
    let x = cycle_pos * crate::model::LFO_JITTER_STEPS_PER_CYCLE;
    let step = x.floor();
    let frac = (x - step) as f32;
    lerp(random_unit(seed, step as i64), random_unit(seed, step as i64 + 1), frac)
}

/// Smooth 以外を掛けた 1 点の LFO 値 (波形 → Shape → Steps → Jitter)。
#[inline]
fn lfo_point(c: &LfoConfig, g: GenParams, cycle_pos: f64) -> f32 {
    let p = (cycle_pos + f64::from(g.lfo_phase)).rem_euclid(1.0);
    let p = warp_phase(c.shape, p, g.lfo_shape_amt);
    // Pulse の duty は変調されうるので `GenParams` 側を使う (config の値は base)。
    let shape = match c.shape {
        LfoShape::Pulse { .. } => LfoShape::Pulse { width: g.pulse_width },
        other => other,
    };
    let mut v = lfo_shape_value(shape, p);
    if c.steps >= 2 {
        let n = f32::from(c.steps.min(crate::model::LFO_STEPS_MAX));
        v = (v * n).floor().min(n - 1.0) / (n - 1.0);
    }
    let jitter = g.lfo_jitter.clamp(0.0, 1.0);
    if jitter > 0.0 {
        v += jitter * (jitter_noise(c.seed, cycle_pos) - 0.5);
    }
    v.clamp(0.0, 1.0)
}

/// Smooth の平均に使う点数 (前後対称、 中心を含む奇数)。
const LFO_SMOOTH_TAPS: i32 = 9;

#[inline]
fn eval_lfo(c: &LfoConfig, g: GenParams) -> f32 {
    let smooth = g.lfo_smooth.clamp(0.0, 1.0);
    let v = if smooth <= 0.0 {
        lfo_point(c, g, g.cycle_pos)
    } else {
        // 周期位置の前後 `smooth × 1/4 周` の箱平均 (時間の純関数のまま鈍らせる)。
        let half = f64::from(smooth) * 0.125;
        let mut sum = 0.0f32;
        for i in -(LFO_SMOOTH_TAPS / 2)..=(LFO_SMOOTH_TAPS / 2) {
            let off = half * f64::from(i) / f64::from(LFO_SMOOTH_TAPS / 2);
            sum += lfo_point(c, g, g.cycle_pos + off);
        }
        sum / LFO_SMOOTH_TAPS as f32
    };
    // Delay / Fade In: 起点からの経過拍で「開始値 → 波形」 を混ぜる。 どちらも 0 なら
    // 従来どおり (起点より前でも波形をそのまま出す)。
    let env = lfo_fade_env(c, g.elapsed_beats);
    if env >= 1.0 {
        return v;
    }
    let start = lfo_point(c, g, 0.0);
    lerp(start, v, env)
}

/// r.md #116: Delay / Fade In の包絡 (0..=1)。 起点からの経過拍 `elapsed_beats` で、 Delay の
/// 間は 0、 その後 Fade In をかけて線形に 1 へ。 どちらも 0 なら常に 1 (起点より前でも
/// 波形をそのまま出す)。 出力は `lerp(開始値, 波形, env)`。 プレビューの「今の振幅」 も
/// この 1 本で出す。
#[inline]
#[must_use]
pub fn lfo_fade_env(c: &LfoConfig, elapsed_beats: f64) -> f32 {
    if c.delay_beats <= 0.0 && c.fade_in_beats <= 0.0 {
        return 1.0;
    }
    let t = elapsed_beats - f64::from(c.delay_beats.max(0.0));
    if t <= 0.0 {
        0.0
    } else if c.fade_in_beats <= 0.0 {
        1.0
    } else {
        #[allow(clippy::cast_possible_truncation)]
        {
            (t / f64::from(c.fade_in_beats)).min(1.0) as f32
        }
    }
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
            ..LfoConfig::default()
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
            ..LfoConfig::default()
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
            ..LfoConfig::default()
        };
        // 2 Hz: 0.25 秒で半周 → SawUp=0.5。
        let got = generator_scalar(&ModSourceKind::Lfo(c), ModTime::new(0.0, 0.25)).unwrap();
        assert!((got - 0.5).abs() < 1e-6, "got={got}");
    }

    fn lfo_at(c: LfoConfig, beat: f64) -> f32 {
        generator_scalar(&ModSourceKind::Lfo(c), ModTime::new(beat, 0.0)).unwrap()
    }

    /// r.md #116 Shape: 0.5 は恒等、 Triangle は 0 側で SawDown に、 1 側で SawUp に寄る
    /// (山の位置が `amt` へ動く)。
    #[test]
    fn lfo_shapeは山の位置を動かし中央では恒等() {
        let tri = |amt| LfoConfig { shape: LfoShape::Triangle, rate: sync_quarter(), shape_amt: amt, ..LfoConfig::default() };
        for beat in [0.0, 0.1, 0.37, 0.5, 0.8] {
            let plain = lfo_shape_value(LfoShape::Triangle, beat);
            assert!((lfo_at(tri(0.5), beat) - plain).abs() < 1e-6, "0.5 は恒等 (beat={beat})");
        }
        // 山 (値 1) は amt の位置に来る (amt = 0.25 → 0.25 拍、 0.75 → 0.75 拍)。
        assert!((lfo_at(tri(0.25), 0.26) - 1.0).abs() < 1e-2);
        assert!((lfo_at(tri(0.75), 0.74) - 1.0).abs() < 1e-2);
        // amt 0.5 の Triangle は 0.5 拍で 1。
        assert!((lfo_at(tri(0.5), 0.5) - 1.0).abs() < 1e-6);

        // Sine: 山は amt の位置 (0.17 → 山が 0.0916 拍 = 0.17·0.48+0.01)、 0.5 は恒等、 そして
        // **全域で傾きが連続** (継ぎ目が山谷にあるので、 中線を横切る所に角が出ない)。
        let sine = |amt| LfoConfig { shape: LfoShape::Sine, rate: sync_quarter(), shape_amt: amt, ..LfoConfig::default() };
        for beat in [0.0, 0.1, 0.37, 0.5, 0.8] {
            let plain = lfo_shape_value(LfoShape::Sine, beat);
            assert!((lfo_at(sine(0.5), beat) - plain).abs() < 1e-6, "0.5 は恒等 (beat={beat})");
        }
        assert!((lfo_at(sine(0.17), 0.0916) - 1.0).abs() < 1e-3);
        let h = 1e-3;
        let mut max_jump = 0.0f32;
        let mut prev_slope: Option<f32> = None;
        for i in 0..2000 {
            let b = f64::from(i) * h;
            let slope = (lfo_at(sine(0.17), b + h) - lfo_at(sine(0.17), b)) / h as f32;
            if let Some(p) = prev_slope {
                max_jump = max_jump.max((slope - p).abs());
            }
            prev_slope = Some(slope);
        }
        // 傾きの変化は曲率由来の小さな値だけ (継ぎ目の速さ比 4.4 倍が中線で出ると ~10 になる)。
        assert!(max_jump < 0.2, "Sine の傾きが不連続: max_jump={max_jump}");
    }

    /// r.md #116 Steps: n 段に量子化 (端は 0 と 1)。 0 / 1 は off。
    #[test]
    fn lfo_stepsは出力をn段に量子化する() {
        let saw = |steps| LfoConfig { shape: LfoShape::SawUp, rate: sync_quarter(), steps, ..LfoConfig::default() };
        assert!((lfo_at(saw(0), 0.3) - 0.3).abs() < 1e-6, "off");
        assert!((lfo_at(saw(1), 0.3) - 0.3).abs() < 1e-6, "1 段も off");
        // 4 段: 0.3 → floor(1.2) = 1 → 1/3。 0.99 → 3/3。
        assert!((lfo_at(saw(4), 0.3) - 1.0 / 3.0).abs() < 1e-6);
        assert!((lfo_at(saw(4), 0.99) - 1.0).abs() < 1e-6);
        assert_eq!(lfo_at(saw(2), 0.49), 0.0);
        assert_eq!(lfo_at(saw(2), 0.51), 1.0);
    }

    /// r.md #116 Jitter / Smooth: 決定論 (同じ beat で同じ値)、 jitter は seed で変わり、
    /// smooth は矩形の段差を鈍らせる。 どちらも 0 なら従来と bit 一致。
    #[test]
    fn lfo_jitterは決定論的でsmoothは段差を鈍らせる() {
        let base = LfoConfig { shape: LfoShape::Square, rate: sync_quarter(), ..LfoConfig::default() };
        let jit = |seed| LfoConfig { jitter: 0.5, seed, ..base };
        let a1 = lfo_at(jit(1), 0.3);
        assert_eq!(a1, lfo_at(jit(1), 0.3), "同 beat で同値");
        assert_ne!(a1, lfo_at(jit(2), 0.3), "seed で変わる");
        assert_ne!(a1, lfo_at(base, 0.3), "jitter が乗っている");
        // smooth: 矩形の立ち下がり (0.5 拍) の直前直後が 1 / 0 でなく中間になる。
        let sm = LfoConfig { smooth: 1.0, ..base };
        let v = lfo_at(sm, 0.5);
        assert!(v > 0.2 && v < 0.8, "段差が鈍る: {v}");
        assert_eq!(lfo_at(base, 0.5), 0.0, "smooth 0 は従来どおり");
    }

    /// r.md #117: `Note` は note-on (`anchor_beat` / `anchor_secs`) を起点に走る。 起点が無い
    /// (`ModTime::new`) と cycle 0 = 開始値。 Delay / Fade In もノートから数える。
    #[test]
    fn note_retriggerはnote_onを起点に走る() {
        let saw = LfoConfig {
            shape: LfoShape::SawUp,
            rate: sync_quarter(),
            retrigger: RetriggerMode::Note,
            ..LfoConfig::default()
        };
        let k = ModSourceKind::Lfo(saw);
        // 起点 8 拍で鳴ったノート: 8.25 拍で 0.25 周。
        let v = generator_scalar(&k, ModTime::at_note(8.25, 0.0, 8.0, 0.0, None)).unwrap();
        assert!((v - 0.25).abs() < 1e-6, "{v}");
        // 起点無し = 開始値 (0)。
        assert_eq!(generator_scalar(&k, ModTime::new(8.25, 0.0)).unwrap(), 0.25, "FreeRun 扱いではなく beat そのもの");
        // Fade In もノートから: 1 拍で 0 → 1。 8.5 拍 (0.5 拍後) は 0.5 × 0.5。
        let fade = ModSourceKind::Lfo(LfoConfig { fade_in_beats: 1.0, ..saw });
        let v = generator_scalar(&fade, ModTime::at_note(8.5, 0.0, 8.0, 0.0, None)).unwrap();
        assert!((v - 0.25).abs() < 1e-6, "{v}");
    }

    /// r.md #117: ADSR は秒基準。 attack 線形 → decay 指数 → sustain、 note-off 後は release 指数。
    #[test]
    fn adsrは押している間attack_decay_sustainで離すとreleaseする() {
        let (a, d, s, r) = (100.0, 200.0, 0.5, 100.0);
        let env = |t, rel| adsr_env(a, d, s, r, t, rel);
        assert_eq!(env(-0.1, None), 0.0, "起点前は 0");
        assert!((env(0.05, None) - 0.5).abs() < 1e-6, "attack 半分");
        assert!((env(0.1, None) - 1.0).abs() < 1e-6, "attack 終端で 1");
        let mid = env(0.2, None);
        assert!(mid > 0.5 && mid < 1.0, "decay 途中: {mid}");
        assert!((env(2.0, None) - 0.5).abs() < 1e-3, "sustain に収束");
        // 2.0 秒で離した: release 0.1 s 後は 5% 以下。
        assert!(env(2.05, Some(0.05)) < env(2.0, None));
        assert!(env(2.1, Some(0.1)) < 0.5 * 0.05 + 1e-6);
        // attack の途中で離すと、 その時点の値から release。
        let at_off = env(0.05, None);
        assert!((env(0.05, Some(0.0)) - at_off).abs() < 1e-6);
        // 種別として評価: config 経由でも同じ。
        let k = ModSourceKind::Adsr(crate::model::AdsrConfig { attack_ms: a, decay_ms: d, sustain: s, release_ms: r });
        let v = generator_scalar(&k, ModTime::at_note(0.0, 8.05, 0.0, 8.0, None)).unwrap();
        assert!((v - 0.5).abs() < 1e-6, "{v}");
        let v = generator_scalar(&k, ModTime::at_note(0.0, 10.1, 0.0, 8.0, Some(10.0))).unwrap();
        assert!(v < 0.03, "release 後: {v}");
    }

    /// r.md #116 Delay / Fade In: 起点 (FromBeat の anchor) から delay の間は開始値、 その後
    /// fade_in かけて線形に波形へ。 どちらも 0 なら起点より前でも波形そのまま。
    #[test]
    fn lfo_delayとfade_inは起点からの経過拍で波形を混ぜる() {
        let saw = LfoConfig {
            shape: LfoShape::SawUp,
            rate: sync_quarter(),
            retrigger: RetriggerMode::FromBeat { anchor_beat: 4.0 },
            ..LfoConfig::default()
        };
        // 起点より前でも波形 (従来): 3.5 拍 = anchor から -0.5 → SawUp = 0.5。
        assert!((lfo_at(saw, 3.5) - 0.5).abs() < 1e-6);
        let env = LfoConfig { delay_beats: 1.0, fade_in_beats: 2.0, ..saw };
        // 開始値 = 位相 0 の値 = 0。 delay 中 (4.0..5.0) は 0。
        assert_eq!(lfo_at(env, 4.5), 0.0);
        // 6.0 拍 = delay 後 1 拍 = fade 半分: 波形 (SawUp、 2 周目の 0 → 0.0) … 6.25 で波形 0.25 × 0.625。
        let v = lfo_at(env, 6.25);
        assert!((v - 0.25 * 0.625).abs() < 1e-6, "fade 途中: {v}");
        // fade 完了後は波形そのまま。
        assert!((lfo_at(env, 7.5) - 0.5).abs() < 1e-6);
        // delay だけ (fade 0) は段で切り替わる。
        let d = LfoConfig { delay_beats: 1.0, ..saw };
        assert_eq!(lfo_at(d, 4.9), 0.0);
        assert!((lfo_at(d, 5.5) - 0.5).abs() < 1e-6);
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
