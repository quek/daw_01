//! r.md #129: 内蔵 device (Comp / EQ / Bus Comp / Tone EQ) と master Limiter の DSP 状態
//! (`docs/plan_rack_native_devices.md` §8.2)。旧 `mixer/channel_strip.rs` と
//! `mixer/master_strip.rs` を置き換える。
//!
//! 係数と利得計算の式は [`common::dsp`] が持つ (GUI のカーブ描画と同じ実装)。ここが持つのは
//! **状態** — バイクワッドの遅延、平滑済みの利得、係数のキャッシュ、リミッターの先読みリング。
//! ON/OFF (bypass) と切り替えの crossfade は呼び出し側 (`graph::native::run_native`) が持つので、
//! ここの `process` は常に「効いている」ものとして処理する。
//!
//! RT 規約: 確保・ロック・I/O なし。三角関数を呼ぶ係数の組み直しは値が変わった buffer だけ。
//! 状態は固定長 (`Copy`) なので、再 compile を跨ぐ引き継ぎ ([`NativeDsp::adopt_state_from`]) も
//! 構造体のコピーだけで済む。

mod bus_comp;
mod comp;
mod eq;
mod limiter;
mod tone_eq;
#[cfg(test)]
mod tests;

use common::dsp::{Biquad, BiquadState, StereoBiquad};
use common::model::{NativeKind, NativeParams};

pub use limiter::MasterLimiterState;

/// [`NativeDsp::process`] に渡す 1 buffer。
pub struct NativeBlock<'a> {
    /// 処理するバス (in-place)。
    pub l: &'a mut [f32],
    pub r: &'a mut [f32],
    pub n: usize,
    pub sample_rate: f32,
    /// 外部 (または自トラック Pre-FX) の検出信号。長さが `n` に足りない分は 0 とみなす。
    /// `None` = 自分の入力で検出する。SC を受けない種類は無視する。
    pub sidechain: Option<(&'a [f32], &'a [f32])>,
    /// SC Listen: SC フィルタを通した後の検出信号の書き先 (Comp 以外では常に `None`)。
    /// バス自体は通常どおり処理する。
    pub listen_out: Option<(&'a mut [f32], &'a mut [f32])>,
}

/// 内蔵 device 1 台ぶんの DSP 状態 (種類ごと)。
///
/// variant の大きさは揃っていない (EQ はバイクワッド 6 段ぶん) が、Box にしない: 状態は compile 時に
/// `ChainProgram::natives` へ並べて確保し、再 compile を跨ぐ引き継ぎは RT 上の固定長コピー (`Copy`) で行う。
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy)]
pub enum NativeDsp {
    Comp(comp::CompState),
    Eq(eq::EqState),
    BusComp(bus_comp::BusCompState),
    ToneEq(tone_eq::ToneEqState),
}

impl NativeDsp {
    #[must_use]
    pub fn new(kind: NativeKind) -> Self {
        match kind {
            NativeKind::Comp => Self::Comp(comp::CompState::default()),
            NativeKind::Eq => Self::Eq(eq::EqState::default()),
            NativeKind::BusComp => Self::BusComp(bus_comp::BusCompState::default()),
            NativeKind::ToneEq => Self::ToneEq(tone_eq::ToneEqState::default()),
        }
    }

    #[must_use]
    pub fn kind(&self) -> NativeKind {
        match self {
            Self::Comp(_) => NativeKind::Comp,
            Self::Eq(_) => NativeKind::Eq,
            Self::BusComp(_) => NativeKind::BusComp,
            Self::ToneEq(_) => NativeKind::ToneEq,
        }
    }

    /// バイクワッドの遅延・平滑値・係数キャッシュを無音の状態に戻す (確保なし)。
    /// bypass から戻る瞬間に呼ぶ — 古いフィルタ状態のまま再開すると、止めた時点の残響が鳴る。
    pub fn reset(&mut self) {
        *self = Self::new(self.kind());
    }

    /// `params` で 1 buffer を処理し、この buffer の GR (dB、0 以下) を返す。EQ 系は 0。
    /// `params` の種類が自分と違えば素通しして 0 (構造の変更は song と schedule が同じ便で届くので
    /// 通常は起きない、防御用)。
    pub fn process(&mut self, params: &NativeParams, b: NativeBlock<'_>) -> f32 {
        match (self, params) {
            (Self::Comp(st), NativeParams::Comp(s)) => st.process(s, b),
            (Self::Eq(st), NativeParams::Eq(s)) => st.process(s, b),
            (Self::BusComp(st), NativeParams::BusComp(s)) => st.process(s, b),
            (Self::ToneEq(st), NativeParams::ToneEq(s)) => st.process(s, b),
            _ => 0.0,
        }
    }

    /// 再 compile を跨ぐ状態の引き継ぎ。同じ種類のときだけ `old` の状態をコピーして `true`。
    pub fn adopt_state_from(&mut self, old: &NativeDsp) -> bool {
        if self.kind() != old.kind() {
            return false;
        }
        *self = *old;
        true
    }
}

/// `n` を処理できる長さに丸める (`l` / `r` の短い方)。
fn block_len(l: &[f32], r: &[f32], n: usize) -> usize {
    n.min(l.len()).min(r.len())
}

/// バイクワッドを直列に並べた段の共通処理 (EQ / Tone EQ)。`active` の bit が立っている段だけを
/// サンプルごとに回し (L / R 同時、[`StereoBiquad`])、素通し ([`Biquad::IDENTITY`]) の段は回さない (§8.8-3)。
///
/// 回さない段も、遅延状態だけは「素通しを回した場合」と同じ値に揃える (末尾 2 サンプルを
/// IDENTITY で通す)。これで段が後から有効になった瞬間の出力は、全段を常に回していた旧実装と
/// 同じになる (止まっていた間の古い遅延値から再開しない)。
fn run_stages<const N: usize>(
    state: &mut [[BiquadState; 2]; N],
    coeffs: &[Biquad; N],
    active: u8,
    l: &mut [f32],
    r: &mut [f32],
    n: usize,
) {
    for (k, (st, c)) in state.iter_mut().zip(coeffs).enumerate() {
        if active & (1 << k) != 0 {
            StereoBiquad::run(st, c, l, r, n);
        } else {
            for i in n.saturating_sub(2)..n {
                let _ = st[0].process(&Biquad::IDENTITY, l[i]);
                let _ = st[1].process(&Biquad::IDENTITY, r[i]);
            }
        }
    }
}

/// 素通しでない段の bit 集合 (段 k = bit k)。
fn active_mask(coeffs: &[Biquad]) -> u8 {
    coeffs
        .iter()
        .enumerate()
        .filter(|(_, c)| **c != Biquad::IDENTITY)
        .fold(0u8, |m, (k, _)| m | (1 << k))
}

/// Comp / Bus Comp 共通の 1 サンプルの利得計算 (§8.8-2)。検出レベル `det` から平滑済みの
/// `gain_db` (0 以下) を 1 サンプル進める。
///
/// - 検出が非有限なら 0 とみなす (NaN を平滑状態に入れると二度と戻らない、§18-H)。
/// - ニーの下端より小さい検出は `comp_static_gain_db` を呼ばずに 0 (同関数は
///   `over <= -half_knee` で厳密に 0 を返すので値は変わらない)。
/// - `gain_db` が −1e-6 dB より浅くなったら 0 に落とす (無音で非正規化数へ漂うのを断つ)。
#[inline]
#[allow(clippy::too_many_arguments)]
fn comp_gain_step(
    gain_db: &mut f32,
    det: f32,
    knee_floor_amp: f32,
    threshold_db: f32,
    ratio: f32,
    attack_c: f32,
    release_c: f32,
) {
    let det = if det.is_finite() { det } else { 0.0 };
    let target = if det <= knee_floor_amp {
        0.0
    } else {
        common::dsp::comp_static_gain_db(common::dsp::amp_to_db(det), threshold_db, ratio)
    };
    let coeff = if target < *gain_db { attack_c } else { release_c };
    *gain_db = target + (*gain_db - target) * coeff;
    if *gain_db > -1e-6 {
        *gain_db = 0.0;
    }
}

/// サイドチェインの `i` 番目のサンプル (足りない分は 0)。
#[inline]
fn sc_sample(sc: (&[f32], &[f32]), i: usize) -> (f32, f32) {
    (sc.0.get(i).copied().unwrap_or(0.0), sc.1.get(i).copied().unwrap_or(0.0))
}
