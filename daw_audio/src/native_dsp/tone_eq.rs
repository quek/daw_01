//! 内蔵 Tone EQ (3 バンド固定周波数)。旧 `mixer/master_strip.rs` のトーン EQ 部分の移植。
//! 係数は [`common::dsp::tone_eq_stages`] (段順は `ToneEqBand::ALL`)。

use common::dsp::{Biquad, BiquadState, tone_eq_stages};
use common::model::ToneEqSettings;

use super::{NativeBlock, active_mask, block_len, run_stages};

const STAGES: usize = 3;

#[derive(Debug, Clone, Copy)]
pub struct ToneEqState {
    /// 3 段 × 2ch の遅延状態。
    state: [[BiquadState; 2]; STAGES],
    coeffs: [Biquad; STAGES],
    /// 素通しでない段の bit 集合。
    active: u8,
    cached: Option<(ToneEqSettings, f32)>,
}

impl Default for ToneEqState {
    fn default() -> Self {
        Self {
            state: [[BiquadState::default(); 2]; STAGES],
            coeffs: [Biquad::IDENTITY; STAGES],
            active: 0,
            cached: None,
        }
    }
}

impl ToneEqState {
    pub(super) fn process(&mut self, s: &ToneEqSettings, b: NativeBlock<'_>) -> f32 {
        let n = block_len(b.l, b.r, b.n);
        if n == 0 || b.sample_rate <= 0.0 {
            return 0.0;
        }
        if self.cached != Some((*s, b.sample_rate)) {
            self.coeffs = tone_eq_stages(s, b.sample_rate);
            self.active = active_mask(&self.coeffs);
            self.cached = Some((*s, b.sample_rate));
        }
        run_stages(&mut self.state, &self.coeffs, self.active, b.l, b.r, n);
        0.0
    }
}
