//! Curve evaluation helpers for automation lanes.
//!
//! Pure functions over `AutomationContent` / `AutomationLane`. No I/O,
//! no allocation, no tracing — safe to call from the audio thread.
//! See `docs/plan_automation.md` §3 / §8.
//!
//! Two main entry points:
//!
//! - [`evaluate_clip`] — evaluate an `AutomationContent` at a clip-local
//!   beat position, returning the interpolated plain-units value.
//! - [`lane_value_at`] — full lookup: walk a lane's clips and return
//!   the curve value at a song-level beat, falling back to
//!   `lane.default_value` when no clip covers the beat or the lane is
//!   disabled.

use crate::model::{
    AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationTarget,
    ClipContent, ContentId, Song, TrackBuiltinParam,
};
use std::collections::HashMap;

// ============================================================================
// plain ↔ normalized (0..=1) 変換
// ============================================================================
//
// widget (gui_01) は parameter を 0..=1 の `value_norm` で受け取り、daw_01
// 内部 (`AutomationLane.default_value` / `AutomationPoint.value`) は
// target の plain 単位で持つ。両側で同じ変換を使うため、SSoT として
// ここに集める。

/// Plain (target's native unit) → normalized 0..=1。
/// `PluginParam` の正規化は plugin の `min_value` / `max_value` を使う
/// のが正確だが、 Phase 1 では IPC で param info を送る経路がまだない
/// ので `clamp(0,1)` の placeholder。Phase 2 で
/// `AppData.plugin_params` lookup に置換する。
pub fn plain_to_norm(target: &AutomationTarget, plain: f64) -> f32 {
    let v = match target {
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => plain / 2.0,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => (plain + 1.0) / 2.0,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute) => {
            if plain >= 0.5 {
                1.0
            } else {
                0.0
            }
        }
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { .. }) => plain / 2.0,
        AutomationTarget::PluginParam { .. } => plain,
        AutomationTarget::SongTempo | AutomationTarget::SongTimeSigNumerator => 0.0,
    };
    v.clamp(0.0, 1.0) as f32
}

/// Normalized 0..=1 → plain (target's native unit)。`plain_to_norm` の
/// 逆変換。`Mute` は 0.5 を閾値に 0.0 / 1.0 へ snap。
pub fn norm_to_plain(target: &AutomationTarget, norm: f32) -> f64 {
    let n = norm.clamp(0.0, 1.0) as f64;
    match target {
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => n * 2.0,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => n * 2.0 - 1.0,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute) => {
            if n >= 0.5 {
                1.0
            } else {
                0.0
            }
        }
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { .. }) => n * 2.0,
        AutomationTarget::PluginParam { .. } => n,
        AutomationTarget::SongTempo | AutomationTarget::SongTimeSigNumerator => n,
    }
}

/// Evaluate the curve value at `t` (clip-local beats) inside an
/// `AutomationContent`. Returns the plain-units value (matching the
/// `AutomationPoint::value` convention).
///
/// Behaviour at the edges:
/// - empty content → `0.0` (caller should fall back to
///   `lane.default_value`)
/// - `t` before first point → first point's value (constant clamp)
/// - `t` after last point → last point's value (constant clamp;
///   the lane's default_value takes over only outside the clip itself,
///   not inside the clip after the last point)
///
/// The `curve` field of an `AutomationPoint` describes the *incoming*
/// segment (from the previous point to this one), so the segment
/// `[points[i-1].time_beat, points[i].time_beat]` uses
/// `points[i].curve`.
#[inline]
pub fn evaluate_clip(content: &AutomationContent, t: f64) -> f64 {
    let pts = &content.points;
    if pts.is_empty() {
        return 0.0;
    }
    if pts.len() == 1 || t <= pts[0].time_beat {
        return pts[0].value;
    }
    if t >= pts[pts.len() - 1].time_beat {
        return pts[pts.len() - 1].value;
    }
    // Binary search for the segment containing `t`.
    // Invariant maintained by `time_beat` sort on insert.
    let i = pts.partition_point(|p| p.time_beat <= t);
    // i is the index of the first point with time_beat > t, so the
    // segment is `pts[i-1] -> pts[i]`. Both i-1 and i are in range
    // because the early-return above ruled out the endpoint cases.
    debug_assert!(i >= 1 && i < pts.len());
    let prev = &pts[i - 1];
    let next = &pts[i];
    let span = next.time_beat - prev.time_beat;
    if span <= 0.0 {
        return next.value;
    }
    let u = ((t - prev.time_beat) / span).clamp(0.0, 1.0);
    apply_curve(prev.value, next.value, u, next.curve)
}

/// Apply `curve` to interpolate from `a` to `b` with parameter
/// `u ∈ [0, 1]`. The curve attribute lives on the *incoming* point so
/// the caller passes `next.curve`.
///
/// `Bezier { tension }` 数式 (SSoT、 gui_01 widget もこれをミラー):
///
/// 2D cubic Bezier。 制御点 4 つの x 座標は固定 (`c1x = 1/3`, `c2x = 2/3`)、
/// y 座標を `tension` で対角線から hold 方向へ離す。
///
/// ```text
///   diag1 = a + (b - a) * (1/3)
///   diag2 = a + (b - a) * (2/3)
///   tension >= 0 (S 字):  c1y = lerp(diag1, a, tension), c2y = lerp(diag2, b, tension)
///   tension <  0 (反転): c1y = lerp(diag1, b, |tension|), c2y = lerp(diag2, a, |tension|)
/// ```
///
/// `tension = 0.0` で制御点 4 つが対角線上に乗り、 Bezier は直線 (=
/// `Linear` と一致)。 `tension = +1.0` で制御点 y が end y で hold される
/// 滑らかな S 字。 `tension = -1.0` で制御点 y が反対 end で hold される
/// inverse S 字 (overshoot 系)。
///
/// 制御点 x が (1/3, 2/3) 固定なので Bernstein 基底で打ち消し合って
/// `x(t) = t` に縮退、 時間軸 `u` から Bezier parameter `t` は `t = u`
/// で即決定 (Newton iter 不要)。 RT 安全 (O(1) operations、 heap alloc
/// / I/O なし)。 詳細は `eval_bezier` の docstring。
#[inline]
pub fn apply_curve(a: f64, b: f64, u: f64, curve: AutomationCurve) -> f64 {
    let u = u.clamp(0.0, 1.0);
    match curve {
        AutomationCurve::Hold => a, // step jump happens at u = 1.0 (handled by next segment)
        AutomationCurve::Linear => a + (b - a) * u,
        AutomationCurve::Bezier { tension } => eval_bezier(a, b, u, f64::from(tension)),
        AutomationCurve::Exponential { bend } => {
            // value = a + (b - a) * u^k, where k = 2^bend.
            //   bend == 0 → k=1 (linear)
            //   bend == +1 → k=2 (quadratic ease-in)
            //   bend == -1 → k=0.5 (sqrt ease-out)
            let k = 2f64.powf(f64::from(bend));
            a + (b - a) * u.powf(k)
        }
    }
}

/// Bezier curve 評価 (`apply_curve` 内で SSoT)。
///
/// 制御点 x = (1/3, 2/3) 固定なので、 Bernstein 基底で打ち消し合って
/// **`x(t) = t` に縮退**する (gui_01 #033 reply で指摘された数学的観察):
///
/// ```text
///   x(t) = (1-t)^3 * 0 + 3(1-t)^2*t * (1/3) + 3(1-t)*t^2 * (2/3) + t^3 * 1
///        = (1-t)^2 * t + 2(1-t) * t^2 + t^3
///        = t * ((1-t) + t)^2
///        = t
/// ```
///
/// よって時間軸 `u` から Bezier parameter `t` への逆算は不要 (`t = u`)。
/// y のみ 4 制御点 Bernstein 形で評価。 RT 安全 (O(1) operations、 ヒープ
/// alloc / I/O なし)。
#[inline]
fn eval_bezier(a: f64, b: f64, u: f64, tension: f64) -> f64 {
    // tension で制御点 y を対角線 (linear) と end-hold の間で blend。
    let diag1 = a + (b - a) * (1.0 / 3.0);
    let diag2 = a + (b - a) * (2.0 / 3.0);
    let mix = tension.abs().min(1.0);
    let (target1, target2) = if tension >= 0.0 { (a, b) } else { (b, a) };
    let c1y = diag1 * (1.0 - mix) + target1 * mix;
    let c2y = diag2 * (1.0 - mix) + target2 * mix;
    // x(t) = t に縮退するので t = u をそのまま使う (Newton iter 不要)。
    let t = u;
    let omt = 1.0 - t;
    omt.powi(3) * a + 3.0 * omt.powi(2) * t * c1y + 3.0 * omt * t.powi(2) * c2y + t.powi(3) * b
}

/// Resolve `lane` at `song_beat` (song timeline). Walks the lane's
/// clips, finds the one (if any) covering `song_beat`, looks up its
/// `AutomationContent` from the song-level `clip_contents` store, and
/// evaluates the curve. Returns `lane.default_value` when:
/// - `lane.enabled == false`, or
/// - no clip covers the beat (gaps / before first / after last), or
/// - the clip's `content_id` resolves to a non-`Automation` variant.
pub fn lane_value_at(
    lane: &AutomationLane,
    clip_contents: &HashMap<ContentId, ClipContent>,
    song_beat: f64,
) -> f64 {
    if !lane.enabled {
        return lane.default_value;
    }
    let Some(clip) = clip_covering(&lane.clips, song_beat) else {
        return lane.default_value;
    };
    let Some(content) = clip_contents.get(&clip.content_id) else {
        return lane.default_value;
    };
    let ClipContent::Automation(auto) = content else {
        return lane.default_value;
    };
    if auto.points.is_empty() {
        return lane.default_value;
    }
    let local = song_beat - clip.start_beat;
    evaluate_clip(auto, local)
}

/// Convenience over `lane_value_at` that takes a `Song` directly.
#[inline]
pub fn song_lane_value_at(song: &Song, lane: &AutomationLane, song_beat: f64) -> f64 {
    lane_value_at(lane, &song.clip_contents, song_beat)
}

/// Phase 4 Step D (`docs/plan_automation.md` §6): recording 中の point 列に
/// 新しい `(time_beat, plain_value, Linear)` を online で挿入し、 直前の
/// point が collinear (= prev_prev → new_point の直線上にある) なら削除する
/// helper。 RDP 風の単純化を 1 ステップずつ実行する形で、 「knob を一定
/// 方向に滑らかに動かす」 と dense な 1/64 beat 刻みの点列が「始点 + 終点
/// + 変曲点」 だけに収束する。
///
/// 戻り値: `(insert_at, removed_prev)`
/// - `insert_at`: new_point を挿入した位置 (`points[insert_at] == new`)
/// - `removed_prev`: collinear で prev が削除されたかどうか
///
/// 不変条件: 呼び出し前後で `points` は `time_beat` 昇順を保つ。 新規 point
/// は `partition_point(|p| p.time_beat <= time_beat)` 位置に挿入される (=
/// 同時刻 point が複数ある場合は **末尾** に追加)。
///
/// `epsilon` は plain 単位 (= target の native スケール) の許容誤差。 例:
/// Volume 範囲 0..=2 なら 0.005 で 0.25% 程度、 Pan 範囲 -1..=1 でも同程度。
///
/// `points.len() < 2` (= 始点 or 1 点のみ) のときは collinear 判定する prev が
/// 取れないので thinning なしで insert のみ。 `dt_full` が 0 (= prev_prev と
/// new_point が同時刻) のときも安全側で thinning skip (= divide by 0 回避)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinInsertResult {
    pub insert_at: usize,
    pub removed_prev: bool,
}

pub fn thin_collinear_and_insert(
    points: &mut Vec<crate::model::AutomationPoint>,
    time_beat: f64,
    plain_value: f64,
    epsilon: f64,
) -> ThinInsertResult {
    let mut insert_at = points.partition_point(|p| p.time_beat <= time_beat);
    let mut removed_prev = false;
    if insert_at >= 2 {
        let prev = &points[insert_at - 1];
        let prev_prev = &points[insert_at - 2];
        let dt_full = time_beat - prev_prev.time_beat;
        if dt_full > f64::EPSILON {
            let alpha = (prev.time_beat - prev_prev.time_beat) / dt_full;
            let interp_y = prev_prev.value + (plain_value - prev_prev.value) * alpha;
            if (prev.value - interp_y).abs() < epsilon {
                points.remove(insert_at - 1);
                insert_at -= 1;
                removed_prev = true;
            }
        }
    }
    let new_point = crate::model::AutomationPoint {
        time_beat,
        value: plain_value,
        curve: crate::model::AutomationCurve::Linear,
    };
    points.insert(insert_at, new_point);
    ThinInsertResult { insert_at, removed_prev }
}

/// Phase 5 Step 5.2 (`docs/plan_automation.md` §10): evaluate the song-level
/// tempo at the given `beat` position。 SongTempo lane が存在し enabled なら
/// その curve eval 結果を return、 無ければ `song.bpm` を constant として
/// return。 audio thread が buffer 頭で呼び、 結果を `current_bpm` として
/// 当該 buffer の sample-to-beat 変換 / set_pd_transport / automation ramp
/// 等に渡す。 RT 安全 (= heap alloc 無し、 lane_value_at の浮動小数演算のみ)。
///
/// `beat` は累積 beat-domain playhead (= engine が integrate 維持)。
pub fn evaluate_song_tempo(song: &Song, beat: f64) -> f32 {
    for lane in &song.song_lanes {
        if !lane.enabled {
            continue;
        }
        if matches!(lane.target, AutomationTarget::SongTempo) {
            let v = lane_value_at(lane, &song.clip_contents, beat);
            // SongTempo の plain value は BPM (= song.bpm と同単位)。 sanity
            // clamp: 1 BPM 未満は不正 (divide by zero リスク)、 上限 1000 BPM
            // で防御 (= 通常 user は 20..=300 程度)。
            return (v as f32).clamp(1.0, 1000.0);
        }
    }
    song.bpm
}

/// Find the `AutomationClip` whose half-open range
/// `[start_beat, start_beat + length_beats)` contains `song_beat`.
/// Returns the *first* match — overlapping clips on the same lane are
/// not expected in practice but are tolerated.
fn clip_covering(clips: &[AutomationClip], song_beat: f64) -> Option<&AutomationClip> {
    clips.iter().find(|c| {
        c.length_beats > 0.0
            && song_beat >= c.start_beat
            && song_beat < c.start_beat + c.length_beats
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
        AutomationTarget, TrackBuiltinParam,
    };

    fn pt(t: f64, v: f64, curve: AutomationCurve) -> AutomationPoint {
        AutomationPoint {
            time_beat: t,
            value: v,
            curve,
        }
    }

    #[test]
    fn empty_content_returns_zero() {
        let c = AutomationContent::default();
        assert_eq!(evaluate_clip(&c, 0.0), 0.0);
        assert_eq!(evaluate_clip(&c, 5.0), 0.0);
    }

    #[test]
    fn single_point_returns_constant() {
        let c = AutomationContent {
            points: vec![pt(2.0, 0.42, AutomationCurve::Linear)],
        };
        assert_eq!(evaluate_clip(&c, 0.0), 0.42);
        assert_eq!(evaluate_clip(&c, 2.0), 0.42);
        assert_eq!(evaluate_clip(&c, 100.0), 0.42);
    }

    #[test]
    fn linear_interpolates_midpoint() {
        let c = AutomationContent {
            points: vec![
                pt(0.0, 0.0, AutomationCurve::Linear),
                pt(4.0, 1.0, AutomationCurve::Linear),
            ],
        };
        assert!((evaluate_clip(&c, 2.0) - 0.5).abs() < 1e-9);
        assert!((evaluate_clip(&c, 1.0) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn before_first_clamps_to_first_value() {
        let c = AutomationContent {
            points: vec![
                pt(2.0, 0.5, AutomationCurve::Linear),
                pt(4.0, 1.0, AutomationCurve::Linear),
            ],
        };
        assert!((evaluate_clip(&c, 0.0) - 0.5).abs() < 1e-9);
        assert!((evaluate_clip(&c, 1.99) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn after_last_clamps_to_last_value() {
        let c = AutomationContent {
            points: vec![
                pt(0.0, 0.0, AutomationCurve::Linear),
                pt(4.0, 1.0, AutomationCurve::Linear),
            ],
        };
        assert!((evaluate_clip(&c, 4.0) - 1.0).abs() < 1e-9);
        assert!((evaluate_clip(&c, 100.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn hold_does_not_interpolate_within_segment() {
        // Hold says: previous value holds until *this* point, then
        // jumps. Inside the segment we should still see the previous
        // value (a), and the next segment starts from b at t = next.
        let c = AutomationContent {
            points: vec![
                pt(0.0, 0.2, AutomationCurve::Linear),
                pt(4.0, 0.8, AutomationCurve::Hold),
            ],
        };
        assert!((evaluate_clip(&c, 1.0) - 0.2).abs() < 1e-9);
        assert!((evaluate_clip(&c, 3.99) - 0.2).abs() < 1e-9);
        // At and past t == 4.0 we clamp to the last point's value.
        assert!((evaluate_clip(&c, 4.0) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn bezier_tension_zero_is_exactly_linear() {
        // tension=0 で制御点 4 つが対角線上に乗り、 Bezier は直線になる
        // (SSoT: `apply_curve` Bezier コメント参照)。 数 sample 取って
        // linear と一致を確認。
        let c = AutomationContent {
            points: vec![
                pt(0.0, 0.0, AutomationCurve::Linear),
                pt(4.0, 1.0, AutomationCurve::Bezier { tension: 0.0 }),
            ],
        };
        for k in 0..=8 {
            let t = (k as f64) / 8.0 * 4.0;
            let expected = t / 4.0;
            let got = evaluate_clip(&c, t);
            assert!(
                (got - expected).abs() < 1e-6,
                "t={t} expected linear {expected}, got {got}"
            );
        }
    }

    #[test]
    fn bezier_endpoints_exact_for_all_tensions() {
        for tension in [-1.0, -0.5, 0.0, 0.5, 1.0] {
            let c = AutomationContent {
                points: vec![
                    pt(0.0, 0.0, AutomationCurve::Linear),
                    pt(4.0, 1.0, AutomationCurve::Bezier { tension }),
                ],
            };
            assert!(
                (evaluate_clip(&c, 0.0) - 0.0).abs() < 1e-9,
                "tension={tension} start"
            );
            assert!(
                (evaluate_clip(&c, 4.0) - 1.0).abs() < 1e-9,
                "tension={tension} end"
            );
        }
    }

    #[test]
    fn bezier_tension_positive_makes_s_curve() {
        // tension=+1.0 で制御点 y が end y で hold → S 字。 中点 (u=0.5) は
        // 対称 ((y=a+b)/2 = 0.5)、 quarter 点 (u=0.25) は linear (0.25) より
        // **小さい** (= 前半が緩い、 S 字の凹み)、 three-quarter 点 (u=0.75)
        // は linear (0.75) より **大きい**。
        let c = AutomationContent {
            points: vec![
                pt(0.0, 0.0, AutomationCurve::Linear),
                pt(4.0, 1.0, AutomationCurve::Bezier { tension: 1.0 }),
            ],
        };
        let mid = evaluate_clip(&c, 2.0);
        assert!((mid - 0.5).abs() < 1e-6, "midpoint expected 0.5, got {mid}");
        let q = evaluate_clip(&c, 1.0);
        assert!(
            q < 0.25 - 1e-3,
            "quarter expected < 0.25 (concave), got {q}"
        );
        let tq = evaluate_clip(&c, 3.0);
        assert!(
            tq > 0.75 + 1e-3,
            "three-quarter expected > 0.75 (convex), got {tq}"
        );
    }

    #[test]
    fn bezier_tension_negative_inverts_s_curve() {
        // tension=-1.0 で制御点 y が反対 end で hold → inverse S 字。
        // 中点で対称、 quarter 点は linear より **大きい**、 3/4 点は
        // linear より **小さい**。
        let c = AutomationContent {
            points: vec![
                pt(0.0, 0.0, AutomationCurve::Linear),
                pt(4.0, 1.0, AutomationCurve::Bezier { tension: -1.0 }),
            ],
        };
        let mid = evaluate_clip(&c, 2.0);
        assert!((mid - 0.5).abs() < 1e-6, "midpoint expected 0.5, got {mid}");
        let q = evaluate_clip(&c, 1.0);
        assert!(q > 0.25 + 1e-3, "quarter expected > 0.25, got {q}");
        let tq = evaluate_clip(&c, 3.0);
        assert!(tq < 0.75 - 1e-3, "three-quarter expected < 0.75, got {tq}");
    }

    #[test]
    fn exponential_bend_zero_is_linear() {
        let c = AutomationContent {
            points: vec![
                pt(0.0, 0.0, AutomationCurve::Linear),
                pt(4.0, 1.0, AutomationCurve::Exponential { bend: 0.0 }),
            ],
        };
        assert!((evaluate_clip(&c, 1.0) - 0.25).abs() < 1e-9);
        assert!((evaluate_clip(&c, 2.0) - 0.5).abs() < 1e-9);
        assert!((evaluate_clip(&c, 3.0) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn exponential_bend_one_is_quadratic_ease_in() {
        let c = AutomationContent {
            points: vec![
                pt(0.0, 0.0, AutomationCurve::Linear),
                pt(4.0, 1.0, AutomationCurve::Exponential { bend: 1.0 }),
            ],
        };
        // u = 0.5 → 0.5^2 = 0.25
        assert!((evaluate_clip(&c, 2.0) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn lane_value_falls_back_to_default_when_disabled() {
        let lane = AutomationLane {
            id: 1,
            enabled: false,
            default_value: 0.7,
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.7,
            )
        };
        let store: HashMap<ContentId, ClipContent> = HashMap::new();
        assert!((lane_value_at(&lane, &store, 1.0) - 0.7).abs() < 1e-9);
    }

    #[test]
    fn lane_value_falls_back_to_default_in_clip_gaps() {
        let lane = AutomationLane {
            id: 1,
            default_value: 0.3,
            clips: vec![],
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.3,
            )
        };
        let store: HashMap<ContentId, ClipContent> = HashMap::new();
        assert!((lane_value_at(&lane, &store, 5.0) - 0.3).abs() < 1e-9);
    }

    #[test]
    fn lane_value_evaluates_clip_when_inside_range() {
        let cid: ContentId = 42;
        let mut store: HashMap<ContentId, ClipContent> = HashMap::new();
        store.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    pt(0.0, 0.0, AutomationCurve::Linear),
                    pt(4.0, 1.0, AutomationCurve::Linear),
                ],
            }),
        );
        let lane = AutomationLane {
            id: 1,
            default_value: 0.0,
            clips: vec![AutomationClip {
                id: 1,
                name: "auto1".into(),
                start_beat: 8.0,
                length_beats: 4.0,
                content_id: cid,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.0,
            )
        };
        // At song_beat = 10.0 (= clip-local 2.0) → 0.5.
        assert!((lane_value_at(&lane, &store, 10.0) - 0.5).abs() < 1e-9);
        // Outside the clip range falls back to default.
        assert!((lane_value_at(&lane, &store, 7.99) - 0.0).abs() < 1e-9);
        assert!((lane_value_at(&lane, &store, 12.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn wrong_variant_falls_back_to_default() {
        // Defensive: if a content_id resolves to a Midi/Audio variant,
        // we treat the lane as if no clip covered the beat.
        let cid: ContentId = 7;
        let mut store: HashMap<ContentId, ClipContent> = HashMap::new();
        store.insert(cid, ClipContent::Midi(crate::model::MidiContent::default()));
        let lane = AutomationLane {
            id: 1,
            default_value: 0.42,
            clips: vec![AutomationClip {
                id: 1,
                name: "stale".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
            }],
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.42,
            )
        };
        assert!((lane_value_at(&lane, &store, 1.0) - 0.42).abs() < 1e-9);
    }

    // ---- Phase 4 Step D: thin_collinear_and_insert tests ----

    /// ε は plain 単位 0.005 (Volume / Pan の Live 規定 0.5% に相当)。
    const TEST_EPS: f64 = 0.005;

    /// 直線的に valueを上げる drag を sim: 各 tick で linear に増える値を
    /// 連続 insert すると、 thinning が中間点を削除して 最初と最後だけ残す。
    #[test]
    fn thin_linear_drag_collapses_to_endpoints() {
        let mut pts: Vec<AutomationPoint> = vec![pt(0.0, 0.0, AutomationCurve::Linear)];
        // beat 0.0 (= start), 0.1, 0.2, ..., 1.0 で y = beat を insert
        for i in 1..=10 {
            let t = i as f64 / 10.0;
            thin_collinear_and_insert(&mut pts, t, t, TEST_EPS);
        }
        // 始点 (0,0) と直近 insert された点 (1.0, 1.0) の 2 点だけ残るはず。
        // 中間は collinear なので毎回 prev が削除される。
        assert_eq!(pts.len(), 2, "expected 2 points after linear thinning, got {pts:?}");
        assert!((pts[0].time_beat - 0.0).abs() < 1e-9);
        assert!((pts[0].value - 0.0).abs() < 1e-9);
        assert!((pts[1].time_beat - 1.0).abs() < 1e-9);
        assert!((pts[1].value - 1.0).abs() < 1e-9);
    }

    /// 折り返し drag (= V 字形): 上昇 → 下降 すると変曲点 (peak) が残る。
    /// 直線部分は thin されるが、 折り返しの瞬間は collinear でないので残る。
    #[test]
    fn thin_v_shape_drag_keeps_inflection_point() {
        let mut pts: Vec<AutomationPoint> = vec![pt(0.0, 0.0, AutomationCurve::Linear)];
        // 上昇: t=0.1..0.5, y=t (直線で 0→0.5)
        for i in 1..=5 {
            let t = i as f64 / 10.0;
            thin_collinear_and_insert(&mut pts, t, t, TEST_EPS);
        }
        // 下降: t=0.6..1.0, y = 1.0 - t (peak 0.5 から 0→0.0)
        for i in 6..=10 {
            let t = i as f64 / 10.0;
            let y = 1.0 - t;
            thin_collinear_and_insert(&mut pts, t, y, TEST_EPS);
        }
        // 期待: (0,0) + (0.5, 0.5) peak + (1.0, 0.0) の 3 点に収束する想定。
        // (0,0) と (0.5, 0.5) の間は collinear で thin、 (0.5, 0.5) は折り返し
        // 直後の (0.6, 0.4) が collinear 検査されると prev_prev=(0,0) と new=
        // (0.6, 0.4) を結ぶ線上に (0.5, 0.5) は乗らない → 残る。
        assert!(
            pts.len() >= 3,
            "expected at least start + peak + end, got {pts:?}"
        );
        // peak が含まれていること (y >= 0.49 の点が 1 つ以上)
        assert!(
            pts.iter().any(|p| p.value > 0.49),
            "peak point ~0.5 not found in {pts:?}"
        );
        // 末尾は (1.0, 0.0)
        let last = pts.last().unwrap();
        assert!((last.time_beat - 1.0).abs() < 1e-9);
        assert!(last.value.abs() < 1e-9);
    }

    /// points.len() < 2 (= 始点のみ) のとき thinning skip、 普通に insert される。
    #[test]
    fn thin_skips_when_fewer_than_two_points() {
        let mut pts: Vec<AutomationPoint> = vec![];
        let r = thin_collinear_and_insert(&mut pts, 0.0, 0.5, TEST_EPS);
        assert_eq!(pts.len(), 1);
        assert!(!r.removed_prev);

        let r2 = thin_collinear_and_insert(&mut pts, 0.5, 0.7, TEST_EPS);
        assert_eq!(pts.len(), 2);
        assert!(!r2.removed_prev);
    }

    /// 同値連続 insert (= knob を動かさず drag): 直線 y=const なので、 中間
    /// 点は collinear で削除される。 結果 2 点。
    #[test]
    fn thin_constant_value_collapses_to_two_points() {
        let mut pts: Vec<AutomationPoint> = vec![];
        for i in 0..=10 {
            let t = i as f64 / 10.0;
            thin_collinear_and_insert(&mut pts, t, 0.7, TEST_EPS);
        }
        assert_eq!(pts.len(), 2, "expected 2 endpoints after constant thinning, got {pts:?}");
        assert!((pts[0].value - 0.7).abs() < 1e-9);
        assert!((pts[1].value - 0.7).abs() < 1e-9);
    }

    /// ε 境界: prev の y が補間値 ± ε 外 (= ε より大きく逸脱) なら thin
    /// されない。 ε ぎりぎり内なら thin される。
    #[test]
    fn thin_epsilon_boundary() {
        // ε = 0.005、 prev=(0.5, 0.502) ← interp は 0.5、 逸脱は 0.002 < ε → thin
        let mut pts_thin: Vec<AutomationPoint> = vec![
            pt(0.0, 0.0, AutomationCurve::Linear),
            pt(0.5, 0.502, AutomationCurve::Linear),
        ];
        thin_collinear_and_insert(&mut pts_thin, 1.0, 1.0, TEST_EPS);
        assert_eq!(pts_thin.len(), 2, "should have removed middle point");

        // ε = 0.005、 prev=(0.5, 0.51) ← interp は 0.5、 逸脱は 0.01 > ε → keep
        let mut pts_keep: Vec<AutomationPoint> = vec![
            pt(0.0, 0.0, AutomationCurve::Linear),
            pt(0.5, 0.51, AutomationCurve::Linear),
        ];
        thin_collinear_and_insert(&mut pts_keep, 1.0, 1.0, TEST_EPS);
        assert_eq!(pts_keep.len(), 3, "should NOT have removed middle point");
    }

    /// dt_full == 0 (= prev_prev と new_point が同時刻) のときは divide
    /// by 0 を避けて thinning skip。 sort 順は保たれ、 単に末尾に append。
    #[test]
    fn thin_skips_when_dt_zero() {
        let mut pts: Vec<AutomationPoint> = vec![
            pt(0.5, 0.0, AutomationCurve::Linear),
            pt(0.5, 0.3, AutomationCurve::Linear),
        ];
        let r = thin_collinear_and_insert(&mut pts, 0.5, 0.5, TEST_EPS);
        assert_eq!(pts.len(), 3);
        assert!(!r.removed_prev);
        // partition_point の <= 比較で末尾に挿入される
        assert_eq!(r.insert_at, 2);
    }
}
