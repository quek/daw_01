//! マスターストリップ (バスコンプ → トーン EQ → … → リミッター) の RT 実行。
//!
//! 設計正本は `docs/plan_master_strip.md`。信号順は
//! `合算 → Comp → EQ → insert → フェーダー → リミッター` で、この module は
//! [`render_master_buffer`](crate::graph::render_master_buffer) から **2 回** 呼ばれる:
//! insert の前に [`MasterStripState::process_pre`]、フェーダーの後に
//! [`MasterStripState::process_limiter`]。
//!
//! 係数と利得計算は [`common::channel_strip_dsp`] (通常 ch と共有)。ここが持つのは
//! 状態 — バイクワッドの遅延、平滑済みゲイン、リミッターのルックアヘッドリング。
//!
//! RT 規約: 確保・ロック・I/O なし。ルックアヘッドの遅延線は最大サンプルレートぶんを
//! `new()` で確保し、以後は書き換えるだけ。

use common::channel_strip_dsp::{
    Biquad, BiquadState, amp_to_db, comp_static_gain_db, limiter_gain_db, master_auto_release_ms,
    master_eq_stages, smoothing_coeff,
};
use common::model::{
    MASTER_AUTO_RELEASE_TRACK_MS, MASTER_LIMITER_LOOKAHEAD_MS, MASTER_LIMITER_RELEASE_MS,
    MasterEqSettings, MasterRelease, MasterStrip,
};

/// ルックアヘッド遅延線の確保長 (サンプル)。192kHz で 5ms を賄える長さを
/// **起動時に 1 度だけ**確保し、実 SR ではその一部だけを使う (RT で再確保しない)。
const LOOKAHEAD_CAPACITY: usize = (192_000.0 * MASTER_LIMITER_LOOKAHEAD_MS / 1000.0) as usize + 1;

/// マスターストリップの状態。engine と書き出しがそれぞれ 1 個ずつ所有する
/// (書き出しは毎回新品 = 決定論的)。
pub struct MasterStripState {
    /// トーン EQ 3 段 × 2ch の遅延状態。
    eq: [[BiquadState; 2]; 3],
    eq_coeffs: [Biquad; 3],
    cached_eq: Option<(MasterEqSettings, f32)>,
    /// バスコンプの平滑済みゲイン変化量 (dB、0 以下)。
    comp_gain_db: f32,
    /// `Auto` リリース用: 「最近どれくらい潰れ続けているか」の平均 (dB、0 以下)。
    sustained_gr_db: f32,
    /// 直前 buffer の最大リダクション (dB、0 以下)。
    comp_gr_db: f32,

    /// リミッターのルックアヘッド遅延線 (L/R)。
    look_l: Vec<f32>,
    look_r: Vec<f32>,
    /// 遅延線の書き込み位置。
    look_pos: usize,
    /// 実効ルックアヘッド長 (サンプル)。SR から導出し、変わったときだけ張り替える。
    look_len: usize,
    cached_look_sr: f32,
    /// リミッターの平滑済みゲイン (dB、0 以下)。
    limiter_gain_db: f32,
    limiter_gr_db: f32,
}

impl Default for MasterStripState {
    fn default() -> Self {
        Self::new()
    }
}

impl MasterStripState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            eq: [[BiquadState::default(); 2]; 3],
            eq_coeffs: [Biquad::IDENTITY; 3],
            cached_eq: None,
            comp_gain_db: 0.0,
            sustained_gr_db: 0.0,
            comp_gr_db: 0.0,
            look_l: vec![0.0; LOOKAHEAD_CAPACITY],
            look_r: vec![0.0; LOOKAHEAD_CAPACITY],
            look_pos: 0,
            look_len: 0,
            cached_look_sr: 0.0,
            limiter_gain_db: 0.0,
            limiter_gr_db: 0.0,
        }
    }

    /// 直前 buffer の GR `(コンプ, リミッター)` (dB、0 以下)。publish 用。
    #[must_use]
    pub fn gain_reduction_db(&self) -> (f32, f32) {
        (self.comp_gr_db, self.limiter_gr_db)
    }

    /// insert の **前** に通す段: バスコンプ → トーン EQ。
    pub fn process_pre(
        &mut self,
        strip: &MasterStrip,
        l: &mut [f32],
        r: &mut [f32],
        n: usize,
        sample_rate: f32,
    ) {
        let n = n.min(l.len()).min(r.len());
        if n == 0 || sample_rate <= 0.0 {
            return;
        }
        if strip.comp.on {
            self.process_comp(strip, l, r, n, sample_rate);
        } else {
            self.comp_gr_db = 0.0;
            self.comp_gain_db = 0.0;
            self.sustained_gr_db = 0.0;
        }

        if strip.eq.on {
            if self.cached_eq != Some((strip.eq, sample_rate)) {
                self.eq_coeffs = master_eq_stages(&strip.eq, sample_rate);
                self.cached_eq = Some((strip.eq, sample_rate));
            }
            for (stage, coeff) in self.eq.iter_mut().zip(&self.eq_coeffs) {
                for i in 0..n {
                    l[i] = stage[0].process(coeff, l[i]);
                    r[i] = stage[1].process(coeff, r[i]);
                }
            }
        } else {
            // OFF の間は係数キャッシュを捨てて、次に ON にしたとき必ず組み直す。
            self.cached_eq = None;
        }
    }

    /// バスコンプ。段階式パラメータの実値化と `Auto` リリースはここだけが持つ。
    fn process_comp(
        &mut self,
        strip: &MasterStrip,
        l: &mut [f32],
        r: &mut [f32],
        n: usize,
        sample_rate: f32,
    ) {
        let comp = &strip.comp;
        let ratio = comp.ratio.value();
        let attack_c = smoothing_coeff(comp.attack.ms(), sample_rate);
        let fixed_release_c = comp.release.ms().map(|ms| smoothing_coeff(ms, sample_rate));
        // `Auto` は「最近の潰れ具合」から毎 buffer リリースを引き直す (サンプル単位で
        // 引き直すと exp() がホットループに入るので block-rate)。
        let auto_release_c = if comp.release == MasterRelease::Auto {
            smoothing_coeff(master_auto_release_ms(self.sustained_gr_db), sample_rate)
        } else {
            0.0
        };
        let release_c = fixed_release_c.unwrap_or(auto_release_c);
        let sustain_c = smoothing_coeff(MASTER_AUTO_RELEASE_TRACK_MS, sample_rate);
        let makeup = comp.makeup_db;
        let mut worst = 0.0_f32;

        for i in 0..n {
            let det = l[i].abs().max(r[i].abs());
            let target = comp_static_gain_db(amp_to_db(det), comp.threshold_db, ratio);
            let coeff = if target < self.comp_gain_db { attack_c } else { release_c };
            self.comp_gain_db = target + (self.comp_gain_db - target) * coeff;
            // 長時間平均 (次 buffer の Auto リリースを決める材料)。
            self.sustained_gr_db =
                self.comp_gain_db + (self.sustained_gr_db - self.comp_gain_db) * sustain_c;
            if self.comp_gain_db < worst {
                worst = self.comp_gain_db;
            }
            let g = 10f32.powf((self.comp_gain_db + makeup) / 20.0);
            l[i] *= g;
            r[i] *= g;
        }
        self.comp_gr_db = worst;
    }

    /// フェーダーの **後** に通す最終段: ルックアヘッドリミッター。
    ///
    /// **OFF でも遅延だけは通す** — ON/OFF で出力が 5ms 飛ぶと、再生中に切り替えた
    /// 瞬間にプツッと鳴るため (`docs/plan_master_strip.md` §2)。
    pub fn process_limiter(
        &mut self,
        strip: &MasterStrip,
        l: &mut [f32],
        r: &mut [f32],
        n: usize,
        sample_rate: f32,
    ) {
        let n = n.min(l.len()).min(r.len());
        if n == 0 || sample_rate <= 0.0 {
            return;
        }
        if !strip.limiter.on {
            // OFF は**遅延も含めて素通し** (docs/plan_master_strip.md §2)。遅延線は
            // 捨てる — 次に ON になったとき `refresh_lookahead` が無音で張り直すので、
            // 切り替えの瞬間に 5ms 飛ぶ / 詰まる (承知のうえの設計判断)。
            self.cached_look_sr = 0.0;
            self.limiter_gain_db = 0.0;
            self.limiter_gr_db = 0.0;
            return;
        }
        self.refresh_lookahead(sample_rate);
        let ceiling = strip.limiter.ceiling_db;
        let release_c = smoothing_coeff(MASTER_LIMITER_RELEASE_MS, sample_rate);
        let mut worst = 0.0_f32;

        for i in 0..n {
            // 先読み: いま入ってきたサンプルのピークで利得を決め、出力するのは
            // `look_len` サンプル前の音。これで「ピークが来る前に下げ終わっている」。
            let peak = l[i].abs().max(r[i].abs());
            let (out_l, out_r) = self.push_lookahead(l[i], r[i]);
            let target = limiter_gain_db(amp_to_db(peak), ceiling);
            // 落とすときは即座に (先読みぶんの猶予で滑らかになる)、戻すときだけ平滑。
            self.limiter_gain_db = if target < self.limiter_gain_db {
                target
            } else {
                target + (self.limiter_gain_db - target) * release_c
            };
            if self.limiter_gain_db < worst {
                worst = self.limiter_gain_db;
            }
            let g = 10f32.powf(self.limiter_gain_db / 20.0);
            l[i] = out_l * g;
            r[i] = out_r * g;
        }
        self.limiter_gr_db = worst;
    }

    /// SR が変わったときだけルックアヘッド長を張り替える (確保はしない)。
    fn refresh_lookahead(&mut self, sample_rate: f32) {
        if (self.cached_look_sr - sample_rate).abs() < f32::EPSILON {
            return;
        }
        // 長さは PDC 会計 (`compile.rs` の `master_latency_samples`) と **同じ式**から取る
        // — ここだけ丸め方が違うと、書き出しの窓ずらしと実際の遅延が 1 サンプルずれる。
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let len = (common::model::limiter_lookahead_samples(sample_rate as u32) as usize)
            .clamp(1, LOOKAHEAD_CAPACITY);
        self.look_len = len;
        self.look_pos = 0;
        self.look_l[..len].fill(0.0);
        self.look_r[..len].fill(0.0);
        self.cached_look_sr = sample_rate;
    }

    /// 遅延線へ 1 サンプル入れて、`look_len` サンプル前の値を取り出す。
    #[inline]
    fn push_lookahead(&mut self, l: f32, r: f32) -> (f32, f32) {
        let len = self.look_len.max(1).min(self.look_l.len());
        let pos = self.look_pos % len;
        let out = (self.look_l[pos], self.look_r[pos]);
        self.look_l[pos] = l;
        self.look_r[pos] = r;
        self.look_pos = (pos + 1) % len;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{
        MASTER_AUTO_RELEASE_MAX_MS, MASTER_AUTO_RELEASE_MIN_MS, MasterEqBand, MasterRatio,
        MasterStripParam,
    };

    const SR: f32 = 48_000.0;

    /// 1kHz 正弦を `blocks` 個の buffer 分流して、最後の buffer のピークを返す。
    fn run_pre(strip: &MasterStrip, st: &mut MasterStripState, amp: f32, blocks: usize) -> f32 {
        let n = 512;
        let mut peak = 0.0_f32;
        for b in 0..blocks {
            let (mut l, mut r) = (vec![0.0_f32; n], vec![0.0_f32; n]);
            for (i, (ls, rs)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let t = (b * n + i) as f32 / SR;
                let s = amp * (std::f32::consts::TAU * 1_000.0 * t).sin();
                *ls = s;
                *rs = s;
            }
            st.process_pre(strip, &mut l, &mut r, n, SR);
            if b + 1 == blocks {
                peak = l.iter().fold(0.0_f32, |m, v| m.max(v.abs()));
            }
        }
        peak
    }

    #[test]
    fn バイパス中は素通しする() {
        let strip = MasterStrip::default();
        let mut st = MasterStripState::new();
        let peak = run_pre(&strip, &mut st, 0.5, 4);
        assert!((peak - 0.5).abs() < 1e-6, "peak={peak}");
        assert_eq!(st.gain_reduction_db(), (0.0, 0.0));
    }

    #[test]
    fn バスコンプが段階式レシオどおりに潰す() {
        // -20dB スレッショルド / 4:1 / 入力 0dB → 定常で -15dB。
        let mut strip = MasterStrip::default();
        strip.comp.on = true;
        strip.comp.threshold_db = -20.0;
        strip.comp.ratio = MasterRatio::R4;
        strip.set_param(MasterStripParam::CompAttack, 0.0); // 0.1ms
        strip.set_param(MasterStripParam::CompRelease, 0.0); // 100ms
        let mut st = MasterStripState::new();
        let peak = run_pre(&strip, &mut st, 1.0, 40);
        let db = 20.0 * peak.log10();
        assert!((db - -15.0).abs() < 0.6, "db={db}");
        let (gr, _) = st.gain_reduction_db();
        assert!((gr - -15.0).abs() < 0.6, "gr={gr}");
    }

    #[test]
    fn auto_リリースは潰れ続けたあとほど遅くなる() {
        use common::channel_strip_dsp::master_auto_release_ms;
        let brief = master_auto_release_ms(-0.5);
        let sustained = master_auto_release_ms(-12.0);
        assert!(brief < sustained, "brief={brief} sustained={sustained}");
        assert!((brief - MASTER_AUTO_RELEASE_MIN_MS).abs() < 100.0);
        assert!((sustained - MASTER_AUTO_RELEASE_MAX_MS).abs() < 1e-3);
    }

    #[test]
    fn トーン_eq_は固定周波数で効く() {
        let mut strip = MasterStrip::default();
        strip.eq.on = true;
        strip.set_param(MasterStripParam::EqGain(MasterEqBand::LoMid), 6.0);
        let mut st = MasterStripState::new();
        // 300Hz のワイドベルなので 1kHz でも少しは持ち上がるが、300Hz の方が強い。
        let stages = common::channel_strip_dsp::master_eq_stages(&strip.eq, SR);
        let at_center = common::channel_strip_dsp::master_eq_magnitude_db(&stages, SR, 300.0);
        let far = common::channel_strip_dsp::master_eq_magnitude_db(&stages, SR, 20.0);
        assert!((at_center - 6.0).abs() < 0.2, "center={at_center}");
        assert!(far < 1.0, "far={far}");
        // 実際に信号を通しても持ち上がる。
        let peak = run_pre(&strip, &mut st, 0.1, 8);
        assert!(peak > 0.1, "1kHz でも持ち上がる: {peak}");
    }

    #[test]
    fn リミッターはシーリングを超えさせない() {
        let mut strip = MasterStrip::default();
        strip.limiter.on = true;
        strip.limiter.ceiling_db = -1.0;
        let mut st = MasterStripState::new();
        let n = 512;
        let mut peak = 0.0_f32;
        for b in 0..20 {
            let (mut l, mut r) = (vec![0.0_f32; n], vec![0.0_f32; n]);
            for (i, (ls, rs)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let t = (b * n + i) as f32 / SR;
                // 0dBFS を超える入力 (+6dB)。
                let s = 2.0 * (std::f32::consts::TAU * 200.0 * t).sin();
                *ls = s;
                *rs = s;
            }
            st.process_limiter(&strip, &mut l, &mut r, n, SR);
            if b >= 2 {
                peak = peak.max(l.iter().fold(0.0_f32, |m, v| m.max(v.abs())));
            }
        }
        let ceiling_amp = 10f32.powf(-1.0 / 20.0);
        assert!(peak <= ceiling_amp * 1.02, "peak={peak} ceiling={ceiling_amp}");
        let (_, gr) = st.gain_reduction_db();
        assert!(gr < -5.0, "+6dB 入力なのに GR が浅い: {gr}");
    }

    #[test]
    fn リミッターは_off_なら遅延ゼロで素通し_on_で会計と同じ遅延() {
        let n = 512;
        // OFF: 1 サンプルも遅れず、値も変わらない。
        let strip = MasterStrip::default();
        let mut st = MasterStripState::new();
        let (mut l, mut r) = (vec![1.0_f32; n], vec![1.0_f32; n]);
        st.process_limiter(&strip, &mut l, &mut r, n, SR);
        assert!(l.iter().all(|v| (*v - 1.0).abs() < 1e-6), "OFF なのに遅延か変化がある");

        // ON: PDC 会計 (`limiter_lookahead_samples`) と**同じ数**だけ無音が先行し、
        // その直後から信号が出る。ここがずれると書き出しの窓ずらしとクリックが
        // 1 サンプル狂う。
        let mut strip = MasterStrip::default();
        strip.limiter.on = true;
        strip.limiter.ceiling_db = 0.0;
        let mut st = MasterStripState::new();
        let (mut l, mut r) = (vec![0.5_f32; n], vec![0.5_f32; n]);
        st.process_limiter(&strip, &mut l, &mut r, n, SR);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let delay = common::model::limiter_lookahead_samples(SR as u32) as usize;
        assert_eq!(delay, 240);
        assert!(l[..delay].iter().all(|v| *v == 0.0), "遅延ぶんの無音が出ていない");
        assert!((l[delay] - 0.5).abs() < 1e-6, "遅延の直後に信号が出る: {}", l[delay]);
    }
}
