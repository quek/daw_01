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
use daw_ui_renderer::{Color, GlyphArea, Rect, Scene, TextureHandle, TexturedQuad, VAlign};

use crate::video_fx::{VideoFxEngine, VideoFxRenderer};

/// 1 トラックの合成画に積む 1 アイテム。座標は合成キャンバス（= project
/// resolution、transform 拡大時は supersample 後）内の normalized 0..1。
/// FIXME #54 Wave2: 立ち絵 group の子パーツも、通常トラックの動画フレーム /
/// PiP 画像 / テキストも、すべてこの型で「トラック合成画」へ積む（SSoT）。
#[derive(Debug, Clone)]
pub enum CompositeItem {
    /// テクスチャ quad（動画フレーム / PiP 画像 / 立ち絵子パーツ）。
    Quad {
        texture: TextureHandle,
        /// 合成キャンバス内 normalized 0..1 PiP rect (x, y, w, h)。
        dest: (f32, f32, f32, f32),
        alpha: f32,
        /// rect 中心回転（radians）。
        rotation_radians: f32,
    },
    /// テキストオーバーレイ（canvas-normalized rect + project-px font）。合成画へ
    /// 焼き込むことで track 効果がテキストにも乗る（plan_video_fx §3）。
    Text(crate::text_compose::ActiveTextFrame),
}

/// 1 トラックの視覚合成（plan_video_fx §3 の「トラック合成画」）。動画 + PiP 画像 +
/// テキストを z 順に 1 枚の RGBA へ合成し、track 効果チェーンを 1 回かけてから配置する。
///
/// `transform == None` は identity 配置（合成画を canvas 全体 = project_box に置く）。
/// `Some` は立ち絵 group / Transform device の affine 配置（approach X: 1 枚へ合成
/// した**あと**に親 affine、shear / 二重適用なし）。`items` が単一 quad で `fx` 空・
/// `transform` None のときは、消費側が合成往復を省いて直接描く（plain トラックの
/// fast-path = 現状維持・クリスプ・無コスト）。
#[derive(Debug, Clone)]
pub struct TrackComposite {
    /// この合成の owning track id（選択判定 / デバッグ用）。
    pub track_id: u32,
    /// bottom→top の合成アイテム（canvas 空間）。
    pub items: Vec<CompositeItem>,
    /// 配置 transform。None = identity（canvas 全体）。Some = group / Transform affine。
    pub transform: Option<GroupTransform>,
    /// track 効果チェーン（解決済み実効値）。合成画 1 枚へチェーン順適用。空なら効果なし。
    pub fx: Vec<crate::video_fx::ResolvedEffect>,
    /// 選択中か（group / Transform 選択時に bounding box + anchor を描く）。
    pub selected: bool,
}

impl TrackComposite {
    /// fast-path 可否: 効果も配置 transform も無ければ、合成往復せず item を直接描ける。
    #[must_use]
    pub fn is_passthrough(&self) -> bool {
        self.fx.is_empty() && self.transform.is_none()
    }
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
    mod_scalars: &[f32],
) -> Option<GroupTransform> {
    let has_lane = group_track
        .automation_lanes
        .iter()
        .any(|l| matches!(l.target, AutomationTarget::GroupTransform(_)));
    // docs/plan_modulation_routing_redesign.md §3.1: GroupTransform param を
    // 変調する routing があれば lane / group_transform が無くても transform を
    // アクティブにする (lane-free モジュレーション)。
    let has_mod = group_track
        .mod_routings
        .iter()
        .any(|r| matches!(r.target, AutomationTarget::GroupTransform(_)));
    if group_track.group_transform.is_none() && !has_lane && !has_mod {
        return None;
    }
    let mut t = group_track.group_transform.unwrap_or_default();
    let resolve = |param: GroupTransformParam, fallback: f32| -> f32 {
        let target = AutomationTarget::GroupTransform(param);
        // base = lane があれば lane 値 (plain)、無ければ group_transform の値。
        // そこに当該 GroupTransform 変調を正規化領域で合成 (lane 無しでも)。
        let base = match group_track.automation_lanes.iter().find(
            |l| matches!(l.target, AutomationTarget::GroupTransform(p) if p == param),
        ) {
            Some(lane) => common::automation::lane_value_at(lane, &song.clip_contents, song_beat),
            None => f64::from(fallback),
        };
        common::automation::apply_modulation_with_scalars(
            song,
            &target,
            base,
            &group_track.mod_routings,
            mod_scalars,
        ) as f32
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
    mod_scalars: &[f32],
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
        let gt = group_active_transform(track, song, song_beat, mod_scalars).unwrap_or_default();
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

/// `src` を `canvas` に aspect-fit（レターボックス）した **normalized 0..1** rect を返す。
/// FIXME #54 Wave2: 動画フレームをトラック合成キャンバス内に収めるのに使う（PiP 画像は
/// 既に normalized なので不要）。`canvas`/`src` は px 寸法。
#[must_use]
pub fn aspect_fit_norm(canvas: (f32, f32), src: (f32, f32)) -> (f32, f32, f32, f32) {
    let (cw, ch) = (canvas.0.max(1.0), canvas.1.max(1.0));
    let (sw, sh) = (src.0.max(1.0), src.1.max(1.0));
    let scale = (cw / sw).min(ch / sh);
    let w = sw * scale;
    let h = sh * scale;
    let x = (cw - w) * 0.5;
    let y = (ch - h) * 0.5;
    (x / cw, y / ch, w / cw, h / ch)
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

/// FIXME #54 Wave2 (plan_video_fx §3): 1 [`TrackComposite`] を `scene` に描く
/// **preview / export 共通の SSoT 経路**。
///
/// - `is_passthrough`（効果も配置 transform も無い plain track）: items を `project_box`
///   内 screen/canvas px へ直接描く（合成往復なし・クリスプ・無コスト）。
/// - さもなくば: items を canvas（`group_composite_canvas`、transform 拡大時 supersample）へ
///   1 枚合成 → track 効果チェーンを `apply_chain` → 配置 transform（identity = `project_box`
///   全体 / group affine = approach X）で 1 quad push。
///
/// 選択オーバーレイ（bounding box / handle）は含まない（preview のみが別途描く）。
/// `proj_res` は合成キャンバス基準解像度（preview = `Song.video_resolution`、export =
/// 出力解像度）。`project_box` は配置先 px 矩形（preview = letterbox 区域、export =
/// `(0,0,out_w,out_h)`）。
pub(crate) fn composite_and_place<R: VideoFxRenderer>(
    tc: &TrackComposite,
    project_box: (f32, f32, f32, f32),
    proj_res: (u32, u32),
    renderer: &mut R,
    fx_engine: &mut VideoFxEngine,
    scene: &mut Scene,
) {
    if tc.is_passthrough() {
        let pscale = if proj_res.0 == 0 {
            1.0
        } else {
            project_box.2 / proj_res.0 as f32
        };
        for item in &tc.items {
            match item {
                CompositeItem::Quad { texture, dest, alpha, rotation_radians } => {
                    if *alpha <= 0.0 {
                        continue;
                    }
                    scene.push_textured_quad(TexturedQuad {
                        rect: Rect::new(
                            project_box.0 + dest.0 * project_box.2,
                            project_box.1 + dest.1 * project_box.3,
                            dest.2 * project_box.2,
                            dest.3 * project_box.3,
                        ),
                        texture: *texture,
                        alpha: *alpha,
                        uv_min: (0.0, 0.0),
                        uv_max: (1.0, 1.0),
                        clip_rect: None,
                        rotation_radians: *rotation_radians,
                        rotation_pivot: None,
                    });
                }
                CompositeItem::Text(tf) => push_text_glyph(scene, tf, project_box, pscale),
            }
        }
        return;
    }
    if tc.items.is_empty() {
        return; // 効果/transform はあるが描く素材が無い（選択 group の overlay は呼び側）。
    }
    let (proj_w, proj_h) = proj_res;
    let (cw, ch) = match tc.transform {
        Some(t) => group_composite_canvas((proj_w, proj_h), &t),
        None => (proj_w.max(1), proj_h.max(1)),
    };
    let canvas_scale = cw as f32 / proj_w.max(1) as f32;
    let mut sub = Scene::new();
    for item in &tc.items {
        match item {
            CompositeItem::Quad { texture, dest, alpha, rotation_radians } => {
                sub.push_textured_quad(TexturedQuad {
                    rect: Rect::new(
                        dest.0 * cw as f32,
                        dest.1 * ch as f32,
                        dest.2 * cw as f32,
                        dest.3 * ch as f32,
                    ),
                    texture: *texture,
                    alpha: *alpha,
                    uv_min: (0.0, 0.0),
                    uv_max: (1.0, 1.0),
                    clip_rect: None,
                    rotation_radians: *rotation_radians,
                    rotation_pivot: None,
                });
            }
            CompositeItem::Text(tf) => {
                push_text_glyph(&mut sub, tf, (0.0, 0.0, cw as f32, ch as f32), canvas_scale);
            }
        }
    }
    let handle = match renderer.fx_composite_scene(&sub, cw, ch) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, track = tc.track_id, "composite track 合成失敗");
            return;
        }
    };
    let handle = if tc.fx.is_empty() {
        handle
    } else {
        fx_engine.apply_chain(renderer, handle, cw, ch, &tc.fx)
    };
    match tc.transform {
        Some(t) => {
            let (rx, ry, rw, rh, rot, px, py, alpha) = group_quad_params(&t, project_box);
            if rw > 0.0 && rh > 0.0 && alpha > 0.0 {
                scene.push_textured_quad(TexturedQuad {
                    rect: Rect::new(rx, ry, rw, rh),
                    texture: handle,
                    alpha,
                    uv_min: (0.0, 0.0),
                    uv_max: (1.0, 1.0),
                    clip_rect: None,
                    rotation_radians: rot,
                    rotation_pivot: Some((px, py)),
                });
            }
        }
        None => {
            scene.push_textured_quad(TexturedQuad {
                rect: Rect::new(project_box.0, project_box.1, project_box.2, project_box.3),
                texture: handle,
                alpha: 1.0,
                uv_min: (0.0, 0.0),
                uv_max: (1.0, 1.0),
                clip_rect: None,
                rotation_radians: 0.0,
                rotation_pivot: None,
            });
        }
    }
}

/// 1 つの [`crate::text_compose::ActiveTextFrame`] を `box_xywh`（px 領域）内に
/// `font_scale`（project px → 出力 px）でスケールして `scene` に push する。fast-path
/// （box = `project_box` の screen px、scale = box幅/proj幅）と、トラック合成画への
/// 焼き込み（box = `(0,0,cw,ch)` の canvas px、scale = canvas/proj）で共有する。
pub(crate) fn push_text_glyph(
    scene: &mut Scene,
    tf: &crate::text_compose::ActiveTextFrame,
    box_xywh: (f32, f32, f32, f32),
    font_scale: f32,
) {
    if tf.alpha <= 0.0 || tf.text.is_empty() {
        return;
    }
    let rx = box_xywh.0 + tf.x * box_xywh.2;
    let ry = box_xywh.1 + tf.y * box_xywh.3;
    let rw = tf.w * box_xywh.2;
    let rh = tf.h * box_xywh.3;
    let font_size = (tf.font_size_px * font_scale).max(1.0);
    let line_height = font_size * 1.2;
    let fill = Color::rgba(
        tf.fill_color[0],
        tf.fill_color[1],
        tf.fill_color[2],
        tf.fill_color[3] * tf.alpha,
    );
    let outline = Color::rgba(
        tf.outline_color[0],
        tf.outline_color[1],
        tf.outline_color[2],
        tf.outline_color[3] * tf.alpha,
    );
    let shadow = Color::rgba(
        tf.shadow_color[0],
        tf.shadow_color[1],
        tf.shadow_color[2],
        tf.shadow_color[3] * tf.alpha,
    );
    scene.push_text(GlyphArea {
        text: tf.text.clone(),
        // 空文字列は renderer default フォント (= None)、指定があればそのファミリ。
        font_family: if tf.font_family.is_empty() {
            None
        } else {
            Some(tf.font_family.clone())
        },
        left: rx,
        top: ry,
        font_size,
        line_height,
        color: fill,
        clip_rect: None,
        outline_color: outline,
        outline_width_px: tf.outline_width_px * font_scale,
        shadow_color: shadow,
        shadow_offset_px: (tf.shadow_offset_px.0 * font_scale, tf.shadow_offset_px.1 * font_scale),
        shadow_blur_px: tf.shadow_blur_px * font_scale,
        rotation_radians: tf.rotation_radians,
        // FIXME #28: box 内アライメント (実 glyph 幅でレンダラが配置)。
        box_width: Some(rw),
        box_height: Some(rh),
        align_h: crate::text_compose::halign_for(tf.align),
        align_v: VAlign::Center,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident() -> GroupTransform {
        GroupTransform::default()
    }

    #[test]
    fn aspect_fit_norm_letterbox_and_pillarbox() {
        // 16:9 source into 4:3 canvas → 上下バー (height < 1、横は全幅)。
        let (x, y, w, h) = aspect_fit_norm((640.0, 480.0), (1920.0, 1080.0));
        assert!((x - 0.0).abs() < 1e-4, "x={x}");
        assert!((w - 1.0).abs() < 1e-4, "w={w}");
        assert!((h - 0.75).abs() < 1e-3, "h={h}"); // 360/480
        assert!((y - 0.125).abs() < 1e-3, "y={y}"); // 60/480
        // 9:16 portrait into 4:3 landscape → 左右バー (width < 1、縦は全高)。
        let (x, y, w, h) = aspect_fit_norm((640.0, 480.0), (1080.0, 1920.0));
        assert!((y - 0.0).abs() < 1e-4, "y={y}");
        assert!((h - 1.0).abs() < 1e-4, "h={h}");
        assert!((w - 0.421875).abs() < 1e-3, "w={w}"); // 270/640
        assert!((x - 0.289_062_5).abs() < 1e-3, "x={x}"); // 185/640
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
        assert!(group_active_transform(&track, &song, 0.0, &[]).is_none());
        track.group_transform = Some(ident());
        assert!(group_active_transform(&track, &song, 0.0, &[]).is_some());
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
        let active = active_visual_groups(&song, 0.0, &[]);
        let gt = active.get(&group_id).expect("visual group must be active");
        assert_eq!(*gt, GroupTransform::default(), "未設定なら identity");
    }

    #[test]
    fn audio_only_group_is_excluded() {
        // 視覚 clip を持たない純 audio バスは合成 / overlay の対象外。
        let (song, group_id) = song_with_group(false);
        assert!(is_group_track(&song, group_id));
        assert!(!group_has_visual_content(&song, group_id));
        assert!(!active_visual_groups(&song, 0.0, &[]).contains_key(&group_id));
    }
}
