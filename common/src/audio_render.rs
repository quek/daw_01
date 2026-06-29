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

/// clip 伸縮量 = source の native 再生長 (秒) /
/// event の配置長 (秒、 nominal bpm 基準) の比。 `1.0` で trim 相当 (= source を
/// そのまま native rate で再生)、 `< 1.0` で event slot の方が長い → source を
/// 引き伸ばす (slow)、 `> 1.0` で event slot が短い → 詰める (fast)。 engine SR に
/// 依らない (秒で比較するので source SR ≠ engine SR でも一意)。 退化入力
/// (SR/bpm/長さ 0) は `1.0` (= 伸縮なし) を返す defensive。 compile 時 (off-RT) に
/// 1 回だけ呼び、 render loop では結果を掛けるだけにする。
#[inline]
pub fn stretch_ratio_for(
    native_frames: u64,
    source_sample_rate: u32,
    event_length_beats: f64,
    bpm: f32,
) -> f64 {
    if source_sample_rate == 0 || bpm <= 0.0 || event_length_beats <= 0.0 || native_frames == 0 {
        return 1.0;
    }
    let native_secs = native_frames as f64 / f64::from(source_sample_rate);
    let event_secs = event_length_beats * 60.0 / f64::from(bpm);
    if event_secs <= 1e-9 {
        return 1.0;
    }
    native_secs / event_secs
}

/// source 進度 = clip の手動 `stretch_ratio` (= [`stretch_ratio_for`]、 nominal
/// bpm 基準の native長/配置長) × tempo 追従比 (`current_bpm / nominal_bpm`)。
/// この 2 つを掛けると、 clip は **拍数を固定したまま** tempo 変化に追従して
/// 伸縮する (= MIDI clip と同じ挙動: project tempo が変わると実時間長が変わる)。
///
/// 数式上、 `event_length_beats` が固定なら、 この戻り値は nominal_bpm の取り方に
/// **不変**:
/// `stretch_ratio * current/nominal = (native_secs * nominal / (elb*60)) *
/// current/nominal = native_secs * current / (elb*60)`。
/// よって schedule の再コンパイル (= nominal_bpm が現 song.bpm に更新され、
/// stretch_ratio も同時に再算出される) を跨いでも追従結果が一致する。
///
/// `current_bpm` は呼び出し側で、 Stretch (granular) は LP smoothed な値 (=
/// click 抑制、 grain source jump 抑制)、 Repitch / Slice は instant な値 (=
/// pitch / slice trigger の追随性優先) を渡す。 `nominal_bpm <= 0` は退化入力と
/// して `stretch_ratio` を素通し (= 追従なし) する defensive。 RT path で呼ばれる
/// ので alloc / panic free。
#[inline]
pub fn tempo_follow_ratio(stretch_ratio: f64, current_bpm: f64, nominal_bpm: f64) -> f64 {
    if nominal_bpm <= 0.0 {
        return stretch_ratio;
    }
    stretch_ratio * (current_bpm / nominal_bpm)
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

    // ---- stretch_ratio_for ----

    #[test]
    fn stretch_ratio_native_equals_event_is_unity() {
        // 1 秒の source (48k) を 120bpm で 2 拍 (= 1 秒) に置く → trim 相当、 比 1.0。
        let r = stretch_ratio_for(48_000, 48_000, 2.0, 120.0);
        assert!((r - 1.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn stretch_ratio_longer_event_slows_source() {
        // 1 秒の source を 2 秒分 (= 4 拍 @120bpm) の slot に伸ばす → source は
        // 半速で進む → 比 0.5。
        let r = stretch_ratio_for(48_000, 48_000, 4.0, 120.0);
        assert!((r - 0.5).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn stretch_ratio_shorter_event_speeds_source() {
        // 1 秒の source を 0.5 秒分 (= 1 拍 @120bpm) の slot に詰める → 倍速 → 比 2.0。
        let r = stretch_ratio_for(48_000, 48_000, 1.0, 120.0);
        assert!((r - 2.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn stretch_ratio_independent_of_engine_sr() {
        // source SR が違っても秒で比較するので比は同じ (24k source、 1 秒 = 24000
        // frames を 1 秒 slot へ)。
        let r = stretch_ratio_for(24_000, 24_000, 2.0, 120.0);
        assert!((r - 1.0).abs() < 1e-9, "got {r}");
    }

    #[test]
    fn stretch_ratio_degenerate_is_unity() {
        assert!((stretch_ratio_for(0, 48_000, 2.0, 120.0) - 1.0).abs() < 1e-9);
        assert!((stretch_ratio_for(48_000, 0, 2.0, 120.0) - 1.0).abs() < 1e-9);
        assert!((stretch_ratio_for(48_000, 48_000, 0.0, 120.0) - 1.0).abs() < 1e-9);
        assert!((stretch_ratio_for(48_000, 48_000, 2.0, 0.0) - 1.0).abs() < 1e-9);
    }

    // ---- tempo_follow_ratio ----

    #[test]
    fn tempo_follow_ratio_cases() {
        // (stretch_ratio, current_bpm, nominal_bpm, expected)
        let cases = [
            (1.0, 120.0, 120.0, 1.0),  // current == nominal → 追従なし、 native rate
            (1.0, 240.0, 120.0, 2.0),  // 倍テンポ → source 倍速で進む (= 同じ拍に収める)
            (1.0, 60.0, 120.0, 0.5),   // 半テンポ → source 半速
            (0.5, 240.0, 120.0, 1.0),  // 手動 stretch 0.5 × 追従 2.0 = 1.0 (乗算合成)
            (2.0, 90.0, 180.0, 1.0),   // 手動 2.0 × 追従 0.5
            (1.0, 140.0, 0.0, 1.0),    // nominal=0 は退化 → stretch_ratio 素通し
        ];
        for (stretch, current, nominal, expected) in cases {
            let got = tempo_follow_ratio(stretch, current, nominal);
            assert!(
                (got - expected).abs() < 1e-9,
                "stretch={stretch} current={current} nominal={nominal} got={got} want={expected}"
            );
        }
    }

    #[test]
    fn tempo_follow_spans_fixed_beats_across_tempo() {
        // MIDI 流の不変条件: import 時に event_length_beats = native長(拍) で置いた
        // clip は、 どの current_bpm でも source 全体がちょうど beat window に収まる
        // (= advance_ratio × window_secs == native_secs)。 これが「拍数を固定して
        // tempo に追従する」 の数学的定義。 nominal の取り方 (import時 vs 再コンパイル時)
        // に依らず成立することも検証する。
        let native_frames = 96_000u64; // 2.0 s @ 48k
        let sr = 48_000u32;
        let native_secs = native_frames as f64 / f64::from(sr);
        // (nominal_bpm = clip 取り込み時テンポ, current_bpm = 再生時テンポ)
        let cases = [
            (120.0f32, 120.0f64),
            (120.0, 140.0),
            (120.0, 90.0),
            (90.0, 174.0),
            (174.0, 100.0),
        ];
        for (nominal_bpm, current_bpm) in cases {
            // import path (app.rs frames_to_beats) と同じ: 配置拍 = native秒 × bpm/60。
            let event_length_beats = native_secs * f64::from(nominal_bpm) / 60.0;
            let manual = stretch_ratio_for(native_frames, sr, event_length_beats, nominal_bpm);
            // 取り込み直後は手動 stretch なし → 比 1.0。
            assert!((manual - 1.0).abs() < 1e-9, "manual={manual}");
            let advance = tempo_follow_ratio(manual, current_bpm, f64::from(nominal_bpm));
            let window_secs = event_length_beats * 60.0 / current_bpm;
            assert!(
                (advance * window_secs - native_secs).abs() < 1e-6,
                "nominal={nominal_bpm} current={current_bpm} advance={advance} \
                 window_secs={window_secs} native_secs={native_secs}"
            );
        }
    }
}
