// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `ViewportState1D` — 1 次元 (時間軸 / pitch / track index 等) の表示範囲 + pan/zoom 状態。
//!
//! M5 までは sample_editor / waveform_validation / sample_edit_ops / piano_roll / arrangement の
//! 各 example が同じ式 (`pan_pixels` / `zoom_at` / `unit_to_px` / `px_to_unit`) を独立に書いていた。
//! M7 Phase 22 で library に集約 (DRY、`feedback_use_new_abstractions` 適合)。
//!
//! 単位 (`unit`) は呼び出し側のドメインに依存:
//! - sample_editor / waveform_validation: 1 unit = 1 sample (frame)
//! - piano_roll: 1 unit = 1 beat (X 軸) / 1 semitone (Y 軸)
//! - arrangement: 1 unit = 1 sample (X 軸) / 1 track (Y 軸)
//!
//! `f64` を採用するのは、大規模 DAW project の sample 数 (= 24h @ 48 kHz ≈ 4.1G) が `u32` を
//! 超えるため。fractional zoom (e.g. 0.5 sample / px のサブサンプル表示) も自然に扱える。

/// 1 次元の view 範囲 + pan/zoom helper。`view_start` / `view_len` は呼び出し側のドメイン単位。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportState1D {
    /// 表示範囲の先頭 unit (≥ 0)。
    pub view_start: f64,
    /// 表示範囲の長さ unit (> 0)。
    pub view_len: f64,
}

impl ViewportState1D {
    pub const fn new(start: f64, len: f64) -> Self {
        Self { view_start: start, view_len: len }
    }

    /// `dx_px` 分の pan を適用 (画面 right ドラッグ = `dx_px > 0` → view_start 減少)。
    /// `viewport_px` は widget の幅 (px)、`view_len` を参照して 1 px あたりの unit 数を求める。
    /// クランプは行わない (利用者が `clamp_to` で総 unit 上限に揃える)。
    pub fn pan_pixels(&mut self, dx_px: f32, viewport_px: f32) {
        if viewport_px <= 0.0 || self.view_len <= 0.0 {
            return;
        }
        let units_per_px = self.view_len / f64::from(viewport_px);
        self.view_start -= f64::from(dx_px) * units_per_px;
    }

    /// `anchor_frac` (0..1) を中心に `factor` 倍 zoom する (factor < 1 = zoom in)。
    /// クランプ無し。`min_len` 以上を保つことだけ保証 (factor=0 で長さ 0 になるのを防止)。
    pub fn zoom_at(&mut self, factor: f32, anchor_frac: f32, min_len: f64) {
        if self.view_len <= 0.0 {
            return;
        }
        let anchor_unit = self.view_start + f64::from(anchor_frac) * self.view_len;
        let new_len = (self.view_len * f64::from(factor)).max(min_len);
        let new_anchor_offset = f64::from(anchor_frac) * new_len;
        self.view_start = anchor_unit - new_anchor_offset;
        self.view_len = new_len;
    }

    /// `view_start` / `view_len` を `[0, max_units]` 内に収める。
    /// pan / zoom 後の境界補正に使う。
    pub fn clamp_to(&mut self, max_units: f64) {
        if max_units <= 0.0 {
            self.view_start = 0.0;
            self.view_len = self.view_len.max(0.0);
            return;
        }
        if self.view_len > max_units {
            self.view_len = max_units;
        }
        if self.view_len < 0.0 {
            self.view_len = 0.0;
        }
        let max_start = max_units - self.view_len;
        if self.view_start < 0.0 {
            self.view_start = 0.0;
        } else if self.view_start > max_start {
            self.view_start = max_start;
        }
    }

    /// `unit` を `viewport_px` 内のピクセル座標に変換 (左端 = view_start、右端 = view_start+view_len)。
    pub fn unit_to_px(&self, u: f64, viewport_px: f32) -> f32 {
        if self.view_len <= 0.0 {
            return 0.0;
        }
        ((u - self.view_start) / self.view_len * f64::from(viewport_px)) as f32
    }

    /// ピクセル座標を `unit` に変換。
    pub fn px_to_unit(&self, x_px: f32, viewport_px: f32) -> f64 {
        if viewport_px <= 0.0 {
            return self.view_start;
        }
        self.view_start + f64::from(x_px) / f64::from(viewport_px) * self.view_len
    }

    /// 1 ピクセルあたりの unit 数 (`view_len / viewport_px`)。
    pub fn units_per_pixel(&self, viewport_px: f32) -> f64 {
        if viewport_px <= 0.0 {
            return 0.0;
        }
        self.view_len / f64::from(viewport_px)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn pan_pixels_moves_view_start() {
        let mut v = ViewportState1D::new(1000.0, 4800.0);
        // 100px 右ドラッグ → view 全体が 100px 左に動く = view_start 減少
        // viewport=1920px、view_len=4800、units_per_px=2.5 → 100px = 250 units
        v.pan_pixels(100.0, 1920.0);
        assert!((v.view_start - 750.0).abs() < 1e-3);
        assert_eq!(v.view_len, 4800.0);
    }

    #[test]
    fn zoom_at_anchor_left_keeps_left_edge() {
        let mut v = ViewportState1D::new(0.0, 100.0);
        v.zoom_at(0.5, 0.0, 1.0);
        // anchor_frac=0 で zoom in → 左端は動かない、長さ半分
        assert_eq!(v.view_start, 0.0);
        assert_eq!(v.view_len, 50.0);
    }

    #[test]
    fn zoom_at_anchor_center_centers() {
        let mut v = ViewportState1D::new(0.0, 100.0);
        v.zoom_at(0.5, 0.5, 1.0);
        // anchor_frac=0.5 (中央=50) で zoom in → 中央 50 を保ったまま長さ半分 = [25, 75]
        assert_eq!(v.view_start, 25.0);
        assert_eq!(v.view_len, 50.0);
    }

    #[test]
    fn clamp_to_bounds_view_start() {
        let mut v = ViewportState1D::new(-10.0, 100.0);
        v.clamp_to(200.0);
        assert_eq!(v.view_start, 0.0);
        assert_eq!(v.view_len, 100.0);

        let mut v = ViewportState1D::new(150.0, 100.0);
        v.clamp_to(200.0);
        assert_eq!(v.view_start, 100.0); // 200 - 100
        assert_eq!(v.view_len, 100.0);
    }

    #[test]
    fn unit_to_px_round_trip() {
        let v = ViewportState1D::new(1000.0, 4800.0);
        let u = 2500.0;
        let px = v.unit_to_px(u, 1920.0);
        let u2 = v.px_to_unit(px, 1920.0);
        assert!((u - u2).abs() < 1e-3);
    }

    #[test]
    fn zoom_min_len_prevents_zero() {
        let mut v = ViewportState1D::new(0.0, 100.0);
        v.zoom_at(0.0, 0.5, 1.0);
        assert_eq!(v.view_len, 1.0);
    }
}
