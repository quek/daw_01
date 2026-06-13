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
    ClipContent, ContentId, ModRouting, Polarity, Song, TrackBuiltinParam,
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
        // Image PiP field: x/y/w/h/opacity は既に 0..=1 (恒等)、 Rotation
        // のみ Pan と同 idiom で `(plain + π) / (2π)` mapping。
        AutomationTarget::ImageBuiltin(crate::model::ImageBuiltinParam::Rotation) => {
            (plain + std::f64::consts::PI) / (2.0 * std::f64::consts::PI)
        }
        AutomationTarget::ImageBuiltin(_) => plain,
        // Text Builtin: image と同じく Rotation のみ Pan idiom、 残りは
        // plain と norm が同単位 (= color/x/y/w/h は 0..=1、 font_size /
        // outline_width / shadow_offset / shadow_blur は px だが
        // automation lane の値域は plain 直接、 UI 側で範囲を整える)。
        AutomationTarget::TextBuiltin(crate::model::TextBuiltinParam::Rotation) => {
            (plain + std::f64::consts::PI) / (2.0 * std::f64::consts::PI)
        }
        AutomationTarget::TextBuiltin(_) => plain,
        // Group transform (§4.4): 位置/アンカー (X/Y/AnchorX/AnchorY) と Opacity は
        // 0..=1 恒等、 Rotation は Pan idiom、 ScaleX/ScaleY は 0.1..=10 の log space。
        // ScaleX/Y は round-trip が norm_to_plain と厳密逆 (0.1·100^n)。
        AutomationTarget::GroupTransform(crate::model::GroupTransformParam::Rotation) => {
            (plain + std::f64::consts::PI) / (2.0 * std::f64::consts::PI)
        }
        AutomationTarget::GroupTransform(
            crate::model::GroupTransformParam::ScaleX | crate::model::GroupTransformParam::ScaleY,
        ) => (plain.clamp(0.1, 10.0) / 0.1).ln() / 100.0_f64.ln(),
        AutomationTarget::GroupTransform(_) => plain,
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
        // Image PiP field: x/y/w/h/opacity は normalize と plain が同単位、
        // Rotation のみ `n * 2π - π` で -π..=π に展開。
        AutomationTarget::ImageBuiltin(crate::model::ImageBuiltinParam::Rotation) => {
            n * 2.0 * std::f64::consts::PI - std::f64::consts::PI
        }
        AutomationTarget::ImageBuiltin(_) => n,
        AutomationTarget::TextBuiltin(crate::model::TextBuiltinParam::Rotation) => {
            n * 2.0 * std::f64::consts::PI - std::f64::consts::PI
        }
        AutomationTarget::TextBuiltin(_) => n,
        // Group transform (§4.4): plain_to_norm の厳密逆。X/Y/AnchorX/AnchorY/
        // Opacity は恒等、 Rotation は `n·2π - π`、 ScaleX/ScaleY は `0.1·100^n`。
        AutomationTarget::GroupTransform(crate::model::GroupTransformParam::Rotation) => {
            n * 2.0 * std::f64::consts::PI - std::f64::consts::PI
        }
        AutomationTarget::GroupTransform(
            crate::model::GroupTransformParam::ScaleX | crate::model::GroupTransformParam::ScaleY,
        ) => 0.1 * 100.0_f64.powf(n),
        AutomationTarget::GroupTransform(_) => n,
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

/// `docs/plan_modulation_routing_redesign.md` §3: the summed **normalized**
/// (`0..=1` domain) modulation offset applied to `target` by every `ModRouting`
/// in `routings` whose `target` matches. **lane 非依存** — `routings` is a
/// `Track.mod_routings` / `Song.song_mod_routings` slice, not tied to any lane.
///
/// ```text
/// offset = Σ routings(target match): s = clamp(scalar(source_id), 0..1)
///          Unipolar => depth*s,  Bipolar => depth*(2s - 1)
/// ```
///
/// Composition happens in the normalized domain so a given `depth` means the
/// same fraction of range across heterogeneous targets (volume `0..2`, rotation
/// `-π..π`, image x `0..1`). `scalar` resolves a `ModRouting::source_id` to its
/// latest follower value (live `AudioBridge::mod_scalars` in preview, baked
/// sidecar in export). Returns `0.0` when no routing matches. Pure / RT-safe.
pub fn modulation_offset_norm(
    target: &AutomationTarget,
    routings: &[ModRouting],
    scalar: impl Fn(u32) -> f32,
) -> f32 {
    let mut sum = 0.0f32;
    for r in routings {
        if &r.target != target {
            continue;
        }
        let s = scalar(r.source_id).clamp(0.0, 1.0);
        sum += match r.polarity {
            Polarity::Unipolar => r.depth * s,
            Polarity::Bipolar => r.depth * (2.0 * s - 1.0),
        };
    }
    sum
}

/// `docs/plan_modulation_routing_redesign.md` §3.1: the **effective plain-units
/// value** of a non-plugin `target` = `base` (the model value or automation
/// curve value) with `modulation_offset_norm` added in the normalized domain:
///
/// ```text
/// norm_eff  = clamp(plain_to_norm(target, base) + offset, 0..1)
/// plain_eff = norm_to_plain(target, norm_eff)
/// ```
///
/// With no matching routing this returns `base` unchanged (no normalize
/// round-trip), so unmodulated params are bit-for-bit unaffected. Used for
/// track-builtin / image / text / group / song targets where the daw owns the
/// final value. Plugin params instead send `modulation_offset_norm` to the
/// plugin host (CLAP `param_mod`) — see the plan §3.2. Pure / RT-safe.
pub fn apply_modulation(
    target: &AutomationTarget,
    base: f64,
    routings: &[ModRouting],
    scalar: impl Fn(u32) -> f32,
) -> f64 {
    let offset = modulation_offset_norm(target, routings, scalar);
    if offset == 0.0 && !routings.iter().any(|r| &r.target == target) {
        return base;
    }
    let norm_eff = (plain_to_norm(target, base) + offset).clamp(0.0, 1.0);
    norm_to_plain(target, norm_eff)
}

/// Resolve a `ModRouting::source_id` to its latest follower scalar from
/// `Song::mod_sources` (slot = position in the Vec) against a polled
/// `mod_scalars` plane (e.g. `AppData::mod_scalars`). Dangling / out-of-range
/// sources read as `0`.
#[inline]
pub fn source_scalar(song: &Song, mod_scalars: &[f32], source_id: u32) -> f32 {
    song.mod_sources
        .iter()
        .position(|m| m.id == source_id)
        .and_then(|i| mod_scalars.get(i))
        .copied()
        .unwrap_or(0.0)
}

/// [`apply_modulation`] resolving follower scalars from `Song::mod_sources`.
pub fn apply_modulation_with_scalars(
    song: &Song,
    target: &AutomationTarget,
    base: f64,
    routings: &[ModRouting],
    mod_scalars: &[f32],
) -> f64 {
    apply_modulation(target, base, routings, |source_id| {
        source_scalar(song, mod_scalars, source_id)
    })
}

/// [`modulation_offset_norm`] resolving follower scalars from `Song::mod_sources`.
pub fn modulation_offset_norm_with_scalars(
    song: &Song,
    target: &AutomationTarget,
    routings: &[ModRouting],
    mod_scalars: &[f32],
) -> f32 {
    modulation_offset_norm(target, routings, |source_id| {
        source_scalar(song, mod_scalars, source_id)
    })
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
            let v = lane_value_at(lane, &song.clip_contents, beat) as f32;
            // SongTempo の plain value は BPM (= song.bpm と同単位)。 sanity
            // clamp: 1 BPM 未満は不正 (divide by zero リスク)、 上限 1000 BPM
            // で防御 (= 通常 user は 20..=300 程度)。 NaN/Inf は clamp を
            // 素通りするので finite チェックを先に行い、 song.bpm へ fallback。
            if !v.is_finite() {
                return song.bpm;
            }
            return v.clamp(1.0, 1000.0);
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
    fn group_transform_scale_norm_is_exact_inverse() {
        use crate::model::GroupTransformParam as G;
        // ScaleX/ScaleY は 0.1..=10 の log space。plain→norm→plain が元値へ
        // 戻らないと automation point が drift する (recon risk)。norm は pipeline
        // 上 f32 で持つので相対許容で判定 (ln(100) 倍の増幅を考慮)。
        for param in [G::ScaleX, G::ScaleY] {
            let target = AutomationTarget::GroupTransform(param);
            for plain in [0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 10.0] {
                let norm = plain_to_norm(&target, plain);
                let back = norm_to_plain(&target, norm);
                assert!(
                    (back / plain - 1.0).abs() < 1e-5,
                    "scale round-trip drift: {plain} -> {norm} -> {back}"
                );
            }
            // 等倍 1.0 は log space の中点 norm 0.5。
            assert!((f64::from(plain_to_norm(&target, 1.0)) - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn group_transform_identity_and_rotation_norm() {
        use crate::model::GroupTransformParam as G;
        // 位置 / アンカー / Opacity は 0..=1 恒等。
        for param in [G::X, G::Y, G::AnchorX, G::AnchorY, G::Opacity] {
            let target = AutomationTarget::GroupTransform(param);
            assert!((f64::from(plain_to_norm(&target, 0.3)) - 0.3).abs() < 1e-5);
            assert!((norm_to_plain(&target, 0.3) - 0.3).abs() < 1e-5);
        }
        // Rotation は Pan idiom: 0 rad → norm 0.5、round-trip 厳密。
        let rot = AutomationTarget::GroupTransform(G::Rotation);
        assert!((f64::from(plain_to_norm(&rot, 0.0)) - 0.5).abs() < 1e-6);
        let back = norm_to_plain(&rot, plain_to_norm(&rot, 1.234));
        assert!((back - 1.234).abs() < 1e-5);
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

    // ---- Phase 5 Step 5.2: evaluate_song_tempo tests ----

    /// Phase 5 Step 5.2: helper を build する Song fixture。 base bpm 120、
    /// SongTempo lane (curve 60..240) を持たせる。 single SongTempo lane
    /// (= Bitwig も typical に 1 lane) を assumption。
    fn song_with_tempo_curve(
        start_value: f64,
        end_value: f64,
        clip_length: f64,
    ) -> crate::model::Song {
        use crate::model::{
            AutomationClip, AutomationContent, AutomationLane, AutomationPoint,
            AutomationTarget, ClipContent, Song,
        };
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        time_beat: 0.0,
                        value: start_value,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        time_beat: clip_length,
                        value: end_value,
                        curve: AutomationCurve::Linear,
                    },
                ],
            }),
        );
        let lane_id = song.alloc_song_lane_id();
        let mut lane = AutomationLane::new(AutomationTarget::SongTempo, 120.0);
        lane.id = lane_id;
        lane.clips.push(AutomationClip {
            id: 1,
            name: "Tempo".into(),
            start_beat: 0.0,
            length_beats: clip_length,
            content_id: cid,
        });
        lane.next_clip_id = 2;
        song.song_lanes.push(lane);
        song
    }

    /// SongTempo lane が無い場合は `song.bpm` を返す (= constant fallback)。
    #[test]
    fn evaluate_song_tempo_falls_back_to_song_bpm_when_no_lane() {
        let song = crate::model::Song {
            bpm: 140.0,
            ..crate::model::Song::default()
        };
        assert_eq!(evaluate_song_tempo(&song, 0.0), 140.0);
        assert_eq!(evaluate_song_tempo(&song, 100.0), 140.0);
    }

    /// SongTempo lane が disabled の場合も song.bpm にフォールバック。
    #[test]
    fn evaluate_song_tempo_disabled_lane_falls_back() {
        let mut song = song_with_tempo_curve(80.0, 160.0, 4.0);
        song.song_lanes[0].enabled = false;
        // curve 評価で beat=2 なら 120.0 になるはずだが、 disabled で 120.0
        // (= song.bpm = base) に戻る。 たまたま同値なので分かりにくいが、
        // base を変えて確認。
        song.bpm = 90.0;
        assert!((evaluate_song_tempo(&song, 2.0) - 90.0).abs() < 1e-6);
    }

    /// SongTempo curve の midpoint 評価 (= linear で 60→180 なら beat 2 で 120)。
    #[test]
    fn evaluate_song_tempo_linear_midpoint() {
        let song = song_with_tempo_curve(60.0, 180.0, 4.0);
        let v = evaluate_song_tempo(&song, 2.0);
        assert!((v - 120.0).abs() < 0.01, "expected ~120 at beat 2, got {v}");
    }

    /// curve 範囲外 (= clip 外) は `lane.default_value` = 120.0 を返す。
    #[test]
    fn evaluate_song_tempo_out_of_clip_returns_default() {
        let song = song_with_tempo_curve(60.0, 180.0, 4.0);
        // beat = 10 は clip [0, 4) の外。 default_value (= 120.0) が返る。
        let v = evaluate_song_tempo(&song, 10.0);
        assert!((v - 120.0).abs() < 0.01, "expected default 120, got {v}");
    }

    /// 上限 clamp: SongTempo curve の値が異常に大きい場合 1000 BPM で clamp。
    #[test]
    fn evaluate_song_tempo_clamps_high() {
        let song = song_with_tempo_curve(5000.0, 5000.0, 4.0);
        let v = evaluate_song_tempo(&song, 2.0);
        assert!((v - 1000.0).abs() < 1e-6, "expected clamp to 1000, got {v}");
    }

    /// 下限 clamp: SongTempo curve の値が 1 BPM 未満なら 1 で clamp
    /// (= divide by zero リスク回避)。
    #[test]
    fn evaluate_song_tempo_clamps_low() {
        let song = song_with_tempo_curve(0.0, 0.0, 4.0);
        let v = evaluate_song_tempo(&song, 2.0);
        assert!((v - 1.0).abs() < 1e-6, "expected clamp to 1, got {v}");
    }

    /// Bezier curve でも tempo eval が curve 評価結果を返すことを確認
    /// (= curve type が変わっても evaluate_song_tempo が透過に lane_value_at
    /// を呼んでいる、 regression 防止)。
    #[test]
    fn evaluate_song_tempo_with_bezier_curve() {
        use crate::model::{
            AutomationClip, AutomationContent, AutomationLane, AutomationPoint,
            AutomationTarget, ClipContent, Song,
        };
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        time_beat: 0.0,
                        value: 60.0,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        time_beat: 4.0,
                        value: 180.0,
                        curve: AutomationCurve::Bezier { tension: 0.5 },
                    },
                ],
            }),
        );
        let lane_id = song.alloc_song_lane_id();
        let mut lane = AutomationLane::new(AutomationTarget::SongTempo, 120.0);
        lane.id = lane_id;
        lane.clips.push(AutomationClip {
            id: 1,
            name: "Tempo".into(),
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: cid,
        });
        lane.next_clip_id = 2;
        song.song_lanes.push(lane);

        // beat=0 は始点 (= 60)、 beat=2 は中央 (60..180 範囲内)、 beat=3 は
        // 終端近傍 (= clip 内で 180 に近い)。 clip 末端 beat=4 は半開区間
        // `[0, 4)` で clip 外扱いなので、 evaluate_song_tempo は
        // `lane.default_value = 120.0` を返す (= eval を確認するなら範囲内
        // beat を使う)。
        let v_start = evaluate_song_tempo(&song, 0.0);
        let v_mid = evaluate_song_tempo(&song, 2.0);
        let v_near_end = evaluate_song_tempo(&song, 3.9);
        assert!((v_start - 60.0).abs() < 0.1, "start = 60, got {v_start}");
        assert!(
            v_mid > 60.0 && v_mid < 180.0,
            "mid in (60, 180), got {v_mid}"
        );
        assert!(
            v_near_end > 150.0 && v_near_end < 180.0,
            "near_end in (150, 180), got {v_near_end}"
        );
    }

    #[test]
    fn apply_modulation_adds_unipolar() {
        use crate::model::{AutomationTarget, ModRouting, Polarity, TrackBuiltinParam};
        // docs/plan_modulation_routing_redesign.md §3: base (1.0) + a unipolar
        // routing keyed by target. No matching routing → base unchanged; scalar
        // 0 → base; scalar 1.0 raises the value.
        let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume);
        let base = 1.0_f64;
        // No routings → base regardless of scalars.
        assert_eq!(apply_modulation(&target, base, &[], |_| 0.5), base);
        let routings = vec![ModRouting {
            target: target.clone(),
            source_id: 5,
            depth: 0.5,
            polarity: Polarity::Unipolar,
        }];
        assert_eq!(
            apply_modulation(&target, base, &routings, |_| 0.0),
            base,
            "scalar 0 → base (no modulation)"
        );
        let v_full =
            apply_modulation(&target, base, &routings, |sid| if sid == 5 { 1.0 } else { 0.0 });
        assert!(
            v_full > base,
            "unipolar depth 0.5 at scalar 1.0 must raise above base ({base} vs {v_full})"
        );
        // A routing whose target doesn't match is ignored.
        let other = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan);
        assert_eq!(
            apply_modulation(&other, 0.0, &routings, |_| 1.0),
            0.0,
            "non-matching target → base unchanged"
        );
        // offset_norm exposes the raw normalized offset.
        let off = modulation_offset_norm(&target, &routings, |_| 1.0);
        assert!((off - 0.5).abs() < 1e-6, "unipolar depth 0.5 at s=1 → 0.5, got {off}");
    }
}
