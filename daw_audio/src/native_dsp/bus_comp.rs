//! 内蔵 Bus Comp (SSL / Reason のバスコンプ流)。旧 `mixer/master_strip.rs` のバスコンプ部分の移植。
//!
//! Ratio / Attack / Release は段階式で、`Auto` リリースは「最近どれくらい潰れ続けているか」の
//! 平均から buffer ごとに時定数を引き直す (サンプルごとに引き直すと exp() がホットループに入る)。
//! 検出フィルタは持たない。外部サイドチェインは受ける (Q19)。

use common::dsp::{bus_comp_auto_release_ms, db_to_amp, smoothing_coeff};
use common::model::{BUS_COMP_AUTO_RELEASE_TRACK_MS, BusCompSettings, COMP_KNEE_DB};

use super::{NativeBlock, block_len, comp_gain_step, sc_sample};

#[derive(Debug, Clone, Copy, Default)]
pub struct BusCompState {
    /// 平滑済みのゲイン変化量 (dB、0 以下)。
    gain_db: f32,
    /// `Auto` リリース用: 「最近どれくらい潰れ続けているか」の平均 (dB、0 以下)。
    sustained_gr_db: f32,
}

impl BusCompState {
    pub(super) fn process(&mut self, s: &BusCompSettings, b: NativeBlock<'_>) -> f32 {
        let NativeBlock { l, r, n, sample_rate, sidechain, .. } = b;
        let n = block_len(l, r, n);
        if n == 0 || sample_rate <= 0.0 {
            return 0.0;
        }
        let ratio = s.ratio.value();
        let attack_c = smoothing_coeff(s.attack.ms(), sample_rate);
        let release_c = s.release.ms().map_or_else(
            || smoothing_coeff(bus_comp_auto_release_ms(self.sustained_gr_db), sample_rate),
            |ms| smoothing_coeff(ms, sample_rate),
        );
        let sustain_c = smoothing_coeff(BUS_COMP_AUTO_RELEASE_TRACK_MS, sample_rate);
        let knee_floor_amp = db_to_amp(s.threshold_db - COMP_KNEE_DB * 0.5);
        let makeup_amp = db_to_amp(s.makeup_db);
        let mut worst = 0.0_f32;

        for i in 0..n {
            let (xl, xr) = match sidechain {
                Some(sc) => sc_sample(sc, i),
                None => (l[i], r[i]),
            };
            comp_gain_step(&mut self.gain_db, xl.abs().max(xr.abs()), knee_floor_amp, s.threshold_db, ratio, attack_c, release_c);
            // 長時間平均 (次 buffer の Auto リリースを決める材料)。短いピークだけの素材では
            // −1e-6 dB 付近の小さな値が積み上がって効くので、`gain_db` と同じ閾値では落とさない。
            // 無音が数分続いて非正規化数へ漂うのだけを断つ。
            self.sustained_gr_db = self.gain_db + (self.sustained_gr_db - self.gain_db) * sustain_c;
            if self.sustained_gr_db > -1e-20 {
                self.sustained_gr_db = 0.0;
            }
            if self.gain_db < worst {
                worst = self.gain_db;
            }
            let g = if self.gain_db == 0.0 { makeup_amp } else { db_to_amp(self.gain_db + s.makeup_db) };
            l[i] *= g;
            r[i] *= g;
        }
        if !self.gain_db.is_finite() || !self.sustained_gr_db.is_finite() {
            self.gain_db = 0.0;
            self.sustained_gr_db = 0.0;
        }
        worst
    }
}
