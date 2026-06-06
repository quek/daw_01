//! M14 Phase 58: text_input の cursor / selection x 位置を **glyphon と同じレイアウト** で
//! pixel-accurate に計算するための shape ユーティリティ。
//!
//! renderer 側の `GlyphPipeline` は draw のために自前で `cosmic_text::FontSystem` を持つが、
//! ui crate からは触れないので、ui 側にも同等の `FontSystem` を保持する。両者は同じ
//! system fonts を読むので shape 結果も一致する (キャッシュは別)。
//!
//! `Ui::measure_text(&str, font_size)` の 1 fn 経由で呼ばれる。`approx_text_width`
//! (ASCII 7px / CJK 14px の固定概算) は proportional font で実 advance とのずれが
//! 大きく ("m" は ~11px、"i" は ~4px)、長い text で 40-50px 単位の cursor ずれが出ていた。

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping};
use daw_ui_renderer::DEFAULT_FONT_FAMILY;
// `Align` は `Buffer::set_text` の 5 番目引数 (cosmic-text 0.18 から導入)。
// 1 行 measure では align 不要なので `None` を渡す。

/// text shape による 1 行テキストの advance 計算器。`UiHost` が 1 つ持つ。
///
/// `measure_advance` は 0 以上の f32 を返す (空文字列なら 0.0)。
pub struct TextMetrics {
    font_system: FontSystem,
    /// re-use する scratch buffer (毎呼び出しで `set_text` し直す)。
    scratch: Buffer,
    /// 省略記号文字列。 描画フォントで `…` (U+2026) が実字形を持てば `"…"`、
    /// 無ければ ASCII `"..."`。 初回 [`Self::ellipsis`] 呼び出し時に shape して 1 度だけ確定。
    ellipsis: Option<&'static str>,
}

impl Default for TextMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TextMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextMetrics").finish_non_exhaustive()
    }
}

impl TextMetrics {
    #[must_use]
    pub fn new() -> Self {
        let mut font_system = FontSystem::new();
        // metrics は measure_advance で都度上書きするので、ここでは仮の小さい値で初期化。
        let scratch = Buffer::new(&mut font_system, Metrics::new(14.0, 16.8));
        Self { font_system, scratch, ellipsis: None }
    }

    /// 省略記号として描画する文字列を返す。 描画フォント (`DEFAULT_FONT_FAMILY` +
    /// cosmic-text の font fallback) で `…` (U+2026) が実字形 (= `.notdef` 以外) に
    /// shape できれば `"…"`、 できなければ豆腐 (□) を避けて ASCII `"..."` を返す。
    /// renderer 側 `GlyphPipeline` と同じ family / shaping で判定するので、 ここで
    /// 実字形と判定したものは実描画でも豆腐にならない (Single Source of Truth)。
    /// 結果は初回に 1 度だけ確定して cache する。
    pub fn ellipsis(&mut self) -> &'static str {
        if let Some(e) = self.ellipsis {
            return e;
        }
        let resolved = if self.shapes_to_real_glyphs("…") { "…" } else { "..." };
        self.ellipsis = Some(resolved);
        resolved
    }

    /// `text` を描画フォントで shape し、 並んだ全 glyph が `.notdef` (glyph_id 0 = 豆腐)
    /// 以外なら `true`。 空文字列は `false`。 `measure_advance` と同じ scratch / attrs を
    /// 使うので、 ここで `true` の文字列は実描画でも同じ glyph に解決される。
    fn shapes_to_real_glyphs(&mut self, text: &str) -> bool {
        let metrics = Metrics::new(14.0, 16.8);
        self.scratch.set_metrics(&mut self.font_system, metrics);
        self.scratch.set_size(&mut self.font_system, None, Some(16.8));
        let attrs = Attrs::new().family(Family::Name(DEFAULT_FONT_FAMILY));
        self.scratch
            .set_text(&mut self.font_system, text, &attrs, Shaping::Advanced, None);
        self.scratch.shape_until_scroll(&mut self.font_system, false);
        let mut any = false;
        for run in self.scratch.layout_runs() {
            for glyph in run.glyphs {
                any = true;
                // glyph_id 0 は OpenType/TrueType 仕様で常に `.notdef` (豆腐)。
                // cosmic-text は HackGen に無ければ fallback font を試すので、 0 が残るのは
                // システム上どのフォントにも字形が無い真の豆腐ケースだけ。
                if glyph.glyph_id == 0 {
                    return false;
                }
            }
        }
        any
    }

    /// `text` を `font_size` で shape し、最初の line の **末尾までの x advance** を返す。
    /// 空文字列なら 0.0。
    ///
    /// line_height は advance 計算に影響しないが、cosmic-text の API 上必要。font_size の
    /// 1.2 倍を使う (ui 内で固定の慣用値)。
    pub fn measure_advance(&mut self, text: &str, font_size: f32) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let metrics = Metrics::new(font_size, font_size * 1.2);
        self.scratch.set_metrics(&mut self.font_system, metrics);
        // width=None で wrap せず 1 行で並べさせる。glyphon の draw 側でも `set_size(None, ..)`
        // を使う設定。
        self.scratch
            .set_size(&mut self.font_system, None, Some(font_size * 1.2));
        // renderer 側 GlyphPipeline と同じ family / shaping で shape する (Single Source of Truth)。
        let attrs = Attrs::new().family(Family::Name(DEFAULT_FONT_FAMILY));
        self.scratch.set_text(
            &mut self.font_system,
            text,
            &attrs,
            Shaping::Advanced,
            None, // Align: 1 行 measure には不要
        );
        self.scratch.shape_until_scroll(&mut self.font_system, false);
        // layout_runs は実 layout 済みの runs (= 各 line)。1 行を想定して max line.w を取る。
        self.scratch
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0_f32, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_zero() {
        let mut m = TextMetrics::new();
        assert!(m.measure_advance("", 14.0).abs() < f32::EPSILON);
    }

    #[test]
    fn longer_text_yields_wider_advance() {
        let mut m = TextMetrics::new();
        let short = m.measure_advance("a", 14.0);
        let long = m.measure_advance("aaaaaaaaaa", 14.0);
        assert!(long > short, "10 chars should be wider than 1: short={short} long={long}");
        // proportional font の実 advance は 0 ではないはず
        assert!(short > 0.0);
    }

    #[test]
    fn larger_font_size_yields_wider_advance() {
        let mut m = TextMetrics::new();
        let small = m.measure_advance("hello", 10.0);
        let big = m.measure_advance("hello", 28.0);
        // 同じ text なら font_size 大の方が advance も大きい (font 種別に依存しない普遍テスト)。
        assert!(big > small, "size 28 should be wider than 10: small={small} big={big}");
    }

    #[test]
    fn ellipsis_resolves_to_renderable_string() {
        // daw_01 #079: ellipsis は実字形を持つ文字列を返す ("…" か fallback "...")、
        // どちらでも measure > 0 で豆腐ではない。 同じ結果を cache する。
        let mut m = TextMetrics::new();
        let e = m.ellipsis();
        assert!(e == "…" || e == "...", "ellipsis は '…' か '...': got {e:?}");
        assert!(m.measure_advance(e, 14.0) > 0.0, "ellipsis は実幅を持つ");
        assert_eq!(m.ellipsis(), e, "2 回目も同じ結果 (cache)");
    }

    #[test]
    fn ascii_text_shapes_to_real_glyphs() {
        // sanity: 通常の ASCII は .notdef にならない (= 検出ロジックが真陽性を返す)。
        let mut m = TextMetrics::new();
        assert!(m.shapes_to_real_glyphs("abc"));
        assert!(!m.shapes_to_real_glyphs(""), "空文字列は実字形なし");
    }
}
