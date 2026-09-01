//! Per-source envelope follower (docs/plan_modulation.md §3).
//!
//! RT-safe: no alloc / lock / IO. One [`FollowerSlot`] per `ModSource`,
//! owned by the [`super::Schedule`] (rebuilt on recompile, persists across
//! buffers). `compile_schedule` bakes the coefficients from `FollowerConfig`
//! once; the audio thread only runs the per-sample one-pole math each buffer.

use common::model::{BandFilter, FollowerConfig, FollowerMode};

/// 刻み境界の envelope を保持する本数 (2 の冪、剰余を安く取るため)。
/// **1 buffer で踏む刻み数 + 遅れ**より大きいこと。
const FOLLOWER_HIST: usize = 64;

/// **変調がフォロワーの値を読むときの遅れ (刻み数)。**
///
/// フォロワーの env は「その buffer の音」を通してからでないと出ない。一方
/// 変調の値面は**描く前に**作る必要がある (描画がそれを消費する) ので、
/// 同じ buffer の env は原理的に使えない。そこで **buffer 長に依存しない
/// 固定の刻み数**だけ遡って読む。
///
/// 1 buffer で踏む刻み数 (最大 [`crate::mod_tick::MAX_TICKS_PER_BUFFER`]) 以上に
/// しておけば、要求する刻みは必ず**前の buffer で記録済み**になる。これで
/// live (device buffer 長は可変) と書き出し (1024 固定) の遅れ量が一致する —
/// 以前は「1 buffer 前」だったので 480 frame なら 10ms、1024 frame なら 21.3ms と
/// 環境で変わり、サイドチェインの立ち上がりが聴いた音と書き出しでずれていた。
pub const FOLLOWER_LAG_TICKS: i64 = crate::mod_tick::MAX_TICKS_PER_BUFFER as i64;

/// One-pole smoothing coefficient for a cutoff `fc` Hz at `sample_rate`
/// (`y += a*(x - y)`). Clamped to a stable `[0, 1]` range.
fn one_pole_coeff(fc_hz: f32, sample_rate: u32) -> f32 {
    if sample_rate == 0 || fc_hz <= 0.0 {
        return 1.0;
    }
    let a = 1.0 - (-2.0 * std::f32::consts::PI * fc_hz / sample_rate as f32).exp();
    a.clamp(0.0, 1.0)
}

/// Attack/release one-pole coefficient for a time constant in milliseconds.
/// `ms <= 0` yields an instantaneous (coefficient `1.0`) response.
fn time_coeff(ms: f32, sample_rate: u32) -> f32 {
    if sample_rate == 0 || ms <= 0.0 {
        return 1.0;
    }
    let a = 1.0 - (-1.0 / (ms * 0.001 * sample_rate as f32)).exp();
    a.clamp(0.0, 1.0)
}

/// One-pole band filter (high-pass then low-pass = band-pass) used to
/// isolate e.g. a kick's frequency band before envelope detection. State is
/// scalar (the detector signal is mono), so this is a couple of FLOPs/sample.
#[derive(Debug, Clone, Copy)]
struct Band {
    a_hp: f32,
    a_lp: f32,
    /// State: the low-pass whose output is subtracted to form the high-pass.
    lp_hp: f32,
    /// State: the final low-pass.
    lp: f32,
}

impl Band {
    fn new(f: &BandFilter, sample_rate: u32) -> Self {
        Self {
            a_hp: one_pole_coeff(f.hp_hz, sample_rate),
            a_lp: one_pole_coeff(f.lp_hz, sample_rate),
            lp_hp: 0.0,
            lp: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        // high-pass = x - low-pass(x) at the hp cutoff.
        self.lp_hp += self.a_hp * (x - self.lp_hp);
        let hp = x - self.lp_hp;
        // low-pass at the lp cutoff.
        self.lp += self.a_lp * (hp - self.lp);
        self.lp
    }
}

/// Per-`ModSource` envelope follower. Coefficients are baked at compile time
/// (`from_config`); the audio thread advances [`Self::env`] each buffer via
/// [`Self::process_block`]. `env` は block-rate で変調値面
/// ([`common::mod_plane::ModPlane`]) に載って GUI へ publish され、param 変調では
/// 制御刻みごとにサンプルされる。**値面のアドレスは `ModSource::id`**
/// (SSoT は `common/src/mod_plane.rs` の module doc)。
#[derive(Debug, Clone, Copy)]
pub struct FollowerSlot {
    atk: f32,
    rel: f32,
    gain: f32,
    mode: FollowerMode,
    rectify: bool,
    band: Option<Band>,
    /// Running smoothed envelope (`>= 0`). Persists across buffers; reset to
    /// `0` only when the schedule is recompiled.
    pub env: f32,
    /// 刻み境界ごとの envelope (絶対刻み番号 % [`FOLLOWER_HIST`] で引く)。
    /// 変調はここを [`FOLLOWER_LAG_TICKS`] だけ遡って読む。
    hist: [f32; FOLLOWER_HIST],
    /// `hist` に書いた最新の絶対刻み番号 (`i64::MIN` = まだ 1 つも無い)。
    hist_tick: i64,
}

impl FollowerSlot {
    pub fn from_config(cfg: &FollowerConfig, sample_rate: u32) -> Self {
        Self {
            atk: time_coeff(cfg.attack_ms, sample_rate),
            rel: time_coeff(cfg.release_ms, sample_rate),
            gain: cfg.gain,
            mode: cfg.mode,
            rectify: cfg.rectify,
            band: cfg.band_filter.as_ref().map(|f| Band::new(f, sample_rate)),
            env: 0.0,
            hist: [0.0; FOLLOWER_HIST],
            hist_tick: i64::MIN,
        }
    }

    /// r.md #89: **変調された係数を刻みごとに差し込む。**
    ///
    /// `from_config` が compile 時に焼いた係数だけを読む作りだと、フォロワーの
    /// A / R / ゲイン / 帯域を変調先にしても何も起きない (器だけあって効かない)。
    /// 走行状態 (`env` と帯域フィルタの内部状態) は触らないので、刻みごとに
    /// 呼んでも滑らかに繋がる。
    ///
    /// 帯域フィルタは **compile 時に有効だったときだけ**係数を差し替える —
    /// 変調で `Some`/`None` が切り替わると走行状態が飛ぶうえ、`BandFilter` の
    /// 有無は config の構造 (topology) であって連続量ではない。
    ///
    /// RT-safe: 係数の算術のみ (確保・ロック無し)。
    pub fn set_effective(&mut self, e: crate::mod_tick::FollowerEff, sample_rate: u32) {
        self.atk = time_coeff(e.attack_ms, sample_rate);
        self.rel = time_coeff(e.release_ms, sample_rate);
        self.gain = e.gain;
        if let Some(b) = self.band.as_mut() {
            b.a_hp = one_pole_coeff(e.hp_hz, sample_rate);
            b.a_lp = one_pole_coeff(e.lp_hz, sample_rate);
        }
    }

    /// Schedule 再 compile 間の状態移送 (`Schedule::adopt_state_from`)。
    /// 係数 (attack/release/gain/band cutoff — 新 config 由来) は保持した
    /// まま、走行状態 (`env` + band filter state) だけを `old` から引き継ぐ。
    /// これで topology 編集ごとに follower env が 0 へ落ちて変調先が段差を
    /// 踏む問題が消える。RT-safe: f32 コピーのみ。
    pub fn adopt_state_from(&mut self, old: &FollowerSlot) {
        self.env = old.env;
        self.hist = old.hist;
        self.hist_tick = old.hist_tick;
        if let (Some(nb), Some(ob)) = (self.band.as_mut(), old.band.as_ref()) {
            nb.lp_hp = ob.lp_hp;
            nb.lp = ob.lp;
        }
    }

    /// Advance the envelope over `n` frames of the source track's stereo
    /// scratch. Order matches docs/plan_modulation.md §3: stereo detector →
    /// optional band filter → pre-gain → rectify → attack/release one-pole.
    /// RT-safe: pure arithmetic over the provided slices, no alloc / lock.
    /// `first_sample` は `l[0]` の **絶対 song サンプル位置** — 刻み境界を跨ぐたびに
    /// その時点の envelope を [`Self::hist`] へ記録するために要る (境界は絶対位置で
    /// 決まるので、buffer の切り方に依存しない)。
    #[inline]
    pub fn process_block(&mut self, l: &[f32], r: &[f32], n: usize, first_sample: u64) {
        let n = n.min(l.len()).min(r.len());
        let tick_frames = u64::from(crate::mod_tick::MOD_TICK_FRAMES);
        let mut env = self.env;
        for i in 0..n {
            let det = match self.mode {
                FollowerMode::Peak => l[i].abs().max(r[i].abs()),
                FollowerMode::Rms => (0.5 * (l[i] * l[i] + r[i] * r[i])).sqrt(),
            };
            let det = match &mut self.band {
                Some(b) => b.process(det),
                None => det,
            };
            let det = det * self.gain;
            let t = if self.rectify { det.abs() } else { det };
            let coeff = if t > env { self.atk } else { self.rel };
            env += coeff * (t - env);
            // サンプル `abs` を通した直後が刻み境界 `abs+1` なら、その境界の値を記録。
            let boundary = first_sample + i as u64 + 1;
            if boundary.is_multiple_of(tick_frames) {
                #[allow(clippy::cast_possible_wrap)]
                let tick = (boundary / tick_frames) as i64;
                self.hist[(tick as usize) % FOLLOWER_HIST] = env;
                self.hist_tick = tick;
            }
        }
        self.env = env;
    }

    /// 絶対刻み `tick` 時点の envelope。
    ///
    /// 記録の窓 (直近 [`FOLLOWER_HIST`] 刻み) の外なら直近値へ倒す — シーク直後や
    /// 再生開始直後は履歴が無いので、そこだけ「今の値」で始まる。
    #[must_use]
    #[inline]
    pub fn env_at_tick(&self, tick: i64) -> f32 {
        // **窓の外でも buffer 非依存の値へ倒す。** ここで走行中の `self.env` を返すと
        // 「今どこまで音を通したか」= buffer の切り方が滲み出て、live と書き出しが
        // 食い違う (この関数はそれを消すためにある)。
        if tick < 0 || self.hist_tick == i64::MIN {
            // 曲頭より前 / まだ 1 つも記録が無い = 無音。
            return 0.0;
        }
        #[allow(clippy::cast_possible_wrap)]
        let cap = FOLLOWER_HIST as i64;
        // 記録済みの窓へクランプ (端は「記録した中でいちばん近い刻み」)。
        let t = tick.clamp((self.hist_tick - cap + 1).max(0), self.hist_tick);
        self.hist[(t as usize) % FOLLOWER_HIST]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: usize = crate::mod_tick::MOD_TICK_FRAMES as usize;

    fn cfg() -> FollowerConfig {
        FollowerConfig { attack_ms: 1.0, release_ms: 50.0, ..FollowerConfig::default() }
    }

    /// **フォロワーの遅れが buffer の切り方に依存しない**こと (r.md #89)。
    ///
    /// 変調の値面は音を描く**前**に作るので、同じ buffer の env は原理的に使えない。
    /// 遡る量を「1 buffer」で決めていた頃は live (480 frame = 10ms) と書き出し
    /// (1024 frame = 21.3ms) でサイドチェインの立ち上がりがずれていた。
    ///
    /// ここでは実際の消費順を再現する — buffer ごとに **先に**その buffer の刻みが
    /// 読む env を集め、**後で**その buffer の音を通す。集めた列を絶対刻み番号で
    /// 突き合わせるので、切り方を変えても一致しなければ落ちる。
    #[test]
    fn 変調が読むenvはbufferの切り方に依存しない() {
        let sr = 48_000;
        let n = 40 * TICK;
        // 立ち上がりの位置がはっきり出る断続入力 (キック相当)。
        let sig: Vec<f32> = (0..n).map(|i| if i % 800 < 100 { 0.9 } else { 0.0 }).collect();
        let run = |chunks: &[usize]| {
            let mut fs = FollowerSlot::from_config(&cfg(), sr);
            let mut seen: Vec<(i64, f32)> = Vec::new();
            let mut at = 0usize;
            for &c in chunks {
                let end = (at + c).min(n);
                if at >= end {
                    continue;
                }
                // 1) この buffer が踏む刻みが読む env (= 音を通す前)。
                let first_tick = (at / TICK) as i64;
                let last_tick = ((end - 1) / TICK) as i64;
                for k in first_tick..=last_tick {
                    seen.push((k, fs.env_at_tick(k - FOLLOWER_LAG_TICKS)));
                }
                // 2) そのあとで音を通す。
                fs.process_block(&sig[at..end], &sig[at..end], end - at, at as u64);
                at = end;
            }
            seen
        };
        let a = run(&[1024, 1024, 512]);
        let b = run(&[480, 544, 512, 1024, 2]);
        // 刻み番号で突き合わせる (切り方が違うと踏む刻みの重複の仕方が変わる)。
        for (k, va) in &a {
            if let Some((_, vb)) = b.iter().find(|(kk, _)| kk == k) {
                assert_eq!(va, vb, "tick={k} の env が buffer の切り方で変わった");
            }
        }
        assert!(a.iter().any(|(_, v)| *v > 0.01), "そもそも env が動いている");
    }
}
