// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Master-output level meter primitives shared between daw_audio (which
//! computes the raw per-block peak) and daw_gui (which converts to dB and
//! runs the visual decay).

pub const METER_DB_MIN: f32 = -60.0;
pub const METER_DB_MAX: f32 = 0.0;

/// Returns `max(|s|)` over `samples`. Empty slice returns `0.0`.
pub fn compute_block_peak(samples: &[f32]) -> f32 {
    let mut peak = 0.0_f32;
    for &s in samples {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
    }
    peak
}

/// Converts a linear amplitude to decibels. `v <= 0` clamps to
/// [`METER_DB_MIN`]; the result is never below that floor.
pub fn linear_to_db(v: f32) -> f32 {
    if v <= 0.0 {
        return METER_DB_MIN;
    }
    (20.0 * v.log10()).max(METER_DB_MIN)
}

/// Maps a dB value to `[0.0, 1.0]` for meter-bar rendering:
/// [`METER_DB_MIN`] → 0.0, [`METER_DB_MAX`] → 1.0, clamped to that range.
pub fn db_to_norm(db: f32) -> f32 {
    ((db - METER_DB_MIN) / (METER_DB_MAX - METER_DB_MIN)).clamp(0.0, 1.0)
}

/// Peak-meter release: classic fast-attack, exponential-release. When the
/// incoming value is louder, it snaps in instantly; otherwise the previous
/// displayed value decays by `release_factor` (typically `0.80`–`0.90` at a
/// 30 Hz UI tick, giving ~20–40 dB/s fall).
pub fn update_peak(prev: f32, new_value: f32, release_factor: f32) -> f32 {
    if new_value >= prev {
        new_value
    } else {
        prev * release_factor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) {
        assert!(
            (a - b).abs() < 0.01,
            "expected approx {b}, got {a} (delta {})",
            (a - b).abs()
        );
    }

    #[test]
    fn block_peak_empty_is_zero() {
        assert_eq!(compute_block_peak(&[]), 0.0);
    }

    #[test]
    fn block_peak_picks_largest_absolute_value() {
        assert_eq!(compute_block_peak(&[0.1, -0.4, 0.2, -0.05]), 0.4);
    }

    #[test]
    fn linear_to_db_unity_is_zero() {
        approx(linear_to_db(1.0), 0.0);
    }

    #[test]
    fn linear_to_db_half_is_minus_six() {
        approx(linear_to_db(0.5), -6.02);
    }

    #[test]
    fn linear_to_db_zero_clamps_to_min() {
        assert_eq!(linear_to_db(0.0), METER_DB_MIN);
    }

    #[test]
    fn linear_to_db_below_min_is_clamped() {
        // 1e-10 would be -200 dB; we expect the METER_DB_MIN floor.
        assert_eq!(linear_to_db(1e-10), METER_DB_MIN);
    }

    #[test]
    fn db_to_norm_endpoints() {
        assert_eq!(db_to_norm(METER_DB_MIN), 0.0);
        assert_eq!(db_to_norm(METER_DB_MAX), 1.0);
    }

    #[test]
    fn db_to_norm_midpoint() {
        // -30 dB should sit halfway for a linear -60..0 mapping.
        approx(db_to_norm(-30.0), 0.5);
    }

    #[test]
    fn db_to_norm_clamps_above_max() {
        assert_eq!(db_to_norm(6.0), 1.0);
    }

    #[test]
    fn update_peak_snaps_up() {
        assert_eq!(update_peak(0.2, 0.7, 0.85), 0.7);
    }

    #[test]
    fn update_peak_decays_when_new_is_lower() {
        // 0.7 * 0.85 = 0.595
        approx(update_peak(0.7, 0.1, 0.85), 0.595);
    }

    #[test]
    fn update_peak_stays_flat_when_equal() {
        assert_eq!(update_peak(0.5, 0.5, 0.85), 0.5);
    }
}
