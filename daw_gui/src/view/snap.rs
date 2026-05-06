//! Grid snap UI ヘルパ。
//!
//! Phase A の暫定実装は M14 Phase 60 で gui_01 (`daw_ui_core::SnapConfig`) に移行した。
//! 本モジュールは gui_01 type の re-export と、daw_01 専用の dropdown UI 用 helper
//! (`SNAP_LABELS` / `choice_to_mode` / `narrow_choice` 等) を持つ。
//!
//! 注意: `SnapConfig::default() == Adaptive ON` (gui_01 #010 で確定)。 daw_01 は
//! `pianoroll_snap_choice` 等から明示的に SnapMode を組み立てるので Default に依存しない。
//!
//! `1/1` (= whole note = 4 beats) は dropdown 候補から除外: 4/4 では `1 bar` と同値、
//! 4/4 以外でも user の運用上不要のため (gui_01 SnapMode::Straight { div: 1 } 自体は残る)。
//! `Adaptive` は `1/2` と `1 bar` の中間 (粗 ↔ 細の連続移動の架け橋) に配置。

pub use daw_ui_core::{SnapConfig, SnapMode};

use crate::app::AppData;

/// Snap dropdown に並ぶ choice 一覧。index は AppData.{pianoroll,arrange}_snap_choice と同期。
pub const SNAP_LABELS: &[&str] = &[
    "1/2", "1/4", "1/8", "1/16", "1/32", "1/64", "1/128", // 0..=6  Straight (1/1 除外)
    "1/2T", "1/4T", "1/8T", "1/16T", "1/32T",             // 7..=11 Triplet
    "1/4.", "1/8.", "1/16.", "1/32.",                     // 12..=15 Dotted
    "Adaptive",                                           // 16
    "1 bar", "2 bar", "4 bar",                            // 17..=19 Bars
];

pub const CHOICE_PIANOROLL_DEFAULT: u8 = 3; // "1/16"
pub const CHOICE_ARRANGE_DEFAULT: u8 = 1; // "1/4"

/// `pianoroll_snap_choice` / `arrange_snap_choice` index → SnapMode 変換。
/// 不正 index は SnapMode::Off を返す。
pub fn choice_to_mode(idx: u8) -> SnapMode {
    match idx {
        0 => SnapMode::Straight { div: 2 },
        1 => SnapMode::Straight { div: 4 },
        2 => SnapMode::Straight { div: 8 },
        3 => SnapMode::Straight { div: 16 },
        4 => SnapMode::Straight { div: 32 },
        5 => SnapMode::Straight { div: 64 },
        6 => SnapMode::Straight { div: 128 },
        7 => SnapMode::Triplet { div: 2 },
        8 => SnapMode::Triplet { div: 4 },
        9 => SnapMode::Triplet { div: 8 },
        10 => SnapMode::Triplet { div: 16 },
        11 => SnapMode::Triplet { div: 32 },
        12 => SnapMode::Dotted { div: 4 },
        13 => SnapMode::Dotted { div: 8 },
        14 => SnapMode::Dotted { div: 16 },
        15 => SnapMode::Dotted { div: 32 },
        16 => SnapMode::Adaptive,
        17 => SnapMode::Bars { count: 1 },
        18 => SnapMode::Bars { count: 2 },
        19 => SnapMode::Bars { count: 4 },
        _ => SnapMode::Off,
    }
}

/// SnapMode → choice index (該当無しなら None)。
/// `Straight { div: 1 }` (= "1/1") は dropdown から除外したため None を返す。
pub fn mode_to_choice(mode: SnapMode) -> Option<u8> {
    match mode {
        SnapMode::Straight { div: 2 } => Some(0),
        SnapMode::Straight { div: 4 } => Some(1),
        SnapMode::Straight { div: 8 } => Some(2),
        SnapMode::Straight { div: 16 } => Some(3),
        SnapMode::Straight { div: 32 } => Some(4),
        SnapMode::Straight { div: 64 } => Some(5),
        SnapMode::Straight { div: 128 } => Some(6),
        SnapMode::Triplet { div: 2 } => Some(7),
        SnapMode::Triplet { div: 4 } => Some(8),
        SnapMode::Triplet { div: 8 } => Some(9),
        SnapMode::Triplet { div: 16 } => Some(10),
        SnapMode::Triplet { div: 32 } => Some(11),
        SnapMode::Dotted { div: 4 } => Some(12),
        SnapMode::Dotted { div: 8 } => Some(13),
        SnapMode::Dotted { div: 16 } => Some(14),
        SnapMode::Dotted { div: 32 } => Some(15),
        SnapMode::Adaptive => Some(16),
        SnapMode::Bars { count: 1 } => Some(17),
        SnapMode::Bars { count: 2 } => Some(18),
        SnapMode::Bars { count: 4 } => Some(19),
        _ => None,
    }
}

pub fn piano_roll_snap_config(app: &AppData) -> SnapConfig {
    SnapConfig {
        mode: choice_to_mode(app.pianoroll_snap_choice),
        enabled: app.pianoroll_snap_enabled,
        min_beat_unit: 1.0 / 128.0,
        time_sig: app.song.time_sig,
    }
}

pub fn arrange_snap_config(app: &AppData) -> SnapConfig {
    SnapConfig {
        mode: choice_to_mode(app.arrange_snap_choice),
        enabled: app.arrange_snap_enabled,
        min_beat_unit: 1.0 / 128.0,
        time_sig: app.song.time_sig,
    }
}

/// "1" キー (Narrow Grid): 細かく方向。 流れ:
/// `4 bar → 2 bar → 1 bar → Adaptive → 1/2 → 1/4 → ... → 1/128` (no-op)。
/// Triplet / Dotted 系は内部で div 倍化 (32 で頭打ち)。
pub fn narrow_choice(idx: u8) -> u8 {
    let mode = choice_to_mode(idx);
    let new_mode = match mode {
        SnapMode::Bars { count } if count > 1 => SnapMode::Bars { count: count / 2 },
        SnapMode::Bars { count: 1 } => SnapMode::Adaptive,
        SnapMode::Adaptive => SnapMode::Straight { div: 2 },
        SnapMode::Straight { div } if div < 128 => SnapMode::Straight { div: div * 2 },
        SnapMode::Triplet { div } if div < 32 => SnapMode::Triplet { div: div * 2 },
        SnapMode::Dotted { div } if div < 32 => SnapMode::Dotted { div: div * 2 },
        other => other,
    };
    mode_to_choice(new_mode).unwrap_or(idx)
}

/// "2" キー (Widen Grid): 粗く方向。 流れ:
/// `1/128 → ... → 1/4 → 1/2 → Adaptive → 1 bar → 2 bar → 4 bar` (no-op)。
/// Triplet / Dotted 系は内部で div 半分 (Triplet は 2 で / Dotted は 4 で底)。
pub fn widen_choice(idx: u8) -> u8 {
    let mode = choice_to_mode(idx);
    let new_mode = match mode {
        SnapMode::Straight { div } if div > 2 => SnapMode::Straight { div: div / 2 },
        SnapMode::Straight { div: 2 } => SnapMode::Adaptive,
        SnapMode::Adaptive => SnapMode::Bars { count: 1 },
        SnapMode::Bars { count } if count < 4 => SnapMode::Bars { count: count * 2 },
        SnapMode::Triplet { div } if div > 2 => SnapMode::Triplet { div: div / 2 },
        SnapMode::Dotted { div } if div > 4 => SnapMode::Dotted { div: div / 2 },
        other => other,
    };
    mode_to_choice(new_mode).unwrap_or(idx)
}

/// "3" キー (Toggle Triplet): Straight ↔ Triplet (div は維持、対応無しなら no-op)。
pub fn toggle_triplet_choice(idx: u8) -> u8 {
    let mode = choice_to_mode(idx);
    let new_mode = match mode {
        SnapMode::Straight { div } if (2..=32).contains(&div) => SnapMode::Triplet { div },
        SnapMode::Triplet { div } => SnapMode::Straight { div },
        other => other,
    };
    mode_to_choice(new_mode).unwrap_or(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_progresses_straight() {
        // "1/4" (idx 1) → "1/8" (idx 2) → "1/16" (idx 3)
        assert_eq!(narrow_choice(1), 2);
        assert_eq!(narrow_choice(2), 3);
        // 最大 "1/128" (idx 6) は no-op
        assert_eq!(narrow_choice(6), 6);
    }

    #[test]
    fn widen_progresses_straight() {
        // "1/4" (idx 1) → "1/2" (idx 0)
        assert_eq!(widen_choice(1), 0);
    }

    #[test]
    fn widen_crosses_through_adaptive_to_bars() {
        // "1/2" (idx 0) → Adaptive (16) → "1 bar" (17) → "2 bar" (18) → "4 bar" (19)
        assert_eq!(widen_choice(0), 16);
        assert_eq!(widen_choice(16), 17);
        assert_eq!(widen_choice(17), 18);
        assert_eq!(widen_choice(18), 19);
        // 最大 "4 bar" (idx 19) は no-op
        assert_eq!(widen_choice(19), 19);
    }

    #[test]
    fn narrow_crosses_back_through_adaptive_to_straight() {
        // "4 bar" (19) → "2 bar" (18) → "1 bar" (17) → Adaptive (16) → "1/2" (0)
        assert_eq!(narrow_choice(19), 18);
        assert_eq!(narrow_choice(18), 17);
        assert_eq!(narrow_choice(17), 16);
        assert_eq!(narrow_choice(16), 0);
    }

    #[test]
    fn toggle_triplet_round_trip() {
        // "1/8" (idx 2) → "1/8T" (idx 9)
        let i = toggle_triplet_choice(2);
        assert_eq!(i, 9);
        // "1/8T" → "1/8"
        assert_eq!(toggle_triplet_choice(i), 2);
    }

    #[test]
    fn toggle_triplet_no_pair_is_noop() {
        // "Adaptive" (idx 16) は no-op
        assert_eq!(toggle_triplet_choice(16), 16);
        // "1 bar" (idx 17) も no-op (Bars に triplet 対応無し)
        assert_eq!(toggle_triplet_choice(17), 17);
        // "1/4." (idx 12) Dotted も no-op
        assert_eq!(toggle_triplet_choice(12), 12);
    }

    #[test]
    fn choice_round_trip() {
        // SNAP_LABELS の全 index で choice → mode → choice が round-trip。
        for i in 0..SNAP_LABELS.len() as u8 {
            let mode = choice_to_mode(i);
            assert_eq!(mode_to_choice(mode), Some(i), "index {i}");
        }
    }

    #[test]
    fn bars_choice_round_trip() {
        // 1 bar (17) / 2 bar (18) / 4 bar (19) は Bars { count } に変換、戻す。
        for i in 17..=19 {
            let mode = choice_to_mode(i);
            assert!(matches!(mode, SnapMode::Bars { .. }), "idx {i}");
            assert_eq!(mode_to_choice(mode), Some(i));
        }
    }

    #[test]
    fn adaptive_at_idx_16() {
        // "Adaptive" は "1/2 と 1 bar の中間" idx 16 に配置。
        assert!(matches!(choice_to_mode(16), SnapMode::Adaptive));
        assert_eq!(mode_to_choice(SnapMode::Adaptive), Some(16));
    }
}
