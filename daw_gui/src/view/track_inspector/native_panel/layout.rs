//! Par の格子の寸法 (Q12、§10.6)。描画と行高 ([`panel_height`]) が **同じ定数** を読む
//! (Par の高さを実測しない = 開いた最初のフレームから行が正しい高さで並ぶ)。
//!
//! 列は Par の幅を 6 等分する (深さ 0 の 336px で 56px、Parallel の中の追加分は幅に追従する)。
//! 左詰めで、列の少ない種類 (Bus Comp 5 列 / Tone EQ 3 列 / Limiter 1 列) は右が空く。

use common::model::NativeKind;
use daw_ui_renderer::Rect;

/// 上下の余白。
pub const PAD: f32 = 6.0;
/// 列の見出し (`HP LF …` / `Thr Rat …`) の行高。
pub const HEAD: f32 = 14.0;
/// 見出しの文字サイズ。
pub const HEAD_FONT: f32 = 10.0;
/// つまみの直径。
pub const KNOB: f32 = 24.0;
/// つまみと数値欄の間。
pub const KNOB_GAP: f32 = 2.0;
/// 数値欄 (常時表示) の寸法。
pub const NUM_W: f32 = 52.0;
pub const NUM_H: f32 = 16.0;
/// つまみ + 数値欄 1 セルの高さ。
pub const CELL: f32 = KNOB + KNOB_GAP + NUM_H;
/// 段と段の間。
pub const ROW_GAP: f32 = 4.0;
/// EQ / Tone EQ のカーブの高さ (幅は Par の全幅)。
pub const CURVE_H: f32 = 72.0;
/// GR バーとモード切り替えの段の高さ。
pub const BAR_H: f32 = 18.0;
/// 切り替えボタン (`[ON]` / `[Bell]` / `[Listen]`) の寸法。
pub const SWITCH_W: f32 = 44.0;
pub const SWITCH_H: f32 = 16.0;
/// 列数。
pub const COLUMNS: usize = 6;
/// セクションの小見出し (`── TIME ──`) の行高。
pub const SECTION_H: f32 = 14.0;
/// 切り替え帯の中で、段階式セレクタの手前に置く小ラベルの幅。
pub const SELECTOR_LABEL_W: f32 = 30.0;
/// 段階式セレクタ (`[Stereo]`) のボタン幅。
pub const SELECTOR_W: f32 = 62.0;

/// 種類ごとの Par の高さ (行の 26px は含まない)。
#[must_use]
pub fn panel_height(kind: NativeKind) -> f32 {
    match kind {
        // カーブ → 見出し → Freq / Gain / Q の 3 段。
        NativeKind::Eq => PAD + CURVE_H + ROW_GAP + HEAD + 3.0 * (CELL + ROW_GAP) + PAD,
        // [LEV|CMP|LIM] + GR → 見出し → 6 セル → SC 列の下に [Listen]。
        NativeKind::Comp => PAD + BAR_H + ROW_GAP + HEAD + CELL + KNOB_GAP + NUM_H + PAD,
        // GR → 見出し → 5 セル (Ratio は段切り替え)。
        NativeKind::BusComp => PAD + BAR_H + ROW_GAP + HEAD + CELL + PAD,
        // カーブ → 見出し → 3 セル。
        NativeKind::ToneEq => PAD + CURVE_H + ROW_GAP + HEAD + CELL + PAD,
        // [Frz] → ROOM 6 セル → TONE / MOD / OUT 6 セル。
        NativeKind::Reverb => PAD + BAR_H + ROW_GAP + 2.0 * (SECTION_H + HEAD + CELL) + ROW_GAP + PAD,
        // 切り替え帯 2 段 → TIME 6 セル → FEEDBACK / TONE 4 セル → MOD / OUT 4 セル。
        NativeKind::Delay => {
            PAD + 2.0 * BAR_H + ROW_GAP + 3.0 * (SECTION_H + HEAD + CELL) + 2.0 * ROW_GAP + PAD
        }
    }
}

/// master Limiter の Par の高さ (GR セグメント → 見出し → Ceiling 1 セル)。
#[must_use]
pub fn limiter_panel_height() -> f32 {
    PAD + BAR_H + ROW_GAP + HEAD + CELL + PAD
}

/// 列 `col` に幅 `w` のものを中央揃えで置いたときの x。
#[must_use]
pub fn column_x(panel: Rect, col: usize, w: f32) -> f32 {
    let col_w = panel.w / COLUMNS as f32;
    panel.x + col as f32 * col_w + (col_w - w) * 0.5
}

/// EQ / Tone EQ のカーブの矩形 (Par の上端から `PAD` 下、全幅 × [`CURVE_H`])。点の座標を
/// 外から求める (テスト) ときも同じ関数を通す。
#[must_use]
pub fn curve_rect(panel: Rect) -> Rect {
    Rect { x: panel.x, y: panel.y + PAD, w: panel.w, h: CURVE_H }
}
