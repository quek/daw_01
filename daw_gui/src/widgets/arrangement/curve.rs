//! r.md #73: automation の曲線と画面の間の変換を 1 か所に集める。
//!
//! **曲線の形の実装は `common::automation::apply_curve` ただ 1 本**で、
//! ここはその評価結果を screen 座標へ写すだけ。以前は draw.rs (screen y) /
//! geometry.rs (handle 位置) / common (再生) の 3 か所に式が散っていて、
//! 「評価と描画は一致しているのにハンドルの向きだけ逆」という不具合を生んでいた。
//!
//! 評価は **plain (再生値) 空間**で行う。`apply_curve` は a / b に対して affine
//! 同変なので、表示窓の内側に収まる affine な target では norm 空間評価と一致する。
//! 一致しないのは 3 つ: `GroupTransform::ScaleX/ScaleY` (log)、`TrackBuiltin::Mute`
//! (階段)、そして **端点が表示窓の外に出る恒等 target** (`GroupTransform::X` 等 —
//! `plain_to_norm_ranged` 末尾の `clamp(0,1)` で飽和する)。そこでは plain 評価だけが
//! 「鳴る形が窓のどこにいるか」を描く。

use super::*;
use common::automation::{
    apply_curve, norm_mapping_is_affine, norm_mapping_is_invertible, norm_to_plain_ranged,
    plain_to_norm_ranged,
};
use common::model::AutomationCurve;

/// 1 レーンぶんの「値 ↔ 画面 y」写像。clip 描画域 (縦 padding 適用済) を含む。
#[derive(Clone, Copy)]
pub(super) struct LaneValueMap<'a> {
    pub target: &'a common::model::AutomationTarget,
    pub plugin_range: Option<(f64, f64)>,
    /// clip 描画域の上端 y と高さ (= lane body から縦 padding を引いたもの)。
    pub clip_y: f32,
    pub clip_h: f32,
}

impl<'a> LaneValueMap<'a> {
    /// `lane.target` / `lane.plugin_range` と clip 描画域から作る。
    #[must_use]
    pub(super) fn from_lane(
        lane: &'a ArrangementAutomationLane,
        clip_rect: Rect,
    ) -> LaneValueMap<'a> {
        LaneValueMap {
            target: &lane.target,
            plugin_range: lane.plugin_range,
            clip_y: clip_rect.y,
            clip_h: clip_rect.h,
        }
    }

    /// norm (0..=1) → screen y。 y は下向きなので `1 - norm` で反転する。
    #[must_use]
    pub(super) fn norm_to_y(self, norm: f32) -> f32 {
        self.clip_y + (1.0 - norm.clamp(0.0, 1.0)) * self.clip_h
    }

    /// plain → screen y (`norm_to_y(to_norm(plain))`)。点 dot / 曲線の共通経路。
    #[must_use]
    pub(super) fn plain_to_y(self, plain: f64) -> f32 {
        self.norm_to_y(self.to_norm(plain))
    }

    #[must_use]
    pub(super) fn to_plain(self, norm: f32) -> f64 {
        norm_to_plain_ranged(self.target, norm, self.plugin_range)
    }

    #[must_use]
    pub(super) fn to_norm(self, plain: f64) -> f32 {
        plain_to_norm_ranged(self.target, plain, self.plugin_range)
    }

    /// この lane で「線を掴んで曲げる」直接操作が成立するか
    /// (= `norm_mapping_is_invertible`)。Mute lane だけが false。
    #[must_use]
    pub(super) fn is_bendable(self) -> bool {
        norm_mapping_is_invertible(self.target)
    }
}

/// この plain 値が **表示窓の内側**にいるか (= `clamp(0,1)` で潰れていないか)。
///
/// `plain_to_norm_ranged` は末尾で `clamp(0.0, 1.0)` する。窓の外の値は端に飽和し、
/// `norm_to_plain_ranged` で戻らない。norm が端にいるときだけ round-trip を確かめる
/// 形にして、f32 量子化を誤検出しないようにする (端でない値は定義上飽和していない)。
#[must_use]
fn plain_is_inside_window(map: LaneValueMap<'_>, plain: f64) -> bool {
    let n = map.to_norm(plain);
    if n > 0.0 && n < 1.0 {
        return true;
    }
    (map.to_plain(n) - plain).abs() <= 1e-6 * (1.0 + plain.abs())
}

/// この区間が **画面上でも 1 次**か (= 2 点の polyline で厳密に描けるか)。
/// `norm_mapping_is_affine` に加えて **端点が両方とも表示窓の内側**であることを要求する。
/// `Linear` の値域は端点の間に収まる (区間は凸) ので、端点が窓の中なら区間全体が中。
#[must_use]
pub(super) fn segment_is_straight_on_screen(
    map: LaneValueMap<'_>,
    a_plain: f64,
    b_plain: f64,
) -> bool {
    norm_mapping_is_affine(map.target)
        && plain_is_inside_window(map, a_plain)
        && plain_is_inside_window(map, b_plain)
}

/// 区間の任意進捗 `u` における **norm 値**。曲線の唯一の評価入口。
/// 描画 / hit-test / 逆算がすべてこれを通る。
#[must_use]
pub(super) fn eval_norm(
    map: LaneValueMap<'_>,
    a_plain: f64,
    b_plain: f64,
    u: f64,
    curve: AutomationCurve,
) -> f32 {
    map.to_norm(apply_curve(a_plain, b_plain, u, curve))
}

/// 区間 1 本のサンプル数。`max(16, ceil(|dx| / max_segment_px))` を 512 で cap。
/// 16 は短い区間でも形が視認できる最小段数 (旧 Exponential 分岐と同じ既定)。
#[must_use]
fn sample_count(dx_px: f32, max_segment_px: f32) -> usize {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = (dx_px.abs() / max_segment_px.max(1e-3)).ceil().max(16.0) as usize;
    n.min(512)
}

/// 1 区間 (前 point → 次 point) を polyline に flatten して `out` へ push。
/// caller は始点を 1 度 push 済の前提、終点 (= 次 point) を含めて push する。
///
/// - `Hold` は階段なので 2 点 (`(x1, y0)` → `(x1, y1)`) で厳密。
/// - `Linear` かつ `segment_is_straight_on_screen` なら 1 点 (= 終点) で厳密。
/// - それ以外は uniform sampling (`sample_count`)。旧実装の adaptive de Casteljau
///   は「y が制御点の 1 次式」を前提にしていて、非 affine / 飽和する写像に持ち込めない
///   ので廃止した。
pub(super) fn flatten_segment(
    map: LaneValueMap<'_>,
    prev: (f32, f64),
    next: (f32, f64),
    curve: AutomationCurve,
    max_segment_px: f32,
    out: &mut Vec<(f32, f32)>,
) {
    let (x0, a_plain) = prev;
    let (x1, b_plain) = next;
    match curve {
        AutomationCurve::Hold => {
            // 階段: 前値を x1 まで水平に保ち、 x1 で垂直に立ち上がる。
            out.push((x1, map.plain_to_y(a_plain)));
            out.push((x1, map.plain_to_y(b_plain)));
        }
        AutomationCurve::Linear if segment_is_straight_on_screen(map, a_plain, b_plain) => {
            out.push((x1, map.plain_to_y(b_plain)));
        }
        _ => {
            let n = sample_count(x1 - x0, max_segment_px);
            for i in 1..=n {
                #[allow(clippy::cast_precision_loss)]
                let u = i as f64 / n as f64;
                #[allow(clippy::cast_possible_truncation)]
                let x = x0 + (x1 - x0) * (u as f32);
                out.push((x, map.norm_to_y(eval_norm(map, a_plain, b_plain, u, curve))));
            }
        }
    }
}

/// clip 1 本ぶんの curve を flatten して screen 座標の点列で返す
/// (旧 `draw::flatten_lane_curve` の置き換え)。
/// `beat_to_px` は **screen-wide な拍 → px 換算** (= `body_w / view.len_beats`)。
/// clip 長 ≠ view 長のとき point dot 描画とずれないための SSoT
/// (#028 user 指摘 2 「curve が point を通らない」の根本原因だった)。
#[must_use]
pub(super) fn flatten_clip_curve(
    clip: &ArrangementAutomationClip,
    map: LaneValueMap<'_>,
    view_start_beat: f64,
    body_origin_x: f32,
    beat_to_px: f64,
    max_segment_px: f32,
) -> Vec<(f32, f32)> {
    if clip.points.is_empty() {
        return Vec::new();
    }
    let to_x = |p: &ArrangementAutomationPoint| -> f32 {
        // clip-local time → arrangement absolute beat → screen x (body_origin_x ベース)
        let abs_beat = clip.start_beat + p.time_beat;
        #[allow(clippy::cast_possible_truncation)]
        let x = body_origin_x + ((abs_beat - view_start_beat) * beat_to_px) as f32;
        x
    };
    let n = clip.points.len();
    let mut out = Vec::with_capacity(n * 8);
    out.push((to_x(&clip.points[0]), map.plain_to_y(clip.points[0].value_plain)));
    for i in 0..(n - 1) {
        let p_prev = &clip.points[i];
        let p_next = &clip.points[i + 1];
        // 各 segment の curve は **次 point** の `curve` を使う (= incoming curve)。
        flatten_segment(
            map,
            (to_x(p_prev), p_prev.value_plain),
            (to_x(p_next), p_next.value_plain),
            p_next.curve,
            max_segment_px,
            &mut out,
        );
    }
    out
}

/// r.md #73: 「掴んだ場所が指に付いてくる」逆算。
///
/// `target_norm` は目標の値 (norm)、`grab_u` は掴んだ位置の区間内進捗。
/// `start_curve` が `Hold` / `Linear` のときは呼び出し側で
/// `Exponential { bend: 0.0 }` に変換してから渡すこと (session の `start_curve`)。
///
/// 解けないとき (`|D| < 1e-6` の S 字固定点、`a ≈ b`) は `None` を返す
/// = caller は直前の preview を維持する。
///
/// **到達不能な目標は clamp された端の値になる** (= 線が指から離れる)。
/// `bend` / `tension` の値域 `-1.0..=1.0` は `AutomationCurve` 自身の宣言で、
/// `Exponential` で `grab_u` から到達できる `w` は `[grab_u², √grab_u]` だけ。
/// 区間の端に近いほど狭い (u0=0.9 なら区間高さの約 14%)。数学的性質であって
/// バグではない — 「その曲線はそれ以上曲がれない」という真実の表示。
///
/// **符号が自動的に正しくなる理由**: `w` は `(b - a)` で割っている。上り区間 (b>a) で
/// カーソルを上げると `w > u0` → `k < 1` → `bend < 0`。下り区間 (b<a) で同じく上げると
/// `w < u0` → `k > 1` → `bend > 0`。同じ画面ジェスチャが区間の向きに応じて逆符号の
/// progress 値を生む = 業界標準の挙動そのもの (定数 `dir` を掛ける小細工は要らない)。
#[must_use]
pub(super) fn solve_bend(
    map: LaneValueMap<'_>,
    a_plain: f64,
    b_plain: f64,
    grab_u: f64,
    start_curve: AutomationCurve,
    target_norm: f32,
) -> Option<AutomationCurve> {
    let span = b_plain - a_plain;
    if span.abs() <= 1e-12 {
        return None; // 水平区間は曲げられない (w が定義できない)
    }
    // `ln(u0) = 0` を避けるため端を外す。
    let u = grab_u.clamp(1e-4, 1.0 - 1e-4);
    let w = ((map.to_plain(target_norm) - a_plain) / span).clamp(1e-6, 1.0 - 1e-6);
    match start_curve {
        AutomationCurve::Exponential { .. } => {
            let k = w.ln() / u.ln();
            if !k.is_finite() || k <= 0.0 {
                return None;
            }
            #[allow(clippy::cast_possible_truncation)]
            let bend = (k.log2().clamp(-1.0, 1.0)) as f32;
            Some(AutomationCurve::Exponential { bend })
        }
        AutomationCurve::Bezier { .. } => {
            // 正規化形状関数は g(t) = t + τ·D (τ ≥ 0) / t + 2τ·D (τ < 0)、
            // D = t(1-t)(2t-1)。 `eval_bezier` から導出済 (制御点の対角線からの
            // ずれが τ≥0 で ∓τ/3、 τ<0 で ±2|τ|/3)。
            let d = u * (1.0 - u) * (2.0 * u - 1.0);
            if d.abs() < 1e-6 {
                // S 字は u=0.5 を必ず通る (数学的な固定点)。 そこを掴んでも動かせない。
                return None;
            }
            let delta = w - u;
            let t0 = delta / d;
            let tension = if t0 >= 0.0 { t0 } else { delta / (2.0 * d) };
            if !tension.is_finite() {
                return None;
            }
            #[allow(clippy::cast_possible_truncation)]
            let tension = tension.clamp(-1.0, 1.0) as f32;
            Some(AutomationCurve::Bezier { tension })
        }
        // caller が `start_curve` を `Exponential { bend: 0.0 }` へ変換済なので到達しない。
        AutomationCurve::Hold | AutomationCurve::Linear => None,
    }
}
