//! master のフェーダー後に固定で掛かる先読みリミッター (`Song::master_limiter`)。
//!
//! 遅延の有無は compile 時に焼いた `Schedule::master_limiter_latency`
//! (= `Song::master_limiter_latency_active`) で決まり、PDC の会計と常に一致する:
//! - 遅延なし: 遅延もゲインも掛けずに素通し (状態は捨てる)。
//! - 遅延あり・解決値 OFF (On レーン / 変調で OFF の区間): 先読みぶんの遅延だけ通す。
//! - 遅延あり・解決値 ON: 先読みリミッター。
//!
//! # 出力は ceiling を超えない
//!
//! 出力サンプル `x[t−L]` (L = 先読み長) に掛けるゲインは、次の 3 段で決める:
//! 1. **窓内の最小値の保持**: 入ってきた各サンプルの必要ゲイン `req[k] = min(0, ceiling − peak[k])` の、
//!    直近 L+1 サンプル `[k−L, k]` の最小値 `held[k]` (単調 deque)。
//! 2. **窓長の移動平均**: `held` の直近 L サンプルの平均。`[t−L+1, t]` のどの `held[k]` の窓も `t−L` を
//!    含むので、平均は `req[t−L]` 以下 = `x[t−L]` を ceiling に収めるゲイン以下になる。保持した最小値を
//!    平均するので、ピークが窓に入ってから出ていくまでの L サンプルで直線的に下がり終わる (段差にならない)。
//! 3. **リリース**: 戻るときだけ平滑する (下げるときは平均に即追従)。平滑は常に平均以下に留まるので
//!    保証を崩さない。
//!
//! 最後に、f32 の丸め (dB ↔ 振幅の往復) で ceiling を 1 ULP でも超えないよう、出ていくサンプル自身の
//! ピークで上限を掛ける。上の 3 段が正しく働いている限りこの上限は丸め誤差ぶんしか効かない。
//!
//! 旧実装 (r.md #129 以前の master strip) は入ってきたサンプルだけで利得を決め、先読みしている間に
//! リリースで利得が戻ったため、孤立したインパルスで ceiling を超えていた (golden で peak 1.003 > −1 dBFS)。
//!
//! RT 規約: 確保・ロック・I/O なし。リングと deque は最大サンプルレートぶんを [`MasterLimiterState::new`]
//! で 1 回だけ確保し、以後は書き換えるだけ。

use common::dsp::{amp_to_db, db_to_amp, limiter_gain_db, smoothing_coeff};
use common::model::{
    MASTER_LIMITER_LOOKAHEAD_MS, MASTER_LIMITER_RELEASE_MS, MasterLimiterSettings, limiter_lookahead_samples,
};

use super::block_len;

/// 先読みリングの確保長 (サンプル)。192kHz で 5ms を賄える長さを **起動時に 1 度だけ**確保し、
/// 実 SR ではその一部だけを使う (RT で再確保しない)。
const LOOKAHEAD_CAPACITY: usize = (192_000.0 * MASTER_LIMITER_LOOKAHEAD_MS / 1000.0) as usize + 1;

/// 固定容量の単調 deque で、直近 `window + 1` サンプルの最小値を O(1) 償却で出す。
struct MinWindow {
    index: Vec<u64>,
    value: Vec<f32>,
    head: usize,
    len: usize,
}

impl MinWindow {
    fn with_capacity(capacity: usize) -> Self {
        Self { index: vec![0; capacity], value: vec![0.0; capacity], head: 0, len: 0 }
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// サンプル `t` の値 `v` を入れ、`[t − window, t]` の最小値を返す。`window + 1 <= 容量` であること。
    fn push(&mut self, t: u64, v: f32, window: u64) -> f32 {
        let cap = self.value.len();
        // 窓から出たものを先に落とす (残りは `[t − window, t − 1]` の高々 window 個)。
        while self.len > 0 && self.index[self.head] + window < t {
            self.head = (self.head + 1) % cap;
            self.len -= 1;
        }
        // 後ろから、新しい値以上のものは二度と最小にならないので落とす。
        while self.len > 0 && self.value[(self.head + self.len - 1) % cap] >= v {
            self.len -= 1;
        }
        let slot = (self.head + self.len) % cap;
        self.index[slot] = t;
        self.value[slot] = v;
        self.len = (self.len + 1).min(cap);
        self.value[self.head]
    }
}

/// master Limiter の状態。live は project ごとに 1 個 (`ProjectRt`)、書き出しは毎回新品。
pub struct MasterLimiterState {
    /// 先読みリング (L/R)。
    look_l: Vec<f32>,
    look_r: Vec<f32>,
    /// リングの書き込み位置。
    look_pos: usize,
    /// 実効の先読み長 L (サンプル)。SR から導出し、変わったときだけ張り替える。
    look_len: usize,
    /// `look_len` を張ったときの SR (`0` = 未初期化 / 状態を捨てた)。
    cached_look_sr: f32,
    /// 必要ゲインの窓内最小値 (`held`)。
    held: MinWindow,
    /// `held` の直近 L サンプル (移動平均のリング) とその和。
    avg_ring: Vec<f32>,
    avg_sum: f64,
    /// 入ってきたサンプルの通し番号 (窓の添字)。
    clock: u64,
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
            held: MinWindow::with_capacity(LOOKAHEAD_CAPACITY + 1),
            avg_ring: vec![0.0; LOOKAHEAD_CAPACITY],
            avg_sum: 0.0,
            clock: 0,
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
        self.clear_analysis();
        self.gain_db = 0.0;
        self.gr_db = 0.0;
    }

    fn clear_analysis(&mut self) {
        self.held.clear();
        self.avg_ring.fill(0.0);
        self.avg_sum = 0.0;
        self.clock = 0;
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
        let release_c = smoothing_coeff(MASTER_LIMITER_RELEASE_MS, sample_rate);
        let ceiling_amp = db_to_amp(s.ceiling_db);
        let mut worst_amp = 1.0_f32;
        for i in 0..n {
            // 解析は OFF の間も回す (ON にした瞬間から、既に窓に入っているピークにも間に合う)。
            let avg = self.analyze(l[i].abs().max(r[i].abs()), s.ceiling_db);
            let (out_l, out_r) = self.push_lookahead(l[i], r[i]);
            if !s.on {
                // 遅延だけ通してゲインは掛けない (PDC の会計どおりの遅延を保つ)。
                self.gain_db = 0.0;
                l[i] = out_l;
                r[i] = out_r;
                continue;
            }
            self.gain_db = if avg < self.gain_db { avg } else { avg + (self.gain_db - avg) * release_c };
            let mut g = db_to_amp(self.gain_db);
            // 丸め (dB ↔ 振幅) で ceiling を超えないための上限。出ていくサンプル自身のピークで掛ける。
            let out_peak = out_l.abs().max(out_r.abs());
            if out_peak * g > ceiling_amp {
                g = ceiling_amp / out_peak;
            }
            if g < worst_amp {
                worst_amp = g;
            }
            l[i] = out_l * g;
            r[i] = out_r * g;
        }
        // GR は実際に掛けた最小ゲイン (log はブロックに 1 回)。
        self.gr_db = if worst_amp < 1.0 { amp_to_db(worst_amp).min(0.0) } else { 0.0 };
    }

    /// 入ってきたサンプルのピークを解析に入れ、出ていくサンプル (L サンプル前) に使える
    /// 「窓内最小値の移動平均」(dB、0 以下) を返す。
    #[inline]
    fn analyze(&mut self, peak: f32, ceiling_db: f32) -> f32 {
        let len = self.look_len.max(1);
        let required = limiter_gain_db(amp_to_db(peak), ceiling_db);
        let held = self.held.push(self.clock, required, len as u64);
        let slot = (self.clock % len as u64) as usize;
        self.avg_sum += f64::from(held) - f64::from(self.avg_ring[slot]);
        self.avg_ring[slot] = held;
        self.clock += 1;
        // f64 の和でも加減算の丸めは積もるので、周回の頭で和を取り直す (O(L) を L サンプルに 1 回)。
        if slot + 1 == len {
            self.avg_sum = self.avg_ring[..len].iter().map(|v| f64::from(*v)).sum();
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let avg = (self.avg_sum / len as f64) as f32;
        avg.min(0.0)
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
        self.clear_analysis();
        self.gain_db = 0.0;
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
