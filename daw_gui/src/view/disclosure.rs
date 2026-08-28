//! 開閉マーク (disclosure triangle) の glyph を決める唯一の場所。
//!
//! r.md #74。 以前は「bool から glyph を直接引く」 リテラルが 3 か所
//! (mixer / arrangement / modulation rack) に複製され、 うち 1 つは別 codepoint
//! (▸/▾) を使っていた。 開示方向が式に入っていないので、 横並びの mixer では
//! ▼ が「何も無い方向」 を指し、 片方だけ直せば意味が食い違うという構造だった。
//!
//! 規則は 1 つだけ:
//!
//! > **展開中の三角は「中身が現れる軸」 の向きを指す。 折り畳み中はその軸から
//! > 90 度回した向きを指す。**
//!
//! 縦に開くもの (arrangement の group track / inspector の modulation rack) は
//! [`RevealAxis::Block`] で 展開 = ▼ / 折り畳み = ▶ となり、 Apple HIG・
//! WinUI TreeView・CSS Counter Styles Level 3 §6.3 (`disclosure-open` = ▾ /
//! `disclosure-closed` = ▸) の慣習と一致する。 横に開く mixer (group strip の
//! 子は **右** に並ぶ) は [`RevealAxis::Inline`] で 展開 = ▶ / 折り畳み = ▼ と、
//! 縦の裏返しになる。
//!
//! **同じ group が arrangement と mixer で別のマークになるのは意図した結果**
//! (r.md #74 で確定)。 三角は「状態」 ではなく「中身がどちらへ開くか」 を伝える。
//! CSS が directional marker について "If the image is directional, it must
//! respond to the writing mode of the element" と定めているのと同じ考え方で、
//! 向きは絶対方向ではなく開示軸に相対で決まる。

/// 展開したときに中身が現れる方向の軸。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealAxis {
    /// 縦 (block) 方向 — 展開すると中身が **下** に現れる。
    /// arrangement の group track、 inspector の modulation rack 行。
    Block,
    /// 横 (inline) 方向 — 展開すると中身が **右** に現れる。
    /// mixer の group strip (子 strip は group strip の右に並ぶ)。
    Inline,
}

/// 開閉マークの glyph。 `collapsed` は「中身が見えていない」 状態。
///
/// **全ての開閉マークはこの関数を通す。** 呼び出し側で `if collapsed { … }` と
/// リテラルを書かないこと (それが r.md #74 の root cause)。
#[must_use]
pub fn disclosure_glyph(collapsed: bool, axis: RevealAxis) -> &'static str {
    match (axis, collapsed) {
        // 展開中は軸の向きを指す。
        (RevealAxis::Block, false) => "\u{25bc}",  // ▼ 中身は下
        (RevealAxis::Inline, false) => "\u{25b6}", // ▶ 中身は右
        // 折り畳み中は軸から 90 度回す。
        (RevealAxis::Block, true) => "\u{25b6}",  // ▶
        (RevealAxis::Inline, true) => "\u{25bc}", // ▼
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// r.md #74: 向きは開示軸に相対。 リテラル複製時代の
    /// 「片方のビューだけ直して逆転が残る」 を機械で止める。
    #[test]
    fn disclosure_glyph_points_along_reveal_axis() {
        // 展開中 = 軸の向き。
        assert_eq!(disclosure_glyph(false, RevealAxis::Block), "\u{25bc}");
        assert_eq!(disclosure_glyph(false, RevealAxis::Inline), "\u{25b6}");
        // 折り畳み中 = 軸から 90 度。
        assert_eq!(disclosure_glyph(true, RevealAxis::Block), "\u{25b6}");
        assert_eq!(disclosure_glyph(true, RevealAxis::Inline), "\u{25bc}");
        // 軸が式に入っている証拠: 同じ状態でも軸が違えば必ず別のマークになる。
        for collapsed in [false, true] {
            assert_ne!(
                disclosure_glyph(collapsed, RevealAxis::Block),
                disclosure_glyph(collapsed, RevealAxis::Inline),
                "collapsed={collapsed} で Block と Inline が同じ glyph になっている"
            );
        }
    }
}
