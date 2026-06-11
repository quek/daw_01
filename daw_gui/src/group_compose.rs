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

/// `parent_group_id` を誰かが指していれば「グループ」（派生判定。`Track::kind`
/// のような型フィールドは持たない）。`AppData::is_group_track` の song-level 版で、
/// preview（runner）と export（render_video）が同一述語を共有する SSoT（§5.6）。
pub fn is_group_track(song: &Song, track_id: u32) -> bool {
    song.tracks.iter().any(|t| t.parent_group_id == Some(track_id))
}

/// track が image / video / text クリップを 1 つでも持つか（§5.6 visual 判定の部品）。
fn track_has_visual_clip(track: &Track, song: &Song) -> bool {
    use common::model::ClipContent;
    track.clips.iter().any(|c| {
        matches!(
            song.clip_contents.get(&c.content_id),
            Some(ClipContent::Image(_))
                | Some(ClipContent::Video(_))
                | Some(ClipContent::Text(_))
        )
    })
}

/// §5.6 visual グループ判定: group 自身が `group_transform` データを持つ、または
/// subtree（`parent_group_id` 再帰）に image / video / text クリップを持つ track が
/// 1 つでもあれば true。`AppData::group_has_visual_content` の song-level 版（SSoT）。
/// cycle 安全のため track 数で hop を打ち切る BFS。root 自身も検査対象。
pub fn group_has_visual_content(song: &Song, group_track_id: u32) -> bool {
    let Some(group) = song.track_by_id(group_track_id) else {
        return false;
    };
    if group.group_transform.is_some() || track_has_visual_clip(group, song) {
        return true;
    }
    let mut seen = vec![group_track_id];
    let mut frontier = vec![group_track_id];
    let mut hops = 0;
    while !frontier.is_empty() {
        hops += 1;
        if hops > song.tracks.len() + 1 {
            break;
        }
        let mut next = Vec::new();
        for &pid in &frontier {
            for t in &song.tracks {
                if t.parent_group_id == Some(pid) && !seen.contains(&t.id) {
                    if track_has_visual_clip(t, song) {
                        return true;
                    }
                    seen.push(t.id);
                    next.push(t.id);
                }
            }
        }
        frontier = next;
    }
    false
}

/// §5.6 合成 / 選択オーバーレイの gate（preview / export 共有 SSoT）。visual
/// グループ（[`is_group_track`] かつ [`group_has_visual_content`]）の effective
/// transform を解決して返す。[`group_active_transform`] と違い、transform も lane も
/// 未設定の visual グループも **identity** として含める（= グループ化直後の立ち絵も
/// 合成され、選択時にバウンディングボックスが出る）。純 audio バスは除外。
pub fn active_visual_groups(
    song: &Song,
    song_beat: f64,
) -> std::collections::HashMap<u32, GroupTransform> {
    use std::collections::{HashMap, HashSet};

    // 親→子の隣接を 1pass で構築（O(N)）。`is_group_track` の per-track O(N) 線形
    // 走査と、`group_has_visual_content` の per-track BFS を排して O(tracks^3) を畳む。
    // adjacency の key 集合 = group track（= 誰かに親として指されている track）。
    let mut group_ids: HashSet<u32> = HashSet::new();
    for t in &song.tracks {
        if let Some(pid) = t.parent_group_id {
            group_ids.insert(pid);
        }
    }

    // 各 track が visual clip を直に持つかを 1 回だけ判定（合計 O(全 clip)）。
    // 直に持つ track から `parent_group_id` チェーンを上に辿り、その track 自身と
    // 全祖先 group を「subtree に visual clip あり」とマーク（cycle 安全に hop 上限）。
    // これで `group_has_visual_content` の descendant BFS（root は self の clip も検査）
    // を、全 group ぶん 1 度の上昇伝播で解く。
    let by_id: HashMap<u32, &Track> = song.tracks.iter().map(|t| (t.id, t)).collect();
    let mut subtree_has_clip: HashSet<u32> = HashSet::new();
    for t in &song.tracks {
        if !track_has_visual_clip(t, song) {
            continue;
        }
        let mut cur = Some(t.id);
        let mut hops = 0;
        while let Some(id) = cur {
            if !subtree_has_clip.insert(id) {
                break; // 既訪 → 以遠の祖先も既に塗られている。
            }
            hops += 1;
            if hops > song.tracks.len() + 1 {
                break; // cycle 安全弁。
            }
            cur = by_id.get(&id).and_then(|t| t.parent_group_id);
        }
    }

    let mut out = HashMap::new();
    for track in &song.tracks {
        if !group_ids.contains(&track.id) {
            continue; // group track でない。
        }
        // visual グループ判定（§5.6）: group 自身が group_transform を持つ、または
        // subtree（自身を含む）に visual clip がある。
        let visual = track.group_transform.is_some() || subtree_has_clip.contains(&track.id);
        if !visual {
            continue;
        }
        let gt = group_active_transform(track, song, song_beat).unwrap_or_default();
        out.insert(track.id, gt);
    }
    out
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

/// canvas-normalized（0..1、立ち絵パーツ / image PiP 系）↔ preview screen px
/// の写像。通常の image / text overlay は canvas == project_box（恒等 affine、
/// rotation 0）。active group の子は親 group の affine（[`group_quad_params`] の
/// rect + pivot 回転）を合成する。選択オーバーレイの **描画 / hit-test / drag
/// 逆写像が同じ 1 つの写像を共有する**（SSoT）ので、子が group 変形しても
/// ハンドルがレンダリング結果に追従する（`docs/plan_tachie_group_transform.md`
/// option A）。アプローチ X は shear が出ないので、child rotation = 0 の子矩形は
/// この写像で厳密に「回転した長方形」になる。
#[derive(Debug, Clone, Copy)]
pub struct CanvasMap {
    /// canvas-norm `(0, 0)` が写る pre-rotation screen 原点（px）。
    pub origin: (f32, f32),
    /// canvas 全体（`u = v = 1`）の screen px サイズ。
    pub size: (f32, f32),
    /// canvas 軸が screen 上で回る角度（rad、= group_rot）。通常 overlay は 0。
    pub rotation: f32,
    /// 回転中心（screen px、= group の pivot / anchor）。`rotation == 0` なら未使用。
    pub pivot: (f32, f32),
}

impl CanvasMap {
    /// 通常 overlay 用の恒等写像（canvas == project_box、回転なし）。
    pub fn project(project_box: (f32, f32, f32, f32)) -> Self {
        Self {
            origin: (project_box.0, project_box.1),
            size: (project_box.2.max(1.0), project_box.3.max(1.0)),
            rotation: 0.0,
            pivot: (0.0, 0.0),
        }
    }

    /// active group の子用。`group_quad_params` の rect / pivot / rotation を
    /// 合成する（= 描画 quad と完全一致）。
    pub fn group(t: &GroupTransform, project_box: (f32, f32, f32, f32)) -> Self {
        let (rx, ry, rw, rh, rot, px, py, _) = group_quad_params(t, project_box);
        Self {
            origin: (rx, ry),
            size: (rw.max(1.0), rh.max(1.0)),
            rotation: rot,
            pivot: (rx + px, ry + py),
        }
    }

    /// canvas-norm `(u, v)` → screen px。
    pub fn to_screen(&self, u: f32, v: f32) -> (f32, f32) {
        let pre = (self.origin.0 + u * self.size.0, self.origin.1 + v * self.size.1);
        if self.rotation == 0.0 {
            return pre;
        }
        let (s, c) = self.rotation.sin_cos();
        let lx = pre.0 - self.pivot.0;
        let ly = pre.1 - self.pivot.1;
        (self.pivot.0 + lx * c - ly * s, self.pivot.1 + lx * s + ly * c)
    }

    /// screen px → canvas-norm（[`to_screen`](Self::to_screen) の逆）。drag delta
    /// の逆写像に使う（pivot まわりの平行移動は delta の差分で相殺される）。
    pub fn from_screen(&self, sx: f32, sy: f32) -> (f32, f32) {
        let pre = if self.rotation == 0.0 {
            (sx, sy)
        } else {
            let (s, c) = self.rotation.sin_cos();
            let lx = sx - self.pivot.0;
            let ly = sy - self.pivot.1;
            // 逆回転（-rotation）。
            (self.pivot.0 + lx * c + ly * s, self.pivot.1 - lx * s + ly * c)
        };
        ((pre.0 - self.origin.0) / self.size.0, (pre.1 - self.origin.1) / self.size.1)
    }
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
    fn canvas_map_project_is_identity_affine() {
        // 通常 overlay: canvas == project_box、回転なし。
        let pb = (100.0, 50.0, 400.0, 300.0);
        let m = CanvasMap::project(pb);
        assert_eq!(m.to_screen(0.0, 0.0), (100.0, 50.0));
        assert_eq!(m.to_screen(1.0, 1.0), (500.0, 350.0));
        // round-trip。
        let (u, v) = m.from_screen(300.0, 200.0);
        assert!((u - 0.5).abs() < 1e-4 && (v - 0.5).abs() < 1e-4);
    }

    #[test]
    fn canvas_map_group_matches_quad_and_round_trips() {
        // group affine（pos + 非一様 scale + 任意 anchor + 回転）。
        let mut t = ident();
        t.x = 0.1;
        t.y = -0.2;
        t.scale_x = 1.4;
        t.scale_y = 0.8;
        t.anchor_x = 0.3;
        t.anchor_y = 0.7;
        t.rotation_radians = 0.5;
        let pb = (0.0, 0.0, 400.0, 300.0);
        let m = CanvasMap::group(&t, pb);
        // canvas-norm のアンカー (= 子 PiP が乗る canvas の anchor) は group の
        // pivot へ写る（回転・スケールの中心 = quad の pivot）。
        let (ax, ay) = m.to_screen(t.anchor_x, t.anchor_y);
        assert!((ax - m.pivot.0).abs() < 1e-3 && (ay - m.pivot.1).abs() < 1e-3);
        // 任意点の round-trip（drag 逆写像が成立する保証）。
        for &(u, v) in &[(0.0, 0.0), (1.0, 1.0), (0.25, 0.8), (0.6, 0.1)] {
            let (sx, sy) = m.to_screen(u, v);
            let (ru, rv) = m.from_screen(sx, sy);
            assert!((ru - u).abs() < 1e-3, "u {u} -> {ru}");
            assert!((rv - v).abs() < 1e-3, "v {v} -> {rv}");
        }
    }

    #[test]
    fn canvas_map_group_corner_matches_overlay_rotate_pt() {
        // CanvasMap::to_screen で写した子矩形の隅が、preview の group overlay
        // が使う rotate_pt（quad と同一）と一致することを確認する。
        let mut t = ident();
        t.scale_x = 2.0;
        t.scale_y = 1.5;
        t.rotation_radians = 0.3;
        let pb = (10.0, 20.0, 320.0, 240.0);
        let (rx, ry, rw, rh, rot, px, py, _) = group_quad_params(&t, pb);
        let pivx = rx + px;
        let pivy = ry + py;
        let (sin_r, cos_r) = rot.sin_cos();
        // quad の右下隅 (canvas-norm (1,1)) を rotate_pt で。
        let lx = (rx + rw) - pivx;
        let ly = (ry + rh) - pivy;
        let expect = (pivx + lx * cos_r - ly * sin_r, pivy + lx * sin_r + ly * cos_r);
        let m = CanvasMap::group(&t, pb);
        let got = m.to_screen(1.0, 1.0);
        assert!((got.0 - expect.0).abs() < 1e-3 && (got.1 - expect.1).abs() < 1e-3);
    }

    #[test]
    fn none_transform_is_inactive() {
        let mut track = crate::app::track_with(|t| t.id = 7);
        let song = Song::default();
        assert!(group_active_transform(&track, &song, 0.0).is_none());
        track.group_transform = Some(ident());
        assert!(group_active_transform(&track, &song, 0.0).is_some());
    }

    /// 子トラックを 1 本ぶら下げた group を作る。`visual` なら子に image clip を、
    /// さもなくば clip 無し（純 audio バス）を付ける。group_transform は未設定。
    fn song_with_group(visual: bool) -> (Song, u32) {
        use common::model::{Clip, ClipContent, ImageContent, ImageEvent, ImageSource, ImageSourcePath};
        let mut song = Song::default();
        let group_id = song.alloc_track_id();
        song.tracks.push(crate::app::track_with(|t| {
            t.id = group_id;
            t.name = "G".into();
        }));
        let child_id = song.alloc_track_id();
        let mut child = crate::app::track_with(|t| {
            t.id = child_id;
            t.name = "child".into();
            t.parent_group_id = Some(group_id);
        });
        if visual {
            let img_id = song.alloc_image_source_id();
            song.image_sources.insert(
                img_id,
                ImageSource {
                    path: ImageSourcePath::Absolute("/tmp/x.png".into()),
                    name: "x.png".into(),
                    width: 100,
                    height: 100,
                    format: "Png".into(),
                },
            );
            let cid = song.alloc_content_id();
            song.clip_contents.insert(
                cid,
                ClipContent::Image(ImageContent {
                    events: vec![ImageEvent {
                        source_id: img_id,
                        event_start_in_clip_beats: 0.0,
                        event_length_beats: 8.0,
                        x: 0.1,
                        y: 0.1,
                        w: 0.5,
                        h: 0.5,
                        opacity: 1.0,
                        rotation_radians: 0.0,
                        muted: false,
                        fade_in_beats: 0.0,
                        fade_out_beats: 0.0,
                        fade_in_curve: common::model::FadeCurve::Linear,
                        fade_out_curve: common::model::FadeCurve::Linear,
                    }],
                }),
            );
            let cl = child.alloc_clip_id();
            child.clips.push(Clip {
                id: cl,
                name: "img".into(),
                start_beat: 0.0,
                length_beats: 8.0,
                content_id: cid,
                notes: Vec::new(),
                color: None,
                auto_lipsync: false,
                ..Default::default()
            });
        }
        song.tracks.push(child);
        (song, group_id)
    }

    #[test]
    fn visual_group_is_active_without_transform() {
        // §5.6 回帰: group_transform / lane が未設定でも、image 子を持つ visual
        // group は active（identity transform）として合成 / overlay gate に乗る。
        // これが false に戻ると「グループ選択しても水色ハンドルが出ない」 バグ。
        let (song, group_id) = song_with_group(true);
        assert!(is_group_track(&song, group_id));
        assert!(group_has_visual_content(&song, group_id));
        let active = active_visual_groups(&song, 0.0);
        let gt = active.get(&group_id).expect("visual group must be active");
        assert_eq!(*gt, GroupTransform::default(), "未設定なら identity");
    }

    #[test]
    fn audio_only_group_is_excluded() {
        // 視覚 clip を持たない純 audio バスは合成 / overlay の対象外。
        let (song, group_id) = song_with_group(false);
        assert!(is_group_track(&song, group_id));
        assert!(!group_has_visual_content(&song, group_id));
        assert!(!active_visual_groups(&song, 0.0).contains_key(&group_id));
    }
}
