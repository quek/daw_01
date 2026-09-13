//! master のフェーダー後に固定で掛かる先読みリミッター (`Song::master_limiter`)。
//! 旧 `mixer/master_strip.rs` のリミッター部分の移植。
//!
//! 遅延の有無は compile 時に焼いた `Schedule::master_limiter_latency`
//! (= `Song::master_limiter_latency_active`) で決まり、PDC の会計と常に一致する:
//! - 遅延なし: 遅延もゲインも掛けずに素通し (状態は捨てる)。
//! - 遅延あり・解決値 OFF (On レーン / 変調で OFF の区間): 先読みぶんの遅延だけ通す。
//! - 遅延あり・解決値 ON: 先読みリミッター。
//!
//! RT 規約: 確保・ロック・I/O なし。先読みリングは最大サンプルレートぶんを [`MasterLimiterState::new`]
//! で 1 回だけ確保し、以後は書き換えるだけ。

use common::dsp::{amp_to_db, db_to_amp, limiter_gain_db, smoothing_coeff};
use common::model::{
    MASTER_LIMITER_LOOKAHEAD_MS, MASTER_LIMITER_RELEASE_MS, MasterLimiterSettings, limiter_lookahead_samples,
};

use super::block_len;

/// 先読みリングの確保長 (サンプル)。192kHz で 5ms を賄える長さを **起動時に 1 度だけ**確保し、
/// 実 SR ではその一部だけを使う (RT で再確保しない)。
const LOOKAHEAD_CAPACITY: usize = (192_000.0 * MASTER_LIMITER_LOOKAHEAD_MS / 1000.0) as usize + 1;

/// master Limiter の状態。live は project ごとに 1 個 (`ProjectRt`)、書き出しは毎回新品。
pub struct MasterLimiterState {
    /// 先読みリング (L/R)。
    look_l: Vec<f32>,
    look_r: Vec<f32>,
    /// リングの書き込み位置。
    look_pos: usize,
    /// 実効の先読み長 (サンプル)。SR から導出し、変わったときだけ張り替える。
    look_len: usize,
    /// `look_len` を張ったときの SR (`0` = 未初期化 / 状態を捨てた)。
    cached_look_sr: f32,
    /// 平滑済みのゲイン (dB、0 以下)。
    gain_db: f32,
    /// 直前 buffer の最大リダクション (dB、0 以下)。
    gr_db: f32,
}

impl Default for MasterLimiterState {
    fn default() -> Self {
        Self::new()
    }
}

impl MasterLimiterState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            look_l: vec![0.0; LOOKAHEAD_CAPACITY],
            look_r: vec![0.0; LOOKAHEAD_CAPACITY],
            look_pos: 0,
            look_len: 0,
            cached_look_sr: 0.0,
            gain_db: 0.0,
            gr_db: 0.0,
        }
    }

    /// 無音の状態に戻す (同じタブで別ファイルを開いたとき、§18-M)。リングを 0 で埋めるだけで確保しない。
    pub fn reset(&mut self) {
        self.look_l.fill(0.0);
        self.look_r.fill(0.0);
        self.look_pos = 0;
        self.look_len = 0;
        self.cached_look_sr = 0.0;
        self.gain_db = 0.0;
        self.gr_db = 0.0;
    }

    /// 直前 buffer の GR (dB、0 以下)。publish 用。
    #[must_use]
    pub fn gain_reduction_db(&self) -> f32 {
        self.gr_db
    }

    /// master の最終段 (フェーダーの後)。規則は module doc。
    pub fn process(
        &mut self,
        s: &MasterLimiterSettings,
        latency_active: bool,
        l: &mut [f32],
        r: &mut [f32],
        n: usize,
        sample_rate: f32,
    ) {
        let n = block_len(l, r, n);
        if n == 0 || sample_rate <= 0.0 {
            return;
        }
        if !latency_active {
            // 遅延を焼いていない = 素通し。状態は捨てる (次に遅延ありで compile されたら無音から張り直す)。
            self.cached_look_sr = 0.0;
            self.gain_db = 0.0;
            self.gr_db = 0.0;
            return;
        }
        self.refresh_lookahead(sample_rate);
        if !s.on {
            // 遅延だけ通してゲインは掛けない (PDC の会計どおりの遅延を保つ)。
            for i in 0..n {
                let (out_l, out_r) = self.push_lookahead(l[i], r[i]);
                l[i] = out_l;
                r[i] = out_r;
            }
            self.gain_db = 0.0;
            self.gr_db = 0.0;
            return;
        }
        let release_c = smoothing_coeff(MASTER_LIMITER_RELEASE_MS, sample_rate);
        let mut worst = 0.0_f32;
        for i in 0..n {
            // 先読み: いま入ってきたサンプルのピークで利得を決め、出力するのは
            // `look_len` サンプル前の音。これで「ピークが来る前に下げ終わっている」。
            let peak = l[i].abs().max(r[i].abs());
            let (out_l, out_r) = self.push_lookahead(l[i], r[i]);
            let target = limiter_gain_db(amp_to_db(peak), s.ceiling_db);
            // 落とすときは即座に (先読みぶんの猶予で滑らかになる)、戻すときだけ平滑。
            self.gain_db = if target < self.gain_db {
                target
            } else {
                target + (self.gain_db - target) * release_c
            };
            if self.gain_db < worst {
                worst = self.gain_db;
            }
            let g = db_to_amp(self.gain_db);
            l[i] = out_l * g;
            r[i] = out_r * g;
        }
        self.gr_db = worst;
    }

    /// SR が変わったときだけ先読み長を張り替える (確保はしない)。
    fn refresh_lookahead(&mut self, sample_rate: f32) {
        if (self.cached_look_sr - sample_rate).abs() < f32::EPSILON {
            return;
        }
        // 長さは PDC 会計 (`compile/pdc.rs` の `master_output_latency`) と **同じ式**から取る
        // — ここだけ丸め方が違うと、書き出しの窓ずらしと実際の遅延が 1 サンプルずれる。
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let len = (limiter_lookahead_samples(sample_rate as u32) as usize).clamp(1, LOOKAHEAD_CAPACITY);
        self.look_len = len;
        self.look_pos = 0;
        self.look_l[..len].fill(0.0);
        self.look_r[..len].fill(0.0);
        self.cached_look_sr = sample_rate;
    }

    /// リングへ 1 サンプル入れて、`look_len` サンプル前の値を取り出す。
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
