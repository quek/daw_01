//! 立ち絵 group transform の合成補助（`docs/plan_tachie_group_transform.md`）。
//!
//! アプローチ X: 親グループにぶら下がる立ち絵パーツ（image 子）を z 順に
//! 1 枚のオフスクリーンテクスチャへ合成し、その 1 枚に親の 2D affine
//! （位置 / 回転 / 非一様スケール / 任意アンカー）+ opacity を 1 回かける。
//! 各子へ個別に行列をかける方式（shear が出る）を避けるための設計。
//!
//! このモジュールは GPU を触らない純粋な解決ロジックのみを持つ:
//! - [`group_active_transform`] — group track の effective transform を解決
//!   （base = `Track.group_transform`、各 param は GroupTransform lane で override）。
//! - [`group_quad_params`] — 解決済み transform を「合成済み 1 枚」へかける
//!   `TexturedQuad` パラメータ（rect / rotation / pivot / alpha）へ変換。
//!
//! GPU 合成（`Renderer::composite_scene_to_texture`）は preview_window /
//! render_video が、[`GroupChildQuad`] / [`GroupLayer`] を受け取って行う。

use common::model::{AutomationTarget, GroupTransform, GroupTransformParam, Song, Track};
use daw_ui_renderer::TextureHandle;

/// 合成キャンバスへ描く 1 子パーツ。`dest` は合成キャンバス（= project
/// resolution）内の normalized 0..1 PiP rect、`rotation_radians` は子自身の
/// rect 中心回転。texture は preview / offscreen renderer に登録済みの handle。
#[derive(Debug, Clone, Copy)]
pub struct GroupChildQuad {
    pub texture: TextureHandle,
    /// 合成キャンバス内 normalized 0..1 PiP rect (x, y, w, h)。
    pub dest: (f32, f32, f32, f32),
    pub alpha: f32,
    pub rotation_radians: f32,
}

/// 1 つの visual group。z 順（bottom→top）の子 quad と、解決済みの親 affine。
#[derive(Debug, Clone)]
pub struct GroupLayer {
    pub children: Vec<GroupChildQuad>,
    pub transform: GroupTransform,
    /// この group track が選択中か（preview に bounding box + anchor を描く）。
    pub selected: bool,
}

/// group track の「現在 effective な」 transform を解決する。
///
/// base = `Track.group_transform`、各 param に `GroupTransform(param)` lane が
/// あればその beat 値（plain 単位）で override。`group_transform` も lane も
/// 無ければ `None`（= visual transform 非アクティブ → 子は通常レイヤーとして
/// 描く）。`TrackBuiltin` と同じく clip 非依存。
pub fn group_active_transform(
    group_track: &Track,
    song: &Song,
    song_beat: f64,
) -> Option<GroupTransform> {
    let has_lane = group_track
        .automation_lanes
        .iter()
        .any(|l| matches!(l.target, AutomationTarget::GroupTransform(_)));
    if group_track.group_transform.is_none() && !has_lane {
        return None;
    }
    let mut t = group_track.group_transform.unwrap_or_default();
    let resolve = |param: GroupTransformParam, fallback: f32| -> f32 {
        let Some(lane) = group_track.automation_lanes.iter().find(
            |l| matches!(l.target, AutomationTarget::GroupTransform(p) if p == param),
        ) else {
            return fallback;
        };
        // lane 値は plain 単位（automation point / default_value が plain）。
        // 正規化（log space / Pan idiom）は UI 表示専用なのでここでは使わない。
        common::automation::lane_value_at(lane, &song.clip_contents, song_beat) as f32
    };
    t.x = resolve(GroupTransformParam::X, t.x);
    t.y = resolve(GroupTransformParam::Y, t.y);
    t.rotation_radians = resolve(GroupTransformParam::Rotation, t.rotation_radians);
    t.scale_x = resolve(GroupTransformParam::ScaleX, t.scale_x);
    t.scale_y = resolve(GroupTransformParam::ScaleY, t.scale_y);
    t.anchor_x = resolve(GroupTransformParam::AnchorX, t.anchor_x);
    t.anchor_y = resolve(GroupTransformParam::AnchorY, t.anchor_y);
    t.opacity = resolve(GroupTransformParam::Opacity, t.opacity);
    Some(t)
}

/// 解決済み group transform を、合成済み 1 枚にかける `TexturedQuad` の
/// パラメータへ変換する純粋関数。
///
/// `box_xywh` = 合成テクスチャが `scale = 1 / pos = 0` のとき占める矩形
/// （screen px）。preview では letterbox された project_box、export では
/// `(0, 0, out_w, out_h)`。`pos` / `anchor` は project-normalized（0..1）。
///
/// 合成済みテクスチャは軸整列した矩形コンテンツなので、`T·R·S`（非一様
/// scale + 任意 anchor）は「rect（位置 + 非一様スケール）+ pivot 回転」で
/// 完全表現できる（`R·S(矩形)` = 回転した矩形 = pivot 回転 rect）。
///
/// 返り値: `(rect_x, rect_y, rect_w, rect_h, rotation_radians, pivot_x, pivot_y, alpha)`。
/// `pivot_*` は rect 左上相対 px（`TexturedQuad.rotation_pivot = Some`）。
pub fn group_quad_params(
    t: &GroupTransform,
    box_xywh: (f32, f32, f32, f32),
) -> (f32, f32, f32, f32, f32, f32, f32, f32) {
    let (bx, by, bw, bh) = box_xywh;
    let rect_w = t.scale_x * bw;
    let rect_h = t.scale_y * bh;
    // scale = 1 のときのアンカー screen 位置（box 内 normalized anchor）。
    let anchor_sx = bx + t.anchor_x * bw;
    let anchor_sy = by + t.anchor_y * bh;
    // rect 左上 = anchor_screen + pos*box − anchor*rect（アンカー中心に拡縮）。
    let rect_x = anchor_sx + t.x * bw - t.anchor_x * rect_w;
    let rect_y = anchor_sy + t.y * bh - t.anchor_y * rect_h;
    // pivot は scale 後 rect 内のアンカー位置（rect 左上相対 px）。
    let pivot_x = t.anchor_x * rect_w;
    let pivot_y = t.anchor_y * rect_h;
    (
        rect_x,
        rect_y,
        rect_w,
        rect_h,
        t.rotation_radians,
        pivot_x,
        pivot_y,
        t.opacity,
    )
}

/// 合成キャンバスの解像度を決める（§8.1 案 B supersample）。
///
/// 親が拡大（scale > 1）すると project 解像度で合成した 1 枚が引き伸ばされて
/// ボケる。max scale に応じて 1× / 2× / 4× で合成解像度を上げ、テクセル密度を
/// 稼ぐ。pool churn（gui_01 のサイズ別 target cache）を避けるため 2 の冪に
/// 量子化し、安全なテクスチャ上限でクランプする。group quad の配置 math
/// （[`group_quad_params`]）はキャンバス解像度に依存しない（uv 0..1 で全体を
/// サンプルするだけ）ので、ここは「何テクセルで焼くか」だけを決める。
pub fn group_composite_canvas(proj: (u32, u32), t: &GroupTransform) -> (u32, u32) {
    const MAX_DIM: u32 = 8192;
    let s = t.scale_x.max(t.scale_y);
    let ss: u32 = if s > 2.0 {
        4
    } else if s > 1.0 {
        2
    } else {
        1
    };
    let w = proj.0.saturating_mul(ss).clamp(1, MAX_DIM);
    let h = proj.1.saturating_mul(ss).clamp(1, MAX_DIM);
    (w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident() -> GroupTransform {
        GroupTransform::default()
    }

    #[test]
    fn composite_canvas_supersamples_on_scale_up() {
        let proj = (1920, 1080);
        // 等倍は project 解像度のまま。
        assert_eq!(group_composite_canvas(proj, &ident()), (1920, 1080));
        let mut t = ident();
        t.scale_x = 1.5;
        assert_eq!(group_composite_canvas(proj, &t), (3840, 2160)); // 2×
        t.scale_x = 3.0;
        // 4× だが 8192 上限でクランプ（7680 は OK、4320 も OK）。
        assert_eq!(group_composite_canvas(proj, &t), (7680, 4320));
    }

    #[test]
    fn identity_maps_rect_to_box() {
        // scale=1, pos=0, anchor=0.5 → rect = box そのもの、pivot = 中心。
        let (rx, ry, rw, rh, rot, px, py, a) =
            group_quad_params(&ident(), (100.0, 50.0, 400.0, 300.0));
        assert!((rx - 100.0).abs() < 1e-4);
        assert!((ry - 50.0).abs() < 1e-4);
        assert!((rw - 400.0).abs() < 1e-4);
        assert!((rh - 300.0).abs() < 1e-4);
        assert_eq!(rot, 0.0);
        assert!((px - 200.0).abs() < 1e-4); // 0.5 * 400
        assert!((py - 150.0).abs() < 1e-4); // 0.5 * 300
        assert!((a - 1.0).abs() < 1e-4);
    }

    #[test]
    fn scale_about_anchor_keeps_anchor_fixed() {
        // anchor=(0.5,0.5), scale=2 → アンカー screen 位置は不変、rect は 2 倍。
        let mut t = ident();
        t.scale_x = 2.0;
        t.scale_y = 2.0;
        let box_xywh = (0.0, 0.0, 400.0, 300.0);
        let (rx, ry, rw, rh, _, _, _, _) = group_quad_params(&t, box_xywh);
        // アンカー screen = (200,150)。rect 中心 = rx+rw/2 / ry+rh/2 が一致するはず。
        assert!((rx + rw / 2.0 - 200.0).abs() < 1e-4);
        assert!((ry + rh / 2.0 - 150.0).abs() < 1e-4);
        assert!((rw - 800.0).abs() < 1e-4);
        assert!((rh - 600.0).abs() < 1e-4);
    }

    #[test]
    fn position_offset_is_box_relative() {
        // pos=(0.25,0) → rect が box 幅の 0.25 倍だけ右へ。
        let mut t = ident();
        t.x = 0.25;
        let (rx, _, _, _, _, _, _, _) = group_quad_params(&t, (0.0, 0.0, 400.0, 300.0));
        assert!((rx - 100.0).abs() < 1e-4); // 0.25 * 400
    }

    #[test]
    fn none_transform_is_inactive() {
        let mut track = Track::default();
        track.id = 7;
        let song = Song::default();
        assert!(group_active_transform(&track, &song, 0.0).is_none());
        track.group_transform = Some(ident());
        assert!(group_active_transform(&track, &song, 0.0).is_some());
    }
}
