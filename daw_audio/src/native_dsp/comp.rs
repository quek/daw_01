//! 内蔵 Comp (チャンネルコンプ)。旧 `mixer/channel_strip.rs` の Comp 部分の移植。
//!
//! 検出 → SC フィルタ → 静的カーブ → アタック/リリース平滑 → 利得適用。検出信号は
//! [`NativeBlock::sidechain`] が `Some` ならその音、無ければ自分の入力。

use common::dsp::{Biquad, BiquadState, db_to_amp, sc_filter, smoothing_coeff};
use common::model::{COMP_KNEE_DB, CompSettings};

use super::{NativeBlock, block_len, comp_gain_step, sc_sample};

#[derive(Debug, Clone, Copy, Default)]
pub struct CompState {
    /// 検出フィルタ 2ch ぶんの遅延状態。
    sc: [BiquadState; 2],
    /// 検出フィルタの係数。`None` = フルレンジ検出。`cached_sc` と一致する間は組み直さない。
    sc_coeff: Option<Biquad>,
    cached_sc: Option<(f32, f32)>,
    /// 平滑済みのゲイン変化量 (dB、0 以下)。buffer をまたいで連続する。
    gain_db: f32,
}

impl CompState {
    pub(super) fn process(&mut self, s: &CompSettings, b: NativeBlock<'_>) -> f32 {
        let NativeBlock { l, r, n, sample_rate, sidechain, mut listen_out } = b;
        let n = block_len(l, r, n);
        if n == 0 || sample_rate <= 0.0 {
            return 0.0;
        }
        let sc_key = (s.sc_freq_hz, sample_rate);
        if self.cached_sc != Some(sc_key) {
            self.sc_coeff = sc_filter(s, sample_rate);
            self.cached_sc = Some(sc_key);
        }
        let (ratio, attack_ms, release_ms) = s.effective();
        let attack_c = smoothing_coeff(attack_ms, sample_rate);
        let release_c = smoothing_coeff(release_ms, sample_rate);
        let knee_floor_amp = db_to_amp(s.threshold_db - COMP_KNEE_DB * 0.5);
        let makeup_amp = db_to_amp(s.makeup_db);
        let mut worst = 0.0_f32;

        for i in 0..n {
            // ---- 検出信号 (SC フィルタが OFF なら素の信号) ----
            let (xl, xr) = match sidechain {
                Some(sc) => sc_sample(sc, i),
                None => (l[i], r[i]),
            };
            let (dl, dr) = match &self.sc_coeff {
                Some(c) => (self.sc[0].process(c, xl), self.sc[1].process(c, xr)),
                None => (xl, xr),
            };
            if let Some((ll, lr)) = listen_out.as_mut()
                && i < ll.len()
                && i < lr.len()
            {
                ll[i] = dl;
                lr[i] = dr;
            }

            // ---- 静的カーブ → 平滑 → 利得 ----
            comp_gain_step(&mut self.gain_db, dl.abs().max(dr.abs()), knee_floor_amp, s.threshold_db, ratio, attack_c, release_c);
            if self.gain_db < worst {
                worst = self.gain_db;
            }
            let g = if self.gain_db == 0.0 { makeup_amp } else { db_to_amp(self.gain_db + s.makeup_db) };
            l[i] *= g;
            r[i] *= g;
        }
        if !self.gain_db.is_finite() {
            self.gain_db = 0.0;
        }
        worst
    }
}
