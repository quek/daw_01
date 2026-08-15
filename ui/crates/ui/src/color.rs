//! 色ユーティリティ — WCAG relative luminance ベースの auto-contrast 判定。
//!
//! arrangement の clip / track 名 (#060 Phase 89) と piano_roll の鍵盤オクターブラベル
//! (#093 Phase 117) が共有する SSoT。「widget が実際に塗った fill の輝度から文字色を導出する」
//! という #060 の設計を、半透明 overlay を背後 bg と合成してから判定する形で一般化した。

use daw_ui_renderer::Color;

/// 白文字と黒文字のコントラスト比が等しくなる relative luminance の分岐点。
/// `luminance > THRESHOLD` を「明るい背景 = 暗文字を選ぶべき」と解釈する。
pub const CONTRAST_LUMINANCE_THRESHOLD: f32 = 0.179;

/// **linear-light** 成分 (各 `0.0..=1.0`) を WCAG 2.x relative luminance に変換する。
///
/// WCAG の relative luminance は定義上 linear-light の加重和なので、入力が既に linear なら
/// **そのまま加重するのが正しい**。gamma decode は「sRGB エンコードされた値」を linear に
/// 戻すための前処理であって、輝度計算の一部ではない。
///
/// この crate のパレット値は linear で持つ (render target が `Rgba8UnormSrgb` で GPU が表示時に
/// エンコードする。`theme::srgb` / テーマ JSON の hex パーサはどちらも `srgb_to_linear` を
/// 通してから格納する) ので、ここで decode を掛けると **二重デコード** になる。
///
/// 実際 2026-08-15 まで decode 込みで実装されていて、輝度を最大 13 倍過小評価していた。
/// 症状: (a) 中間調の面が一律「暗い」と判定され明インクが選ばれる (linear 0.22/0.34/0.52 =
/// 画面上は明るいスチールブルーの既定クリップ色で、白文字は 2.8:1 しか出ていなかった)、
/// (b) [`Palette::adapt_on`](crate::theme::Palette::adapt_on) が 3:1 を満たしたと誤認して
/// 実効 2.5:1 で停止する (ライトテーマのメーター peak readout が読めない)。
#[must_use]
pub fn relative_luminance(r: f32, g: f32, b: f32) -> f32 {
    0.2126 * r.clamp(0.0, 1.0) + 0.7152 * g.clamp(0.0, 1.0) + 0.0722 * b.clamp(0.0, 1.0)
}

/// `fg` を `bg` の上に standard src-over alpha 合成した実効色を返す (RGB のみ、結果は不透明扱い)。
/// 半透明 overlay (clip の `share_group_alpha` / 鍵盤の `root_row_overlay` 等) を背後の不透明 bg と
/// 合成して「実際に目に入る色」を得るのに使う。`fg.a == 1.0` なら `fg` の RGB そのもの。
#[must_use]
pub fn composite_over(fg: Color, bg: Color) -> Color {
    let a = fg.a.clamp(0.0, 1.0);
    Color::rgb(
        fg.r * a + bg.r * (1.0 - a),
        fg.g * a + bg.g * (1.0 - a),
        fg.b * a + bg.b * (1.0 - a),
    )
}

/// `bg` (不透明前提) の relative luminance が閾値を超える (= 明るい) なら `dark`、暗いなら `light`
/// を返す。半透明 fill / overlay を判定したい場合は呼ぶ前に [`composite_over`] で背後 bg と
/// 合成しておくこと。
#[must_use]
pub fn pick_contrast(bg: Color, light: Color, dark: Color) -> Color {
    if relative_luminance(bg.r, bg.g, bg.b) > CONTRAST_LUMINANCE_THRESHOLD {
        dark
    } else {
        light
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_luminance_monotonic_and_extremes() {
        // 黒 = 0、白 = 1 (WCAG 定義の極値)。
        assert!(relative_luminance(0.0, 0.0, 0.0).abs() < 1e-6);
        assert!((relative_luminance(1.0, 1.0, 1.0) - 1.0).abs() < 1e-6);
        // 緑が一番輝度寄与が大きい (0.7152) → 純緑 > 純赤 > 純青。
        let red = relative_luminance(1.0, 0.0, 0.0);
        let green = relative_luminance(0.0, 1.0, 0.0);
        let blue = relative_luminance(0.0, 0.0, 1.0);
        assert!(green > red && red > blue);
    }

    #[test]
    fn composite_over_opaque_returns_fg_rgb() {
        let fg = Color::rgb(0.2, 0.4, 0.6);
        let bg = Color::rgb(0.9, 0.9, 0.9);
        let out = composite_over(fg, bg);
        assert!((out.r - 0.2).abs() < 1e-6 && (out.g - 0.4).abs() < 1e-6 && (out.b - 0.6).abs() < 1e-6);
        assert!((out.a - 1.0).abs() < 1e-6, "結果は不透明");
    }

    #[test]
    fn composite_over_half_alpha_is_midpoint() {
        let fg = Color::rgba(0.0, 0.0, 0.0, 0.5);
        let bg = Color::rgb(1.0, 1.0, 1.0);
        let out = composite_over(fg, bg);
        // black 50% over white = 中間グレー 0.5。
        assert!((out.r - 0.5).abs() < 1e-6 && (out.g - 0.5).abs() < 1e-6 && (out.b - 0.5).abs() < 1e-6);
    }

    #[test]
    fn pick_contrast_threshold_and_alpha_compositing() {
        let light = Color::rgb(0.95, 0.95, 0.97);
        let dark = Color::rgb(0.10, 0.10, 0.15);
        // 明るい白背景 → 暗文字。
        assert_eq!(pick_contrast(Color::rgb(0.95, 0.95, 0.95), light, dark), dark);
        // 暗い背景 → 明文字。
        assert_eq!(pick_contrast(Color::rgb(0.10, 0.10, 0.12), light, dark), light);
        // 不透明な薄緑は明るく暗文字寄りだが、暗 bg に薄く重ねると実効輝度が下がり明文字。
        // = 「overlay 自身の色ではなく合成後の色で判定する」 という契約。
        //
        // alpha は 0.15。 輝度は linear で線形合成されるので、 薄緑 (Y=0.765) を
        // ほぼ黒 (Y=0.021) に 15% 重ねた実効輝度は 0.13 で分岐点 0.179 を下回る。
        // 旧テストは alpha 0.30 / bg (0.12,0.13,0.16) で「明文字」 を期待していたが、
        // 実効 Y は 0.32 = 画面上は中間色のセージグリーンで、暗文字が正しい
        // (relative_luminance の二重デコードで輝度が過小評価されていたため通っていた)。
        let pale_green = Color::rgba(0.55, 0.85, 0.55, 0.15);
        let dark_bg = Color::rgb(0.02, 0.02, 0.03);
        let opaque = pick_contrast(Color::rgb(0.55, 0.85, 0.55), light, dark);
        let composited = pick_contrast(composite_over(pale_green, dark_bg), light, dark);
        assert_eq!(opaque, dark, "不透明な薄緑は暗文字");
        assert_eq!(composited, light, "ほぼ黒に 15% で重ねた薄緑は明文字");
    }
}
