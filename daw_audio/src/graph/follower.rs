//! Per-source envelope follower (docs/plan_modulation.md §3).
//!
//! RT-safe: no alloc / lock / IO. One [`FollowerSlot`] per `ModSource`,
//! owned by the [`super::Schedule`] (rebuilt on recompile, persists across
//! buffers). `compile_schedule` bakes the coefficients from `FollowerConfig`
//! once; the audio thread only runs the per-sample one-pole math each buffer.

use common::model::{BandFilter, FollowerConfig, FollowerMode};

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
        }
    }

    /// Schedule 再 compile 間の状態移送 (`Schedule::adopt_state_from`)。
    /// 係数 (attack/release/gain/band cutoff — 新 config 由来) は保持した
    /// まま、走行状態 (`env` + band filter state) だけを `old` から引き継ぐ。
    /// これで topology 編集ごとに follower env が 0 へ落ちて変調先が段差を
    /// 踏む問題が消える。RT-safe: f32 コピーのみ。
    pub fn adopt_state_from(&mut self, old: &FollowerSlot) {
        self.env = old.env;
        if let (Some(nb), Some(ob)) = (self.band.as_mut(), old.band.as_ref()) {
            nb.lp_hp = ob.lp_hp;
            nb.lp = ob.lp;
        }
    }

    /// Advance the envelope over `n` frames of the source track's stereo
    /// scratch. Order matches docs/plan_modulation.md §3: stereo detector →
    /// optional band filter → pre-gain → rectify → attack/release one-pole.
    /// RT-safe: pure arithmetic over the provided slices, no alloc / lock.
    #[inline]
    pub fn process_block(&mut self, l: &[f32], r: &[f32], n: usize) {
        let n = n.min(l.len()).min(r.len());
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
        }
        self.env = env;
    }
}
