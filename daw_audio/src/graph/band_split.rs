//! r.md #112: Parallel の入力の分割 (`Split`) の RT 部 — [`Splitter`] が variant ごとの分割器を
//! 包み、 `ChainBegin` は「k 番目の出力」 を読むだけ。 [`MidSideSplit`] は `M = (L+R)/2`、
//! `S = (L-R)/2` を Mid chain `(M, M)` / Side chain `(S, -S)` に配る (和は `(L, R)` に戻る)。
//! r.md #114 の `Split::Selector` は [`crate::graph::selector_split::SelectorSplit`] (別ファイル)。
//! 以下は [`BandSplit`] (3 バンド、 `Split::Frequency3`) の説明。
//!
//! 4 次 Linkwitz-Riley (Butterworth 2 次 × 2、24 dB/oct) を 2 段で使う。 LR4 の LP と HP の
//! 和は同じ ω0 / Q の 2 次オールパスに等しい (Rane Note 160 / Linkwitz) ので、
//!
//! ```text
//! in ─┬─ LP4(low) ── AP2(high) ─────────► Low
//!     └─ HP4(low) ─┬─ LP4(high) ────────► Mid
//!                  └─ HP4(high) ────────► High
//! ```
//!
//! と組むと Low + Mid + High = AP2(low)·AP2(high)(in) で振幅は平坦になる (低域に上側
//! クロスオーバーのオールパスを掛けないと Mid + High の位相回転ぶんだけ和が凹む)。
//!
//! 係数は buffer 先頭で 1 回だけ、 周波数が変わったときにだけ組み直す (三角関数を
//! サンプルループに入れない)。 状態は再 compile を跨いで `adopt_state_from` で移送する
//! (捨てると編集のたびにクリックが乗る)。 RT 規約: 確保・ロック・I/O なし。

use common::channel_strip_dsp::{Biquad, BiquadState};
use common::model::{SPLIT_FREQ_RANGE, Split, SplitBand};

use crate::graph::selector_split::SelectorSplit;
use crate::mixer::MAX_FRAMES;

/// `Split` の variant ごとの分割器。 compile 時に `Split` から作り (`None` は分割器なし)、 RT は
/// [`Self::output`] で k 番目の出力を読む。
pub enum Splitter {
    /// 状態 (biquad 9 段 × 2ch + 出力 6 本) が大きいので Box (variant 間のサイズ差)。
    Frequency3(Box<BandSplit>),
    MidSide(MidSideSplit),
    /// r.md #114: chain 数ぶんの出力 + 鳴っている note の表を持つので Box。
    Selector(Box<SelectorSplit>),
}

impl Splitter {
    /// `n_chains` = この Parallel の chain 数 (`Selector` は chain ごとに出力を持つ)。
    pub fn new(split: Split, n_chains: usize) -> Option<Self> {
        match split {
            Split::None => None,
            Split::Frequency3 { .. } => Some(Self::Frequency3(Box::default())),
            Split::MidSide => Some(Self::MidSide(MidSideSplit::new())),
            Split::Selector { .. } => Some(Self::Selector(Box::new(SelectorSplit::new(n_chains)))),
        }
    }

    /// k 番目の出力 (L, R)。 出力数を超える k は `None`。
    pub fn output(&self, k: u8) -> Option<(&[f32], &[f32])> {
        match self {
            Self::Frequency3(bs) => {
                let band = match k {
                    0 => SplitBand::Low,
                    1 => SplitBand::Mid,
                    2 => SplitBand::High,
                    _ => return None,
                };
                Some(bs.band(band))
            }
            Self::MidSide(ms) => ms.output(k),
            Self::Selector(sel) => sel.output(k),
        }
    }

    /// 再 compile 跨ぎの状態移送 (同じ variant のときだけ。 違えば新品のまま)。
    pub fn adopt_state_from(&mut self, old: &Self) {
        match (self, old) {
            (Self::Frequency3(a), Self::Frequency3(b)) => a.adopt_state_from(b),
            (Self::MidSide(a), Self::MidSide(b)) => a.adopt_state_from(b),
            (Self::Selector(a), Self::Selector(b)) => a.adopt_state_from(b),
            _ => {}
        }
    }
}

/// Mid / Side 分割 (状態なし、 出力 buffer だけ)。 出力 0 = Mid `(M, M)`、 1 = Side `(S, -S)`。
pub struct MidSideSplit {
    /// `[出力][ch]`。
    out: [[Vec<f32>; 2]; 2],
}

impl MidSideSplit {
    pub fn new() -> Self {
        Self {
            out: [
                [vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES]],
                [vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES]],
            ],
        }
    }

    pub fn process(&mut self, in_l: &[f32], in_r: &[f32], n: usize) {
        let n = n.min(MAX_FRAMES).min(in_l.len()).min(in_r.len());
        for i in 0..n {
            let m = (in_l[i] + in_r[i]) * 0.5;
            let s = (in_l[i] - in_r[i]) * 0.5;
            self.out[0][0][i] = m;
            self.out[0][1][i] = m;
            self.out[1][0][i] = s;
            self.out[1][1][i] = -s;
        }
    }

    pub fn output(&self, k: u8) -> Option<(&[f32], &[f32])> {
        let o = self.out.get(k as usize)?;
        Some((o[0].as_slice(), o[1].as_slice()))
    }

    /// 前 buffer の出力を引き継ぐ (同 track 内の chain tap が前 buffer の snapshot を読むため)。
    pub fn adopt_state_from(&mut self, old: &Self) {
        for k in 0..2 {
            for ch in 0..2 {
                self.out[k][ch].copy_from_slice(&old.out[k][ch]);
            }
        }
    }
}

impl Default for MidSideSplit {
    fn default() -> Self {
        Self::new()
    }
}

/// Butterworth 2 次の Q (= 1/√2)。 これを 2 段重ねると LR4。
const Q_BUTTERWORTH: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// 周波数のこの相対差未満は「変わっていない」とみなして係数を組み直さない。
const FREQ_EPS_REL: f32 = 1e-4;

/// 1 チャンネルぶんの状態: `[lp_lo×2, hp_lo×2, ap_hi, lp_hi×2, hp_hi×2]`。
const STAGES: usize = 9;

/// Parallel 1 つぶんの分割器。 出力は [`Self::band`] で読む。
pub struct BandSplit {
    sample_rate: f32,
    low_hz: f32,
    high_hz: f32,
    lp_lo: Biquad,
    hp_lo: Biquad,
    ap_hi: Biquad,
    lp_hi: Biquad,
    hp_hi: Biquad,
    state: [[BiquadState; STAGES]; 2],
    /// `[ch][band]` の出力 (MAX_FRAMES)。
    out: [[Vec<f32>; 3]; 2],
    /// クロスオーバー周波数の per-sample ramp (automation + 変調)。 係数は buffer 終端の値で組む。
    pub low_ramp: Vec<f32>,
    pub high_ramp: Vec<f32>,
}

impl BandSplit {
    pub fn new() -> Self {
        Self {
            sample_rate: 0.0,
            low_hz: 0.0,
            high_hz: 0.0,
            lp_lo: Biquad::IDENTITY,
            hp_lo: Biquad::IDENTITY,
            ap_hi: Biquad::IDENTITY,
            lp_hi: Biquad::IDENTITY,
            hp_hi: Biquad::IDENTITY,
            state: [[BiquadState::default(); STAGES]; 2],
            out: [
                [vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES]],
                [vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES]],
            ],
            low_ramp: vec![0.0; MAX_FRAMES],
            high_ramp: vec![0.0; MAX_FRAMES],
        }
    }

    /// 帯域 `band` の出力 (L, R)。
    pub fn band(&self, band: SplitBand) -> (&[f32], &[f32]) {
        let k = band_index(band);
        (self.out[0][k].as_slice(), self.out[1][k].as_slice())
    }

    /// 係数を `low_hz` / `high_hz` (値域へ丸め、 `low <= high`) で組む。 前回と同じなら何もしない。
    fn set_frequencies(&mut self, sample_rate: u32, low_hz: f32, high_hz: f32) {
        let sr = sample_rate.max(1) as f32;
        let low = SPLIT_FREQ_RANGE.clamp(if low_hz.is_finite() { low_hz } else { Split::DEFAULT_FREQS.0 });
        let high = SPLIT_FREQ_RANGE
            .clamp(if high_hz.is_finite() { high_hz } else { Split::DEFAULT_FREQS.1 })
            .max(low);
        let same = |a: f32, b: f32| (a - b).abs() <= a.abs().max(b.abs()) * FREQ_EPS_REL;
        if sr == self.sample_rate && same(low, self.low_hz) && same(high, self.high_hz) {
            return;
        }
        if sr != self.sample_rate {
            // レート変更で過去の状態は意味を失う。
            for ch in &mut self.state {
                for st in ch.iter_mut() {
                    st.reset();
                }
            }
        }
        self.sample_rate = sr;
        self.low_hz = low;
        self.high_hz = high;
        self.lp_lo = Biquad::low_pass(sr, low, Q_BUTTERWORTH);
        self.hp_lo = Biquad::high_pass(sr, low, Q_BUTTERWORTH);
        self.ap_hi = Biquad::all_pass(sr, high, Q_BUTTERWORTH);
        self.lp_hi = Biquad::low_pass(sr, high, Q_BUTTERWORTH);
        self.hp_hi = Biquad::high_pass(sr, high, Q_BUTTERWORTH);
    }

    /// `in_l/r[..n]` を 3 帯域に分ける。 周波数は `low_ramp` / `high_ramp` の buffer 終端の値
    /// (呼び側が先に埋める)。
    pub fn process(&mut self, sample_rate: u32, in_l: &[f32], in_r: &[f32], n: usize) {
        let n = n.min(MAX_FRAMES).min(in_l.len()).min(in_r.len());
        if n == 0 {
            return;
        }
        let (low, high) = (self.low_ramp[n - 1], self.high_ramp[n - 1]);
        self.set_frequencies(sample_rate, low, high);
        for (ch, input) in [in_l, in_r].into_iter().enumerate() {
            let st = &mut self.state[ch];
            let out = &mut self.out[ch];
            for i in 0..n {
                let x = input[i];
                let lo = st[0].process(&self.lp_lo, x);
                let lo = st[1].process(&self.lp_lo, lo);
                let hi = st[2].process(&self.hp_lo, x);
                let hi = st[3].process(&self.hp_lo, hi);
                out[0][i] = st[4].process(&self.ap_hi, lo);
                let mid = st[5].process(&self.lp_hi, hi);
                out[1][i] = st[6].process(&self.lp_hi, mid);
                let top = st[7].process(&self.hp_hi, hi);
                out[2][i] = st[8].process(&self.hp_hi, top);
            }
        }
    }

    /// 再 compile 跨ぎの状態移送 (RT 上、 f32 コピーのみ)。
    pub fn adopt_state_from(&mut self, old: &Self) {
        self.sample_rate = old.sample_rate;
        self.low_hz = old.low_hz;
        self.high_hz = old.high_hz;
        self.lp_lo = old.lp_lo;
        self.hp_lo = old.hp_lo;
        self.ap_hi = old.ap_hi;
        self.lp_hi = old.lp_hi;
        self.hp_hi = old.hp_hi;
        self.state = old.state;
        for ch in 0..2 {
            for k in 0..3 {
                self.out[ch][k].copy_from_slice(&old.out[ch][k]);
            }
        }
    }
}

impl Default for BandSplit {
    fn default() -> Self {
        Self::new()
    }
}

fn band_index(band: SplitBand) -> usize {
    match band {
        SplitBand::Low => 0,
        SplitBand::Mid => 1,
        SplitBand::High => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt()
    }

    /// 定常正弦を `secs` 秒流し、 最後の buffer の (Low, Mid, High) 出力を返す。
    fn settle(f: f32, low_hz: f32, high_hz: f32, n: usize, secs: usize) -> (BandSplit, usize) {
        let sr = 48_000u32;
        let mut bs = BandSplit::new();
        let blocks = (sr as usize * secs) / n;
        for b in 0..blocks {
            let x: Vec<f32> = (0..n)
                .map(|i| (std::f32::consts::TAU * f * ((b * n + i) as f32) / sr as f32).sin())
                .collect();
            bs.low_ramp[..n].fill(low_hz);
            bs.high_ramp[..n].fill(high_hz);
            bs.process(sr, &x, &x, n);
        }
        (bs, n)
    }

    fn sum_db(bs: &BandSplit, n: usize) -> f32 {
        let (lo, _) = bs.band(SplitBand::Low);
        let (mid, _) = bs.band(SplitBand::Mid);
        let (hi, _) = bs.band(SplitBand::High);
        let sum: Vec<f32> = (0..n).map(|i| lo[i] + mid[i] + hi[i]).collect();
        20.0 * (rms(&sum) / std::f32::consts::FRAC_1_SQRT_2).log10()
    }

    /// 定常正弦を通し、 (a) 3 帯域の和の振幅が入力と同じ、 (b) その周波数の帯域にだけ
    /// エネルギーが集まる、 を帯域ごとに確かめる。
    #[test]
    fn three_bands_sum_flat_and_isolate_their_frequency() {
        let in_rms = std::f32::consts::FRAC_1_SQRT_2;
        // 測定窓 (960 frames = 20 ms) に各周波数の周期が整数個乗るよう選ぶ (rms が窓で揺れない)。
        for (f, expect) in [(50.0f32, SplitBand::Low), (700.0, SplitBand::Mid), (8_000.0, SplitBand::High)] {
            let (bs, n) = settle(f, 200.0, 2_000.0, 960, 4);
            let db = sum_db(&bs, n);
            assert!(db.abs() < 0.05, "f={f}: 和の振幅が平坦でない ({db:.3} dB)");
            for band in [SplitBand::Low, SplitBand::Mid, SplitBand::High] {
                let (l, _) = bs.band(band);
                let r = rms(&l[..n]);
                if band == expect {
                    assert!(r > in_rms * 0.9, "f={f}: 期待帯域 {band:?} rms {r}");
                } else {
                    assert!(r < in_rms * 0.1, "f={f}: {band:?} への漏れ rms {r}");
                }
            }
        }
    }

    /// Mid / Side: `(M, M) + (S, -S) = (L, R)`、 Mid は両 ch 同じ、 Side は逆相。
    #[test]
    fn mid_side_outputs_reconstruct_the_input_when_summed() {
        let l = [1.0f32, -0.5, 0.25, 0.0];
        let r = [0.5f32, 0.5, -0.75, 0.0];
        let mut ms = MidSideSplit::new();
        ms.process(&l, &r, 4);
        let (ml, mr) = ms.output(0).unwrap();
        let (sl, sr) = ms.output(1).unwrap();
        for i in 0..4 {
            assert_eq!(ml[i], mr[i], "Mid は両 ch 同じ");
            assert_eq!(sl[i], -sr[i], "Side は逆相");
            assert!((ml[i] + sl[i] - l[i]).abs() < 1e-6);
            assert!((mr[i] + sr[i] - r[i]).abs() < 1e-6);
        }
        assert!(ms.output(2).is_none());
    }

    /// クロスオーバー周波数そのものでも和は平坦 (LR の -6 dB × 2 が同相で足し合う)。
    #[test]
    fn sum_is_flat_at_the_crossover_frequencies() {
        for f in [200.0f32, 2_000.0] {
            let (bs, n) = settle(f, 200.0, 2_000.0, 480, 1);
            let db = sum_db(&bs, n);
            assert!(db.abs() < 0.05, "f={f}: {db:.3} dB");
        }
    }
}
