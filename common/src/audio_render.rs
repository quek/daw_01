//! Audio rendering の純粋関数 helper (Phase 2 PR-A)。
//!
//! `daw_audio::audio_clip_renderer` (live audio thread の per-buffer mix
//! loop) と `daw_gui::AppData::bounce_clip_in_place` (offline mix → WAV
//! 書き出し) で同一ロジックが重複していたので、 共通 crate (= common
//! crate) に切り出して DRY 化する。
//!
//! 切り出し対象:
//! - [`fade_envelope`]: fade-in / fade-out の 0..=1 envelope (Linear /
//!   Exponential / SCurve)
//! - [`pitch_ratio_for`]: source frame stride per output frame の計算
//!   (Raw / Repitch、 sample-rate 比 + pitch 比)
//!
//! どちらも RT path で呼ばれることを想定し allocation / panic free。
//! Stretch / Slice モードは Phase 1 fallback で Raw 同等の挙動を返す
//! (本実装は Phase 3+)。

use crate::model::{FadeCurve, StretchMode};

/// Fade envelope at frame offset `t` (frames since fade start). Output
/// is in `0..=1`. `fade_len == 0` or `t >= fade_len` で 1.0 (= 完全
/// pass through)。 RT path で呼ばれるので branch 1 つの fast path を
/// 最初に置く。
///
/// 各 curve の数式 (`docs/plan_audio_clip.md` §3.5):
/// - `Linear`: `t / fade_len`
/// - `Exponential`: `(t / fade_len)^2`
/// - `SCurve`: `0.5 - 0.5 * cos(π * t / fade_len)` (= equal-power に
///   近い、 Auto-Crossfade で重なり時のクリップ防止に推奨)
#[inline]
pub fn fade_envelope(t: u64, fade_len: u64, curve: FadeCurve) -> f32 {
    if fade_len == 0 || t >= fade_len {
        return 1.0;
    }
    let x = (t as f32) / (fade_len as f32);
    match curve {
        FadeCurve::Linear => x,
        FadeCurve::Exponential => x * x,
        FadeCurve::SCurve => 0.5 - 0.5 * (std::f32::consts::PI * x).cos(),
    }
}

/// 1 output frame あたりの source frame 進度 (= linear interp の lookup
/// step)。 `Raw` は単純な sample-rate 補正、 `Repitch` は SR 比 ×
/// pitch 比 (タープ式の "tape pitch" 挙動)。 `Stretch` / `Slice` は
/// Phase 1 では Raw 同等で fallback (= pitch も SR 比のみ)、 Phase 3+
/// で granular / chunk crossfade に置き換え予定。
///
/// 単位: `output_frame_at_engine_sr * pitch_ratio = source_frame_at_event_local`。
/// Reverse は別経路 (caller 側で `source_len - 1 - source_pos` する)
/// なのでここでは扱わない。
#[inline]
pub fn pitch_ratio_for(
    stretch_mode: StretchMode,
    source_sample_rate: u32,
    engine_sample_rate: u32,
    pitch_semitones: f32,
) -> f64 {
    let sr_factor = if engine_sample_rate == 0 {
        1.0
    } else {
        f64::from(source_sample_rate) / f64::from(engine_sample_rate)
    };
    let pitch_factor = 2f64.powf(f64::from(pitch_semitones) / 12.0);
    match stretch_mode {
        StretchMode::Raw => sr_factor,
        StretchMode::Repitch => sr_factor * pitch_factor,
        // Phase 1 fallback: Stretch / Slice は Raw 同等。 Phase 3+ で
        // granular / phase vocoder / chunk + crossfade に置き換え。
        StretchMode::Stretch | StretchMode::Slice => sr_factor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- fade_envelope ----

    #[test]
    fn fade_envelope_zero_len_passes_through() {
        assert_eq!(fade_envelope(0, 0, FadeCurve::Linear), 1.0);
        assert_eq!(fade_envelope(100, 0, FadeCurve::Exponential), 1.0);
    }

    #[test]
    fn fade_envelope_t_at_or_past_len_is_unity() {
        assert_eq!(fade_envelope(100, 100, FadeCurve::Linear), 1.0);
        assert_eq!(fade_envelope(101, 100, FadeCurve::SCurve), 1.0);
    }

    #[test]
    fn fade_envelope_linear_midpoint_is_half() {
        let v = fade_envelope(50, 100, FadeCurve::Linear);
        assert!((v - 0.5).abs() < 1e-6, "got {v}");
    }

    #[test]
    fn fade_envelope_exp_is_squared_linear() {
        let lin = fade_envelope(50, 100, FadeCurve::Linear);
        let exp = fade_envelope(50, 100, FadeCurve::Exponential);
        assert!((exp - lin * lin).abs() < 1e-6);
    }

    #[test]
    fn fade_envelope_scurve_midpoint_is_half() {
        // 0.5 - 0.5 * cos(π/2) = 0.5
        let v = fade_envelope(50, 100, FadeCurve::SCurve);
        assert!((v - 0.5).abs() < 1e-6, "got {v}");
    }

    #[test]
    fn fade_envelope_scurve_endpoints_zero_and_one() {
        // SCurve at t=0 → 0、 t=1 (full) → 1.0 (early-out 経由)。
        assert!(fade_envelope(0, 100, FadeCurve::SCurve).abs() < 1e-6);
        assert!((fade_envelope(99, 100, FadeCurve::SCurve) - 1.0).abs() < 0.01);
    }

    // ---- pitch_ratio_for ----

    #[test]
    fn pitch_ratio_raw_matches_sr_factor() {
        let r = pitch_ratio_for(StretchMode::Raw, 44_100, 48_000, 12.0);
        let expected = 44100.0 / 48000.0;
        assert!((r - expected).abs() < 1e-9);
    }

    #[test]
    fn pitch_ratio_repitch_applies_pitch_octave() {
        // +12 semitones (1 octave up) → frame stride 2x。
        let r = pitch_ratio_for(StretchMode::Repitch, 48_000, 48_000, 12.0);
        assert!((r - 2.0).abs() < 1e-6);
    }

    #[test]
    fn pitch_ratio_repitch_applies_pitch_negative_octave() {
        let r = pitch_ratio_for(StretchMode::Repitch, 48_000, 48_000, -12.0);
        assert!((r - 0.5).abs() < 1e-6);
    }

    #[test]
    fn pitch_ratio_repitch_combines_sr_and_pitch() {
        // 24kHz source @ engine 48kHz + 12 semitones → 0.5 * 2 = 1.0
        let r = pitch_ratio_for(StretchMode::Repitch, 24_000, 48_000, 12.0);
        assert!((r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn pitch_ratio_stretch_slice_fallback_to_raw() {
        let raw = pitch_ratio_for(StretchMode::Raw, 48_000, 48_000, 5.0);
        let stretch = pitch_ratio_for(StretchMode::Stretch, 48_000, 48_000, 5.0);
        let slice = pitch_ratio_for(StretchMode::Slice, 48_000, 48_000, 5.0);
        assert!((raw - stretch).abs() < 1e-9);
        assert!((raw - slice).abs() < 1e-9);
    }

    #[test]
    fn pitch_ratio_engine_sr_zero_is_safe() {
        // Defensive: engine_sr=0 で divide-by-zero しない (= sr_factor=1.0)。
        let r = pitch_ratio_for(StretchMode::Raw, 48_000, 0, 0.0);
        assert!((r - 1.0).abs() < 1e-9);
    }
}
