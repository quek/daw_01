//! 縦に同じ間隔で並ぶ行のうち、**いま見えている行** の範囲を求める口 (daw_01 r.md #129)。
//!
//! 行数に上限の無い一覧 (plugin の数万 param など) を `scroll_area` の中に描くとき、見えない行の widget まで
//! 毎フレーム回すと行数に比例して重くなり、clip の外で見えない行が当たり判定を持つことにもなる。自分で
//! `scroll_area` を持つ一覧 ([`Ui::list_view`]) も、外側の `scroll_area` の中に置いた一覧も、今の clip
//! (祖先の `scroll_area` / `with_clip_rect` が張った可視領域の交差。無ければ画面) と行の画面位置だけで
//! 範囲が決まるので、ここ 1 本で出す。
//!
//! 範囲外の行は描かないが、呼び出し側は **全行ぶんの高さを消費する** (レイアウトと scroll の content 高は
//! 描いた行数に依らない)。

use std::ops::Range;

use crate::ui::Ui;

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 画面上の `first_top` から `pitch` 間隔で並ぶ `count` 行 (i 行目は `[first_top + i·pitch, first_top + (i+1)·pitch)`)
    /// のうち、今の clip の縦範囲に一部でもかかる行の index の範囲 (module doc)。
    #[must_use]
    pub fn visible_rows(&self, first_top: f32, pitch: f32, count: usize) -> Range<usize> {
        #[allow(clippy::cast_precision_loss)]
        let (top, bottom) = self
            .current_clip_rect()
            .map_or((0.0, self.screen().height as f32), |clip| (clip.y, clip.y + clip.h));
        visible_row_range(top, bottom, first_top, pitch, count)
    }
}

/// [`Ui::visible_rows`] の本体。見えている縦範囲 `[top, bottom)` にかかる行の index の範囲。
#[must_use]
pub fn visible_row_range(top: f32, bottom: f32, first_top: f32, pitch: f32, count: usize) -> Range<usize> {
    if count == 0 || pitch.is_nan() || pitch <= 0.0 || bottom <= top {
        return 0..0;
    }
    // i 行目がかかる ⟺ first_top + (i+1)·pitch > top かつ first_top + i·pitch < bottom。
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let index = |v: f32| if v.is_nan() || v <= 0.0 { 0 } else { (v as usize).min(count) };
    let start = index(((top - first_top) / pitch).floor());
    let end = index(((bottom - first_top) / pitch).ceil());
    start..end.max(start)
}

#[cfg(test)]
mod tests {
    use super::visible_row_range;

    #[test]
    fn rows_touching_the_visible_band_are_included_and_rows_ending_at_its_edge_are_not() {
        // 行の間隔 10、先頭の行の上端 100。見えている範囲 [120, 150) には 2〜4 行目がかかる
        // (境界ちょうど: 1 行目は 120 で終わるので入らず、5 行目は 150 から始まるので入らない)。
        assert_eq!(visible_row_range(120.0, 150.0, 100.0, 10.0, 1000), 2..5);
        // 半端な位置は、はみ出した行も含む。
        assert_eq!(visible_row_range(125.0, 155.0, 100.0, 10.0, 1000), 2..6);
        // 一覧が見えている範囲より上 / 下 / 行数で頭打ち / 行が無い。
        assert_eq!(visible_row_range(0.0, 50.0, 100.0, 10.0, 1000), 0..0);
        assert!(visible_row_range(500.0, 600.0, 100.0, 10.0, 10).is_empty());
        assert_eq!(visible_row_range(0.0, 1.0e9, 100.0, 10.0, 7), 0..7);
        assert!(visible_row_range(0.0, 100.0, 0.0, 0.0, 7).is_empty());
    }
}
