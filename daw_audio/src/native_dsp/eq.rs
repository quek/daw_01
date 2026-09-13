//! 内蔵 EQ (6 バンド)。旧 `mixer/channel_strip.rs` の EQ 部分の移植。
//! 係数は [`common::dsp::eq_stages`] (段順は `EqBand::ALL`、OFF のバンドは素通し)。

use common::dsp::{Biquad, BiquadState, EQ_STAGES, eq_stages};
use common::model::EqSettings;

use super::{NativeBlock, active_mask, block_len, run_stages};

#[derive(Debug, Clone, Copy)]
pub struct EqState {
    /// 6 段 × 2ch の遅延状態。
    state: [[BiquadState; 2]; EQ_STAGES],
    coeffs: [Biquad; EQ_STAGES],
    /// 素通しでない段の bit 集合 (`coeffs` と一緒に組み直す)。
    active: u8,
    /// 係数キャッシュのキー。一致する間は組み直さない。
    cached: Option<(EqSettings, f32)>,
}

impl Default for EqState {
    fn default() -> Self {
        Self {
            state: [[BiquadState::default(); 2]; EQ_STAGES],
            coeffs: [Biquad::IDENTITY; EQ_STAGES],
            active: 0,
            cached: None,
        }
    }
}

impl EqState {
    pub(super) fn process(&mut self, s: &EqSettings, b: NativeBlock<'_>) -> f32 {
        let n = block_len(b.l, b.r, b.n);
        if n == 0 || b.sample_rate <= 0.0 {
            return 0.0;
        }
        if self.cached != Some((*s, b.sample_rate)) {
            self.coeffs = eq_stages(s, b.sample_rate);
            self.active = active_mask(&self.coeffs);
            self.cached = Some((*s, b.sample_rate));
        }
        run_stages(&mut self.state, &self.coeffs, self.active, b.l, b.r, n);
        0.0
    }
}
