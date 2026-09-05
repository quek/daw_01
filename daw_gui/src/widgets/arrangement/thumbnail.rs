//! video / image clip の **サムネイル敷き詰め** (幾何 + 描画)。
//!
//! r.md #94: アレンジのクリップとランチャーのセルは同じ content を見せるので、
//! 「どう敷き詰めるか」 は **ここ 1 本** ([`draw_thumbnail_tiles`]) を両方が呼ぶ
//! (アーキテクチャ不変条件 6 の精神 — 1 つの見え方を二重実装しない)。 呼び出し側は
//! 「矩形 + 可視域 + [`ContentMap`]」 だけを渡す。 アレンジはビューのズーム由来の
//! map、 セルは「content の窓をセル内側幅に引き伸ばした」 map を渡すので、 サムネイルの
//! 見え方の規則 (行高 × native aspect の固定サイズ / content 原点に位相固定 / はみ出しは
//! 切り抜き) はどちらの面でも同じになる。
//!
//! `draw.rs` から分離 (ファイル budget、 不変条件 9)。

use super::*;

/// 1 つの clip に敷くサムネイルタイルの上限。
///
/// 可視域カリング後の枚数なので通常は 2 桁に収まる (16:9 / 行高 46px なら 1 枚 82px、
/// lanes 幅 1920px でも 24 枚)。 上限に当たるのは「極端に縦長のソース × 高ズーム」
/// だけで、 そのとき残りはタイルを描かず base fill のまま残る (= 黙って全部描くのを
/// やめる代わりに、 描けた分は正しい位相で並ぶ)。
pub(super) const MAX_THUMBNAIL_TILES: u32 = 512;

/// M14 Phase 72 (daw_01 #044) / r.md #68: video / image clip のサムネイル敷き詰め方。
///
/// **clip 矩形にフィットさせない**。 サムネイルも clip の「中身」 なので、
/// 1. 1 枚の寸法は **行高 × native aspect** で決まり clip の長さには一切依存しない、
/// 2. 並べる位相は **content 原点**に固定する、 3. はみ出す分は clip 矩形で切り抜く。
///
/// 旧実装 (`aspect_fit_rect`) は clip 矩形の中央に 1 枚を letterbox 配置していたため、
/// 1. clip を伸ばすとサムネイルが水平に滑り、 2. 細い clip では拡大縮小し、
/// 3. 長い clip を横スクロールして先頭が画面外に出ると 1 枚も見えなくなっていた。
///
/// 敷き詰めは REAPER の **Preferences → Video/REX/Misc → still image thumbnail
/// display mode = "Center/tile image"** と同じ流儀 (同じ 1 枚を隙間なく繰り返す)。
/// 位相を clip の左端ではなく content 原点に取るのが肝で、 これにより
/// 「トリムしてもタイルの絶対時間位置が変わらない」 = 絵が滑らない が成り立つ
/// (左端 trim は `start_beat` と `content_offset_beats` が同量動くので content 原点は不変)。
///
/// `visible_x0` / `visible_x1` は描画対象の可視域 (= `clip_rect ∩ lanes` の x 範囲)。
/// ここでカリングするので、 長い clip でもタイル数は画面幅で頭打ちになる。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ThumbnailTiling {
    /// 可視域に掛かる **1 枚目** の左端 x (content 原点からの整数枚目)。
    pub first_x: f32,
    /// タイル 1 枚の幅 px (= 行高 × native aspect)。
    pub tile_w: f32,
    /// 高さ px (= clip 矩形の高さ)。
    pub tile_h: f32,
    /// 描く枚数 ([`MAX_THUMBNAIL_TILES`] で頭打ち)。
    pub count: u32,
    /// 上限で打ち切ったか (= 可視域を覆いきれていない)。
    pub truncated: bool,
}

/// r.md #68: サムネイルの敷き詰め幾何。 可視域が無い / 退化した寸法では `None`。
///
/// - `tex_w` / `tex_h` = 0 は 1 に clamp (`u32` の 0 を許容しつつ ZeroDiv を回避)
/// - `clip_rect.h` 0 近傍も 0.001 に clamp (= 0px 高さの異常 case で幅を 0 に押さえる)
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
pub(super) fn thumbnail_tiling(
    clip_rect: Rect,
    visible_x0: f32,
    visible_x1: f32,
    map: ContentMap,
    thumb: ClipThumbnail,
) -> Option<ThumbnailTiling> {
    if visible_x1 <= visible_x0 {
        return None;
    }
    let texture_width = thumb.width.max(1) as f32;
    let texture_height = thumb.height.max(1) as f32;
    let tile_h = clip_rect.h.max(0.001);
    let tile_w = tile_h * (texture_width / texture_height);
    if !tile_w.is_finite() || tile_w <= 0.0 {
        return None;
    }
    // 位相の原点 = この thumbnail が表す event の content 上の開始拍。
    let origin_x = map.x(thumb.start_in_content_beats);
    // 可視域に掛かる最初の整数枚目 (原点より左でも負の index で正しく続く)。
    let k0 = ((visible_x0 - origin_x) / tile_w).floor();
    let first_x = origin_x + k0 * tile_w;
    let span = visible_x1 - first_x;
    // 壊れた project (NaN の start_beat 等) で NaN 座標の quad を積まない。
    // `NaN <= 0.0` は false をすり抜けるので、 有限性を明示的に見る。
    if !first_x.is_finite() || !span.is_finite() || span <= 0.0 {
        return None;
    }
    let needed = (span / tile_w).ceil().max(1.0);
    let cap = f32::from(u16::try_from(MAX_THUMBNAIL_TILES).unwrap_or(u16::MAX));
    Some(ThumbnailTiling {
        first_x,
        tile_w,
        tile_h,
        count: if needed >= cap { MAX_THUMBNAIL_TILES } else { needed as u32 },
        truncated: needed > cap,
    })
}

/// r.md #94: サムネイルを `r` に **敷き詰めて** 描く (アレンジのクリップ / ランチャーの
/// セルの共通経路)。
///
/// `r` = サムネイルを置く矩形 (高さがタイルの高さになる)、 `visible` = 実際に見える
/// 範囲 (`r ∩ レーン / 格子`、 これで切り抜く)、 `map` = content-local 拍 → 画面 x。
/// 位相は content 原点に固定する ([`thumbnail_tiling`]) ので、 呼び出し側の map が
/// ズーム由来でも「窓をセル幅に引き伸ばした」 ものでも、 トリムで絵が滑らない性質は
/// 同じに保たれる。 fill / muted ハッチ / ラベルは描かない — それらは面ごとの描画順
/// (fill → サムネイル → ハッチ → ラベル) を持つ呼び出し側の責務。
#[allow(clippy::cast_precision_loss)]
pub(super) fn draw_thumbnail_tiles<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    visible: Rect,
    map: ContentMap,
    thumb: ClipThumbnail,
) {
    let Some(t) = thumbnail_tiling(r, visible.x, visible.x + visible.w, map, thumb) else {
        return;
    };
    for k in 0..t.count {
        hctx.push_textured_quad(TexturedQuad {
            rect: Rect::new(t.first_x + t.tile_w * k as f32, r.y, t.tile_w, t.tile_h),
            texture: thumb.texture,
            alpha: 1.0,
            uv_min: (0.0, 0.0),
            uv_max: (1.0, 1.0),
            clip_rect: Some(visible),
            rotation_radians: 0.0,
            rotation_pivot: None,
        });
    }
}
