// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! オシロスコープの波形取り込み (r.md #50)。
//!
//! 表示窓 (既定 20ms) のぶんだけリングから切り出し、列ごとの min/max にする。
//! トリガは Mid (L+R) のゼロクロスで、**サブサンプル位置を線形補間**して窓の
//! 開始位置を小数で決める。これがないと 1 サンプル (48kHz で 20.8µs) のずれが
//! そのまま横揺れになる。
//!
//! zita-scope は ×5 → 局所 ×25 アップサンプル + 放物線フィットで
//! 1/100000 サンプル精度を出すが、あれは 50µs/div のような超高速掃引が前提。
//! 音楽用の 20ms / 640px では 1px ≒ 1.5 サンプルなので、線形補間の残差
//! (滑らかな波形で 0.1 サンプル未満) は 0.07px 未満 = ピクセル格子より 1 桁
//! 細かく、それ以上の精度は表示に現れない。

use super::settings::ScopeTrigger;

/// 列数 (パネル最大幅より十分多く取る。widget 側は min/max でさらに畳む)。
pub const SCOPE_COLUMNS: usize = 768;

/// リング容量 [frames]。最長窓 (100ms) + 同じ長さのトリガ探索範囲を
/// 192kHz でも収める (100ms × 2 × 192k = 38400 < 65536)。
const RING_FRAMES: usize = 1 << 16;
const RING_MASK: u64 = (RING_FRAMES as u64) - 1;

/// 1 列ぶんの `[L 最小, L 最大, R 最小, R 最大]`。
pub type ScopeColumn = [f32; 4];

pub struct ScopeCapture {
    sample_rate: u32,
    ring: Vec<[f32; 2]>,
    /// 累積書き込みフレーム数。
    write: u64,
    columns: Vec<ScopeColumn>,
}

impl ScopeCapture {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            ring: vec![[0.0; 2]; RING_FRAMES],
            write: 0,
            columns: vec![[0.0; 4]; SCOPE_COLUMNS],
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let sr = sample_rate.max(1);
        if sr != self.sample_rate {
            self.sample_rate = sr;
            self.reset();
        }
    }

    pub fn reset(&mut self) {
        self.ring.fill([0.0; 2]);
        self.write = 0;
        self.columns.fill([0.0; 4]);
    }

    pub fn push(&mut self, frames: &[[f32; 2]]) {
        for fr in frames {
            self.ring[(self.write & RING_MASK) as usize] = *fr;
            self.write += 1;
        }
    }

    #[inline]
    fn at(&self, i: u64) -> [f32; 2] {
        self.ring[(i & RING_MASK) as usize]
    }

    #[inline]
    fn mid(&self, i: u64) -> f32 {
        let f = self.at(i);
        (f[0] + f[1]) * 0.5
    }

    /// 窓の開始位置 (小数フレーム) を決める。トリガが見つからなければ最新窓の先頭。
    fn find_start(&self, window: u64, trigger: ScopeTrigger) -> f64 {
        let free_start = self.write.saturating_sub(window);
        if matches!(trigger, ScopeTrigger::Free) {
            return free_start as f64;
        }
        // 探索範囲: [free_start - window, free_start]。リングに無い所は見ない。
        let oldest = self.write.saturating_sub(RING_FRAMES as u64 - 1);
        let lo = free_start.saturating_sub(window).max(oldest);
        if free_start <= lo {
            return free_start as f64;
        }
        let rising = matches!(trigger, ScopeTrigger::RisingZero);
        let mut i = free_start;
        while i > lo {
            let a = self.mid(i - 1);
            let b = self.mid(i);
            let crossed = if rising { a <= 0.0 && b > 0.0 } else { a >= 0.0 && b < 0.0 };
            if crossed {
                let denom = b - a;
                let frac = if denom.abs() < 1e-12 { 0.0 } else { (-a / denom) as f64 };
                return (i - 1) as f64 + frac.clamp(0.0, 1.0);
            }
            i -= 1;
        }
        free_start as f64
    }

    /// 現在の設定で 1 フレーム分の列を作る。データが足りなければ `false`。
    pub fn capture(&mut self, window_ms: f32, trigger: ScopeTrigger) -> bool {
        let window = ((window_ms.max(1.0) / 1000.0) * self.sample_rate as f32) as u64;
        let window = window.clamp(16, (RING_FRAMES / 2) as u64);
        if self.write < window {
            return false;
        }
        let start = self.find_start(window, trigger);
        let oldest = self.write.saturating_sub(RING_FRAMES as u64 - 1) as f64;
        let start = start.max(oldest);
        let step = window as f64 / SCOPE_COLUMNS as f64;
        for c in 0..SCOPE_COLUMNS {
            let p0 = start + c as f64 * step;
            let p1 = p0 + step;
            self.columns[c] = if step < 1.0 {
                // 1 列 < 1 サンプル: 折れ線として見せたいので線形補間する。
                let v = self.sample_interpolated(p0);
                [v[0], v[0], v[1], v[1]]
            } else {
                self.column_min_max(p0, p1)
            };
        }
        true
    }

    fn sample_interpolated(&self, pos: f64) -> [f32; 2] {
        let i = pos.floor();
        let t = (pos - i) as f32;
        let i = i.max(0.0) as u64;
        let a = self.at(i);
        let b = self.at((i + 1).min(self.write.saturating_sub(1)));
        [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
    }

    fn column_min_max(&self, p0: f64, p1: f64) -> ScopeColumn {
        let i0 = p0.ceil().max(0.0) as u64;
        let mut i1 = p1.ceil().max(0.0) as u64;
        if i1 <= i0 {
            i1 = i0 + 1;
        }
        let limit = self.write;
        let mut out = [f32::MAX, f32::MIN, f32::MAX, f32::MIN];
        let mut any = false;
        let mut i = i0;
        while i < i1 && i < limit {
            let f = self.at(i);
            out[0] = out[0].min(f[0]);
            out[1] = out[1].max(f[0]);
            out[2] = out[2].min(f[1]);
            out[3] = out[3].max(f[1]);
            any = true;
            i += 1;
        }
        if any { out } else { [0.0; 4] }
    }

    pub fn columns(&self) -> &[ScopeColumn] {
        &self.columns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(fs: u32, freq: f32, n: usize, phase: f32) -> Vec<[f32; 2]> {
        (0..n)
            .map(|i| {
                let v = (std::f32::consts::TAU * freq * i as f32 / fs as f32 + phase).sin();
                [v, v]
            })
            .collect()
    }

    /// トリガをかけると、窓の先頭は必ず立ち上がりゼロクロス (値 ≈ 0、傾き正)。
    #[test]
    fn rising_trigger_starts_the_window_at_a_zero_crossing() {
        let fs = 48_000;
        let mut s = ScopeCapture::new(fs);
        s.push(&sine(fs, 100.0, 20_000, 0.7));
        assert!(s.capture(20.0, ScopeTrigger::RisingZero));
        let first = s.columns()[0];
        // L の min/max とも 0 近傍から始まる。
        assert!(first[0].abs() < 0.05 && first[1].abs() < 0.08, "got {first:?}");
        // 直後の列は正へ向かう。
        let later = s.columns()[SCOPE_COLUMNS / 40];
        assert!(later[1] > first[1], "not rising: {first:?} -> {later:?}");
    }

    /// 位相を変えても、トリガ後の波形は同じ位置から始まる (横揺れしない)。
    #[test]
    fn trigger_removes_phase_jitter_between_captures() {
        let fs = 48_000;
        let mut a = ScopeCapture::new(fs);
        let mut b = ScopeCapture::new(fs);
        a.push(&sine(fs, 220.0, 20_000, 0.0));
        b.push(&sine(fs, 220.0, 20_003, 0.0)); // 3 サンプルずらして書く
        a.capture(20.0, ScopeTrigger::RisingZero);
        b.capture(20.0, ScopeTrigger::RisingZero);
        for c in [0, 50, 200, 500] {
            let (x, y) = (a.columns()[c], b.columns()[c]);
            assert!(
                (x[1] - y[1]).abs() < 0.05,
                "column {c} drifted: {x:?} vs {y:?}"
            );
        }
    }

    /// トリガ無しは最新の窓をそのまま切り出す。
    #[test]
    fn free_run_uses_the_newest_window() {
        let fs = 48_000;
        let mut s = ScopeCapture::new(fs);
        let mut frames = vec![[0.0_f32, 0.0]; 4_000];
        // 末尾 1 フレームだけ 1.0 にして、最終列に出ることを確認する。
        *frames.last_mut().unwrap() = [1.0, 1.0];
        s.push(&frames);
        assert!(s.capture(20.0, ScopeTrigger::Free));
        let last = s.columns()[SCOPE_COLUMNS - 1];
        assert!(last[1] > 0.5, "last column = {last:?}");
    }

    /// データが窓に満たないうちは capture しない (ゴミを描かない)。
    #[test]
    fn capture_fails_before_a_full_window_is_available() {
        let mut s = ScopeCapture::new(48_000);
        s.push(&vec![[0.1, 0.1]; 100]);
        assert!(!s.capture(20.0, ScopeTrigger::RisingZero));
    }

    /// L と R が別々に取り込まれる。
    #[test]
    fn channels_are_captured_independently() {
        let fs = 48_000;
        let mut s = ScopeCapture::new(fs);
        let frames: Vec<[f32; 2]> = (0..4_000).map(|_| [0.8, -0.3]).collect();
        s.push(&frames);
        s.capture(20.0, ScopeTrigger::Free);
        let c = s.columns()[SCOPE_COLUMNS / 2];
        assert!((c[1] - 0.8).abs() < 1e-4, "L = {c:?}");
        assert!((c[3] - (-0.3)).abs() < 1e-4, "R = {c:?}");
    }

    /// 極端に短い窓 (1 列 < 1 サンプル) でも折れ線として値が並ぶ。
    #[test]
    fn sub_sample_columns_interpolate_instead_of_stepping() {
        let fs = 48_000;
        let mut s = ScopeCapture::new(fs);
        // 直線的に増える信号を入れる。
        let frames: Vec<[f32; 2]> = (0..4_000)
            .map(|i| {
                let v = i as f32 / 4_000.0;
                [v, v]
            })
            .collect();
        s.push(&frames);
        // 5ms = 240 サンプル < 768 列。
        assert!(s.capture(5.0, ScopeTrigger::Free));
        let a = s.columns()[100][1];
        let b = s.columns()[101][1];
        assert!(b > a, "interpolation collapsed: {a} then {b}");
    }
}
