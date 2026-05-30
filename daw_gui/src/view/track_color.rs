//! トラック / クリップの色 (`docs/plan_track_clip_color.md`)。
//!
//! model (`common`) は色値を `Option<[f32; 3]>` (RGB, 不透明) でしか持たない。
//! ここ (view 層) がパレット + 継承ロジック + renderer `Color` への変換を
//! 担う。SSoT: 「導出可能な値は保存しない」ので、トラック色は `None` のとき
//! id から安定導出する (auto-assignment)。
//!
//! - `Track.color == None`  ⇒ `derived_track_color(id)` (パレット巡回)
//! - `Track.color == Some`  ⇒ ユーザー上書き
//! - `Clip.color  == None`  ⇒ 所属トラックの effective 色を継承
//! - `Clip.color  == Some`  ⇒ クリップ個別上書き

use common::model::{Clip, Track};
use daw_ui_renderer::Color;

/// トラック / クリップの自動割り当て + ピッカーで共有する 16 色パレット
/// (Ableton Live のデフォルトパレットを参考にした、暗背景で映える彩度高め)。
/// 値は RGB (`[f32; 3]`、不透明)。`derived_track_color` の巡回と picker
/// スウォッチ表示で同じ配列を使う。
pub const PALETTE: [[f32; 3]; 16] = [
    [0.90, 0.30, 0.30], // red
    [0.92, 0.52, 0.25], // orange
    [0.93, 0.74, 0.28], // amber
    [0.86, 0.86, 0.32], // yellow
    [0.58, 0.82, 0.34], // lime
    [0.35, 0.78, 0.42], // green
    [0.30, 0.78, 0.62], // teal
    [0.32, 0.74, 0.84], // cyan
    [0.34, 0.60, 0.90], // blue
    [0.42, 0.46, 0.90], // indigo
    [0.58, 0.42, 0.90], // violet
    [0.74, 0.40, 0.88], // purple
    [0.90, 0.42, 0.78], // magenta
    [0.90, 0.40, 0.56], // pink
    [0.60, 0.55, 0.50], // taupe
    [0.55, 0.62, 0.70], // slate
];

/// `Track.color == None` のときに id から導出する安定パレット色。
/// id は `Song::alloc_track_id` で単調増加するので、`(id - 1) % N` で
/// 連番トラックが連続パレット色になり (= 巡回割り当て)、reorder しても
/// id ベースなので色が動かない。
#[must_use]
pub fn derived_track_color(track_id: u32) -> [f32; 3] {
    let n = PALETTE.len() as u32;
    let idx = track_id.saturating_sub(1) % n;
    PALETTE[idx as usize]
}

/// トラックの実効色: 明示上書き (`Some`) か、無ければ id 由来の導出色。
#[must_use]
pub fn effective_track_color(track: &Track) -> [f32; 3] {
    track.color.unwrap_or_else(|| derived_track_color(track.id))
}

/// クリップの実効色: 明示上書き (`Some`) か、無ければ所属トラックの実効色を継承。
#[must_use]
pub fn effective_clip_color(track: &Track, clip: &Clip) -> [f32; 3] {
    clip.color.unwrap_or_else(|| effective_track_color(track))
}

/// model の `[f32; 3]` を renderer `Color` (不透明) に変換。
#[must_use]
pub fn to_renderer(rgb: [f32; 3]) -> Color {
    Color::rgb(rgb[0], rgb[1], rgb[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_color_cycles_by_id_and_is_stable() {
        // id 1 と id 17 (= 1 + 16) は同じパレット先頭色。
        assert_eq!(derived_track_color(1), PALETTE[0]);
        assert_eq!(derived_track_color(17), PALETTE[0]);
        // 連番 id は連続パレット色。
        assert_eq!(derived_track_color(2), PALETTE[1]);
        assert_eq!(derived_track_color(16), PALETTE[15]);
        // id 0 (未採番 sentinel) も panic せず先頭色。
        assert_eq!(derived_track_color(0), PALETTE[0]);
    }

    #[test]
    fn effective_track_color_prefers_override() {
        let mut track = Track { id: 3, ..Track::default() };
        assert_eq!(effective_track_color(&track), derived_track_color(3));
        track.color = Some([0.1, 0.2, 0.3]);
        assert_eq!(effective_track_color(&track), [0.1, 0.2, 0.3]);
    }

    #[test]
    fn effective_clip_color_inherits_then_overrides() {
        let track = Track { id: 5, color: Some([0.4, 0.5, 0.6]), ..Track::default() };
        // clip.color == None ⇒ トラック実効色を継承。
        let mut clip = Clip { id: 1, color: None, ..Clip::default() };
        assert_eq!(effective_clip_color(&track, &clip), [0.4, 0.5, 0.6]);
        // clip.color == Some ⇒ 個別上書き。
        clip.color = Some([0.7, 0.8, 0.9]);
        assert_eq!(effective_clip_color(&track, &clip), [0.7, 0.8, 0.9]);
    }
}
