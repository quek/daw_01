//! r.md #73: 曲線 (`curve.rs`) と区間 hit-test の単体テスト。
//!
//! `tests.rs` に足さず別ファイルにしてあるのは god file budget のため
//! (アーキテクチャ不変条件 9)。 「超えそうになったら切り出す」ではなく
//! **書き始める前に切り出した** — 到達してから分けると分割 commit と機能 commit が
//! 混ざってどちらの diff も読めなくなる。
//!
//! **符号 (`bend` / `tension` の正負) を assert しないこと。** 保存する値は progress 基準
//! なので、同じ画面ジェスチャでも区間の向きで符号が逆になる。 assert するのは
//! 「見えるもの」= 画面上の y (= norm 値) の向きだけ。

use super::*;
use common::model::{AutomationCurve, AutomationTarget, GroupTransformParam, TrackBuiltinParam};

// ============================================================
// 足場
// ============================================================

/// clip 描画域は x∈[0, 100)、 y∈[0, 100)。 y は下向きなので norm 1.0 → y=0。
const CLIP_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };

fn lane_with(target: AutomationTarget) -> ArrangementAutomationLane {
    ArrangementAutomationLane {
        id: 1,
        target,
        plugin_range: None,
        label: Arc::from("test"),
        icon_glyph: 'V',
        color: Color::rgb(1.0, 1.0, 1.0),
        enabled: true,
        visible: true,
        height_px: 100,
        default_value_norm: 0.5,
        clips: Vec::new(),
    }
}

/// affine かつ表示窓の内側に収まる lane (Volume: plain 0..=2 ↔ norm 0..=1)。
fn volume_lane() -> ArrangementAutomationLane {
    lane_with(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume))
}

/// 段の lane (逆写像を持たない = 曲げられない)。
fn mute_lane() -> ArrangementAutomationLane {
    lane_with(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute))
}

/// log 空間の lane (窓の内側でも非 affine)。
fn scale_x_lane() -> ArrangementAutomationLane {
    lane_with(AutomationTarget::GroupTransform(GroupTransformParam::ScaleX))
}

/// 恒等 + clamp 飽和の lane (端点が窓の外に出られる)。
fn group_x_lane() -> ArrangementAutomationLane {
    lane_with(AutomationTarget::GroupTransform(GroupTransformParam::X))
}

/// Volume の norm → plain (norm 0.2 = plain 0.4)。
fn vol_plain(norm: f64) -> f64 {
    norm * 2.0
}

/// hit-test 用の最小 view (snap OFF、 header/ruler 無し)。
fn hit_view() -> ArrangementView {
    ArrangementView {
        start_beat: 0.0,
        scroll_beat_raw: 0.0,
        len_beats: 4.0,
        track_top: 0.0,
        tracks_visible: 8.0,
        track_row_h: 20.0,
        header_w: 0.0,
        ruler_h: 0.0,
        playhead_beat: None,
        loop_range: None,
        data_generation: 0,
        bpm: 120.0,
        time_sig: (4, 4),
        snap: SnapConfig::OFF,
        arranger_lane_h: 0.0,
    }
}

/// lane 1 本を持つ最小 track。
fn track_with_lane(lane: ArrangementAutomationLane) -> ArrangementTrack {
    ArrangementTrack {
        id: 10,
        name: Arc::from("t"),
        muted: false,
        solo: false,
        armed: false,
        clips: Vec::new(),
        volume: 1.0,
        parent_id: None,
        depth: 0,
        automation_lanes_collapsed: false,
        automation_lanes: vec![lane],
        collapsed: false,
        row_h: None,
        kind: TrackKind::Audio,
        color: None,
    }
}

/// テーマから組んだ style (`tests.rs::test_style` と同じ流儀)。
fn test_style() -> ArrangementStyle {
    ArrangementStyle::from_theme(
        &crate::theme::Theme::builtin("dark").expect("組込みダークテーマは常に存在する"),
    )
}

/// 出力点列から (画面 x=cx) の y を線形補間で求める。 polyline 内挿。
/// (旧 `tests.rs::sample_polyline_y` をそのまま移設。)
fn sample_polyline_y(pts: &[(f32, f32)], cx: f32) -> f32 {
    assert!(pts.len() >= 2);
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (lo_x, hi_x) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
        if cx >= lo_x && cx <= hi_x {
            if (b.0 - a.0).abs() < 1e-6 {
                return a.1; // 垂直 segment は始点 y を返す (Hold の立ち上がりは対象外)
            }
            let t = (cx - a.0) / (b.0 - a.0);
            return a.1 + (b.1 - a.1) * t;
        }
    }
    if cx <= pts.first().unwrap().0 { pts.first().unwrap().1 } else { pts.last().unwrap().1 }
}

/// 1 区間を flatten して点列を返す (始点込み)。
fn flatten(
    lane: &ArrangementAutomationLane,
    a_plain: f64,
    b_plain: f64,
    curve: AutomationCurve,
) -> Vec<(f32, f32)> {
    let map = curve::LaneValueMap::from_lane(lane, CLIP_RECT);
    let mut out = vec![(0.0_f32, map.plain_to_y(a_plain))];
    curve::flatten_segment(map, (0.0, a_plain), (100.0, b_plain), curve, 2.0, &mut out);
    out
}

// ============================================================
// 移設分: flatten の性質 (**上り / 下りの両方**で確かめる)
// ============================================================

/// 端点を常に通る (= 描画ズレなし)。 旧 `flatten_segment_endpoints_exact_for_all_curve_kinds`。
#[test]
fn flatten_segment_endpoints_exact_for_all_curve_kinds() {
    let lane = volume_lane();
    let map = curve::LaneValueMap::from_lane(&lane, CLIP_RECT);
    for (a_norm, b_norm) in [(0.2_f64, 0.8_f64), (0.8, 0.2)] {
        let (a, b) = (vol_plain(a_norm), vol_plain(b_norm));
        for curve in [
            AutomationCurve::Hold,
            AutomationCurve::Linear,
            AutomationCurve::Bezier { tension: 0.0 },
            AutomationCurve::Bezier { tension: 0.5 },
            AutomationCurve::Bezier { tension: -0.5 },
            AutomationCurve::Exponential { bend: 0.0 },
            AutomationCurve::Exponential { bend: 0.8 },
            AutomationCurve::Exponential { bend: -0.8 },
        ] {
            let out = flatten(&lane, a, b, curve);
            let last = *out.last().expect("終点は必ず push される");
            let want_y = map.plain_to_y(b);
            assert!(
                (last.0 - 100.0).abs() < 1e-3 && (last.1 - want_y).abs() < 1e-3,
                "{curve:?} ({a_norm}→{b_norm}): 出力末尾 = 終点を期待 (got {last:?}, want y={want_y})"
            );
        }
    }
}

/// 量 0 は直線と一致する (`Bezier { tension: 0 }` / `Exponential { bend: 0 }`)。
/// 旧 `bezier_tension_zero_is_linear` / `exponential_bend_zero_is_linear` を
/// 上り / 下りの 2 ケースに拡張したもの。
#[test]
fn zero_amount_matches_the_straight_line() {
    let lane = volume_lane();
    let map = curve::LaneValueMap::from_lane(&lane, CLIP_RECT);
    for (a_norm, b_norm) in [(0.2_f64, 0.8_f64), (0.8, 0.2)] {
        let (a, b) = (vol_plain(a_norm), vol_plain(b_norm));
        let linear_mid = map.norm_to_y(((a_norm + b_norm) * 0.5) as f32);
        for curve in [
            AutomationCurve::Linear,
            AutomationCurve::Bezier { tension: 0.0 },
            AutomationCurve::Exponential { bend: 0.0 },
        ] {
            let out = flatten(&lane, a, b, curve);
            let mid = sample_polyline_y(&out, 50.0);
            assert!(
                (mid - linear_mid).abs() < 0.5,
                "{curve:?} ({a_norm}→{b_norm}): 中央は線形中点 {linear_mid} を期待 (got {mid})"
            );
        }
    }
}

/// **画面上へ膨らませる向き**に量を増やすと、区間の中ほどで polyline の y が小さくなる
/// (= 画面上で上がる)。 上りは `Exponential { bend: -x }`、 下りは `+x` が上向き
/// (= progress 基準の符号が区間の向きで逆になる、という #73 の核心)。
///
/// 旧 `exponential_bend_positive_is_quadratic` / `..._negative_is_sqrt` は screen y で
/// 1 方向しか見ておらず、この非対称を通していた。
#[test]
fn increasing_the_amount_raises_the_line_on_both_directions() {
    let lane = volume_lane();
    for (a_norm, b_norm, up_sign) in [(0.2_f64, 0.8_f64, -1.0_f32), (0.8, 0.2, 1.0)] {
        let (a, b) = (vol_plain(a_norm), vol_plain(b_norm));
        let straight = sample_polyline_y(&flatten(&lane, a, b, AutomationCurve::Linear), 50.0);
        for amount in [0.4_f32, 0.8] {
            let bent = sample_polyline_y(
                &flatten(&lane, a, b, AutomationCurve::Exponential { bend: up_sign * amount }),
                50.0,
            );
            assert!(
                bent < straight - 2.0,
                "曲線 ({a_norm}→{b_norm}, amount {amount}): 直線 {straight} より画面上 (小さい y) を期待 (got {bent})"
            );
        }
    }
}

/// S 字 (`Bezier`) は u=0.5 を必ず通るので、中点ではなく **1/4 点**で向きを見る。
/// 上りでも下りでも「tension を上げると前半が画面上へ寄る」向きが揃うのは、
/// `Bezier` が画面基準ではなく progress 基準だから — ここも符号ではなく y で assert する。
#[test]
fn s_curve_moves_the_quarter_point_consistently() {
    let lane = volume_lane();
    for (a_norm, b_norm) in [(0.2_f64, 0.8_f64), (0.8, 0.2)] {
        let (a, b) = (vol_plain(a_norm), vol_plain(b_norm));
        let straight = sample_polyline_y(&flatten(&lane, a, b, AutomationCurve::Linear), 25.0);
        let pos = sample_polyline_y(
            &flatten(&lane, a, b, AutomationCurve::Bezier { tension: 1.0 }),
            25.0,
        );
        let neg = sample_polyline_y(
            &flatten(&lane, a, b, AutomationCurve::Bezier { tension: -1.0 }),
            25.0,
        );
        // tension の符号で 1/4 点が直線の反対側に出る (= S 字が反転する)。
        assert!(
            (pos - straight).signum() != (neg - straight).signum(),
            "({a_norm}→{b_norm}): tension ± で 1/4 点が直線の反対側に出るはず \
             (straight {straight} / +1 {pos} / -1 {neg})"
        );
        assert!(
            (pos - straight).abs() > 2.0 && (neg - straight).abs() > 2.0,
            "({a_norm}→{b_norm}): 1/4 点は直線から明確に離れるはず"
        );
    }
}

// ============================================================
// r.md #73 の回帰網
// ============================================================

/// **#73 の本体**: 上り区間でも下り区間でも「カーソルを上へ動かすと画面上で線が上がる」。
/// 旧実装は上り区間で逆になっていた (感度定数 `-dy * 2 / lane_h` が区間の向きを見ていない)。
///
/// `solve_bend` に「直線より上の目標値」を渡し、得た curve を `eval_norm` で grab_u で
/// 評価すると **直線より値が大きい (= 画面上で上)** ことを確認する。
/// **符号 (bend の正負) は assert しない** — progress 基準なので区間の向きで変わる。
#[test]
fn bend_drag_up_raises_the_line_on_both_rising_and_falling_segments() {
    let lane = volume_lane();
    let map = curve::LaneValueMap::from_lane(&lane, CLIP_RECT);
    let grab_u = 0.4_f64;
    for (a_norm, b_norm) in [(0.2_f64, 0.8_f64), (0.8, 0.2)] {
        let (a, b) = (vol_plain(a_norm), vol_plain(b_norm));
        let straight_norm = curve::eval_norm(map, a, b, grab_u, AutomationCurve::Linear);
        // 直線より 0.08 だけ上 (norm は上向き) を目標にする。
        let target = straight_norm + 0.08;
        for start in [
            AutomationCurve::Exponential { bend: 0.0 },
            AutomationCurve::Bezier { tension: 0.0 },
        ] {
            let solved = curve::solve_bend(map, a, b, grab_u, start, target)
                .unwrap_or_else(|| panic!("{start:?} ({a_norm}→{b_norm}) は解けるはず"));
            let got = curve::eval_norm(map, a, b, grab_u, solved);
            assert!(
                got > straight_norm + 0.02,
                "{start:?} ({a_norm}→{b_norm}): 直線 {straight_norm} より上を期待 (got {got}, solved {solved:?})"
            );
        }
    }
}

/// 掴んだ場所が指に付いてくる: 到達可能な範囲内の目標なら、
/// 解いた curve を grab_u で評価すると目標値に一致する (1e-3 以内)。
#[test]
fn bend_solve_puts_the_curve_under_the_finger() {
    let lane = volume_lane();
    let map = curve::LaneValueMap::from_lane(&lane, CLIP_RECT);
    for (a_norm, b_norm) in [(0.1_f64, 0.9_f64), (0.9, 0.1)] {
        let (a, b) = (vol_plain(a_norm), vol_plain(b_norm));
        for grab_u in [0.3_f64, 0.5, 0.7] {
            let straight = curve::eval_norm(map, a, b, grab_u, AutomationCurve::Linear);
            // 到達可能な小さい変位 (Exponential の帯 [u², √u] に収まる)。
            for delta in [-0.05_f32, 0.05] {
                let target = (straight + delta).clamp(0.0, 1.0);
                let solved = curve::solve_bend(
                    map,
                    a,
                    b,
                    grab_u,
                    AutomationCurve::Exponential { bend: 0.0 },
                    target,
                )
                .expect("Exponential は中点でも解ける");
                let got = curve::eval_norm(map, a, b, grab_u, solved);
                assert!(
                    (got - target).abs() < 1e-3,
                    "({a_norm}→{b_norm}, u={grab_u}, delta={delta}): 指の位置 {target} に一致するはず (got {got})"
                );
            }
        }
    }
}

/// 到達不能な目標では端に飽和して止まる (発散も符号反転も NaN も起きない)。
/// `bend ∈ [-1, 1]` = `k ∈ [0.5, 2]` なので、grab_u で到達できる `w` は `[u², √u]` だけ。
/// u=0.9 なら w ∈ [0.81, 0.949] — その外を要求すると clamp された端で止まる。
#[test]
fn bend_solve_saturates_instead_of_diverging() {
    let lane = volume_lane();
    let map = curve::LaneValueMap::from_lane(&lane, CLIP_RECT);
    // 上り 0.0 → 1.0 (norm) にすると w = 目標 norm そのもの。
    let (a, b) = (vol_plain(0.0), vol_plain(1.0));
    let grab_u = 0.9_f64;
    // w = 0.5 は [0.81, 0.949] の外 → bend = +1.0 に張り付く。
    let solved =
        curve::solve_bend(map, a, b, grab_u, AutomationCurve::Exponential { bend: 0.0 }, 0.5)
            .expect("解が返る (飽和した端の値)");
    let AutomationCurve::Exponential { bend } = solved else {
        panic!("Exponential を期待: {solved:?}");
    };
    assert!(bend.is_finite(), "NaN / inf にならない: {bend}");
    assert!((bend - 1.0).abs() < 1e-5, "値域の端 +1.0 に張り付くはず (got {bend})");
    // 反対側も同様に -1.0 で止まる (w = 0.99 > √0.9 ≈ 0.949)。
    let solved =
        curve::solve_bend(map, a, b, grab_u, AutomationCurve::Exponential { bend: 0.0 }, 0.99)
            .expect("解が返る (飽和した端の値)");
    let AutomationCurve::Exponential { bend } = solved else {
        panic!("Exponential を期待: {solved:?}");
    };
    assert!((bend + 1.0).abs() < 1e-5, "値域の端 -1.0 に張り付くはず (got {bend})");
}

/// S 字は u=0.5 を必ず通る (数学的な固定点) ので、そこを掴んでも解けない。
/// caller は `None` を受けて直前の preview を維持する。
#[test]
fn bend_solve_returns_none_at_the_s_curve_fixed_point() {
    let lane = volume_lane();
    let map = curve::LaneValueMap::from_lane(&lane, CLIP_RECT);
    let (a, b) = (vol_plain(0.2), vol_plain(0.8));
    assert!(
        curve::solve_bend(map, a, b, 0.5, AutomationCurve::Bezier { tension: 0.0 }, 0.7).is_none(),
        "u=0.5 の S 字は固定点なので解けない"
    );
    // 中点から離れれば解ける。
    assert!(
        curve::solve_bend(map, a, b, 0.25, AutomationCurve::Bezier { tension: 0.0 }, 0.45)
            .is_some(),
        "u=0.25 なら S 字も解ける"
    );
}

/// 水平区間 (a == b) と Mute lane は曲げられない。
#[test]
fn bend_is_refused_on_flat_segments_and_mute_lanes() {
    let lane = volume_lane();
    let map = curve::LaneValueMap::from_lane(&lane, CLIP_RECT);
    let flat = vol_plain(0.5);
    assert!(
        curve::solve_bend(map, flat, flat, 0.4, AutomationCurve::Exponential { bend: 0.0 }, 0.7)
            .is_none(),
        "a == b の区間は w が定義できない"
    );
    // Mute lane は逆写像を持たないので直接操作の対象外 (press 側が `is_bendable` で弾く)。
    let mute = mute_lane();
    let mute_map = curve::LaneValueMap::from_lane(&mute, CLIP_RECT);
    assert!(!mute_map.is_bendable(), "Mute lane は曲げられない");
    assert!(map.is_bendable(), "Volume lane は曲げられる");
}

/// 区間 hit-test は描かれた曲線の上で当たり、離れると当たらない。
/// 点の上では `automation_point_at` が先に当たる (呼び出し側の契約)。
/// `id == 0` の点は対象外。
#[test]
fn automation_segment_at_hits_the_drawn_curve() {
    let style = test_style();
    // lanes 全体を 1 track / 1 lane で埋める。 track row 20px + lane 100px で tops を組む。
    let lanes = Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 };
    let view = hit_view();
    let mut lane = volume_lane();
    lane.height_px = 100;
    lane.clips = vec![ArrangementAutomationClip {
        id: 1,
        start_beat: 0.0,
        len_beats: 4.0,
        name: Arc::from("c"),
        points: vec![
            ArrangementAutomationPoint {
                id: 1,
                time_beat: 0.0,
                value_norm: 0.2,
                value_plain: vol_plain(0.2),
                curve: AutomationCurve::Linear,
            },
            ArrangementAutomationPoint {
                id: 2,
                time_beat: 4.0,
                value_norm: 0.8,
                value_plain: vol_plain(0.8),
                curve: AutomationCurve::Linear,
            },
        ],
        share_group_color: None,
    }];
    let tracks = vec![track_with_lane(lane)];
    let tops = vec![0.0_f32, 120.0];
    // lane body は y∈[20, 120)、 clip 描画域は縦 padding ぶん内側。
    let pad = style.automation_clip_v_pad_px;
    let clip_y = 20.0 + pad;
    let clip_h = (100.0 - pad * 2.0).max(2.0);
    // 中央 (u=0.5) の値は norm 0.5 → y = clip_y + 0.5 * clip_h。
    let mid_x = 100.0_f32;
    let mid_y = clip_y + 0.5 * clip_h;
    let hit = curve::automation_segment_at(
        &tracks, &tops, view.track_row_h, view, 0.0, 0.0, lanes, mid_x, mid_y, &style,
    );
    let hit = hit.expect("曲線の上なので当たる");
    assert_eq!(hit.point.point_id, 2, "入射側 (後ろの) 点を指す");
    assert!((hit.grab_u - 0.5).abs() < 1e-2, "掴んだ進捗は 0.5 前後: got {}", hit.grab_u);
    // 20px 離れると当たらない (既定の hit 半径は 6px)。
    assert!(
        curve::automation_segment_at(
            &tracks,
            &tops,
            view.track_row_h,
            view,
            0.0,
            0.0,
            lanes,
            mid_x,
            mid_y - 20.0,
            &style,
        )
        .is_none(),
        "20px 離れたら当たらない"
    );
    // id == 0 の点は対象外 (未採番 sentinel を安定 id で指せない)。
    let mut tracks_unnumbered = tracks.clone();
    tracks_unnumbered[0].automation_lanes[0].clips[0].points[1].id = 0;
    assert!(
        curve::automation_segment_at(
            &tracks_unnumbered,
            &tops,
            view.track_row_h,
            view,
            0.0,
            0.0,
            lanes,
            mid_x,
            mid_y,
            &style,
        )
        .is_none(),
        "id == 0 の点は曲線編集の対象外"
    );
    // Mute lane は曲げられないので当たらない。
    let mut mute_tracks = tracks.clone();
    mute_tracks[0].automation_lanes[0].target =
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute);
    assert!(
        curve::automation_segment_at(
            &mute_tracks,
            &tops,
            view.track_row_h,
            view,
            0.0,
            0.0,
            lanes,
            mid_x,
            mid_y,
            &style,
        )
        .is_none(),
        "Mute lane は hover 強調も bend も起こさない"
    );
}

/// 描画は「鳴る形」と一致する。
/// - (a) affine + 窓の内側では norm 空間の直線と同値 (= 旧実装と 1px も変わらない)
/// - (b) log な target (`GroupTransform::ScaleX`) では `plain_to_norm(apply_curve(...))` と一致
///   (= `Linear` 区間が曲線として描かれる。 これが #73 の意図した修正)
/// - (c) 端点が窓の外 (`GroupTransform::X` で a=-0.5, b=0.5) では前半が下端に張り付いてから
///   立ち上がる (= 旧 norm 直線とは別の形)
#[test]
fn flatten_matches_apply_curve_in_plain_space() {
    // (a) affine + 窓の内側
    let vol = volume_lane();
    let vmap = curve::LaneValueMap::from_lane(&vol, CLIP_RECT);
    let (a, b) = (vol_plain(0.2), vol_plain(0.8));
    let mid = sample_polyline_y(&flatten(&vol, a, b, AutomationCurve::Linear), 50.0);
    assert!((mid - vmap.norm_to_y(0.5)).abs() < 0.5, "affine + 窓内は norm 直線と同値");

    // (b) log target — Linear 区間が曲線として描かれる。
    let sx = scale_x_lane();
    let smap = curve::LaneValueMap::from_lane(&sx, CLIP_RECT);
    let (a, b) = (0.5_f64, 5.0_f64);
    let out = flatten(&sx, a, b, AutomationCurve::Linear);
    let got = sample_polyline_y(&out, 50.0);
    let want = smap.plain_to_y(common::automation::apply_curve(a, b, 0.5, AutomationCurve::Linear));
    assert!(
        (got - want).abs() < 1.0,
        "log lane では plain 評価と一致するはず (got {got}, want {want})"
    );
    // norm どうしの直線とは **違う** (= 形が変わったことの確認)。
    let norm_mid = smap.norm_to_y((smap.to_norm(a) + smap.to_norm(b)) * 0.5);
    assert!(
        (got - norm_mid).abs() > 2.0,
        "log lane では norm 直線とはっきり異なるはず (got {got}, norm 直線 {norm_mid})"
    );

    // (c) 端点が窓の外 — 前半は下端に張り付く。
    let gx = group_x_lane();
    let gmap = curve::LaneValueMap::from_lane(&gx, CLIP_RECT);
    let (a, b) = (-0.5_f64, 0.5_f64);
    let out = flatten(&gx, a, b, AutomationCurve::Linear);
    let bottom_y = gmap.norm_to_y(0.0);
    let quarter = sample_polyline_y(&out, 25.0);
    assert!(
        (quarter - bottom_y).abs() < 0.5,
        "前半 (u=0.25 で plain=-0.25) は下端に張り付くはず (got {quarter}, 下端 {bottom_y})"
    );
    // 旧実装 (端の飽和値どうしを直線で結ぶ) なら u=0.25 は窓の 1/4 高さにいた。
    let old_quarter = gmap.norm_to_y(0.25);
    assert!(
        (quarter - old_quarter).abs() > 10.0,
        "旧 norm 直線 ({old_quarter}) とは別の形になるはず (got {quarter})"
    );
    // 後半は立ち上がる (u=0.75 で plain=0.25 → norm 0.25)。
    let three_quarter = sample_polyline_y(&out, 75.0);
    assert!(
        (three_quarter - gmap.norm_to_y(0.25)).abs() < 1.0,
        "後半は実際の値どおり立ち上がるはず (got {three_quarter})"
    );
}

/// `segment_is_straight_on_screen`: Volume の 0.5→1.5 は true、
/// `GroupTransform::X` の -0.5→0.5 は false、ScaleX はどこでも false。
#[test]
fn straight_on_screen_requires_affine_and_in_window_endpoints() {
    let vol = volume_lane();
    let vmap = curve::LaneValueMap::from_lane(&vol, CLIP_RECT);
    assert!(
        curve::segment_is_straight_on_screen(vmap, 0.5, 1.5),
        "Volume の 0.5→1.5 は窓の内側の affine"
    );

    let gx = group_x_lane();
    let gmap = curve::LaneValueMap::from_lane(&gx, CLIP_RECT);
    assert!(
        curve::segment_is_straight_on_screen(gmap, 0.25, 0.75),
        "窓の内側なら X も 1 次"
    );
    assert!(
        !curve::segment_is_straight_on_screen(gmap, -0.5, 0.5),
        "端点が窓の外に出ると 1 次でなくなる (clamp 飽和)"
    );

    let sx = scale_x_lane();
    let smap = curve::LaneValueMap::from_lane(&sx, CLIP_RECT);
    assert!(
        !curve::segment_is_straight_on_screen(smap, 0.5, 5.0),
        "ScaleX は log なのでどこでも非 affine"
    );
}
