//! float の clip 矩形 → 整数 scissor 矩形への **唯一の** 変換 (r.md #53)。
//!
//! GPU の scissor は整数ピクセル単位なので、 論理座標 (float) の clip 矩形は必ずどこかで
//! 丸められる。 その規則が pipeline ごとに違うと、 同じ論理 clip でも rect / line / texture /
//! glyph が別々のピクセルで切られる (= 中身とクロームのエッジが 1px ずれる)。
//!
//! 旧実装は 4 pipeline とも `set_scissor_rect(l as u32, t as u32, (r - l) as u32, (b - t) as u32)`
//! で、 **左端と幅を独立に切り捨て** ていた。 実効右端は `floor(l) + floor(r - l)` になり、
//! `frac(r) < frac(l)` のとき `floor(r) - 1` に落ちる。 clip 矩形がスクロールに追従して
//! サブピクセルで動くと、 この 1px の切り欠きが出たり入ったりして最右列が明滅する
//! (波形 / thumbnail / fade カーブ / mute ハッチはクリップ矩形を scissor に使う)。
//! さらに glyph だけは各辺独立 floor という **3 つ目の規約** だった。
//!
//! ここでは各辺を独立に `round` する (= ピクセル中心が clip の内側に入る列 / 行だけを残す)。
//! これで
//!
//! - 右端は **右端だけの関数** になる (= 同じ位置で終わる clip は必ず同じ列で切れる)、
//! - 隣接する clip は境界ピクセルを共有する (隙間も重なりも出ない)、
//! - clip が整数ピクセルだけ平行移動する限り scissor も剛体平行移動する
//!   (= 追従スクロールは `pixel_snapped_scroll_beat` で整数移動になっているので、
//!   最右列が出没しない)、
//!
//! が同時に成り立つ。 保守的な外側丸め (floor/ceil) ではなく round にするのは、 波形が
//! 隣のクリップへ 1px はみ出さないようにするため。 なお小数幅の矩形を **任意の**
//! サブピクセル量だけ動かせば可視列数が 137 ↔ 138 と変わるのは整数 scissor の原理的な
//! 限界で、 だからこそ上流 (時間軸の原点) を整数に載せる必要がある。

use daw_ui_platform::PhysicalSize;

use crate::scene::Rect;

/// clip 矩形を画面内に収めつつ整数の `[left, top, right, bottom]` へ丸める。
/// 空 (幅または高さ 0) になる場合は `None` (= その span は描画ごと skip する)。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn scissor_edges(clip: Rect, screen: PhysicalSize) -> Option<[u32; 4]> {
    let sw = screen.width as f32;
    let sh = screen.height as f32;
    let l = clip.x.max(0.0).min(sw).round();
    let t = clip.y.max(0.0).min(sh).round();
    let r = (clip.x + clip.w).clamp(0.0, sw).round();
    let b = (clip.y + clip.h).clamp(0.0, sh).round();
    if r <= l || b <= t {
        return None;
    }
    Some([l as u32, t as u32, r as u32, b as u32])
}

/// `scissor_edges` の `(x, y, w, h)` 版 (`wgpu::RenderPass::set_scissor_rect` にそのまま渡す)。
#[must_use]
pub fn scissor_rect(clip: Rect, screen: PhysicalSize) -> Option<(u32, u32, u32, u32)> {
    let [l, t, r, b] = scissor_edges(clip, screen)?;
    Some((l, t, r - l, b - t))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: PhysicalSize = PhysicalSize {
        width: 1000,
        height: 800,
    };

    fn rect(x: f32, w: f32) -> Rect {
        Rect { x, y: 0.0, w, h: 100.0 }
    }

    /// **右端は右端だけで決まる** (左端に依存しない)。
    ///
    /// 旧実装 `floor(l) + floor(r - l)` はこれを満たさず、 同じ位置で終わる 2 つの clip でも
    /// 左端の端数次第で右端が `floor(r)` と `floor(r) - 1` に割れていた。 これが「クリップ
    /// 追従の clip_rect で切っている波形の最右列が、 スクロール中に出たり入ったりする」
    /// 症状の正体。
    #[test]
    fn right_edge_depends_only_on_right_edge() {
        let right = 300.4_f32;
        let rights: Vec<u32> = (0..40)
            .map(|i| {
                let x = 100.0 + i as f32 * 0.025;
                scissor_edges(rect(x, right - x), SCREEN).expect("non-empty")[2]
            })
            .collect();
        assert!(
            rights.iter().all(|&v| v == rights[0]),
            "同じ右端 {right} なのに scissor の右端が割れた: {rights:?}"
        );
    }

    /// 隣接する矩形 (`A.right == B.left`) は scissor でも境界を共有する
    /// (= 隙間も重なりも作らない)。
    #[test]
    fn adjacent_rects_share_the_boundary() {
        for i in 0..20 {
            let boundary = 200.0 + i as f32 * 0.05;
            let a = scissor_edges(rect(100.0, boundary - 100.0), SCREEN).unwrap();
            let b = scissor_edges(rect(boundary, 80.0), SCREEN).unwrap();
            assert_eq!(a[2], b[0], "boundary {boundary} で A.right != B.left");
        }
    }

    /// 丸め誤差は最大でも半ピクセル (= round 規約)。 外側 (floor/ceil) に丸めて隣の
    /// クリップへ 1px はみ出すことはない。
    #[test]
    fn edges_stay_within_half_a_pixel() {
        for i in 0..20 {
            let x = 100.0 + i as f32 * 0.05;
            let [l, _, r, _] = scissor_edges(rect(x, 137.4), SCREEN).unwrap();
            assert!((f64::from(l) - f64::from(x)).abs() <= 0.5);
            assert!((f64::from(r) - f64::from(x + 137.4)).abs() <= 0.5);
        }
    }

    /// 整数ピクセルだけ平行移動したら scissor もちょうど同じだけ動く (剛体平行移動)。
    #[test]
    fn integer_translation_moves_scissor_rigidly() {
        let base = scissor_edges(rect(100.3, 50.6), SCREEN).unwrap();
        let moved = scissor_edges(rect(107.3, 50.6), SCREEN).unwrap();
        assert_eq!(moved[0], base[0] + 7);
        assert_eq!(moved[2], base[2] + 7);
    }

    #[test]
    fn clamps_to_screen_and_rejects_empty() {
        assert_eq!(scissor_rect(rect(-20.0, 10.0), SCREEN), None);
        let (x, _, w, _) = scissor_rect(rect(-20.0, 50.0), SCREEN).unwrap();
        assert_eq!((x, w), (0, 30));
        assert_eq!(scissor_rect(rect(995.0, 100.0), SCREEN).unwrap().2, 5);
    }
}
