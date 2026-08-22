// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! daw_gui 固有の library widget。
//!
//! daw-ui-core は Model 非依存の汎用 immediate-mode プリミティブ (button / fader /
//! waveform / heavy キャッシュ …) を提供する。 arrangement のように **`common::model`
//! (Song / Track / Clip) と `AppData` を直接読み、 `edit_song` 経由で `Edit<AppData>` を
//! 直発行する** DAW 固有の複合 widget は daw-ui-core には置けない (Model 非依存の不変条件を
//! 破る)。 それらをここに置く (S4b で `ui/crates` から移設)。
//!
//! 汎用プリミティブ (heavy / push_rect / take_drag_rect_in_rect …) は引き続き
//! daw-ui-core の `pub` API を呼ぶ。

pub mod arrangement;
pub mod piano_roll;
pub mod ruler_ops;
pub mod select_modifier;
pub mod time_grid;

/// slice 境界の縦線を出す最小間隔 (px)。 これより密なスライスは線だけで領域が
/// 埋まって波形もスライス配置も読めなくなるので間引く。
///
/// アレンジビュー ([`arrangement`]) とオーディオエディタ
/// (`crate::view::audio_editor`) の **両方**が同じ規約で描くための SSoT
/// (r.md #41: 片方だけガードが無く、 長尺素材を Slice にすると
/// オーディオエディタが縦線で埋まっていた)。
pub const SLICE_DIVIDER_MIN_PX: f32 = 3.0;

/// 昇順の x 列から、 直前に採用した線と `SLICE_DIVIDER_MIN_PX` 以上離れたものだけを残す。
///
/// 「スライス幅」 ではなく「隣接する線の間隔」 で間引くので、 gap で離れた細い
/// スライス (伸ばした Slice clip) の頭にはちゃんと線が出る。
pub fn thin_slice_dividers(xs: impl IntoIterator<Item = f32>) -> Vec<f32> {
    let mut kept: Vec<f32> = Vec::new();
    for x in xs {
        if kept.last().is_none_or(|&p| x - p >= SLICE_DIVIDER_MIN_PX) {
            kept.push(x);
        }
    }
    kept
}
