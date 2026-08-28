//! 可変背景の上に置く**標識**が、どの背景でも読めることの検査。
//!
//! r.md #73 の実機不具合 (クリップ選択中に Alt hover すると曲線が消える) と同じ
//! root cause — **固定の色トークンを、内容で色が変わる面の上に置いた** — の兄弟を
//! まとめて止める (memory `feedback_ui_indicator_contrast_on_variable_bg`)。
//!
//! **尺度は「描かれたか」ではなく「背景に対してコントラストが取れているか」。**
//! 描画の有無で見ると、背景と同色で塗った標識が緑のまま通る (#73 で実際に通した)。
//!
//! **fixture には必ず「沈む側」の背景を入れる。** ここを入れ忘れると、この欠陥は
//! 原理的に検出できない — #73 で最初に書いた検査が赤にならなかったのは、
//! fixture に「選択中クリップ (黄)」が無く、標識が沈む背景を一度も踏まなかったから。

use super::*;
use daw_ui_core::theme::{contrast_ratio, Palette};

/// WCAG の非テキスト UI 部品の最低要件。標識はこれを下回ったら「見えない」と扱う。
const MIN_CONTRAST: f32 = 3.0;

/// 標識が乗りうる背景の spectrum。**沈む側を必ず含む**。
///
/// - 明レーン … `automation_lane_bg = p.window_bg` はライトテーマで明るい。
///   明インク (`ink_on_dark`) 固定の標識はここで消える。
/// - 選択中クリップ … `clip_selected_fill = p.selection_warm` (明るい黄)。同上。
/// - 暗いユーザー着色 … 暗インク (`ink_on_bright`) 固定の標識はここで消える。
fn variable_backgrounds() -> Vec<(&'static str, Color)> {
    let dark = Palette::dark();
    let light = Palette::light();
    vec![
        ("暗レーン (ダークの window_bg)", dark.window_bg),
        ("明レーン (ライトの window_bg)", light.window_bg),
        ("選択中クリップ (selection_warm)", dark.selection_warm),
        ("ユーザー着色 (暗い青)", Color::rgb(0.05, 0.08, 0.16)),
        ("ユーザー着色 (明るいクリーム)", Color::rgb(0.93, 0.90, 0.72)),
    ]
}

/// `got` が `bg` に対して読めるか。落ちたときにどの背景で沈んだかが分かる形にする。
fn assert_readable(what: &str, bg_name: &str, got: Color, bg: Color) {
    let ratio = contrast_ratio(got, bg);
    assert!(
        ratio >= MIN_CONTRAST,
        "{what} が「{bg_name}」の上で読めない: contrast {ratio:.2}:1 (最低 {MIN_CONTRAST}:1)"
    );
}

fn theme() -> crate::theme::Theme {
    crate::theme::Theme::builtin("dark").expect("組込みダークテーマは常に存在する")
}

/// #1 選択中 automation point の dot。
///
/// 旧実装は fill / border とも `ink_on_dark` 固定だった。 dot は automation clip の
/// 面の上に乗るので、ライトテーマの明レーンでも、クリップ選択中の黄でも沈む。
#[test]
fn selected_automation_point_dot_is_readable_on_every_lane_background() {
    let p = Palette::dark();
    let style = ArrangementStyle::from_theme(&theme());
    for (name, bg) in variable_backgrounds() {
        let (fill, border) = automation_point_selected_colors(&p, &style);
        // dot は fill と border の 2 層。**どちらか一方**が背景に対して読めればよい
        // (= 逆極性の縁を持つ dot の idiom。 fill が沈んでも縁で輪郭が出る)。
        let best = contrast_ratio(fill, bg).max(contrast_ratio(border, bg));
        assert!(
            best >= MIN_CONTRAST,
            "選択 point の dot が「{name}」の上で読めない: \
             fill {:.2}:1 / border {:.2}:1 (最低 {MIN_CONTRAST}:1)",
            contrast_ratio(fill, bg),
            contrast_ratio(border, bg)
        );
    }
}

/// #2 リンク / 複製バッジ (`⇌` / `+`)、#3 ゲインのハンドル線、#4 ドラッグ中のゴーストラベル。
///
/// いずれも **クリップ面の上**に置く標識。旧実装は極性固定インクをそのまま使っていた。
/// 3 つとも同じ 1 本 (`clip_ink_for`) を通すので、まとめて確かめる。
#[test]
fn clip_face_indicators_are_readable_on_every_clip_fill() {
    let p = Palette::dark();
    let style = ArrangementStyle::from_theme(&theme());
    for (name, bg) in variable_backgrounds() {
        // 実効背景 = クリップの塗り (不透明扱い) 。call site は同じ関数を通す。
        let ink = clip_ink_for(&p, bg, style.bg);
        assert_readable("クリップ面の標識 (バッジ / ハンドル線 / ゴーストラベル)", name, ink, bg);
    }
}

/// #7 トグルの ON 時の文字色は **ON 塗りから自動で決める**。
///
/// `ToggleButtonStyle::from_palette` の既定が `None` = 「ON 塗りの輝度から
/// auto-contrast」で、その doc は「caller が `on_color` を上書きしても文字が
/// 読めることが保証される」と書いている。 固定色で上書きすると、ユーザーテーマが
/// `daw.solo` を暗くした瞬間に「S」が消える (ミキサーの Solo は
/// `ink_for(theme.daw.solo)` で解いており、規則が 2 つに割れてもいた)。
#[test]
fn solo_toggle_on_text_is_derived_from_the_on_fill() {
    let style = ArrangementStyle::from_theme(&theme());
    assert_eq!(
        style.solo_button.on_text_color, None,
        "ON 時の文字色は固定せず daw-ui の既定 (ON 塗りから auto-contrast) に委ねる"
    );
    // 委ねた先が実際に読めることも確かめる (既定が壊れたらここで落ちる)。
    let p = Palette::dark();
    for (name, fill) in [
        ("既定の solo 黄", style.solo_button.on_color),
        ("ユーザーテーマが暗くした solo", Color::rgb(0.06, 0.05, 0.02)),
    ] {
        assert_readable("トグル ON の文字", name, p.ink_for(fill), fill);
    }
}
