//! 内蔵チャンネルストリップ (コンプ → EQ) の RT 実行。
//!
//! 設計正本は `docs/plan_channel_strip.md`。信号順は
//! `inserts → Comp → EQ → Pan → Fader` で固定なので、この module は
//! [`crate::mixer::apply_strip`] (= pan / volume / peak) の **直前** に呼ばれる。
//!
//! 係数と利得計算そのものは [`common::channel_strip_dsp`] が持つ (GUI のカーブ
//! 描画と同じ実装を共有する)。ここが持つのは **状態** — バイクワッドの遅延、
//! コンプの平滑済み利得、係数のキャッシュ。
//!
//! RT 規約: 確保・ロック・I/O なし。三角関数を呼ぶ係数の組み直しは
//! **パラメータが実際に変わった buffer だけ**行う (`cached_*` の比較)。

use common::channel_strip_dsp::{
    Biquad, BiquadState, EQ_STAGES, amp_to_db, comp_static_gain_db, eq_stages, sc_filter,
    smoothing_coeff,
};
use common::model::{ChannelStrip, CompSettings, EqSettings};

/// 1 track ぶんのストリップ状態。`TrackScratch` に埋め込んで使い回す
/// (毎 buffer の確保をしない)。
#[derive(Debug, Clone)]
pub struct StripState {
    /// EQ 6 段 × 2ch の遅延状態。
    eq: [[BiquadState; 2]; EQ_STAGES],
    /// 検出フィルタ 2ch ぶんの遅延状態。
    sc: [BiquadState; 2],
    /// 係数キャッシュ。`cached_eq` / `cached_sc` と一致する間は組み直さない。
    eq_coeffs: [Biquad; EQ_STAGES],
    sc_coeff: Option<Biquad>,
    cached_eq: Option<(EqSettings, f32)>,
    cached_sc: Option<(f32, f32)>,
    /// 平滑済みのゲイン変化量 (dB、0 以下)。buffer をまたいで連続する。
    gain_db: f32,
    /// 直前 buffer の最大リダクション量 (dB、0 以下)。メーターの publish 元。
    gr_db: f32,
}

impl Default for StripState {
    fn default() -> Self {
        Self {
            eq: [[BiquadState::default(); 2]; EQ_STAGES],
            sc: [BiquadState::default(); 2],
            eq_coeffs: [Biquad::IDENTITY; EQ_STAGES],
            sc_coeff: None,
            cached_eq: None,
            cached_sc: None,
            gain_db: 0.0,
            gr_db: 0.0,
        }
    }
}

impl StripState {
    /// パラメータが変わっていれば係数を組み直す (三角関数はここだけ)。
    fn refresh_coeffs(&mut self, strip: &ChannelStrip, sample_rate: f32) {
        if self.cached_eq != Some((strip.eq, sample_rate)) {
            self.eq_coeffs = eq_stages(&strip.eq, sample_rate);
            self.cached_eq = Some((strip.eq, sample_rate));
        }
        let sc_key = (strip.comp.sc_freq_hz, sample_rate);
        if self.cached_sc != Some(sc_key) {
            self.sc_coeff = sc_filter(&strip.comp, sample_rate);
            self.cached_sc = Some(sc_key);
        }
    }

    /// `l` / `r` の先頭 `n` サンプルを in-place で処理する。
    ///
    /// 戻り値はこの buffer の最大ゲインリダクション (dB、0 以下)。
    /// `strip.is_bypassed()` なら何もせず `0.0` を返す (= 完全な無回帰)。
    pub fn process(
        &mut self,
        strip: &ChannelStrip,
        l: &mut [f32],
        r: &mut [f32],
        n: usize,
        sample_rate: f32,
    ) -> f32 {
        let n = n.min(l.len()).min(r.len());
        if n == 0 || sample_rate <= 0.0 || strip.is_bypassed() {
            self.gr_db = 0.0;
            return 0.0;
        }
        self.refresh_coeffs(strip, sample_rate);

        if strip.comp.on {
            self.process_comp(&strip.comp, l, r, n, sample_rate);
            // SC Listen 中は検出信号がそのまま出力なので EQ は通さない
            // (「いま何を聴いてコンプが動いているか」を素で確かめるため)。
            if strip.comp.sc_listen {
                return self.gr_db;
            }
        } else {
            self.gr_db = 0.0;
            self.gain_db = 0.0;
        }

        if strip.eq.on {
            for (stage, coeff) in self.eq.iter_mut().zip(&self.eq_coeffs) {
                for i in 0..n {
                    l[i] = stage[0].process(coeff, l[i]);
                    r[i] = stage[1].process(coeff, r[i]);
                }
            }
        }
        self.gr_db
    }

    /// コンプ本体。検出 → 静的カーブ → アタック/リリース平滑 → 利得適用。
    fn process_comp(
        &mut self,
        comp: &CompSettings,
        l: &mut [f32],
        r: &mut [f32],
        n: usize,
        sample_rate: f32,
    ) {
        let (ratio, attack_ms, release_ms) = comp.effective();
        let attack_c = smoothing_coeff(attack_ms, sample_rate);
        let release_c = smoothing_coeff(release_ms, sample_rate);
        let makeup = comp.makeup_db;
        let listen = comp.sc_listen;
        let mut worst = 0.0_f32;

        for i in 0..n {
            // ---- 検出信号 (SC フィルタが OFF なら素の信号) ----
            let (dl, dr) = match &self.sc_coeff {
                Some(c) => (self.sc[0].process(c, l[i]), self.sc[1].process(c, r[i])),
                None => (l[i], r[i]),
            };
            let det = dl.abs().max(dr.abs());

            // ---- 静的カーブ → 平滑 ----
            let target = comp_static_gain_db(amp_to_db(det), comp.threshold_db, ratio);
            let coeff = if target < self.gain_db { attack_c } else { release_c };
            self.gain_db = target + (self.gain_db - target) * coeff;
            if self.gain_db < worst {
                worst = self.gain_db;
            }

            if listen {
                // 検出信号そのものを聴く (メイクアップも圧縮も掛けない素の音)。
                l[i] = dl;
                r[i] = dr;
                continue;
            }
            let g = 10f32.powf((self.gain_db + makeup) / 20.0);
            l[i] *= g;
            r[i] *= g;
        }
        self.gr_db = worst;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{CompMode, EqBand};

    const SR: f32 = 48_000.0;

    fn run(strip: &ChannelStrip, state: &mut StripState, amp: f32, blocks: usize) -> (f32, f32) {
        let n = 512;
        let mut peak = 0.0_f32;
        let mut gr = 0.0_f32;
        for b in 0..blocks {
            let mut l = vec![0.0_f32; n];
            let mut r = vec![0.0_f32; n];
            for (i, (ls, rs)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
                let t = (b * n + i) as f32 / SR;
                let s = amp * (std::f32::consts::TAU * 1_000.0 * t).sin();
                *ls = s;
                *rs = s;
            }
            gr = state.process(strip, &mut l, &mut r, n, SR);
            if b + 1 == blocks {
                peak = l.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
            }
        }
        (peak, gr)
    }

    #[test]
    fn バイパス中は信号もリダクションも変わらない() {
        let strip = ChannelStrip::default();
        let mut st = StripState::default();
        let (peak, gr) = run(&strip, &mut st, 0.5, 4);
        assert!((peak - 0.5).abs() < 1e-6, "peak={peak}");
        assert_eq!(gr, 0.0);
    }

    #[test]
    fn 閾値を超えた信号が理論値まで圧縮される() {
        // -20dB スレッショルド / 4:1 / 入力 0dB → 定常で -15dB のリダクション。
        let strip = ChannelStrip {
            comp: CompSettings {
                on: true,
                mode: CompMode::Compressor,
                threshold_db: -20.0,
                ratio: 4.0,
                attack_ms: 1.0,
                release_ms: 50.0,
                ..CompSettings::default()
            },
            ..ChannelStrip::default()
        };
        let mut st = StripState::default();
        // 十分に定常化させる (アタック 1ms に対して 40 buffer ≒ 427ms)。
        let (peak, gr) = run(&strip, &mut st, 1.0, 40);
        assert!((gr - -15.0).abs() < 0.6, "gr={gr}");
        let peak_db = 20.0 * peak.log10();
        assert!((peak_db - -15.0).abs() < 0.6, "peak_db={peak_db}");
    }

    #[test]
    fn メイクアップは圧縮後に足される() {
        let strip = ChannelStrip {
            comp: CompSettings {
                on: true,
                threshold_db: -20.0,
                ratio: 4.0,
                attack_ms: 1.0,
                release_ms: 50.0,
                makeup_db: 10.0,
                ..CompSettings::default()
            },
            ..ChannelStrip::default()
        };
        let mut st = StripState::default();
        let (peak, _) = run(&strip, &mut st, 1.0, 40);
        let peak_db = 20.0 * peak.log10();
        assert!((peak_db - -5.0).abs() < 0.6, "peak_db={peak_db}");
    }

    #[test]
    fn 検出フィルタが外れた帯域ではコンプが動かない() {
        // 検出を 8kHz に絞ると、1kHz の信号は検出器にほとんど届かない。
        let mut strip = ChannelStrip {
            comp: CompSettings {
                on: true,
                threshold_db: -20.0,
                ratio: 4.0,
                attack_ms: 1.0,
                release_ms: 50.0,
                sc_freq_hz: 8_000.0,
                ..CompSettings::default()
            },
            ..ChannelStrip::default()
        };
        let mut st = StripState::default();
        let (_, gr) = run(&strip, &mut st, 1.0, 40);
        assert!(gr > -3.0, "帯域外なのに深く効いている: gr={gr}");
        // 同じ設定で検出 OFF なら深く効く (= 差が検出フィルタによるものだと示す)。
        strip.comp.sc_freq_hz = 0.0;
        let mut st2 = StripState::default();
        let (_, gr_off) = run(&strip, &mut st2, 1.0, 40);
        assert!(gr_off < -12.0, "gr_off={gr_off}");
    }

    #[test]
    fn eq_のハイパスは低域を落とす() {
        let mut strip = ChannelStrip::default();
        strip.eq.on = true;
        {
            let hp = strip.eq.band_mut(EqBand::Hp);
            hp.on = true;
            hp.freq_hz = 3_000.0;
        }
        let mut st = StripState::default();
        // 1kHz は 3kHz HPF の下 → 大きく減衰する。
        let (peak, _) = run(&strip, &mut st, 0.5, 8);
        let db = 20.0 * (peak / 0.5).log10();
        assert!(db < -10.0, "db={db}");
    }

}
