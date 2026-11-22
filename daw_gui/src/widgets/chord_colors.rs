//! コードトラックの箱の色 (r.md #157、`docs/plan_chord_track.md`)。
//!
//! 箱を描く全ての面 (アレンジの帯 / ピアノロール上端の帯) がここを引く SSoT。
//! 箱の上の文字は塗りが可変 (ユーザー着色) なので、極性固定インク [`Palette::ink_for`] で決める。

use common::model::ChordEvent;
use daw_ui_core::color::composite_over;
use daw_ui_renderer::Color;

use crate::theme::Palette;
use crate::view::track_color;

/// 箱の塗り。`ChordEvent.color` があればその色 (トラック色と同じ linear 値)、無ければ
/// テーマの accent を面 (`panel_raised`) に薄く重ねた色。
///
/// 既定色の濃さ (alpha 0.12) は「文字が本文の 4.5:1 で読める」側に寄せた値: 白黒どちらのインクとも
/// 拮抗する中間の明度 (相対輝度 ≈ 0.18) を避け、ダークでは暗い箱 + 明インク、ライトでは明るい箱 +
/// 暗インクになる (0.25 / 0.35 だとダークで中間に入り 3:1 しか出なかった)。ユーザー色は中間の明度も
/// 取りうるので、そちらで保証できるのは `ink_for` の 3:1 まで。
#[must_use]
pub(crate) fn chord_box_fill(p: &Palette, ev: &ChordEvent) -> Color {
    match ev.color {
        Some(rgb) => track_color::to_renderer(rgb),
        None => composite_over(p.accent.with_alpha(0.12), p.panel_raised),
    }
}

/// 箱の上の文字 (コード名) の色。塗りから決める (明るい箱 → 暗インク / 暗い箱 → 明インク)。
#[must_use]
pub(crate) fn chord_box_ink(p: &Palette, fill: Color) -> Color {
    p.ink_for(fill)
}
