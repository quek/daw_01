//! `SnapConfig` / `SnapMode` の単位 test。
//!
//! reference 実装は `daw_01/daw_gui/src/view/snap.rs` (free function 版) で、
//! こちらは inherent method 化 + Default を `Adaptive ON` に変更済み。

use daw_ui_core::{SnapConfig, SnapMode};

#[test]
fn default_is_adaptive_on() {
    let cfg = SnapConfig::default();
    assert!(cfg.enabled);
    assert!(matches!(cfg.mode, SnapMode::Adaptive));
    assert!((cfg.min_beat_unit - 1.0 / 128.0).abs() < f64::EPSILON);
    assert_eq!(cfg, SnapConfig::DEFAULT);
}

#[test]
fn off_const_is_inactive() {
    let cfg = SnapConfig::OFF;
    assert!(!cfg.enabled);
    assert!(matches!(cfg.mode, SnapMode::Off));
    assert!(!cfg.is_active(false));
    assert!(!cfg.is_active(true));
    assert!((cfg.snap_beat(1.234, false, 64.0) - 1.234).abs() < 1e-12);
}

#[test]
fn snap_disabled_returns_raw() {
    let cfg = SnapConfig {
        mode: SnapMode::Straight { div: 16 },
        enabled: false,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
    };
    assert!(!cfg.is_active(false));
    assert!((cfg.snap_beat(1.234, false, 64.0) - 1.234).abs() < 1e-12);
    assert!((cfg.snap_beat_delta(1.234, false, 64.0) - 1.234).abs() < 1e-12);
}

#[test]
fn alt_pressed_returns_raw() {
    let cfg = SnapConfig {
        mode: SnapMode::Straight { div: 16 },
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
    };
    assert!(cfg.is_active(false));
    assert!(!cfg.is_active(true));
    assert!((cfg.snap_beat(1.234, true, 64.0) - 1.234).abs() < 1e-12);
    assert!((cfg.snap_beat_delta(1.234, true, 64.0) - 1.234).abs() < 1e-12);
}

#[test]
fn straight_16_snaps_quarter() {
    let cfg = SnapConfig {
        mode: SnapMode::Straight { div: 16 },
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
    };
    // 1/16 = 0.0625 拍。1.234 / 0.0625 = 19.744 → round 20 → 20 * 0.0625 = 1.25
    let snapped = cfg.snap_beat(1.234, false, 64.0);
    assert!((snapped - 1.25).abs() < 1e-9, "got {snapped}");
}

#[test]
fn triplet_4_unit() {
    let cfg = SnapConfig {
        mode: SnapMode::Triplet { div: 4 },
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
    };
    let unit = cfg.beat_unit(64.0).expect("active");
    // (2/3) / 4 = 0.16666...
    assert!((unit - (2.0 / 3.0 / 4.0)).abs() < 1e-9);
}

#[test]
fn dotted_8_unit() {
    let cfg = SnapConfig {
        mode: SnapMode::Dotted { div: 8 },
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
    };
    let unit = cfg.beat_unit(64.0).expect("active");
    // 1.5 / 8 = 0.1875
    assert!((unit - 0.1875).abs() < 1e-9);
}

#[test]
fn adaptive_picks_coarser_at_low_zoom() {
    let cfg = SnapConfig::DEFAULT;
    // zoom_x = 4 px/beat (極端に zoom out): unit_px = 4 * 1 = 4 < 12 で 1.0 維持
    let unit = cfg.beat_unit(4.0).unwrap();
    assert!((unit - 1.0).abs() < 1e-12);

    // zoom_x = 64: 64 * 1/8 = 8 < 12, 64 * 1/4 = 16 >= 12 → unit = 1/4
    let unit = cfg.beat_unit(64.0).unwrap();
    assert!((unit - 0.25).abs() < 1e-9);

    // zoom_x = 1600: 1600 * 1/128 = 12.5 >= 12 → unit = 1/128
    let unit = cfg.beat_unit(1600.0).unwrap();
    assert!((unit - (1.0 / 128.0)).abs() < 1e-9);
}

#[test]
fn min_beat_unit_floor() {
    // min_beat_unit = 1/64 だと Adaptive zoom 1600.0 でも 1/128 まで行かず 1/64 で止まる
    let cfg = SnapConfig {
        mode: SnapMode::Adaptive,
        enabled: true,
        min_beat_unit: 1.0 / 64.0,
        time_sig: (4, 4),
    };
    let unit = cfg.beat_unit(1600.0).unwrap();
    assert!((unit - (1.0 / 64.0)).abs() < 1e-9, "got {unit}");
}

#[test]
fn snap_beat_delta_negative() {
    let cfg = SnapConfig {
        mode: SnapMode::Straight { div: 16 },
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
    };
    // -1.234 / 0.0625 = -19.744 → round -20 → -20 * 0.0625 = -1.25
    let snapped = cfg.snap_beat_delta(-1.234, false, 64.0);
    assert!((snapped + 1.25).abs() < 1e-9, "got {snapped}");
}

#[test]
fn snap_beat_zero_returns_zero() {
    let cfg = SnapConfig::DEFAULT;
    assert!(cfg.snap_beat(0.0, false, 64.0).abs() < 1e-12);
    assert!(cfg.snap_beat_delta(0.0, false, 64.0).abs() < 1e-12);
}

#[test]
fn min_visible_grid_px_boundary_64() {
    // 12px 閾値の確認: zoom 48 (= 12 / 0.25) で 1/4 が境界
    // 48 * 1/4 = 12 ちょうど → unit = 1/4 が選ばれる
    // 48 * 1/8 = 6 < 12 → 1/8 は行き過ぎ
    let cfg = SnapConfig::DEFAULT;
    let unit = cfg.beat_unit(48.0).unwrap();
    assert!((unit - 0.25).abs() < 1e-9, "got {unit}");
}

// (M14 Phase 61c / daw_01 #011) Bars { count } 系 unit test。 1 bar = `time_sig.0 * 4 /
// time_sig.1` 拍 (4/4 → 4 拍、 3/4 → 3 拍、 6/8 → 3 拍)。 count = 0 は None で defensive。

#[test]
fn bars_1_at_4_4_is_4_beats() {
    let cfg = SnapConfig {
        mode: SnapMode::Bars { count: 1 },
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
    };
    let unit = cfg.beat_unit(64.0).expect("active");
    assert!((unit - 4.0).abs() < 1e-9, "1 bar @ 4/4 = 4 拍、 got {unit}");
}

#[test]
fn bars_2_at_3_4_is_6_beats() {
    let cfg = SnapConfig {
        mode: SnapMode::Bars { count: 2 },
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (3, 4),
    };
    let unit = cfg.beat_unit(64.0).expect("active");
    // 3/4: beats_per_bar = 3 * 4 / 4 = 3 拍。 2 bars = 6 拍。
    assert!((unit - 6.0).abs() < 1e-9, "2 bars @ 3/4 = 6 拍、 got {unit}");
}

#[test]
fn bars_4_at_6_8_is_12_beats() {
    let cfg = SnapConfig {
        mode: SnapMode::Bars { count: 4 },
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (6, 8),
    };
    let unit = cfg.beat_unit(64.0).expect("active");
    // 6/8: beats_per_bar = 6 * 4 / 8 = 3 拍。 4 bars = 12 拍。
    assert!((unit - 12.0).abs() < 1e-9, "4 bars @ 6/8 = 12 拍、 got {unit}");
}

#[test]
fn bars_count_zero_returns_none() {
    let cfg = SnapConfig {
        mode: SnapMode::Bars { count: 0 },
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
    };
    assert!(
        cfg.beat_unit(64.0).is_none(),
        "Bars count=0 は None (defensive、 dropdown 等で 0 漏れ防止)"
    );
}

#[test]
fn bars_snap_aligns_to_bar_boundary() {
    // 4/4 で raw 7.3 拍 → 1 bar (= 4 拍) snap → round(7.3 / 4) = round(1.825) = 2 → 2 * 4 = 8.0
    let cfg = SnapConfig {
        mode: SnapMode::Bars { count: 1 },
        enabled: true,
        min_beat_unit: 1.0 / 128.0,
        time_sig: (4, 4),
    };
    let snapped = cfg.snap_beat(7.3, false, 64.0);
    assert!((snapped - 8.0).abs() < 1e-9, "Bars 1 で 7.3 → 8.0 (2 小節目頭)、 got {snapped}");
}
