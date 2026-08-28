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

// ============================================================
// 区間の当たり判定 (画面 → 区間)
// ============================================================

/// r.md #73: `automation_segment_at` の戻り値。 bend session の anchor をそのまま作れる形。
#[derive(Clone, Copy, Debug)]
pub(super) struct AutomationSegmentHit {
    /// 入射側 point (= curve 属性を持つ後ろの点)。
    pub point: AutomationPointIdKey,
    /// 掴んだ位置の区間内進捗。
    pub grab_u: f64,
    /// 区間の始点値 / 終点値 (plain)。 overlay の再描画にもそのまま使う。
    pub a_plain: f64,
    pub b_plain: f64,
    /// 現在の curve。
    pub curve: AutomationCurve,
    /// clip 描画域 (縦 padding 適用済)。 norm ↔ y の anchor。
    pub clip_rect: Rect,
}

/// r.md #73: lane body 内の cursor から、曲げられる区間の当たりを返す。
///
/// 判定は「cursor x から区間内進捗 `u` を出し、`eval_norm` で曲線の y を評価して
/// `|cy - y| <= style.automation_curve_segment_hit_px`」。曲線の形の評価は
/// `common::automation::apply_curve` 1 本を通る (SSoT) ので、この関数は
/// `geometry.rs` ではなくここ (= 曲線 ↔ 画面の変換を集める場所) に居る。
///
/// **`automation_point_at` が `None` のときだけ呼ぶこと** — 点の当たり判定 (半径 2 倍)
/// が区間より先に効く (Alt+クリックの点削除と共存させるため)。
///
/// 引数の並びは `geometry::automation_point_at` と同じ (`.., lanes, cx, cy, style`)。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub(super) fn automation_segment_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    view: ArrangementView,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes: Rect,
    cx: f32,
    cy: f32,
    style: &ArrangementStyle,
) -> Option<AutomationSegmentHit> {
    if !lanes.contains(cx, cy) {
        return None;
    }
    let mut best: Option<(f32, AutomationSegmentHit)> = None;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes.x,
        lanes.w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            if cy < body_rect.y || cy >= body_rect.y + body_rect.h {
                return;
            }
            let track_id = visible_tracks[t_idx].id;
            let Some(cand) = segment_hit_in_lane(track_id, lane, body_rect, view, style, cx, cy)
            else {
                return;
            };
            if best.as_ref().is_none_or(|(d, _)| cand.0 < *d) {
                best = Some(cand);
            }
        },
    );
    best.map(|(_, hit)| hit)
}

/// 1 lane の中で cursor に最も近い区間と、その距離 (px) を返す。
///
/// 除外するもの:
/// - `norm_mapping_is_invertible` が false の lane (= Mute。逆算に連続解が無い)
/// - 端点の screen y の差が 1px 未満の区間 (= 水平区間。数学的に曲げられない。
///   端点が両方とも窓の外で同じ端に飽和している区間もここで落ちる)
/// - 幅 0 の区間
/// - 入射側 point の `id == 0` (未採番 sentinel。安定 id で指せない)
///
/// `beat_to_px` / `clip_y` / `clip_h` は `geometry::automation_point_at` と **同じ式**
/// (point dot と curve の x がずれる既知バグ #028 user 指摘 2 の再発防止)。
#[must_use]
fn segment_hit_in_lane(
    track_id: u32,
    lane: &ArrangementAutomationLane,
    body_rect: Rect,
    view: ArrangementView,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> Option<(f32, AutomationSegmentHit)> {
    let pad = style.automation_clip_v_pad_px;
    let clip_rect = Rect {
        x: body_rect.x,
        y: body_rect.y + pad,
        w: body_rect.w,
        h: (body_rect.h - pad * 2.0).max(2.0),
    };
    let map = LaneValueMap::from_lane(lane, clip_rect);
    if !map.is_bendable() {
        return None; // Mute lane は逆写像を持たないので曲げられない
    }
    let hit_px = style.automation_curve_segment_hit_px.max(1.0);
    let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
    let to_x = |abs_beat: f64| -> f32 {
        #[allow(clippy::cast_possible_truncation)]
        let x = body_rect.x + ((abs_beat - view.start_beat) * beat_to_px) as f32;
        x
    };
    let mut best: Option<(f32, AutomationSegmentHit)> = None;
    for clip_in in &lane.clips {
        for i in 1..clip_in.points.len() {
            let (p_prev, p_next) = (&clip_in.points[i - 1], &clip_in.points[i]);
            let x_prev = to_x(clip_in.start_beat + p_prev.time_beat);
            let x_next = to_x(clip_in.start_beat + p_next.time_beat);
            let bendable = p_next.id != 0
                && x_next - x_prev > 1e-3
                && cx > x_prev
                && cx < x_next
                && (map.plain_to_y(p_next.value_plain) - map.plain_to_y(p_prev.value_plain)).abs()
                    >= 1.0;
            if !bendable {
                continue;
            }
            let u = f64::from(cx - x_prev) / f64::from(x_next - x_prev);
            let y = map.norm_to_y(eval_norm(
                map,
                p_prev.value_plain,
                p_next.value_plain,
                u,
                p_next.curve,
            ));
            let d = (cy - y).abs();
            if d > hit_px || best.as_ref().is_some_and(|(bd, _)| *bd <= d) {
                continue;
            }
            let clip_key =
                AutomationClipKey { track: track_id, lane: lane.id, clip: clip_in.id };
            let point = AutomationPointIdKey { clip: clip_key, point_id: p_next.id };
            let a_plain = p_prev.value_plain;
            let b_plain = p_next.value_plain;
            let curve = p_next.curve;
            best = Some((d, AutomationSegmentHit {
                point,
                grab_u: u,
                a_plain,
                b_plain,
                curve,
                clip_rect,
            }));
        }
    }
    best
}

/// r.md #73: 安定 id で区間を引く (`geometry::find_automation_point_data` の id 版)。
/// 戻り値は `(前の点, この点)` — 曲線は **入射区間**の属性なので、片方だけでは
/// 形を描けない。 前の点が無い (= clip の先頭) なら `None`。
#[must_use]
pub(super) fn find_automation_segment_by_id(
    visible_tracks: &[ArrangementTrack],
    key: AutomationPointIdKey,
) -> Option<(&ArrangementAutomationPoint, &ArrangementAutomationPoint)> {
    let (_lane, clip) = find_lane_clip(visible_tracks, key.clip)?;
    let idx = clip.points.iter().position(|p| p.id == key.point_id && p.id != 0)?;
    if idx == 0 {
        return None;
    }
    Some((&clip.points[idx - 1], &clip.points[idx]))
}

// ============================================================
// bend の drag session
// ============================================================

/// r.md #73: レーン本体の線 (= 2 点の間の区間) を Alt+ドラッグして曲げる session。
///
/// 掴んだ場所が指に付いてくるよう、感度定数ではなく **逆算** で curve を決める
/// ([`solve_bend`])。逆算は区間の符号付き高さ `(b - a)` で割るので、
/// 上り区間 / 下り区間で自動的に正しい符号の progress 値になる
/// (保存する値は progress 基準なので、同じ画面ジェスチャが区間の向きで逆符号になる)。
/// commit は release で 1 回だけ (undo 1 段)。
///
/// 他の drag session と違ってこの型だけ `mod.rs` ではなくここに居る —
/// hit ([`AutomationSegmentHit`]) → anchor → 逆算がひと続きの subsystem で、
/// 全フィールドが曲線ドメインの値だから。
#[derive(Clone, Copy, Debug)]
pub(super) struct AutomationSegmentBendSession {
    /// 曲げる区間の **入射側** point (= `curve` 属性を持つ後ろの点)。
    pub point: AutomationPointIdKey,
    /// press 時に掴んだ位置の区間内進捗 `u ∈ (0, 1)`。drag 中不変
    /// (横スクロール / ズームしても掴んだ場所が動かない)。
    pub grab_u: f64,
    /// 区間の始点値 / 終点値 (plain)。drag 中 model 不変なので anchor 固定。
    pub a_plain: f64,
    pub b_plain: f64,
    /// press 時の curve (= release の no-op 判定に使う anchor)。
    pub anchor_curve: AutomationCurve,
    /// 逆算の基準になる curve。`anchor_curve` が Hold / Linear なら
    /// `Exponential { bend: 0.0 }` (= 直線へ自動変換)、それ以外は `anchor_curve`。
    pub start_curve: AutomationCurve,
    /// press 時点の `apply_curve(a, b, grab_u, start_curve)` を norm に写した値。
    /// 指の移動 px をここに足して目標値を作る (= 変換直後の線から相対で追従)。
    /// **Hold 区間ではここで 1 度だけ線が飛ぶ** — `k ∈ [0.5, 2]` のどの
    /// `Exponential` も `u<1` で値 `a` を通らないので連続解が存在しない。
    pub anchor_value_norm: f32,
    /// press 時点の clip 描画域 (norm ↔ y の anchor。view scroll 耐性)。
    pub clip_rect_anchor: Rect,
    pub anchor_mouse_y: f32,
    /// 直近の cursor y (release frame は anchor と異なるときだけ更新 — 既存 pattern)。
    pub last_mouse_y: f32,
    /// drag 中の live curve (overlay 描画 + release commit の SSoT)。
    /// `anchor_curve` と同値なら release で no-op。
    pub preview_curve: AutomationCurve,
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

// ============================================================
// 書き込み側 (`AppEvent::SetAutomationCurve` を出す 2 経路)
//
// release.rs ではなくここに居るのは、hit → session → 逆算 → commit が
// ひと続きの subsystem だから。書き込み側だけを 1,000 行の
// `commit_releases` の隣に置くと、機能が 2 ファイルに割れて読めなくなる。
// ============================================================

/// r.md #73: 区間 bend の release commit (`SetAutomationCurve` 1 件 = undo 1 段)。
///
/// preview が anchor と同値なら no-op (= Alt+クリックしただけで動かしていない)。
/// point は **安定 id** で指す — 曲線編集は press → release を跨ぐので positional index
/// では追加 / 削除でずれる (アーキテクチャ不変条件 1)。 undo は snapshot 方式なので
/// `prev` は載せない。
pub(super) fn commit_segment_bend(
    ui: &mut Ui<'_, AppData>,
    released: Option<AutomationSegmentBendSession>,
) {
    let Some(bd) = released else { return };
    if bd.preview_curve == bd.anchor_curve || bd.point.point_id == 0 {
        return;
    }
    emit_set_curve(ui, bd.point, bd.preview_curve);
}

/// r.md #73: 線の上 (`automation_curve_segment_hit_px` 以内) の Alt+ダブルクリックで
/// その区間を直線に戻す。 **消費したら `true`** を返す (= caller は次の分岐へ落とさない)。
///
/// 既に `Linear` の区間でも `true` を返す — 「線を狙った」ことは確かなので、
/// そこに点を足す経路へ落とすと「戻すつもりが点が増えた」になる。
pub(super) fn reset_segment_to_linear(
    ui: &mut Ui<'_, AppData>,
    f: &ArrangementFrame<'_>,
    cx: f32,
    cy: f32,
) -> bool {
    let hit = automation_segment_at(
        &f.visible_tracks, &f.tops, f.view.track_row_h, f.view,
        f.header_pane.x, f.header_pane.w, f.lanes, cx, cy, f.style,
    );
    let Some(seg) = hit.filter(|s| s.point.point_id != 0) else { return false };
    if seg.curve != AutomationCurve::Linear {
        emit_set_curve(ui, seg.point, AutomationCurve::Linear);
    }
    true
}

/// 上の 2 経路が共有する 1 本の発行口 (event が 1 種類しかないことを形で示す)。
fn emit_set_curve(ui: &mut Ui<'_, AppData>, key: AutomationPointIdKey, next: AutomationCurve) {
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::SetAutomationCurve {
            track_id: key.clip.track,
            lane_id: key.clip.lane,
            clip_id: key.clip.clip,
            point_id: key.point_id,
            next,
        });
    }));
}
