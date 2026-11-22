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
/// テーマの accent を面 (`panel_raised`) に薄く重ねた色 (ダーク / ライトのどちらでも面から浮き、
/// 文字が `chord_box_ink` で読める明度に収まる)。
#[must_use]
pub(crate) fn chord_box_fill(p: &Palette, ev: &ChordEvent) -> Color {
    match ev.color {
        Some(rgb) => track_color::to_renderer(rgb),
        None => composite_over(p.accent.with_alpha(0.35), p.panel_raised),
    }
}

/// 箱の上の文字 (コード名) の色。塗りから決める (明るい箱 → 暗インク / 暗い箱 → 明インク)。
#[must_use]
pub(crate) fn chord_box_ink(p: &Palette, fill: Color) -> Color {
    p.ink_for(fill)
}
